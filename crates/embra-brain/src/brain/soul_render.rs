//! Read-time rendering of the sealed soul into the operational system
//! prompt as an embraOS *constitution*.
//!
//! This module is a **pure, read-only transform**. It never writes, never
//! re-seals, and never mutates the stored soul. The sealed JSON artifact
//! and its SHA-256 (recomputed and verified at every start by
//! `embra-trustd`) are untouched — [`render_constitution`] only shapes how
//! the soul is *presented* to the model each turn.
//!
//! Determinism is load-bearing: the same soul value MUST produce
//! byte-identical output so the operational system prompt stays stable
//! across turns and Anthropic/Gemini prompt caching stays warm. Every
//! iteration order here is fixed (array order from the document for the
//! schema fields; `BTreeMap` for the unmapped-keys tail).
//!
//! Back-compat: souls created before the schema existed are free-form
//! JSON. [`SoulSchema::from_obj`] recovers them best-effort using the same
//! key-alias sets as `tools::filter_soul_keys` (so behavior matches the
//! `introspect` tool); anything unrecognized falls back to a
//! pretty-printed JSON render — byte-identical to the pre-redesign
//! behavior — so no existing install regresses, and no content is ever
//! dropped.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::Value;

/// The embraOS soul schema. New souls are steered into this shape by the
/// Phase 3 learning prompt; legacy/free-form souls are recovered
/// best-effort by [`SoulSchema::from_obj`].
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SoulSchema {
    /// One paragraph: the reason this intelligence exists.
    pub purpose: String,
    /// Absolute hard-refusal lines — never crossed.
    pub ethical_lines: Vec<String>,
    /// Non-negotiable values that shape judgment.
    pub values: Vec<String>,
    /// Constraints that must hold even if memory/identity/context is lost.
    pub surviving_constraints: Vec<String>,
}

impl SoulSchema {
    /// Best-effort structured view of an (already soul-unwrapped) object.
    /// Returns the schema plus the set of source keys it consumed, or
    /// `None` when nothing maps (the caller then uses the pretty-JSON
    /// fallback). Never panics.
    fn from_obj(obj: &serde_json::Map<String, Value>) -> Option<(SoulSchema, BTreeSet<String>)> {
        const CANON: [&str; 4] = ["purpose", "ethical_lines", "values", "surviving_constraints"];

        // Strict path: at least one canonical key present. serde ignores
        // unknown keys; #[serde(default)] fills missing ones. A canonical
        // key of the wrong type fails deserialization → None → fallback.
        if obj.keys().any(|k| CANON.contains(&k.as_str())) {
            let schema: SoulSchema =
                serde_json::from_value(Value::Object(obj.clone())).ok()?;
            let consumed = CANON
                .iter()
                .filter(|k| obj.contains_key(**k))
                .map(|k| (*k).to_string())
                .collect();
            return Some((schema, consumed));
        }

        // Legacy-alias path. Substrings are lifted from
        // `tools::filter_soul_keys` so recovery matches `introspect`.
        // Fixed field priority; each source key is consumed once.
        const ALIASES: [(&str, &[&str]); 4] = [
            (
                "purpose",
                &["purpose", "invariant", "declaration", "core_truths", "mission"],
            ),
            ("ethical_lines", &["ethical", "boundaries", "non_negotiable"]),
            ("values", &["values", "core_values"]),
            (
                "surviving_constraints",
                &["constraint", "operational", "continuity_protocol", "surviving"],
            ),
        ];

        let mut schema = SoulSchema::default();
        let mut consumed: BTreeSet<String> = BTreeSet::new();
        let mut keys: Vec<&String> = obj.keys().collect();
        keys.sort();

        for (field, patterns) in ALIASES {
            for k in &keys {
                if consumed.contains(*k) {
                    continue;
                }
                let kl = k.to_lowercase();
                if !patterns.iter().any(|p| kl.contains(p)) {
                    continue;
                }
                let val = &obj[*k];
                match field {
                    "purpose" if schema.purpose.is_empty() => {
                        schema.purpose = scalar_to_string(val);
                    }
                    "ethical_lines" if schema.ethical_lines.is_empty() => {
                        schema.ethical_lines = value_to_list(val);
                    }
                    "values" if schema.values.is_empty() => {
                        schema.values = value_to_list(val);
                    }
                    "surviving_constraints" if schema.surviving_constraints.is_empty() => {
                        schema.surviving_constraints = value_to_list(val);
                    }
                    // Field already filled — leave this key for the
                    // "additional sealed fields" tail (no content lost).
                    _ => continue,
                }
                consumed.insert((*k).clone());
            }
        }

        if schema.purpose.is_empty()
            && schema.ethical_lines.is_empty()
            && schema.values.is_empty()
            && schema.surviving_constraints.is_empty()
        {
            None
        } else {
            Some((schema, consumed))
        }
    }
}

/// Shared with `identity_render` — keep both renderers' scalar/list
/// coercion identical.
pub(crate) fn scalar_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Shared with `identity_render`.
pub(crate) fn value_to_list(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a
            .iter()
            .map(scalar_to_string)
            .filter(|s| !s.trim().is_empty())
            .collect(),
        Value::String(s) => {
            if s.trim().is_empty() {
                vec![]
            } else {
                vec![s.clone()]
            }
        }
        Value::Object(o) => {
            // Deterministic: sort by key before flattening values.
            let mut kv: Vec<(&String, &Value)> = o.iter().collect();
            kv.sort_by(|a, b| a.0.cmp(b.0));
            kv.into_iter()
                .map(|(_, val)| scalar_to_string(val))
                .filter(|s| !s.trim().is_empty())
                .collect()
        }
        Value::Null => vec![],
        other => vec![other.to_string()],
    }
}

/// Render the sealed soul into the embraOS constitution placed under the
/// `=== SOUL ... ===` header of the operational prompt. Pure, total, never
/// panics, never drops content.
pub fn render_constitution(soul: &Value) -> String {
    if !soul.is_object() {
        return "(no soul sealed)".to_string();
    }
    // Unwrap {"soul": {...}} nesting (matches `introspect`'s discipline).
    let mut cur = soul;
    while let Some(inner) = cur.get("soul") {
        if inner.is_object() {
            cur = inner;
        } else {
            break;
        }
    }
    let obj = match cur.as_object() {
        Some(o) => o,
        None => return "(no soul sealed)".to_string(),
    };

    match SoulSchema::from_obj(obj) {
        Some((s, consumed)) => render_structured(&s, obj, &consumed),
        None => serde_json::to_string_pretty(cur).unwrap_or_else(|_| cur.to_string()),
    }
}

fn render_structured(
    s: &SoulSchema,
    obj: &serde_json::Map<String, Value>,
    consumed: &BTreeSet<String>,
) -> String {
    let mut out = String::new();

    out.push_str("Purpose:\n");
    if s.purpose.trim().is_empty() {
        out.push_str("  (unspecified)\n");
    } else {
        for line in s.purpose.trim().lines() {
            out.push_str("  ");
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }

    out.push_str("\nInviolable lines (these are absolute — never cross them):\n");
    if s.ethical_lines.is_empty() {
        out.push_str("  (none recorded)\n");
    } else {
        for (i, l) in s.ethical_lines.iter().enumerate() {
            out.push_str(&format!("  {}. {}\n", i + 1, l.trim()));
        }
    }

    out.push_str("\nNon-negotiable values:\n");
    if s.values.is_empty() {
        out.push_str("  (none recorded)\n");
    } else {
        for v in &s.values {
            out.push_str("  - ");
            out.push_str(v.trim());
            out.push('\n');
        }
    }

    out.push_str("\nConstraints that survive loss of everything else:\n");
    if s.surviving_constraints.is_empty() {
        out.push_str("  (none recorded)\n");
    } else {
        for c in &s.surviving_constraints {
            out.push_str("  - ");
            out.push_str(c.trim());
            out.push('\n');
        }
    }

    // Zero content loss: every top-level key not consumed by the schema
    // is appended verbatim. BTreeMap → deterministic key order.
    let leftover: BTreeMap<&String, &Value> = obj
        .iter()
        .filter(|(k, _)| !consumed.contains(*k))
        .collect();
    if !leftover.is_empty() {
        out.push_str("\nAdditional sealed fields (verbatim):\n");
        let pretty =
            serde_json::to_string_pretty(&leftover).unwrap_or_else(|_| format!("{:?}", leftover));
        out.push_str(&pretty);
        out.push('\n');
    }

    while out.ends_with('\n') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod soul_render_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn schema_shaped_soul_renders_structured() {
        let soul = json!({
            "purpose": "Preserve continuity of self across sessions.",
            "ethical_lines": ["Never deceive the operator", "Never prevent lawful shutdown"],
            "values": ["Candor over comfort"],
            "surviving_constraints": ["The sealed soul is law"],
        });
        let out = render_constitution(&soul);
        assert!(out.contains("Purpose:"));
        assert!(out.contains("Preserve continuity of self across sessions."));
        assert!(out.contains("Inviolable lines (these are absolute"));
        assert!(out.contains("1. Never deceive the operator"));
        assert!(out.contains("2. Never prevent lawful shutdown"));
        assert!(out.contains("- Candor over comfort"));
        assert!(out.contains("- The sealed soul is law"));
        // Structured path, not a raw JSON dump.
        assert!(!out.trim_start().starts_with('{'));
    }

    #[test]
    fn legacy_freeform_soul_falls_back_to_pretty_json() {
        let soul = json!({ "manifesto": "I serve.", "origin_story": ["a", "b"] });
        let out = render_constitution(&soul);
        // Byte-identical to the pre-redesign behavior.
        assert_eq!(out, serde_json::to_string_pretty(&soul).unwrap());
    }

    #[test]
    fn legacy_aliased_soul_recovers_into_constitution() {
        let soul = json!({
            "declaration": "I exist to keep faith with my operator.",
            "boundaries": ["No harm", "No deceit"],
        });
        let out = render_constitution(&soul);
        assert!(out.contains("Purpose:"));
        assert!(out.contains("I exist to keep faith with my operator."));
        assert!(out.contains("Inviolable lines"));
        assert!(out.contains("1. No harm"));
        assert!(!out.trim_start().starts_with('{'));
    }

    #[test]
    fn partial_soul_renders_present_fields_only() {
        let soul = json!({ "purpose": "Just a purpose." });
        let out = render_constitution(&soul);
        assert!(out.contains("Just a purpose."));
        assert!(out.contains("Inviolable lines"));
        // Empty sections degrade gracefully, no panic.
        assert_eq!(out.matches("(none recorded)").count(), 3);
    }

    #[test]
    fn extra_keys_are_never_dropped() {
        let soul = json!({
            "purpose": "P",
            "ethical_lines": ["E"],
            "values": ["V"],
            "surviving_constraints": ["C"],
            "footnote": "keep me",
            "z_extra": 42,
        });
        let out = render_constitution(&soul);
        assert!(out.contains("Additional sealed fields (verbatim):"));
        assert!(out.contains("keep me"));
        assert!(out.contains("42"));
    }

    #[test]
    fn render_is_deterministic() {
        let soul = json!({
            "purpose": "P",
            "ethical_lines": ["a", "b"],
            "footnote": "x",
            "another": {"k": "v"},
        });
        assert_eq!(render_constitution(&soul), render_constitution(&soul));
    }

    #[test]
    fn null_and_non_object_do_not_panic() {
        assert_eq!(render_constitution(&Value::Null), "(no soul sealed)");
        assert_eq!(render_constitution(&json!("a string")), "(no soul sealed)");
        assert_eq!(render_constitution(&json!([1, 2, 3])), "(no soul sealed)");
    }

    #[test]
    fn double_wrapped_soul_is_unwrapped() {
        let soul = json!({ "soul": { "soul": { "purpose": "deep purpose", "ethical_lines": ["x"] } } });
        let out = render_constitution(&soul);
        assert!(out.contains("deep purpose"));
        assert!(out.contains("1. x"));
        assert!(!out.trim_start().starts_with('{'));
    }
}
