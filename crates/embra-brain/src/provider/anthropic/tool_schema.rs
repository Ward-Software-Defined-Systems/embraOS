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
    if let Some(last) = tools.last_mut()
        && let Some(obj) = last.as_object_mut()
    {
        obj.insert("cache_control".into(), json!({"type": "ephemeral"}));
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

    fn sha256_hex(s: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(s.as_bytes());
        format!("{:x}", h.finalize())
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

    /// Byte-stability tripwire for the tool manifest. The serialized
    /// snapshot is part of the prompt-cache key on every turn, so a moved
    /// byte — a reworded description, a doc comment on a `JsonSchema`
    /// field, a `schemars`/`serde_json` bump that reorders or reshapes a
    /// schema — is a cache event for every instance. If the hash changes
    /// unintentionally, fix the change, not the pin. Re-pinning is the
    /// deliberate act that records a tool-surface change; the count moves
    /// with `CATEGORY_COUNTS`.
    ///
    /// Every re-pin is recorded in `docs/CHANGE-LOG.md`, with the proof that
    /// only the intended bytes moved: each provider's manifest written out
    /// before and after the change and compared tool by tool.
    #[test]
    fn tools_snapshot_bytes_are_frozen() {
        let snapshot = snapshot();
        assert_eq!(snapshot.len(), 117, "registered tool count moved");

        let canonical = serde_json::to_string(&serde_json::Value::Array(snapshot.clone()))
            .expect("snapshot serializes");
        // Per-tool digests localize a failure: diff this list against the
        // same output from the last green commit.
        let per_tool: Vec<String> = snapshot
            .iter()
            .map(|t| {
                let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("<unknown>");
                let body = serde_json::to_string(t).unwrap_or_default();
                format!("{name} {}", &sha256_hex(&body)[..12])
            })
            .collect();
        assert_eq!(
            sha256_hex(&canonical),
            "3d68d01f84e502bda12c181f042e673d4c3cd5afff042d0b40bc8315102d567f",
            "TOOL MANIFEST BYTES MOVED. Do not update the pinned hash unless \
             the tool surface was changed on purpose.\n---\n{}",
            per_tool.join("\n")
        );
    }

    /// Writes the proof a re-pin records: each provider's manifest as the
    /// provider sends it, one `name<TAB>json` line per tool, then the
    /// SHA-256 of the whole manifest (Anthropic's equals the pin). Write
    /// it before and after a change and diff the two directories:
    ///
    /// `EMBRA_MANIFEST_DUMP_DIR=<dir> cargo test -p embra-brain the_three_manifests -- --ignored`
    #[test]
    #[ignore = "writes files; run by hand for a re-pin proof"]
    fn the_three_manifests_can_be_written_out_for_a_re_pin_proof() {
        let dir = std::env::var("EMBRA_MANIFEST_DUMP_DIR")
            .expect("EMBRA_MANIFEST_DUMP_DIR names the directory to write into");
        std::fs::create_dir_all(&dir).expect("create the dump directory");
        let descriptors: Vec<&'static ToolDescriptor> = registry::all_descriptors().collect();
        // (file, manifest, pointer to its list of tools, pointer to a tool's name)
        let manifests = [
            ("anthropic", build_tools_snapshot(&descriptors), "", "/name"),
            (
                "gemini",
                crate::provider::gemini::tool_schema::translate(&descriptors)
                    .expect("the Gemini translator accepts every tool"),
                "/0/functionDeclarations",
                "/name",
            ),
            (
                "openai_compat",
                crate::provider::openai_compat::tool_schema::translate(&descriptors)
                    .expect("the OpenAI-compat translator accepts every tool"),
                "",
                "/function/name",
            ),
        ];
        for (provider, manifest, list_at, name_at) in manifests {
            let tools = manifest
                .pointer(list_at)
                .and_then(|v| v.as_array())
                .expect("a list of tools");
            let mut out = String::new();
            for tool in tools {
                let name = tool
                    .pointer(name_at)
                    .and_then(|v| v.as_str())
                    .expect("a tool name");
                out.push_str(&format!("{name}\t{tool}\n"));
            }
            let whole = serde_json::to_string(&manifest).expect("the manifest serializes");
            out.push_str(&format!("sha256\t{}\n", sha256_hex(&whole)));
            std::fs::write(format!("{dir}/{provider}.tsv"), out).expect("write the dump");
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
