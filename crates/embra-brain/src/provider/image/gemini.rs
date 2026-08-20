//! Gemini image generation over the Interactions API
//! (`POST https://generativelanguage.googleapis.com/v1beta/interactions`,
//! header `x-goog-api-key`). Verified against the REST reference
//! 2026-08-20: request `{model, input:[{type:"text",text}|{type:"image",
//! mime_type,data}], response_format:{type:"image", mime_type,
//! aspect_ratio?, image_size?}}` (snake_case); response `{id, model,
//! status, steps:[{type:"model_output", content:[{type:"text",text}|
//! {type:"image", data, mime_type}]}], usage}`; `status` ∈ completed |
//! failed | incomplete | …; errors `{error:{code, message}}`.
//!
//! The request/response codecs are pure functions (wiremock-tested); the
//! transport is one POST with explicit timeouts — the tool layer adds the
//! `IMAGE_GEN_TIMEOUT` ceiling and the dispatcher its 600 s backstop.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value as JsonValue};

use super::{GeneratedImage, ImageGenError, ImageGenRequest, ImageGenResponse, ImageProvider, ImageProviderKind};

pub const DEFAULT_ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta/interactions";

pub struct GeminiImageProvider {
    api_key: String,
    model: String,
    endpoint: String,
    http: reqwest::Client,
}

impl GeminiImageProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self::with_endpoint(api_key, model, DEFAULT_ENDPOINT.to_string())
    }

    pub fn with_endpoint(api_key: String, model: String, endpoint: String) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(super::IMAGE_GEN_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { api_key, model, endpoint, http }
    }

    /// Pure request codec.
    pub fn request_body(model: &str, req: &ImageGenRequest) -> JsonValue {
        let mut input = Vec::with_capacity(req.reference_images.len() + 1);
        input.push(json!({"type": "text", "text": req.prompt}));
        for (mime, bytes) in &req.reference_images {
            input.push(json!({
                "type": "image",
                "mime_type": mime,
                "data": STANDARD.encode(bytes),
            }));
        }
        let mut response_format = serde_json::Map::new();
        response_format.insert("type".into(), json!("image"));
        response_format.insert("mime_type".into(), json!(req.output_format.mime()));
        if let Some(ar) = &req.aspect_ratio {
            response_format.insert("aspect_ratio".into(), json!(ar));
        }
        if let Some(size) = &req.size {
            response_format.insert("image_size".into(), json!(size));
        }
        json!({
            "model": model,
            "input": input,
            "response_format": JsonValue::Object(response_format),
        })
    }

    /// Pure response codec: every `image` block in every step's `content`
    /// (the `model_output` step in practice), plus concatenated text.
    pub fn parse_response(body: &JsonValue) -> Result<ImageGenResponse, ImageGenError> {
        if let Some(err) = body.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            return Err(ImageGenError::Api { status: 0, body: msg.to_string() });
        }
        let status = body.get("status").and_then(|s| s.as_str()).unwrap_or("completed");
        let mut out = ImageGenResponse::default();
        let mut texts: Vec<String> = Vec::new();
        let steps = body.get("steps").and_then(|s| s.as_array());
        for step in steps.into_iter().flatten() {
            let content = step.get("content").and_then(|c| c.as_array());
            for block in content.into_iter().flatten() {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("image") => {
                        let data = block.get("data").and_then(|d| d.as_str()).unwrap_or("");
                        let mime = block
                            .get("mime_type")
                            .and_then(|m| m.as_str())
                            .unwrap_or("image/png")
                            .to_string();
                        let bytes = STANDARD
                            .decode(data)
                            .map_err(|e| ImageGenError::NoImage(format!("image block is not valid base64: {e}")))?;
                        if !bytes.is_empty() {
                            out.images.push(GeneratedImage { media_type: mime, data: bytes });
                        }
                    }
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(|t| t.as_str())
                            && !t.trim().is_empty()
                        {
                            texts.push(t.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
        if !texts.is_empty() {
            out.text = Some(texts.join("\n"));
        }
        if out.images.is_empty() {
            let detail = match (status, out.text.as_deref()) {
                ("completed", Some(t)) => format!("status completed, text only: {t}"),
                ("completed", None) => "status completed, no image or text in the response".to_string(),
                (s, Some(t)) => format!("status {s}: {t}"),
                (s, None) => format!("status {s}"),
            };
            return Err(ImageGenError::NoImage(detail));
        }
        Ok(out)
    }
}

#[async_trait::async_trait]
impl ImageProvider for GeminiImageProvider {
    fn kind(&self) -> ImageProviderKind {
        ImageProviderKind::Gemini
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn generate(&self, req: &ImageGenRequest) -> Result<ImageGenResponse, ImageGenError> {
        let body = Self::request_body(&self.model, req);
        let resp = self
            .http
            .post(&self.endpoint)
            .header("x-goog-api-key", &self.api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ImageGenError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| ImageGenError::Transport(e.to_string()))?;
        if !(200..300).contains(&status) {
            // Never echo the key; the body is Google's error envelope.
            let detail = serde_json::from_str::<JsonValue>(&text)
                .ok()
                .and_then(|v| v.get("error")?.get("message")?.as_str().map(str::to_string))
                .unwrap_or_else(|| text.chars().take(600).collect());
            return Err(ImageGenError::Api { status, body: detail });
        }
        let value: JsonValue = serde_json::from_str(&text)
            .map_err(|e| ImageGenError::Transport(format!("response is not JSON: {e}")))?;
        Self::parse_response(&value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::image::OutputFormat;

    fn req() -> ImageGenRequest {
        ImageGenRequest {
            prompt: "a 16:9 poster of a lighthouse".into(),
            aspect_ratio: Some("16:9".into()),
            size: Some("2K".into()),
            output_format: OutputFormat::Png,
            reference_images: vec![("image/jpeg".into(), vec![0xFF, 0xD8, 0xFF, 0x00])],
        }
    }

    #[test]
    fn gemini_request_body_shape_matches_docs() {
        let body = GeminiImageProvider::request_body("gemini-3-pro-image", &req());
        assert_eq!(body["model"], "gemini-3-pro-image");
        assert_eq!(body["input"][0]["type"], "text");
        assert_eq!(body["input"][0]["text"], "a 16:9 poster of a lighthouse");
        assert_eq!(body["input"][1]["type"], "image");
        assert_eq!(body["input"][1]["mime_type"], "image/jpeg");
        assert_eq!(body["input"][1]["data"], "/9j/AA==");
        assert_eq!(body["response_format"]["type"], "image");
        assert_eq!(body["response_format"]["mime_type"], "image/png");
        assert_eq!(body["response_format"]["aspect_ratio"], "16:9");
        assert_eq!(body["response_format"]["image_size"], "2K");
        // snake_case everywhere; no camelCase leaks.
        let s = body.to_string();
        assert!(!s.contains("mimeType") && !s.contains("responseFormat") && !s.contains("imageSize"));
        // Optional fields are omitted, not nulled.
        let mut minimal = req();
        minimal.aspect_ratio = None;
        minimal.size = None;
        minimal.reference_images.clear();
        let body = GeminiImageProvider::request_body("gemini-3.1-flash-image", &minimal);
        assert!(body["response_format"].get("aspect_ratio").is_none());
        assert!(body["response_format"].get("image_size").is_none());
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn gemini_parse_response_reads_steps_content_image() {
        let body = json!({
            "id": "int_1", "model": "gemini-3-pro-image", "status": "completed", "object": "interaction",
            "steps": [
                {"type": "user_input", "content": [{"type": "text", "text": "a poster"}]},
                {"type": "model_output", "content": [
                    {"type": "text", "text": "Here is your poster."},
                    {"type": "image", "mime_type": "image/png", "data": "iVBORw0KGgo="}
                ]}
            ],
            "usage": {"total_tokens": 10}
        });
        let out = GeminiImageProvider::parse_response(&body).unwrap();
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].media_type, "image/png");
        assert_eq!(out.images[0].data, STANDARD.decode("iVBORw0KGgo=").unwrap());
        // Prompt echo in the user_input step is NOT mistaken for output text
        // when it is a text block... (it is concatenated; callers show it
        // as-is) — but the image is what matters.
        assert!(out.text.as_deref().unwrap().contains("Here is your poster."));
    }

    #[test]
    fn gemini_parse_response_errors_are_instructive() {
        let err = GeminiImageProvider::parse_response(&json!({"error": {"code": "x", "message": "quota exceeded"}})).unwrap_err();
        assert!(matches!(err, ImageGenError::Api { body, .. } if body == "quota exceeded"));
        let err = GeminiImageProvider::parse_response(&json!({
            "status": "completed",
            "steps": [{"type": "model_output", "content": [{"type": "text", "text": "I can't draw that."}]}]
        }))
        .unwrap_err();
        assert!(matches!(err, ImageGenError::NoImage(d) if d.contains("I can't draw that.")));
        let err = GeminiImageProvider::parse_response(&json!({"status": "failed", "steps": []})).unwrap_err();
        assert!(matches!(err, ImageGenError::NoImage(d) if d == "status failed"));
        let err = GeminiImageProvider::parse_response(&json!({
            "status": "completed",
            "steps": [{"type": "model_output", "content": [{"type": "image", "mime_type": "image/png", "data": "%%%"}]}]
        }))
        .unwrap_err();
        assert!(matches!(err, ImageGenError::NoImage(d) if d.contains("base64")));
    }

    #[tokio::test]
    async fn gemini_generate_round_trips_through_wiremock() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1beta/interactions"))
            .and(header("x-goog-api-key", "k-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "int_2", "status": "completed",
                "steps": [{"type": "model_output", "content": [
                    {"type": "image", "mime_type": "image/jpeg", "data": STANDARD.encode([0xFF, 0xD8, 0xFF, 0xE0])}
                ]}]
            })))
            .mount(&server)
            .await;
        let p = GeminiImageProvider::with_endpoint(
            "k-test".into(),
            "gemini-3-pro-image".into(),
            format!("{}/v1beta/interactions", server.uri()),
        );
        let out = p.generate(&req()).await.unwrap();
        assert_eq!(out.images[0].media_type, "image/jpeg");
        assert_eq!(out.images[0].data, vec![0xFF, 0xD8, 0xFF, 0xE0]);

        // API error envelope → Api error with the message, never the key.
        let server2 = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_json(json!({"error": {"code": "rate", "message": "slow down"}})))
            .mount(&server2)
            .await;
        let p2 = GeminiImageProvider::with_endpoint("k-secret".into(), "gemini-3-pro-image".into(), format!("{}/v1beta/interactions", server2.uri()));
        let err = p2.generate(&req()).await.unwrap_err();
        match err {
            ImageGenError::Api { status, body } => {
                assert_eq!(status, 429);
                assert_eq!(body, "slow down");
                assert!(!body.contains("k-secret"));
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }
}
