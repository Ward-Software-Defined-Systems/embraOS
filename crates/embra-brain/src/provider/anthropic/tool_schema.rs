//! Anthropic-specific tool manifest builder.
//!
//! Produces the `tools` array shape Anthropic's `/v1/messages` accepts:
//! `[{name, description, input_schema}, ...]` — sorted by name for
//! deterministic prompt-cache key stability and with `cache_control:
//! ephemeral` stamped on the alphabetically-last entry so the entire
//! tools block becomes a cache breakpoint.

use serde_json::json;

use crate::tools::registry::ToolDescriptor;

/// Build the Anthropic tools array from a slice of registry descriptors.
pub fn build_tools_snapshot(descriptors: &[&'static ToolDescriptor]) -> serde_json::Value {
    let mut tools: Vec<serde_json::Value> = descriptors
        .iter()
        .map(|d| {
            json!({
                "name": d.name,
                "description": d.description,
                "input_schema": (d.input_schema)(),
            })
        })
        .collect();
    // Sort by name so the array order is deterministic across builds —
    // keeps the prompt cache key stable even if inventory iteration
    // order shifts.
    tools.sort_by(|a, b| {
        a.get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .cmp(b.get("name").and_then(|n| n.as_str()).unwrap_or(""))
    });
    if let Some(last) = tools.last_mut() {
        if let Some(obj) = last.as_object_mut() {
            obj.insert("cache_control".into(), json!({"type": "ephemeral"}));
        }
    }
    serde_json::Value::Array(tools)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry;

    fn snapshot() -> Vec<serde_json::Value> {
        let descriptors: Vec<&'static ToolDescriptor> = registry::all_descriptors().collect();
        match build_tools_snapshot(&descriptors) {
            serde_json::Value::Array(v) => v,
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn tools_snapshot_is_sorted_and_last_has_cache_control() {
        let snapshot = snapshot();
        if snapshot.is_empty() {
            return;
        }
        // Sorted by name ascending.
        let names: Vec<&str> = snapshot
            .iter()
            .map(|t| t.get("name").and_then(|v| v.as_str()).unwrap_or(""))
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);

        // Last entry has cache_control.
        let last = snapshot.last().unwrap();
        assert_eq!(last["cache_control"]["type"], "ephemeral");
        if snapshot.len() > 1 {
            let earlier = &snapshot[snapshot.len() - 2];
            assert!(
                earlier.get("cache_control").is_none(),
                "only the last tool should carry cache_control"
            );
        }
    }

    /// Regression guard for Anthropic's explicit rejection of
    /// `oneOf`, `allOf`, or `anyOf` at the top level of `input_schema`
    /// (error: "tools.N.custom.input_schema: input_schema does not
    /// support oneOf, allOf, or anyOf at the top level"). schemars
    /// emits these when a tool args struct uses `#[serde(flatten)]`
    /// over a tagged enum — every args struct must deserialize to a
    /// plain object schema.
    #[test]
    fn every_tool_schema_is_plain_object_no_top_level_combinators() {
        let snapshot = snapshot();
        for tool in &snapshot {
            let name = tool
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown>");
            let schema = &tool["input_schema"];
            assert!(
                schema.get("oneOf").is_none(),
                "{name}: input_schema has top-level oneOf — Anthropic will 400 on this tool"
            );
            assert!(
                schema.get("allOf").is_none(),
                "{name}: input_schema has top-level allOf — Anthropic will 400 on this tool"
            );
            assert!(
                schema.get("anyOf").is_none(),
                "{name}: input_schema has top-level anyOf — Anthropic will 400 on this tool"
            );
            assert_eq!(
                schema["type"], "object",
                "{name}: input_schema root type must be \"object\""
            );
        }
    }

    #[test]
    fn tools_snapshot_is_nonempty_and_includes_known_tools() {
        let snapshot = snapshot();
        assert!(
            snapshot.len() >= 70,
            "registry should have >=70 tools, got {}",
            snapshot.len()
        );
        let names: Vec<&str> = snapshot
            .iter()
            .map(|t| t.get("name").and_then(|v| v.as_str()).unwrap_or(""))
            .collect();
        for known in [
            "system_status",
            "recall",
            "remember",
            "git_status",
            "cron_add",
            "knowledge_query",
        ] {
            assert!(
                names.contains(&known),
                "expected {} in tool snapshot",
                known
            );
        }
    }
}
