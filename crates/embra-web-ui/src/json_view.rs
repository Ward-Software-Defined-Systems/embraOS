//! JSON as the browser shows it: pretty-printed under a cap, and tokenized
//! into the TUI's colours (keys cyan, strings green, numbers and the three
//! literals amber). Shared by the mobile chat's tool cards and the desktop
//! sidebar's tool sheet; the helpers moved here from `chat.rs` when the
//! sheet arrived.

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

/// Match `kw` at `i` in `chars` on word boundaries (so `null` inside
/// `nullable` doesn't match).
pub(crate) fn kw_at(chars: &[char], i: usize, kw: &str) -> bool {
    let k: Vec<char> = kw.chars().collect();
    if i + k.len() > chars.len() || chars[i..i + k.len()] != k[..] {
        return false;
    }
    let before_ok = i == 0 || !chars[i - 1].is_alphanumeric();
    let after = i + k.len();
    let after_ok = after >= chars.len() || !chars[after].is_alphanumeric();
    before_ok && after_ok
}

/// Tokenize a JSON-ish string into `(text, css-class)` segments, mirroring
/// the TUI's scheme (`embra-console` `render.rs::parse_json_line`): keys →
/// `jk` (cyan), string values → `js` (green), numbers + true/false/null →
/// `jn`/`jb` (amber), everything else → `""` (default). Newlines are kept
/// verbatim for the surrounding `<pre>`.
pub(crate) fn highlight_json_segments(s: &str) -> Vec<(String, &'static str)> {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut segs: Vec<(String, &'static str)> = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < len {
        let ch = chars[i];
        if ch == '"' {
            if !cur.is_empty() {
                segs.push((std::mem::take(&mut cur), ""));
            }
            let mut lit = String::from('"');
            i += 1;
            while i < len && chars[i] != '"' {
                if chars[i] == '\\' && i + 1 < len {
                    lit.push(chars[i]);
                    i += 1;
                }
                lit.push(chars[i]);
                i += 1;
            }
            if i < len {
                lit.push('"');
                i += 1;
            }
            // A string is a key when the next non-space char is ':'.
            let mut j = i;
            while j < len && chars[j] == ' ' {
                j += 1;
            }
            let cls = if j < len && chars[j] == ':' { "jk" } else { "js" };
            segs.push((lit, cls));
        } else if (ch.is_ascii_digit() || ch == '-')
            && (cur.trim().is_empty()
                || cur.ends_with(": ")
                || cur.ends_with(',')
                || cur.ends_with('['))
        {
            if !cur.is_empty() {
                segs.push((std::mem::take(&mut cur), ""));
            }
            let mut num = String::from(ch);
            i += 1;
            while i < len
                && (chars[i].is_ascii_digit() || matches!(chars[i], '.' | 'e' | 'E' | '+' | '-'))
            {
                num.push(chars[i]);
                i += 1;
            }
            segs.push((num, "jn"));
        } else if kw_at(&chars, i, "true") || kw_at(&chars, i, "false") || kw_at(&chars, i, "null")
        {
            if !cur.is_empty() {
                segs.push((std::mem::take(&mut cur), ""));
            }
            let kw = if kw_at(&chars, i, "false") {
                "false"
            } else if kw_at(&chars, i, "true") {
                "true"
            } else {
                "null"
            };
            segs.push((kw.to_string(), "jb"));
            i += kw.len();
        } else {
            cur.push(ch);
            i += 1;
        }
    }
    if !cur.is_empty() {
        segs.push((cur, ""));
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
