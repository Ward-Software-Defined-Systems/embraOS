//! Gemini image generation over the Interactions API
//! (`POST https://generativelanguage.googleapis.com/v1beta/interactions`,
//! header `x-goog-api-key`). The shapes, by the REST reference: request
//! `{model, input:[{type:"text",text}|{type:"image", mime_type,data}],
//! response_format:{type:"image", mime_type, aspect_ratio?,
//! image_size?}}` (snake_case); response `{id, model, status,
//! steps:[{type:"model_output", content:[{type:"text",text}|
//! {type:"image", data, mime_type}]}], usage}`; `status` ∈ completed |
//! failed | incomplete | …; errors `{error:{code, message}}`.
//!
//! The values, by the same reference's `ImageResponseFormat`: `mime_type`
//! has one, `image/jpeg` (`OUTPUT_MIME`); `image_size` has `512`, `1K`,
//! `2K` and `4K` (`IMAGE_SIZES`), and which of them a model renders is
//! `sizes_of`'s. A value is read from the reference's list of values,
//! never from the guide's prose or the name of a file in its samples:
//! the prose says "512px" and a sample saves `generated_image.png`.
//!
//! The request/response codecs are pure functions (wiremock-tested); the
//! transport is one POST with explicit timeouts — the tool layer adds the
//! `IMAGE_GEN_TIMEOUT` ceiling and the dispatcher its 600 s backstop.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value as JsonValue};

use super::{
    GeneratedImage, ImageGenError, ImageGenRequest, ImageGenResponse, ImageProvider, ImageProviderKind, OUTPUT_MIME,
};

pub struct GeminiImageProvider {
    api_key: String,
    model: String,
    endpoint: String,
    http: reqwest::Client,
}

impl GeminiImageProvider {
    /// `endpoint` is `image::interactions_url`'s answer: the provider's own
    /// host, or the provider gateway's base while it is on.
    pub fn with_endpoint(api_key: String, model: String, endpoint: String) -> Self {
        let http = crate::net::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(super::IMAGE_GEN_TIMEOUT)
            .build()
            .expect("a reqwest client with two timeouts and a redirect policy builds");
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
        response_format.insert("mime_type".into(), json!(OUTPUT_MIME));
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
                            .unwrap_or(OUTPUT_MIME)
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

    fn req() -> ImageGenRequest {
        ImageGenRequest {
            prompt: "a 16:9 poster of a lighthouse".into(),
            aspect_ratio: Some("16:9".into()),
            size: Some("2K".into()),
            reference_images: vec![("image/jpeg".into(), vec![0xFF, 0xD8, 0xFF, 0x00])],
        }
    }

    #[test]
    fn gemini_request_body_shape_matches_docs() {
        let body = GeminiImageProvider::request_body("gemini-nano-banana-2.1", &req());
        assert_eq!(body["model"], "gemini-nano-banana-2.1");
        assert_eq!(body["input"][0]["type"], "text");
        assert_eq!(body["input"][0]["text"], "a 16:9 poster of a lighthouse");
        assert_eq!(body["input"][1]["type"], "image");
        assert_eq!(body["input"][1]["mime_type"], "image/jpeg");
        assert_eq!(body["input"][1]["data"], "/9j/AA==");
        assert_eq!(body["response_format"]["type"], "image");
        assert_eq!(body["response_format"]["mime_type"], "image/jpeg");
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
        // The call with no option at all: the type and JPEG, nothing more.
        assert_eq!(body["response_format"], json!({"type": "image", "mime_type": "image/jpeg"}));
    }

    /// Whatever the tool lets through reaches the API as a value of its
    /// reference: every model with every size and ratio the validator
    /// accepts, against the reference's lists typed here from it
    /// (`ImageResponseFormat`: one `mime_type`, four `image_size`s, and
    /// the ratios, of which the tool offers the ten every model renders).
    #[test]
    fn every_request_the_validator_lets_through_carries_values_of_the_reference() {
        use crate::provider::image::{validate_request, ASPECT_RATIOS, GEMINI_IMAGE_MODELS, IMAGE_SIZES};
        const MIME_TYPES: [&str; 1] = ["image/jpeg"];
        const SIZES: [&str; 4] = ["512", "1K", "2K", "4K"];
        const RATIOS: [&str; 14] = [
            "1:1", "2:3", "3:2", "3:4", "4:3", "4:5", "5:4", "9:16", "16:9", "21:9", "1:8", "8:1", "1:4", "4:1",
        ];
        let mut sent = 0;
        for model in GEMINI_IMAGE_MODELS {
            for size in IMAGE_SIZES.iter().map(|s| Some(*s)).chain([None]) {
                for ratio in ASPECT_RATIOS.iter().map(|r| Some(*r)).chain([None]) {
                    let request = ImageGenRequest {
                        prompt: "a poster".into(),
                        aspect_ratio: ratio.map(str::to_string),
                        size: size.map(str::to_string),
                        reference_images: Vec::new(),
                    };
                    if validate_request(model, &request).is_err() {
                        continue;
                    }
                    let body = GeminiImageProvider::request_body(model, &request);
                    let format = body["response_format"].as_object().expect("an object");
                    assert_eq!(format["type"], "image");
                    assert!(MIME_TYPES.contains(&format["mime_type"].as_str().unwrap()), "{model}: {format:?}");
                    match format.get("image_size") {
                        Some(v) => assert!(SIZES.contains(&v.as_str().unwrap()), "{model}: {format:?}"),
                        None => assert!(size.is_none()),
                    }
                    match format.get("aspect_ratio") {
                        Some(v) => assert!(RATIOS.contains(&v.as_str().unwrap()), "{model}: {format:?}"),
                        None => assert!(ratio.is_none()),
                    }
                    for key in format.keys() {
                        assert!(["type", "mime_type", "aspect_ratio", "image_size"].contains(&key.as_str()), "{key}");
                    }
                    sent += 1;
                }
            }
        }
        // 5 models; sizes 3+3+4+1+1 and none each: 17 rows, by 11 ratios.
        assert_eq!(sent, 17 * 11);
    }

    #[test]
    fn gemini_parse_response_reads_steps_content_image() {
        let body = json!({
            "id": "int_1", "model": "gemini-3-pro-image", "status": "completed", "object": "interaction",
            "steps": [
                {"type": "user_input", "content": [{"type": "text", "text": "a poster"}]},
                {"type": "model_output", "content": [
                    {"type": "text", "text": "Here is your poster."},
                    {"type": "image", "mime_type": "image/jpeg", "data": "/9j/4AAQ"}
                ]}
            ],
            "usage": {"total_tokens": 10}
        });
        let out = GeminiImageProvider::parse_response(&body).unwrap();
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].media_type, "image/jpeg");
        assert_eq!(out.images[0].data, STANDARD.decode("/9j/4AAQ").unwrap());
        // A block that declares no type is taken as the endpoint's one
        // output type; the tool sniffs the bytes either way.
        let undeclared = json!({
            "status": "completed",
            "steps": [{"type": "model_output", "content": [{"type": "image", "data": "/9j/4AAQ"}]}]
        });
        let taken = GeminiImageProvider::parse_response(&undeclared).unwrap();
        assert_eq!(taken.images[0].media_type, "image/jpeg");
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
            "steps": [{"type": "model_output", "content": [{"type": "image", "mime_type": "image/jpeg", "data": "%%%"}]}]
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
