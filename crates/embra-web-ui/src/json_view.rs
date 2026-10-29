//! JSON as the browser shows it: pretty-printed under a cap, and tokenized
//! by the tokenizer the TUI shares into its colours (keys cyan, strings
//! green, numbers and the three literals amber). Shared by the mobile chat's tool cards and the desktop
//! sidebar's tool sheet; the helpers moved here from `chat.rs` when the
//! sheet arrived.

use embra_tools_core::{JsonTokenKind, tokenize_line};
use leptos::prelude::*;

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Pretty-print a JSON string with a soft character cap. Falls back to
/// the raw input on parse failure (defensive — tool input is always
/// valid JSON in practice). Tools can ship very large inputs (file
/// contents, etc.); the cap keeps the expanded card from blowing past
/// what a phone can scroll through comfortably.
pub(crate) fn pretty_json_capped(s: &str, max: usize) -> String {
    let pretty = serde_json::from_str::<serde_json::Value>(s)
        .and_then(|v| serde_json::to_string_pretty(&v))
        .unwrap_or_else(|_| s.to_string());
    if pretty.chars().count() <= max {
        return pretty;
    }
    let mut out: String = pretty.chars().take(max).collect();
    out.push_str("\n…(truncated)");
    out
}

/// The class a token kind renders with: keys `jk` (cyan), strings `js`
/// (green), numbers `jn` and the three literals `jb` (amber), the
/// structure and anything else the default. Every kind is named here.
pub(crate) fn json_class(kind: JsonTokenKind) -> &'static str {
    match kind {
        JsonTokenKind::Key => "jk",
        JsonTokenKind::Str => "js",
        JsonTokenKind::Number => "jn",
        JsonTokenKind::Bool | JsonTokenKind::Null => "jb",
        JsonTokenKind::Punct | JsonTokenKind::Text => "",
    }
}

/// Tokenize a JSON-ish string into `(text, css-class)` segments, line by
/// line through the tokenizer the console shares
/// (`embra_tools_core::tokenize_line`, #40). Newlines are kept verbatim
/// for the surrounding `<pre>`.
pub(crate) fn highlight_json_segments(s: &str) -> Vec<(String, &'static str)> {
    let mut segs: Vec<(String, &'static str)> = Vec::new();
    for (i, line) in s.split('\n').enumerate() {
        if i > 0 {
            segs.push(("\n".to_string(), ""));
        }
        for t in tokenize_line(line) {
            segs.push((t.text(line).to_string(), json_class(t.kind)));
        }
    }
    segs
}

/// Render tool input/result into the card `<pre>` with JSON syntax colors.
/// Only JSON-shaped content under a size cap is tokenized; large or plain
/// text renders unstyled (one node) to keep the DOM light.
pub(crate) fn json_pre(content: &str) -> AnyView {
    let trimmed = content.trim_start();
    let looks_json = trimmed.starts_with('{') || trimmed.starts_with('[');
    if looks_json && content.len() <= 8192 {
        highlight_json_segments(content)
            .into_iter()
            .map(|(text, cls)| view! { <span class=cls>{text}</span> }.into_any())
            .collect_view()
            .into_any()
    } else {
        view! { <span>{content.to_string()}</span> }.into_any()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every token kind of the shared tokenizer has its class here, and a
    /// multi-line string keeps its newlines between the lines' tokens.
    #[test]
    fn the_two_renderers_cover_every_token_kind() {
        for (kind, class) in [
            (JsonTokenKind::Key, "jk"),
            (JsonTokenKind::Str, "js"),
            (JsonTokenKind::Number, "jn"),
            (JsonTokenKind::Bool, "jb"),
            (JsonTokenKind::Null, "jb"),
            (JsonTokenKind::Punct, ""),
            (JsonTokenKind::Text, ""),
        ] {
            assert_eq!(json_class(kind), class, "{kind:?}");
        }
        let segs = highlight_json_segments("{\"a\": [1, true]}\n\"nullable\": null");
        let classes: Vec<(&str, &str)> = segs.iter().map(|(t, c)| (t.as_str(), *c)).collect();
        assert_eq!(
            classes,
            vec![
                ("{", ""), ("\"a\"", "jk"), (": [", ""), ("1", "jn"), (", ", ""), ("true", "jb"), ("]}", ""),
                ("\n", ""),
                ("\"nullable\"", "jk"), (": ", ""), ("null", "jb"),
            ]
        );
    }
}
