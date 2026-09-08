//! Inverse-document-frequency weighting for retrieval's content matcher.
//!
//! Content matching counted every query token equally, so three stopword hits
//! outscored one rare-term hit. Measured against production on 2026-09-08:
//! `the` appears in 58% of semantic nodes, `and` 33%, `not` 26%, `for` 21%,
//! `from` 20% — and "What is the plan for the code review?" returned nodes
//! matched purely on `for,the,what` while `plan` (df 7) and `review` (df 14)
//! contributed nothing to rank.
//!
//! This module owns the weighting math ONLY. It deliberately does not touch
//! `text::content_tokens`: `audit::tokenize` delegates there and
//! `text.rs::content_tokens_match_audit_similarity_rule` pins the two byte-for
//! -byte, so changing tokenization would silently diverge the audit's
//! similarity scoring from retrieval's matching.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

/// A query token appearing in more than this share of the corpus is a
/// stopword: it still contributes strength, but it can never satisfy the
/// admission requirement on its own.
pub(crate) const STOPWORD_DF_RATIO: f64 = 0.15;

/// Below this corpus size document frequency carries no information — a
/// freshly-seeded instance is exactly that case — so the stopword gate is
/// skipped entirely. Same reasoning as retrieval's degenerate-recency guard.
pub(crate) const MIN_CORPUS_FOR_STOPWORDS: usize = 50;

/// How many query tokens the strength denominator sums over, highest IDF
/// first. Preserves the intent of retrieval's `RELEVANCE_DENOM_CAP` under IDF
/// weighting: a 25-word message cannot dilute a strong hit.
pub(crate) const IDF_DENOM_CAP: usize = 8;

/// Per-query document frequencies over the corpus a retrieval call has
/// already prefetched. Only the query's own tokens are counted, so this is a
/// handful of entries regardless of corpus size.
pub(crate) struct DocFreq {
    n_docs: usize,
    df: HashMap<String, usize>,
}

impl DocFreq {
    /// One pass over the corpus. `docs` yields each document's token set — the
    /// caller owns tokenization so this module never duplicates the rules in
    /// `text.rs`.
    pub(crate) fn build<I>(query_tokens: &HashSet<String>, docs: I) -> Self
    where
        I: IntoIterator<Item = HashSet<String>>,
    {
        let mut df: HashMap<String, usize> =
            query_tokens.iter().map(|t| (t.clone(), 0usize)).collect();
        let mut n_docs = 0usize;
        for tokens in docs {
            n_docs += 1;
            for (token, count) in df.iter_mut() {
                if tokens.contains(token) {
                    *count += 1;
                }
            }
        }
        Self { n_docs, df }
    }

    /// Smoothed IDF, floored at 1.0 so no query token ever weighs zero.
    /// At the production corpus (N = 2,281): `the` (df ~1,324) scores 1.55,
    /// `review` (df 14) scores 6.02 — a ~3.9x spread.
    pub(crate) fn idf(&self, token: &str) -> f64 {
        let df = self.df.get(token).copied().unwrap_or(0) as f64;
        ((self.n_docs as f64 + 1.0) / (df + 1.0)).ln() + 1.0
    }

    /// True when the token is too common to carry admission signal. Always
    /// false on a corpus below `MIN_CORPUS_FOR_STOPWORDS`.
    pub(crate) fn is_stopword(&self, token: &str) -> bool {
        if self.n_docs < MIN_CORPUS_FOR_STOPWORDS {
            return false;
        }
        let df = self.df.get(token).copied().unwrap_or(0) as f64;
        df / (self.n_docs as f64) > STOPWORD_DF_RATIO
    }

    /// Strength denominator: the summed IDF of the `IDF_DENOM_CAP`
    /// highest-weighted query tokens. Never zero (IDF is floored at 1.0 and a
    /// non-empty query yields at least one term), so callers divide safely.
    pub(crate) fn denominator(&self, query_tokens: &HashSet<String>) -> f64 {
        let mut weights: Vec<f64> = query_tokens.iter().map(|t| self.idf(t)).collect();
        weights.sort_by(|a, b| b.partial_cmp(a).unwrap_or(Ordering::Equal));
        weights.truncate(IDF_DENOM_CAP);
        let sum: f64 = weights.iter().sum();
        if sum > 0.0 { sum } else { 1.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> HashSet<String> {
        s.split_whitespace().map(|t| t.to_string()).collect()
    }

    /// The corpus below is 100 docs so the stopword gate is live
    /// (>= MIN_CORPUS_FOR_STOPWORDS): "the" is in 60, "review" in 3.
    fn corpus() -> Vec<HashSet<String>> {
        let mut docs = Vec::new();
        for i in 0..100 {
            let mut d = toks("filler content");
            if i < 60 {
                d.insert("the".into());
            }
            if i < 3 {
                d.insert("review".into());
            }
            docs.push(d);
        }
        docs
    }

    #[test]
    fn idf_is_smoothed_floored_at_one_and_rarer_scores_higher() {
        let q = toks("the review absent");
        let df = DocFreq::build(&q, corpus());
        assert_eq!(df.n_docs, 100);
        // ln(101/61)+1
        let the = df.idf("the");
        // ln(101/4)+1
        let review = df.idf("review");
        // ln(101/1)+1 — never observed, maximum weight
        let absent = df.idf("absent");
        assert!(the >= 1.0, "IDF is floored at 1.0, got {the}");
        assert!(review > the, "rarer term must outweigh the common one");
        assert!(absent > review, "unseen term carries the most weight");
        assert!((the - ((101.0f64 / 61.0).ln() + 1.0)).abs() < 1e-9);
        assert!((review - ((101.0f64 / 4.0).ln() + 1.0)).abs() < 1e-9);
    }

    #[test]
    fn idf_floor_is_exactly_one_when_every_doc_contains_the_token() {
        let q = toks("ubiquitous");
        let docs: Vec<HashSet<String>> = (0..100).map(|_| toks("ubiquitous")).collect();
        let df = DocFreq::build(&q, docs);
        // ln((100+1)/(100+1)) + 1 = 1.0 exactly.
        assert!((df.idf("ubiquitous") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn stopword_gate_uses_the_df_ratio() {
        let q = toks("the review");
        let df = DocFreq::build(&q, corpus());
        assert!(df.is_stopword("the"), "60/100 is over the 0.15 ratio");
        assert!(!df.is_stopword("review"), "3/100 is under the ratio");
    }

    #[test]
    fn stopword_gate_is_skipped_on_a_small_corpus() {
        // A freshly-seeded instance: df carries no information, so nothing is
        // a stopword no matter how ubiquitous it looks.
        let q = toks("the");
        let docs: Vec<HashSet<String>> = (0..MIN_CORPUS_FOR_STOPWORDS - 1)
            .map(|_| toks("the"))
            .collect();
        let df = DocFreq::build(&q, docs);
        assert!(df.n_docs < MIN_CORPUS_FOR_STOPWORDS);
        assert!(!df.is_stopword("the"));
    }

    #[test]
    fn denominator_sums_the_top_eight_weights_only() {
        // 12 query tokens, none observed → every IDF is identical, so the
        // denominator must be exactly 8 of them, not 12.
        let q: HashSet<String> = (0..12).map(|i| format!("tok{i}")).collect();
        let df = DocFreq::build(&q, Vec::<HashSet<String>>::new());
        let one = df.idf("tok0");
        assert!((df.denominator(&q) - one * IDF_DENOM_CAP as f64).abs() < 1e-9);
    }

    #[test]
    fn denominator_prefers_the_highest_weighted_tokens() {
        let q = toks("the review absent");
        let df = DocFreq::build(&q, corpus());
        // Under the cap, so all three sum — and the sum must exceed a
        // three-way sum of the cheapest term.
        let expected = df.idf("the") + df.idf("review") + df.idf("absent");
        assert!((df.denominator(&q) - expected).abs() < 1e-9);
    }

    #[test]
    fn denominator_is_never_zero_for_an_empty_query() {
        let empty: HashSet<String> = HashSet::new();
        let df = DocFreq::build(&empty, corpus());
        assert!(df.denominator(&empty) > 0.0, "callers divide by this");
    }
}
