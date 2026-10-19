//! Typed tool registry — foundation for NATIVE-TOOLS-01 native tool-use.
//!
//! Each tool declares a typed args struct annotated with
//! `#[embra_tool(name = "...", description = "...")]`. The macro emits
//! `inventory::submit!` targeting the [`ToolDescriptor`] type defined here.
//! At first access, [`REGISTRY`] collects every submission into a
//! `HashMap<&'static str, &'static ToolDescriptor>` for O(1) lookup.
//!
//! [`dispatch`] is the one entry point: the gRPC tool loop and the cron
//! loop both call it; the string dispatcher it replaced is gone.

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
    /// The session a guided delete is preserving, in that flow's memorize
    /// turn (`PendingSessionDelete.name` in `grpc_service.rs`); `None` on
    /// every other dispatch. `remember` attributes a memory to it, and
    /// `session_extract` drops its approval round for it.
    pub deleting_session: Option<&'a str>,
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
///
/// Known limitation: the timeout drops the future, and that does not end a
/// child process the tool started. `tokio::process::Command::output()` does
/// not kill on drop, so a subprocess tool that times out leaves its child
/// running until the child exits by itself.
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

/// The tools whose input leaves the OS: a GitHub or GitLab request, a
/// commit message, a push, a clone, a remote command, an image prompt, a
/// Guardian call. [`dispatch`] refuses an input that carries one of the
/// operator's stored secrets (`crate::secrets::collect`) before the handler
/// runs. Guard `every_outbound_tool_is_listed`.
pub(crate) const OUTBOUND_TOOLS: &[&str] = &[
    "gh_issue_close", "gh_issue_comment", "gh_issue_create", "gh_issue_reopen", "gh_issue_view",
    "gh_issues", "gh_pr_close", "gh_pr_comment", "gh_pr_create", "gh_pr_merge", "gh_pr_view",
    "gh_prs", "gh_project_list", "gh_project_view",
    "gl_board_view", "gl_boards", "gl_issue_close", "gl_issue_comment", "gl_issue_create",
    "gl_issue_reopen", "gl_issue_view", "gl_issues", "gl_mr_close", "gl_mr_comment",
    "gl_mr_create", "gl_mr_merge", "gl_mr_view", "gl_mrs",
    "git_clone", "git_commit", "git_push",
    "ssh_remote_admin", "ssh_session_end", "ssh_session_exec", "ssh_session_start",
    "image_generate", "guardian_call",
];

/// The refusal for an outbound tool whose input carries a stored secret:
/// the kind, never the value.
fn outbound_refusal(name: &str, input: &JsonValue, config: &SystemConfig) -> Option<String> {
    if !OUTBOUND_TOOLS.contains(&name) {
        return None;
    }
    let text = serde_json::to_string(input).unwrap_or_default();
    let kind = crate::secrets::collect(Some(config)).found_in(&text)?;
    Some(format!(
        "refused: the input of {name} carries {kind}; a stored credential is never sent in a tool's input — the OS attaches it where it is needed"
    ))
}

/// Native-tool-use dispatcher.
///
/// Looks up `name` in [`REGISTRY`], refuses an outbound input that carries
/// a stored secret ([`OUTBOUND_TOOLS`]), runs the handler with the typed
/// context under the [`MAX_TOOL_DURATION`] ceiling, and applies the
/// 2 MiB text cap + the image COUNT cap. The gRPC tool loop calls it;
/// `tools/cron.rs` consumes `.text` only.
pub async fn dispatch(
    name: &str,
    input: JsonValue,
    ctx: DispatchContext<'_>,
) -> Result<ToolOutput, DispatchError> {
    let Some(desc) = REGISTRY.get(name) else {
        return Err(DispatchError::Unknown(name.into()));
    };
    // The activity feed sees every dispatch, a cron's included. The span
    // reports an error when this future is dropped: an operator stop
    // abandons it (`grpc_service.rs`).
    let origin = crate::activity::ToolOrigin::from_session(ctx.session_name);
    let span = crate::activity::ToolSpan::start(name, origin);
    if let Some(refusal) = outbound_refusal(name, &input, ctx.config) {
        span.finish(true);
        return Err(DispatchError::Handler(refusal));
    }
    let raw = enforce_timeout((desc.handler)(input, ctx), name, MAX_TOOL_DURATION).await;
    span.finish(raw.is_err());
    Ok(apply_caps(raw?))
}

#[cfg(test)]
mod activity_tests {
    use super::*;
    use crate::activity::{Event, ToolOrigin};

    /// A dispatch is announced with its name and origin and finished with
    /// its error state, the cron loop's dispatches included.
    #[tokio::test]
    async fn a_dispatch_reports_its_tool_name_origin_and_error_state() {
        let config: SystemConfig = serde_json::from_value(serde_json::json!({
            "name": "Embra", "api_key": "k", "timezone": "UTC", "deployment_mode": "phase1",
            "created_at": "", "version": "test", "kg_temporal_window_secs": 1800,
            "kg_max_traversal_depth": 3, "kg_traversal_depth_ceiling": 5,
            "kg_edge_candidate_limit": 50, "api_provider": "anthropic",
        }))
        .unwrap();
        let db = WardsonDbClient::from_url("http://127.0.0.1:1");
        let trace = embra_tools_core::new_turn_trace_handle();
        let mut events = crate::activity::subscribe();

        let ctx = DispatchContext {
            db: &db,
            config: &config,
            session_name: "cron",
            config_tz: "UTC",
            trace: &trace,
            turn_index: 0,
            deleting_session: None,
        };
        let out = dispatch("time", serde_json::json!({}), ctx).await;
        assert!(out.is_ok(), "{out:?}");

        let mine: Vec<Event> = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|e| {
                matches!(e, Event::ToolStarted { name, .. } | Event::ToolFinished { name, .. } if name == "time")
            })
            .collect();
        assert!(
            matches!(mine.first(), Some(Event::ToolStarted { origin: ToolOrigin::Cron, .. })),
            "{mine:?}"
        );
        assert!(
            matches!(
                mine.last(),
                Some(Event::ToolFinished { origin: ToolOrigin::Cron, is_error: false, .. })
            ),
            "{mine:?}"
        );
    }
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
            files: Vec::new(),
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
            files: Vec::new(),
        });
        assert_eq!(out.images.len(), MAX_TOOL_RESULT_IMAGES);
        assert!(out.text.ends_with("[2 image(s) dropped: at most 4 images per tool result]"), "{}", out.text);
        // Within the cap: text untouched, no note.
        let out = apply_caps(ToolOutput {
            text: "ok".into(),
            images: vec![px("a.png")],
            media_refs: Vec::new(),
            files: Vec::new(),
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

#[cfg(test)]
mod outbound_tests {
    //! The outbound secret guard in the dispatcher: which tools it covers,
    //! and that the refusal comes before the handler, with no request.
    use super::*;

    fn config_with_token() -> SystemConfig {
        serde_json::from_value(serde_json::json!({
            "name": "Embra", "api_key": "k", "timezone": "UTC", "deployment_mode": "phase1",
            "created_at": "", "version": "test", "api_provider": "anthropic",
            "github_token": "ghp_0123456789abcdefghij",
        }))
        .unwrap()
    }

    #[test]
    fn every_outbound_tool_is_listed() {
        for name in REGISTRY.keys() {
            let outbound = name.starts_with("gh_")
                || name.starts_with("gl_")
                || name.starts_with("ssh_")
                || matches!(*name, "git_clone" | "git_commit" | "git_push" | "image_generate" | "guardian_call");
            assert_eq!(OUTBOUND_TOOLS.contains(name), outbound, "{name}");
        }
        for name in OUTBOUND_TOOLS {
            assert!(REGISTRY.contains_key(name), "{name} is not a registered tool");
        }
    }

    #[tokio::test]
    async fn a_github_comment_that_carries_the_token_is_refused_without_a_request() {
        let config = config_with_token();
        let db = WardsonDbClient::from_url("http://127.0.0.1:1");
        let trace = embra_tools_core::new_turn_trace_handle();
        for body in [
            "here it is: ghp_0123456789abcdefghij",
            // base64 of the token
            "see Z2hwXzAxMjM0NTY3ODlhYmNkZWZnaGlq",
        ] {
            let ctx = DispatchContext {
                db: &db,
                config: &config,
                session_name: "main",
                config_tz: "UTC",
                trace: &trace,
                turn_index: 0,
                deleting_session: None,
            };
            let out = dispatch(
                "gh_issue_comment",
                serde_json::json!({"repo": "o/r", "number": 1, "body": body}),
                ctx,
            )
            .await;
            match out {
                Err(DispatchError::Handler(m)) => {
                    assert!(m.starts_with("refused: the input of gh_issue_comment carries the GitHub token"), "{m}");
                    assert!(!m.contains("ghp_") && !m.contains("Z2hw"), "the value is never echoed: {m}");
                }
                other => panic!("the handler ran: {other:?}"),
            }
        }
    }

    /// The input of the live check of 2026-10-18 (item 7), with the
    /// throwaway secret stored where that run had not stored it: a
    /// `/guardian secret` value in a GitLab comment body is refused by
    /// kind before the handler, like the GitHub token.
    #[tokio::test]
    async fn a_gitlab_comment_that_carries_a_guardian_secret_is_refused_without_a_request() {
        let config: SystemConfig = serde_json::from_value(serde_json::json!({
            "name": "Embra", "api_key": "k", "timezone": "UTC", "deployment_mode": "phase1",
            "created_at": "", "version": "test", "api_provider": "anthropic",
            "guardian_secrets": {"echo.example.test": {"x-test-token": "test-secret-0123456789"}},
        }))
        .unwrap();
        let db = WardsonDbClient::from_url("http://127.0.0.1:1");
        let trace = embra_tools_core::new_turn_trace_handle();
        let ctx = DispatchContext {
            db: &db,
            config: &config,
            session_name: "code-review-sec-debug_v1",
            config_tz: "UTC",
            trace: &trace,
            turn_index: 0,
            deleting_session: None,
        };
        let out = dispatch(
            "gl_issue_comment",
            serde_json::json!({"body": "probe: test-secret-0123456789", "host": "gitlab.ops.wsds", "iid": 23, "project": "embraOS/Embra"}),
            ctx,
        )
        .await;
        match out {
            Err(DispatchError::Handler(m)) => {
                assert!(m.starts_with("refused: the input of gl_issue_comment carries a /guardian secret"), "{m}");
                assert!(!m.contains("test-secret"), "the value is never echoed: {m}");
            }
            other => panic!("the handler ran: {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_same_text_in_a_workspace_write_is_not_refused() {
        let config = config_with_token();
        let db = WardsonDbClient::from_url("http://127.0.0.1:1");
        let trace = embra_tools_core::new_turn_trace_handle();
        let ctx = DispatchContext {
            db: &db,
            config: &config,
            session_name: "main",
            config_tz: "UTC",
            trace: &trace,
            turn_index: 0,
            deleting_session: None,
        };
        // file_write is not an outbound tool: the handler runs (and on a
        // developer host answers for the missing workspace root).
        let out = dispatch(
            "file_write",
            serde_json::json!({"path": "notes.txt", "content": "ghp_0123456789abcdefghij"}),
            ctx,
        )
        .await;
        assert!(
            !matches!(&out, Err(DispatchError::Handler(m)) if m.starts_with("refused: the input")),
            "{out:?}"
        );
    }
}
