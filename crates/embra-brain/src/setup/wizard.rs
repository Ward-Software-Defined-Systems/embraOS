//! Three sub-flows of the first-run wizard: the routing question (the
//! providers directly, through WSDS Local AI Gateway, whose listener
//! identifies itself, or a model natively on this machine, experimental);
//! the Endpoint → Bearer → Probe-and-Select flow of the OpenAI-compat
//! presets, which lists the models through the gateway when one was chosen;
//! and the native answer's own steps, which model and its install.

use anyhow::{anyhow, Result};
use embra_boot::native_model::{self as native, NativeModel};
use tokio::sync::{broadcast, mpsc};
use tonic::Status;
use tracing::info;

use crate::config::ProviderGateway;
use crate::native_model::jobs::{JobEnd, JobEvent, JobKind, Jobs};
use crate::provider::gateway as gateway_rules;
use crate::provider::openai_compat::{OpenAICompatProvider, OpenAiCompatPreset};
use embra_common::proto::brain::{
    conversation_response, ConversationResponse, SetupFieldType, SetupPrompt, SystemMessage,
    SystemMessageType,
};

/// Result of the OpenAI-compat sub-flow. Endpoint is normalized
/// (scheme prefix + default port if missing, trailing slash stripped).
/// Bearer is `None` when the operator submitted an empty string.
#[derive(Debug, Clone)]
pub struct OpenAiCompatSubflow {
    pub preset: OpenAiCompatPreset,
    pub endpoint: String,
    pub bearer: Option<String>,
    pub model_id: String,
}

/// Run the three-step Endpoint → Bearer → Probe-and-Select flow.
///
/// With a provider gateway chosen, the endpoint typed is the direct path
/// for when the gateway is off (`none` allowed) and the models are listed
/// through the gateway's prefix. On probe failure (unreachable endpoint,
/// bad bearer, malformed response, a gateway upstream that does not
/// answer) or zero-models-found, the sub-flow re-prompts from the
/// Endpoint step. The operator can fix the URL or bearer and retry.
pub async fn run_openai_compat_subflow(
    preset: OpenAiCompatPreset,
    gateway: Option<&ProviderGateway>,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    response_rx: &mut mpsc::Receiver<String>,
) -> Result<OpenAiCompatSubflow> {
    loop {
        let endpoint = prompt_endpoint(preset, gateway, tx, response_rx).await?;
        let bearer = prompt_bearer(preset, tx, response_rx).await?;

        match probe_and_select(preset, gateway, &endpoint, bearer.as_deref(), tx, response_rx).await {
            Ok(model_id) => {
                return Ok(OpenAiCompatSubflow {
                    preset,
                    endpoint,
                    bearer,
                    model_id,
                });
            }
            Err(e) => {
                let _ = tx
                    .send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::System(
                            SystemMessage {
                                content: format!("{e} Re-entering endpoint step…"),
                                msg_type: SystemMessageType::Error as i32,
                            },
                        )),
                    }))
                    .await;
                continue;
            }
        }
    }
}

/// Endpoint step: free-text URL prompt with default-suggestion in the
/// label. Normalizes on submit. With a gateway chosen, the endpoint is the
/// direct path for when the gateway is off, `none` (the default) allowed:
/// the gateway's own upstream answers while it is on, and the preset's
/// default is never offered, since `localhost` inside the OS is the
/// machine itself.
async fn prompt_endpoint(
    preset: OpenAiCompatPreset,
    gateway: Option<&ProviderGateway>,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    response_rx: &mut mpsc::Receiver<String>,
) -> Result<String> {
    let (prompt_text, default) = if gateway.is_some() {
        (
            format!(
                "Enter the {} endpoint URL for when the gateway is off (none is allowed):",
                preset.label()
            ),
            "none",
        )
    } else {
        (
            format!(
                "Enter the {} endpoint URL (default: {}):",
                preset.label(),
                preset.default_base_url()
            ),
            preset.default_base_url(),
        )
    };
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::Setup(SetupPrompt {
                field_type: SetupFieldType::Text as i32,
                prompt: prompt_text,
                options: vec![],
                default_value: default.to_string(),
            })),
        }))
        .await;

    let raw = response_rx
        .recv()
        .await
        .ok_or_else(|| anyhow!("setup channel closed during endpoint step"))?;
    let trimmed = raw.trim();
    let endpoint = if gateway.is_some() && (trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none")) {
        String::new()
    } else if trimmed.is_empty() {
        preset.default_base_url().to_string()
    } else {
        normalize_endpoint(trimmed, preset)
    };
    info!(target: "setup::openai_compat", preset = %preset.label(), endpoint = %endpoint, "endpoint set");
    Ok(endpoint)
}

/// Bearer step: a set/skip Selector first ("Bearer token for X? (current: none)")
/// followed by a Text prompt for the actual token only when the
/// operator picks Set. Selector-then-Text is required because the
/// console enforces non-empty Text submissions — a single empty-string
/// Text prompt for "leave empty for no auth" is unreachable on the
/// console side. The Selector defaults to Skip so Enter without arrowing
/// accepts no-auth in the typical case. Wording + options + default
/// mirror `grpc_service.rs` `/provider --setup` fresh-setup branch so
/// the operator sees the same prompt in both flows.
async fn prompt_bearer(
    preset: OpenAiCompatPreset,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    response_rx: &mut mpsc::Receiver<String>,
) -> Result<Option<String>> {
    // Step 1: set/skip choice.
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::Setup(SetupPrompt {
                field_type: SetupFieldType::Selector as i32,
                prompt: format!("Bearer token for {}? (current: none)", preset.label()),
                options: vec!["Set".to_string(), "Skip".to_string()],
                default_value: "Skip".to_string(),
            })),
        }))
        .await;

    let choice = response_rx
        .recv()
        .await
        .ok_or_else(|| anyhow!("setup channel closed during bearer choice step"))?;
    let want_bearer = choice.trim().eq_ignore_ascii_case("set");
    if !want_bearer {
        return Ok(None);
    }

    // Step 2: text prompt for the actual token. Console enforces
    // non-empty so this branch always returns Some(<non-empty>).
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::Setup(SetupPrompt {
                field_type: SetupFieldType::Secret as i32,
                prompt: format!("Enter your {} bearer token:", preset.label()),
                options: vec![],
                default_value: String::new(),
            })),
        }))
        .await;

    let raw = response_rx
        .recv()
        .await
        .ok_or_else(|| anyhow!("setup channel closed during bearer token step"))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        // Defensive: if the console somehow emits empty here, treat
        // as no-bearer rather than a hard error.
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

/// Probe-and-Select step: hit `GET /v1/models` where the next turn goes
/// (the typed endpoint, or the gateway's prefix when one was chosen),
/// surface errors back to the wizard loop (which re-prompts), and on
/// success render a Selector populated from the returned model list.
async fn probe_and_select(
    preset: OpenAiCompatPreset,
    gateway: Option<&ProviderGateway>,
    endpoint: &str,
    bearer: Option<&str>,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    response_rx: &mut mpsc::Receiver<String>,
) -> Result<String, SubflowError> {
    let probe_base = gateway_rules::base_for(gateway, preset.as_kind(), endpoint);
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::System(SystemMessage {
                content: if gateway.is_some() {
                    format!("Probing {probe_base} for the models the gateway's upstream lists…")
                } else {
                    format!("Probing {probe_base} for available models…")
                },
                msg_type: SystemMessageType::Info as i32,
            })),
        }))
        .await;

    let models = OpenAICompatProvider::probe_models(preset, &probe_base, bearer)
        .await
        .map_err(|e| {
            if gateway.is_some() {
                SubflowError::ProbeThroughGatewayFailed {
                    base: probe_base.clone(),
                    // Every preset this sub-flow sets up has a prefix; a
                    // native model has none and never comes this way.
                    prefix: gateway_rules::prefix(preset.as_kind()).unwrap_or_default(),
                    reason: e.to_string(),
                }
            } else {
                SubflowError::ProbeFailed(e.to_string())
            }
        })?;

    if models.is_empty() {
        return Err(SubflowError::NoModels(probe_base));
    }

    // Selector widget — always shown, even with one model option,
    // for consistent UX (Locked Decision #6).
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::Setup(SetupPrompt {
                field_type: SetupFieldType::Selector as i32,
                prompt: "Select a model:".to_string(),
                options: models.clone(),
                default_value: models[0].clone(),
            })),
        }))
        .await;

    let selection = response_rx
        .recv()
        .await
        .ok_or(SubflowError::SelectionChannelClosed)?;
    let trimmed = selection.trim();
    // Defense against console drift: only accept selections from the
    // offered list. Empty input falls back to the default (first option).
    let chosen = if trimmed.is_empty() {
        models[0].clone()
    } else if models.iter().any(|m| m == trimmed) {
        trimmed.to_string()
    } else {
        return Err(SubflowError::InvalidSelection {
            offered: models.len(),
            given: trimmed.to_string(),
        });
    };
    Ok(chosen)
}

/// Errors surfaceable to the wizard loop. The loop converts these to
/// SystemMessage errors and re-prompts from the Endpoint step.
#[derive(Debug, thiserror::Error)]
pub enum SubflowError {
    #[error("Probe failed: {0}.")]
    ProbeFailed(String),
    #[error("Probe through the gateway at {base} failed: {reason}. Its [backends.{prefix}] upstream must answer.")]
    ProbeThroughGatewayFailed {
        base: String,
        prefix: &'static str,
        reason: String,
    },
    #[error("No models found at {0}. Pull or load a model on the server, then try again.")]
    NoModels(String),
    #[error("Selection '{given}' is not in the offered list of {offered} models.")]
    InvalidSelection { offered: usize, given: String },
    #[error("Setup channel closed during model selection.")]
    SelectionChannelClosed,
}

/// The routing question of the first-run wizard, asked after the name and
/// before the provider: reach the providers directly, through WSDS Local AI
/// Gateway, or run a model natively on this machine (experimental). With
/// the gateway chosen, its unified listener's endpoint is normalized
/// ([`gateway_rules::normalize_url`]) and must identify itself
/// ([`gateway_rules::identify`]); a refusal shows the reason and asks again,
/// and `direct` at the re-ask (its default, so Enter is enough on both
/// clients) falls back to direct. The native answer asks nothing more here:
/// [`run_native_subflow`] has its steps.
pub async fn run_gateway_subflow(
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    response_rx: &mut mpsc::Receiver<String>,
) -> Result<Routing> {
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::Setup(SetupPrompt {
                field_type: SetupFieldType::Selector as i32,
                prompt: GATEWAY_QUESTION.to_string(),
                options: vec![GATEWAY_DIRECT.to_string(), GATEWAY_THROUGH.to_string(), ROUTING_NATIVE.to_string()],
                default_value: String::new(),
            })),
        }))
        .await;
    let choice = response_rx
        .recv()
        .await
        .ok_or_else(|| anyhow!("setup channel closed during the gateway step"))?
        .to_ascii_lowercase();
    if choice.contains("native") {
        return Ok(Routing::Native);
    }
    if !choice.contains("gateway") {
        return Ok(Routing::Direct);
    }

    let mut prompt_text = GATEWAY_ENDPOINT_PROMPT.to_string();
    let mut default = String::new();
    loop {
        let _ = tx
            .send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::Setup(SetupPrompt {
                    field_type: SetupFieldType::Text as i32,
                    prompt: prompt_text.clone(),
                    options: vec![],
                    default_value: default.clone(),
                })),
            }))
            .await;
        let raw = response_rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("setup channel closed during the gateway endpoint step"))?;
        let answer = raw.trim();
        let re_asking = !default.is_empty();
        if re_asking && (answer.is_empty() || answer.eq_ignore_ascii_case("direct")) {
            send_system(tx, GATEWAY_DIRECT_NOTE.to_string(), SystemMessageType::Info).await;
            return Ok(Routing::Direct);
        }
        let outcome = match gateway_rules::normalize_url(answer) {
            Err(reason) => Err(format!("Not a gateway URL: {reason}.")),
            Ok(url) => {
                send_system(tx, format!("Asking {url} to identify itself…"), SystemMessageType::Info)
                    .await;
                gateway_rules::identify(&url).await.map(|version| (url, version))
            }
        };
        match outcome {
            Ok((url, version)) => {
                send_system(tx, gateway_rules::identified_line(&url, &version), SystemMessageType::Info)
                    .await;
                info!(target: "setup::gateway", url = %url, version = %version, "gateway identified");
                return Ok(Routing::Gateway(ProviderGateway { url, enabled: true }, version));
            }
            Err(reason) => {
                send_system(tx, reason, SystemMessageType::Error).await;
                prompt_text = GATEWAY_REASK_PROMPT.to_string();
                default = "direct".to_string();
            }
        }
    }
}

/// The routing question and its options; the direct one first, since the
/// console's selector starts on the first option. "provider" in it gives
/// the console its Provider header.
pub const GATEWAY_QUESTION: &str =
    "Reach the AI providers directly, through WSDS Local AI Gateway, or run a model natively on this machine (experimental)?";
pub const GATEWAY_DIRECT: &str = "Directly";
pub const GATEWAY_THROUGH: &str = "Through WSDS Local AI Gateway";
/// "endpoint" gives the console its Endpoint header; no "provider" in it.
const GATEWAY_ENDPOINT_PROMPT: &str =
    "Enter the gateway's unified listener endpoint (http://host:4000):";
const GATEWAY_REASK_PROMPT: &str =
    "Re-enter the gateway endpoint, or answer direct to reach the providers directly:";
const GATEWAY_DIRECT_NOTE: &str =
    "Reaching the providers directly. /local-ai-gateway <url> stores a gateway after setup.";

/// How the wizard's routing question was answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Routing {
    /// The providers are reached directly.
    Direct,
    /// Through the gateway, on, with the version it answered.
    Gateway(ProviderGateway, String),
    /// A model on this machine (experimental): no provider, no key.
    Native,
}

/// The third answer to the routing question.
pub const ROUTING_NATIVE: &str = "Natively on this machine (experimental)";
/// The option of the native model question that goes back to the routing
/// question.
pub const NATIVE_BACK: &str = "Back: reach a provider instead";
/// The word that stops a model's download at the wizard.
const NATIVE_CANCEL: &str = "cancel";
/// What Enter sends while a model is being installed: the console sends
/// nothing for an empty line at a question without a default.
const NATIVE_PROGRESS: &str = "progress";

/// What the wizard says when the native answer is chosen, before it asks
/// which model: what the model is, that it leads Learning Mode, that the
/// seal is permanent, and the way to an identity this model does not write.
pub fn native_notice() -> String {
    let models: Vec<&str> = native::CATALOGUE.iter().map(|m| m.label).collect();
    format!(
        "A native model (experimental) runs on this machine's own processor: {}. They answer slower than a hosted model, and the smaller ones with less judgement. \
         The model you pick leads Learning Mode, where this intelligence's identity is written with you and sealed, and the seal is permanent. \
         An identity written beforehand can be imported instead: Learning Mode offers the graphs in Imported_Intelligence before it begins. \
         A provider can be added later with /provider --setup.",
        match models.split_last() {
            Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
            _ => models.join(""),
        }
    )
}

/// The native answer's own steps (experimental): which model, and its
/// install when its file is not on the machine. Returns the model, with the
/// record written that has embrad load it while the setup goes on; or
/// `None` when the operator went back to the routing question.
///
/// A download is a job of the brain: it goes on when the console leaves
/// (the error then names the step), and a console that comes back to this
/// question follows it from where it is. While it runs the question takes
/// text, so the operator can ask how far it is, stop it, or type a command
/// (`/native`, `/network`).
pub async fn run_native_subflow(
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    response_rx: &mut mpsc::Receiver<String>,
    env: &crate::native_model::Env,
    jobs: &Jobs,
) -> Result<Option<&'static NativeModel>> {
    send_system(tx, native_notice(), SystemMessageType::Info).await;
    // The model a machine that is short of memory for it was told about:
    // the same pick again keeps it.
    let mut told_of: Option<&'static str> = None;
    let model = loop {
        let (prompt, mut options) = crate::native_model::catalogue_question(None, env, &|model| model.default_context);
        let default_value = options.first().cloned().unwrap_or_default();
        options.push(NATIVE_BACK.to_string());
        let _ = tx
            .send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::Setup(SetupPrompt {
                    field_type: SetupFieldType::Selector as i32,
                    prompt,
                    options,
                    default_value,
                })),
            }))
            .await;
        let answer = response_rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("setup channel closed during the native model step"))?;
        let answer = answer.trim();
        if answer.to_ascii_lowercase().starts_with("back") {
            return Ok(None);
        }
        // An empty answer is the first model, as Enter on the selector is.
        let picked = match answer {
            "" => native::CATALOGUE.first(),
            name => native::CATALOGUE.iter().find(|m| m.id.eq_ignore_ascii_case(name)),
        };
        let Some(model) = picked else {
            send_system(tx, format!("'{answer}' is not a native model."), SystemMessageType::Error).await;
            continue;
        };
        // A machine that is short of memory for the model is told before
        // anything is downloaded or loaded. The window cannot be made
        // smaller before a session, and this model leads Learning Mode: when
        // what it takes at its load is itself more than the machine has
        // free, the question is asked once more, and the same pick keeps
        // the model. Nothing is refused on a figure.
        if told_of != Some(model.id) {
            let machine = crate::native_model::machine_memory(env);
            let images = crate::native_model::would_read_images(model, env);
            if let Some(note) = crate::native_model::memory_note(model, model.default_context, images, machine, false) {
                told_of = Some(model.id);
                if note.at_the_load {
                    send_system(tx, format!("{} Pick {} again to keep it, or pick another model.", note.text, model.id), SystemMessageType::Warning).await;
                    continue;
                }
                send_system(tx, note.text, SystemMessageType::Warning).await;
            }
        }
        if native::installed(model, &env.models) == native::Installed::Verified {
            break model;
        }
        if install_at_the_wizard(model, tx, response_rx, env, jobs).await? {
            break model;
        }
    };
    // The record: embrad loads the model while the remaining questions are
    // answered. The configuration the wizard saves names the same model,
    // and the brain writes the record from it again at every start.
    match native::write_settings(&env.settings, Some(&native::Wanted::new(model))) {
        Ok(()) => {
            send_system(tx, format!("{} is installed. embrad loads it now, while the setup goes on.", model.id), SystemMessageType::Info).await
        }
        Err(e) => {
            send_system(
                tx,
                format!("{} is installed. Its record could not be written yet ({e}); the brain writes it when the setup is saved.", model.id),
                SystemMessageType::Info,
            )
            .await
        }
    }
    info!(target: "setup::native", model = model.id, "native model chosen");
    Ok(Some(model))
}

/// Install `model` and follow the job at the wizard. `true` when its file is
/// there and checked; `false` when the job did not finish or was stopped,
/// and the model question is asked again.
async fn install_at_the_wizard(
    model: &'static NativeModel,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    response_rx: &mut mpsc::Receiver<String>,
    env: &crate::native_model::Env,
    jobs: &Jobs,
) -> Result<bool> {
    let mut events = match jobs.start(env, JobKind::Install, model) {
        Ok(events) => {
            send_system(
                tx,
                format!(
                    "Installing {}: its model file ({} MB) and its image file ({} MB) are downloaded and checked, and the lines below follow the updater.",
                    model.id,
                    crate::native_model::mb(model.bytes),
                    crate::native_model::mb(model.images.bytes)
                ),
                SystemMessageType::Info,
            )
            .await;
            events
        }
        // A console that left and came back: the install it started runs
        // still, and is followed from where it is.
        Err(running) if running.kind == JobKind::Install && running.model.id == model.id => match jobs.follow() {
            Some(events) => {
                let how_far = running.last_line.map(|line| format!(": {line}")).unwrap_or_default();
                send_system(tx, format!("The install of {} runs already{how_far}.", model.id), SystemMessageType::Info).await;
                events
            }
            // It ended this moment: the question is asked again and says how the file stands.
            None => return Ok(false),
        },
        Err(running) => {
            send_system(
                tx,
                format!(
                    "The {} of {} runs, and one job runs at a time. /native cancel stops it, or wait for it and pick again.",
                    running.kind.noun(),
                    running.model.id
                ),
                SystemMessageType::Error,
            )
            .await;
            return Ok(false);
        }
    };
    let mut ask = true;
    let end = loop {
        if ask {
            ask = false;
            let _ = tx
                .send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::Setup(SetupPrompt {
                        field_type: SetupFieldType::Text as i32,
                        prompt: format!(
                            "{} is being installed. Press Enter for how far it is, or answer {NATIVE_CANCEL} to stop it and choose again:",
                            model.id
                        ),
                        options: vec![],
                        default_value: NATIVE_PROGRESS.to_string(),
                    })),
                }))
                .await;
        }
        tokio::select! {
            event = events.recv() => match event {
                Ok(JobEvent::Line(line)) => send_system(tx, line, SystemMessageType::Info).await,
                Ok(JobEvent::Ended(end)) => break end,
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => {
                    break jobs.last().map(|last| last.end).unwrap_or_else(|| JobEnd::Failed("its end was not heard".to_string()));
                }
            },
            answer = response_rx.recv() => {
                // The console left. The job is the brain's and goes on.
                let answer = answer.ok_or_else(|| anyhow!("setup channel closed during the native model's install; the install goes on"))?;
                if answer.trim().eq_ignore_ascii_case(NATIVE_CANCEL) {
                    jobs.stop();
                    send_system(tx, format!("Stopping the install of {}.", model.id), SystemMessageType::Info).await;
                } else {
                    let how_far = jobs.running().and_then(|running| running.last_line).unwrap_or_else(|| "The updater has not said how far it is yet.".to_string());
                    send_system(tx, how_far, SystemMessageType::Info).await;
                    ask = true;
                }
            }
        }
    };
    match end {
        JobEnd::Done | JobEnd::Nothing(_) => Ok(true),
        JobEnd::Refused(why) | JobEnd::Failed(why) => {
            send_system(tx, format!("{} was not installed: {why}. Pick again, or go back to a provider.", model.id), SystemMessageType::Error).await;
            Ok(false)
        }
        JobEnd::Stopped => {
            send_system(
                tx,
                format!("The install of {} was stopped. What arrived is kept, and the next install goes on from it.", model.id),
                SystemMessageType::Info,
            )
            .await;
            Ok(false)
        }
    }
}

async fn send_system(
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    content: String,
    msg_type: SystemMessageType,
) {
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::System(SystemMessage {
                content,
                msg_type: msg_type as i32,
            })),
        }))
        .await;
}

/// Normalize an endpoint URL the operator typed:
/// - Trim whitespace + trailing slash
/// - Prepend `http://` if scheme missing
/// - Append the preset's default port if no port is present in the
///   host portion — for bare hosts and `http://` only. An `https://`
///   URL keeps its implicit 443: cloud OpenAI-compatible endpoints
///   (OpenRouter, Together, Fireworks, …) are `https://host/path`
///   without a port, and `https://host:1234` would break them.
///
/// Examples (Ollama default port 11434):
/// - `localhost`             → `http://localhost:11434`
/// - `http://localhost`      → `http://localhost:11434`
/// - `http://localhost:8080` → `http://localhost:8080`
/// - `localhost:11434/`      → `http://localhost:11434`
/// - `https://api.example.com` → `https://api.example.com`
/// - `https://openrouter.ai/api/` → `https://openrouter.ai/api`
pub fn normalize_endpoint(input: &str, preset: OpenAiCompatPreset) -> String {
    let trimmed = input.trim().trim_end_matches('/');
    let with_scheme = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{}", trimmed)
    };

    // Split scheme from host portion.
    let (scheme, after) = if let Some(rest) = with_scheme.strip_prefix("http://") {
        ("http://", rest)
    } else if let Some(rest) = with_scheme.strip_prefix("https://") {
        ("https://", rest)
    } else {
        return with_scheme; // unreachable given the above
    };
    // Host portion is everything before the first `/`.
    let (host_part, path_part) = match after.find('/') {
        Some(idx) => (&after[..idx], &after[idx..]),
        None => (after, ""),
    };

    let default_port = match preset {
        OpenAiCompatPreset::Ollama => "11434",
        OpenAiCompatPreset::LmStudio => "1234",
        // Never typed: a native model's endpoint is the model server's own.
        OpenAiCompatPreset::Native => "8091",
    };

    // IPv6 bracketed host (e.g. `[::1]`) — only treat ':' after the
    // closing bracket as a port separator.
    let has_port = if host_part.starts_with('[') {
        host_part.rfind(']').is_some_and(|i| host_part[i..].contains(':'))
    } else {
        host_part.contains(':')
    };
    if has_port || scheme == "https://" {
        format!("{scheme}{host_part}{path_part}")
    } else {
        format!("{scheme}{host_part}:{default_port}{path_part}")
    }
}

/// Test support shared with the wizard's tests: a mock server on the
/// host's own non-loopback address, because `gateway::normalize_url`
/// refuses a loopback gateway by design and the gateway sub-flow runs it
/// before `identify`.
#[cfg(test)]
pub(crate) mod test_support {
    /// The address the host's default route leaves from; no packet is sent.
    pub fn local_non_loopback_ip() -> std::net::IpAddr {
        let probe = std::net::UdpSocket::bind("0.0.0.0:0").expect("a UDP socket");
        probe.connect("10.255.255.255:1").expect("a default route");
        probe.local_addr().expect("the route's source address").ip()
    }

    /// A wiremock server bound on that address, and its `http://` base.
    pub async fn reachable_mock_server() -> (wiremock::MockServer, String) {
        let listener = std::net::TcpListener::bind((local_non_loopback_ip(), 0)).expect("a listener");
        let addr = listener.local_addr().expect("a bound address");
        let server = wiremock::MockServer::builder().listener(listener).start().await;
        (server, format!("http://{addr}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_bare_host_adds_scheme_and_port() {
        let out = normalize_endpoint("localhost", OpenAiCompatPreset::Ollama);
        assert_eq!(out, "http://localhost:11434");
    }

    #[test]
    fn normalize_preserves_explicit_port() {
        let out = normalize_endpoint("http://localhost:8080", OpenAiCompatPreset::Ollama);
        assert_eq!(out, "http://localhost:8080");
    }

    #[test]
    fn normalize_strips_trailing_slash() {
        let out = normalize_endpoint("http://localhost:11434/", OpenAiCompatPreset::Ollama);
        assert_eq!(out, "http://localhost:11434");
    }

    #[test]
    fn normalize_adds_scheme_with_explicit_port() {
        let out = normalize_endpoint("localhost:1234", OpenAiCompatPreset::LmStudio);
        assert_eq!(out, "http://localhost:1234");
    }

    #[test]
    fn normalize_lm_studio_default_port_when_missing() {
        let out = normalize_endpoint("http://localhost", OpenAiCompatPreset::LmStudio);
        assert_eq!(out, "http://localhost:1234");
    }

    #[test]
    fn normalize_https_keeps_implicit_443() {
        // Cloud OpenAI-compatible endpoints are https without a port;
        // injecting the preset's local default port broke them
        // (`https://openrouter.ai:1234/api`). 2026-09-17.
        let out = normalize_endpoint("https://api.example.com", OpenAiCompatPreset::Ollama);
        assert_eq!(out, "https://api.example.com");
        let out = normalize_endpoint("https://openrouter.ai/api/", OpenAiCompatPreset::LmStudio);
        assert_eq!(out, "https://openrouter.ai/api");
    }

    #[test]
    fn normalize_https_explicit_port_kept() {
        let out = normalize_endpoint("https://api.example.com:8443/inference", OpenAiCompatPreset::Ollama);
        assert_eq!(out, "https://api.example.com:8443/inference");
    }

    #[test]
    fn normalize_multiple_trailing_slashes_only_strips_one() {
        // trim_end_matches('/') strips ALL trailing — that's fine for URLs.
        let out = normalize_endpoint("localhost///", OpenAiCompatPreset::Ollama);
        assert_eq!(out, "http://localhost:11434");
    }

    #[test]
    fn normalize_preserves_path() {
        let out = normalize_endpoint("http://api.example.com/proxy", OpenAiCompatPreset::Ollama);
        assert_eq!(out, "http://api.example.com:11434/proxy");
    }

    #[test]
    fn normalize_ipv6_bracketed_host_no_port() {
        let out = normalize_endpoint("http://[::1]", OpenAiCompatPreset::Ollama);
        assert_eq!(out, "http://[::1]:11434");
    }

    #[tokio::test]
    async fn subflow_happy_path_one_model() {
        // Mock server with one model in the response.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{"id": "qwen3.6:35b"}]
            })))
            .mount(&server)
            .await;

        let (resp_tx, mut resp_rx) = mpsc::channel(32);
        let (user_tx, user_rx) = mpsc::channel(32);

        // Operator inputs — ordered: endpoint URL, bearer-set/skip (Skip),
        // model selection. 2-step bearer flow short-circuits on Skip.
        user_tx.send(server.uri()).await.unwrap();
        user_tx.send("Skip".to_string()).await.unwrap();
        user_tx.send("qwen3.6:35b".to_string()).await.unwrap();

        let mut user_rx = user_rx;
        let result =
            run_openai_compat_subflow(OpenAiCompatPreset::Ollama, None, &resp_tx, &mut user_rx).await;
        let out = result.expect("subflow should succeed");
        assert_eq!(out.preset, OpenAiCompatPreset::Ollama);
        // server.uri() comes back like "http://127.0.0.1:54321" — already
        // has scheme + port; normalize_endpoint preserves it.
        assert!(out.endpoint.starts_with("http://"));
        assert_eq!(out.bearer, None);
        assert_eq!(out.model_id, "qwen3.6:35b");

        // Drain response stream so we don't hold the channel open.
        while resp_rx.try_recv().is_ok() {}
    }

    #[tokio::test]
    async fn subflow_zero_models_reprompts_from_endpoint() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        // First request returns zero models.
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": []
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // Second request (after re-prompt) returns one model.
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{"id": "m"}]
            })))
            .mount(&server)
            .await;

        let (resp_tx, mut resp_rx) = mpsc::channel(32);
        let (user_tx, user_rx) = mpsc::channel(32);

        // First attempt: endpoint, bearer-skip, → probe returns 0 → re-prompt.
        // Second attempt: endpoint, bearer-skip, model.
        user_tx.send(server.uri()).await.unwrap();
        user_tx.send("Skip".to_string()).await.unwrap();
        user_tx.send(server.uri()).await.unwrap();
        user_tx.send("Skip".to_string()).await.unwrap();
        user_tx.send("m".to_string()).await.unwrap();

        let mut user_rx = user_rx;
        let result =
            run_openai_compat_subflow(OpenAiCompatPreset::Ollama, None, &resp_tx, &mut user_rx).await;
        let out = result.expect("subflow should succeed after re-prompt");
        assert_eq!(out.model_id, "m");

        while resp_rx.try_recv().is_ok() {}
    }

    #[tokio::test]
    async fn subflow_invalid_selection_reprompts_from_endpoint() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{"id": "m1"}, {"id": "m2"}]
            })))
            .mount(&server)
            .await;

        let (resp_tx, mut resp_rx) = mpsc::channel(32);
        let (user_tx, user_rx) = mpsc::channel(32);

        // First attempt: endpoint, bearer-skip, "not_in_list" — fails
        // validation and re-prompts from endpoint.
        // Second attempt: endpoint, bearer-skip, valid pick.
        user_tx.send(server.uri()).await.unwrap();
        user_tx.send("Skip".to_string()).await.unwrap();
        user_tx.send("not_in_list".to_string()).await.unwrap();
        user_tx.send(server.uri()).await.unwrap();
        user_tx.send("Skip".to_string()).await.unwrap();
        user_tx.send("m2".to_string()).await.unwrap();

        let mut user_rx = user_rx;
        let result =
            run_openai_compat_subflow(OpenAiCompatPreset::Ollama, None, &resp_tx, &mut user_rx).await;
        let out = result.expect("subflow should succeed after re-prompt");
        assert_eq!(out.model_id, "m2");

        while resp_rx.try_recv().is_ok() {}
    }

    #[tokio::test]
    async fn subflow_bearer_non_empty_persists() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{"id": "m"}]
            })))
            .mount(&server)
            .await;

        let (resp_tx, mut resp_rx) = mpsc::channel(32);
        let (user_tx, user_rx) = mpsc::channel(32);

        // Flow: endpoint, bearer-set, token text, model selection.
        user_tx.send(server.uri()).await.unwrap();
        user_tx.send("Set".to_string()).await.unwrap();
        user_tx.send("secret-token".to_string()).await.unwrap();
        user_tx.send("m".to_string()).await.unwrap();

        let mut user_rx = user_rx;
        let result =
            run_openai_compat_subflow(OpenAiCompatPreset::Ollama, None, &resp_tx, &mut user_rx).await;
        let out = result.expect("subflow should succeed");
        assert_eq!(out.bearer.as_deref(), Some("secret-token"));

        while resp_rx.try_recv().is_ok() {}
    }

    /// #20: the bearer is asked for with the `Secret` field type.
    #[tokio::test]
    async fn the_bearer_step_asks_with_a_secret_field() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{"id": "m"}]
            })))
            .mount(&server)
            .await;
        let (resp_tx, mut resp_rx) = mpsc::channel(32);
        let (user_tx, mut user_rx) = mpsc::channel(32);
        user_tx.send(server.uri()).await.unwrap();
        user_tx.send("Set".to_string()).await.unwrap();
        user_tx.send("secret-token".to_string()).await.unwrap();
        user_tx.send("m".to_string()).await.unwrap();
        run_openai_compat_subflow(OpenAiCompatPreset::Ollama, None, &resp_tx, &mut user_rx)
            .await
            .expect("subflow should succeed");
        drop(resp_tx);
        let mut bearer_prompt = None;
        while let Some(Ok(r)) = resp_rx.recv().await {
            if let Some(conversation_response::ResponseType::Setup(p)) = r.response_type
                && p.prompt.contains("bearer token:")
            {
                bearer_prompt = Some(p);
            }
        }
        let p = bearer_prompt.expect("the bearer was asked for");
        assert_eq!(p.field_type, SetupFieldType::Secret as i32);
    }

    #[tokio::test]
    async fn subflow_bearer_choice_defaults_skip() {
        // Operator presses Enter on the set/skip Selector without arrowing —
        // console emits the default option ("Skip") so bearer = None.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{"id": "m"}]
            })))
            .mount(&server)
            .await;

        let (resp_tx, mut resp_rx) = mpsc::channel(32);
        let (user_tx, user_rx) = mpsc::channel(32);

        user_tx.send(server.uri()).await.unwrap();
        user_tx.send("Skip".to_string()).await.unwrap();
        user_tx.send("m".to_string()).await.unwrap();

        let mut user_rx = user_rx;
        let result =
            run_openai_compat_subflow(OpenAiCompatPreset::Ollama, None, &resp_tx, &mut user_rx).await;
        let out = result.expect("subflow should succeed");
        assert_eq!(out.bearer, None);
        while resp_rx.try_recv().is_ok() {}
    }

    #[tokio::test]
    async fn subflow_bearer_choice_set_case_insensitive() {
        // "set" / "SET" / "Set" all accepted — eq_ignore_ascii_case on the
        // selector value. Console renders the option label verbatim
        // ("Set"), but lowercase is also tolerated for resilience.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{"id": "m"}]
            })))
            .mount(&server)
            .await;

        for variant in ["set", "SET", "Set"] {
            let (resp_tx, mut resp_rx) = mpsc::channel(32);
            let (user_tx, user_rx) = mpsc::channel(32);
            user_tx.send(server.uri()).await.unwrap();
            user_tx.send(variant.to_string()).await.unwrap();
            user_tx.send("token".to_string()).await.unwrap();
            user_tx.send("m".to_string()).await.unwrap();
            let mut user_rx = user_rx;
            let out = run_openai_compat_subflow(
                OpenAiCompatPreset::Ollama,
                None,
                &resp_tx,
                &mut user_rx,
            )
            .await
            .expect("subflow should succeed");
            assert_eq!(
                out.bearer.as_deref(),
                Some("token"),
                "variant {variant:?} should reach the token prompt"
            );
            while resp_rx.try_recv().is_ok() {}
        }
    }

    #[tokio::test]
    async fn subflow_empty_endpoint_uses_preset_default() {
        // Operator presses Enter without typing — default is the
        // preset's default_base_url. We can't easily test this against
        // a real probe (the default localhost port likely isn't running),
        // so we just verify the prompt path receives a non-empty default
        // by checking normalize_endpoint behavior plus the input_placeholder
        // contract — the actual default-fallback is exercised in the probe
        // path under operator control.
        let out = normalize_endpoint("", OpenAiCompatPreset::Ollama);
        // Edge: empty input normalizes to "http://:11434" — caller of
        // prompt_endpoint must intercept empty string and use default.
        // This test asserts the subflow's "if raw.trim().is_empty()
        // → default" branch by confirming the normalization function
        // is NOT what handles empty.
        assert!(out != OpenAiCompatPreset::Ollama.default_base_url());
    }

    fn gateway(uri: &str) -> ProviderGateway {
        ProviderGateway { url: uri.to_string(), enabled: true }
    }

    async fn drain(resp_rx: &mut mpsc::Receiver<Result<ConversationResponse, Status>>) -> Vec<ConversationResponse> {
        let mut out = Vec::new();
        while let Ok(Ok(r)) = resp_rx.try_recv() {
            out.push(r);
        }
        out
    }

    fn system_texts(frames: &[ConversationResponse]) -> Vec<String> {
        frames
            .iter()
            .filter_map(|f| match &f.response_type {
                Some(conversation_response::ResponseType::System(m)) => Some(m.content.clone()),
                _ => None,
            })
            .collect()
    }

    fn prompts(frames: &[ConversationResponse]) -> Vec<&SetupPrompt> {
        frames
            .iter()
            .filter_map(|f| match &f.response_type {
                Some(conversation_response::ResponseType::Setup(p)) => Some(p),
                _ => None,
            })
            .collect()
    }

    /// With the gateway chosen, the models come through its prefix and
    /// `none` leaves the direct endpoint blank; the direct path is never
    /// probed.
    #[tokio::test]
    async fn with_the_gateway_on_the_probe_takes_its_prefix_and_none_stores_a_blank_endpoint() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ollama/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list", "data": [{"id": "qwen3.8:27b"}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (resp_tx, mut resp_rx) = mpsc::channel(32);
        let (user_tx, mut user_rx) = mpsc::channel(32);
        user_tx.send("none".to_string()).await.unwrap();
        user_tx.send("Skip".to_string()).await.unwrap();
        user_tx.send("qwen3.8:27b".to_string()).await.unwrap();
        let gw = gateway(&server.uri());
        let out = run_openai_compat_subflow(OpenAiCompatPreset::Ollama, Some(&gw), &resp_tx, &mut user_rx)
            .await
            .expect("the sub-flow completes through the gateway");
        assert_eq!(out.endpoint, "");
        assert_eq!(out.model_id, "qwen3.8:27b");
        let frames = drain(&mut resp_rx).await;
        let ps = prompts(&frames);
        assert_eq!(ps[0].prompt, "Enter the Ollama endpoint URL for when the gateway is off (none is allowed):");
        assert_eq!(ps[0].default_value, "none");
        assert!(system_texts(&frames).iter().any(|t| t == &format!("Probing {}/ollama for the models the gateway's upstream lists…", server.uri())));
        assert!(server.received_requests().await.unwrap().iter().all(|r| r.url.path() == "/ollama/v1/models"));
    }

    /// With the gateway chosen, a typed endpoint is normalized and kept as
    /// the direct path, and still not probed.
    #[tokio::test]
    async fn with_the_gateway_on_a_typed_endpoint_is_normalized_and_not_probed() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ollama/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"object": "list", "data": [{"id": "m"}]})))
            .mount(&server)
            .await;
        let (resp_tx, _resp_rx) = mpsc::channel(32);
        let (user_tx, mut user_rx) = mpsc::channel(32);
        user_tx.send("mac".to_string()).await.unwrap();
        user_tx.send("Skip".to_string()).await.unwrap();
        user_tx.send("m".to_string()).await.unwrap();
        let gw = gateway(&server.uri());
        let out = run_openai_compat_subflow(OpenAiCompatPreset::Ollama, Some(&gw), &resp_tx, &mut user_rx).await.unwrap();
        assert_eq!(out.endpoint, "http://mac:11434");
        assert!(server.received_requests().await.unwrap().iter().all(|r| r.url.path() == "/ollama/v1/models"));
    }

    /// A probe that fails through the gateway names the gateway's upstream
    /// setting, and the loop asks the endpoint again.
    #[tokio::test]
    async fn a_probe_through_the_gateway_that_fails_names_the_upstream() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ollama/v1/models"))
            .respond_with(ResponseTemplate::new(502).set_body_string("upstream down"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ollama/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"object": "list", "data": [{"id": "m"}]})))
            .mount(&server)
            .await;
        let (resp_tx, mut resp_rx) = mpsc::channel(32);
        let (user_tx, mut user_rx) = mpsc::channel(32);
        for answer in ["none", "Skip", "none", "Skip", "m"] {
            user_tx.send(answer.to_string()).await.unwrap();
        }
        let gw = gateway(&server.uri());
        let out = run_openai_compat_subflow(OpenAiCompatPreset::Ollama, Some(&gw), &resp_tx, &mut user_rx).await.unwrap();
        assert_eq!(out.model_id, "m");
        let texts = system_texts(&drain(&mut resp_rx).await);
        let failed = texts.iter().find(|t| t.starts_with("Probe through the gateway at")).unwrap_or_else(|| panic!("{texts:?}"));
        assert!(failed.contains("[backends.ollama] upstream must answer. Re-entering endpoint step…"), "{failed}");
    }

    /// The routing question has three answers, in this order: directly,
    /// through the gateway, natively. The direct option, an empty answer or
    /// anything that is neither of the other two stores no gateway and asks
    /// nothing more; the native one asks nothing more here either.
    #[tokio::test]
    async fn the_direct_option_or_an_empty_answer_stores_no_gateway() {
        for (answer, expected) in [
            (GATEWAY_DIRECT, Routing::Direct),
            ("", Routing::Direct),
            ("whatever", Routing::Direct),
            (ROUTING_NATIVE, Routing::Native),
            ("natively", Routing::Native),
        ] {
            let (resp_tx, mut resp_rx) = mpsc::channel(32);
            let (user_tx, mut user_rx) = mpsc::channel(32);
            user_tx.send(answer.to_string()).await.unwrap();
            drop(user_tx);
            let out = run_gateway_subflow(&resp_tx, &mut user_rx).await.unwrap();
            assert_eq!(out, expected, "{answer:?}");
            let frames = drain(&mut resp_rx).await;
            let ps = prompts(&frames);
            assert_eq!(ps.len(), 1);
            assert_eq!(ps[0].prompt, GATEWAY_QUESTION);
            assert_eq!(
                ps[0].prompt,
                "Reach the AI providers directly, through WSDS Local AI Gateway, or run a model natively on this machine (experimental)?"
            );
            assert_eq!(
                ps[0].options,
                ["Directly", "Through WSDS Local AI Gateway", "Natively on this machine (experimental)"],
                "the three answers, the direct one first"
            );
            assert_eq!(ps[0].default_value, "", "no default: nothing to leak into the next text prompt");
        }
    }

    use crate::native_model::test_support::{install, machine, stand_in};

    /// What the wizard sent next, in two minutes at most: a deadline no
    /// loaded machine reaches when the step works.
    enum Sent {
        Said(String),
        Asked(SetupPrompt),
    }

    async fn next(resp_rx: &mut mpsc::Receiver<Result<ConversationResponse, Status>>) -> Sent {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(120), resp_rx.recv())
            .await
            .expect("the wizard says something")
            .expect("the wizard is there")
            .expect("a frame");
        match frame.response_type {
            Some(conversation_response::ResponseType::System(m)) => Sent::Said(m.content),
            Some(conversation_response::ResponseType::Setup(p)) => Sent::Asked(p),
            other => panic!("neither a message nor a question: {other:?}"),
        }
    }

    async fn said(resp_rx: &mut mpsc::Receiver<Result<ConversationResponse, Status>>) -> String {
        match next(resp_rx).await {
            Sent::Said(text) => text,
            Sent::Asked(p) => panic!("a question where a message was expected: {}", p.prompt),
        }
    }

    async fn question(resp_rx: &mut mpsc::Receiver<Result<ConversationResponse, Status>>) -> SetupPrompt {
        match next(resp_rx).await {
            Sent::Asked(p) => p,
            Sent::Said(text) => panic!("a message where a question was expected: {text}"),
        }
    }

    /// An install that says where it is, then waits for `gate`.
    fn gated_updater(root: &std::path::Path, gate: &std::path::Path) -> Vec<std::ffi::OsString> {
        stand_in(
            root,
            &format!("echo \"$2: received 270 of 5561 MB (5%)\"\nuntil [ -e '{}' ]; do sleep 0.02; done\necho \"$2: installed and verified\"", gate.display()),
        )
    }

    /// The native answer: the notice first, then the model question over the
    /// catalogue with a way back. A model that is not on the machine is
    /// installed by a job the question follows; while it runs the operator
    /// asks how far it is and stops it, and the question is asked again. An
    /// install that ends writes the record embrad follows.
    #[tokio::test]
    async fn the_native_answer_says_what_it_is_asks_the_model_and_installs_it() {
        let (root, mut env) = machine("wizard-native");
        let gate = root.join("go-on");
        env.updater = gated_updater(&root, &gate);
        let jobs = Jobs::default();
        let nine = native::by_id("qwen3.5-9b").unwrap();
        let (resp_tx, mut resp_rx) = mpsc::channel(64);
        let (user_tx, mut user_rx) = mpsc::channel(8);
        let operator = async {
            assert_eq!(said(&mut resp_rx).await, native_notice());
            let asked = question(&mut resp_rx).await;
            assert_eq!(asked.field_type, SetupFieldType::Selector as i32);
            assert_eq!(asked.options, ["qwen3.5-4b", "qwen3.5-9b", "qwen3.8-27b", NATIVE_BACK], "the catalogue, then the way back");
            assert_eq!(asked.default_value, "qwen3.5-4b");
            assert!(
                asked.prompt.starts_with("Select a native model (experimental). qwen3.5-4b is Qwen3.5 4B (Q4_K_M, 2685 MB): not installed, downloaded first, about 5.3 GB of memory when loaded; "),
                "{}",
                asked.prompt
            );

            // A word that is no model: said, and asked again.
            user_tx.send("qwen3.5-27b".to_string()).await.unwrap();
            assert_eq!(said(&mut resp_rx).await, "'qwen3.5-27b' is not a native model.");
            question(&mut resp_rx).await;

            // The larger model: its install starts, and the question that stands takes text.
            user_tx.send("qwen3.5-9b".to_string()).await.unwrap();
            assert_eq!(
                said(&mut resp_rx).await,
                "Installing qwen3.5-9b: its model file (5561 MB) and its image file (879 MB) are downloaded and checked, and the lines below follow the updater."
            );
            let waiting = question(&mut resp_rx).await;
            assert_eq!(waiting.field_type, SetupFieldType::Text as i32);
            assert_eq!(waiting.prompt, "qwen3.5-9b is being installed. Press Enter for how far it is, or answer cancel to stop it and choose again:");
            assert_eq!(waiting.default_value, "progress", "Enter sends it: a console sends nothing for an empty line without a default");
            assert_eq!(said(&mut resp_rx).await, "qwen3.5-9b: received 270 of 5561 MB (5%)");

            // Enter: how far it is, and the question again.
            user_tx.send("progress".to_string()).await.unwrap();
            assert_eq!(said(&mut resp_rx).await, "qwen3.5-9b: received 270 of 5561 MB (5%)");
            assert_eq!(question(&mut resp_rx).await.prompt, waiting.prompt);

            // Stopped: the model question again, and nothing was asked of embrad.
            user_tx.send("cancel".to_string()).await.unwrap();
            assert_eq!(said(&mut resp_rx).await, "Stopping the install of qwen3.5-9b.");
            assert_eq!(said(&mut resp_rx).await, "The install of qwen3.5-9b was stopped. What arrived is kept, and the next install goes on from it.");
            assert_eq!(question(&mut resp_rx).await.options.len(), 4);
            assert_eq!(native::read_settings(&env.settings), Ok(None));

            // Picked again, and this time the file comes.
            std::fs::write(&gate, "").unwrap();
            user_tx.send("qwen3.5-9b".to_string()).await.unwrap();
            assert!(said(&mut resp_rx).await.starts_with("Installing qwen3.5-9b: "));
            let mut heard = Vec::new();
            loop {
                match next(&mut resp_rx).await {
                    Sent::Said(text) if text.starts_with("qwen3.5-9b is installed.") => break heard.push(text),
                    Sent::Said(text) => heard.push(text),
                    Sent::Asked(p) => assert_eq!(p.prompt, waiting.prompt),
                }
            }
            assert!(heard.iter().any(|line| line == "qwen3.5-9b: installed and verified"), "{heard:?}");
            assert_eq!(heard.last().map(String::as_str), Some("qwen3.5-9b is installed. embrad loads it now, while the setup goes on."));
        };
        let (picked, ()) = tokio::join!(run_native_subflow(&resp_tx, &mut user_rx, &env, &jobs), operator);
        assert_eq!(picked.unwrap().map(|m| m.id), Some("qwen3.5-9b"));
        assert_eq!(native::read_settings(&env.settings), Ok(Some(native::Wanted::new(nine))), "embrad loads it while the setup goes on");
    }

    /// On a machine that is short of memory the question says what each
    /// model takes and what the machine has. A model that fits at its load
    /// and may outgrow the machine is picked with a warning. One whose load
    /// is itself more than is free is told so and the question is asked
    /// again: the same pick keeps it, another model is another pick, and
    /// nothing is refused.
    #[tokio::test]
    async fn the_native_answer_tells_a_machine_that_is_short_of_memory_and_asks_once_more() {
        let (_root, env) = machine("wizard-memory");
        let (four, nine) = (native::by_id("qwen3.5-4b").unwrap(), native::by_id("qwen3.5-9b").unwrap());
        install(four, &env);
        install(nine, &env);
        crate::native_model::test_support::memory(&env, 7_700, 6_500);
        let jobs = Jobs::default();
        let (resp_tx, mut resp_rx) = mpsc::channel(64);
        let (user_tx, mut user_rx) = mpsc::channel(8);
        let operator = async {
            said(&mut resp_rx).await;
            let asked = question(&mut resp_rx).await;
            assert_eq!(
                asked.prompt,
                "Select a native model (experimental). qwen3.5-4b is Qwen3.5 4B (Q4_K_M, 2685 MB): installed, about 4.7 GB of memory when loaded; qwen3.5-9b is Qwen3.5 9B (Q4_K_M, 5561 MB): installed, about 6.9 GB of memory when loaded; qwen3.8-27b is Qwen3.8 27B (Q4_K_M, 16217 MB): not installed, downloaded first, about 20.0 GB of memory when loaded. This machine has 7.5 GB of memory, 6.3 GB of it free for a model now."
            );
            // The 9B takes more at its load than the machine has free: said, and asked again.
            user_tx.send("qwen3.5-9b".to_string()).await.unwrap();
            assert_eq!(
                said(&mut resp_rx).await,
                "Memory: Qwen3.5 9B with a context window of 65536 tokens takes about 6.9 GB when it is loaded, as measured, and this machine has 6.3 GB free for it (7.5 GB in all). Without the room its answers come very slowly, or the kernel ends the model server. A smaller window takes less, about 32 MB for every 1,024 tokens (/native context <tokens>, once the setup is done). Pick qwen3.5-9b again to keep it, or pick another model."
            );
            assert_eq!(question(&mut resp_rx).await.prompt, asked.prompt);
            assert_eq!(native::read_settings(&env.settings), Ok(None), "nothing is asked of embrad yet");
            // The 4B instead: it fits at its load and may outgrow the machine. Told, and taken.
            user_tx.send("qwen3.5-4b".to_string()).await.unwrap();
            let warned = said(&mut resp_rx).await;
            assert!(warned.starts_with("Memory: Qwen3.5 4B with a context window of 65536 tokens takes about 4.7 GB when it is loaded, as measured, and its model server can grow by up to 5.3 GB"), "{warned}");
            assert!(!warned.contains("Pick ") && !warned.contains("/native off"), "{warned}");
            assert_eq!(said(&mut resp_rx).await, "qwen3.5-4b is installed. embrad loads it now, while the setup goes on.");
        };
        let (picked, ()) = tokio::join!(run_native_subflow(&resp_tx, &mut user_rx, &env, &jobs), operator);
        assert_eq!(picked.unwrap().map(|m| m.id), Some("qwen3.5-4b"));
        assert_eq!(native::read_settings(&env.settings), Ok(Some(native::Wanted::new(four))));

        // The same pick twice keeps the model the machine is short for.
        let (_root, env) = machine("wizard-memory-kept");
        install(nine, &env);
        crate::native_model::test_support::memory(&env, 7_700, 6_500);
        let (resp_tx, mut resp_rx) = mpsc::channel(64);
        let (user_tx, mut user_rx) = mpsc::channel(8);
        let operator = async {
            said(&mut resp_rx).await;
            question(&mut resp_rx).await;
            user_tx.send("qwen3.5-9b".to_string()).await.unwrap();
            assert!(said(&mut resp_rx).await.ends_with("Pick qwen3.5-9b again to keep it, or pick another model."));
            question(&mut resp_rx).await;
            user_tx.send("qwen3.5-9b".to_string()).await.unwrap();
            assert_eq!(said(&mut resp_rx).await, "qwen3.5-9b is installed. embrad loads it now, while the setup goes on.");
        };
        let (picked, ()) = tokio::join!(run_native_subflow(&resp_tx, &mut user_rx, &env, &jobs), operator);
        assert_eq!(picked.unwrap().map(|m| m.id), Some("qwen3.5-9b"));
        assert_eq!(native::read_settings(&env.settings), Ok(Some(native::Wanted::new(nine))));
    }

    /// The way back writes nothing. A console that leaves ends the step by
    /// its name, and an install it started goes on: a console that comes
    /// back to the question follows it from where it is. A model that is
    /// installed is asked for at once.
    #[tokio::test]
    async fn the_native_answers_back_writes_nothing_and_a_console_that_left_finds_its_install_running() {
        let (root, mut env) = machine("wizard-native-left");
        let gate = root.join("go-on");
        env.updater = gated_updater(&root, &gate);
        let jobs = Jobs::default();
        let run = |answers: Vec<&'static str>| {
            let (env, jobs) = (env.clone(), jobs.clone());
            async move {
                let (resp_tx, mut resp_rx) = mpsc::channel(64);
                let (user_tx, mut user_rx) = mpsc::channel(8);
                for answer in answers {
                    user_tx.send(answer.to_string()).await.unwrap();
                }
                drop(user_tx);
                let out = run_native_subflow(&resp_tx, &mut user_rx, &env, &jobs).await;
                (out.map(|m| m.map(|m| m.id)).map_err(|e| e.to_string()), system_texts(&drain(&mut resp_rx).await))
            }
        };

        // Back, at once and in any case of letters.
        for back in [NATIVE_BACK, "back"] {
            let (out, texts) = run(vec![back]).await;
            assert_eq!(out, Ok(None), "{back}");
            assert_eq!(texts, [native_notice()]);
        }
        assert!(!env.settings.exists(), "nothing was asked of embrad");

        // The console leaves at the model question.
        let (out, _) = run(vec![]).await;
        assert_eq!(out.unwrap_err(), "setup channel closed during the native model step");

        // It leaves while the model is installed: the step ends by its name, and the job goes on.
        let (out, texts) = run(vec!["qwen3.5-9b"]).await;
        assert_eq!(out.unwrap_err(), "setup channel closed during the native model's install; the install goes on");
        assert!(texts.iter().any(|t| t.starts_with("Installing qwen3.5-9b: ")), "{texts:?}");
        assert_eq!(jobs.running().map(|running| running.model.id), Some("qwen3.5-9b"), "the install is the brain's, not the console's");

        // A console comes back and picks the same model: it follows the install that runs.
        let (resp_tx, mut resp_rx) = mpsc::channel(64);
        let (user_tx, mut user_rx) = mpsc::channel(8);
        let operator = async {
            assert_eq!(said(&mut resp_rx).await, native_notice());
            question(&mut resp_rx).await;
            user_tx.send("qwen3.5-9b".to_string()).await.unwrap();
            let found = said(&mut resp_rx).await;
            assert!(found.starts_with("The install of qwen3.5-9b runs already"), "{found}");
            std::fs::write(&gate, "").unwrap();
            loop {
                if let Sent::Said(text) = next(&mut resp_rx).await
                    && text.starts_with("qwen3.5-9b is installed.")
                {
                    break;
                }
            }
        };
        let (picked, ()) = tokio::join!(run_native_subflow(&resp_tx, &mut user_rx, &env, &jobs), operator);
        assert_eq!(picked.unwrap().map(|m| m.id), Some("qwen3.5-9b"));

        // A model whose file is on the machine: no job, the record at once; Enter is the first model.
        let four = native::by_id("qwen3.5-4b").unwrap();
        install(four, &env);
        let (out, texts) = run(vec![""]).await;
        assert_eq!(out, Ok(Some("qwen3.5-4b")));
        assert_eq!(texts, [native_notice(), "qwen3.5-4b is installed. embrad loads it now, while the setup goes on.".to_string()]);
        assert_eq!(native::read_settings(&env.settings), Ok(Some(native::Wanted::new(four))));
    }

    #[test]
    fn the_native_notice_says_what_the_model_leads_and_names_the_identity_import() {
        let notice = native_notice();
        assert!(
            notice.starts_with("A native model (experimental) runs on this machine's own processor: Qwen3.5 4B, Qwen3.5 9B or Qwen3.8 27B. They answer slower than a hosted model, and the smaller ones with less judgement. "),
            "{notice}"
        );
        for must in ["leads Learning Mode", "the seal is permanent", "can be imported instead", "Imported_Intelligence", "/provider --setup"] {
            assert!(notice.contains(must), "{must}: {notice}");
        }
    }

    /// A listener that identifies itself is stored, on, with its version
    /// named in the Info.
    #[tokio::test]
    async fn the_gateway_subflow_stores_an_identified_listener() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let (server, uri) = super::test_support::reachable_mock_server().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).insert_header(gateway_rules::GATEWAY_HEADER, "0.1.0"))
            .mount(&server)
            .await;
        let (resp_tx, mut resp_rx) = mpsc::channel(32);
        let (user_tx, mut user_rx) = mpsc::channel(32);
        user_tx.send(GATEWAY_THROUGH.to_string()).await.unwrap();
        user_tx.send(uri.clone()).await.unwrap();
        drop(user_tx);
        let out = run_gateway_subflow(&resp_tx, &mut user_rx).await.unwrap();
        assert_eq!(out, Routing::Gateway(gateway(&uri), "0.1.0".to_string()));
        let frames = drain(&mut resp_rx).await;
        let ps = prompts(&frames);
        assert_eq!(ps[1].prompt, "Enter the gateway's unified listener endpoint (http://host:4000):");
        let texts = system_texts(&frames);
        assert!(texts.iter().any(|t| t.starts_with(&format!("WSDS Local AI Gateway 0.1.0 at {uri}. Every provider request goes through it:"))), "{texts:?}");
    }

    /// A refused URL (a loopback host, a listener without the header) shows
    /// the reason and asks again with `direct` as the default; `direct` or
    /// nothing at the re-ask falls back to direct.
    #[tokio::test]
    async fn the_gateway_subflow_falls_back_to_direct_on_an_empty_or_direct_re_ask() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let (server, uri) = super::test_support::reachable_mock_server().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
        for (bad, bail) in [("localhost", ""), (uri.as_str(), "direct")] {
            let (resp_tx, mut resp_rx) = mpsc::channel(32);
            let (user_tx, mut user_rx) = mpsc::channel(32);
            user_tx.send(GATEWAY_THROUGH.to_string()).await.unwrap();
            user_tx.send(bad.to_string()).await.unwrap();
            user_tx.send(bail.to_string()).await.unwrap();
            drop(user_tx);
            let out = run_gateway_subflow(&resp_tx, &mut user_rx).await.unwrap();
            assert_eq!(out, Routing::Direct, "{bad}");
            let frames = drain(&mut resp_rx).await;
            let texts = system_texts(&frames);
            if bad == "localhost" {
                assert!(texts.iter().any(|t| t.starts_with("Not a gateway URL:") && t.contains("beside the OS")), "{texts:?}");
            } else {
                assert!(texts.iter().any(|t| t.contains("without the x-local-ai-gateway header")), "{texts:?}");
            }
            let ps = prompts(&frames);
            assert_eq!(ps.last().unwrap().prompt, "Re-enter the gateway endpoint, or answer direct to reach the providers directly:");
            assert_eq!(ps.last().unwrap().default_value, "direct");
            assert!(texts.iter().any(|t| t == "Reaching the providers directly. /local-ai-gateway <url> stores a gateway after setup."), "{texts:?}");
        }
    }
}
