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
    description = "Propose a new Guardian dynamic tool by drafting its complete Rust module source. It does NOT run when you call this: it is statically validated, then evaluated against your soul (the \"replicant check\"). Only if it passes is it saved as a PROPOSAL for the human operator to approve; if it conflicts with your soul it is refused and never proposed. Use this only when a needed capability exists in neither the built-in tools nor the current guardian tools (call guardian_list first). After a successful proposal, tell the operator to review with /guardian show <name> and approve with /guardian approve <name> (or reject with /guardian reject <name>); it will NOT run until approved. Module contract (fix and re-propose if validation fails): (1) first line is the marker // guardian-tool: <name>; (2) const GUARDIAN_NAME: &str equals the marker, 3-40 chars, lowercase-first, [a-z0-9_] only, not colliding with an existing tool; (3) const GUARDIAN_DESC: &str, 1-600 chars; (4) const GUARDIAN_SCHEMA: &str, valid JSON, object root with \"type\":\"object\", <= 8192 bytes; (5) optional const GUARDIAN_CAPS: &[&str] = &[\"http_get\"]; only http_get and web_search are allowed, omit for pure compute; (6) exactly one fn run(input: &str) -> String (not pub, not unsafe), where input is the JSON arguments string. Guest constraints (no_std): NO std::{fs,net,process,os,env,thread}, NO unsafe, NO FFI/extern/mod, NO extra-crate use (only core, alloc, and scaffold helpers json/host/html_text), NO include!/env!/asm! macros, NO ABI attributes. Host-mediated capabilities: with http_get call host::http_get(url: &str) -> String (https-only, SSRF-blocked); with web_search call host::web_search(query: &str) -> String. Provide the full module as the source argument."
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
}
