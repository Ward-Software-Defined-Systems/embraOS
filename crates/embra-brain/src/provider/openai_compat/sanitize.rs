//! Always-on harmony token sanitization for OpenAI-compat tool-call names.
//!
//! gpt-oss models occasionally leak harmony channel markers into the
//! `tool_calls[].function.name` field. The reported pattern is
//! `"name": "assistant<|channel|>analysis"` instead of the expected real
//! tool name. Upstream Ollama fixed the specific case in PR #11759
//! (issue #11704), but older versions remain in production and other
//! harmony-format models may exhibit the same bug class.
//!
//! Strategy per Locked Decision #11: always-on sanitization at the
//! wire→IR boundary with `tracing::warn!` telemetry on every match.
//! Sanitization is conservative: only strip a trailing
//! `<|channel|>X` marker where X is `[a-z]+`. Any other shape passes
//! through untouched.
//!
//! False-positive analysis: `<|` is not a valid identifier character
//! in any tool-naming convention; no plausible legitimate tool name
//! contains the substring `<|channel|>`.

use std::borrow::Cow;

const MARKER: &str = "<|channel|>";

/// Sanitize a tool-call name. If the name ends with `<|channel|>X`
/// where X is lowercase ASCII letters, strip the marker and emit a
/// `warn`-level event with `model_id`, `original`, `sanitized`.
/// Otherwise return the input borrowed.
pub fn sanitize_harmony_tokens<'a>(name: &'a str, model_id: &str) -> Cow<'a, str> {
    let Some(idx) = name.rfind(MARKER) else {
        return Cow::Borrowed(name);
    };
    let after = &name[idx + MARKER.len()..];
    if after.is_empty() || !after.bytes().all(|b| b.is_ascii_lowercase()) {
        return Cow::Borrowed(name);
    }
    let sanitized = &name[..idx];
    tracing::warn!(
        target: "provider::openai_compat::sanitize",
        model_id = %model_id,
        original = %name,
        sanitized = %sanitized,
        "harmony channel marker leaked into tool-call name; stripped"
    );
    Cow::Owned(sanitized.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_known_leak_pattern() {
        // Bug as reported in Ollama issue #11704.
        let name = "assistant<|channel|>analysis";
        let out = sanitize_harmony_tokens(name, "gpt-oss:120b");
        assert_eq!(out, "assistant");
    }

    #[test]
    fn strips_other_channel_names() {
        for channel in ["analysis", "final", "commentary"] {
            let name = format!("exec<|channel|>{channel}");
            let out = sanitize_harmony_tokens(&name, "gpt-oss:20b");
            assert_eq!(out, "exec", "should strip <|channel|>{channel}");
        }
    }

    #[test]
    fn legitimate_names_pass_through_borrowed() {
        let name = "git_commit";
        let out = sanitize_harmony_tokens(name, "gpt-oss:20b");
        assert_eq!(out, "git_commit");
        // Pass-through MUST be borrowed (zero-copy).
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    #[test]
    fn empty_after_marker_is_not_a_match() {
        // Trailing "<|channel|>" with nothing after — not a documented
        // bug pattern; conservatively pass through.
        let name = "tool_name<|channel|>";
        let out = sanitize_harmony_tokens(name, "any-model");
        assert_eq!(out, "tool_name<|channel|>");
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    #[test]
    fn marker_with_uppercase_after_passes_through() {
        // Pattern with mixed-case after marker — not a known bug shape.
        let name = "tool<|channel|>Analysis";
        let out = sanitize_harmony_tokens(name, "any-model");
        assert_eq!(out, "tool<|channel|>Analysis");
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    #[test]
    fn marker_in_middle_with_trailing_garbage_passes_through() {
        // Marker followed by digits or other chars is not the known
        // leak shape; pass through.
        let name = "tool<|channel|>analysis_x";
        let out = sanitize_harmony_tokens(name, "any-model");
        assert_eq!(out, "tool<|channel|>analysis_x");
    }

    #[test]
    fn pipe_or_angle_alone_is_not_a_match() {
        // Plain pipe character — not a marker.
        let name = "weird|name";
        let out = sanitize_harmony_tokens(name, "any-model");
        assert_eq!(out, "weird|name");
        assert!(matches!(out, Cow::Borrowed(_)));
    }
}
