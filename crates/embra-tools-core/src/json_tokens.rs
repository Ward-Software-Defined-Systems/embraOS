//! One JSON tokenizer for the two renderers that colour a JSON line: the
//! console (`render.rs::parse_json_line`, ratatui styles) and the web
//! console (`json_view.rs::highlight_json_segments`, CSS classes). The two
//! had drifted apart (#40); this is the one set of rules, and each
//! renderer maps a kind to its own look.
//!
//! The rules, byte ranges over one line, every byte covered in order:
//! - a string literal, escapes included, is a `Key` when the next
//!   non-space byte is `:` and a `Str` otherwise;
//! - a number starts with a digit or `-` where only structure precedes it
//!   on the line (nothing, whitespace, or a `:`, `,` or `[` and
//!   whitespace) and runs over digits, `.`, `e`, `E`, `+` and `-`;
//! - `true` and `false` are a `Bool` and `null` a `Null`, at word
//!   boundaries only (`null` inside `nullable` is text);
//! - a run of structural bytes and whitespace (`{ } [ ] : ,`) is `Punct`;
//!   any other run, a line that is not JSON among them, is `Text`.

/// What a token of a JSON line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonTokenKind {
    /// A string literal followed by `:`.
    Key,
    /// Any other string literal.
    Str,
    Number,
    /// `true` or `false`.
    Bool,
    Null,
    /// Structure and whitespace between the values.
    Punct,
    /// Anything else, a line that is not JSON included.
    Text,
}

/// A token: its kind and its byte range on the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonToken {
    pub kind: JsonTokenKind,
    pub start: usize,
    pub end: usize,
}

impl JsonToken {
    /// The token's text on `line`.
    pub fn text<'a>(&self, line: &'a str) -> &'a str {
        &line[self.start..self.end]
    }
}

/// The tokens of one line, contiguous and in order; an empty line has none.
pub fn tokenize_line(line: &str) -> Vec<JsonToken> {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut tokens: Vec<JsonToken> = Vec::new();
    // The run of bytes not yet classified, as a start offset.
    let mut run_start: Option<usize> = None;
    let mut i = 0;

    let flush = |run_start: &mut Option<usize>, end: usize, tokens: &mut Vec<JsonToken>| {
        if let Some(start) = run_start.take()
            && start < end
        {
            let run = &line[start..end];
            let kind = if run.chars().all(|c| c.is_whitespace() || matches!(c, '{' | '}' | '[' | ']' | ':' | ',')) {
                JsonTokenKind::Punct
            } else {
                JsonTokenKind::Text
            };
            tokens.push(JsonToken { kind, start, end });
        }
    };

    while i < len {
        let b = bytes[i];
        if b == b'"' {
            flush(&mut run_start, i, &mut tokens);
            let start = i;
            i += 1;
            while i < len && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < len {
                    i += 1;
                }
                i += 1;
            }
            if i < len {
                i += 1; // the closing quote
            }
            let end = i;
            let mut j = end;
            while j < len && bytes[j] == b' ' {
                j += 1;
            }
            let kind = if j < len && bytes[j] == b':' { JsonTokenKind::Key } else { JsonTokenKind::Str };
            tokens.push(JsonToken { kind, start, end });
        } else if (b.is_ascii_digit() || b == b'-') && number_may_start(line, run_start, i) {
            flush(&mut run_start, i, &mut tokens);
            let start = i;
            i += 1;
            while i < len && (bytes[i].is_ascii_digit() || matches!(bytes[i], b'.' | b'e' | b'E' | b'+' | b'-')) {
                i += 1;
            }
            tokens.push(JsonToken { kind: JsonTokenKind::Number, start, end: i });
        } else if let Some((word, kind)) = keyword_at(line, i) {
            flush(&mut run_start, i, &mut tokens);
            tokens.push(JsonToken { kind, start: i, end: i + word.len() });
            i += word.len();
        } else {
            if run_start.is_none() {
                run_start = Some(i);
            }
            // Step over the whole character, so a run never ends inside one.
            i += line[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    flush(&mut run_start, len, &mut tokens);
    tokens
}

/// A number may start at `i` when the unclassified run before it is empty
/// or whitespace, or ends with `:`, `,` or `[` and whitespace.
fn number_may_start(line: &str, run_start: Option<usize>, i: usize) -> bool {
    match run_start {
        None => true,
        Some(start) => {
            let run = line[start..i].trim_end();
            run.is_empty() || run.ends_with(':') || run.ends_with(',') || run.ends_with('[')
        }
    }
}

/// `true`, `false` or `null` at `i`, on word boundaries.
fn keyword_at(line: &str, i: usize) -> Option<(&'static str, JsonTokenKind)> {
    for (word, kind) in [("true", JsonTokenKind::Bool), ("false", JsonTokenKind::Bool), ("null", JsonTokenKind::Null)] {
        if !line[i..].starts_with(word) {
            continue;
        }
        let before_ok = i == 0 || !line[..i].chars().next_back().is_some_and(char::is_alphanumeric);
        let after = i + word.len();
        let after_ok = after >= line.len() || !line[after..].chars().next().is_some_and(char::is_alphanumeric);
        if before_ok && after_ok {
            return Some((word, kind));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::JsonTokenKind::*;
    use super::*;

    fn kinds(line: &str) -> Vec<(JsonTokenKind, &str)> {
        tokenize_line(line).iter().map(|t| (t.kind, t.text(line))).collect()
    }

    #[test]
    fn keys_strings_numbers_and_the_three_keywords_are_told_apart() {
        assert_eq!(
            kinds(r#"{"name": "Embra", "turns": 42, "sealed": true, "model": null, "off": false}"#),
            vec![
                (Punct, "{"), (Key, "\"name\""), (Punct, ": "), (Str, "\"Embra\""), (Punct, ", "),
                (Key, "\"turns\""), (Punct, ": "), (Number, "42"), (Punct, ", "),
                (Key, "\"sealed\""), (Punct, ": "), (Bool, "true"), (Punct, ", "),
                (Key, "\"model\""), (Punct, ": "), (Null, "null"), (Punct, ", "),
                (Key, "\"off\""), (Punct, ": "), (Bool, "false"), (Punct, "}"),
            ]
        );
    }

    #[test]
    fn a_string_keeps_its_escapes_and_a_key_is_told_by_the_colon_after_it() {
        assert_eq!(
            kinds(r#"  "quote\"d \\ key" :  "va\"lue""#),
            vec![(Punct, "  "), (Key, "\"quote\\\"d \\\\ key\""), (Punct, " :  "), (Str, "\"va\\\"lue\"")]
        );
        // An unterminated string runs to the end of the line.
        assert_eq!(kinds(r#""open"#), vec![(Str, "\"open")]);
    }

    #[test]
    fn numbers_start_after_structure_only_negative_and_exponent_included() {
        assert_eq!(
            kinds("[-1.5e+3, 2, 7]"),
            vec![(Punct, "["), (Number, "-1.5e+3"), (Punct, ", "), (Number, "2"), (Punct, ", "), (Number, "7"), (Punct, "]")]
        );
        assert_eq!(kinds("  3"), vec![(Punct, "  "), (Number, "3")]);
        // A digit inside a word is text, not a number.
        assert_eq!(kinds("abc1 - 2"), vec![(Text, "abc1 - 2")]);
    }

    #[test]
    fn keywords_need_word_boundaries() {
        assert_eq!(kinds("nullable"), vec![(Text, "nullable")]);
        assert_eq!(kinds("[null]"), vec![(Punct, "["), (Null, "null"), (Punct, "]")]);
        assert_eq!(kinds("truest true"), vec![(Text, "truest "), (Bool, "true")]);
    }

    #[test]
    fn nested_punctuation_is_one_run_between_the_values() {
        assert_eq!(
            kinds(r#"{"a": [{"b": []}], "c": {}}"#),
            vec![
                (Punct, "{"), (Key, "\"a\""), (Punct, ": [{"), (Key, "\"b\""), (Punct, ": []}], "),
                (Key, "\"c\""), (Punct, ": {}}"),
            ]
        );
    }

    #[test]
    fn a_line_that_is_not_json_is_text_and_an_empty_one_has_no_token() {
        assert_eq!(kinds("hello there, world"), vec![(Text, "hello there, world")]);
        assert_eq!(kinds("日本語 の テキスト"), vec![(Text, "日本語 の テキスト")]);
        assert!(tokenize_line("").is_empty());
    }

    /// Every byte of the line is covered, in order, by one token.
    #[test]
    fn the_tokens_cover_the_line_in_order() {
        for line in [
            r#"{"k": "v", "n": -1, "b": true, "z": null, "l": [1, 2]}"#,
            "plain text with 3 numbers 4 and \"a quote\"",
            "日本語: \"値\", 1",
            "   ",
            "}}}",
            "\"unterminated",
        ] {
            let tokens = tokenize_line(line);
            let mut at = 0;
            for t in &tokens {
                assert_eq!(t.start, at, "{line}: a gap before {t:?}");
                assert!(t.end > t.start, "{line}: an empty token {t:?}");
                assert!(line.is_char_boundary(t.start) && line.is_char_boundary(t.end), "{line}: {t:?}");
                at = t.end;
            }
            assert_eq!(at, line.len(), "{line}: the tokens stop short");
        }
    }
}
