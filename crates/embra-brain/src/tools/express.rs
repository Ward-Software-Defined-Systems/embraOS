use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::Utc;
use serde_json::json;

use crate::db::WardsonDbClient;

const MAX_EXPRESSION_BYTES: usize = 2048;

pub async fn express(db: &WardsonDbClient, param: &str) -> String {
    let decoded_payload = match decode_payload(param) {
        Ok(s) => s,
        Err(e) => return format!("express rejected ({e})"),
    };
    let sanitized = sanitize(&decoded_payload);

    let current = db.read("ui", "expression").await.ok();
    let current_content = current
        .as_ref()
        .and_then(|doc| doc.get("content").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();
    let current_version = current
        .as_ref()
        .and_then(|doc| doc.get("version").and_then(|v| v.as_u64()))
        .unwrap_or(0);

    if sanitized == current_content {
        return "expression unchanged (no-op)".into();
    }

    let new_version = current_version.saturating_add(1);

    let patch = json!({
        "content": sanitized,
        "version": new_version,
        "updated_at": Utc::now().to_rfc3339(),
    });

    match db.patch_document("ui", "expression", &patch).await {
        Ok(_) => {
            if sanitized.is_empty() {
                "expression cleared".into()
            } else {
                format!("expression updated (v{new_version})")
            }
        }
        Err(e) => format!("express failed: {e}"),
    }
}

/// Decode the raw tool parameter. A `base64:` prefix lets Embra bypass the
/// tool-tag parser's whitespace/newline collapse by transporting arbitrary
/// bytes — the decoded payload still goes through `sanitize` afterwards, so
/// ANSI and control-char policy is preserved regardless of transport.
fn decode_payload(param: &str) -> Result<String, String> {
    match param.strip_prefix("base64:") {
        Some(rest) => STANDARD
            .decode(rest.trim())
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .map_err(|e| format!("invalid base64: {e}")),
        None => Ok(param.to_string()),
    }
}

fn sanitize(input: &str) -> String {
    let no_csi = strip_ansi(input);

    let mut out = String::with_capacity(no_csi.len());
    for ch in no_csi.chars() {
        match ch {
            '\n' => out.push(ch),
            c if (c as u32) < 0x20 => continue,
            '\u{7f}' => continue,
            c if (c as u32) >= 0x80 && (c as u32) < 0xA0 => continue,
            c => out.push(c),
        }
    }

    if out.len() > MAX_EXPRESSION_BYTES {
        let mut cut = MAX_EXPRESSION_BYTES;
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    out
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek() {
                Some(&'[') => {
                    chars.next();
                    while let Some(&c) = chars.peek() {
                        chars.next();
                        let cv = c as u32;
                        if (0x40..=0x7e).contains(&cv) {
                            break;
                        }
                    }
                }
                Some(&']') => {
                    chars.next();
                    while let Some(&c) = chars.peek() {
                        chars.next();
                        if c == '\x07' {
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_csi() {
        assert_eq!(sanitize("\x1b[31mred\x1b[0m"), "red");
    }

    #[test]
    fn strips_ansi_osc() {
        assert_eq!(sanitize("\x1b]0;title\x07hello"), "hello");
    }

    #[test]
    fn strips_control_chars_keeps_newline() {
        assert_eq!(sanitize("a\tb\r\nc"), "ab\nc");
    }

    #[test]
    fn truncates_at_boundary() {
        let huge = "a".repeat(MAX_EXPRESSION_BYTES + 50);
        let s = sanitize(&huge);
        assert_eq!(s.len(), MAX_EXPRESSION_BYTES);
    }

    #[test]
    fn utf8_boundary_safe() {
        let prefix = "a".repeat(MAX_EXPRESSION_BYTES - 1);
        let input = format!("{prefix}€");
        let s = sanitize(&input);
        assert!(s.is_char_boundary(s.len()));
        assert!(s.len() <= MAX_EXPRESSION_BYTES);
    }

    #[test]
    fn preserves_unicode() {
        assert_eq!(sanitize("hello ✦ world"), "hello ✦ world");
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(sanitize(""), "");
    }

    #[test]
    fn trailing_esc_dropped() {
        assert_eq!(sanitize("hello\x1b"), "hello");
    }

    #[test]
    fn base64_roundtrip_plain_text() {
        // "hello" base64-encoded
        assert_eq!(decode_payload("base64:aGVsbG8=").unwrap(), "hello");
    }

    #[test]
    fn base64_preserves_newlines_through_transport() {
        // "line1\nline2" — the tag parser would collapse the \n to a space,
        // base64 transport preserves it end-to-end.
        assert_eq!(
            decode_payload("base64:bGluZTEKbGluZTI=").unwrap(),
            "line1\nline2"
        );
    }

    #[test]
    fn base64_invalid_rejected() {
        assert!(decode_payload("base64:not!valid!base64").is_err());
    }

    #[test]
    fn base64_plus_sanitize_still_strips_ansi() {
        // base64 of "\x1b[31mred"
        let decoded = decode_payload("base64:G1szMW1yZWQ=").unwrap();
        assert_eq!(sanitize(&decoded), "red");
    }

    #[test]
    fn non_base64_prefix_is_literal_text() {
        assert_eq!(decode_payload("hello world").unwrap(), "hello world");
    }

    #[test]
    fn base64_empty_payload_decodes_to_empty() {
        assert_eq!(decode_payload("base64:").unwrap(), "");
    }
}
