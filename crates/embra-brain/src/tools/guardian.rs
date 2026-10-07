//! embra-guardian-v1 meta-tools — the *only* surface the model sees for
//! dynamic tools. Three static `#[embra_tool]`s (`guardian_call`,
//! `guardian_list`, `guardian_propose`) registered at compile time
//! (so the provider tool snapshot stays byte-stable; dynamic tools are
//! NEVER injected into the schema — the prompt-cache invariant holds).
//! Backends live in `crate::guardian`.

use embra_tool_macro::embra_tool;
use embra_tools_core::DispatchError;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::tools::registry::DispatchContext;

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "guardian_list",
    description = "List the dynamically-defined Guardian tools available to call: name, description, declared capabilities, build status, and input schema. Call this before guardian_call to discover what dynamic tools exist."
)]
pub struct GuardianListArgs {}

impl GuardianListArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        crate::guardian::list_for_model(ctx.db)
            .await
            .map_err(DispatchError::Handler)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "guardian_call",
    is_side_effectful = true,
    description = "Invoke a Guardian-defined dynamic tool by name with a JSON input object (action=\"invoke\"), or check a tool's build status (action=\"status\"). Use guardian_list first to see available tools and their input schemas. A tool only runs once its status is \"ready\". Optional data_file: a path under /embra/workspace read host-side and injected as the input.data string before dispatch — the bridge for feeding files (e.g. knowledge_dump JSONL) to sandboxed tools, which cannot read the filesystem. Max 2 MiB; only valid with action=\"invoke\"; rejected if input.data is already set."
)]
#[serde(deny_unknown_fields)]
pub struct GuardianCallArgs {
    /// "invoke" to run the tool, or "status" to poll its build state.
    pub action: String,
    /// The dynamic tool's name (as shown by guardian_list).
    pub tool: String,
    /// The tool's input: a JSON object matching its input schema (see
    /// guardian_list), given as an object, not as a string of JSON. The
    /// tool's own fields go here, never beside action and tool. Used by
    /// action="invoke"; ignored by action="status".
    // The manifest declares an open object (`object_input_schema`) and no
    // default: an omitted input reaches the guest as the text `null`, and
    // `skip_serializing_if` keeps schemars from advertising that null.
    // The Rust type stays `Value`: `normalize_invoke_input` decides what
    // an input that is not an object means.
    #[serde(default)]
    #[schemars(
        schema_with = "object_input_schema",
        skip_serializing_if = "serde_json::Value::is_null"
    )]
    pub input: serde_json::Value,
    /// Optional path under /embra/workspace whose contents are read
    /// host-side and injected as the input.data string before dispatch —
    /// feeds files (e.g. knowledge_dump JSONL) to sandboxed tools, which
    /// cannot read the filesystem. Max 2 MiB; only valid with
    /// action="invoke"; rejected if input.data is already set.
    #[serde(default)]
    pub data_file: Option<String>,
}

/// `input`'s schema in the manifest: an object, open to any fields. A
/// declared type is what makes Ollama's Qwen3-Coder parser decode the
/// argument instead of keeping its text. No `properties` key: llama.cpp
/// compiles `"properties": {}` into a grammar that accepts only `{}`.
/// Gemini gets the property without a type (`untype_open_nested_objects`).
fn object_input_schema(_: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
    schemars::schema::SchemaObject {
        instance_type: Some(schemars::schema::InstanceType::Object.into()),
        ..Default::default()
    }
    .into()
}

/// Byte ceiling for `data_file` reads — 2 MiB, deliberately independent of
/// engineering's FILE_READ_MAX (which is derived from the dispatcher cap
/// minus framing headroom since sprint-6): this gate's binding constraint is
/// the guest arena, not the context window. Guest-side that's the 8 MiB bump
/// arena with a no-op dealloc — parse-heavy tools should stay near 1 MiB of
/// bridged data; this gate keeps the host side of the bridge sane.
const GUARDIAN_DATA_FILE_MAX: u64 = 2 * 1024 * 1024;

/// Pure gate for a data_file request: only action="invoke" may carry one,
/// and the path must resolve inside the workspace jail (the shared
/// resolver's uniform `Denied:` messages pass through).
fn validate_data_file_request(action: &str, path: &str) -> Result<String, String> {
    if action != "invoke" {
        return Err(format!(
            "guardian_call: data_file is only valid with action=\"invoke\" (got \"{action}\")"
        ));
    }
    crate::tools::engineering::resolve_workspace_path(path)
}

/// Size-gated read of an already-resolved data_file path. `max` is a
/// parameter so tests exercise the gate without multi-MiB fixtures.
async fn load_data_file(path: &str, max: u64) -> Result<String, String> {
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("guardian_call: data_file '{path}' is not readable: {e}"))?;
    if !meta.is_file() {
        return Err(format!(
            "guardian_call: data_file '{path}' is not a regular file"
        ));
    }
    if meta.len() > max {
        return Err(format!(
            "guardian_call: data_file '{path}' is {} bytes — exceeds the {max}-byte limit. \
             Dump slim/filtered instead (knowledge_dump include_payload=false + edge_types).",
            meta.len()
        ));
    }
    tokio::fs::read_to_string(path)
        .await
        .map_err(|e| format!("guardian_call: failed reading data_file '{path}': {e}"))
}

/// Inject file content as `input.data`. Omitted input (Null) upgrades to an
/// empty object; a non-object input or a pre-existing `data` key is an
/// error — the caller must pick one source for `data`.
fn inject_data_file_content(
    input: serde_json::Value,
    content: String,
) -> Result<serde_json::Value, String> {
    let mut input = if input.is_null() {
        serde_json::json!({})
    } else {
        input
    };
    let Some(obj) = input.as_object_mut() else {
        return Err("guardian_call: input must be a JSON object when data_file is set".into());
    };
    if obj.contains_key("data") {
        return Err(
            "guardian_call: input.data is already set — provide the content inline OR via data_file, not both"
                .into(),
        );
    }
    obj.insert("data".to_string(), serde_json::Value::String(content));
    Ok(input)
}

/// How many layers of JSON text an invoke's input may carry: an object
/// sent as a string, or as a string of that string (a model that quoted
/// its JSON inside an XML tool-call tag).
const INPUT_DECODE_LEVELS: usize = 2;

/// The input an invoke hands the guest. Every dynamic tool's
/// `GUARDIAN_SCHEMA` has an object root (the validator's
/// `normalize_schema`), so an input is an object or absent; an absent one
/// is null and reaches the guest as the text `null`. Some servers deliver
/// the object as JSON text: Ollama's Qwen3-Coder parser (parsers `qwen3.5`
/// and `glm-4.7`) keeps an argument as a string when its schema declares
/// no type, and a model can write one. A string that decodes to an object
/// becomes that object. Any other input can never be valid and is refused
/// rather than passed on. `status` ignores the input.
fn normalize_invoke_input(
    action: &str,
    input: serde_json::Value,
) -> Result<serde_json::Value, String> {
    use serde_json::Value;
    if action != "invoke" {
        return Ok(input);
    }
    let mut text = match input {
        Value::Null | Value::Object(_) => return Ok(input),
        Value::String(s) => s,
        Value::Bool(_) => return Err(input_is_not_an_object("a boolean")),
        Value::Number(_) => return Err(input_is_not_an_object("a number")),
        Value::Array(_) => return Err(input_is_not_an_object("an array")),
    };
    for _ in 0..INPUT_DECODE_LEVELS {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(Value::Null);
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(v @ (Value::Object(_) | Value::Null)) => return Ok(v),
            Ok(Value::String(inner)) => text = inner,
            _ => break,
        }
    }
    Err(input_is_not_an_object("a string that is not a JSON object"))
}

/// The refusal names the contract in its own words: an OpenAI-compatible
/// server never sees `is_error`, only this text.
fn input_is_not_an_object(got: &str) -> String {
    format!(
        "guardian_call: input must be a JSON object matching the tool's input schema \
         (see guardian_list); got {got}"
    )
}

impl GuardianCallArgs {
    pub async fn run(mut self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let arrived_as_text = self.input.is_string();
        self.input =
            normalize_invoke_input(&self.action, self.input).map_err(DispatchError::Handler)?;
        if arrived_as_text && self.input.is_object() {
            tracing::warn!(
                target: "guardian",
                tool = %self.tool,
                "guardian_call: input arrived as JSON text; decoded to an object"
            );
        }
        // An invoke's input is now an object or null. Without `data_file` a
        // null input reaches the guest as the text `null`, as it always has.
        if let Some(path) = self.data_file.as_deref() {
            let resolved =
                validate_data_file_request(&self.action, path).map_err(DispatchError::Handler)?;
            let content = load_data_file(&resolved, GUARDIAN_DATA_FILE_MAX)
                .await
                .map_err(DispatchError::Handler)?;
            self.input =
                inject_data_file_content(self.input, content).map_err(DispatchError::Handler)?;
        }
        crate::guardian::guardian_call(ctx.db, &self.action, &self.tool, self.input).await
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "guardian_propose",
    is_side_effectful = true,
    description = r##"Propose a new Guardian dynamic tool by drafting its full Rust module source. It does NOT run when you call this: it is statically validated, evaluated against your soul (the "replicant check"), and on a pass saved as a PROPOSAL the operator must approve before it compiles; a draft that conflicts with the soul is refused and never proposed. Use this only when a needed capability exists in neither the built-in tools nor the current guardian tools (call guardian_list first). Start from this exact skeleton and fill it in:

// guardian-tool: example_tool
// Paste only this shape (+ any private helper fns); the scaffold owns
// #![no_std], the allocator, the panic handler, the ABI, and the
// json/host/html_text helpers. The validator rejects: std::*, unsafe,
// extern/FFI, mod, pub free items, `use` outside core/alloc/json/host/
// html_text, include!/env!/asm!, third-party crates. vec!/format! are fine;
// run must never panic (a panic becomes a tool error).
const GUARDIAN_NAME: &str = "example_tool";  // == marker above; ^[a-z][a-z0-9_]{2,39}$
const GUARDIAN_DESC: &str = "What this tool does and when to call it.";  // 1..=600 chars
const GUARDIAN_SCHEMA: &str = r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#;  // valid JSON, object root, <=8 KiB
// const GUARDIAN_CAPS: &[&str] = &["http_get"];  // optional; "http_get" and/or "web_search"; omit for pure compute

fn run(input: &str) -> String {
    // `input` is the JSON args matching GUARDIAN_SCHEMA. Parse defensively,
    // do the work, return any String (JSON recommended).
    let args = match json::parse(input) {
        Ok(a) => a,
        Err(e) => return json::stringify(&json::obj(vec![("error", json::s(&e))])),
    };
    let text = args.get("text").as_str().unwrap_or("");
    // ...your logic here (with a declared cap, e.g. host::http_get("https://..."))...
    json::stringify(&json::obj(vec![("ok", json::b(true)), ("text", json::s(text))]))
}

Helpers in run (no `use` needed): json::{parse,stringify,obj,arr,s,n,b,null}, Json::{get,idx,as_str,as_f64,as_bool,as_array}; with a declared cap, host::http_get(url) and host::web_search(query) each return a JSON envelope string. After a successful proposal, tell the operator to review with /guardian show <name> then /guardian approve <name> (or /guardian reject <name>); it will NOT run until approved. Provide the full module as the source argument."##
)]
pub struct GuardianProposeArgs {
    /// The complete Guardian module source (marker + GUARDIAN_* consts + fn run).
    pub source: String,
}

impl GuardianProposeArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        crate::guardian::propose(ctx.db, ctx.config, &self.source).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_deserialize() {
        let _: GuardianListArgs = serde_json::from_value(serde_json::json!({})).unwrap();
        let c: GuardianCallArgs = serde_json::from_value(serde_json::json!({
            "action": "invoke", "tool": "web_search", "input": {"q": "x"}
        }))
        .unwrap();
        assert_eq!(c.action, "invoke");
        assert_eq!(c.tool, "web_search");
        // input defaults when omitted
        let c2: GuardianCallArgs = serde_json::from_value(serde_json::json!({
            "action": "status", "tool": "web_search"
        }))
        .unwrap();
        assert!(c2.input.is_null());
    }

    #[test]
    fn propose_args_deserialize() {
        let p: GuardianProposeArgs = serde_json::from_value(serde_json::json!({
            "source": "// guardian-tool: t\nfn run(i:&str)->String{String::new()}"
        }))
        .unwrap();
        assert!(p.source.starts_with("// guardian-tool:"));
    }

    #[test]
    fn meta_tools_registered() {
        let names: Vec<&str> = crate::tools::registry::all_descriptors()
            .map(|d| d.name)
            .collect();
        assert!(names.contains(&"guardian_list"));
        assert!(names.contains(&"guardian_call"));
        assert!(names.contains(&"guardian_propose"));
    }

    #[test]
    fn guardian_call_data_file_deserializes_and_defaults_none() {
        let c: GuardianCallArgs = serde_json::from_value(serde_json::json!({
            "action": "invoke", "tool": "kg_scan",
            "data_file": "/embra/workspace/KG_DUMPS/kg-dump-x.jsonl",
            "input": {"action": "scan"}
        }))
        .unwrap();
        assert_eq!(
            c.data_file.as_deref(),
            Some("/embra/workspace/KG_DUMPS/kg-dump-x.jsonl")
        );
        // Absent → None, so existing guardian_call shapes are unchanged.
        let c2: GuardianCallArgs = serde_json::from_value(serde_json::json!({
            "action": "invoke", "tool": "kg_scan"
        }))
        .unwrap();
        assert!(c2.data_file.is_none());
    }

    #[test]
    fn data_file_requires_invoke_action() {
        let err = validate_data_file_request("status", "KG_DUMPS/x.jsonl").unwrap_err();
        assert!(err.contains("invoke"), "{err}");
    }

    #[test]
    fn data_file_rejects_workspace_escape() {
        // Outside the jail entirely.
        let err = validate_data_file_request("invoke", "/etc/passwd").unwrap_err();
        assert!(err.starts_with("Denied:"), "{err}");
        // Traversal in relative and absolute form.
        let err = validate_data_file_request("invoke", "../x").unwrap_err();
        assert!(err.contains(".."), "{err}");
        let err =
            validate_data_file_request("invoke", "/embra/workspace/../etc/passwd").unwrap_err();
        assert!(err.contains(".."), "{err}");
        // In-jail paths resolve, in both the absolute and relative forms the
        // shared resolver accepts.
        assert_eq!(
            validate_data_file_request("invoke", "/embra/workspace/KG_DUMPS/a.jsonl").unwrap(),
            "/embra/workspace/KG_DUMPS/a.jsonl"
        );
        assert_eq!(
            validate_data_file_request("invoke", "KG_DUMPS/a.jsonl").unwrap(),
            "/embra/workspace/KG_DUMPS/a.jsonl"
        );
    }

    #[test]
    fn inject_data_file_replaces_null_input_and_sets_data() {
        // Omitted input deserializes to Null; the bridge upgrades it to {}.
        let out = inject_data_file_content(serde_json::Value::Null, "l1\nl2".into()).unwrap();
        assert_eq!(out, serde_json::json!({"data": "l1\nl2"}));
        // Sibling fields survive injection.
        let out =
            inject_data_file_content(serde_json::json!({"action": "scan"}), "x".into()).unwrap();
        assert_eq!(out, serde_json::json!({"action": "scan", "data": "x"}));
    }

    #[test]
    fn inject_data_file_requires_object_input() {
        assert!(inject_data_file_content(serde_json::json!(5), "x".into()).is_err());
        assert!(inject_data_file_content(serde_json::json!("s"), "x".into()).is_err());
        assert!(inject_data_file_content(serde_json::json!([1]), "x".into()).is_err());
    }

    #[test]
    fn inject_data_file_rejects_preexisting_data_key() {
        let err = inject_data_file_content(serde_json::json!({"data": "inline"}), "x".into())
            .unwrap_err();
        assert!(err.contains("data_file"), "{err}");
    }

    #[tokio::test]
    async fn load_data_file_reads_happy_path() {
        let path =
            std::env::temp_dir().join(format!("guardian_data_file_{}.jsonl", std::process::id()));
        tokio::fs::write(&path, "{\"type\":\"node\"}\n").await.unwrap();
        let content = load_data_file(path.to_str().unwrap(), GUARDIAN_DATA_FILE_MAX)
            .await
            .unwrap();
        assert_eq!(content, "{\"type\":\"node\"}\n");
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn load_data_file_rejects_oversize_and_missing() {
        let path = std::env::temp_dir()
            .join(format!("guardian_data_file_big_{}.jsonl", std::process::id()));
        tokio::fs::write(&path, "0123456789").await.unwrap();
        let err = load_data_file(path.to_str().unwrap(), 4).await.unwrap_err();
        assert!(err.contains("10 bytes"), "{err}");
        let _ = tokio::fs::remove_file(&path).await;

        let err = load_data_file("/nonexistent/definitely_missing.jsonl", 100)
            .await
            .unwrap_err();
        assert!(err.contains("not readable"), "{err}");
    }

    #[test]
    fn propose_description_embeds_the_validated_template() {
        // The skeleton shown to the model must be the exact one the validator
        // accepts (embra_guardian::GUARDIAN_TEMPLATE), so we never teach it a
        // shape the gate rejects. Guards against the description and the const
        // drifting apart.
        let desc = crate::tools::registry::all_descriptors()
            .find(|d| d.name == "guardian_propose")
            .expect("guardian_propose registered")
            .description;
        assert!(
            desc.contains(embra_guardian::GUARDIAN_TEMPLATE),
            "guardian_propose description must embed GUARDIAN_TEMPLATE verbatim"
        );
    }

    #[test]
    fn an_object_input_passes_unchanged() {
        for input in [
            serde_json::json!({}),
            serde_json::json!({"query": "x", "opts": {"n": 3, "tags": ["a"]}}),
        ] {
            assert_eq!(normalize_invoke_input("invoke", input.clone()).unwrap(), input);
        }
    }

    #[test]
    fn a_json_object_sent_as_a_string_reaches_the_tool_as_an_object() {
        for text in ["{\"query\":\"x\"}", "  {\"query\": \"x\"}\n"] {
            assert_eq!(
                normalize_invoke_input("invoke", serde_json::json!(text)).unwrap(),
                serde_json::json!({"query": "x"})
            );
        }
    }

    #[test]
    fn an_object_encoded_twice_is_decoded_and_no_further() {
        let once = serde_json::json!({"query": "x"}).to_string();
        let twice = serde_json::Value::String(once).to_string();
        let thrice = serde_json::Value::String(twice.clone()).to_string();
        assert_eq!(
            normalize_invoke_input("invoke", serde_json::json!(twice)).unwrap(),
            serde_json::json!({"query": "x"})
        );
        let err = normalize_invoke_input("invoke", serde_json::json!(thrice)).unwrap_err();
        assert!(err.contains("must be a JSON object"), "{err}");
    }

    #[test]
    fn a_null_input_still_reaches_the_tool_as_null() {
        for input in [
            serde_json::Value::Null,
            serde_json::json!("null"),
            serde_json::json!(""),
            serde_json::json!("  "),
        ] {
            let out = normalize_invoke_input("invoke", input.clone()).unwrap();
            assert!(out.is_null(), "{input} gave {out}");
        }
    }

    #[test]
    fn an_input_that_is_not_an_object_is_refused() {
        for input in [
            serde_json::json!(5),
            serde_json::json!(true),
            serde_json::json!([1]),
            serde_json::json!("embraOS"),
            serde_json::json!("[1]"),
            serde_json::json!("5"),
        ] {
            let err = normalize_invoke_input("invoke", input.clone()).unwrap_err();
            assert!(
                err.starts_with("guardian_call: input must be a JSON object")
                    && err.contains("guardian_list"),
                "{input}: {err}"
            );
        }
    }

    #[test]
    fn a_status_call_keeps_its_input_as_given() {
        for input in [
            serde_json::json!(5),
            serde_json::json!("{\"a\":1}"),
            serde_json::json!([1]),
        ] {
            assert_eq!(normalize_invoke_input("status", input.clone()).unwrap(), input);
        }
    }

    #[test]
    fn a_stringified_input_takes_data_file_content() {
        let input =
            normalize_invoke_input("invoke", serde_json::json!("{\"action\":\"scan\"}")).unwrap();
        let out = inject_data_file_content(input, "x".into()).unwrap();
        assert_eq!(out, serde_json::json!({"action": "scan", "data": "x"}));
    }

    /// The manifest tells every provider that `input` is an object, the
    /// type Ollama's parser needs to decode the argument. It carries no
    /// default and is not required, and the root refuses an argument it
    /// does not know.
    #[test]
    fn the_guardian_input_is_declared_an_object_without_a_default() {
        let d = crate::tools::registry::all_descriptors()
            .find(|d| d.name == "guardian_call")
            .expect("guardian_call registered");
        let schema = (d.input_schema)();
        let input = &schema["properties"]["input"];
        assert_eq!(input["type"], "object", "{input}");
        assert!(input.get("default").is_none() && input.get("properties").is_none(), "{input}");
        let description = input["description"].as_str().unwrap_or_default();
        assert!(description.contains("not as a string of JSON"), "{description}");
        assert_eq!(schema["required"], serde_json::json!(["action", "tool"]));
        assert_eq!(schema["additionalProperties"], false);
    }

    /// A flattened call puts the tool's fields beside action and tool.
    /// They were dropped and the guest got `null`, which a tool with a
    /// required field answers as if the model had given nothing. Now the
    /// call is refused and the field is named.
    #[test]
    fn an_argument_guardian_call_does_not_know_is_refused() {
        let err = serde_json::from_value::<GuardianCallArgs>(serde_json::json!({
            "action": "invoke", "tool": "web_search", "query": "embraOS"
        }))
        .unwrap_err();
        assert!(err.to_string().contains("unknown field `query`"), "{err}");
    }

    /// The committed probe module (`embra-guardian/examples/gen_fixture.rs`):
    /// `{a, b, url?}` -> `{sum, fetched}`.
    const PROBE_WASM: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../embra-guardian/tests/fixtures/probe.wasm"
    ));
    /// The probe's name in the process-wide overlay; no other test uses it.
    const PROBE: &str = "probe_input_shapes";

    /// Invoke the probe through the registry, the way the turn loop and the
    /// cron loop reach `guardian_call`, and return the sum it computed.
    async fn probe_sum(input: serde_json::Value) -> Result<f64, DispatchError> {
        let rt = embra_guardian::overlay::init("test").expect("guardian runtime");
        rt.compile_insert(PROBE, "probe", serde_json::json!({"type": "object"}), Vec::new(), PROBE_WASM)
            .expect("the probe compiles");
        let config: crate::config::SystemConfig = serde_json::from_value(serde_json::json!({
            "name": "Embra", "api_key": "k", "timezone": "UTC", "deployment_mode": "phase1",
            "created_at": "", "version": "test", "kg_temporal_window_secs": 1800,
            "kg_max_traversal_depth": 3, "kg_traversal_depth_ceiling": 5,
            "kg_edge_candidate_limit": 50, "api_provider": "anthropic",
        }))
        .unwrap();
        let db = crate::db::WardsonDbClient::from_url("http://127.0.0.1:1");
        let trace = embra_tools_core::new_turn_trace_handle();
        let ctx = DispatchContext {
            db: &db,
            config: &config,
            session_name: "test",
            config_tz: "UTC",
            trace: &trace,
            turn_index: 0,
        };
        let args = serde_json::json!({"action": "invoke", "tool": PROBE, "input": input});
        let out = crate::tools::registry::dispatch("guardian_call", args, ctx).await?;
        let v: serde_json::Value = serde_json::from_str(&out.text).expect("the probe answers JSON");
        Ok(v["sum"].as_f64().expect("the probe answers a sum"))
    }

    /// The bug, end to end. An OpenAI-compatible server that keeps an
    /// argument without a declared type as text (Ollama's Qwen3-Coder
    /// parser) hands guardian_call the object as JSON text. It reached the
    /// guest as a JSON string, the probe found neither field, and `sum 0`
    /// came back as a success.
    #[tokio::test]
    async fn a_stringified_input_reaches_the_guest_as_an_object() {
        let object = serde_json::json!({"a": 2, "b": 40});
        assert_eq!(probe_sum(object.clone()).await.unwrap(), 42.0);
        assert_eq!(probe_sum(serde_json::Value::String(object.to_string())).await.unwrap(), 42.0);
        let refused = probe_sum(serde_json::json!([1])).await.unwrap_err();
        assert!(
            matches!(&refused, DispatchError::Handler(m) if m.contains("must be a JSON object")),
            "{refused:?}"
        );
    }
}
