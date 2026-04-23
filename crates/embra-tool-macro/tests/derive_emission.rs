//! Integration test for the `#[embra_tool]` attribute macro.
//!
//! Defines a local `tools::registry` module that mirrors the path the macro
//! emits (`crate::tools::registry::ToolDescriptor`). Applies the macro to a
//! test args struct, then verifies registry emission, schema shape, and
//! handler round-trip behavior.

use embra_tool_macro::embra_tool;
use embra_tools_core::{BoxFut, DispatchError, JsonValue};

mod tools {
    pub mod registry {
        use embra_tools_core::{BoxFut, DispatchError, JsonValue};

        pub struct ToolDescriptor {
            pub name: &'static str,
            pub description: &'static str,
            pub is_side_effectful: bool,
            pub input_schema: fn() -> serde_json::Value,
            pub handler: for<'a> fn(JsonValue, DispatchContext<'a>)
                -> BoxFut<'a, Result<String, DispatchError>>,
        }

        inventory::collect!(ToolDescriptor);

        pub struct DispatchContext<'a> {
            pub token: &'a str,
        }
    }
}

use tools::registry::{DispatchContext, ToolDescriptor};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[embra_tool(name = "test_tool", description = "A tool for macro tests")]
pub struct TestToolArgs {
    /// Required free-text query.
    pub query: String,
    /// Optional numeric limit.
    #[serde(default)]
    pub limit: Option<u32>,
}

impl TestToolArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(format!(
            "{}:{}:{}",
            ctx.token,
            self.query,
            self.limit.unwrap_or(0)
        ))
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[embra_tool(
    name = "test_writer",
    description = "Writer tool fixture for is_side_effectful coverage",
    is_side_effectful = true
)]
pub struct TestWriterArgs {}

impl TestWriterArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok("wrote".into())
    }
}

fn find_test_descriptor() -> &'static ToolDescriptor {
    inventory::iter::<ToolDescriptor>()
        .into_iter()
        .find(|d| d.name == "test_tool")
        .expect("test_tool descriptor should be registered via #[embra_tool]")
}

#[test]
fn descriptor_registered_in_inventory() {
    let d = find_test_descriptor();
    assert_eq!(d.name, "test_tool");
    assert_eq!(d.description, "A tool for macro tests");
    // Default for `is_side_effectful` when the keyword is omitted.
    assert!(!d.is_side_effectful);
}

#[test]
fn schema_emission_is_well_formed() {
    let d = find_test_descriptor();
    let schema = (d.input_schema)();
    assert_eq!(schema["type"], "object");
    let props = &schema["properties"];
    assert_eq!(props["query"]["type"], "string");
    assert!(props.get("limit").is_some());
}

#[tokio::test]
async fn handler_round_trips_valid_input() {
    let d = find_test_descriptor();
    let ctx = DispatchContext { token: "ok" };
    let result = (d.handler)(
        serde_json::json!({"query": "hello", "limit": 3}),
        ctx,
    )
    .await;
    assert_eq!(result.unwrap(), "ok:hello:3");
}

#[tokio::test]
async fn handler_rejects_malformed_input() {
    let d = find_test_descriptor();
    let ctx = DispatchContext { token: "x" };
    let result = (d.handler)(serde_json::json!({"limit": 3}), ctx).await;
    match result {
        Err(DispatchError::BadInput { tool, .. }) => assert_eq!(tool, "test_tool"),
        other => panic!("expected BadInput, got {other:?}"),
    }
}

#[test]
fn writer_descriptor_marks_is_side_effectful() {
    let d = inventory::iter::<ToolDescriptor>()
        .into_iter()
        .find(|d| d.name == "test_writer")
        .expect("test_writer descriptor should be registered");
    assert!(d.is_side_effectful);
}

#[test]
fn unused_import_prevention() {
    // Silence "unused import" warnings for type aliases we re-export from
    // embra_tools_core for readability in the test file scope.
    let _: Option<BoxFut<'_, ()>> = None;
    let _: Option<JsonValue> = None;
}
