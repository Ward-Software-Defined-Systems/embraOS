//! Read-time rendering of the operator's user profile into the
//! operational system prompt as a legible profile — the third member of
//! the [`crate::brain::soul_render`] / [`crate::brain::identity_render`]
//! family.
//!
//! Like identity, the user profile is **not sealed or hashed** (no
//! `embra-trustd`, no `introspect`/`filter_soul_keys` contract), so the
//! same blended recovery applies: strict canonical read, then an alias
//! pass that fills only still-empty fields.
//!
//! Same invariants: pure and deterministic (cache-stable), total (never
//! panics), lossless (unmapped keys appended verbatim, `_id` storage key
//! excluded), and the unrecognized-shape fallback is byte-identical to the
//! pre-redesign `to_string_pretty` behavior so no existing install
//! regresses.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::Value;

use crate::brain::soul_render::{scalar_to_string, value_to_list};

/// The embraOS user-profile schema. New profiles are steered into this
/// shape by the Phase 1 learning prompt; legacy/free-form profiles are
/// recovered best-effort by [`UserSchema::from_obj`].
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct UserSchema {
    /// What the operator wants to be called.
    pub name: String,
    /// Their role (developer, researcher, operator, ...).
    pub role: String,
    /// A sentence or two on their technical background.
    pub background: String,
    /// Communication preferences (direct, concise, formal, ...).
    pub communication: Vec<String>,
    /// Operator-set boundaries — things the intelligence should never do.
    pub boundaries: Vec<String>,
}

impl UserSchema {
    /// Strict canonical read first, then an alias pass fills only the
    /// fields still empty. Returns the schema plus the consumed source
    /// keys, or `None` when nothing maps (caller uses the pretty-JSON
    /// fallback). Never panics.
    fn from_obj(obj: &serde_json::Map<String, Value>) -> Option<(UserSchema, BTreeSet<String>)> {
        const CANON: [&str; 5] = ["name", "role", "background", "communication", "boundaries"];

        let mut schema = UserSchema::default();
        let mut consumed: BTreeSet<String> = BTreeSet::new();

        if obj.keys().any(|k| CANON.contains(&k.as_str()))
            && let Ok(s) = serde_json::from_value::<UserSchema>(Value::Object(obj.clone()))
        {
            schema = s;
            for k in CANON {
                if obj.contains_key(k) {
                    consumed.insert(k.to_string());
                }
            }
        }

        // Alias fill — only for still-empty fields, from not-yet-consumed
        // keys. `_id` is WardSONDB storage metadata, never profile content.
        const ALIASES: [(&str, &[&str]); 5] = [
            ("name", &["name", "preferred_name", "call_me", "callsign", "handle"]),
            ("role", &["role", "occupation", "title", "position"]),
            (
                "background",
                &["background", "technical_background", "expertise", "experience", "bio", "about"],
            ),
            (
                "communication",
                &["communication", "comm_style", "preferences", "style", "tone"],
            ),
            (
                "boundaries",
                &["boundaries", "constraints", "never", "do_not", "avoid", "dislikes"],
            ),
        ];

        let mut keys: Vec<&String> = obj.keys().collect();
        keys.sort();

        for (field, patterns) in ALIASES {
            let already = match field {
                "name" => !schema.name.is_empty(),
                "role" => !schema.role.is_empty(),
                "background" => !schema.background.is_empty(),
                "communication" => !schema.communication.is_empty(),
                "boundaries" => !schema.boundaries.is_empty(),
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
                    "role" => schema.role = scalar_to_string(val),
                    "background" => schema.background = scalar_to_string(val),
                    "communication" => schema.communication = value_to_list(val),
                    "boundaries" => schema.boundaries = value_to_list(val),
                    _ => {}
                }
                consumed.insert((*k).clone());
                break;
            }
        }

        if schema.name.is_empty()
            && schema.role.is_empty()
            && schema.background.is_empty()
            && schema.communication.is_empty()
            && schema.boundaries.is_empty()
        {
            None
        } else {
            Some((schema, consumed))
        }
    }
}

/// Render the user profile into the operator profile placed under the
/// `=== USER PROFILE ===` header. Pure, total, never panics, never drops
/// content.
pub fn render_user_profile(profile: &Value) -> String {
    if !profile.is_object() {
        return "(no operator profile)".to_string();
    }
    // Unwrap {"user": {...}} / {"profile": {...}} nesting defensively.
    let mut cur = profile;
    loop {
        if let Some(inner) = cur.get("user").filter(|v| v.is_object()) {
            cur = inner;
        } else if let Some(inner) = cur.get("profile").filter(|v| v.is_object()) {
            cur = inner;
        } else {
            break;
        }
    }
    let obj = match cur.as_object() {
        Some(o) => o,
        None => return "(no operator profile)".to_string(),
    };

    match UserSchema::from_obj(obj) {
        Some((s, consumed)) => render_profile(&s, obj, &consumed),
        None => serde_json::to_string_pretty(cur).unwrap_or_else(|_| cur.to_string()),
    }
}

fn render_profile(
    s: &UserSchema,
    obj: &serde_json::Map<String, Value>,
    consumed: &BTreeSet<String>,
) -> String {
    let mut out = String::new();

    if !s.name.trim().is_empty() {
        out.push_str("Operator: ");
        out.push_str(s.name.trim());
        out.push_str("\n\n");
    }

    out.push_str("Role:\n");
    if s.role.trim().is_empty() {
        out.push_str("  (unspecified)\n");
    } else {
        out.push_str("  ");
        out.push_str(s.role.trim());
        out.push('\n');
    }

    out.push_str("\nBackground:\n");
    if s.background.trim().is_empty() {
        out.push_str("  (unspecified)\n");
    } else {
        for line in s.background.trim().lines() {
            out.push_str("  ");
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }

    out.push_str("\nCommunication preferences:\n");
    if s.communication.is_empty() {
        out.push_str("  (none recorded)\n");
    } else {
        for c in &s.communication {
            out.push_str("  - ");
            out.push_str(c.trim());
            out.push('\n');
        }
    }

    out.push_str("\nOperator boundaries (things to never do):\n");
    if s.boundaries.is_empty() {
        out.push_str("  (none recorded)\n");
    } else {
        for b in &s.boundaries {
            out.push_str("  - ");
            out.push_str(b.trim());
            out.push('\n');
        }
    }

    // Zero content loss; exclude the `_id` storage artifact (the profile
    // doc is read whole from WardSONDB, like identity).
    let leftover: BTreeMap<&String, &Value> = obj
        .iter()
        .filter(|(k, _)| !consumed.contains(*k) && k.as_str() != "_id")
        .collect();
    if !leftover.is_empty() {
        out.push_str("\nAdditional profile fields (verbatim):\n");
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
mod user_render_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn schema_shaped_profile_renders_structured() {
        let p = json!({
            "name": "William",
            "role": "Owner / developer",
            "background": "Experienced Rust developer.",
            "communication": ["direct", "no filler"],
            "boundaries": ["Never push without asking", "Never touch gitignored docs"],
        });
        let out = render_user_profile(&p);
        assert!(out.starts_with("Operator: William"));
        assert!(out.contains("Role:"));
        assert!(out.contains("Owner / developer"));
        assert!(out.contains("Background:"));
        assert!(out.contains("Experienced Rust developer."));
        assert!(out.contains("Communication preferences:"));
        assert!(out.contains("- direct"));
        assert!(out.contains("Operator boundaries (things to never do):"));
        assert!(out.contains("- Never push without asking"));
        assert!(!out.trim_start().starts_with('{'));
    }

    #[test]
    fn legacy_freeform_profile_falls_back_to_pretty_json() {
        let p = json!({ "anecdote": "Likes tea.", "lucky_number": 7 });
        let out = render_user_profile(&p);
        assert_eq!(out, serde_json::to_string_pretty(&p).unwrap());
    }

    #[test]
    fn legacy_aliased_profile_recovers() {
        let p = json!({ "preferred_name": "Will", "occupation": "engineer", "do_not": ["spam them"] });
        let out = render_user_profile(&p);
        assert!(out.starts_with("Operator: Will"));
        assert!(out.contains("engineer"));
        assert!(out.contains("Operator boundaries"));
        assert!(out.contains("- spam them"));
        assert!(!out.trim_start().starts_with('{'));
    }

    #[test]
    fn partial_profile_renders_present_only() {
        let p = json!({ "name": "Will" });
        let out = render_user_profile(&p);
        assert!(out.starts_with("Operator: Will"));
        assert_eq!(out.matches("(unspecified)").count(), 2); // Role, Background
        assert_eq!(out.matches("(none recorded)").count(), 2); // Communication, boundaries
    }

    #[test]
    fn extra_keys_never_dropped_and_id_hidden() {
        let p = json!({
            "name": "W",
            "role": "dev",
            "background": "b",
            "communication": ["x"],
            "boundaries": ["y"],
            "_id": "user",
            "timezone": "UTC",
        });
        let out = render_user_profile(&p);
        assert!(out.contains("Additional profile fields (verbatim):"));
        assert!(out.contains("timezone"));
        assert!(out.contains("UTC"));
        assert!(!out.contains("_id"));
    }

    #[test]
    fn render_is_deterministic() {
        let p = json!({
            "name": "W",
            "communication": ["a", "b"],
            "extra": "x",
            "nested": {"k": "v"},
        });
        assert_eq!(render_user_profile(&p), render_user_profile(&p));
    }

    #[test]
    fn null_and_non_object_do_not_panic() {
        assert_eq!(render_user_profile(&Value::Null), "(no operator profile)");
        assert_eq!(render_user_profile(&json!("s")), "(no operator profile)");
        assert_eq!(render_user_profile(&json!([1])), "(no operator profile)");
    }

    #[test]
    fn id_only_doc_falls_back_without_panic() {
        let p = json!({ "_id": "user" });
        let out = render_user_profile(&p);
        assert_eq!(out, serde_json::to_string_pretty(&p).unwrap());
    }
}
