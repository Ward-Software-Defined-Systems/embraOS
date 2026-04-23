//! Typed tool registry — foundation for NATIVE-TOOLS-01 native tool-use.
//!
//! Each tool declares a typed args struct annotated with
//! `#[embra_tool(name = "...", description = "...")]`. The macro emits
//! `inventory::submit!` targeting the [`ToolDescriptor`] type defined here.
//! At first access, [`REGISTRY`] collects every submission into a
//! `HashMap<&'static str, &'static ToolDescriptor>` for O(1) lookup.
//!
//! Stage 2 of the migration populates the registry in parallel with the
//! legacy string dispatcher at `tools/mod.rs`. Stage 3 removes the legacy
//! dispatcher and makes [`dispatch`] the single entry point.

use embra_tools_core::{BoxFut, DispatchError, JsonValue};
use once_cell::sync::Lazy;
use std::collections::HashMap;

use crate::config::SystemConfig;
use crate::db::WardsonDbClient;

/// Runtime context passed to every tool handler.
///
/// Replaces the `(db, config, session_name)` tuple threaded through the
/// legacy string dispatcher at `tools/mod.rs:34-180`. `config_tz` is hoisted
/// so tools that need the timezone don't have to re-derive it from `config`.
pub struct DispatchContext<'a> {
    pub db: &'a WardsonDbClient,
    pub config: &'a SystemConfig,
    pub session_name: &'a str,
    pub config_tz: &'a str,
}

/// Inventory-collected tool descriptor.
///
/// Populated by the `#[embra_tool]` attribute macro at compile time via
/// `inventory::submit!`. The macro emission sits in a `const _: () = {};`
/// block next to each args struct and pays no runtime cost beyond static
/// data — the map build in [`REGISTRY`] is `O(n)` over the descriptor count
/// and runs once per process.
pub struct ToolDescriptor {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: fn() -> serde_json::Value,
    pub handler: for<'a> fn(JsonValue, DispatchContext<'a>)
        -> BoxFut<'a, Result<String, DispatchError>>,
}

inventory::collect!(ToolDescriptor);

/// Global tool registry.
///
/// Built lazily on first access from the inventory iterator. Subsequent
/// lookups are O(1). The map takes a `&'static ToolDescriptor`, which lives
/// as long as the process.
pub static REGISTRY: Lazy<HashMap<&'static str, &'static ToolDescriptor>> = Lazy::new(|| {
    inventory::iter::<ToolDescriptor>()
        .into_iter()
        .map(|d| (d.name, d))
        .collect()
});

pub fn tool_count() -> usize {
    REGISTRY.len()
}

pub fn all_descriptors() -> impl Iterator<Item = &'static ToolDescriptor> {
    REGISTRY.values().copied()
}

const MAX_TOOL_RESULT_SIZE: usize = 2_097_152;

fn apply_max_size(s: String) -> String {
    if s.len() <= MAX_TOOL_RESULT_SIZE {
        return s;
    }
    let mut end = MAX_TOOL_RESULT_SIZE;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...\n[truncated: {} bytes total]", &s[..end], s.len())
}

/// Native-tool-use dispatcher.
///
/// Looks up `name` in [`REGISTRY`], runs the handler with the typed
/// context, and applies the 2 MiB result cap that matched the legacy
/// dispatcher's behavior. Stage 3 wires this into the gRPC dispatch loop.
pub async fn dispatch(
    name: &str,
    input: JsonValue,
    ctx: DispatchContext<'_>,
) -> Result<String, DispatchError> {
    let Some(desc) = REGISTRY.get(name) else {
        return Err(DispatchError::Unknown(name.into()));
    };
    let raw = (desc.handler)(input, ctx).await?;
    Ok(apply_max_size(raw))
}

/// Write the current tool registry snapshot to WardSONDB's `tools.registry`
/// collection. Idempotent — overwrites any previous snapshot on every boot.
/// Replaces the old Learning-Mode Phase 4 placeholder doc with the full
/// macro-generated schema set (locked decision in NATIVE-TOOLS-01).
///
/// Called once from `main.rs` immediately after `run_migrations` completes
/// and before the gRPC server accepts connections.
pub async fn write_snapshot(db: &crate::db::WardsonDbClient) -> anyhow::Result<()> {
    use anyhow::Context;

    let tools: Vec<serde_json::Value> = all_descriptors()
        .map(|d| {
            serde_json::json!({
                "name": d.name,
                "description": d.description,
                "input_schema": (d.input_schema)(),
            })
        })
        .collect();

    let snapshot = serde_json::json!({
        "_id": "registry",
        "format_version": 2,
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "tool_count": tools.len(),
        "tools": tools,
    });

    if !db
        .collection_exists("tools.registry")
        .await
        .unwrap_or(false)
    {
        db.create_collection("tools.registry")
            .await
            .context("create tools.registry collection")?;
    }

    // Try update first (well-known _id "registry"), fall back to write for
    // first-ever boot. WardSONDB's update is idempotent-ish: replaces doc.
    match db
        .update("tools.registry", "registry", &snapshot)
        .await
    {
        Ok(_) => Ok(()),
        Err(_) => {
            db.write("tools.registry", &snapshot)
                .await
                .context("write tools.registry snapshot")?;
            Ok(())
        }
    }
}
