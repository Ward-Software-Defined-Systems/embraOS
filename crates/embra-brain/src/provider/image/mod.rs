//! Image-generation backends for the `image_generate` tool (media wave,
//! part 2). Pluggable like `LlmProvider`, but deliberately NOT an arm on
//! `ProviderKind` — that enum drives LLM construction and session-compat
//! checks. One backend today: Gemini's image models (`gemini.rs`).
//!
//! Resolution (`resolve_image_provider`): `SystemConfig.image_provider`
//! (or Gemini by default when a Gemini key is resolvable) + the model
//! allowlist + the key precedence `EMBRA_IMAGE_API_KEY` env >
//! `/embra/state/api_key_image_gemini` (written by `/image-provider key`)
//! > the LLM Gemini key (`SystemConfig::key_for(Gemini)`).

pub mod gemini;

use std::time::Duration;

use crate::config::SystemConfig;
use crate::provider::ProviderKind;

/// Wall-clock ceiling for one generation call. The pro model reasons
/// before rendering; the tool also sits under the dispatcher's 600 s
/// global backstop.
pub const IMAGE_GEN_TIMEOUT: Duration = Duration::from_secs(120);

/// STATE file for the dedicated image-generation key (0600, written via
/// `config::write_credential_state` by `/image-provider key <token>`).
pub const IMAGE_KEY_PATH_GEMINI: &str = "/embra/state/api_key_image_gemini";
/// Env override for the image key (highest precedence; dev/test).
pub const IMAGE_KEY_ENV: &str = "EMBRA_IMAGE_API_KEY";

/// Gemini image models, each with the sizes it renders: Google's image
/// guide and the model's own page. The first is the default, William's
/// pick: the model the guide recommends for new work. There is no
/// `gemini-3.1-pro-image`; the 3.1 line is Flash and Flash-Lite only.
const MODEL_SIZES: &[(&str, &[&str])] = &[
    ("gemini-nano-banana-2.1", &["1K", "2K", "4K"]),
    ("gemini-3-pro-image", &["1K", "2K", "4K"]),
    ("gemini-3.1-flash-image", &["512", "1K", "2K", "4K"]),
    ("gemini-3.1-flash-lite-image", &["1K"]),
    ("gemini-2.5-flash-image", &["1K"]),
];
/// The names of `MODEL_SIZES`, in its order (guard
/// `every_image_model_has_its_sizes_and_each_is_a_value_of_the_api`).
pub const GEMINI_IMAGE_MODELS: &[&str] = &[
    "gemini-nano-banana-2.1",
    "gemini-3-pro-image",
    "gemini-3.1-flash-image",
    "gemini-3.1-flash-lite-image",
    "gemini-2.5-flash-image",
];
pub const DEFAULT_GEMINI_IMAGE_MODEL: &str = "gemini-nano-banana-2.1";

/// The ten ratios every model above renders.
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

/// The sizes `model` renders; none for a model that is not listed.
pub fn sizes_of(model: &str) -> &'static [&'static str] {
    MODEL_SIZES
        .iter()
        .find(|(name, _)| *name == model)
        .map_or(&[], |(_, sizes)| sizes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProviderKind {
    Gemini,
}

impl ImageProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ImageProviderKind::Gemini => "gemini",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "gemini" => Some(ImageProviderKind::Gemini),
            _ => None,
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            ImageProviderKind::Gemini => DEFAULT_GEMINI_IMAGE_MODEL,
        }
    }

    pub fn models(self) -> &'static [&'static str] {
        match self {
            ImageProviderKind::Gemini => GEMINI_IMAGE_MODELS,
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
}

#[async_trait::async_trait]
pub trait ImageProvider: Send + Sync {
    fn kind(&self) -> ImageProviderKind;
    fn model(&self) -> &str;
    async fn generate(&self, req: &ImageGenRequest) -> Result<ImageGenResponse, ImageGenError>;
}

/// Validate the (aspect_ratio, size, model) combination the way the tool
/// surfaces it — instructive messages, never a bare 400 from the API for
/// things we can check locally. A size is checked twice: that it is a
/// value of the API, then that this model renders it.
pub fn validate_request(model: &str, req: &ImageGenRequest) -> Result<(), ImageGenError> {
    if req.prompt.trim().is_empty() {
        return Err(ImageGenError::InvalidRequest("prompt is empty".into()));
    }
    if let Some(ar) = req.aspect_ratio.as_deref()
        && !ASPECT_RATIOS.contains(&ar)
    {
        return Err(ImageGenError::InvalidRequest(format!(
            "aspect_ratio '{}' is not one of {}",
            ar,
            ASPECT_RATIOS.join("|")
        )));
    }
    if let Some(size) = req.size.as_deref() {
        if !IMAGE_SIZES.contains(&size) {
            return Err(ImageGenError::InvalidRequest(format!(
                "size '{}' is not one of {}",
                size,
                IMAGE_SIZES.join("|")
            )));
        }
        let rendered = sizes_of(model);
        if !rendered.contains(&size) {
            let by: Vec<&str> = MODEL_SIZES
                .iter()
                .filter(|(_, sizes)| sizes.contains(&size))
                .map(|(name, _)| *name)
                .collect();
            return Err(ImageGenError::InvalidRequest(format!(
                "{model} renders {}; size {size} is rendered by {} (the operator picks the model with /image-provider model <id>). Omit size or ask for one this model renders",
                rendered.join("|"),
                by.join(", ")
            )));
        }
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

/// Key precedence: env > STATE image key > the LLM Gemini key.
pub fn resolve_image_key(cfg: &SystemConfig) -> Option<(String, &'static str)> {
    resolve_image_key_inner(
        std::env::var(IMAGE_KEY_ENV).ok().filter(|s| !s.trim().is_empty()),
        read_state_key(IMAGE_KEY_PATH_GEMINI),
        cfg.key_for(ProviderKind::Gemini).map(str::to_string),
    )
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
            .ok_or_else(|| ImageGenError::NotConfigured(format!("unknown image provider '{s}' (supported: gemini)")))?,
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
    let key_source = resolve_image_key(cfg).map(|(_, src)| src);
    Ok(ImageProviderChoice { kind, model, key_source })
}

/// Build the configured backend, or an instructive `NotConfigured`.
pub fn resolve_image_provider(cfg: &SystemConfig) -> Result<Box<dyn ImageProvider>, ImageGenError> {
    let choice = resolve_image_choice(cfg)?;
    let (key, _) = resolve_image_key(cfg).ok_or_else(|| {
        ImageGenError::NotConfigured(
            "no Gemini API key for image generation — set one with `/image-provider key <token>` (or configure Gemini as the LLM provider)".into(),
        )
    })?;
    match choice.kind {
        ImageProviderKind::Gemini => Ok(Box::new(gemini::GeminiImageProvider::with_endpoint(
            key,
            choice.model,
            interactions_url(cfg),
        ))),
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
        let named: Vec<&str> = MODEL_SIZES.iter().map(|(name, _)| *name).collect();
        assert_eq!(named, GEMINI_IMAGE_MODELS);
        assert_eq!(GEMINI_IMAGE_MODELS[0], DEFAULT_GEMINI_IMAGE_MODEL, "the default is listed first");
        for (model, sizes) in MODEL_SIZES {
            assert!(!sizes.is_empty(), "{model}");
            assert!(sizes.contains(&"1K"), "{model}: every model renders 1K, the size of a call that names none");
            for size in *sizes {
                assert!(REFERENCE_SIZES.contains(size), "{model}: {size} is no value of image_size");
            }
            assert_eq!(sizes_of(model), *sizes);
        }
        for size in REFERENCE_SIZES {
            assert!(MODEL_SIZES.iter().any(|(_, sizes)| sizes.contains(&size)), "no model renders {size}");
        }
        assert!(sizes_of("gemini-3.1-pro-image").is_empty(), "a model that is not listed renders nothing");
        assert_eq!(OUTPUT_MIME, "image/jpeg", "the one value of the reference's mime_type");
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
