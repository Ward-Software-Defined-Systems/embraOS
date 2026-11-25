//! Image-generation backends for the `image_generate` tool (media wave,
//! part 2). Pluggable like `LlmProvider`, but deliberately NOT an arm on
//! `ProviderKind` — that enum drives LLM construction and session-compat
//! checks. Two backends: Gemini's image models (`gemini.rs`) and
//! Higgsfield's (`higgsfield.rs`).
//!
//! Resolution (`resolve_image_provider`): `SystemConfig.image_provider`
//! (Gemini when unset) + the model allowlist + the provider's key. Gemini's:
//! `EMBRA_IMAGE_API_KEY` env > `/embra/state/api_key_image_gemini` (written
//! by `/image-provider key`) > the LLM Gemini key
//! (`SystemConfig::key_for(Gemini)`). Higgsfield's: `EMBRA_HIGGSFIELD_API_KEY`
//! env > `/embra/state/api_key_image_higgsfield`, a `<key id>:<key secret>`
//! of its API console. A key of the other provider's shape is never sent
//! (`key_shape_ok`).

pub mod gemini;
pub mod higgsfield;

use std::time::Duration;

use crate::config::SystemConfig;
use crate::provider::ProviderKind;

/// Wall-clock ceiling for one generation call. The pro model reasons
/// before rendering; the tool also sits under the dispatcher's 600 s
/// global backstop.
pub const IMAGE_GEN_TIMEOUT: Duration = Duration::from_secs(120);

/// STATE files for the dedicated image-generation keys, one a provider
/// (0600, written via `config::write_credential_state` by `/image-provider
/// key <token>` for the selected provider).
pub const IMAGE_KEY_PATH_GEMINI: &str = "/embra/state/api_key_image_gemini";
pub const IMAGE_KEY_PATH_HIGGSFIELD: &str = "/embra/state/api_key_image_higgsfield";
/// Env overrides for the image keys (highest precedence; dev/test).
pub const IMAGE_KEY_ENV: &str = "EMBRA_IMAGE_API_KEY";
pub const HIGGSFIELD_KEY_ENV: &str = "EMBRA_HIGGSFIELD_API_KEY";
/// A stand-in for Higgsfield's API (dev/test): its base replaces
/// `higgsfield::API_BASE`, and its origin is also taken for a picture's
/// address. Unset in the OS.
pub const HIGGSFIELD_BASE_ENV: &str = "EMBRA_HIGGSFIELD_BASE";

/// How a model takes the tool's `reference_images`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum References {
    /// It takes none, in this OS.
    None,
    /// Sent inline with the request itself (Gemini's).
    Inline,
}

/// One image model and what the tool may ask of it, read from the model's
/// own page. The tool's word for a size and the API's value for it are
/// kept apart, since an API may write a size its own way.
#[derive(Debug)]
pub struct ImageModel {
    pub id: &'static str,
    pub kind: ImageProviderKind,
    /// `(the tool's word, the API's value)`, in the model's own order;
    /// none for a model whose size is its own.
    pub sizes: &'static [(&'static str, &'static str)],
    /// The aspect ratios the tool takes for the model.
    pub ratios: &'static [&'static str],
    pub references: References,
    /// The prompt's bounds in characters, where the model's page states
    /// them.
    pub prompt_chars: Option<(usize, usize)>,
}

impl ImageModel {
    /// The API's value for one of the tool's size words.
    pub fn api_size(&self, word: &str) -> Option<&'static str> {
        self.sizes.iter().find(|(w, _)| *w == word).map(|(_, value)| *value)
    }
}

/// Every image model the OS offers, each with the sizes it renders.
/// Gemini's: Google's image guide and the model's own page. There is no
/// `gemini-3.1-pro-image`; the 3.1 line is Flash and Flash-Lite only.
pub const IMAGE_MODELS: &[ImageModel] = &[
    ImageModel {
        id: "gemini-nano-banana-2.1",
        kind: ImageProviderKind::Gemini,
        sizes: &[("1K", "1K"), ("2K", "2K"), ("4K", "4K")],
        ratios: ASPECT_RATIOS,
        references: References::Inline,
        prompt_chars: None,
    },
    ImageModel {
        id: "gemini-3-pro-image",
        kind: ImageProviderKind::Gemini,
        sizes: &[("1K", "1K"), ("2K", "2K"), ("4K", "4K")],
        ratios: ASPECT_RATIOS,
        references: References::Inline,
        prompt_chars: None,
    },
    ImageModel {
        id: "gemini-3.1-flash-image",
        kind: ImageProviderKind::Gemini,
        sizes: &[("512", "512"), ("1K", "1K"), ("2K", "2K"), ("4K", "4K")],
        ratios: ASPECT_RATIOS,
        references: References::Inline,
        prompt_chars: None,
    },
    ImageModel {
        id: "gemini-3.1-flash-lite-image",
        kind: ImageProviderKind::Gemini,
        sizes: &[("1K", "1K")],
        ratios: ASPECT_RATIOS,
        references: References::Inline,
        prompt_chars: None,
    },
    ImageModel {
        id: "gemini-2.5-flash-image",
        kind: ImageProviderKind::Gemini,
        sizes: &[("1K", "1K")],
        ratios: ASPECT_RATIOS,
        references: References::Inline,
        prompt_chars: None,
    },
    // Higgsfield's: each model's page at docs.higgsfield.ai/docs/models/,
    // its input schema, read 2026-11-24. Each is its own endpoint, whose id
    // is the model's id here. A size is the tool's word and the API's
    // value; a ratio of the page but "auto" (no ratio leaves the field
    // out, and the page's default stands).
    ImageModel {
        id: "xai/grok-imagine-image-2.0",
        kind: ImageProviderKind::Higgsfield,
        sizes: &[("1K", "1k"), ("2K", "2k")],
        ratios: &["1:1", "1:2", "2:1", "3:2", "2:3", "4:3", "3:4", "16:9", "9:16"],
        references: References::None,
        prompt_chars: None,
    },
    ImageModel {
        id: "higgsfield-ai/soul/v2/standard",
        kind: ImageProviderKind::Higgsfield,
        sizes: &[("720p", "720p"), ("1080p", "1080p")],
        ratios: &["9:16", "16:9", "4:3", "3:4", "1:1", "2:3", "3:2"],
        references: References::None,
        prompt_chars: None,
    },
    ImageModel {
        id: "ideogram/v4.0",
        kind: ImageProviderKind::Higgsfield,
        sizes: &[],
        ratios: &[
            "1:1", "1:2", "2:1", "2:3", "3:2", "4:5", "5:4", "9:16", "16:9", "5:8", "8:5", "3:4", "4:3", "9:22", "22:9",
            "9:23", "23:9", "3:8", "8:3", "5:12", "12:5", "1:3", "3:1",
        ],
        references: References::None,
        prompt_chars: Some((2, 2048)),
    },
    ImageModel {
        id: "z-image/turbo",
        kind: ImageProviderKind::Higgsfield,
        sizes: &[("1K", "1k"), ("2K", "2k")],
        ratios: &["1:1", "2:3", "3:2", "3:4", "4:3", "7:9", "9:7", "9:16", "16:9", "21:9"],
        references: References::None,
        prompt_chars: Some((1, 800)),
    },
];
/// Gemini's models of `IMAGE_MODELS`, in its order (guard
/// `every_image_model_has_its_sizes_and_each_is_a_value_of_the_api`). The
/// first is the default, William's pick: the model Google's guide
/// recommends for new work.
pub const GEMINI_IMAGE_MODELS: &[&str] = &[
    "gemini-nano-banana-2.1",
    "gemini-3-pro-image",
    "gemini-3.1-flash-image",
    "gemini-3.1-flash-lite-image",
    "gemini-2.5-flash-image",
];
pub const DEFAULT_GEMINI_IMAGE_MODEL: &str = "gemini-nano-banana-2.1";

/// Higgsfield's models of `IMAGE_MODELS`, in its order. The first is the
/// default, William's pick: the one that both makes a picture and edits
/// one with references.
pub const HIGGSFIELD_IMAGE_MODELS: &[&str] = &[
    "xai/grok-imagine-image-2.0",
    "higgsfield-ai/soul/v2/standard",
    "ideogram/v4.0",
    "z-image/turbo",
];
pub const DEFAULT_HIGGSFIELD_IMAGE_MODEL: &str = "xai/grok-imagine-image-2.0";
/// The size words of Higgsfield's models, every one's in the order of
/// `IMAGE_MODELS`.
pub const HIGGSFIELD_SIZE_WORDS: &[&str] = &["1K", "2K", "720p", "1080p"];

/// The ten ratios the tool takes for each of Gemini's models: every one
/// of them renders these.
pub const ASPECT_RATIOS: &[&str] = &["1:1", "3:2", "2:3", "3:4", "4:3", "4:5", "5:4", "9:16", "16:9", "21:9"];
/// The values of `image_size` in the API's reference
/// (`ImageResponseFormat`). The smallest is `512`: "512px" is how the
/// reference describes it and never a value. Which of the four a model
/// renders is `sizes_of`'s.
pub const IMAGE_SIZES: &[&str] = &["512", "1K", "2K", "4K"];
/// What the endpoint returns: `image/jpeg` is the one value of the
/// reference's `mime_type`. The file's name and the summary follow the
/// bytes that came back, not this.
pub const OUTPUT_MIME: &str = "image/jpeg";

/// The model of that id, of any provider.
pub fn image_model(id: &str) -> Option<&'static ImageModel> {
    IMAGE_MODELS.iter().find(|model| model.id == id)
}

/// The tool's words for the sizes `model` renders; none for a model that is
/// not listed.
pub fn sizes_of(model: &str) -> Vec<&'static str> {
    image_model(model).map_or_else(Vec::new, |model| model.sizes.iter().map(|(word, _)| *word).collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProviderKind {
    Gemini,
    Higgsfield,
}

impl ImageProviderKind {
    pub const ALL: [ImageProviderKind; 2] = [ImageProviderKind::Gemini, ImageProviderKind::Higgsfield];

    pub fn as_str(self) -> &'static str {
        match self {
            ImageProviderKind::Gemini => "gemini",
            ImageProviderKind::Higgsfield => "higgsfield",
        }
    }

    /// The provider's name in a sentence.
    pub fn label(self) -> &'static str {
        match self {
            ImageProviderKind::Gemini => "Gemini",
            ImageProviderKind::Higgsfield => "Higgsfield",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "gemini" => Some(ImageProviderKind::Gemini),
            "higgsfield" => Some(ImageProviderKind::Higgsfield),
            _ => None,
        }
    }

    /// The provider's dedicated key on STATE.
    pub fn key_path(self) -> &'static str {
        match self {
            ImageProviderKind::Gemini => IMAGE_KEY_PATH_GEMINI,
            ImageProviderKind::Higgsfield => IMAGE_KEY_PATH_HIGGSFIELD,
        }
    }

    /// The environment variable that overrides the provider's key.
    pub fn key_env(self) -> &'static str {
        match self {
            ImageProviderKind::Gemini => IMAGE_KEY_ENV,
            ImageProviderKind::Higgsfield => HIGGSFIELD_KEY_ENV,
        }
    }

    /// How long one call of the provider may take before the tool ends
    /// it: the provider's own bounds fit inside it.
    pub fn time_limit(self) -> Duration {
        match self {
            ImageProviderKind::Gemini => IMAGE_GEN_TIMEOUT,
            ImageProviderKind::Higgsfield => higgsfield::TIME_LIMIT,
        }
    }

    /// The tool's words for a refusal of the provider's API.
    pub fn api_error_words(self, status: u16, body: &str) -> String {
        match self {
            ImageProviderKind::Gemini => gemini::api_error_words(status, body),
            ImageProviderKind::Higgsfield => higgsfield::api_error_words(status, body),
        }
    }

    /// The tool's words when a call outlasted `time_limit`.
    pub fn timeout_words(self, limit: Duration) -> String {
        match self {
            ImageProviderKind::Gemini => gemini::timeout_words(limit),
            ImageProviderKind::Higgsfield => higgsfield::timeout_words(limit),
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            ImageProviderKind::Gemini => DEFAULT_GEMINI_IMAGE_MODEL,
            ImageProviderKind::Higgsfield => DEFAULT_HIGGSFIELD_IMAGE_MODEL,
        }
    }

    pub fn models(self) -> &'static [&'static str] {
        match self {
            ImageProviderKind::Gemini => GEMINI_IMAGE_MODELS,
            ImageProviderKind::Higgsfield => HIGGSFIELD_IMAGE_MODELS,
        }
    }

    /// The size words the provider's models know, in the order a refusal
    /// names them: for Gemini the values of its API's `image_size`.
    pub fn size_words(self) -> &'static [&'static str] {
        match self {
            ImageProviderKind::Gemini => IMAGE_SIZES,
            ImageProviderKind::Higgsfield => HIGGSFIELD_SIZE_WORDS,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImageGenRequest {
    pub prompt: String,
    pub aspect_ratio: Option<String>,
    pub size: Option<String>,
    /// `(mime_type, bytes)` reference images for editing / style guidance.
    pub reference_images: Vec<(String, Vec<u8>)>,
}

#[derive(Debug, Clone)]
pub struct GeneratedImage {
    pub media_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct ImageGenResponse {
    pub images: Vec<GeneratedImage>,
    /// Any text the model returned alongside (captions, refusals).
    pub text: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ImageGenError {
    #[error("image provider not configured: {0}")]
    NotConfigured(String),
    #[error("invalid image request: {0}")]
    InvalidRequest(String),
    #[error("image API error {status}: {body}")]
    Api { status: u16, body: String },
    #[error("image API returned no image: {0}")]
    NoImage(String),
    #[error("image generation transport error: {0}")]
    Transport(String),
    /// The request was still running at the provider's deadline.
    #[error("the request {request_id} did not finish within {waited_secs} s: {detail}")]
    StillRunning { request_id: String, waited_secs: u64, detail: String },
}

#[async_trait::async_trait]
pub trait ImageProvider: Send + Sync {
    fn kind(&self) -> ImageProviderKind;
    fn model(&self) -> &str;
    async fn generate(&self, req: &ImageGenRequest) -> Result<ImageGenResponse, ImageGenError>;
}

/// `validate_options` on a whole request: the tests' form. The tool
/// checks before it reads the references, with `validate_options`.
#[cfg(test)]
pub fn validate_request(model_id: &str, req: &ImageGenRequest) -> Result<(), ImageGenError> {
    validate_options(model_id, &req.prompt, req.aspect_ratio.as_deref(), req.size.as_deref(), req.reference_images.len())
}

/// Validate a request's options against the model, the way the tool
/// surfaces it — instructive messages, never a bare 400 from the API for
/// things we can check locally — before the references are read
/// (`references` is how many were asked for). Every check reads the
/// model's row of `IMAGE_MODELS`. A size is checked twice: that it is a
/// word of the model's provider, then that this model renders it.
pub fn validate_options(
    model_id: &str,
    prompt: &str,
    aspect_ratio: Option<&str>,
    size: Option<&str>,
    references: usize,
) -> Result<(), ImageGenError> {
    if prompt.trim().is_empty() {
        return Err(ImageGenError::InvalidRequest("prompt is empty".into()));
    }
    let Some(model) = image_model(model_id) else {
        return Err(ImageGenError::InvalidRequest(format!("{model_id} is not an image model of this OS")));
    };
    if let Some((least, most)) = model.prompt_chars {
        let n = prompt.chars().count();
        if n < least || n > most {
            return Err(ImageGenError::InvalidRequest(format!(
                "the prompt is {n} characters; {model_id} takes {least} to {most}"
            )));
        }
    }
    if let Some(ar) = aspect_ratio
        && !model.ratios.contains(&ar)
    {
        return Err(ImageGenError::InvalidRequest(format!(
            "aspect_ratio '{}' is not one of {}",
            ar,
            model.ratios.join("|")
        )));
    }
    if let Some(size) = size {
        let words = model.kind.size_words();
        if !words.contains(&size) {
            return Err(ImageGenError::InvalidRequest(format!(
                "size '{}' is not one of {}",
                size,
                words.join("|")
            )));
        }
        let rendered = sizes_of(model_id);
        if !rendered.contains(&size) {
            let by: Vec<&str> = IMAGE_MODELS
                .iter()
                .filter(|other| other.kind == model.kind && other.api_size(size).is_some())
                .map(|other| other.id)
                .collect();
            if rendered.is_empty() {
                return Err(ImageGenError::InvalidRequest(format!(
                    "{model_id} takes no size, its own is fixed; size {size} is rendered by {} (the operator picks the model with /image-provider model <id>). Omit size",
                    by.join(", ")
                )));
            }
            return Err(ImageGenError::InvalidRequest(format!(
                "{model_id} renders {}; size {size} is rendered by {} (the operator picks the model with /image-provider model <id>). Omit size or ask for one this model renders",
                rendered.join("|"),
                by.join(", ")
            )));
        }
    }
    if references > 0 && model.references == References::None {
        let by: Vec<&str> = IMAGE_MODELS
            .iter()
            .filter(|other| other.kind == model.kind && other.references != References::None)
            .map(|other| other.id)
            .collect();
        let way = if by.is_empty() {
            format!(
                "none of {}'s models takes one here: Gemini's do (the operator picks the provider with /image-provider gemini)",
                model.kind.label()
            )
        } else {
            format!("{} do (the operator picks the model with /image-provider model <id>)", by.join(", "))
        };
        return Err(ImageGenError::InvalidRequest(format!(
            "{model_id} takes no reference image; {way}. Omit reference_images"
        )));
    }
    Ok(())
}

/// Resolved backend choice, independent of whether a key is present —
/// `/image-provider` status renders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageProviderChoice {
    pub kind: ImageProviderKind,
    pub model: String,
    /// Where the key came from: `env` | `state` | `gemini-llm-key` | none.
    pub key_source: Option<&'static str>,
}

fn read_state_key(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Whether `key` has the shape of the provider's key, before it is stored
/// and again before it is sent, so a key never goes to the other provider:
/// Gemini's has no colon; Higgsfield's is a key id and a key secret joined
/// by one colon. Neither holds a space.
pub fn key_shape_ok(kind: ImageProviderKind, key: &str) -> bool {
    if key.is_empty() || key.chars().any(char::is_whitespace) {
        return false;
    }
    match kind {
        ImageProviderKind::Gemini => !key.contains(':'),
        ImageProviderKind::Higgsfield => matches!(key.split_once(':'), Some((id, secret)) if !id.is_empty() && !secret.is_empty() && !secret.contains(':')),
    }
}

/// The provider's key and where it came from. Gemini's: env > STATE image
/// key > the LLM Gemini key. Higgsfield's: env > STATE image key.
pub fn resolve_image_key(cfg: &SystemConfig, kind: ImageProviderKind) -> Option<(String, &'static str)> {
    resolve_image_key_inner(
        std::env::var(kind.key_env()).ok().filter(|s| !s.trim().is_empty()),
        read_state_key(kind.key_path()),
        match kind {
            ImageProviderKind::Gemini => cfg.key_for(ProviderKind::Gemini).map(str::to_string),
            ImageProviderKind::Higgsfield => None,
        },
    )
}

/// Every image key the OS holds, whichever provider is selected: each
/// provider's STATE file and environment variable, and the secret half of a
/// Higgsfield key on its own, for the outbound guard (`secrets::collect`).
/// `read` reads a STATE file, `var` the environment.
pub(crate) fn image_key_values(read: &dyn Fn(&str) -> Option<String>, var: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let mut values = Vec::new();
    for kind in ImageProviderKind::ALL {
        for value in [read(kind.key_path()), var(kind.key_env())].into_iter().flatten() {
            let value = value.trim().to_string();
            if value.is_empty() {
                continue;
            }
            if kind == ImageProviderKind::Higgsfield
                && let Some((_, secret)) = value.split_once(':')
                && !secret.is_empty()
            {
                values.push(secret.to_string());
            }
            values.push(value);
        }
    }
    values
}

/// `image_key_values` on this machine.
pub(crate) fn stored_image_keys() -> Vec<String> {
    image_key_values(&read_state_key, &|name| std::env::var(name).ok())
}

pub(crate) fn resolve_image_key_inner(
    env: Option<String>,
    state: Option<String>,
    llm_gemini: Option<String>,
) -> Option<(String, &'static str)> {
    env.map(|k| (k.trim().to_string(), "env"))
        .or_else(|| state.map(|k| (k, "state")))
        .or_else(|| llm_gemini.filter(|k| !k.is_empty()).map(|k| (k, "gemini-llm-key")))
}

/// The backend + model + key source the config resolves to (no network).
/// `image_provider` unset defaults to Gemini; an unknown provider or model
/// string is an error (the `/image-provider` command validates before
/// persisting, so this only trips on hand-edited config).
pub fn resolve_image_choice(cfg: &SystemConfig) -> Result<ImageProviderChoice, ImageGenError> {
    let kind = match cfg.image_provider.as_deref() {
        None | Some("") => ImageProviderKind::Gemini,
        Some(s) => ImageProviderKind::parse(s)
            .ok_or_else(|| ImageGenError::NotConfigured(format!("unknown image provider '{s}' (supported: gemini, higgsfield)")))?,
    };
    let model = match cfg.image_model.as_deref() {
        None | Some("") => kind.default_model().to_string(),
        Some(m) => {
            if !kind.models().contains(&m) {
                return Err(ImageGenError::NotConfigured(format!(
                    "image model '{}' is not one of {}",
                    m,
                    kind.models().join("|")
                )));
            }
            m.to_string()
        }
    };
    let key_source = resolve_image_key(cfg, kind).map(|(_, src)| src);
    Ok(ImageProviderChoice { kind, model, key_source })
}

/// Build the configured backend, or an instructive `NotConfigured`.
pub fn resolve_image_provider(cfg: &SystemConfig) -> Result<Box<dyn ImageProvider>, ImageGenError> {
    resolve_image_provider_with(
        cfg,
        &|kind| resolve_image_key(cfg, kind),
        std::env::var(HIGGSFIELD_BASE_ENV).ok().filter(|base| !base.trim().is_empty()),
    )
}

/// `resolve_image_provider` with the key and the stand-in's base given.
pub(crate) fn resolve_image_provider_with(
    cfg: &SystemConfig,
    key_of: &dyn Fn(ImageProviderKind) -> Option<(String, &'static str)>,
    stand_in: Option<String>,
) -> Result<Box<dyn ImageProvider>, ImageGenError> {
    let choice = resolve_image_choice(cfg)?;
    let (key, source) = key_of(choice.kind).ok_or_else(|| {
        ImageGenError::NotConfigured(match choice.kind {
            ImageProviderKind::Gemini => "no Gemini API key for image generation — set one with `/image-provider key <token>` (or configure Gemini as the LLM provider)".into(),
            ImageProviderKind::Higgsfield => "no Higgsfield key for image generation — set one with `/image-provider key <key id>:<key secret>`, a key of Higgsfield's API console at open.higgsfield.ai (an account and balance of its own, apart from a higgsfield.ai plan)".into(),
        })
    })?;
    if !key_shape_ok(choice.kind, &key) {
        return Err(ImageGenError::NotConfigured(format!(
            "the {} key ({source}) does not have the shape of one, so it is not sent: /image-provider key stores another",
            choice.kind.label()
        )));
    }
    match choice.kind {
        ImageProviderKind::Gemini => Ok(Box::new(gemini::GeminiImageProvider::with_endpoint(
            key,
            choice.model,
            interactions_url(cfg),
        ))),
        // Never through the provider gateway, which has no route for it.
        ImageProviderKind::Higgsfield => {
            let base = stand_in.clone().unwrap_or_else(|| higgsfield::API_BASE.to_string());
            Ok(Box::new(higgsfield::HiggsfieldImageProvider::new(
                &key,
                &choice.model,
                &base,
                stand_in.is_some(),
                higgsfield::Pacing::ADVISED,
            )?))
        }
    }
}

/// The Gemini image endpoint on the base the provider gateway rule gives:
/// the provider's own host direct, the gateway's `{url}/gemini` while it
/// is on.
pub(crate) fn interactions_url(cfg: &SystemConfig) -> String {
    let base = crate::provider::gateway::base_for(
        cfg.provider_gateway.as_ref(),
        crate::provider::ProviderKind::Gemini,
        crate::provider::gemini::DIRECT_BASE,
    );
    format!("{base}{}/interactions", crate::provider::gemini::V1BETA)
}

#[cfg(test)]
mod gateway_tests {
    const DIRECT: &str = "https://generativelanguage.googleapis.com/v1beta/interactions";

    #[test]
    fn the_image_endpoint_follows_the_gateway_when_on() {
        let mut cfg = crate::config::tests_support::minimal_cfg();
        assert_eq!(super::interactions_url(&cfg), DIRECT);
        cfg.provider_gateway = Some(crate::config::ProviderGateway {
            url: "http://gw:4000".into(),
            enabled: true,
        });
        assert_eq!(
            super::interactions_url(&cfg),
            "http://gw:4000/gemini/v1beta/interactions"
        );
        cfg.provider_gateway.as_mut().unwrap().enabled = false;
        assert_eq!(super::interactions_url(&cfg), DIRECT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(size: Option<&str>) -> ImageGenRequest {
        ImageGenRequest {
            prompt: "a poster".into(),
            aspect_ratio: Some("16:9".into()),
            size: size.map(str::to_string),
            reference_images: Vec::new(),
        }
    }

    #[test]
    fn image_provider_model_allowlist_defaults_to_nano_banana_and_rejects_unknown() {
        assert_eq!(ImageProviderKind::Gemini.default_model(), "gemini-nano-banana-2.1");
        assert!(GEMINI_IMAGE_MODELS.contains(&ImageProviderKind::Gemini.default_model()));
        assert!(GEMINI_IMAGE_MODELS.contains(&"gemini-3-pro-image"), "the former default stays selectable");
        assert!(GEMINI_IMAGE_MODELS.contains(&"gemini-3.1-flash-image"));
        assert!(!GEMINI_IMAGE_MODELS.contains(&"gemini-3.1-pro-image"), "no such model");
        let mut cfg = crate::config::tests_support::minimal_cfg();
        let c = resolve_image_choice(&cfg).unwrap();
        assert_eq!(c.kind, ImageProviderKind::Gemini);
        assert_eq!(c.model, "gemini-nano-banana-2.1");
        // A model the operator picked before the default moved stays theirs.
        cfg.image_model = Some("gemini-3-pro-image".into());
        assert_eq!(resolve_image_choice(&cfg).unwrap().model, "gemini-3-pro-image");
        cfg.image_model = Some("gemini-3.1-pro-image".into());
        assert!(matches!(resolve_image_choice(&cfg), Err(ImageGenError::NotConfigured(_))));
        cfg.image_model = Some("gemini-3.1-flash-image".into());
        assert_eq!(resolve_image_choice(&cfg).unwrap().model, "gemini-3.1-flash-image");
        cfg.image_provider = Some("openai".into());
        assert!(matches!(resolve_image_choice(&cfg), Err(ImageGenError::NotConfigured(_))));
    }

    /// The values of the API's reference, typed here from it and not read
    /// from the constants under test: `ImageResponseFormat.image_size`.
    const REFERENCE_SIZES: [&str; 4] = ["512", "1K", "2K", "4K"];

    /// Every listed model has a row of sizes, in the list's order, no row
    /// holds a size the API does not know, and every size of the API is
    /// rendered by a model, so a refusal can always name one.
    #[test]
    fn every_image_model_has_its_sizes_and_each_is_a_value_of_the_api() {
        assert_eq!(IMAGE_SIZES, REFERENCE_SIZES);
        let gemini: Vec<&ImageModel> = IMAGE_MODELS.iter().filter(|m| m.kind == ImageProviderKind::Gemini).collect();
        let named: Vec<&str> = gemini.iter().map(|m| m.id).collect();
        assert_eq!(named, GEMINI_IMAGE_MODELS);
        assert_eq!(GEMINI_IMAGE_MODELS[0], DEFAULT_GEMINI_IMAGE_MODEL, "the default is listed first");
        for model in &gemini {
            assert!(!model.sizes.is_empty(), "{}", model.id);
            assert!(model.api_size("1K").is_some(), "{}: every model renders 1K, the size of a call that names none", model.id);
            for (word, value) in model.sizes {
                assert!(REFERENCE_SIZES.contains(value), "{}: {value} is no value of image_size", model.id);
                assert_eq!(word, value, "{}: Gemini's API takes the tool's word as it is", model.id);
            }
            assert_eq!(model.ratios, ASPECT_RATIOS, "{}", model.id);
        }
        for size in REFERENCE_SIZES {
            assert!(gemini.iter().any(|m| m.api_size(size).is_some()), "no model renders {size}");
        }
        let ids: Vec<&str> = IMAGE_MODELS.iter().map(|m| m.id).collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "an id names one model");
        assert!(sizes_of("gemini-3.1-pro-image").is_empty(), "a model that is not listed renders nothing");
        assert_eq!(OUTPUT_MIME, "image/jpeg", "the one value of the reference's mime_type");
    }

    /// Higgsfield's rows: one a model of its list, in its order, the default
    /// first; its size words are its models', each rendered by one; a word
    /// is the API's value but for its case.
    #[test]
    fn higgsfields_models_are_its_list_and_its_size_words_are_theirs() {
        let rows: Vec<&ImageModel> = IMAGE_MODELS.iter().filter(|m| m.kind == ImageProviderKind::Higgsfield).collect();
        let named: Vec<&str> = rows.iter().map(|m| m.id).collect();
        assert_eq!(named, HIGGSFIELD_IMAGE_MODELS);
        assert_eq!(HIGGSFIELD_IMAGE_MODELS[0], DEFAULT_HIGGSFIELD_IMAGE_MODEL, "the default is listed first");
        let mut words: Vec<&str> = Vec::new();
        for model in &rows {
            for (word, value) in model.sizes {
                assert!(word.eq_ignore_ascii_case(value), "{}: {word} sends {value}", model.id);
                if !words.contains(word) {
                    words.push(word);
                }
            }
            assert!(!model.ratios.contains(&"auto"), "{}: no ratio means the model's own", model.id);
        }
        assert_eq!(words, HIGGSFIELD_SIZE_WORDS);
        assert_eq!(ImageProviderKind::Higgsfield.size_words(), HIGGSFIELD_SIZE_WORDS);
        assert_eq!(ImageProviderKind::parse(" Higgsfield "), Some(ImageProviderKind::Higgsfield));
        for kind in ImageProviderKind::ALL {
            assert_eq!(ImageProviderKind::parse(kind.as_str()), Some(kind));
            assert!(kind.models().contains(&kind.default_model()));
        }
    }

    /// What Higgsfield's models refuse before a call: a size the model does
    /// not render, a size on the model whose size is its own, a prompt past
    /// a model's bounds, a ratio of another model, and a reference.
    #[test]
    fn higgsfields_models_refuse_what_their_pages_do_not_take() {
        let refused = |model: &str, prompt: &str, ratio: Option<&str>, size: Option<&str>, references: usize| {
            validate_options(model, prompt, ratio, size, references).expect_err("refused").to_string()
        };
        const TAIL: &str = " (the operator picks the model with /image-provider model <id>).";
        assert_eq!(
            refused("ideogram/v4.0", "a poster", None, Some("1K"), 0),
            format!("invalid image request: ideogram/v4.0 takes no size, its own is fixed; size 1K is rendered by xai/grok-imagine-image-2.0, z-image/turbo{TAIL} Omit size")
        );
        assert_eq!(
            refused("z-image/turbo", "a poster", None, Some("720p"), 0),
            format!("invalid image request: z-image/turbo renders 1K|2K; size 720p is rendered by higgsfield-ai/soul/v2/standard{TAIL} Omit size or ask for one this model renders")
        );
        assert_eq!(
            refused("xai/grok-imagine-image-2.0", "a poster", None, Some("4K"), 0),
            "invalid image request: size '4K' is not one of 1K|2K|720p|1080p"
        );
        assert_eq!(refused("ideogram/v4.0", "x", None, None, 0), "invalid image request: the prompt is 1 characters; ideogram/v4.0 takes 2 to 2048");
        assert_eq!(
            refused("z-image/turbo", &"é".repeat(801), None, None, 0),
            "invalid image request: the prompt is 801 characters; z-image/turbo takes 1 to 800"
        );
        assert!(validate_options("z-image/turbo", &"é".repeat(800), None, None, 0).is_ok(), "characters, not bytes");
        assert_eq!(
            refused("higgsfield-ai/soul/v2/standard", "a poster", Some("21:9"), None, 0),
            "invalid image request: aspect_ratio '21:9' is not one of 9:16|16:9|4:3|3:4|1:1|2:3|3:2"
        );
        assert_eq!(
            refused("xai/grok-imagine-image-2.0", "a poster", None, None, 1),
            "invalid image request: xai/grok-imagine-image-2.0 takes no reference image; none of Higgsfield's models takes one here: Gemini's do (the operator picks the provider with /image-provider gemini). Omit reference_images"
        );
        assert!(validate_options("ideogram/v4.0", "a poster", Some("22:9"), None, 0).is_ok());
        assert!(validate_options("higgsfield-ai/soul/v2/standard", "a poster", Some("3:2"), Some("1080p"), 0).is_ok());
    }

    /// Every refusal of Gemini's models, word for word as it read before
    /// the models went into one table with a second provider's: a change
    /// to the table changes none of them.
    #[test]
    fn every_gemini_refusal_reads_as_it_did() {
        let refused = |model: &str, prompt: &str, ratio: Option<&str>, size: Option<&str>| {
            let request = ImageGenRequest {
                prompt: prompt.into(),
                aspect_ratio: ratio.map(str::to_string),
                size: size.map(str::to_string),
                reference_images: Vec::new(),
            };
            validate_request(model, &request).expect_err("refused").to_string()
        };
        const TAIL: &str = " (the operator picks the model with /image-provider model <id>). Omit size or ask for one this model renders";
        for model in GEMINI_IMAGE_MODELS {
            assert_eq!(refused(model, "  ", None, None), "invalid image request: prompt is empty");
            for ratio in ["7:3", "1:4", "4:1", "auto", ""] {
                assert_eq!(
                    refused(model, "a poster", Some(ratio), None),
                    format!("invalid image request: aspect_ratio '{ratio}' is not one of 1:1|3:2|2:3|3:4|4:3|4:5|5:4|9:16|16:9|21:9")
                );
            }
            for size in ["512px", "3K", "1k", "0.5K", "1024", "720p", ""] {
                assert_eq!(
                    refused(model, "a poster", Some("16:9"), Some(size)),
                    format!("invalid image request: size '{size}' is not one of 512|1K|2K|4K")
                );
            }
        }
        let renders = [
            ("gemini-nano-banana-2.1", "512", "gemini-nano-banana-2.1 renders 1K|2K|4K; size 512 is rendered by gemini-3.1-flash-image"),
            ("gemini-3-pro-image", "512", "gemini-3-pro-image renders 1K|2K|4K; size 512 is rendered by gemini-3.1-flash-image"),
            ("gemini-3.1-flash-lite-image", "512", "gemini-3.1-flash-lite-image renders 1K; size 512 is rendered by gemini-3.1-flash-image"),
            ("gemini-3.1-flash-lite-image", "2K", "gemini-3.1-flash-lite-image renders 1K; size 2K is rendered by gemini-nano-banana-2.1, gemini-3-pro-image, gemini-3.1-flash-image"),
            ("gemini-3.1-flash-lite-image", "4K", "gemini-3.1-flash-lite-image renders 1K; size 4K is rendered by gemini-nano-banana-2.1, gemini-3-pro-image, gemini-3.1-flash-image"),
            ("gemini-2.5-flash-image", "512", "gemini-2.5-flash-image renders 1K; size 512 is rendered by gemini-3.1-flash-image"),
            ("gemini-2.5-flash-image", "2K", "gemini-2.5-flash-image renders 1K; size 2K is rendered by gemini-nano-banana-2.1, gemini-3-pro-image, gemini-3.1-flash-image"),
            ("gemini-2.5-flash-image", "4K", "gemini-2.5-flash-image renders 1K; size 4K is rendered by gemini-nano-banana-2.1, gemini-3-pro-image, gemini-3.1-flash-image"),
        ];
        for (model, size, words) in renders {
            assert_eq!(refused(model, "a poster", None, Some(size)), format!("invalid image request: {words}{TAIL}"));
        }
        // And what is not refused: each model with the sizes it renders and every ratio.
        let taken = [
            ("gemini-nano-banana-2.1", &["1K", "2K", "4K"][..]),
            ("gemini-3-pro-image", &["1K", "2K", "4K"][..]),
            ("gemini-3.1-flash-image", &["512", "1K", "2K", "4K"][..]),
            ("gemini-3.1-flash-lite-image", &["1K"][..]),
            ("gemini-2.5-flash-image", &["1K"][..]),
        ];
        for (model, sizes) in taken {
            for size in sizes.iter().map(|s| Some(*s)).chain([None]) {
                for ratio in ["1:1", "3:2", "2:3", "3:4", "4:3", "4:5", "5:4", "9:16", "16:9", "21:9"].map(Some).into_iter().chain([None]) {
                    let request = ImageGenRequest {
                        prompt: "a poster".into(),
                        aspect_ratio: ratio.map(str::to_string),
                        size: size.map(str::to_string),
                        reference_images: vec![("image/png".into(), vec![1, 2, 3]); 4],
                    };
                    assert!(validate_request(model, &request).is_ok(), "{model} {size:?} {ratio:?}");
                }
            }
        }
    }

    /// A size is refused before the call when it is no value of the API,
    /// and when the model does not render it; the refusal names the model's
    /// own sizes and the models that render what was asked.
    #[test]
    fn a_size_is_checked_against_the_api_and_against_the_model() {
        for model in GEMINI_IMAGE_MODELS {
            assert!(validate_request(model, &req(None)).is_ok(), "{model}: no size");
            assert!(validate_request(model, &req(Some("1K"))).is_ok(), "{model}: 1K");
            for size in REFERENCE_SIZES {
                let answer = validate_request(model, &req(Some(size)));
                assert_eq!(answer.is_ok(), sizes_of(model).contains(&size), "{model} {size}: {answer:?}");
            }
            for no_value in ["512px", "3K", "1k", "0.5K", "1024"] {
                let err = validate_request(model, &req(Some(no_value))).unwrap_err().to_string();
                assert!(err.contains("is not one of 512|1K|2K|4K"), "{model} {no_value}: {err}");
            }
        }
        // The default model and the former one: 1K, 2K and 4K, never 512.
        for model in ["gemini-nano-banana-2.1", "gemini-3-pro-image"] {
            assert!(validate_request(model, &req(Some("2K"))).is_ok());
            assert!(validate_request(model, &req(Some("4K"))).is_ok(), "4K needs no other option");
            let err = validate_request(model, &req(Some("512"))).unwrap_err().to_string();
            assert!(
                err.contains(&format!("{model} renders 1K|2K|4K; size 512 is rendered by gemini-3.1-flash-image (")),
                "{err}"
            );
            assert!(err.contains("/image-provider model <id>"), "{err}");
        }
        assert!(validate_request("gemini-3.1-flash-image", &req(Some("512"))).is_ok());
        for model in ["gemini-3.1-flash-lite-image", "gemini-2.5-flash-image"] {
            let err = validate_request(model, &req(Some("2K"))).unwrap_err().to_string();
            assert!(
                err.contains(&format!(
                    "{model} renders 1K; size 2K is rendered by gemini-nano-banana-2.1, gemini-3-pro-image, gemini-3.1-flash-image ("
                )),
                "{err}"
            );
        }
        let mut bad_ar = req(None);
        bad_ar.aspect_ratio = Some("7:3".into());
        assert!(validate_request("gemini-3-pro-image", &bad_ar).is_err());
        let mut empty = req(None);
        empty.prompt = "  ".into();
        assert!(validate_request("gemini-3-pro-image", &empty).is_err());
    }

    /// A key reaches the provider whose shape it has and no other.
    #[test]
    fn a_key_is_taken_by_the_provider_whose_shape_it_has() {
        use ImageProviderKind::{Gemini, Higgsfield};
        for (key, gemini, higgsfield) in [
            ("AIzaSy-stand-in-0123", true, false),
            ("hf-id-1:hf-secret-1", false, true),
            ("a:b", false, true),
            (":secret", false, false),
            ("id:", false, false),
            ("id:se:cret", false, false),
            ("id: secret", false, false),
            ("", false, false),
            ("AIza Sy", false, false),
        ] {
            assert_eq!(key_shape_ok(Gemini, key), gemini, "{key:?} for Gemini");
            assert_eq!(key_shape_ok(Higgsfield, key), higgsfield, "{key:?} for Higgsfield");
        }
    }

    /// The provider the configuration selects, with its key: refused
    /// without one, and refused with a key of the other provider's shape,
    /// which is never sent.
    #[test]
    fn the_selected_provider_is_built_with_its_own_key_and_never_the_others() {
        let mut cfg = crate::config::tests_support::minimal_cfg();
        let none = |_: ImageProviderKind| None;
        cfg.image_provider = Some("higgsfield".into());
        let err = resolve_image_provider_with(&cfg, &none, None).err().unwrap().to_string();
        assert!(err.contains("/image-provider key <key id>:<key secret>") && err.contains("open.higgsfield.ai"), "{err}");
        let gemini_shaped = |_: ImageProviderKind| Some(("AIzaSy-0123456789".to_string(), "env"));
        let err = resolve_image_provider_with(&cfg, &gemini_shaped, None).err().unwrap().to_string();
        assert_eq!(err, "image provider not configured: the Higgsfield key (env) does not have the shape of one, so it is not sent: /image-provider key stores another");
        let pair = |_: ImageProviderKind| Some(("hf-id:hf-secret".to_string(), "state"));
        let built = resolve_image_provider_with(&cfg, &pair, None).ok().unwrap();
        assert_eq!((built.kind(), built.model()), (ImageProviderKind::Higgsfield, DEFAULT_HIGGSFIELD_IMAGE_MODEL));
        cfg.image_model = Some("z-image/turbo".into());
        assert_eq!(resolve_image_provider_with(&cfg, &pair, Some("http://10.0.2.2:28082".into())).ok().unwrap().model(), "z-image/turbo");
        cfg.image_provider = None;
        cfg.image_model = None;
        let err = resolve_image_provider_with(&cfg, &pair, None).err().unwrap().to_string();
        assert!(err.contains("the Gemini key (state) does not have the shape of one"), "{err}");
        let built = resolve_image_provider_with(&cfg, &gemini_shaped, None).ok().unwrap();
        assert_eq!(built.kind(), ImageProviderKind::Gemini);
        // A Higgsfield model stored under Gemini is refused, as any model
        // not of the provider is.
        cfg.image_model = Some("z-image/turbo".into());
        assert!(matches!(resolve_image_choice(&cfg), Err(ImageGenError::NotConfigured(_))));
    }

    /// Every image key on the machine is collected for the outbound guard,
    /// whichever provider is selected, and a Higgsfield key's secret half on
    /// its own as well.
    #[test]
    fn every_image_key_and_the_secret_half_of_higgsfields_are_collected() {
        let read = |path: &str| match path {
            IMAGE_KEY_PATH_GEMINI => Some("AIzaSy-stored-0123456789\n".to_string()),
            IMAGE_KEY_PATH_HIGGSFIELD => Some("hf-id-1:hf-secret-stored-1\n".to_string()),
            _ => None,
        };
        let var = |name: &str| match name {
            HIGGSFIELD_KEY_ENV => Some("hf-id-2:hf-secret-env-2".to_string()),
            _ => None,
        };
        let values = image_key_values(&read, &var);
        assert_eq!(
            values,
            ["AIzaSy-stored-0123456789", "hf-secret-stored-1", "hf-id-1:hf-secret-stored-1", "hf-secret-env-2", "hf-id-2:hf-secret-env-2"]
        );
        assert!(image_key_values(&|_| None, &|_| Some("  ".into())).is_empty());
    }

    #[test]
    fn resolve_image_provider_precedence_env_state_gemini_fallback() {
        assert_eq!(
            resolve_image_key_inner(Some(" e ".into()), Some("s".into()), Some("g".into())),
            Some(("e".to_string(), "env"))
        );
        assert_eq!(
            resolve_image_key_inner(None, Some("s".into()), Some("g".into())),
            Some(("s".to_string(), "state"))
        );
        assert_eq!(
            resolve_image_key_inner(None, None, Some("g".into())),
            Some(("g".to_string(), "gemini-llm-key"))
        );
        assert_eq!(resolve_image_key_inner(None, None, Some(String::new())), None);
        assert_eq!(resolve_image_key_inner(None, None, None), None);
    }
}
