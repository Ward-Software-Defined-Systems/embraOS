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
    description = "Invoke a Guardian-defined dynamic tool by name with a JSON input object (action=\"invoke\"), or check a tool's build status (action=\"status\"). Use guardian_list first to see available tools and their input schemas. A tool only runs once its status is \"ready\"."
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
}

impl GuardianCallArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
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
