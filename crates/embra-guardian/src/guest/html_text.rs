// Vendored by embra-guardian — a zero-dependency HTML→text reducer for
// `#![no_std]` + `alloc` Guardian tool guests. NOT compiled as part of
// embra-guardian; `include_str!`'d verbatim into each generated tool's
// `src/html_text.rs` (always shipped, like `json`). Lets a tool that
// declares `http_get` turn a fetched page into model-readable text
// without a parser crate (crates are banned in v1).
//
// This is a HEURISTIC stripper, NOT an HTML parser: it drops
// <script>/<style> element bodies, removes tags, decodes a small entity
// set, and collapses whitespace. Conservative by design. The result is
// still attacker-controlled — the caller MUST injection-scrub it.

use alloc::string::String;

/// Reduce `html` to readable text. Never panics; worst case returns the
/// input with tags stripped.
pub fn to_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let b = html.as_bytes();
    let lb = lower.as_bytes();
    let n = b.len();
    let mut out = String::with_capacity(n / 2 + 16);
    let mut i = 0usize;
    while i < n {
        if b[i] == b'<' {
            // <script>/<style>: drop the whole element incl. its body.
            if let Some(j) =
                skip_element(lb, i, b"script").or_else(|| skip_element(lb, i, b"style"))
            {
                i = j;
                push_space(&mut out);
                continue;
            }
            // Any other tag: skip to the closing '>' (a soft space).
            i = match memchr(b, b'>', i + 1) {
                Some(k) => k + 1,
                None => n,
            };
            push_space(&mut out);
            continue;
        }
        // Nested `if` (NOT a let-chain) — generated guest crates are
        // edition 2021, where `if … && let …` is unavailable. clippy's
        // collapsible_if suggestion would break the guest build, so it is
        // suppressed here deliberately. Do not collapse.
        #[allow(clippy::collapsible_if)]
        if b[i] == b'&' {
            if let Some(used) = decode_entity(&html[i..], &mut out) {
                i += used;
                continue;
            }
        }
        let cl = utf8_len(b[i]);
        let end = core::cmp::min(i + cl, n);
        if let Some(ch) = html[i..end].chars().next() {
            if ch.is_whitespace() {
                push_space(&mut out);
            } else {
                out.push(ch);
            }
        }
        i = end;
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// If a `<name …>` tag opens at `at` (case-insensitive, name-boundary
/// checked), return the index just past its matching `</name>` (or end
/// of input if unterminated). `lb` is the ASCII-lowercased view.
fn skip_element(lb: &[u8], at: usize, name: &[u8]) -> Option<usize> {
    let after = at + 1;
    if after + name.len() > lb.len() || &lb[after..after + name.len()] != name {
        return None;
    }
    // Next byte must be a tag-name boundary so "<scriptx" is not "<script".
    let boundary = lb.get(after + name.len()).copied();
    match boundary {
        Some(c) if c == b'>' || c == b'/' || c.is_ascii_whitespace() => {}
        _ => return None,
    }
    // Find the end of the open tag, then the closing `</name`.
    let open_end = memchr(lb, b'>', after + name.len())?;
    let mut close_tag = alloc::vec::Vec::with_capacity(name.len() + 2);
    close_tag.push(b'<');
    close_tag.push(b'/');
    close_tag.extend_from_slice(name);
    match find_sub(lb, &close_tag, open_end + 1) {
        Some(p) => Some(memchr(lb, b'>', p).map(|g| g + 1).unwrap_or(lb.len())),
        None => Some(lb.len()),
    }
}

/// Decode one HTML entity at the start of `s` into `out`. Returns the
/// number of input bytes consumed, or `None` if `s` does not start with a
/// recognized entity (the caller then treats `&` as a literal).
fn decode_entity(s: &str, out: &mut String) -> Option<usize> {
    for (name, ch) in [
        ("&amp;", '&'),
        ("&lt;", '<'),
        ("&gt;", '>'),
        ("&quot;", '"'),
        ("&apos;", '\''),
        ("&#39;", '\''),
        ("&nbsp;", ' '),
    ] {
        if s.starts_with(name) {
            push_collapsible(out, ch);
            return Some(name.len());
        }
    }
    // Numeric: &#DDD;  or  &#xHH;
    let body = s.strip_prefix("&#")?;
    let semi = body.find(';')?;
    if semi == 0 || semi > 8 {
        return None;
    }
    let (digits, radix) = match body.strip_prefix(['x', 'X']) {
        Some(hex) => (&hex[..semi - 1], 16),
        None => (&body[..semi], 10),
    };
    let code = u32::from_str_radix(digits, radix).ok()?;
    let ch = char::from_u32(code)?;
    push_collapsible(out, ch);
    Some(2 + semi + 1)
}

fn push_collapsible(out: &mut String, ch: char) {
    if ch.is_whitespace() {
        push_space(out);
    } else {
        out.push(ch);
    }
}

/// Append a single space, collapsing runs and never leading.
fn push_space(out: &mut String) {
    if !out.is_empty() && !out.ends_with(' ') {
        out.push(' ');
    }
}

fn memchr(h: &[u8], needle: u8, from: usize) -> Option<usize> {
    let mut i = from;
    while i < h.len() {
        if h[i] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn find_sub(h: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from + needle.len() > h.len() {
        return None;
    }
    let mut i = from;
    while i + needle.len() <= h.len() {
        if &h[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn utf8_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first < 0xE0 {
        2
    } else if first < 0xF0 {
        3
    } else {
        4
    }
}
