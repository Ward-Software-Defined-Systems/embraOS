//! Shared JSON Schema translation utilities.
//!
//! schemars-emitted schemas land in every provider's tool-schema
//! translator with the same upstream shapes: `definitions` / `$defs`
//! sidecar maps, `$ref` placeholders pointing into them. Each provider
//! has its own downstream cleanup (Gemini uppercases types and rejects
//! `oneOf`; OpenAI-compat passes most JSON Schema through), but the
//! ref-inlining step is provider-agnostic and shared here.

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
}
