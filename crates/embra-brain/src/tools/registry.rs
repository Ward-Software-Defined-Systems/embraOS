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

use embra_tools_core::{BoxFut, DispatchError, JsonValue, ToolOutput, TurnTraceHandle};
use once_cell::sync::Lazy;
use std::collections::HashMap;

use crate::config::SystemConfig;
use crate::db::WardsonDbClient;

/// Runtime context passed to every tool handler.
///
/// Replaces the `(db, config, session_name)` tuple threaded through the
/// legacy string dispatcher at `tools/mod.rs:34-180`. `config_tz` is hoisted
/// so tools that need the timezone don't have to re-derive it from `config`.
///
/// `trace` + `turn_index` (NATIVE-TOOLS-01 follow-up) expose the
/// current turn's in-memory tool-call trace so tools like `turn_trace` can
/// introspect what the model has done this turn without round-tripping
/// through session history.
pub struct DispatchContext<'a> {
    pub db: &'a WardsonDbClient,
    pub config: &'a SystemConfig,
    pub session_name: &'a str,
    pub config_tz: &'a str,
    pub trace: &'a TurnTraceHandle,
    pub turn_index: usize,
}

/// Inventory-collected tool descriptor.
///
/// Populated by the `#[embra_tool]` attribute macro at compile time via
/// `inventory::submit!`. The macro emission sits in a `const _: () = {};`
/// block next to each args struct and pays no runtime cost beyond static
/// data — the map build in [`REGISTRY`] is `O(n)` over the descriptor count
/// and runs once per process.
///
/// `is_side_effectful` classifies writer tools (`remember`, `git_commit`,
/// `file_write`, etc.) separately from pure readers. The empty-text-turn
/// defense in `grpc_service.rs` consults this to decide whether a silent
/// end-turn after tool use is worth surfacing as a diagnostic.
pub struct ToolDescriptor {
    pub name: &'static str,
    pub description: &'static str,
    pub is_side_effectful: bool,
    pub input_schema: fn() -> serde_json::Value,
    pub handler: for<'a> fn(JsonValue, DispatchContext<'a>)
        -> BoxFut<'a, Result<ToolOutput, DispatchError>>,
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

/// Hard byte ceiling applied to every Ok tool result by [`dispatch`] via
/// [`apply_max_size`]. Truncation is silent — `is_error` stays false and a
/// generic `[truncated: N bytes total]` marker replaces the tail — so any
/// tool that emits its own framing (headers, continuation trailers) must
/// keep content + framing under this cap or the framing is exactly what
/// gets cut. [`TOOL_RESULT_ENVELOPE`] is the headroom reserved for that
/// framing; per-tool ceilings (file_read's `FILE_READ_MAX`) derive from
/// the difference.
pub(crate) const MAX_TOOL_RESULT_SIZE: usize = 2_097_152;

/// Headroom reserved under [`MAX_TOOL_RESULT_SIZE`] for per-tool result
/// framing. file_read's worst case is ~250 fixed bytes plus twice the path
/// length (the path appears in both header and trailer); 4096 covers paths
/// up to ~1.9 KB. Pathological longer paths degrade to the generic
/// truncation marker — the pre-fix behavior, never worse. Sprint-6 fix:
/// FILE_READ_MAX used to EQUAL the cap, so a full-ceiling read of a larger
/// file overflowed on its own framing and [`apply_max_size`] amputated the
/// continuation trailer — the model lost the "continue at offset" contract
/// on exactly the files that need it.
pub(crate) const TOOL_RESULT_ENVELOPE: usize = 4096;

// Compile-time pins: the envelope must leave real room under the cap (a
// zero or near-zero reserve re-creates the trailer-amputation defect) while
// staying a small fraction of it.
const _: () = assert!(TOOL_RESULT_ENVELOPE >= 512);
const _: () = assert!(TOOL_RESULT_ENVELOPE < MAX_TOOL_RESULT_SIZE / 100);

/// Ceiling on the number of images one tool result may hand the model.
/// Images bypass [`apply_max_size`] (byte-truncating an encoded image would
/// corrupt it silently, `is_error:false`), so they are bounded by COUNT
/// here and by per-image size at ingest (`media::MEDIA_INLINE_MAX`). Extra
/// images are dropped with a note appended to the text so the model knows.
pub(crate) const MAX_TOOL_RESULT_IMAGES: usize = 4;

/// Global wall-clock ceiling for any single tool dispatch. The mirror of
/// [`MAX_TOOL_RESULT_SIZE`] for execution time: it bounds runaway tools
/// uniformly instead of relying on each handler to wrap itself. Tools that
/// need a tighter ceiling (`port_scan` per-port probes, `ssh_session_start`
/// handshake, `gh_clone`, etc.) keep their own inner `tokio::time::timeout`
/// wrappers; this is the backstop, not the operational limit.
const MAX_TOOL_DURATION: std::time::Duration = std::time::Duration::from_secs(600);

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

/// Apply the dispatcher caps to a structured result: the text cap on
/// `text`, the COUNT cap on `images` (never a byte cut — see
/// [`MAX_TOOL_RESULT_IMAGES`]).
fn apply_caps(mut out: ToolOutput) -> ToolOutput {
    if out.images.len() > MAX_TOOL_RESULT_IMAGES {
        let dropped = out.images.len() - MAX_TOOL_RESULT_IMAGES;
        out.images.truncate(MAX_TOOL_RESULT_IMAGES);
        out.text.push_str(&format!(
            "\n[{} image(s) dropped: at most {} images per tool result]",
            dropped, MAX_TOOL_RESULT_IMAGES
        ));
    }
    out.text = apply_max_size(out.text);
    out
}

/// Wraps a handler future in a wall-time ceiling. Lifted out of
/// [`dispatch`] so the timeout behavior is unit-testable without standing
/// up a full `DispatchContext` against the live `REGISTRY`.
pub(crate) async fn enforce_timeout<T, F>(
    fut: F,
    tool: &str,
    timeout: std::time::Duration,
) -> Result<T, DispatchError>
where
    F: std::future::Future<Output = Result<T, DispatchError>>,
{
    match tokio::time::timeout(timeout, fut).await {
        Ok(result) => result,
        Err(_) => Err(DispatchError::Timeout {
            tool: tool.into(),
            limit_secs: timeout.as_secs(),
        }),
    }
}

/// Native-tool-use dispatcher.
///
/// Looks up `name` in [`REGISTRY`], runs the handler with the typed
/// context under the [`MAX_TOOL_DURATION`] ceiling, and applies the
/// 2 MiB text cap + the image COUNT cap. Stage 3 wires this into the gRPC
/// dispatch loop; `tools/cron.rs` consumes `.text` only.
pub async fn dispatch(
    name: &str,
    input: JsonValue,
    ctx: DispatchContext<'_>,
) -> Result<ToolOutput, DispatchError> {
    let Some(desc) = REGISTRY.get(name) else {
        return Err(DispatchError::Unknown(name.into()));
    };
    let raw = enforce_timeout((desc.handler)(input, ctx), name, MAX_TOOL_DURATION).await?;
    Ok(apply_caps(raw))
}

#[cfg(test)]
mod timeout_tests {
    use super::*;

    #[tokio::test]
    async fn enforce_timeout_passes_through_fast_ok() {
        let fast = async { Ok::<_, DispatchError>("done".to_string()) };
        let res = enforce_timeout(fast, "fake", std::time::Duration::from_secs(1)).await;
        assert_eq!(res.unwrap(), "done");
    }

    #[tokio::test]
    async fn enforce_timeout_passes_through_handler_error() {
        let err = async { Err::<String, _>(DispatchError::Handler("boom".to_string())) };
        let res = enforce_timeout(err, "fake", std::time::Duration::from_secs(1)).await;
        assert!(matches!(res, Err(DispatchError::Handler(m)) if m == "boom"));
    }

    #[tokio::test]
    async fn enforce_timeout_returns_timeout_when_exceeded() {
        // Real-time short durations — workspace tokio doesn't have the
        // `test-util` feature enabled, so `start_paused` isn't available.
        // 50ms timeout vs 500ms sleep is well above the scheduler tick so
        // the timeout fires reliably without being flaky.
        let slow = async {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            Ok::<_, DispatchError>("never".to_string())
        };
        let res = enforce_timeout(slow, "fake_tool", std::time::Duration::from_millis(50)).await;
        assert!(matches!(
            res,
            Err(DispatchError::Timeout { tool, .. })
                if tool == "fake_tool"
        ));
    }
}

#[cfg(test)]
mod result_cap_tests {
    use super::*;

    #[test]
    fn caps_pinned() {
        // Style of tools/mod.rs::line_caps_pinned — a deliberate literal pin
        // so a casual cap change fails loudly and gets its docs sweep.
        assert_eq!(MAX_TOOL_RESULT_SIZE, 2_097_152);
        assert_eq!(TOOL_RESULT_ENVELOPE, 4096);
        assert_eq!(MAX_TOOL_RESULT_IMAGES, 4);
    }

    fn px(name: &str) -> embra_tools_core::ToolImage {
        embra_tools_core::ToolImage {
            media_type: "image/png".into(),
            data: vec![0x89, b'P', b'N', b'G', 0, 0, 0, 0],
            width: 1,
            height: 1,
            name: name.into(),
            media_ref: None,
        }
    }

    #[test]
    fn dispatch_caps_text_but_not_images() {
        // Oversized text is cut; the image bytes ride through untouched —
        // a byte cut on an encoded image would be silent corruption.
        let out = apply_caps(ToolOutput {
            text: "x".repeat(MAX_TOOL_RESULT_SIZE + 10),
            images: vec![px("a.png")],
            media_refs: Vec::new(),
        });
        assert!(out.text.contains("[truncated:"));
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].data.len(), 8);
    }

    #[test]
    fn dispatch_drops_images_beyond_max_with_note() {
        let out = apply_caps(ToolOutput {
            text: "six".into(),
            images: (0..6).map(|i| px(&format!("{i}.png"))).collect(),
            media_refs: Vec::new(),
        });
        assert_eq!(out.images.len(), MAX_TOOL_RESULT_IMAGES);
        assert!(out.text.ends_with("[2 image(s) dropped: at most 4 images per tool result]"), "{}", out.text);
        // Within the cap: text untouched, no note.
        let out = apply_caps(ToolOutput {
            text: "ok".into(),
            images: vec![px("a.png")],
            media_refs: Vec::new(),
        });
        assert_eq!(out.text, "ok");
    }

    #[test]
    fn apply_max_size_at_cap_untouched() {
        let s = "x".repeat(MAX_TOOL_RESULT_SIZE);
        let out = apply_max_size(s.clone());
        assert_eq!(out, s);
    }

    #[test]
    fn apply_max_size_over_cap_truncates_on_char_boundary() {
        // Multi-byte char straddling the boundary: 'é' is 2 bytes; start one
        // ASCII byte before the cap so the 2-byte char spans it.
        let total = MAX_TOOL_RESULT_SIZE + 10;
        let mut s = "a".repeat(MAX_TOOL_RESULT_SIZE - 1);
        while s.len() < total {
            s.push('é');
        }
        let original_len = s.len();
        let out = apply_max_size(s);
        assert!(out.len() <= MAX_TOOL_RESULT_SIZE + 64, "marker overhead only");
        // Valid UTF-8 is guaranteed by construction (String), but the cut
        // must not have landed inside the multi-byte char: the kept payload
        // ends at or below the cap on a boundary.
        let marker = format!("...\n[truncated: {} bytes total]", original_len);
        assert!(out.ends_with(&marker), "marker must report the ORIGINAL length: {}", &out[out.len() - 64..]);
        let payload_len = out.len() - marker.len();
        assert!(payload_len <= MAX_TOOL_RESULT_SIZE);
        assert!(payload_len >= MAX_TOOL_RESULT_SIZE - 4, "cut should back up at most a few bytes");
    }
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
                "is_side_effectful": d.is_side_effectful,
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

    // Ensure tools.turn_trace exists so the fire-and-forget dispatch persist
    // path (grpc_service.rs) doesn't race with first-write collection
    // creation. The trace docs are small per-dispatch records keyed by
    // (session, turn_index) and queryable by the `turn_trace` tool.
    if !db
        .collection_exists("tools.turn_trace")
        .await
        .unwrap_or(false)
    {
        db.create_collection("tools.turn_trace")
            .await
            .context("create tools.turn_trace collection")?;
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
