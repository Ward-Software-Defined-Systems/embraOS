//! Higgsfield's image models over its asynchronous API
//! (`https://api.higgsfield.ai`; the reference is at
//! `https://docs.higgsfield.ai/docs/`, one page a model). The API is its own
//! account at `open.higgsfield.ai`, paid from a balance of its own, apart
//! from a plan on higgsfield.ai.
//!
//! A call: the request goes to the model's own endpoint
//! (`POST /<endpoint id>`, the model's id in this OS) and is answered at once
//! with a request id and two URLs; its status is read until it ends; the
//! picture is fetched from the address the status names, on a host of
//! Higgsfield's choosing. By the reference:
//! - every API call carries `Authorization: Key <key id>:<key secret>`; a
//!   submission also `Idempotency-Key: <uuid>`, one a call, sent again with
//!   the same key after a network failure or a 5xx, so a submission that did
//!   arrive is not made twice;
//! - the submission's answer is `{status, request_id, status_url,
//!   cancel_url}`;
//! - a status: `queued` and `in_progress` go on; `completed` (with
//!   `images: [{url}]`), `failed` (with `error`), `nsfw` and `canceled` end
//!   it;
//! - `POST cancel_url` answers 202 when the request is canceled and 400 once
//!   it has started; a request that completes is charged, one that fails,
//!   is refused by moderation or is canceled is not;
//! - an error is `{"detail": <a string, a list or an object>}`;
//! - a reference image is a public URL: `POST /files/generate-upload-url`
//!   with its content type answers `{public_url, upload_url,
//!   upload_headers}`, the bytes are `PUT` to `upload_url` with those
//!   headers and without the credential, and `public_url` goes into the
//!   model's own field (`References::Urls` or `Url`).
//!
//! The log names the host a picture was fetched from and the host a
//! reference was put to (`net::host_shown`): Higgsfield chooses both, and an
//! operator who filters what the machine may reach has to know them. Never
//! the address itself, whose path or query can be a signature.
//!
//! The credential goes to the API's own origin and nowhere else: a status
//! or cancel URL of another origin is not followed (the URL the reference
//! documents is built from the request id in its place), and the picture is
//! fetched by `net::public_url_client` (https, public addresses, pinned, no
//! credential). Every value a request carries is read from the model's page
//! (`IMAGE_MODELS`); the guard `every_request_the_validator_lets_through_carries_values_of_each_models_page`
//! holds them against the pages' lists, typed into it from the pages.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderValue, AUTHORIZATION};
use serde_json::{json, Map, Value as JsonValue};
use tokio::time::Instant;

use super::{
    image_model, GeneratedImage, ImageGenError, ImageGenRequest, ImageGenResponse, ImageModel, ImageProvider, ImageProviderKind,
    References,
};
use crate::net;
use crate::proactive::{Notification, Priority};

/// The API's own origin.
pub const API_BASE: &str = "https://api.higgsfield.ai";

/// One call to the API: a submission, a status read, a cancel.
const API_TIMEOUT: Duration = Duration::from_secs(10);
/// One fetch of the picture.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20);
/// One reference image sent to its upload slot.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(20);
/// The most an answer of the API itself may be: a status is a few hundred
/// bytes.
const API_ANSWER_MAX: usize = 256 * 1024;
/// The most of an error's `detail` a message quotes.
const DETAIL_SHOWN_MAX: usize = 600;

/// The pacing of the status reads, and the deadline for a status that ends
/// the request, counted from the call's start, its submission included.
#[derive(Debug, Clone, Copy)]
pub struct Pacing {
    /// The wait before the first read.
    pub first: Duration,
    /// The longest wait between two reads.
    pub max: Duration,
    /// From the call's start to the deadline.
    pub wait: Duration,
}

impl Pacing {
    /// The reference's advice for polling: two seconds, growing by half, to
    /// ten. The wait is provisional: how long a picture takes was not
    /// measured, and a request that outlasts it is still charged if it
    /// completes, so it is longer than Gemini's whole call.
    pub const ADVISED: Pacing = Pacing {
        first: Duration::from_secs(2),
        max: Duration::from_secs(10),
        wait: Duration::from_secs(240),
    };
}

/// The whole of a call: the wait, the last status read begun before the
/// deadline, and two fetches of the picture (or the cancel, which is less).
/// A reference's upload is begun before the deadline only, and ends within
/// its two calls of it, less than the fetches.
pub const TIME_LIMIT: Duration = Duration::from_secs(300);
const _: () = assert!(
    Pacing::ADVISED.wait.as_secs() + API_TIMEOUT.as_secs() + 2 * DOWNLOAD_TIMEOUT.as_secs() <= TIME_LIMIT.as_secs()
);
const _: () = assert!(API_TIMEOUT.as_secs() + UPLOAD_TIMEOUT.as_secs() <= API_TIMEOUT.as_secs() + 2 * DOWNLOAD_TIMEOUT.as_secs());
const _: () = assert!(TIME_LIMIT.as_secs() < crate::tools::registry::MAX_TOOL_DURATION.as_secs());

/// The tool's words for a refusal of Higgsfield's API: the API's words and
/// what to do are in `body` already (`api_error`).
pub(crate) fn api_error_words(status: u16, body: &str) -> String {
    format!("Higgsfield answered {status}: {body}")
}

/// The tool's words when a call outlasted its limit, which the provider's
/// own bounds keep it inside of: a backstop.
pub(crate) fn timeout_words(limit: Duration) -> String {
    format!(
        "no answer within {}s, and the call was ended. A request Higgsfield took may still complete and be charged; the Higgsfield console at open.higgsfield.ai lists it.",
        limit.as_secs()
    )
}

/// The wait after `previous`: half again, to `max`.
pub fn next_delay(previous: Duration, max: Duration) -> Duration {
    (previous + previous / 2).min(max)
}

/// Where the operator is told what a stopped call's cancel came to. The
/// turn that made the call has ended by then, so the sentence is a notice
/// of its own (`proactive::tell_operator`).
pub type StopNotice = Arc<dyn Fn(Notification) + Send + Sync>;

/// What a cancel came to. By the reference a cancel is answered 202 when
/// the request is canceled and 400 once the request has started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Canceled {
    Yes,
    /// Too late: the request had started.
    Started,
    /// Another answer of the API.
    Answered(u16),
    /// The cancel did not arrive, with the cause.
    Unreached(String),
}

impl Canceled {
    fn of(answer: Result<reqwest::Response, reqwest::Error>) -> Self {
        match answer {
            Ok(a) if a.status().is_success() => Canceled::Yes,
            Ok(a) if a.status().as_u16() == 400 => Canceled::Started,
            Ok(a) => Canceled::Answered(a.status().as_u16()),
            Err(e) => Canceled::Unreached(net::error_chain(&e)),
        }
    }

    /// The deadline's words: the request was `last` when the wait ran out.
    fn at_the_deadline(&self, last: &str) -> String {
        match self {
            Canceled::Yes => format!("it was {last} and is canceled: nothing is charged"),
            Canceled::Started => format!(
                "it was {last} and had started, so it can no longer be canceled: it is charged if it completes, and the Higgsfield console at open.higgsfield.ai keeps its picture for seven days"
            ),
            Canceled::Answered(status) => {
                format!("it was {last}, and the cancel was answered HTTP {status}: it may still complete, and is charged if it does")
            }
            Canceled::Unreached(why) => {
                format!("it was {last}, and the cancel did not reach Higgsfield ({why}): it may still complete, and is charged if it does")
            }
        }
    }

    /// The notice after an operator's stop: what became of the request the
    /// stopped call had made, and whether it is charged. A request that is
    /// canceled is information; one that may be charged is a notice.
    pub(crate) fn after_a_stop(&self, request_id: &str) -> Notification {
        match self {
            Canceled::Yes => Notification::new(
                Priority::Low,
                format!("The stopped image call's request {request_id} is canceled at Higgsfield: nothing is charged."),
            ),
            Canceled::Started => Notification::new(
                Priority::Normal,
                format!(
                    "The stopped image call's request {request_id} had started at Higgsfield and can no longer be canceled: it is charged if it completes, and the Higgsfield console at open.higgsfield.ai keeps its picture for seven days."
                ),
            ),
            Canceled::Answered(status) => Notification::new(
                Priority::Normal,
                format!(
                    "The cancel of the stopped image call's request {request_id} was answered HTTP {status} by Higgsfield: the request may still complete, and is charged if it does."
                ),
            ),
            Canceled::Unreached(why) => Notification::new(
                Priority::Normal,
                format!(
                    "The cancel of the stopped image call's request {request_id} did not reach Higgsfield ({why}): the request may still complete, and is charged if it does."
                ),
            ),
        }
    }
}

pub struct HiggsfieldImageProvider {
    /// `Key <key id>:<key secret>`, marked sensitive.
    authorization: HeaderValue,
    model: &'static ImageModel,
    /// The API's origin, with a path that ends in `/`.
    base: reqwest::Url,
    /// The base is a stand-in (`EMBRA_HIGGSFIELD_BASE`): its origin is also
    /// taken as it is for a picture's address.
    stand_in: bool,
    pacing: Pacing,
    /// How many cancels a dropped call's guard has sent: none but after a
    /// stop.
    cancels_on_drop: Arc<AtomicUsize>,
    /// The client of the API's own calls. A provider is made for one call
    /// of `image_generate` and keeps it that long.
    http: reqwest::Client,
    /// Where a stopped call's cancel is told.
    on_stop: StopNotice,
}

impl HiggsfieldImageProvider {
    /// `key` is `<key id>:<key secret>`; `model` one of Higgsfield's rows of
    /// `IMAGE_MODELS`; `base` the API's origin (`API_BASE`, or a stand-in);
    /// `on_stop` takes the notice of what a stopped call's cancel came to.
    pub fn new(key: &str, model: &str, base: &str, stand_in: bool, pacing: Pacing, on_stop: StopNotice) -> Result<Self, ImageGenError> {
        let model = image_model(model)
            .filter(|m| m.kind == ImageProviderKind::Higgsfield)
            .ok_or_else(|| ImageGenError::NotConfigured(format!("{model} is not one of Higgsfield's image models")))?;
        let mut authorization = HeaderValue::from_str(&format!("Key {key}"))
            .map_err(|_| ImageGenError::NotConfigured("the Higgsfield key holds a character no header can carry".into()))?;
        authorization.set_sensitive(true);
        let mut base = reqwest::Url::parse(base).map_err(|e| ImageGenError::NotConfigured(format!("the Higgsfield base {base} is not a URL ({e})")))?;
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        let http = net::builder()
            .connect_timeout(net::CONNECT_TIMEOUT)
            .timeout(API_TIMEOUT)
            .build()
            .map_err(|e| ImageGenError::Transport(format!("an HTTP client could not be built: {e}")))?;
        Ok(Self { authorization, model, base, stand_in, pacing, cancels_on_drop: Arc::new(AtomicUsize::new(0)), http, on_stop })
    }

    /// The model's endpoint: the base and the model's id.
    fn endpoint(&self) -> reqwest::Url {
        self.base.join(self.model.id).expect("a model's id joins the base")
    }

    /// The URL the API named for this request when it is of the API's own
    /// origin (a relative one is taken against the base); else the one the
    /// reference documents, built from the request id.
    fn own_url(&self, named: Option<&str>, request_id: &str, what: &str) -> reqwest::Url {
        if let Some(named) = named {
            match self.base.join(named) {
                Ok(url) if net::same_origin(&url, &self.base) => return url,
                _ => tracing::warn!(
                    target: "media",
                    request_id,
                    named = %net::url_shown(&reqwest::Url::parse(named).unwrap_or_else(|_| self.base.clone())),
                    "Higgsfield named a {what} URL of another origin; the request's own is used"
                ),
            }
        }
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("an http base has a path")
            .pop_if_empty()
            .extend(["requests", request_id, what]);
        url
    }

    /// A reference image sent to Higgsfield's storage: an upload slot asked
    /// of the API, the bytes put to it without the credential (through the
    /// one client that may reach the slot's address), and the public URL
    /// that names it in a request.
    async fn upload(&self, media_type: &str, bytes: &[u8]) -> Result<String, ImageGenError> {
        let mut slot_url = self.base.clone();
        slot_url.path_segments_mut().expect("an http base has a path").pop_if_empty().extend(["files", "generate-upload-url"]);
        let answer = self
            .http
            .post(slot_url)
            .header(AUTHORIZATION, self.authorization.clone())
            .json(&json!({"content_type": media_type}))
            .send()
            .await
            .map_err(|e| ImageGenError::Transport(format!("Higgsfield's upload slot: {}", net::error_chain(&e))))?;
        if !answer.status().is_success() {
            return Err(api_error(answer, Asked::Storage).await);
        }
        let bytes_read = net::read_capped(answer, API_ANSWER_MAX)
            .await
            .map_err(|e| ImageGenError::Transport(format!("Higgsfield's upload slot: {e}")))?;
        let slot: JsonValue = serde_json::from_slice(&bytes_read)
            .map_err(|e| ImageGenError::Transport(format!("Higgsfield's upload slot is not JSON ({e})")))?;
        let UploadSlot { public_url, upload_url, headers } = parse_upload_slot(&slot).map_err(ImageGenError::Transport)?;
        let also = self.stand_in.then_some(&self.base);
        let reach = net::public_url_client(&upload_url, also, UPLOAD_TIMEOUT)
            .await
            .map_err(|e| ImageGenError::Transport(format!("Higgsfield's upload slot is refused: {e}; nothing was submitted, nothing is charged")))?;
        let mut put = reach.client.put(reach.url.clone()).body(bytes.to_vec());
        for (name, value) in &headers {
            put = put.header(name.as_str(), value.as_str());
        }
        let answer = put
            .send()
            .await
            .map_err(|e| ImageGenError::Transport(format!("a reference image could not be put to Higgsfield's storage: {}", net::error_chain(&e))))?;
        if !answer.status().is_success() {
            return Err(ImageGenError::Transport(format!(
                "Higgsfield's storage answered HTTP {} to a reference image; nothing was submitted, nothing is charged",
                answer.status().as_u16()
            )));
        }
        tracing::info!(target: "media", host = %net::host_shown(&reach.url), "a reference image was put to Higgsfield's storage");
        Ok(public_url)
    }

    /// The submission: once, and once more with the same idempotency key
    /// after a network failure or a 5xx; never after a 4xx.
    async fn submit(&self, body: &JsonValue) -> Result<Submitted, ImageGenError> {
        let key = uuid::Uuid::new_v4().to_string();
        let mut last = String::new();
        for attempt in 1..=2 {
            let sent = self
                .http
                .post(self.endpoint())
                .header(AUTHORIZATION, self.authorization.clone())
                .header("Idempotency-Key", &key)
                .json(body)
                .send()
                .await;
            match sent {
                Err(e) => last = net::error_chain(&e),
                Ok(answer) if answer.status().is_server_error() && attempt == 1 => {
                    last = format!("HTTP {}", answer.status().as_u16());
                }
                Ok(answer) if !answer.status().is_success() => return Err(api_error(answer, Asked::Model).await),
                Ok(answer) => {
                    let bytes = net::read_capped(answer, API_ANSWER_MAX)
                        .await
                        .map_err(|e| ImageGenError::Transport(format!("Higgsfield's answer to the submission: {e}")))?;
                    let value: JsonValue = serde_json::from_slice(&bytes)
                        .map_err(|e| ImageGenError::Transport(format!("Higgsfield's answer to the submission is not JSON ({e})")))?;
                    return parse_submit(&value).map_err(ImageGenError::Transport);
                }
            }
            if attempt == 1 {
                tracing::info!(target: "media", why = %last, "Higgsfield's submission failed; sent once more with the same idempotency key");
            }
        }
        Err(ImageGenError::Transport(format!("the submission to Higgsfield failed twice: {last}")))
    }

    /// The status reads until one ends the request, a read that ends the
    /// call, or the deadline. A network failure, a 5xx or a 429 is no end:
    /// the read is made again, after the wait the 429 names when it names
    /// one.
    async fn wait(&self, status_url: &reqwest::Url, deadline: Instant) -> Waited {
        let mut delay = self.pacing.first;
        let mut last = "queued".to_string();
        let mut told_unknown = false;
        loop {
            if Instant::now() + delay >= deadline {
                return Waited::Deadline(last);
            }
            tokio::time::sleep(delay).await;
            let mut next = next_delay(delay, self.pacing.max);
            match self.http.get(status_url.clone()).header(AUTHORIZATION, self.authorization.clone()).send().await {
                Err(e) => tracing::info!(target: "media", why = %net::error_chain(&e), "a status read of Higgsfield failed; read again"),
                Ok(answer) if answer.status().is_server_error() || answer.status().as_u16() == 429 => {
                    if let Some(after) = retry_after(&answer) {
                        next = next.max(after);
                    }
                    tracing::info!(target: "media", status = answer.status().as_u16(), "Higgsfield answered a status read with an error; read again");
                }
                Ok(answer) if !answer.status().is_success() => return Waited::Failed(api_error(answer, Asked::Model).await),
                Ok(answer) => {
                    let read = net::read_capped(answer, API_ANSWER_MAX)
                        .await
                        .and_then(|bytes| serde_json::from_slice::<JsonValue>(&bytes).map_err(|e| format!("not JSON ({e})")))
                        .and_then(|value| parse_status(&value));
                    match read {
                        Ok(Status::Waiting(word)) => {
                            if !matches!(word.as_str(), "queued" | "in_progress") && !told_unknown {
                                told_unknown = true;
                                tracing::warn!(target: "media", status = %word, "Higgsfield named a status this OS does not know; the wait goes on");
                            }
                            last = word;
                        }
                        Ok(ended) => return Waited::Ended(ended),
                        Err(e) => tracing::info!(target: "media", why = %e, "a status of Higgsfield could not be read; read again"),
                    }
                }
            }
            delay = next;
        }
    }

    /// At the deadline: the request is asked to cancel, once, and the error
    /// says which it came to.
    async fn cancel_at_deadline(&self, cancel_url: &reqwest::Url, request_id: &str, last: &str) -> ImageGenError {
        let answer = self.http.post(cancel_url.clone()).header(AUTHORIZATION, self.authorization.clone()).send().await;
        let detail = Canceled::of(answer).at_the_deadline(last);
        ImageGenError::StillRunning { request_id: request_id.to_string(), waited_secs: self.pacing.wait.as_secs(), detail }
    }

    /// The picture of a completed request, from the address the status
    /// named: through the one client that may reach it, capped at what a
    /// generated picture is taken at (one over the media store's ceiling
    /// is fitted by the tool), and fetched once more after a network
    /// failure. A
    /// picture that cannot be fetched is paid for: the error names the
    /// request and the address. A fetch that works leaves the host in the
    /// log.
    async fn download(&self, url: &str, request_id: &str) -> Result<GeneratedImage, ImageGenError> {
        let paid = |why: &str| {
            ImageGenError::NoImage(format!(
                "the picture of Higgsfield's request {request_id} could not be fetched from {url}: {why}. It is paid for: the address works for seven days, and the Higgsfield console at open.higgsfield.ai keeps it"
            ))
        };
        let also = self.stand_in.then_some(&self.base);
        let reach = net::public_url_client(url, also, DOWNLOAD_TIMEOUT).await.map_err(|e| paid(&e))?;
        let mut last = String::new();
        for attempt in 1..=2 {
            match reach.client.get(reach.url.clone()).send().await {
                Err(e) => {
                    last = net::error_chain(&e);
                    if attempt == 1 {
                        tracing::info!(target: "media", why = %last, "the fetch of Higgsfield's picture failed; fetched once more");
                    }
                }
                Ok(answer) if !answer.status().is_success() => return Err(paid(&format!("HTTP {}", answer.status().as_u16()))),
                Ok(answer) => {
                    let bytes =
                        net::read_capped(answer, crate::media::MEDIA_GENERATED_RAW_MAX).await.map_err(|e| paid(&e))?;
                    tracing::info!(
                        target: "media",
                        request_id,
                        host = %net::host_shown(&reach.url),
                        bytes = bytes.len(),
                        "Higgsfield's picture was fetched"
                    );
                    let media_type = crate::media::ingest::sniff(&bytes).map_or("application/octet-stream", |kind| kind.media_type());
                    return Ok(GeneratedImage { media_type: media_type.to_string(), data: bytes });
                }
            }
        }
        Err(paid(&last))
    }
}

#[async_trait::async_trait]
impl ImageProvider for HiggsfieldImageProvider {
    fn kind(&self) -> ImageProviderKind {
        ImageProviderKind::Higgsfield
    }

    fn model(&self) -> &str {
        self.model.id
    }

    async fn generate(&self, req: &ImageGenRequest) -> Result<ImageGenResponse, ImageGenError> {
        let deadline = Instant::now() + self.pacing.wait;
        // Each reference goes to Higgsfield's storage first; the request
        // names it by its public URL. Nothing is submitted, and nothing is
        // charged, when one cannot be sent or the wait ran out meanwhile: an
        // upload is begun before the deadline only.
        let mut reference_urls = Vec::with_capacity(req.reference_images.len());
        for (media_type, bytes) in &req.reference_images {
            reference_urls.push(self.upload(media_type, bytes).await?);
            if Instant::now() >= deadline {
                return Err(ImageGenError::NoImage(
                    "the reference images took the whole wait; nothing was submitted, nothing is charged".into(),
                ));
            }
        }
        let body = request_body(self.model, req, &reference_urls);
        let submitted = self.submit(&body).await?;
        let request_id = submitted.request_id.as_str();
        let status_url = self.own_url(submitted.status_url.as_deref(), request_id, "status");
        let cancel_url = self.own_url(submitted.cancel_url.as_deref(), request_id, "cancel");
        tracing::info!(target: "media", request_id, model = self.model.id, "Higgsfield took the request");
        // From here to an end of the request or the deadline's own cancel,
        // a call that is dropped (the operator's stop) asks for the cancel
        // and tells the operator what it came to. A stop before the
        // submission's answer leaves the request running: there is no
        // request id to cancel by.
        let mut guard = CancelOnDrop {
            armed: Some(Armed {
                http: self.http.clone(),
                authorization: self.authorization.clone(),
                cancel_url: cancel_url.clone(),
                request_id: request_id.to_string(),
                tell: self.on_stop.clone(),
            }),
            sent: self.cancels_on_drop.clone(),
        };
        match self.wait(&status_url, deadline).await {
            Waited::Deadline(last) => {
                guard.armed = None;
                Err(self.cancel_at_deadline(&cancel_url, request_id, &last).await)
            }
            Waited::Failed(e) => Err(e),
            Waited::Ended(status) => {
                guard.armed = None;
                match status {
                    Status::Completed(urls) => {
                        let url = urls.first().ok_or_else(|| {
                            ImageGenError::NoImage(format!("Higgsfield's request {request_id} completed and named no picture"))
                        })?;
                        let image = self.download(url, request_id).await?;
                        Ok(ImageGenResponse { images: vec![image], text: None })
                    }
                    Status::Failed(error) => Err(ImageGenError::NoImage(format!(
                        "Higgsfield's request {request_id} failed{}; a failed request is not charged",
                        error.map(|e| format!(": {e}")).unwrap_or_default()
                    ))),
                    Status::Nsfw => Err(ImageGenError::NoImage(format!(
                        "Higgsfield's moderation refused the prompt or the picture of request {request_id}; a refused request is not charged"
                    ))),
                    Status::Canceled => Err(ImageGenError::NoImage(format!(
                        "Higgsfield's request {request_id} was canceled; nothing is charged"
                    ))),
                    Status::Waiting(_) => unreachable!("the wait ends on a status that ends the request"),
                }
            }
        }
    }
}

/// How a wait ended.
enum Waited {
    Ended(Status),
    /// The deadline, with the last status read.
    Deadline(String),
    /// A read the API refused (a 4xx but 429).
    Failed(ImageGenError),
}

/// Sends the cancel of a request when it is dropped armed: the call it
/// belongs to was dropped before the request ended. One cancel at most, from
/// a task of its own, when a runtime is there to run it; what the cancel
/// came to goes to the log and, as a notice, to the operator.
struct CancelOnDrop {
    armed: Option<Armed>,
    sent: Arc<AtomicUsize>,
}

/// What the guard cancels a request with, and whom it tells.
struct Armed {
    http: reqwest::Client,
    authorization: HeaderValue,
    cancel_url: reqwest::Url,
    request_id: String,
    tell: StopNotice,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let Some(Armed { http, authorization, cancel_url, request_id, tell }) = self.armed.take() else { return };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
        self.sent.fetch_add(1, Ordering::SeqCst);
        runtime.spawn(async move {
            let answer = http.post(cancel_url).header(AUTHORIZATION, authorization).send().await;
            match &answer {
                Ok(a) => tracing::info!(target: "media", request_id, status = a.status().as_u16(), "the stopped call's Higgsfield request was asked to cancel"),
                Err(e) => tracing::info!(target: "media", request_id, why = %net::error_chain(e), "the cancel of a stopped call's Higgsfield request did not reach it"),
            }
            tell(Canceled::of(answer).after_a_stop(&request_id));
        });
    }
}

/// The submission's answer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Submitted {
    pub(crate) request_id: String,
    pub(crate) status_url: Option<String>,
    pub(crate) cancel_url: Option<String>,
}

/// A status as the reference words it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Status {
    /// `queued`, `in_progress`, or a word this OS does not know: the wait
    /// goes on.
    Waiting(String),
    Completed(Vec<String>),
    Failed(Option<String>),
    Nsfw,
    Canceled,
}

/// The request's body: the prompt, the ratio and the model's size word for
/// the size when they are asked, the references' public URLs in the model's
/// own field, a reference's weight where the model has that setting and a
/// reference goes with it, and nothing else, so the model's own defaults
/// stand for the rest.
pub(crate) fn request_body(model: &ImageModel, req: &ImageGenRequest, reference_urls: &[String]) -> JsonValue {
    let mut body = Map::new();
    body.insert("prompt".into(), json!(req.prompt));
    if let Some(ratio) = &req.aspect_ratio {
        body.insert("aspect_ratio".into(), json!(ratio));
    }
    if let Some(value) = req.size.as_deref().and_then(|size| model.api_size(size)) {
        body.insert("resolution".into(), json!(value));
    }
    match model.references {
        References::Urls { field, .. } if !reference_urls.is_empty() => {
            body.insert(field.into(), json!(reference_urls));
        }
        References::Url { field } => {
            if let Some(first) = reference_urls.first() {
                body.insert(field.into(), json!(first));
            }
        }
        _ => {}
    }
    if let (Some(weight), Some(setting)) = (req.reference_weight, model.reference_weight)
        && !reference_urls.is_empty()
    {
        body.insert(setting.field.into(), json!(weight));
    }
    JsonValue::Object(body)
}

/// A slot in Higgsfield's storage for one reference.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct UploadSlot {
    /// The address a request names the reference by.
    pub(crate) public_url: String,
    /// The address its bytes are put to.
    pub(crate) upload_url: String,
    /// The headers to put them with: every one the answer names.
    pub(crate) headers: Vec<(String, String)>,
}

/// An upload slot, as the answer to `POST /files/generate-upload-url`
/// names it; every header is text.
pub(crate) fn parse_upload_slot(value: &JsonValue) -> Result<UploadSlot, String> {
    let text = |field: &str| value.get(field).and_then(JsonValue::as_str).filter(|v| !v.trim().is_empty()).map(str::to_string);
    let public_url = text("public_url").ok_or_else(|| "Higgsfield's upload slot names no public_url".to_string())?;
    let upload_url = text("upload_url").ok_or_else(|| "Higgsfield's upload slot names no upload_url".to_string())?;
    let mut headers = Vec::new();
    if let Some(named) = value.get("upload_headers").and_then(JsonValue::as_object) {
        for (name, value) in named {
            let value = value.as_str().ok_or_else(|| format!("Higgsfield's upload slot names a header {name} that is not text"))?;
            headers.push((name.clone(), value.to_string()));
        }
    }
    Ok(UploadSlot { public_url, upload_url, headers })
}

pub(crate) fn parse_submit(value: &JsonValue) -> Result<Submitted, String> {
    let text = |field: &str| value.get(field).and_then(JsonValue::as_str).map(str::to_string);
    let request_id = text("request_id")
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| "Higgsfield's answer to the submission names no request_id".to_string())?;
    Ok(Submitted { request_id, status_url: text("status_url"), cancel_url: text("cancel_url") })
}

pub(crate) fn parse_status(value: &JsonValue) -> Result<Status, String> {
    let word = value
        .get("status")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "the answer names no status".to_string())?;
    Ok(match word {
        "completed" => Status::Completed(
            value
                .get("images")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
                .filter_map(|image| image.get("url").and_then(JsonValue::as_str).map(str::to_string))
                .collect(),
        ),
        "failed" => Status::Failed(value.get("error").and_then(JsonValue::as_str).map(str::to_string)),
        "nsfw" => Status::Nsfw,
        "canceled" => Status::Canceled,
        other => Status::Waiting(other.to_string()),
    })
}

/// An error's `detail` as words: a string as it is; a list (a validation
/// error) as each item's place and message; an object by its message.
pub(crate) fn detail_text(body: &[u8]) -> String {
    let text = match serde_json::from_slice::<JsonValue>(body) {
        Ok(value) => match value.get("detail") {
            Some(JsonValue::String(text)) => text.clone(),
            Some(JsonValue::Array(items)) => items
                .iter()
                .map(|item| {
                    let place = item.get("loc").and_then(JsonValue::as_array).and_then(|loc| loc.last()).map(|last| match last {
                        JsonValue::String(s) => s.clone(),
                        other => other.to_string(),
                    });
                    let message = item.get("msg").and_then(JsonValue::as_str).map(str::to_string).unwrap_or_else(|| item.to_string());
                    match place {
                        Some(place) => format!("{place}: {message}"),
                        None => message,
                    }
                })
                .collect::<Vec<_>>()
                .join("; "),
            Some(JsonValue::Object(object)) => object
                .get("message")
                .and_then(JsonValue::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| JsonValue::Object(object.clone()).to_string()),
            Some(other) => other.to_string(),
            None => value.to_string(),
        },
        Err(_) => String::from_utf8_lossy(body).into_owned(),
    };
    let text = text.trim();
    match text.char_indices().nth(DETAIL_SHOWN_MAX) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

/// A `Retry-After` of seconds.
fn retry_after(answer: &reqwest::Response) -> Option<Duration> {
    answer
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// What a refused call asked for: the model's request (its submission or
/// its status), or a slot in Higgsfield's storage for a reference.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Asked {
    Model,
    Storage,
}

/// An answer of the API that refuses: its status, its words, and what the
/// operator can do about it. A model's advice (open to the account, blocked
/// for a while) is given only for a call of the model's.
async fn api_error(answer: reqwest::Response, asked: Asked) -> ImageGenError {
    let status = answer.status().as_u16();
    let after = retry_after(&answer);
    let correlation = answer
        .headers()
        .get("x-correlation-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let detail = match net::read_capped(answer, API_ANSWER_MAX).await {
        Ok(bytes) => detail_text(&bytes),
        Err(e) => e,
    };
    let detail = if detail.is_empty() { format!("HTTP {status}") } else { detail };
    let advice = match status {
        401 => " The key was refused: /image-provider key <key id>:<key secret> stores another (the API console at open.higgsfield.ai makes keys).".to_string(),
        403 => " Higgsfield refused the account: the API's balance at open.higgsfield.ai may be empty, or the key disabled.".to_string(),
        404 if asked == Asked::Model => " The model is not open to this account: /image-provider model <id> picks another.".to_string(),
        423 if asked == Asked::Model => " The model is blocked for a while: try again later, or pick another with /image-provider model <id>.".to_string(),
        429 => match after {
            Some(after) => format!(" The account's queue is full: try again in {} s.", after.as_secs()),
            None => " The account's queue is full: try again later.".to_string(),
        },
        400 | 422 => " The request was refused as it was sent.".to_string(),
        500.. => " Higgsfield's side failed: try again later.".to_string(),
        _ => String::new(),
    };
    let correlation = correlation.map(|id| format!(" (Higgsfield's correlation id {id})")).unwrap_or_default();
    ImageGenError::Api { status, body: format!("{detail}.{advice}{correlation}") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::image::validate_request;
    use std::sync::{Arc, Mutex};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    const NOW: Duration = Duration::ZERO;
    /// No waits between reads, and a deadline no test reaches.
    const QUICK: Pacing = Pacing { first: NOW, max: NOW, wait: Duration::from_secs(3600) };
    /// The deadline at the call's start: no status is read.
    const AT_ONCE: Pacing = Pacing { first: NOW, max: NOW, wait: NOW };
    const KEY: &str = "key-id-1:key-secret-1";

    fn req(prompt: &str, ratio: Option<&str>, size: Option<&str>) -> ImageGenRequest {
        ImageGenRequest {
            prompt: prompt.into(),
            aspect_ratio: ratio.map(str::to_string),
            size: size.map(str::to_string),
            reference_images: Vec::new(),
            reference_weight: None,
        }
    }

    /// A provider that tells nobody of a stop: for a test of anything else.
    fn silent() -> StopNotice {
        Arc::new(|_| {})
    }

    fn provider(server: &MockServer, model: &str, pacing: Pacing) -> HiggsfieldImageProvider {
        HiggsfieldImageProvider::new(KEY, model, &server.uri(), true, pacing, silent()).expect("a provider")
    }

    fn png() -> Vec<u8> {
        crate::media::ingest::tests::png_fixture(4, 3)
    }

    /// The submission's answer of the stand-in: its own URLs.
    fn queued(server: &MockServer, id: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "status": "queued",
            "request_id": id,
            "status_url": format!("{}/requests/{id}/status", server.uri()),
            "cancel_url": format!("{}/requests/{id}/cancel", server.uri()),
        }))
    }

    fn status(word: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"status": word, "request_id": "r-1"}))
    }

    async fn mount_once(server: &MockServer, verb: &str, at: &str, answer: ResponseTemplate) {
        Mock::given(method(verb)).and(path(at)).respond_with(answer).up_to_n_times(1).mount(server).await;
    }

    /// A responder that tells a channel each time it is asked.
    struct Told {
        tell: tokio::sync::mpsc::UnboundedSender<()>,
        answer: ResponseTemplate,
    }

    impl Respond for Told {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            let _ = self.tell.send(());
            self.answer.clone()
        }
    }

    /// The values of each model's page, typed here from the page's input
    /// schema (`docs.higgsfield.ai/docs/models/<family>/<workflow>.md`,
    /// read 2026-11-24) and not from the table under test: the endpoint,
    /// the field of its reference images and how many it takes,
    /// `resolution`'s values, `aspect_ratio`'s values, the prompt's bounds
    /// in characters, the weight of a reference where the page has that
    /// setting (read 2026-11-25), and every property the schema has.
    /// How the page takes reference images.
    enum PageReferences {
        None,
        List { field: &'static str, max: usize },
        One { field: &'static str },
    }

    struct Page {
        endpoint: &'static str,
        references: PageReferences,
        resolutions: &'static [&'static str],
        ratios: &'static [&'static str],
        prompt: Option<(usize, usize)>,
        /// The field and the bounds of a reference's weight, where the page
        /// has one: it "applies to editing requests only".
        weight: Option<(&'static str, u32, u32)>,
        properties: &'static [&'static str],
    }

    const PAGES: [Page; 4] = [
        Page {
            endpoint: "xai/grok-imagine-image-2.0",
            references: PageReferences::List { field: "image_urls", max: 10 },
            resolutions: &["1k", "2k"],
            ratios: &["auto", "1:1", "1:2", "2:1", "3:2", "2:3", "4:3", "3:4", "16:9", "9:16"],
            prompt: None,
            weight: None,
            properties: &["prompt", "quality", "image_urls", "resolution", "aspect_ratio"],
        },
        Page {
            endpoint: "higgsfield-ai/soul/v2/standard",
            references: PageReferences::None,
            resolutions: &["720p", "1080p"],
            ratios: &["9:16", "16:9", "4:3", "3:4", "1:1", "2:3", "3:2"],
            prompt: None,
            weight: None,
            properties: &[
                "prompt", "aspect_ratio", "resolution", "enhance_prompt", "custom_reference_id", "custom_reference_strength",
                "style_id", "style_strength", "seed", "batch_size",
            ],
        },
        Page {
            endpoint: "ideogram/v4.0",
            references: PageReferences::One { field: "image_url" },
            resolutions: &[],
            ratios: &[
                "1:1", "1:2", "2:1", "2:3", "3:2", "4:5", "5:4", "9:16", "16:9", "5:8", "8:5", "3:4", "4:3", "9:22", "22:9",
                "9:23", "23:9", "3:8", "8:3", "5:12", "12:5", "1:3", "3:1",
            ],
            prompt: Some((2, 2048)),
            weight: Some(("image_weight", 1, 100)),
            properties: &["prompt", "image_url", "aspect_ratio", "image_weight", "rendering_speed"],
        },
        Page {
            endpoint: "z-image/turbo",
            references: PageReferences::None,
            resolutions: &["1k", "2k"],
            ratios: &["1:1", "2:3", "3:2", "3:4", "4:3", "7:9", "9:7", "9:16", "16:9", "21:9"],
            prompt: Some((1, 800)),
            weight: None,
            properties: &["seed", "prompt", "resolution", "aspect_ratio", "prompt_extend"],
        },
    ];

    /// Whatever the validator lets through reaches Higgsfield as values of
    /// the model's page: every model, every size word of the provider and
    /// every ratio any model has, with prompts at and past each bound, and
    /// a reference's weight at and past its bounds, with a reference and
    /// without. And every value of a page the tool offers is let through.
    #[test]
    fn every_request_the_validator_lets_through_carries_values_of_each_models_page() {
        use crate::provider::image::{HIGGSFIELD_IMAGE_MODELS, IMAGE_MODELS};
        let endpoints: Vec<&str> = PAGES.iter().map(|page| page.endpoint).collect();
        assert_eq!(HIGGSFIELD_IMAGE_MODELS, endpoints.as_slice(), "one page a model");
        let words = ImageProviderKind::Higgsfield.size_words();
        let mut ratios: Vec<&str> = IMAGE_MODELS.iter().flat_map(|m| m.ratios.iter().copied()).collect();
        ratios.extend(["auto", "7:3", ""]);
        const WEIGHTS: [Option<u32>; 6] = [None, Some(0), Some(1), Some(50), Some(100), Some(101)];
        let mut sent = 0;
        for page in &PAGES {
            let model = image_model(page.endpoint).expect("a row of the table");
            let prompts: Vec<String> = match page.prompt {
                Some((least, most)) => vec!["x".repeat(least), "x".repeat(most), "x".repeat(most + 1), "x".repeat(least.saturating_sub(1))],
                None => vec!["a lighthouse".to_string(), "x".repeat(5000)],
            };
            for prompt in &prompts {
                for size in words.iter().copied().map(Some).chain([None, Some("4K"), Some("1k")]) {
                    let asked = ratios
                        .iter()
                        .copied()
                        .map(Some)
                        .chain([None])
                        .flat_map(|ratio| [0usize, 1, 2, 4].into_iter().flat_map(move |refs| WEIGHTS.map(move |weight| (ratio, refs, weight))));
                    for (ratio, refs, weight) in asked {
                        let mut request = req(prompt, ratio, size);
                        request.reference_images = vec![("image/png".to_string(), vec![1u8]); refs];
                        request.reference_weight = weight;
                        if validate_request(page.endpoint, &request).is_err() {
                            continue;
                        }
                        let urls: Vec<String> = (0..refs).map(|n| format!("https://cdn.example/in/{n}.png")).collect();
                        let body = request_body(model, &request, &urls);
                        let fields = body.as_object().expect("an object");
                        for key in fields.keys() {
                            assert!(page.properties.contains(&key.as_str()), "{}: {key} is no property of its page", page.endpoint);
                        }
                        if let Some(value) = fields.get("resolution") {
                            assert!(page.resolutions.contains(&value.as_str().unwrap()), "{}: {value}", page.endpoint);
                        }
                        if let Some(value) = fields.get("aspect_ratio") {
                            assert!(page.ratios.contains(&value.as_str().unwrap()), "{}: {value}", page.endpoint);
                        }
                        if let Some((least, most)) = page.prompt {
                            let n = fields["prompt"].as_str().unwrap().chars().count();
                            assert!((least..=most).contains(&n), "{}: a prompt of {n}", page.endpoint);
                        }
                        match page.references {
                            PageReferences::List { field, max } => {
                                let sent_refs = fields.get(field).map_or(0, |v| v.as_array().unwrap().len());
                                assert!(sent_refs <= max && sent_refs == refs, "{}: {sent_refs} of {refs}", page.endpoint);
                            }
                            PageReferences::One { field } => {
                                assert!(refs <= 1, "{}: {refs} references let through", page.endpoint);
                                assert_eq!(fields.contains_key(field), refs == 1, "{}", page.endpoint);
                                assert!(fields.get(field).is_none_or(JsonValue::is_string), "{}: {field} is a string", page.endpoint);
                            }
                            PageReferences::None => assert_eq!(refs, 0, "{}: a reference let through", page.endpoint),
                        }
                        match page.weight {
                            Some((field, least, most)) => {
                                let sent_weight = fields.get(field);
                                assert_eq!(sent_weight.is_some(), weight.is_some(), "{}: a weight asked for is the weight sent", page.endpoint);
                                if let Some(value) = sent_weight {
                                    let n = value.as_u64().expect("an integer");
                                    assert!((u64::from(least)..=u64::from(most)).contains(&n), "{}: a weight of {n}", page.endpoint);
                                    assert!(refs >= 1, "{}: a weight without its reference", page.endpoint);
                                }
                            }
                            None => assert!(weight.is_none(), "{}: a weight let through", page.endpoint),
                        }
                        sent += 1;
                    }
                }
            }
            // What the page offers and the tool takes is let through: each of
            // its ratios but auto, and a size word for each of its values.
            for ratio in page.ratios.iter().filter(|r| **r != "auto") {
                assert!(validate_request(page.endpoint, &req("a lighthouse", Some(ratio), None)).is_ok(), "{} {ratio}", page.endpoint);
            }
            for value in page.resolutions {
                let word = model.sizes.iter().find(|(_, v)| v == value).map(|(w, _)| *w);
                assert!(word.is_some(), "{}: no size word sends {value}", page.endpoint);
            }
            // The weight of a reference: the row has the page's field and
            // bounds, and both ends of them are let through.
            assert_eq!(model.reference_weight.map(|w| (w.field, w.min, w.max)), page.weight, "{}", page.endpoint);
            if let Some((_, least, most)) = page.weight {
                for weight in [least, most] {
                    let mut request = req("a lighthouse", None, None);
                    request.reference_images = vec![("image/png".to_string(), vec![1u8])];
                    request.reference_weight = Some(weight);
                    assert!(validate_request(page.endpoint, &request).is_ok(), "{} at {weight}", page.endpoint);
                }
            }
        }
        assert!(sent > 200, "{sent} requests were let through: the loops did not run");
    }

    #[test]
    fn the_body_holds_what_was_asked_and_nothing_else() {
        let grok = image_model("xai/grok-imagine-image-2.0").unwrap();
        assert_eq!(request_body(grok, &req("a lighthouse", None, None), &[]), json!({"prompt": "a lighthouse"}));
        assert_eq!(
            request_body(grok, &req("a lighthouse", Some("16:9"), Some("2K")), &[]),
            json!({"prompt": "a lighthouse", "aspect_ratio": "16:9", "resolution": "2k"})
        );
        let soul = image_model("higgsfield-ai/soul/v2/standard").unwrap();
        assert_eq!(
            request_body(soul, &req("a portrait", Some("3:4"), Some("1080p")), &[]),
            json!({"prompt": "a portrait", "aspect_ratio": "3:4", "resolution": "1080p"})
        );
        let urls = ["https://cdn/in/1.png".to_string(), "https://cdn/in/2.png".to_string()];
        assert_eq!(
            request_body(grok, &req("the same, at night", None, None), &urls),
            json!({"prompt": "the same, at night", "image_urls": ["https://cdn/in/1.png", "https://cdn/in/2.png"]})
        );
        let ideogram = image_model("ideogram/v4.0").unwrap();
        assert_eq!(
            request_body(ideogram, &req("the same, in ink", None, None), &urls[..1]),
            json!({"prompt": "the same, in ink", "image_url": "https://cdn/in/1.png"})
        );
        // A reference's weight goes with its reference, on the model whose
        // page has the setting, as an integer.
        let mut weighted = req("the same, in ink", None, None);
        weighted.reference_weight = Some(40);
        assert_eq!(
            request_body(ideogram, &weighted, &urls[..1]),
            json!({"prompt": "the same, in ink", "image_url": "https://cdn/in/1.png", "image_weight": 40})
        );
        assert_eq!(request_body(ideogram, &weighted, &[]), json!({"prompt": "the same, in ink"}), "no reference, no weight");
        assert_eq!(
            request_body(grok, &weighted, &urls[..1]),
            json!({"prompt": "the same, in ink", "image_urls": ["https://cdn/in/1.png"]}),
            "a model without the setting is sent none"
        );
        assert_eq!(
            parse_upload_slot(&json!({"public_url": "https://cdn/in/1.png", "upload_url": "https://storage/put", "content_type": "image/png", "upload_headers": {"Content-Type": "image/png", "x-amz-tagging": "retention=temporary"}})),
            Ok(UploadSlot {
                public_url: "https://cdn/in/1.png".into(),
                upload_url: "https://storage/put".into(),
                headers: vec![("Content-Type".into(), "image/png".into()), ("x-amz-tagging".into(), "retention=temporary".into())],
            })
        );
        assert!(parse_upload_slot(&json!({"upload_url": "https://storage/put"})).is_err());
        assert!(parse_upload_slot(&json!({"public_url": "https://cdn/in/1.png", "upload_url": "https://storage/put", "upload_headers": {"x": 1}})).is_err());
    }

    #[test]
    fn the_answers_are_read_as_the_reference_words_them() {
        assert_eq!(
            parse_submit(&json!({"status": "queued", "request_id": "d7e6", "status_url": "https://a/s", "cancel_url": "https://a/c"})),
            Ok(Submitted { request_id: "d7e6".into(), status_url: Some("https://a/s".into()), cancel_url: Some("https://a/c".into()) })
        );
        assert_eq!(parse_submit(&json!({"request_id": "d7e6"})).unwrap().status_url, None);
        assert!(parse_submit(&json!({"status": "queued"})).is_err());
        assert!(parse_submit(&json!({"status": "queued", "request_id": " "})).is_err());
        assert_eq!(parse_status(&json!({"status": "queued"})), Ok(Status::Waiting("queued".into())));
        assert_eq!(parse_status(&json!({"status": "in_progress"})), Ok(Status::Waiting("in_progress".into())));
        assert_eq!(parse_status(&json!({"status": "rendering"})), Ok(Status::Waiting("rendering".into())), "a word this OS does not know waits");
        assert_eq!(
            parse_status(&json!({"status": "completed", "images": [{"url": "https://cdn/1.jpg"}, {"url": "https://cdn/2.jpg"}]})),
            Ok(Status::Completed(vec!["https://cdn/1.jpg".into(), "https://cdn/2.jpg".into()]))
        );
        assert_eq!(parse_status(&json!({"status": "completed"})), Ok(Status::Completed(vec![])));
        assert_eq!(parse_status(&json!({"status": "failed", "error": "Generation failed"})), Ok(Status::Failed(Some("Generation failed".into()))));
        assert_eq!(parse_status(&json!({"status": "nsfw"})), Ok(Status::Nsfw));
        assert_eq!(parse_status(&json!({"status": "canceled"})), Ok(Status::Canceled));
        assert!(parse_status(&json!({"request_id": "x"})).is_err());
        assert_eq!(detail_text(br#"{"detail": "Invalid credentials"}"#), "Invalid credentials");
        assert_eq!(
            detail_text(br#"{"detail": [{"loc": ["body", "aspect_ratio"], "msg": "Input should be '1:1'", "type": "enum"}, {"msg": "bad"}]}"#),
            "aspect_ratio: Input should be '1:1'; bad"
        );
        assert_eq!(
            detail_text(br#"{"detail": {"code": "account_queue_full", "message": "Too many queued generations for this account", "retryable": true}}"#),
            "Too many queued generations for this account"
        );
        assert_eq!(detail_text(b"Bad Gateway"), "Bad Gateway");
        assert_eq!(detail_text(format!("{{\"detail\": \"{}\"}}", "é".repeat(700)).as_bytes()).chars().count(), DETAIL_SHOWN_MAX + 1);
    }

    #[test]
    fn the_reads_are_paced_as_the_reference_advises() {
        let mut delay = Pacing::ADVISED.first;
        let mut ladder = vec![delay.as_millis()];
        for _ in 0..6 {
            delay = next_delay(delay, Pacing::ADVISED.max);
            ladder.push(delay.as_millis());
        }
        assert_eq!(ladder, [2000, 3000, 4500, 6750, 10000, 10000, 10000]);
    }

    /// The whole of a call on the stand-in: the submission with the key and
    /// an idempotency key, two reads, the picture fetched without the key;
    /// no cancel is asked of a request that ended.
    #[tokio::test]
    async fn a_request_is_submitted_waited_for_and_its_picture_fetched_without_the_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/xai/grok-imagine-image-2.0"))
            .and(header("authorization", "Key key-id-1:key-secret-1"))
            .respond_with(queued(&server, "r-1"))
            .expect(1)
            .mount(&server)
            .await;
        mount_once(&server, "GET", "/requests/r-1/status", status("queued")).await;
        mount_once(&server, "GET", "/requests/r-1/status", status("in_progress")).await;
        Mock::given(method("GET"))
            .and(path("/requests/r-1/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "completed", "request_id": "r-1", "images": [{"url": format!("{}/out/r-1.png", server.uri())}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/out/r-1.png")).respond_with(ResponseTemplate::new(200).set_body_bytes(png())).mount(&server).await;
        Mock::given(method("POST")).and(path("/requests/r-1/cancel")).respond_with(ResponseTemplate::new(202)).expect(0).mount(&server).await;
        let p = provider(&server, "xai/grok-imagine-image-2.0", QUICK);
        let out = p.generate(&req("a lighthouse", Some("16:9"), Some("2K"))).await.expect("a picture");
        assert_eq!(p.cancels_on_drop.load(Ordering::SeqCst), 0, "a request that ended is not canceled");
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].media_type, "image/png");
        assert_eq!(out.images[0].data, png());
        let requests = server.received_requests().await.unwrap();
        let submission = &requests[0];
        assert_eq!(serde_json::from_slice::<JsonValue>(&submission.body).unwrap(), json!({"prompt": "a lighthouse", "aspect_ratio": "16:9", "resolution": "2k"}));
        let key = submission.headers.get("idempotency-key").expect("an idempotency key").to_str().unwrap();
        assert!(uuid::Uuid::parse_str(key).is_ok(), "{key}");
        let reads = requests.iter().filter(|r| r.url.path() == "/requests/r-1/status").count();
        assert_eq!(reads, 3);
        for request in &requests {
            let keyed = request.headers.get("authorization").is_some();
            assert_eq!(keyed, request.url.path() != "/out/r-1.png", "{}: the key goes to the API alone", request.url.path());
        }
    }

    /// A status that ends the request without a picture is said as itself,
    /// and nothing is canceled.
    #[tokio::test]
    async fn a_failed_a_refused_and_a_canceled_request_are_each_said_as_they_are() {
        let cases = [
            (json!({"status": "failed", "request_id": "r-1", "error": "Generation failed"}), "Higgsfield's request r-1 failed: Generation failed; a failed request is not charged"),
            (json!({"status": "nsfw", "request_id": "r-1"}), "Higgsfield's moderation refused the prompt or the picture of request r-1; a refused request is not charged"),
            (json!({"status": "canceled", "request_id": "r-1"}), "Higgsfield's request r-1 was canceled; nothing is charged"),
            (json!({"status": "completed", "request_id": "r-1", "images": []}), "Higgsfield's request r-1 completed and named no picture"),
        ];
        for (answer, words) in cases {
            let server = MockServer::start().await;
            mount_once(&server, "POST", "/z-image/turbo", queued(&server, "r-1")).await;
            Mock::given(method("GET")).and(path("/requests/r-1/status")).respond_with(ResponseTemplate::new(200).set_body_json(answer)).mount(&server).await;
            Mock::given(method("POST")).and(path("/requests/r-1/cancel")).respond_with(ResponseTemplate::new(202)).expect(0).mount(&server).await;
            let p = provider(&server, "z-image/turbo", QUICK);
            let err = p.generate(&req("a lighthouse", None, None)).await.unwrap_err();
            assert_eq!(err.to_string(), format!("image API returned no image: {words}"));
            assert_eq!(p.cancels_on_drop.load(Ordering::SeqCst), 0, "a request that ended is not canceled");
        }
    }

    /// A refusal of the submission is the API's words with what to do; a
    /// 4xx is not sent again, a 5xx once, with the same idempotency key.
    #[tokio::test]
    async fn a_refused_submission_is_said_with_its_way_and_a_5xx_is_sent_once_more_with_the_same_key() {
        let cases = [
            (401, json!({"detail": "Invalid credentials"}), "Invalid credentials. The key was refused: /image-provider key <key id>:<key secret> stores another (the API console at open.higgsfield.ai makes keys)."),
            (403, json!({"detail": "Insufficient balance"}), "Insufficient balance. Higgsfield refused the account: the API's balance at open.higgsfield.ai may be empty, or the key disabled."),
            (404, json!({"detail": "Not found"}), "Not found. The model is not open to this account: /image-provider model <id> picks another."),
            (422, json!({"detail": [{"loc": ["body", "resolution"], "msg": "Input should be '1k' or '2k'"}]}), "resolution: Input should be '1k' or '2k'. The request was refused as it was sent."),
        ];
        for (code, answer, words) in cases {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/z-image/turbo"))
                .respond_with(ResponseTemplate::new(code).set_body_json(answer).insert_header("x-correlation-id", "c-9"))
                .expect(1)
                .mount(&server)
                .await;
            let err = provider(&server, "z-image/turbo", QUICK).generate(&req("a lighthouse", None, None)).await.unwrap_err();
            match err {
                ImageGenError::Api { status, body } => {
                    assert_eq!(status, code);
                    assert_eq!(body, format!("{words} (Higgsfield's correlation id c-9)"));
                    assert!(!body.contains("key-secret-1"));
                }
                other => panic!("{code}: {other:?}"),
            }
        }
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/z-image/turbo"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "30").set_body_json(json!({"detail": {"code": "account_queue_full", "message": "Too many queued generations for this account"}})))
            .mount(&server)
            .await;
        let err = provider(&server, "z-image/turbo", QUICK).generate(&req("a lighthouse", None, None)).await.unwrap_err();
        assert_eq!(err.to_string(), "image API error 429: Too many queued generations for this account. The account's queue is full: try again in 30 s.");
        // A 5xx: once more with the same key, and the call goes on.
        let server = MockServer::start().await;
        mount_once(&server, "POST", "/z-image/turbo", ResponseTemplate::new(503).set_body_json(json!({"detail": "admission_control_unavailable"}))).await;
        mount_once(&server, "POST", "/z-image/turbo", queued(&server, "r-2")).await;
        Mock::given(method("GET"))
            .and(path("/requests/r-2/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "completed", "images": [{"url": format!("{}/out/2.png", server.uri())}]})))
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/out/2.png")).respond_with(ResponseTemplate::new(200).set_body_bytes(png())).mount(&server).await;
        provider(&server, "z-image/turbo", QUICK).generate(&req("a lighthouse", None, None)).await.expect("a picture");
        let keys: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/z-image/turbo")
            .map(|r| r.headers.get("idempotency-key").unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], keys[1], "the same submission");
        // Two 5xx: the call ends with the second one's words.
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/z-image/turbo")).respond_with(ResponseTemplate::new(500)).expect(2).mount(&server).await;
        let err = provider(&server, "z-image/turbo", QUICK).generate(&req("a lighthouse", None, None)).await.unwrap_err();
        assert_eq!(err.to_string(), "image API error 500: HTTP 500. Higgsfield's side failed: try again later.");
    }

    /// A status read that fails, a 5xx and a 429 are read again; a 4xx
    /// ends the call with the API's words.
    #[tokio::test]
    async fn a_failing_status_read_is_read_again_and_a_refused_one_ends_the_call() {
        let server = MockServer::start().await;
        mount_once(&server, "POST", "/z-image/turbo", queued(&server, "r-1")).await;
        mount_once(&server, "GET", "/requests/r-1/status", ResponseTemplate::new(502)).await;
        mount_once(&server, "GET", "/requests/r-1/status", ResponseTemplate::new(429).insert_header("retry-after", "0")).await;
        mount_once(&server, "GET", "/requests/r-1/status", ResponseTemplate::new(200).set_body_string("not json")).await;
        Mock::given(method("GET"))
            .and(path("/requests/r-1/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "completed", "images": [{"url": format!("{}/out/1.png", server.uri())}]})))
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/out/1.png")).respond_with(ResponseTemplate::new(200).set_body_bytes(png())).mount(&server).await;
        provider(&server, "z-image/turbo", QUICK).generate(&req("a lighthouse", None, None)).await.expect("a picture");
        let server = MockServer::start().await;
        mount_once(&server, "POST", "/z-image/turbo", queued(&server, "r-1")).await;
        Mock::given(method("GET"))
            .and(path("/requests/r-1/status"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({"detail": "Request not found"})))
            .mount(&server)
            .await;
        let err = provider(&server, "z-image/turbo", QUICK).generate(&req("a lighthouse", None, None)).await.unwrap_err();
        assert!(matches!(err, ImageGenError::Api { status: 404, ref body } if body.starts_with("Request not found.")), "{err:?}");
    }

    /// A status or cancel URL of another origin is not followed: the key
    /// stays with the API's origin, and the request's own URL is read.
    #[tokio::test]
    async fn a_url_of_another_origin_is_not_followed() {
        let server = MockServer::start().await;
        let elsewhere = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/z-image/turbo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "queued", "request_id": "r-1",
                "status_url": format!("{}/requests/r-1/status", elsewhere.uri()),
                "cancel_url": format!("{}/requests/r-1/cancel", elsewhere.uri()),
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/requests/r-1/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "completed", "images": [{"url": format!("{}/out/1.png", server.uri())}]})))
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/out/1.png")).respond_with(ResponseTemplate::new(200).set_body_bytes(png())).mount(&server).await;
        provider(&server, "z-image/turbo", QUICK).generate(&req("a lighthouse", None, None)).await.expect("a picture");
        assert!(elsewhere.received_requests().await.unwrap().is_empty(), "nothing went to the other origin");
        // A relative URL is taken against the base.
        let p = provider(&server, "z-image/turbo", QUICK);
        assert_eq!(p.own_url(Some("/requests/r-7/status"), "r-7", "status").as_str(), format!("{}/requests/r-7/status", server.uri()));
        assert_eq!(p.own_url(None, "a b/c", "cancel").as_str(), format!("{}/requests/a%20b%2Fc/cancel", server.uri()));
    }

    /// At the deadline the request is asked to cancel, once, and the error
    /// says which it came to.
    #[tokio::test]
    async fn at_the_deadline_the_request_is_canceled_once_and_the_error_says_what_it_came_to() {
        let cases = [
            (ResponseTemplate::new(202), "it was queued and is canceled: nothing is charged"),
            (ResponseTemplate::new(400).set_body_json(json!({"detail": "already started"})), "it was queued and had started, so it can no longer be canceled: it is charged if it completes, and the Higgsfield console at open.higgsfield.ai keeps its picture for seven days"),
            (ResponseTemplate::new(500), "it was queued, and the cancel was answered HTTP 500: it may still complete, and is charged if it does"),
        ];
        for (answer, detail) in cases {
            let server = MockServer::start().await;
            mount_once(&server, "POST", "/xai/grok-imagine-image-2.0", queued(&server, "r-1")).await;
            Mock::given(method("GET")).and(path("/requests/r-1/status")).respond_with(status("queued")).expect(0).mount(&server).await;
            Mock::given(method("POST"))
                .and(path("/requests/r-1/cancel"))
                .and(header("authorization", "Key key-id-1:key-secret-1"))
                .respond_with(answer)
                .expect(1)
                .mount(&server)
                .await;
            let p = provider(&server, "xai/grok-imagine-image-2.0", AT_ONCE);
            let err = p.generate(&req("a lighthouse", None, None)).await.unwrap_err();
            assert_eq!(p.cancels_on_drop.load(Ordering::SeqCst), 0, "the deadline's own cancel is the one");
            match err {
                ImageGenError::StillRunning { request_id, waited_secs, detail: said } => {
                    assert_eq!((request_id.as_str(), waited_secs), ("r-1", 0));
                    assert_eq!(said, detail);
                }
                other => panic!("{other:?}"),
            }
        }
    }

    /// A call dropped while it waits (the operator's stop) asks for the
    /// cancel, once, and tells the operator what it came to: canceled, or
    /// too late, the request having started.
    #[tokio::test]
    async fn a_call_dropped_while_it_waits_asks_for_the_cancel_once_and_says_what_it_came_to() {
        let cases = [
            (ResponseTemplate::new(202), "INFO", "The stopped image call's request r-1 is canceled at Higgsfield: nothing is charged."),
            (
                ResponseTemplate::new(400).set_body_json(json!({"detail": "already started"})),
                "NOTICE",
                "The stopped image call's request r-1 had started at Higgsfield and can no longer be canceled: it is charged if it completes, and the Higgsfield console at open.higgsfield.ai keeps its picture for seven days.",
            ),
        ];
        for (answer, priority, words) in cases {
            let server = MockServer::start().await;
            let (asked, mut status_asked) = tokio::sync::mpsc::unbounded_channel();
            let (told, mut notices) = tokio::sync::mpsc::unbounded_channel();
            mount_once(&server, "POST", "/xai/grok-imagine-image-2.0", queued(&server, "r-1")).await;
            Mock::given(method("GET"))
                .and(path("/requests/r-1/status"))
                .respond_with(Told { tell: asked, answer: status("queued").set_delay(Duration::from_secs(60)) })
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/requests/r-1/cancel"))
                .and(header("authorization", "Key key-id-1:key-secret-1"))
                .respond_with(answer)
                .expect(1)
                .mount(&server)
                .await;
            let on_stop: StopNotice = Arc::new(move |notice| {
                let _ = told.send(notice);
            });
            let p = Arc::new(
                HiggsfieldImageProvider::new(KEY, "xai/grok-imagine-image-2.0", &server.uri(), true, QUICK, on_stop).expect("a provider"),
            );
            let call = tokio::spawn({
                let p = p.clone();
                async move { p.generate(&req("a lighthouse", None, None)).await }
            });
            status_asked.recv().await.expect("the status is read");
            call.abort();
            assert!(call.await.unwrap_err().is_cancelled());
            // The backstop ends the test, not the behaviour: the cancel comes at once.
            let notice = tokio::time::timeout(Duration::from_secs(60), notices.recv()).await.expect("the operator is told").unwrap();
            assert_eq!((notice.priority_label(), notice.message.as_str()), (priority, words));
            assert!(notice.act.is_none(), "a notice, and no turn of the intelligence's own");
            assert_eq!(p.cancels_on_drop.load(Ordering::SeqCst), 1);
            assert!(notices.try_recv().is_err(), "one cancel, one notice");
        }
    }

    /// What a cancel came to, in the deadline's words and in the notice
    /// after a stop, each of the four word for word. A request that is
    /// canceled is information; one that may be charged is a notice.
    #[test]
    fn what_a_cancel_came_to_is_said_at_the_deadline_and_after_a_stop() {
        let cases = [
            (
                Canceled::Yes,
                "it was in_progress and is canceled: nothing is charged",
                "INFO",
                "The stopped image call's request r-7 is canceled at Higgsfield: nothing is charged.",
            ),
            (
                Canceled::Started,
                "it was in_progress and had started, so it can no longer be canceled: it is charged if it completes, and the Higgsfield console at open.higgsfield.ai keeps its picture for seven days",
                "NOTICE",
                "The stopped image call's request r-7 had started at Higgsfield and can no longer be canceled: it is charged if it completes, and the Higgsfield console at open.higgsfield.ai keeps its picture for seven days.",
            ),
            (
                Canceled::Answered(503),
                "it was in_progress, and the cancel was answered HTTP 503: it may still complete, and is charged if it does",
                "NOTICE",
                "The cancel of the stopped image call's request r-7 was answered HTTP 503 by Higgsfield: the request may still complete, and is charged if it does.",
            ),
            (
                Canceled::Unreached("connection refused".into()),
                "it was in_progress, and the cancel did not reach Higgsfield (connection refused): it may still complete, and is charged if it does",
                "NOTICE",
                "The cancel of the stopped image call's request r-7 did not reach Higgsfield (connection refused): the request may still complete, and is charged if it does.",
            ),
        ];
        for (outcome, deadline, priority, stop) in cases {
            assert_eq!(outcome.at_the_deadline("in_progress"), deadline);
            let notice = outcome.after_a_stop("r-7");
            assert_eq!((notice.priority_label(), notice.message.as_str()), (priority, stop));
            assert!(notice.act.is_none());
        }
    }

    /// A picture that cannot be fetched is paid for: the error names the
    /// request and the address. A network failure is fetched once more.
    #[tokio::test]
    async fn a_picture_that_cannot_be_fetched_is_named_with_its_request_and_address() {
        // An HTTP refusal: no second fetch.
        let server = MockServer::start().await;
        mount_once(&server, "POST", "/z-image/turbo", queued(&server, "r-1")).await;
        let picture = format!("{}/out/1.png", server.uri());
        Mock::given(method("GET"))
            .and(path("/requests/r-1/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "completed", "images": [{"url": picture}]})))
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/out/1.png")).respond_with(ResponseTemplate::new(503)).expect(1).mount(&server).await;
        let err = provider(&server, "z-image/turbo", QUICK).generate(&req("a lighthouse", None, None)).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("image API returned no image: the picture of Higgsfield's request r-1 could not be fetched from {picture}: HTTP 503. It is paid for: the address works for seven days, and the Higgsfield console at open.higgsfield.ai keeps it")
        );
        // An address that is not the stand-in's and not https: refused before a connection.
        let server = MockServer::start().await;
        mount_once(&server, "POST", "/z-image/turbo", queued(&server, "r-1")).await;
        Mock::given(method("GET"))
            .and(path("/requests/r-1/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "completed", "images": [{"url": "http://127.0.0.1:9/p.png"}]})))
            .mount(&server)
            .await;
        let err = provider(&server, "z-image/turbo", QUICK).generate(&req("a lighthouse", None, None)).await.unwrap_err();
        assert!(err.to_string().ends_with("from http://127.0.0.1:9/p.png: http://127.0.0.1:9/p.png is not https. It is paid for: the address works for seven days, and the Higgsfield console at open.higgsfield.ai keeps it"), "{err}");
        // A picture over the media store's ceiling is fetched whole (the
        // tool fits it); one over what a generated picture is taken at is not.
        use crate::media::{MEDIA_GENERATED_RAW_MAX, MEDIA_UPLOAD_MAX};
        for (weight, fetched) in [(MEDIA_UPLOAD_MAX + 1, true), (MEDIA_GENERATED_RAW_MAX + 1, false)] {
            let server = MockServer::start().await;
            mount_once(&server, "POST", "/z-image/turbo", queued(&server, "r-1")).await;
            Mock::given(method("GET"))
                .and(path("/requests/r-1/status"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "completed", "images": [{"url": format!("{}/out/big.png", server.uri())}]})))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/out/big.png"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; weight]))
                .mount(&server)
                .await;
            let answer = provider(&server, "z-image/turbo", QUICK).generate(&req("a lighthouse", None, None)).await;
            if fetched {
                assert_eq!(answer.unwrap().images[0].data.len(), weight);
            } else {
                let err = answer.unwrap_err();
                assert!(err.to_string().contains(&format!("the answer is {weight} bytes, more than the {MEDIA_GENERATED_RAW_MAX} taken")), "{err}");
            }
        }
    }

    /// A reference goes to Higgsfield's storage first, by the slot's own
    /// headers and without the key, and the request names it by its public
    /// URL; a slot whose address the client may not reach submits nothing.
    #[tokio::test]
    async fn references_go_to_higgsfields_storage_first_and_without_the_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/files/generate-upload-url"))
            .and(header("authorization", "Key key-id-1:key-secret-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "public_url": format!("{}/in/1.png", server.uri()),
                "upload_url": format!("{}/put/1", server.uri()),
                "content_type": "image/png",
                "upload_headers": {"Content-Type": "image/png", "x-amz-tagging": "retention=temporary"},
            })))
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/put/1"))
            .and(header("content-type", "image/png"))
            .and(header("x-amz-tagging", "retention=temporary"))
            .respond_with(ResponseTemplate::new(200))
            .expect(2)
            .mount(&server)
            .await;
        mount_once(&server, "POST", "/xai/grok-imagine-image-2.0", queued(&server, "r-1")).await;
        Mock::given(method("GET"))
            .and(path("/requests/r-1/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "completed", "images": [{"url": format!("{}/out/1.png", server.uri())}]})))
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/out/1.png")).respond_with(ResponseTemplate::new(200).set_body_bytes(png())).mount(&server).await;
        let mut request = req("the same lighthouse, at night", None, None);
        request.reference_images = vec![("image/png".into(), png()), ("image/png".into(), png())];
        provider(&server, "xai/grok-imagine-image-2.0", QUICK).generate(&request).await.expect("a picture");
        let requests = server.received_requests().await.unwrap();
        let slots: Vec<_> = requests.iter().filter(|r| r.url.path() == "/files/generate-upload-url").collect();
        for slot in &slots {
            assert_eq!(serde_json::from_slice::<JsonValue>(&slot.body).unwrap(), json!({"content_type": "image/png"}));
        }
        for put in requests.iter().filter(|r| r.method.as_str() == "PUT") {
            assert!(put.headers.get("authorization").is_none(), "the key went to the storage");
            assert_eq!(put.body, png());
        }
        let submission = requests.iter().find(|r| r.url.path() == "/xai/grok-imagine-image-2.0").expect("submitted");
        let body: JsonValue = serde_json::from_slice(&submission.body).unwrap();
        let public = format!("{}/in/1.png", server.uri());
        assert_eq!(body, json!({"prompt": "the same lighthouse, at night", "image_urls": [public, public]}));
        // A slot whose address may not be reached: nothing is submitted.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/files/generate-upload-url"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "public_url": "https://cdn.example/in/1.png",
                "upload_url": "http://127.0.0.1:9/put/1",
                "upload_headers": {"Content-Type": "image/png"},
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST")).and(path("/ideogram/v4.0")).respond_with(queued(&server, "r-2")).expect(0).mount(&server).await;
        let mut request = req("the same, in ink", None, None);
        request.reference_images = vec![("image/png".into(), png())];
        let err = provider(&server, "ideogram/v4.0", QUICK).generate(&request).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "image generation transport error: Higgsfield's upload slot is refused: http://127.0.0.1:9/put/1 is not https; nothing was submitted, nothing is charged"
        );
    }

    /// A slot's refusal is the API's, without a model's advice; a wait that
    /// ran out while the references were sent submits nothing. With no wait
    /// at all, the deadline has passed once the first reference is sent.
    #[tokio::test]
    async fn references_that_cannot_be_sent_in_time_or_at_all_submit_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/files/generate-upload-url"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({"detail": "Not Found"})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST")).and(path("/xai/grok-imagine-image-2.0")).respond_with(queued(&server, "r-1")).expect(0).mount(&server).await;
        let mut request = req("the same lighthouse, at night", None, None);
        request.reference_images = vec![("image/png".into(), png())];
        let err = provider(&server, "xai/grok-imagine-image-2.0", QUICK).generate(&request).await.unwrap_err();
        assert_eq!(err.to_string(), "image API error 404: Not Found.");
        drop(server);
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/files/generate-upload-url"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "public_url": format!("{}/in/1.png", server.uri()),
                "upload_url": format!("{}/put/1", server.uri()),
                "upload_headers": {"Content-Type": "image/png"},
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT")).and(path("/put/1")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
        Mock::given(method("POST")).and(path("/xai/grok-imagine-image-2.0")).respond_with(queued(&server, "r-1")).expect(0).mount(&server).await;
        request.reference_images = vec![("image/png".into(), png()), ("image/png".into(), png())];
        let err = provider(&server, "xai/grok-imagine-image-2.0", AT_ONCE).generate(&request).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "image API returned no image: the reference images took the whole wait; nothing was submitted, nothing is charged"
        );
    }

    /// A stand-in of its own, for what a mock server cannot do: it closes
    /// the first `drops` connections that ask for the picture without a
    /// word. Each request is read whole before it is answered.
    async fn dropping_stand_in(drops: usize) -> (String, Arc<Mutex<usize>>, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let pictures = Arc::new(Mutex::new(0));
        let serve = {
            let (base, pictures) = (base.clone(), pictures.clone());
            async move {
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    // The head, then as much body as it declares.
                    let mut got = Vec::new();
                    let mut buffer = [0u8; 4096];
                    let head_end = loop {
                        let n = socket.read(&mut buffer).await.unwrap();
                        if n == 0 {
                            break None;
                        }
                        got.extend_from_slice(&buffer[..n]);
                        if let Some(at) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                            break Some(at + 4);
                        }
                    };
                    let Some(head_end) = head_end else { continue };
                    let head = String::from_utf8_lossy(&got[..head_end]).to_ascii_lowercase();
                    let declared = head
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    while got.len() < head_end + declared {
                        let n = socket.read(&mut buffer).await.unwrap();
                        if n == 0 {
                            break;
                        }
                        got.extend_from_slice(&buffer[..n]);
                    }
                    let reply = |status: &str, body: &[u8], kind: &str| {
                        let mut out = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        out.extend_from_slice(body);
                        out
                    };
                    let answer = if head.starts_with("post /z-image/turbo ") {
                        reply("200 OK", br#"{"status":"queued","request_id":"r-1"}"#, "application/json")
                    } else if head.starts_with("get /requests/r-1/status ") {
                        let status = format!(r#"{{"status":"completed","images":[{{"url":"{base}/out/1.png"}}]}}"#);
                        reply("200 OK", status.as_bytes(), "application/json")
                    } else if head.starts_with("get /out/1.png ") {
                        let dropped = {
                            let mut seen = pictures.lock().unwrap();
                            *seen += 1;
                            *seen <= drops
                        };
                        if dropped {
                            drop(socket);
                            continue;
                        }
                        reply("200 OK", &png(), "image/png")
                    } else {
                        reply("404 Not Found", b"", "text/plain")
                    };
                    socket.write_all(&answer).await.unwrap();
                }
            }
        };
        (base, pictures, tokio::spawn(serve))
    }

    /// A fetch the network ends is made once more, and two such fetches end
    /// the call with the request and the address named: the picture is
    /// paid for.
    #[tokio::test]
    async fn a_fetch_the_network_ends_is_made_once_more_and_two_name_the_request_and_the_address() {
        let (base, pictures, server) = dropping_stand_in(1).await;
        let p = HiggsfieldImageProvider::new(KEY, "z-image/turbo", &base, true, QUICK, silent()).unwrap();
        let out = p.generate(&req("a lighthouse", None, None)).await.expect("the second fetch");
        assert_eq!(out.images[0].data, png());
        assert_eq!(*pictures.lock().unwrap(), 2);
        server.abort();
        let (base, pictures, server) = dropping_stand_in(2).await;
        let p = HiggsfieldImageProvider::new(KEY, "z-image/turbo", &base, true, QUICK, silent()).unwrap();
        let said = p.generate(&req("a lighthouse", None, None)).await.unwrap_err().to_string();
        assert_eq!(*pictures.lock().unwrap(), 2, "two fetches, no third");
        let picture = format!("{base}/out/1.png");
        assert!(said.starts_with(&format!("image API returned no image: the picture of Higgsfield's request r-1 could not be fetched from {picture}: ")), "{said}");
        assert!(said.ends_with("It is paid for: the address works for seven days, and the Higgsfield console at open.higgsfield.ai keeps it"), "{said}");
        server.abort();
    }

    #[test]
    fn a_model_that_is_not_higgsfields_and_a_key_no_header_carries_are_refused() {
        let err = HiggsfieldImageProvider::new(KEY, "gemini-nano-banana-2.1", API_BASE, false, Pacing::ADVISED, silent()).err().unwrap();
        assert_eq!(err.to_string(), "image provider not configured: gemini-nano-banana-2.1 is not one of Higgsfield's image models");
        let err = HiggsfieldImageProvider::new("id:sec\nret", "z-image/turbo", API_BASE, false, Pacing::ADVISED, silent()).err().unwrap();
        assert_eq!(err.to_string(), "image provider not configured: the Higgsfield key holds a character no header can carry");
        let p = HiggsfieldImageProvider::new(KEY, "z-image/turbo", API_BASE, false, Pacing::ADVISED, silent()).unwrap();
        assert_eq!(p.endpoint().as_str(), "https://api.higgsfield.ai/z-image/turbo");
        assert!(format!("{:?}", p.authorization).contains("Sensitive"), "the key is marked sensitive");
        let p = HiggsfieldImageProvider::new(KEY, "z-image/turbo", "http://10.0.2.2:28082/hf", true, Pacing::ADVISED, silent()).unwrap();
        assert_eq!(p.endpoint().as_str(), "http://10.0.2.2:28082/hf/z-image/turbo");
    }
}
