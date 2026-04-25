//! Gemini tool schema translator.
//!
//! Transforms each registered tool's schemars JSON Schema output into
//! the OpenAPI-3.0 subset Gemini accepts in
//! `tools[0].functionDeclarations[N].parameters`.
//!
//! Pipeline (per tool):
//! 1. Pull `definitions` and `$defs` off the root and inline every
//!    `$ref` reference. schemars emits these for any named-enum field
//!    (`GitBranchAction`, `PrMergeMethod`, `DefineAction`,
//!    `DraftAction`); inlining is required because Gemini does not
//!    document `$ref` support.
//! 2. Recursively reject if `oneOf` or `allOf` appears at any level —
//!    Gemini's docs list only `anyOf` among the union keywords. The
//!    Anthropic guard test only catches root-level combinators; here
//!    we walk transitively.
//! 3. Recursively uppercase the `type` field at every level.
//!    Schemars emits lowercase (`"string"`); Gemini requires uppercase
//!    (`"STRING"`).
//! 4. Recursively strip fields outside the documented subset
//!    (`$schema`, `additionalProperties`, `examples`, schemars-specific
//!    keys). Preserve only what Gemini documents as accepted.

use serde_json::Value as JsonValue;

use crate::tools::registry::ToolDescriptor;

/// Recursion ceiling for `$ref` expansion. Higher than any plausible
/// real schema; finite to ensure we exit cyclic graphs cleanly.
const MAX_INLINE_DEPTH: usize = 32;

/// Schema-meta keywords that schemars or upstream callers may emit
/// but Gemini does NOT document as accepted in
/// `https://ai.google.dev/api/caching#FunctionDeclaration`. Stripped
/// recursively from any schema-shaped object. `definitions` / `$defs`
/// are extracted earlier in the pipeline so they don't appear here.
///
/// Note: this is a deny list, not an allow list — property names
/// inside `properties` and `definitions` are arbitrary and must not
/// be filtered against schema-keyword vocabulary.
const STRIP_KEYS: &[&str] = &[
    "$schema",
    "$id",
    "$comment",
    "additionalProperties",
    "unevaluatedProperties",
    "examples",
    "default",
    "readOnly",
    "writeOnly",
    "deprecated",
    "const",
    "if",
    "then",
    "else",
    "not",
    "contentEncoding",
    "contentMediaType",
];

#[derive(Debug, thiserror::Error)]
pub enum TranslateError {
    #[error("tool '{tool}': unsupported combinator '{kw}' (Gemini accepts only anyOf)")]
    UnsupportedCombinator { tool: String, kw: &'static str },
    #[error("tool '{tool}': $ref '{reference}' could not be resolved (no matching definition)")]
    MissingDefinition { tool: String, reference: String },
    #[error("tool '{tool}': cyclic $ref expansion in schema")]
    CyclicRef { tool: String },
    #[error("tool '{tool}': unsupported $ref pointer '{reference}' (only #/definitions/* and #/$defs/* are inlined)")]
    UnsupportedRef { tool: String, reference: String },
}

/// Translate every descriptor and return the Gemini `tools` array shape:
/// `[{"functionDeclarations": [{name, description, parameters}, ...]}]`.
pub fn translate(descriptors: &[&'static ToolDescriptor]) -> Result<JsonValue, TranslateError> {
    let mut declarations: Vec<JsonValue> = Vec::with_capacity(descriptors.len());
    for d in descriptors {
        let parameters = translate_schema(d.name, (d.input_schema)())?;
        declarations.push(serde_json::json!({
            "name": d.name,
            "description": d.description,
            "parameters": parameters,
        }));
    }
    declarations.sort_by(|a, b| {
        a.get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .cmp(b.get("name").and_then(|n| n.as_str()).unwrap_or(""))
    });
    Ok(serde_json::json!([{ "functionDeclarations": declarations }]))
}

/// Translate one tool's schema. Public so the universal-coverage test
/// can pin individual offenders when something fails.
pub fn translate_schema(tool_name: &str, mut schema: JsonValue) -> Result<JsonValue, TranslateError> {
    let definitions = extract_definitions(&mut schema);
    inline_refs(tool_name, &mut schema, &definitions, 0)?;
    collapse_single_all_of(&mut schema);
    collapse_literal_enum_oneof(&mut schema);
    reject_combinators(tool_name, &schema)?;
    uppercase_types(&mut schema);
    strip_unsupported(&mut schema);
    Ok(schema)
}

/// Pull the `definitions` and `$defs` maps off the root and merge
/// them. `$defs` wins on collision (newer keyword).
fn extract_definitions(schema: &mut JsonValue) -> serde_json::Map<String, JsonValue> {
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
fn inline_refs(
    tool: &str,
    schema: &mut JsonValue,
    definitions: &serde_json::Map<String, JsonValue>,
    depth: usize,
) -> Result<(), TranslateError> {
    if depth > MAX_INLINE_DEPTH {
        return Err(TranslateError::CyclicRef {
            tool: tool.to_string(),
        });
    }
    match schema {
        JsonValue::Object(map) => {
            // If this object IS a $ref, replace it wholesale.
            if let Some(reference) = map.get("$ref").and_then(|v| v.as_str()).map(str::to_string) {
                let key = reference
                    .strip_prefix("#/definitions/")
                    .or_else(|| reference.strip_prefix("#/$defs/"))
                    .ok_or_else(|| TranslateError::UnsupportedRef {
                        tool: tool.to_string(),
                        reference: reference.clone(),
                    })?;
                let resolved = definitions.get(key).cloned().ok_or_else(|| {
                    TranslateError::MissingDefinition {
                        tool: tool.to_string(),
                        reference: reference.clone(),
                    }
                })?;
                *schema = resolved;
                inline_refs(tool, schema, definitions, depth + 1)?;
                return Ok(());
            }
            for (_, v) in map.iter_mut() {
                inline_refs(tool, v, definitions, depth)?;
            }
        }
        JsonValue::Array(arr) => {
            for v in arr.iter_mut() {
                inline_refs(tool, v, definitions, depth)?;
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
fn collapse_single_all_of(schema: &mut JsonValue) {
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
/// array. Gemini's OpenAPI subset rejects `oneOf` but accepts `enum`,
/// so we merge the variant strings into a single `enum` and drop the
/// per-variant descriptions (the function description is enough).
///
/// Conservative: the collapse only fires when EVERY branch matches
/// the literal-enum shape. Mixed-shape `oneOf`s (real variant
/// schemas) fall through to the reject pass.
fn collapse_literal_enum_oneof(schema: &mut JsonValue) {
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
            if let Some(values) = collapsed {
                if !values.is_empty() {
                    map.remove("oneOf");
                    map.insert("type".into(), JsonValue::String("string".into()));
                    map.insert("enum".into(), JsonValue::Array(values));
                }
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

/// Reject `oneOf` / `allOf` anywhere in the schema. `anyOf` is allowed
/// (Gemini's docs explicitly list it).
fn reject_combinators(tool: &str, schema: &JsonValue) -> Result<(), TranslateError> {
    match schema {
        JsonValue::Object(map) => {
            if map.contains_key("oneOf") {
                return Err(TranslateError::UnsupportedCombinator {
                    tool: tool.to_string(),
                    kw: "oneOf",
                });
            }
            if map.contains_key("allOf") {
                return Err(TranslateError::UnsupportedCombinator {
                    tool: tool.to_string(),
                    kw: "allOf",
                });
            }
            for (_, v) in map.iter() {
                reject_combinators(tool, v)?;
            }
        }
        JsonValue::Array(arr) => {
            for v in arr {
                reject_combinators(tool, v)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Recursively uppercase every `type` value. Schemars emits
/// `"string"`; Gemini accepts `"STRING"`. Type-as-array
/// (`["string", "null"]`, used by schemars for Option<T>) collapses to
/// the first non-null element.
fn uppercase_types(schema: &mut JsonValue) {
    match schema {
        JsonValue::Object(map) => {
            if let Some(t) = map.get_mut("type") {
                match t {
                    JsonValue::String(s) => {
                        *t = JsonValue::String(s.to_uppercase());
                    }
                    JsonValue::Array(arr) => {
                        let chosen = arr
                            .iter()
                            .find_map(|v| {
                                v.as_str()
                                    .filter(|s| *s != "null")
                                    .map(|s| s.to_uppercase())
                            })
                            .unwrap_or_else(|| "STRING".to_string());
                        *t = JsonValue::String(chosen);
                    }
                    _ => {}
                }
            }
            for (_, v) in map.iter_mut() {
                uppercase_types(v);
            }
        }
        JsonValue::Array(arr) => {
            for v in arr.iter_mut() {
                uppercase_types(v);
            }
        }
        _ => {}
    }
}

/// Strip every key in `STRIP_KEYS` from any object encountered. The
/// recursion walks into all values so nested schemas (under
/// `properties`, `items`, `anyOf`, etc.) are cleaned too. Property
/// names inside `properties` are arbitrary user-defined identifiers,
/// not schema keywords — the deny-list approach leaves them alone.
fn strip_unsupported(schema: &mut JsonValue) {
    match schema {
        JsonValue::Object(map) => {
            for k in STRIP_KEYS {
                map.remove(*k);
            }
            for (_, v) in map.iter_mut() {
                strip_unsupported(v);
            }
        }
        JsonValue::Array(arr) => {
            for v in arr.iter_mut() {
                strip_unsupported(v);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry;
    use serde_json::json;

    /// Universal coverage: every registered descriptor must translate
    /// without error. Catches schemars output that uses unsupported
    /// constructs (`oneOf`, recursive `$ref`s, etc.) before runtime.
    #[test]
    fn all_registry_tools_translate_cleanly() {
        let descriptors: Vec<&'static ToolDescriptor> = registry::all_descriptors().collect();
        let result = translate(&descriptors);
        match result {
            Ok(JsonValue::Array(arr)) => {
                assert_eq!(arr.len(), 1, "expected single tools-block element");
                let decls = arr[0]["functionDeclarations"].as_array().unwrap();
                assert!(decls.len() >= 70, "expected >=70 declarations, got {}", decls.len());
                // Spot-check the four enum-using tools survive inlining.
                let names: Vec<&str> = decls
                    .iter()
                    .map(|d| d.get("name").and_then(|v| v.as_str()).unwrap_or(""))
                    .collect();
                for known in ["git_branch", "gh_pr_merge", "define", "draft"] {
                    assert!(names.contains(&known), "missing {known}");
                }
            }
            Ok(other) => panic!("translator returned non-array root: {other:?}"),
            Err(e) => panic!("translator rejected at least one tool: {e}"),
        }
    }

    #[test]
    fn nested_one_of_rejected() {
        let schema = json!({
            "type": "object",
            "properties": {
                "value": {
                    "oneOf": [
                        {"type": "string"},
                        {"type": "integer"}
                    ]
                }
            }
        });
        let err = translate_schema("synthetic", schema).unwrap_err();
        assert!(matches!(
            err,
            TranslateError::UnsupportedCombinator { kw: "oneOf", .. }
        ));
    }

    #[test]
    fn nested_multi_element_all_of_rejected() {
        // Single-element allOf collapses cleanly (schemars's
        // description-with-ref idiom); multi-element allOf is real
        // schema intersection and not in Gemini's accepted subset.
        let schema = json!({
            "type": "object",
            "properties": {
                "value": {
                    "allOf": [
                        {"type": "string"},
                        {"minLength": 1}
                    ]
                }
            }
        });
        let err = translate_schema("synthetic", schema).unwrap_err();
        assert!(matches!(
            err,
            TranslateError::UnsupportedCombinator { kw: "allOf", .. }
        ));
    }

    #[test]
    fn any_of_passes_through() {
        let schema = json!({
            "type": "object",
            "properties": {
                "value": {
                    "anyOf": [
                        {"type": "string"},
                        {"type": "integer"}
                    ]
                }
            }
        });
        let out = translate_schema("synthetic", schema).unwrap();
        let any_of = out["properties"]["value"]["anyOf"].as_array().unwrap();
        assert_eq!(any_of.len(), 2);
        // Types uppercased inside anyOf branches.
        assert_eq!(any_of[0]["type"], "STRING");
        assert_eq!(any_of[1]["type"], "INTEGER");
    }

    #[test]
    fn ref_inline_simple_enum() {
        // Mirrors schemars's GitBranchAction emission shape.
        let schema = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "title": "GitBranchArgs",
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
        assert_eq!(out["type"], "OBJECT");
        // $ref inlined: action is now a string-with-enum schema.
        assert_eq!(out["properties"]["action"]["type"], "STRING");
        let variants = out["properties"]["action"]["enum"].as_array().unwrap();
        assert_eq!(variants.len(), 3);
        assert_eq!(variants[0], "List");
        // definitions stripped.
        assert!(out.get("definitions").is_none());
        // Unsupported root keys gone.
        assert!(out.get("$schema").is_none());
        // title is allowed (in ALLOWED_FIELDS) so it's preserved.
        assert_eq!(out["title"], "GitBranchArgs");
        // Required list survives.
        assert_eq!(out["required"][0], "path");
    }

    #[test]
    fn ref_to_defs_with_dollar_prefix_inlines() {
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
        assert_eq!(out["properties"]["x"]["type"], "INTEGER");
        assert!(out.get("$defs").is_none());
    }

    #[test]
    fn missing_definition_errors() {
        let schema = json!({
            "type": "object",
            "properties": {
                "x": { "$ref": "#/definitions/Missing" }
            }
        });
        let err = translate_schema("synthetic", schema).unwrap_err();
        assert!(matches!(err, TranslateError::MissingDefinition { .. }));
    }

    #[test]
    fn unsupported_ref_pointer_errors() {
        let schema = json!({
            "type": "object",
            "properties": {
                "x": { "$ref": "https://example.com/external" }
            }
        });
        let err = translate_schema("synthetic", schema).unwrap_err();
        assert!(matches!(err, TranslateError::UnsupportedRef { .. }));
    }

    #[test]
    fn type_array_with_null_collapses_to_first_concrete() {
        let schema = json!({
            "type": "object",
            "properties": {
                "x": { "type": ["string", "null"] }
            }
        });
        let out = translate_schema("synthetic", schema).unwrap();
        assert_eq!(out["properties"]["x"]["type"], "STRING");
    }

    #[test]
    fn single_element_all_of_with_description_collapses() {
        // Mirrors schemars's docs-with-ref idiom: parent has a
        // description, allOf has one element which (after inlining)
        // carries the actual schema.
        let schema = json!({
            "type": "object",
            "properties": {
                "action": {
                    "description": "get | save | delete",
                    "allOf": [{ "$ref": "#/definitions/DefineAction" }]
                }
            },
            "definitions": {
                "DefineAction": {
                    "type": "string",
                    "enum": ["get", "save", "delete"]
                }
            }
        });
        let out = translate_schema("synthetic", schema).unwrap();
        let action = &out["properties"]["action"];
        // allOf gone, content merged in.
        assert!(action.get("allOf").is_none());
        // Parent description preserved (parent wins on key collisions).
        assert_eq!(action["description"], "get | save | delete");
        // Inlined fields present.
        assert_eq!(action["type"], "STRING");
        assert_eq!(action["enum"][0], "get");
    }

    #[test]
    fn one_of_with_per_variant_descriptions_collapses_to_enum() {
        // Mirrors schemars's emission for unit-variant enums whose
        // variants carry doc comments.
        let schema = json!({
            "type": "object",
            "properties": {
                "action": {
                    "oneOf": [
                        {"description": "List branches", "type": "string", "enum": ["list"]},
                        {"description": "Create a branch", "type": "string", "enum": ["create"]},
                        {"description": "Delete a branch", "type": "string", "enum": ["delete"]}
                    ]
                }
            }
        });
        let out = translate_schema("synthetic", schema).unwrap();
        let action = &out["properties"]["action"];
        assert!(action.get("oneOf").is_none());
        assert_eq!(action["type"], "STRING");
        let variants = action["enum"].as_array().unwrap();
        assert_eq!(variants.len(), 3);
        assert_eq!(variants[0], "list");
        assert_eq!(variants[2], "delete");
    }

    #[test]
    fn unsupported_root_fields_stripped() {
        let schema = json!({
            "type": "object",
            "$schema": "http://json-schema.org/draft-07/schema#",
            "additionalProperties": false,
            "examples": [{}],
            "properties": {}
        });
        let out = translate_schema("synthetic", schema).unwrap();
        assert!(out.get("$schema").is_none());
        assert!(out.get("additionalProperties").is_none());
        assert!(out.get("examples").is_none());
        assert_eq!(out["type"], "OBJECT");
    }
}
