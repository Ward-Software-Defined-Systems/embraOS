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
    fn both_meta_tools_registered() {
        let names: Vec<&str> = crate::tools::registry::all_descriptors()
            .map(|d| d.name)
            .collect();
        assert!(names.contains(&"guardian_list"));
        assert!(names.contains(&"guardian_call"));
    }
}
