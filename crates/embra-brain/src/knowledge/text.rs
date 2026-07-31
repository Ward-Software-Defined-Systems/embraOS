//! Shared tokenizers for the knowledge hot paths.
//!
//! Two deliberately different rules, one home — so the audit's similarity
//! math and retrieval's matching can never drift apart:
//! - `query_tag_tokens` keeps tag-shaped tokens intact ("embra-web",
//!   "cert_refresh") for exact tag matching and the relevance denominator.
//! - `content_tokens` is the alphanumeric-run rule the audit's similarity
//!   scoring introduced (2026-07-30), now shared with content matching.

use std::collections::HashSet;

/// Tag-form query tokens (retrieval Step 1 + the relevance denominator):
/// whitespace split → strip leading '#'s → trim leading/trailing
/// punctuation → lowercase → keep len ≥ 3 BYTES (continuity with the
/// historical `len() > 2` filter) → DEDUPED preserving first occurrence.
/// Internal hyphens/underscores survive, so stored tags like "embra-web"
/// stay matchable — this is why content_tokens (which splits on every
/// non-alphanumeric) cannot be used for tag matching.
pub(crate) fn query_tag_tokens(s: &str) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for raw in s.split_whitespace() {
        let t = raw
            .trim_start_matches('#')
            .trim_matches(|c: char| {
                matches!(
                    c,
                    '.' | ',' | '!' | '?' | ';' | ':' | '"' | '\'' | '(' | ')' | '[' | ']'
                        | '{' | '}'
                )
            })
            .to_lowercase();
        if t.len() >= 3 && seen.insert(t.clone()) {
            out.push(t);
        }
    }
    out
}

/// Content tokens: lowercase alphanumeric runs of length ≥ 3 bytes —
/// byte-identical to the audit similarity rule (`audit::tokenize`
/// delegates here).
pub(crate) fn content_tokens(s: &str) -> HashSet<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(|t| t.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_tag_tokens_trims_hash_punct_lowercases_dedupes() {
        let t = query_tag_tokens("#Cert-Refresh works! (cert-refresh) \"Trustd\": trustd, CERT-REFRESH?");
        assert_eq!(t, vec!["cert-refresh", "works", "trustd"]);
    }

    #[test]
    fn query_tag_tokens_len_floor_is_3_bytes_after_trim() {
        // "ok!" trims to "ok" (2 bytes) — dropped; "the" survives (exactly 3).
        let t = query_tag_tokens("ok! the a?! ab, abc.");
        assert_eq!(t, vec!["the", "abc"]);
    }

    #[test]
    fn query_tag_tokens_preserves_first_occurrence_order() {
        let t = query_tag_tokens("graph memory graph knowledge memory");
        assert_eq!(t, vec!["graph", "memory", "knowledge"]);
    }

    #[test]
    fn content_tokens_are_lowercase_alnum_runs_min_3() {
        let t = content_tokens("The cert-refresh FAILED at 03:14, v2!");
        assert!(t.contains("the"));
        assert!(t.contains("cert"));
        assert!(t.contains("refresh"));
        assert!(t.contains("failed"));
        assert!(!t.contains("at"));
        assert!(!t.contains("v2"));
        // Hyphenated forms split — content matching is per-word, unlike tags.
        assert!(!t.contains("cert-refresh"));
    }

    #[test]
    fn content_tokens_match_audit_similarity_rule() {
        // Parity fixture: the exact body audit::tokenize used before
        // delegation (2026-07-30 hygiene wave). If this drifts, audit
        // similarity and retrieval content matching have diverged.
        let s = "Promotion writes a derived_from edge; tags overlap 50%!";
        let legacy: HashSet<String> = s
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.len() >= 3)
            .map(|t| t.to_string())
            .collect();
        assert_eq!(content_tokens(s), legacy);
    }
}
