//! embra-guardian-v1 meta-tools — the *only* surface the model sees for
//! dynamic tools. Two static `#[embra_tool]`s registered at compile time
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
pub struct GuardianCallArgs {
    /// "invoke" to run the tool, or "status" to poll its build state.
    pub action: String,
    /// The dynamic tool's name (as shown by guardian_list).
    pub tool: String,
    /// JSON input object for the tool. Used by action="invoke"; ignored
    /// by action="status". Defaults to an empty object.
    #[serde(default)]
    pub input: serde_json::Value,
    /// Optional path under /embra/workspace whose contents are read
    /// host-side and injected as the input.data string before dispatch —
    /// feeds files (e.g. knowledge_dump JSONL) to sandboxed tools, which
    /// cannot read the filesystem. Max 2 MiB; only valid with
    /// action="invoke"; rejected if input.data is already set.
    #[serde(default)]
    pub data_file: Option<String>,
}

/// Byte ceiling for `data_file` reads (matches engineering's FILE_READ_MAX).
/// The binding constraint guest-side is the 8 MiB bump arena with a no-op
/// dealloc — parse-heavy tools should stay near 1 MiB of bridged data; this
/// gate keeps the host side of the bridge sane.
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

impl GuardianCallArgs {
    pub async fn run(mut self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
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
}
