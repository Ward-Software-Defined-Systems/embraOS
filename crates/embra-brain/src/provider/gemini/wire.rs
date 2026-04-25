//! Gemini-specific wire types.
//!
//! These shapes mirror the `generativelanguage.googleapis.com/v1beta`
//! `:streamGenerateContent` request and SSE response. Private to the
//! Gemini provider — the loop driver works exclusively with neutral IR
//! at `crate::provider::ir`.
//!
//! Field names use camelCase via `#[serde(rename_all = "camelCase")]`
//! to match the Gemini API on the wire while keeping idiomatic
//! snake_case in Rust.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

// ── Request shapes ──

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GeminiContent {
    /// `"user"` for inputs and tool results, `"model"` for assistant
    /// turns. Gemini rejects other values.
    pub role: String,
    pub parts: Vec<GeminiPart>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<GeminiFunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_response: Option<GeminiFunctionResponse>,
    /// Opaque base64 payload returned by Gemini 3 models on
    /// `functionCall` parts (and occasionally on text/thought parts).
    /// Must round-trip verbatim or the API 400s with
    /// `Function call ... is missing a thought_signature`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
    /// `true` when this part is the model's chain-of-thought summary
    /// (filtered from user-visible text).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiFunctionCall {
    /// Provider-assigned id (Gemini 3+). Echoed back on
    /// `functionResponse.id` to correlate.
    pub id: String,
    pub name: String,
    pub args: JsonValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiFunctionResponse {
    /// Must match the originating `functionCall.id`.
    pub id: String,
    pub name: String,
    pub response: JsonValue,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiGenerateRequest<'a> {
    pub contents: &'a [GeminiContent],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<GeminiSystemInstruction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<&'a JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_config: Option<GeminiToolConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GeminiGenerationConfig>,
    /// `cachedContents/<id>` reference. When set, the cache's
    /// `systemInstruction` and `tools` are prepended server-side; do
    /// NOT also pass them in this request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_content: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GeminiSystemInstruction {
    pub parts: Vec<GeminiSystemPart>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GeminiSystemPart {
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiToolConfig {
    pub function_calling_config: GeminiFunctionCallingConfig,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiFunctionCallingConfig {
    /// `"AUTO"` (default), `"ANY"`, `"VALIDATED"`, or `"NONE"`.
    /// Uppercase per the docs.
    pub mode: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiGenerationConfig {
    pub max_output_tokens: u32,
    pub thinking_config: GeminiThinkingConfig,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiThinkingConfig {
    /// `"minimal"`, `"low"`, `"medium"`, or `"high"` (default for
    /// Gemini 3.1 Pro, also the only value embraOS uses).
    pub thinking_level: String,
}

// ── Response shapes (streaming chunks) ──

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GeminiStreamChunk {
    #[serde(default)]
    pub candidates: Vec<GeminiCandidate>,
    #[serde(default)]
    pub usage_metadata: Option<JsonValue>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiCandidate {
    #[serde(default)]
    pub content: GeminiContent,
    /// `STOP`, `MAX_TOKENS`, `SAFETY`, `RECITATION`,
    /// `MALFORMED_FUNCTION_CALL`, or `OTHER`. May be absent on
    /// intermediate chunks. (Per the docs, Gemini 3.1 Pro does NOT
    /// emit a dedicated `TOOL_USE` finish reason — a `STOP` with
    /// `functionCall` parts present is the continuation signal.)
    #[serde(default)]
    pub finish_reason: Option<String>,
    #[serde(default)]
    pub index: u32,
}
