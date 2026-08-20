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

/// Gemini image models (models page, 2026-08-20, all stable). Default =
/// `gemini-3-pro-image` (William's pick — there is no `gemini-3.1-pro-image`;
/// the 3.1 line is Flash / Flash-Lite only).
pub const GEMINI_IMAGE_MODELS: &[&str] = &[
    "gemini-3-pro-image",
    "gemini-3.1-flash-image",
    "gemini-3.1-flash-lite-image",
    "gemini-2.5-flash-image",
];
pub const DEFAULT_GEMINI_IMAGE_MODEL: &str = "gemini-3-pro-image";

pub const ASPECT_RATIOS: &[&str] = &["1:1", "3:2", "2:3", "3:4", "4:3", "4:5", "5:4", "9:16", "16:9", "21:9"];
/// Output sizes. `4K` is accepted ONLY with JPEG output: a 4K PNG can
/// exceed the 12 MiB store / gRPC ceiling (a 4K JPEG is 2–4 MB).
pub const IMAGE_SIZES: &[&str] = &["512px", "1K", "2K", "4K"];

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Png,
    Jpeg,
}

impl OutputFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "png" | "image/png" => Some(OutputFormat::Png),
            "jpeg" | "jpg" | "image/jpeg" => Some(OutputFormat::Jpeg),
            _ => None,
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            OutputFormat::Png => "image/png",
            OutputFormat::Jpeg => "image/jpeg",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImageGenRequest {
    pub prompt: String,
    pub aspect_ratio: Option<String>,
    pub size: Option<String>,
    pub output_format: OutputFormat,
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

/// Validate the (aspect_ratio, size, output_format, model) combination the
/// way the tool surfaces it — instructive messages, never a bare 400 from
/// the API for things we can check locally.
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
        if size == "4K" && req.output_format != OutputFormat::Jpeg {
            return Err(ImageGenError::InvalidRequest(
                "size 4K is only available with output_format=jpeg (a 4K PNG exceeds the 12 MiB store ceiling); use size 2K for PNG".into(),
            ));
        }
        if model == "gemini-3.1-flash-lite-image" && size != "1K" {
            return Err(ImageGenError::InvalidRequest(
                "gemini-3.1-flash-lite-image only renders 1K; omit size or pick another model".into(),
            ));
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
        ImageProviderKind::Gemini => Ok(Box::new(gemini::GeminiImageProvider::new(key, choice.model))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(size: Option<&str>, fmt: OutputFormat) -> ImageGenRequest {
        ImageGenRequest {
            prompt: "a poster".into(),
            aspect_ratio: Some("16:9".into()),
            size: size.map(str::to_string),
            output_format: fmt,
            reference_images: Vec::new(),
        }
    }

    #[test]
    fn image_provider_model_allowlist_defaults_to_pro_and_rejects_unknown() {
        assert_eq!(ImageProviderKind::Gemini.default_model(), "gemini-3-pro-image");
        assert!(GEMINI_IMAGE_MODELS.contains(&"gemini-3.1-flash-image"));
        assert!(!GEMINI_IMAGE_MODELS.contains(&"gemini-3.1-pro-image"), "no such model");
        let mut cfg = crate::config::tests_support::minimal_cfg();
        let c = resolve_image_choice(&cfg).unwrap();
        assert_eq!(c.kind, ImageProviderKind::Gemini);
        assert_eq!(c.model, "gemini-3-pro-image");
        cfg.image_model = Some("gemini-3.1-pro-image".into());
        assert!(matches!(resolve_image_choice(&cfg), Err(ImageGenError::NotConfigured(_))));
        cfg.image_model = Some("gemini-3.1-flash-image".into());
        assert_eq!(resolve_image_choice(&cfg).unwrap().model, "gemini-3.1-flash-image");
        cfg.image_provider = Some("openai".into());
        assert!(matches!(resolve_image_choice(&cfg), Err(ImageGenError::NotConfigured(_))));
    }

    #[test]
    fn image_generate_rejects_4k_png_and_allows_4k_jpeg() {
        assert!(validate_request("gemini-3-pro-image", &req(Some("4K"), OutputFormat::Png)).is_err());
        assert!(validate_request("gemini-3-pro-image", &req(Some("4K"), OutputFormat::Jpeg)).is_ok());
        assert!(validate_request("gemini-3-pro-image", &req(Some("2K"), OutputFormat::Png)).is_ok());
        assert!(validate_request("gemini-3-pro-image", &req(Some("3K"), OutputFormat::Png)).is_err());
        assert!(validate_request("gemini-3.1-flash-lite-image", &req(Some("2K"), OutputFormat::Png)).is_err());
        assert!(validate_request("gemini-3.1-flash-lite-image", &req(Some("1K"), OutputFormat::Png)).is_ok());
        let mut bad_ar = req(None, OutputFormat::Png);
        bad_ar.aspect_ratio = Some("7:3".into());
        assert!(validate_request("gemini-3-pro-image", &bad_ar).is_err());
        let mut empty = req(None, OutputFormat::Png);
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

    #[test]
    fn output_format_parses_and_maps() {
        assert_eq!(OutputFormat::parse("PNG"), Some(OutputFormat::Png));
        assert_eq!(OutputFormat::parse("jpg"), Some(OutputFormat::Jpeg));
        assert_eq!(OutputFormat::parse("image/jpeg"), Some(OutputFormat::Jpeg));
        assert_eq!(OutputFormat::parse("webp"), None);
        assert_eq!(OutputFormat::Jpeg.mime(), "image/jpeg");
        assert_eq!(OutputFormat::Png.mime(), "image/png");
    }
}
