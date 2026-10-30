//! Masking of a credential typed on a command line, shared by the console
//! and the mobile chat (#20). Five slash forms carry a secret as their last
//! argument — the rest of the line from where it starts, since a value may
//! hold spaces (`Authorization Bearer <token>`, quoted or not); a client
//! shows the line with that rest replaced, so neither its input box nor its
//! echoed row ever carries the value. The brain sees the line as typed.

/// The character a masked secret is drawn with.
pub const MASK_CHAR: char = '•';

/// The forms: the leading tokens that identify one, then the index of the
/// token where the secret starts; it runs to the last token of the line.
/// `remove` in that place is a command word, not a value, and is left as it
/// is.
const SECRET_FORMS: &[(&[&str], usize)] = &[
    (&["/github-token"], 1),             // /github-token <token>
    (&["/git-token"], 2),                // /git-token <host> <token>
    (&["/image-provider", "key"], 2),    // /image-provider key <token>
    (&["/guardian", "secret"], 4),       // /guardian secret <host> <header> <value>
    (&["/guardian", "key", "brave"], 3), // /guardian key brave <token>
];

/// The byte range of the secret on `line`, from its first token to the end
/// of the last one, when the line is one of the forms and carries a value in
/// that place. Trailing whitespace stays outside it.
fn secret_span(line: &str) -> Option<(usize, usize)> {
    let tokens = token_spans(line);
    let word = |i: usize| tokens.get(i).map(|&(s, e)| &line[s..e]);
    for (lead, at) in SECRET_FORMS {
        let leads = lead.iter().enumerate().all(|(i, w)| word(i) == Some(w));
        if !leads {
            continue;
        }
        let &(start, end) = tokens.get(*at)?;
        if &line[start..end] == "remove" {
            return None;
        }
        let &(_, last) = tokens.last()?;
        return Some((start, last));
    }
    None
}

/// Whitespace-separated tokens as byte ranges, in order.
fn token_spans(line: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in line.char_indices() {
        match (c.is_whitespace(), start) {
            (false, None) => start = Some(i),
            (true, Some(s)) => {
                spans.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        spans.push((s, line.len()));
    }
    spans
}

fn mask_with(line: &str, mask: impl FnOnce(&str) -> String) -> String {
    match secret_span(line) {
        Some((start, end)) => {
            let mut out = String::with_capacity(line.len());
            out.push_str(&line[..start]);
            out.push_str(&mask(&line[start..end]));
            out.push_str(&line[end..]);
            out
        }
        None => line.to_string(),
    }
}

/// `line` with its secret replaced by four mask characters, spaces of the
/// value included; a line that is none of the forms, or carries no value yet,
/// comes back as it is. For an echoed row, where the value's length is
/// nobody's business.
pub fn mask_secret_argument(line: &str) -> String {
    mask_with(line, |_| MASK_CHAR.to_string().repeat(4))
}

/// `line` with its secret replaced by one mask character per character of
/// the value, its spaces included, so the masked line has the character
/// count of the typed one: an input box keeps its rows and its cursor.
pub fn mask_secret_argument_keeping_length(line: &str) -> String {
    mask_with(line, |v| MASK_CHAR.to_string().repeat(v.chars().count()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_secret_form_is_masked_and_the_rest_of_the_line_kept() {
        for (line, masked) in [
            ("/github-token ghp_abc123", "/github-token ••••"),
            ("/git-token gitlab.example.com glpat-xyz", "/git-token gitlab.example.com ••••"),
            ("/image-provider key AIza-123", "/image-provider key ••••"),
            (
                "/guardian secret api.example.com Authorization Bearer-token-9",
                "/guardian secret api.example.com Authorization ••••",
            ),
            // A value with spaces is one secret to the end of the line, quoted
            // or not (the brain takes the rest of the line as the value).
            (
                "/guardian secret api.example.com Authorization Bearer token-9",
                "/guardian secret api.example.com Authorization ••••",
            ),
            (
                "/guardian secret api.example.com Authorization \"Bearer token-9\"",
                "/guardian secret api.example.com Authorization ••••",
            ),
            ("/github-token ghp_abc 123", "/github-token ••••"),
            ("/guardian key brave BSA-key", "/guardian key brave ••••"),
            // Extra spacing is kept as typed around the masked value.
            ("/github-token   ghp_abc123  ", "/github-token   ••••  "),
        ] {
            assert_eq!(mask_secret_argument(line), masked, "{line}");
        }
    }

    #[test]
    fn the_remove_forms_a_missing_value_and_any_other_line_are_unchanged() {
        for line in [
            "/git-token gitlab.example.com remove",
            "/guardian secret api.example.com remove",
            "/image-provider key remove",
            "/github-token",
            "/github-token ",
            "/git-token gitlab.example.com",
            "/guardian secret api.example.com Authorization",
            "/guardian key",
            "/guardian secret",
            "/help",
            "hello there, the token is secret",
            "",
        ] {
            assert_eq!(mask_secret_argument(line), line, "{line}");
            assert_eq!(mask_secret_argument_keeping_length(line), line, "{line}");
        }
    }

    #[test]
    fn the_length_keeping_mask_has_the_typed_lines_character_count() {
        let line = "/guardian secret host X-Api-Key sk-live-ünïcode";
        let masked = mask_secret_argument_keeping_length(line);
        assert_eq!(masked.chars().count(), line.chars().count());
        assert_eq!(masked, "/guardian secret host X-Api-Key •••••••••••••••");
        // The spaces of a value are masked with it, and the count still holds.
        let line = "/guardian secret host Authorization Bearer abc-9 ";
        let masked = mask_secret_argument_keeping_length(line);
        assert_eq!(masked.chars().count(), line.chars().count());
        assert_eq!(masked, "/guardian secret host Authorization •••••••••••• ");
        // A value being typed is masked from its first character.
        assert_eq!(mask_secret_argument_keeping_length("/github-token g"), "/github-token •");
    }

    #[test]
    fn a_value_that_is_only_a_prefix_of_remove_is_still_masked() {
        assert_eq!(mask_secret_argument("/git-token host remov"), "/git-token host ••••");
    }
}
