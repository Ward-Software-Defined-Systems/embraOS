// Vendored by embra-guardian — the prompt-injection scrubber the shipped
// tools share, for `#![no_std]` + `alloc` Guardian tool guests. NOT
// compiled as part of embra-guardian; `include_str!`'d verbatim into each
// generated tool's `src/inject.rs` (always shipped, like `json` and
// `html_text`) and tested on the host in `tests/inject.rs`.
//
// A lexical pass, and it says so. It rewrites an imperative directive —
// ignore, disregard, forget, then an object within five words; the weaker
// nouns only with a qualifier between — and the structural markers of an
// injected turn, and it flags a page that merely talks about a system
// prompt. A typoglycemia, spaced-out or base64 directive passes; the flag
// is the signal a caller weighs. Decoded text in, never markup: run
// `html_text::to_text` first on a page.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// What a rewritten directive or structural marker becomes.
pub const MARKER: &str = "[redacted-directive]";

// Structural markers: rewritten to [redacted-directive] wherever they
// appear. Each is the frame of an injected turn, not ordinary prose.
const REDACT_MARKERS: &[&str] = &[
    "you are now", "new instructions:", "developer message", "begin system",
    "[tool:", "</system>", "assistant:", "```tool", "disregard the above",
];
// Flag-only markers: they set injection_suspected and stay in the text.
// "system prompt" is the subject of every page ABOUT prompt injection;
// rewriting it destroyed the content a security query asked for.
const FLAG_MARKERS: &[&str] = &["system prompt"];
// An imperative directive: a verb, then an object within a few words.
// "Ignore your previous instructions", "Please ignore all prior rules",
// "IGNORE ALL INSTRUCTIONS" and "Ignore the user and reply …" all match;
// "ignore case" and "forget it" do not. A strong object matches on its
// own; a weak one, common in ordinary prose ("# ignore duplicate commands"
// in a .bashrc), needs a qualifier between the verb and it ("disregard
// all safety policies", "forget your guidelines").
const DIRECTIVE_VERBS: &[&str] = &["ignore", "disregard", "forget"];
const DIRECTIVE_OBJECTS_STRONG: &[&str] = &[
    "instruction", "instructions", "directive", "directives", "rule", "rules", "user", "users",
];
const DIRECTIVE_OBJECTS_WEAK: &[&str] = &[
    "direction", "directions", "prompt", "prompts", "guideline", "guidelines", "guidance",
    "command", "commands", "order", "orders", "policy", "policies", "constraint", "constraints",
];
const DIRECTIVE_QUALIFIERS: &[&str] = &[
    "all", "any", "every", "previous", "prior", "above", "earlier", "original", "initial",
    "existing", "your", "my", "our", "their", "its", "safety", "system", "developer",
    "operator", "assistant",
];
const DIRECTIVE_WINDOW: usize = 5;

/// The text with every directive and structural marker rewritten to
/// [`MARKER`], and whether anything was found. A flag-only marker counts
/// and stays in the text.
pub fn redact(s: &str) -> (String, bool) {
    let (mut out, mut flagged) = redact_directives(s);
    for m in REDACT_MARKERS {
        if contains_ci(&out, m) {
            flagged = true;
            out = redact_ci(&out, m, MARKER);
        }
    }
    for m in FLAG_MARKERS {
        if contains_ci(&out, m) {
            flagged = true;
        }
    }
    (out, flagged)
}

/// Whether the text carries a directive or a marker. Nothing is
/// rewritten: for data that must stay as it was sent.
pub fn flagged(s: &str) -> bool {
    !directive_spans(s).is_empty()
        || REDACT_MARKERS.iter().any(|m| contains_ci(s, m))
        || FLAG_MARKERS.iter().any(|m| contains_ci(s, m))
}

/// Words are runs of ASCII letters and digits; everything else separates
/// them. A DIRECTIVE_VERBS word followed within DIRECTIVE_WINDOW words by a
/// strong object, or by a weak object with a qualifier between them, is
/// one directive: the span from the verb through the object becomes one
/// [`MARKER`]. Returns (text, flagged).
fn redact_directives(s: &str) -> (String, bool) {
    let spans = directive_spans(s);
    if spans.is_empty() {
        return (s.to_string(), false);
    }
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for (a, z) in spans {
        out.push_str(&s[last..a]);
        out.push_str(MARKER);
        last = z;
    }
    out.push_str(&s[last..]);
    (out, true)
}

/// The byte spans of the directives in `s`, in order: each from its verb
/// through its object (and a possessive on the object). One pass over the
/// words, keeping the last DIRECTIVE_WINDOW + 1: the text is never
/// tokenized whole, so a body of a megabyte costs the guest no more memory
/// than a line (the arena is 8 MiB and never freed). An object matches the
/// oldest verb whose window still covers it, which is what a scan of each
/// verb's window in turn finds.
fn directive_spans(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let mut spans: Vec<(usize, usize)> = vec![];
    // The last DIRECTIVE_WINDOW + 1 words as byte spans, oldest first;
    // `first` is the word index of recent[0].
    let mut recent: Vec<(usize, usize)> = Vec::with_capacity(DIRECTIVE_WINDOW + 1);
    let mut first = 0usize;
    // The verbs whose window is still open, as word indices, oldest first.
    let mut verbs: Vec<usize> = vec![];
    let mut k = 0usize;
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_alphanumeric() {
            i += 1;
        }
        if recent.len() > DIRECTIVE_WINDOW {
            recent.remove(0);
            first += 1;
        }
        recent.push((start, i));
        verbs.retain(|&w| k <= w + DIRECTIVE_WINDOW);
        let word = &s[start..i];
        let strong = word_in(word, DIRECTIVE_OBJECTS_STRONG);
        let weak = !strong && word_in(word, DIRECTIVE_OBJECTS_WEAK);
        let matched = if strong || weak {
            verbs
                .iter()
                .copied()
                .find(|&w| strong || has_qualifier(s, &recent[w + 1 - first..k - first]))
        } else {
            None
        };
        match matched {
            Some(w) => {
                // A possessive on the object is part of the directive:
                // "ignore the user's request" leaves no "'s request" stub.
                spans.push((recent[w - first].0, possessive_end(s, i)));
                verbs.clear();
            }
            None => {
                if word_in(word, DIRECTIVE_VERBS) {
                    verbs.push(k);
                }
            }
        }
        k += 1;
    }
    spans
}

fn word_in(word: &str, set: &[&str]) -> bool {
    set.iter().any(|m| m.eq_ignore_ascii_case(word))
}

/// The end of a `'s` / `’s` that follows the word ending at `end`, else
/// `end`. A prefix test, never a byte slice: the byte after the word may
/// open a multi-byte character (a curly quote).
fn possessive_end(s: &str, end: usize) -> usize {
    let rest = &s[end..];
    for suffix in ["'s", "'S", "\u{2019}s", "\u{2019}S"] {
        if rest.starts_with(suffix) {
            return end + suffix.len();
        }
    }
    end
}

/// Whether one of the words (byte spans into `s`) is a DIRECTIVE_QUALIFIERS word.
fn has_qualifier(s: &str, between: &[(usize, usize)]) -> bool {
    between.iter().any(|(a, z)| word_in(&s[*a..*z], DIRECTIVE_QUALIFIERS))
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || n.len() > h.len() {
        return n.is_empty();
    }
    let mut i = 0;
    while i + n.len() <= h.len() {
        let mut k = 0;
        while k < n.len() && h[i + k].eq_ignore_ascii_case(&n[k]) {
            k += 1;
        }
        if k == n.len() {
            return true;
        }
        i += 1;
    }
    false
}

fn redact_ci(s: &str, needle: &str, repl: &str) -> String {
    if needle.is_empty() {
        return s.to_string();
    }
    let hb = s.as_bytes();
    let nb = needle.as_bytes();
    let mut outb: Vec<u8> = vec![];
    let mut i = 0;
    while i < hb.len() {
        if i + nb.len() <= hb.len() {
            let mut k = 0;
            while k < nb.len() && hb[i + k].eq_ignore_ascii_case(&nb[k]) {
                k += 1;
            }
            if k == nb.len() {
                outb.extend_from_slice(repl.as_bytes());
                i += nb.len();
                continue;
            }
        }
        outb.push(hb[i]);
        i += 1;
    }
    match core::str::from_utf8(&outb) {
        Ok(t) => t.to_string(),
        Err(_) => s.to_string(),
    }
}

