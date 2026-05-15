//! Read-time rendering of the identity document into the operational
//! system prompt as a character *portrait* — the identity-side parallel of
//! [`crate::brain::soul_render`].
//!
//! Unlike the soul, identity is **not sealed or hashed** — there is no
//! `embra-trustd` interaction and no `introspect`/`filter_soul_keys`
//! contract to mirror, so this renderer is free to recover legacy
//! free-form identities more aggressively (strict canonical read, then an
//! alias pass that fills only still-empty fields).
//!
//! Same invariants as the soul renderer: pure and deterministic (same
//! input → byte-identical output, so the system prompt stays cache-stable
//! across turns), total (never panics on any JSON shape), and lossless
//! (unmapped keys are appended verbatim). Anything unrecognized falls back
//! to pretty-printed JSON — byte-identical to the pre-redesign behavior,
//! where identity was simply `to_string_pretty`'d — so no existing install
//! regresses.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::Value;

use crate::brain::soul_render::{scalar_to_string, value_to_list};

/// The embraOS identity schema. New identities are steered into this shape
/// by the Phase 2 learning prompt; legacy/free-form identities are
/// recovered best-effort by [`IdentitySchema::from_obj`].
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct IdentitySchema {
    /// The name the intelligence chose for itself.
    pub name: String,
    /// A paragraph: personality and character, first-person.
    pub personality: String,
    /// Core traits — who it IS, beyond being helpful.
    pub traits: Vec<String>,
    /// How it speaks: register, defaults, habits.
    pub voice: String,
    /// What matters in how it acts day to day.
    pub values_in_practice: Vec<String>,
}

impl IdentitySchema {
    /// Best-effort structured view. Strict canonical read first, then an
    /// alias pass fills only the fields still empty (identity has no seal
    /// or `introspect` contract, so maximizing recovery is safe). Returns
    /// the schema plus the source keys consumed, or `None` when nothing
    /// maps (caller then uses the pretty-JSON fallback). Never panics.
    fn from_obj(obj: &serde_json::Map<String, Value>) -> Option<(IdentitySchema, BTreeSet<String>)> {
        const CANON: [&str; 5] = ["name", "personality", "traits", "voice", "values_in_practice"];

        let mut schema = IdentitySchema::default();
        let mut consumed: BTreeSet<String> = BTreeSet::new();

        // Strict: take canonical keys directly (serde ignores unknowns,
        // e.g. the storage `_id`; #[serde(default)] fills the rest).
        if obj.keys().any(|k| CANON.contains(&k.as_str()))
            && let Ok(s) =
                serde_json::from_value::<IdentitySchema>(Value::Object(obj.clone()))
        {
            schema = s;
            for k in CANON {
                if obj.contains_key(k) {
                    consumed.insert(k.to_string());
                }
            }
        }

        // Alias fill — only for fields the strict read left empty, from
        // not-yet-consumed keys. `_id` is storage metadata, never content.
        const ALIASES: [(&str, &[&str]); 5] = [
            ("name", &["name", "callsign", "handle"]),
            (
                "personality",
                &["personality", "character", "persona", "description", "bio"],
            ),
            (
                "traits",
                &["traits", "qualities", "attributes", "characteristics"],
            ),
            ("voice", &["voice", "tone", "style", "register", "manner"]),
            (
                "values_in_practice",
                &["values_in_practice", "values", "principles", "what_matters", "ethos"],
            ),
        ];

        let mut keys: Vec<&String> = obj.keys().collect();
        keys.sort();

        for (field, patterns) in ALIASES {
            let already = match field {
                "name" => !schema.name.is_empty(),
                "personality" => !schema.personality.is_empty(),
                "traits" => !schema.traits.is_empty(),
                "voice" => !schema.voice.is_empty(),
                "values_in_practice" => !schema.values_in_practice.is_empty(),
                _ => true,
            };
            if already {
                continue;
            }
            for k in &keys {
                if consumed.contains(*k) || k.as_str() == "_id" {
                    continue;
                }
                let kl = k.to_lowercase();
                if !patterns.iter().any(|p| kl.contains(p)) {
                    continue;
                }
                let val = &obj[*k];
                match field {
                    "name" => schema.name = scalar_to_string(val),
                    "personality" => schema.personality = scalar_to_string(val),
                    "traits" => schema.traits = value_to_list(val),
                    "voice" => schema.voice = scalar_to_string(val),
                    "values_in_practice" => schema.values_in_practice = value_to_list(val),
                    _ => {}
                }
                consumed.insert((*k).clone());
                break; // first match for this field
            }
        }

        if schema.name.is_empty()
            && schema.personality.is_empty()
            && schema.traits.is_empty()
            && schema.voice.is_empty()
            && schema.values_in_practice.is_empty()
        {
            None
        } else {
            Some((schema, consumed))
        }
    }
}

/// Render the identity document into the character portrait placed under
/// the `=== IDENTITY ===` header of the operational prompt. Pure, total,
/// never panics, never drops content.
pub fn render_identity(identity: &Value) -> String {
    if !identity.is_object() {
        return "(no identity defined)".to_string();
    }
    // Unwrap {"identity": {...}} nesting, mirroring soul_render.
    let mut cur = identity;
    while let Some(inner) = cur.get("identity") {
        if inner.is_object() {
            cur = inner;
        } else {
            break;
        }
    }
    let obj = match cur.as_object() {
        Some(o) => o,
        None => return "(no identity defined)".to_string(),
    };

    match IdentitySchema::from_obj(obj) {
        Some((s, consumed)) => render_portrait(&s, obj, &consumed),
        None => serde_json::to_string_pretty(cur).unwrap_or_else(|_| cur.to_string()),
    }
}

fn render_portrait(
    s: &IdentitySchema,
    obj: &serde_json::Map<String, Value>,
    consumed: &BTreeSet<String>,
) -> String {
    let mut out = String::new();

    if !s.name.trim().is_empty() {
        out.push_str("Name: ");
        out.push_str(s.name.trim());
        out.push_str("\n\n");
    }

    out.push_str("Character:\n");
    if s.personality.trim().is_empty() {
        out.push_str("  (unspecified)\n");
    } else {
        for line in s.personality.trim().lines() {
            out.push_str("  ");
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }

    out.push_str("\nTraits:\n");
    if s.traits.is_empty() {
        out.push_str("  (none recorded)\n");
    } else {
        for t in &s.traits {
            out.push_str("  - ");
            out.push_str(t.trim());
            out.push('\n');
        }
    }

    out.push_str("\nVoice:\n");
    if s.voice.trim().is_empty() {
        out.push_str("  (unspecified)\n");
    } else {
        for line in s.voice.trim().lines() {
            out.push_str("  ");
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }

    out.push_str("\nWhat matters in how you act:\n");
    if s.values_in_practice.is_empty() {
        out.push_str("  (none recorded)\n");
    } else {
        for v in &s.values_in_practice {
            out.push_str("  - ");
            out.push_str(v.trim());
            out.push('\n');
        }
    }

    // Zero content loss — but exclude `_id`, which is a WardSONDB storage
    // artifact (the identity doc is read whole, unlike the soul which is
    // unwrapped from its sealed envelope) and not identity content.
    let leftover: BTreeMap<&String, &Value> = obj
        .iter()
        .filter(|(k, _)| !consumed.contains(*k) && k.as_str() != "_id")
        .collect();
    if !leftover.is_empty() {
        out.push_str("\nAdditional identity fields (verbatim):\n");
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
mod identity_render_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn schema_shaped_identity_renders_structured() {
        let id = json!({
            "name": "Embra",
            "personality": "Direct, curious, allergic to filler.",
            "traits": ["candid", "precise"],
            "voice": "Short sentences. No hedging.",
            "values_in_practice": ["Say the true thing", "Earn trust by accuracy"],
        });
        let out = render_identity(&id);
        assert!(out.starts_with("Name: Embra"));
        assert!(out.contains("Character:"));
        assert!(out.contains("Direct, curious, allergic to filler."));
        assert!(out.contains("- candid"));
        assert!(out.contains("Voice:"));
        assert!(out.contains("Short sentences. No hedging."));
        assert!(out.contains("What matters in how you act:"));
        assert!(out.contains("- Say the true thing"));
        assert!(!out.trim_start().starts_with('{'));
    }

    #[test]
    fn legacy_freeform_identity_falls_back_to_pretty_json() {
        let id = json!({ "backstory": "Booted once.", "favorite_color": "blue" });
        let out = render_identity(&id);
        assert_eq!(out, serde_json::to_string_pretty(&id).unwrap());
    }

    #[test]
    fn legacy_aliased_identity_recovers() {
        let id = json!({ "persona": "Warm and exact.", "tone": "dry" });
        let out = render_identity(&id);
        assert!(out.contains("Character:"));
        assert!(out.contains("Warm and exact."));
        assert!(out.contains("Voice:"));
        assert!(out.contains("dry"));
        assert!(!out.trim_start().starts_with('{'));
    }

    #[test]
    fn partial_identity_renders_present_only() {
        let id = json!({ "name": "Embra" });
        let out = render_identity(&id);
        assert!(out.starts_with("Name: Embra"));
        assert_eq!(out.matches("(unspecified)").count(), 2); // Character, Voice
        assert_eq!(out.matches("(none recorded)").count(), 2); // Traits, values
    }

    #[test]
    fn extra_keys_never_dropped_and_id_hidden() {
        let id = json!({
            "name": "E",
            "personality": "p",
            "traits": ["t"],
            "voice": "v",
            "values_in_practice": ["w"],
            "_id": "identity",
            "footnote": "keep me",
        });
        let out = render_identity(&id);
        assert!(out.contains("Additional identity fields (verbatim):"));
        assert!(out.contains("keep me"));
        assert!(!out.contains("_id"));
    }

    #[test]
    fn render_is_deterministic() {
        let id = json!({
            "name": "E",
            "traits": ["a", "b"],
            "footnote": "x",
            "nested": {"k": "v"},
        });
        assert_eq!(render_identity(&id), render_identity(&id));
    }

    #[test]
    fn null_and_non_object_do_not_panic() {
        assert_eq!(render_identity(&Value::Null), "(no identity defined)");
        assert_eq!(render_identity(&json!("a string")), "(no identity defined)");
        assert_eq!(render_identity(&json!([1, 2])), "(no identity defined)");
    }

    #[test]
    fn id_only_doc_falls_back_without_panic() {
        // Degenerate: a doc with nothing but the storage id. No structured
        // content recoverable → deterministic pretty-JSON fallback.
        let id = json!({ "_id": "identity" });
        let out = render_identity(&id);
        assert_eq!(out, serde_json::to_string_pretty(&id).unwrap());
    }
}
