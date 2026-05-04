//! OpenAI Chat Completions tool-schema translator.
//!
//! Transforms each registered tool's schemars JSON Schema output into
//! the `tools[].function.parameters` shape OpenAI Chat Completions
//! accepts. Both Ollama and LM Studio honor standard JSON Schema on
//! this surface, so the pipeline is far lighter than Gemini's:
//!
//! 1. Extract `definitions` / `$defs` and inline `$ref` placeholders
//!    via the shared `provider::schema_util::inline_refs` helper.
//! 2. Strip `$schema` and `$id` from the root (these are JSON Schema
//!    metadata, harmless but noisy on the wire).
//!
//! Unlike Gemini, OpenAI accepts `oneOf`, `allOf`, `anyOf` at any
//! level, lowercase type names, and permissive vocabulary. Light strip
//! only.

use serde_json::Value as JsonValue;

use crate::provider::schema_util::{self, InlineRefsError};
use crate::tools::registry::ToolDescriptor;

#[derive(Debug, thiserror::Error)]
pub enum TranslateError {
    #[error(transparent)]
    InlineRefs(#[from] InlineRefsError),
}

/// Translate every descriptor and return the OpenAI `tools` array shape:
/// `[{type:"function", function:{name, description, parameters}}, ...]`,
/// alphabetically sorted by function name.
pub fn translate(descriptors: &[&'static ToolDescriptor]) -> Result<JsonValue, TranslateError> {
    let mut tools: Vec<JsonValue> = Vec::with_capacity(descriptors.len());
    for d in descriptors {
        let parameters = translate_schema(d.name, (d.input_schema)())?;
        tools.push(serde_json::json!({
            "type": "function",
            "function": {
                "name": d.name,
                "description": d.description,
                "parameters": parameters,
            }
        }));
    }
    tools.sort_by(|a, b| {
        a.get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .cmp(
                b.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or(""),
            )
    });
    Ok(JsonValue::Array(tools))
}

/// Translate one tool's schema. Public so the universal-coverage test
/// can pin individual offenders when something fails.
pub fn translate_schema(
    tool_name: &str,
    mut schema: JsonValue,
) -> Result<JsonValue, TranslateError> {
    let definitions = schema_util::extract_definitions(&mut schema);
    schema_util::inline_refs(tool_name, &mut schema, &definitions)?;
    strip_root_meta_keys(&mut schema);
    ensure_object_has_properties(&mut schema);
    Ok(schema)
}

/// Strip JSON Schema metadata keys from the root that don't belong
/// on the wire. Only top-level — nested `$schema` (rare) is left as-is
/// since OpenAI tolerates it.
fn strip_root_meta_keys(schema: &mut JsonValue) {
    if let JsonValue::Object(map) = schema {
        map.remove("$schema");
        map.remove("$id");
    }
}

/// Ensure an object-typed root schema carries a `properties` field,
/// even when the args struct is unit-like (no fields). LM Studio's
/// zod-based tool-schema validator rejects requests with
/// `tools[N].function.parameters.properties` missing — error message:
/// `Required (received undefined)` at `[N, "function", "parameters",
/// "properties"]`. schemars's default emission for parameterless
/// structs (e.g., `SystemStatusArgs {}`) is `{"type": "object"}` with
/// no `properties` key; we add an empty object so LM Studio accepts.
///
/// Only stamps when the root has `type: "object"` (or no `type` —
/// schemars's default for record-shaped structs). Non-object roots
/// (rare; would be schemars output for primitive args) pass through.
fn ensure_object_has_properties(schema: &mut JsonValue) {
    let JsonValue::Object(map) = schema else {
        return;
    };
    let is_object_typed = map
        .get("type")
        .and_then(|v| v.as_str())
        .map(|t| t == "object")
        .unwrap_or(true);
    if !is_object_typed {
        return;
    }
    if !map.contains_key("properties") {
        map.insert(
            "properties".to_string(),
            JsonValue::Object(serde_json::Map::new()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry;
    use serde_json::json;

    /// Universal coverage: every registered descriptor must translate
    /// without error. The OpenAI-compat counterpart of Gemini's
    /// `all_registry_tools_translate_cleanly`. Counts >=70 to leave
    /// headroom; current registry is 90.
    #[test]
    fn all_registry_tools_translate_cleanly_openai_compat() {
        let descriptors: Vec<&'static ToolDescriptor> = registry::all_descriptors().collect();
        let result = translate(&descriptors);
        match result {
            Ok(JsonValue::Array(arr)) => {
                assert!(arr.len() >= 70, "expected >=70 tools, got {}", arr.len());
                for tool in &arr {
                    assert_eq!(tool["type"], "function");
                    assert!(tool["function"]["name"].is_string());
                    assert!(tool["function"]["description"].is_string());
                    assert!(tool["function"]["parameters"].is_object());
                }
            }
            Ok(other) => panic!("translator returned non-array root: {other:?}"),
            Err(e) => panic!("translator rejected at least one tool: {e}"),
        }
    }

    /// Manifest must be alphabetically sorted by function name for
    /// deterministic prompt-cache keys (parity with Anthropic / Gemini).
    #[test]
    fn tools_snapshot_is_sorted() {
        let descriptors: Vec<&'static ToolDescriptor> = registry::all_descriptors().collect();
        let JsonValue::Array(arr) = translate(&descriptors).unwrap() else {
            panic!("expected array");
        };
        let names: Vec<&str> = arr
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "tools manifest must be alphabetically sorted");
    }

    #[test]
    fn one_of_passes_through() {
        // OpenAI accepts oneOf at any level — unlike Gemini's reject.
        let schema = json!({
            "type": "object",
            "properties": {
                "value": {
                    "oneOf": [{"type": "string"}, {"type": "integer"}]
                }
            }
        });
        let out = translate_schema("synthetic", schema).unwrap();
        let one_of = out["properties"]["value"]["oneOf"].as_array().unwrap();
        assert_eq!(one_of.len(), 2);
        assert_eq!(one_of[0]["type"], "string");
    }

    #[test]
    fn lowercase_types_preserved() {
        // OpenAI uses lowercase (unlike Gemini's UPPERCASE requirement).
        let schema = json!({
            "type": "object",
            "properties": {
                "x": {"type": "string"}
            }
        });
        let out = translate_schema("synthetic", schema).unwrap();
        assert_eq!(out["type"], "object");
        assert_eq!(out["properties"]["x"]["type"], "string");
    }

    #[test]
    fn dollar_schema_and_dollar_id_stripped_from_root() {
        let schema = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "$id": "GitBranchArgs",
            "type": "object",
            "properties": {}
        });
        let out = translate_schema("synthetic", schema).unwrap();
        assert!(out.get("$schema").is_none());
        assert!(out.get("$id").is_none());
        assert_eq!(out["type"], "object");
    }

    #[test]
    fn additional_properties_preserved() {
        // OpenAI accepts additionalProperties; Gemini stripped it.
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {}
        });
        let out = translate_schema("synthetic", schema).unwrap();
        assert_eq!(out["additionalProperties"], false);
    }

    #[test]
    fn ref_inline_simple_enum() {
        let schema = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {
                "action": { "$ref": "#/definitions/GitBranchAction" },
                "path": { "type": "string" }
            },
            "required": ["path"],
            "definitions": {
                "GitBranchAction": {
                    "type": "string",
                    "enum": ["List", "Create", "Delete"]
                }
            }
        });
        let out = translate_schema("synthetic", schema).unwrap();
        assert_eq!(out["properties"]["action"]["type"], "string");
        let variants = out["properties"]["action"]["enum"].as_array().unwrap();
        assert_eq!(variants.len(), 3);
        // definitions stripped (extracted by schema_util::extract_definitions).
        assert!(out.get("definitions").is_none());
        assert!(out.get("$schema").is_none());
        assert_eq!(out["required"][0], "path");
    }

    #[test]
    fn parameterless_struct_gets_empty_properties() {
        // Mirrors schemars's default emission for unit-like args
        // structs (SystemStatusArgs {}, UptimeReportArgs {}, etc.).
        // LM Studio's zod validator rejects when `properties` is
        // missing — we stamp an empty object so it round-trips.
        let schema = json!({
            "type": "object",
            "title": "SystemStatusArgs"
        });
        let out = translate_schema("synthetic", schema).unwrap();
        assert_eq!(out["type"], "object");
        assert_eq!(out["properties"], json!({}));
    }

    #[test]
    fn parameterless_with_explicit_empty_properties_is_idempotent() {
        let schema = json!({
            "type": "object",
            "properties": {}
        });
        let out = translate_schema("synthetic", schema).unwrap();
        assert_eq!(out["properties"], json!({}));
    }

    #[test]
    fn schema_with_no_type_field_gets_properties() {
        // schemars sometimes emits no `type` for record-shaped args.
        // We treat that as object-shaped and stamp properties.
        let schema = json!({
            "title": "FooArgs"
        });
        let out = translate_schema("synthetic", schema).unwrap();
        assert!(out["properties"].is_object());
    }

    #[test]
    fn schema_with_existing_properties_is_unchanged() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"}
            },
            "required": ["path"]
        });
        let out = translate_schema("synthetic", schema).unwrap();
        // Existing properties preserved exactly.
        assert_eq!(out["properties"]["path"]["type"], "string");
        assert_eq!(out["required"][0], "path");
    }

    #[test]
    fn non_object_root_schema_passes_through_without_properties_stamp() {
        // Defensive: if a tool somehow declared primitive root args,
        // we don't force an `object`-shaped schema onto it.
        let schema = json!({
            "type": "string"
        });
        let out = translate_schema("synthetic", schema).unwrap();
        assert_eq!(out["type"], "string");
        assert!(out.get("properties").is_none());
    }

    #[test]
    fn all_registry_tools_have_properties_field_for_lm_studio() {
        // Universal regression for the LM Studio zod validator:
        // every translated tool schema MUST carry `properties` at
        // the root (even if empty). Catches the bug class for any
        // future parameterless args struct.
        let descriptors: Vec<&'static ToolDescriptor> = registry::all_descriptors().collect();
        let JsonValue::Array(arr) = translate(&descriptors).unwrap() else {
            panic!("expected array");
        };
        for tool in &arr {
            let name = tool["function"]["name"].as_str().unwrap();
            let params = &tool["function"]["parameters"];
            assert!(
                params["properties"].is_object(),
                "tool '{name}' missing properties on parameters: {params}"
            );
        }
    }

    #[test]
    fn defs_pointer_inline() {
        // schemars 0.9+ uses $defs (newer keyword).
        let schema = json!({
            "type": "object",
            "properties": {
                "x": { "$ref": "#/$defs/Foo" }
            },
            "$defs": {
                "Foo": { "type": "integer" }
            }
        });
        let out = translate_schema("synthetic", schema).unwrap();
        assert_eq!(out["properties"]["x"]["type"], "integer");
        assert!(out.get("$defs").is_none());
    }
}
