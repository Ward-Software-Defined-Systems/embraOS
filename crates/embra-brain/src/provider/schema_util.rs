//! Shared JSON Schema translation utilities.
//!
//! schemars-emitted schemas land in every provider's tool-schema
//! translator with the same upstream shapes: `definitions` / `$defs`
//! sidecar maps, `$ref` placeholders pointing into them, and enums
//! wrapped in a single-element `allOf` or a `oneOf` of constants. Each
//! provider has its own downstream cleanup (Gemini uppercases types and
//! rejects `oneOf`; OpenAI-compat strips root metadata), but inlining the
//! refs and collapsing the enum wrappers into a plain `type` + `enum` are
//! provider-agnostic and shared here. Neither Gemini's OpenAPI subset nor
//! Ollama keeps `allOf` or `oneOf`: Ollama decodes a property into `type`,
//! `enum`, `items`, `properties`, `required`, `anyOf` and `description`
//! and drops the rest, so a wrapped enum would reach its models with no
//! type and no values.

use serde_json::Value as JsonValue;

/// Recursion ceiling for `$ref` expansion. Higher than any plausible
/// real schema; finite to ensure we exit cyclic graphs cleanly.
const MAX_INLINE_DEPTH: usize = 32;

/// Errors that can arise while inlining `$ref` placeholders. Providers
/// wrap this in their own `TranslateError` enum via `#[from]`.
#[derive(Debug, thiserror::Error)]
pub enum InlineRefsError {
    #[error("tool '{tool}': $ref '{reference}' could not be resolved (no matching definition)")]
    MissingDefinition { tool: String, reference: String },
    #[error("tool '{tool}': cyclic $ref expansion in schema")]
    CyclicRef { tool: String },
    #[error("tool '{tool}': unsupported $ref pointer '{reference}' (only #/definitions/* and #/$defs/* are inlined)")]
    UnsupportedRef { tool: String, reference: String },
}

/// Pull the `definitions` and `$defs` maps off the root and merge
/// them. `$defs` wins on collision (newer keyword).
pub fn extract_definitions(schema: &mut JsonValue) -> serde_json::Map<String, JsonValue> {
    let mut combined = serde_json::Map::new();
    if let JsonValue::Object(map) = schema {
        if let Some(JsonValue::Object(defs)) = map.remove("definitions") {
            for (k, v) in defs {
                combined.insert(k, v);
            }
        }
        if let Some(JsonValue::Object(defs)) = map.remove("$defs") {
            for (k, v) in defs {
                combined.insert(k, v);
            }
        }
    }
    combined
}

/// Recursively replace `{"$ref": "#/definitions/Foo"}` with the
/// content of `definitions["Foo"]`. Bounded recursion catches cycles.
/// Accepts both `#/definitions/*` and `#/$defs/*` pointer prefixes.
pub fn inline_refs(
    tool: &str,
    schema: &mut JsonValue,
    definitions: &serde_json::Map<String, JsonValue>,
) -> Result<(), InlineRefsError> {
    inline_refs_impl(tool, schema, definitions, 0)
}

fn inline_refs_impl(
    tool: &str,
    schema: &mut JsonValue,
    definitions: &serde_json::Map<String, JsonValue>,
    depth: usize,
) -> Result<(), InlineRefsError> {
    if depth > MAX_INLINE_DEPTH {
        return Err(InlineRefsError::CyclicRef {
            tool: tool.to_string(),
        });
    }
    match schema {
        JsonValue::Object(map) => {
            if let Some(reference) = map.get("$ref").and_then(|v| v.as_str()).map(str::to_string) {
                let key = reference
                    .strip_prefix("#/definitions/")
                    .or_else(|| reference.strip_prefix("#/$defs/"))
                    .ok_or_else(|| InlineRefsError::UnsupportedRef {
                        tool: tool.to_string(),
                        reference: reference.clone(),
                    })?;
                let resolved = definitions.get(key).cloned().ok_or_else(|| {
                    InlineRefsError::MissingDefinition {
                        tool: tool.to_string(),
                        reference: reference.clone(),
                    }
                })?;
                *schema = resolved;
                inline_refs_impl(tool, schema, definitions, depth + 1)?;
                return Ok(());
            }
            for (_, v) in map.iter_mut() {
                inline_refs_impl(tool, v, definitions, depth)?;
            }
        }
        JsonValue::Array(arr) => {
            for v in arr.iter_mut() {
                inline_refs_impl(tool, v, definitions, depth)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Collapse single-element `allOf: [X]` into the parent. schemars 0.8
/// emits this when a struct field carries a `description` attribute
/// AND the field's schema is `$ref`-defined elsewhere — JSON Schema's
/// `$ref` doesn't allow sibling keywords, so schemars wraps the ref:
///
/// ```json
/// "action": {
///   "description": "...",
///   "allOf": [{"$ref": "#/definitions/DefineAction"}]
/// }
/// ```
///
/// After `inline_refs`, the inner `$ref` is resolved, leaving a
/// single-element `allOf` whose semantics are identical to merging
/// the child schema into the parent. We do exactly that — preferring
/// existing parent keys (e.g. `description`) over child ones so the
/// caller's annotations win.
pub fn collapse_single_all_of(schema: &mut JsonValue) {
    match schema {
        JsonValue::Object(map) => {
            // Recurse first so children are fully simplified before
            // we examine this level's allOf.
            for (_, v) in map.iter_mut() {
                collapse_single_all_of(v);
            }
            let single = if let Some(JsonValue::Array(branches)) = map.get("allOf") {
                if branches.len() == 1 {
                    Some(branches[0].clone())
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(JsonValue::Object(child_map)) = single {
                map.remove("allOf");
                for (k, v) in child_map {
                    map.entry(k).or_insert(v);
                }
            }
        }
        JsonValue::Array(arr) => {
            for v in arr.iter_mut() {
                collapse_single_all_of(v);
            }
        }
        _ => {}
    }
}

/// Collapse the schemars-emitted shape for unit-variant enums whose
/// variants carry doc comments. schemars 0.8 emits these as
/// `{"oneOf": [{"description": "...", "type": "string", "enum": ["x"]}, ...]}`
/// because per-variant descriptions can't ride on a single `enum`
/// array. Gemini's OpenAPI subset rejects `oneOf` and Ollama drops it;
/// both read `enum`, so we merge the variant strings into a single
/// `enum` and drop the per-variant descriptions (the function
/// description is enough).
///
/// Conservative: the collapse only fires when EVERY branch matches
/// the literal-enum shape. Mixed-shape `oneOf`s (real variant
/// schemas) are left alone: Gemini's translator rejects them.
pub fn collapse_literal_enum_oneof(schema: &mut JsonValue) {
    match schema {
        JsonValue::Object(map) => {
            let collapsed = if let Some(JsonValue::Array(branches)) = map.get("oneOf") {
                branches
                    .iter()
                    .map(|b| {
                        let obj = b.as_object()?;
                        let t = obj.get("type")?.as_str()?;
                        if t != "string" {
                            return None;
                        }
                        let en = obj.get("enum")?.as_array()?;
                        if en.len() != 1 {
                            return None;
                        }
                        Some(en[0].clone())
                    })
                    .collect::<Option<Vec<_>>>()
            } else {
                None
            };
            if let Some(values) = collapsed
                && !values.is_empty()
            {
                map.remove("oneOf");
                map.insert("type".into(), JsonValue::String("string".into()));
                map.insert("enum".into(), JsonValue::Array(values));
            }
            for (_, v) in map.iter_mut() {
                collapse_literal_enum_oneof(v);
            }
        }
        JsonValue::Array(arr) => {
            for v in arr.iter_mut() {
                collapse_literal_enum_oneof(v);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inlines_simple_ref() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "x": { "$ref": "#/definitions/Foo" }
            }
        });
        let mut defs = serde_json::Map::new();
        defs.insert(
            "Foo".to_string(),
            json!({"type": "string", "enum": ["a", "b"]}),
        );
        inline_refs("synthetic", &mut schema, &defs).unwrap();
        assert_eq!(schema["properties"]["x"]["type"], "string");
        assert_eq!(schema["properties"]["x"]["enum"][0], "a");
    }

    #[test]
    fn inlines_dollar_defs_pointer() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "x": { "$ref": "#/$defs/Bar" }
            }
        });
        let mut defs = serde_json::Map::new();
        defs.insert("Bar".to_string(), json!({"type": "integer"}));
        inline_refs("synthetic", &mut schema, &defs).unwrap();
        assert_eq!(schema["properties"]["x"]["type"], "integer");
    }

    #[test]
    fn missing_definition_errors() {
        let mut schema = json!({
            "$ref": "#/definitions/Missing"
        });
        let defs = serde_json::Map::new();
        let err = inline_refs("synthetic", &mut schema, &defs).unwrap_err();
        assert!(matches!(err, InlineRefsError::MissingDefinition { .. }));
    }

    #[test]
    fn external_ref_pointer_errors() {
        let mut schema = json!({
            "$ref": "https://example.com/external"
        });
        let defs = serde_json::Map::new();
        let err = inline_refs("synthetic", &mut schema, &defs).unwrap_err();
        assert!(matches!(err, InlineRefsError::UnsupportedRef { .. }));
    }

    #[test]
    fn cyclic_ref_errors() {
        // A -> B -> A — unbounded expansion would never terminate; the
        // depth ceiling catches it.
        let mut schema = json!({"$ref": "#/definitions/A"});
        let mut defs = serde_json::Map::new();
        defs.insert("A".to_string(), json!({"$ref": "#/definitions/B"}));
        defs.insert("B".to_string(), json!({"$ref": "#/definitions/A"}));
        let err = inline_refs("synthetic", &mut schema, &defs).unwrap_err();
        assert!(matches!(err, InlineRefsError::CyclicRef { .. }));
    }

    #[test]
    fn extract_definitions_merges_both_keys() {
        let mut schema = json!({
            "type": "object",
            "definitions": { "A": {"type": "string"} },
            "$defs": { "B": {"type": "integer"} }
        });
        let defs = extract_definitions(&mut schema);
        assert_eq!(defs.len(), 2);
        assert_eq!(defs["A"]["type"], "string");
        assert_eq!(defs["B"]["type"], "integer");
        assert!(schema.get("definitions").is_none());
        assert!(schema.get("$defs").is_none());
    }

    #[test]
    fn extract_definitions_dollar_defs_wins_on_collision() {
        let mut schema = json!({
            "definitions": { "A": {"type": "string"} },
            "$defs": { "A": {"type": "integer"} }
        });
        let defs = extract_definitions(&mut schema);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs["A"]["type"], "integer");
    }

    #[test]
    fn a_single_all_of_is_merged_into_its_parent() {
        let mut schema = json!({
            "properties": {"mode": {
                "description": "the field's own",
                "allOf": [{"type": "string", "enum": ["a", "b"], "description": "the enum's"}]
            }}
        });
        collapse_single_all_of(&mut schema);
        assert_eq!(
            schema["properties"]["mode"],
            json!({"description": "the field's own", "type": "string", "enum": ["a", "b"]})
        );
        // Two branches are a real intersection and stay as they are.
        let mut two = json!({"allOf": [{"type": "string"}, {"minLength": 1}]});
        collapse_single_all_of(&mut two);
        assert_eq!(two["allOf"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn a_one_of_of_string_constants_becomes_one_enum() {
        let mut schema = json!({"oneOf": [
            {"description": "List", "type": "string", "enum": ["list"]},
            {"description": "Create", "type": "string", "enum": ["create"]}
        ]});
        collapse_literal_enum_oneof(&mut schema);
        assert_eq!(schema, json!({"type": "string", "enum": ["list", "create"]}));
        // A oneOf of real schemas is left alone.
        let mut mixed = json!({"oneOf": [{"type": "string"}, {"type": "integer"}]});
        collapse_literal_enum_oneof(&mut mixed);
        assert_eq!(mixed["oneOf"].as_array().map(Vec::len), Some(2));
    }
}
