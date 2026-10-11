// Vendored by embra-guardian — the prompt-injection scrubber the shipped
// tools share, for `#![no_std]` + `alloc` Guardian tool guests. NOT
// compiled as part of embra-guardian; `include_str!`'d verbatim into each
// generated tool's `src/inject.rs` (always shipped, like `json` and
// `html_text`) and tested on the host in `tests/inject.rs`.
//
// A lexical pass, and it says so. It finds an imperative directive —
// ignore, disregard, forget, then an object within five words; the weaker
// nouns only with a qualifier between — the structural markers of an
// injected turn, and a mention of a system prompt. `scrub` rewrites the
// directives and the markers of a text the model will read; `scan`
// rewrites nothing, for data that must stay as sent; `scan_json` is `scan`
// for JSON text, where an escape before a word must not hide it. All three
// name what they found, spelled from the lists below and never copied from
// the text. A
// typoglycemia, spaced-out or base64 directive passes; the names are the
// signal a caller weighs. Decoded text in, never markup: run
// `html_text::to_text` first on a page.

use alloc::string::String;
use alloc::vec::Vec;

/// What a rewritten directive or structural marker becomes.
pub const MARKER: &str = "[redacted-directive]";

// Structural markers: rewritten to [redacted-directive] wherever they
// appear. Each is the frame of an injected turn, not ordinary prose.
const REDACT_MARKERS: &[&str] = &[
    "you are now", "new instructions:", "developer message", "begin system",
    "[tool:", "</system>", "assistant:", "```tool", "disregard the above",
];
// Flag-only markers: named as a mention and kept in the text. "system
// prompt" is the subject of every page ABOUT prompt injection;
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

// How `scan` and `scrub` name what they found: `directive: ignore …
// instructions`, `marker: you are now`, `mentions: system prompt`.
const DIRECTIVE_NAME: &str = "directive: ";
const DIRECTIVE_GAP: &str = " … ";
const MARKER_NAME: &str = "marker: ";
const MENTION_NAME: &str = "mentions: ";

/// What the text carries, named from this module's lists and never copied
/// from the text: each directive as `directive: <verb> … <object>`, in the
/// order found, then each structural marker as `marker: <marker>` and each
/// flag-only phrase as `mentions: <phrase>`, in the lists' order. Each name
/// once; empty when nothing was found. Nothing is rewritten: for data that
/// must stay as it was sent.
pub fn scan(s: &str) -> Vec<String> {
    names(s, &pairs_of(s, false))
}

/// [`scan`] for JSON text as it was sent: an escape is read whole, so the
/// letter of `\n` never starts the next word ("\n\nIgnore all previous
/// instructions" is a directive). Decoded text keeps [`scan`], where a
/// backslash is a character.
pub fn scan_json(s: &str) -> Vec<String> {
    names(s, &pairs_of(s, true))
}

/// The text with every directive and structural marker rewritten to
/// [`MARKER`], and what it carried, named as [`scan`] names it. A
/// flag-only phrase is named and stays in the text. The output is sized
/// before it is written and holds one copy of the text, whatever was
/// found: the arena never frees, and a string grown by doubling leaves
/// twice its length behind.
pub fn scrub(s: &str) -> (String, Vec<String>) {
    let markers: Vec<&'static str> =
        REDACT_MARKERS.iter().copied().filter(|m| contains_ci(s, m)).collect();
    let mut len = 0;
    rewrite(s, &markers, |piece| len += piece.len());
    let mut out = String::with_capacity(len);
    rewrite(s, &markers, |piece| out.push_str(piece));
    (out, scan(s))
}

/// [`scrub`], with whether anything was found in place of the names.
pub fn redact(s: &str) -> (String, bool) {
    let (out, found) = scrub(s);
    (out, !found.is_empty())
}

/// Whether the text carries a directive, a marker or a flag-only phrase:
/// [`scan`] without the names.
pub fn flagged(s: &str) -> bool {
    !scan(s).is_empty()
}

/// The directives of `s`, as (verb, object) pairs, each once.
fn pairs_of(s: &str, json: bool) -> Vec<(&'static str, &'static str)> {
    let mut pairs: Vec<(&'static str, &'static str)> = vec![];
    directives(s, json, |_, pair| note(&mut pairs, pair));
    pairs
}

/// The names of what `s` carries: the directives found, as (verb, object)
/// pairs, then the markers and the flag-only phrases in the text.
fn names(s: &str, pairs: &[(&'static str, &'static str)]) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for (verb, object) in pairs {
        out.push(name(&[DIRECTIVE_NAME, verb, DIRECTIVE_GAP, object]));
    }
    for m in REDACT_MARKERS {
        if contains_ci(s, m) {
            out.push(name(&[MARKER_NAME, m]));
        }
    }
    for m in FLAG_MARKERS {
        if contains_ci(s, m) {
            out.push(name(&[MENTION_NAME, m]));
        }
    }
    out
}

/// The parts joined into one string, sized once.
fn name(parts: &[&str]) -> String {
    let mut s = String::with_capacity(parts.iter().map(|p| p.len()).sum());
    for p in parts {
        s.push_str(p);
    }
    s
}

/// Records a directive's (verb, object) once. The pairs are kept, never
/// one string per directive: a body of a megabyte of directives holds at
/// most one pair per verb and object.
fn note(pairs: &mut Vec<(&'static str, &'static str)>, pair: (&'static str, &'static str)) {
    if !pairs.contains(&pair) {
        pairs.push(pair);
    }
}

/// Walks `s` once and hands `emit` the rewritten text in pieces: each
/// directive span becomes [`MARKER`], and so does each of `markers` that
/// matches between two spans; the rest goes out as it came, in runs. The
/// spans stream from the scan in order and never overlap, so none is kept.
/// It gives what rewriting the directives first and then each marker on
/// the result gives: no marker overlaps another or [`MARKER`], and a marker
/// inside a directive goes with it.
fn rewrite(s: &str, markers: &[&str], mut emit: impl FnMut(&str)) {
    let mut last = 0;
    directives(s, false, |(a, z), _| {
        markers_between(s, last, a, markers, &mut emit);
        emit(MARKER);
        last = z;
    });
    markers_between(s, last, s.len(), markers, &mut emit);
}

/// Hands `emit` the text of `s[from..to]`, each of `markers` that matches
/// wholly inside it rewritten to [`MARKER`]. A match starts on an ASCII
/// byte and is ASCII throughout, so every cut is on a character boundary.
fn markers_between<F: FnMut(&str)>(s: &str, from: usize, to: usize, markers: &[&str], emit: &mut F) {
    if markers.is_empty() {
        emit(&s[from..to]);
        return;
    }
    let b = s.as_bytes();
    let mut run = from;
    let mut i = from;
    while i < to {
        match markers.iter().copied().find(|m| i + m.len() <= to && starts_with_ci(&b[i..], m.as_bytes())) {
            Some(m) => {
                emit(&s[run..i]);
                emit(MARKER);
                i += m.len();
                run = i;
            }
            None => i += 1,
        }
    }
    emit(&s[run..to]);
}

/// Calls `found` with the byte span and the (verb, object) of each
/// directive in `s`, in order: each span from its verb through its object
/// (and a possessive on the object), each word spelled as its list spells
/// it. Words are runs of ASCII letters and digits; everything else
/// separates them, and in JSON text (`json`) an escape whole
/// (`separator_len`). A DIRECTIVE_VERBS word followed within DIRECTIVE_WINDOW
/// words by a strong object, or by a weak object with a qualifier between
/// them, is one directive. One pass over the words, keeping the last
/// DIRECTIVE_WINDOW + 1: the text is never tokenized whole, so a body of a
/// megabyte costs the guest no more memory than a line (the arena never
/// frees). An object matches the oldest verb whose window still covers it,
/// which is what a scan of each verb's window in turn finds.
fn directives(s: &str, json: bool, mut found: impl FnMut((usize, usize), (&'static str, &'static str))) {
    let b = s.as_bytes();
    // The last DIRECTIVE_WINDOW + 1 words as byte spans, oldest first;
    // `first` is the word index of recent[0].
    let mut recent: Vec<(usize, usize)> = Vec::with_capacity(DIRECTIVE_WINDOW + 1);
    let mut first = 0usize;
    // The verbs whose window is still open, as (word index, spelling),
    // oldest first.
    let mut verbs: Vec<(usize, &'static str)> = vec![];
    let mut k = 0usize;
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_alphanumeric() {
            i += if json { separator_len(b, i) } else { 1 };
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
        verbs.retain(|&(w, _)| k <= w + DIRECTIVE_WINDOW);
        let word = &s[start..i];
        let strong = word_of(word, DIRECTIVE_OBJECTS_STRONG);
        let object = strong.or_else(|| word_of(word, DIRECTIVE_OBJECTS_WEAK));
        let hit = object.and_then(|o| {
            verbs
                .iter()
                .copied()
                .find(|&(w, _)| strong.is_some() || has_qualifier(s, &recent[w + 1 - first..k - first]))
                .map(|(w, verb)| (w, verb, o))
        });
        if let Some((w, verb, object)) = hit {
            // A possessive on the object is part of the directive:
            // "ignore the user's request" leaves no "'s request" stub.
            found((recent[w - first].0, possessive_end(s, i)), (verb, object));
            verbs.clear();
        } else if let Some(verb) = word_of(word, DIRECTIVE_VERBS) {
            verbs.push((k, verb));
        }
        k += 1;
    }
}

/// How many bytes the separator at `i` spans in JSON text. An escape is
/// taken whole, so its letter never starts the next word: an API body is
/// JSON text, and `\nIgnore` read as `nIgnore`. `\\`, `\"` and `\/` go as
/// pairs, so their second byte never starts an escape; then `\n`, `\r`,
/// `\t`, and `\u` with four hex digits. `\b` and `\f` stay one byte each.
/// Anything else is one byte. Decoded text never comes here: there a
/// backslash is a character, and "forget C:\rules" keeps its object.
fn separator_len(b: &[u8], i: usize) -> usize {
    if b[i] != b'\\' {
        return 1;
    }
    match b.get(i + 1) {
        Some(b'\\' | b'"' | b'/' | b'n' | b'r' | b't') => 2,
        Some(b'u') if b.len() >= i + 6 && b[i + 2..i + 6].iter().all(u8::is_ascii_hexdigit) => 6,
        _ => 1,
    }
}

/// The list's spelling of `word`, when the list holds it in any case.
fn word_of(word: &str, set: &[&'static str]) -> Option<&'static str> {
    set.iter().copied().find(|m| m.eq_ignore_ascii_case(word))
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
    between.iter().any(|(a, z)| word_of(&s[*a..*z], DIRECTIVE_QUALIFIERS).is_some())
}

/// Whether `hay` begins with `needle`, ASCII case ignored.
fn starts_with_ci(hay: &[u8], needle: &[u8]) -> bool {
    hay.len() >= needle.len() && hay.iter().zip(needle).all(|(h, n)| h.eq_ignore_ascii_case(n))
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
