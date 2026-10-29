//! Sessions and flows: the resume briefing's gates, the trigger budget, the guided delete, the provider check of a session, the listing lines.

use super::*;

/// Minimum session idle time (vs `meta.last_active`) before a SessionAttach
/// fires a resume briefing. Mobile browsers drop and reopen the chat WS on
/// every background/lock, and each reopen is a fresh Converse stream +
/// attach — without this gate every flap cost a full LLM recap turn. A
/// completed briefing persists its turns and thereby bumps `last_active`,
/// so the gate is self-limiting. `/switch` briefs unconditionally (explicit
/// user action). Module const, not SystemConfig — SystemConfig has no
/// Default, and every construction literal is a maintenance site for a
/// new field (`grep anthropic_effort:` finds them).
pub(super) const RESUME_BRIEFING_MIN_IDLE_SECS: i64 = 30 * 60;

/// Minimum spacing between briefing STARTS per session. Covers the
/// aborted-mid-recap flap loop the idle gate can't: an aborted turn never
/// persists, so `last_active` stays stale and every re-attach would
/// re-qualify — and a disconnect does not cancel the in-flight turn (the
/// converse task is detached), so rapid re-attaches could otherwise stack
/// concurrent briefings.
pub(super) const RESUME_BRIEFING_ATTEMPT_COOLDOWN_SECS: u64 = 120;

/// Idle-gate arm of the resume-briefing decision: `(allowed, idle_secs)`.
/// Negative idle (clock skew / future `last_active`) counts as not-idle —
/// suppress rather than brief on a nonsense timestamp.
pub(super) fn resume_briefing_idle_ok(
    last_active: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> (bool, i64) {
    let idle = now.signed_duration_since(last_active).num_seconds();
    (idle >= RESUME_BRIEFING_MIN_IDLE_SECS, idle)
}

/// Most trigger turns (`proactive::ActTrigger`) one stream runs in an
/// hour; past it a trigger is shown and logged, not run.
pub(crate) const ACT_TURNS_PER_HOUR_MAX: usize = 12;

pub(super) const ACT_BUDGET_WINDOW: std::time::Duration = std::time::Duration::from_secs(3600);

/// Bytes of a cron result a trigger turn persists in its user text; the
/// model-facing wrapper carries the whole capped result.
pub(super) const TRIGGER_PERSIST_MAX: usize = 1024;

/// The sliding count behind `ACT_TURNS_PER_HOUR_MAX`, one per Converse
/// task (only the stream holding the notification receiver runs triggers).
pub(crate) struct ActBudget {
    pub(super) stamps: std::collections::VecDeque<std::time::Instant>,
}

impl ActBudget {
    pub(crate) fn new() -> Self {
        Self { stamps: std::collections::VecDeque::new() }
    }

    /// Allow a trigger turn now, counting it, unless `ACT_TURNS_PER_HOUR_MAX`
    /// ran in the last hour. `Instant` is monotonic, so a clock jump cannot
    /// widen or narrow the window.
    pub(crate) fn allow(&mut self, now: std::time::Instant) -> bool {
        while let Some(&oldest) = self.stamps.front() {
            if now.duration_since(oldest) >= ACT_BUDGET_WINDOW {
                self.stamps.pop_front();
            } else {
                break;
            }
        }
        if self.stamps.len() >= ACT_TURNS_PER_HOUR_MAX {
            return false;
        }
        self.stamps.push_back(now);
        true
    }
}

/// What a trigger turn persists as its user text: the reminder's message,
/// or the job and the first kilobyte of its result. The model-facing
/// wrapper (`enrichment::build_trigger_context`) carries the whole body.
pub(crate) fn trigger_turn_text(act: &crate::proactive::ActTrigger) -> String {
    use crate::proactive::{TRIGGER_CRON_PREFIX, TRIGGER_REMINDER_PREFIX, TriggerKind};
    match act.kind {
        TriggerKind::Reminder => format!("{TRIGGER_REMINDER_PREFIX} {}", act.body),
        TriggerKind::Cron => format!(
            "{TRIGGER_CRON_PREFIX} {}: {}",
            act.headline,
            crate::tools::sessions::truncate_str(&act.body, TRIGGER_PERSIST_MAX)
        ),
    }
}

/// Why a trigger turn cannot run now, or `None` when it can: the soul is
/// sealed, a session is active, no turn is running on any stream, no
/// setup or delete flow is waiting for the operator's answer, and the
/// hourly cap has room.
pub(super) async fn trigger_refusal(
    db: &Arc<WardsonDbClient>,
    session_mgr: &Arc<RwLock<SessionManager>>,
    in_turn: &Arc<AtomicBool>,
    pending_key_setup: &Arc<Mutex<Option<KeySetupStep>>>,
    pending_openai_compat_setup: &Arc<Mutex<Option<OpenAiCompatSetupState>>>,
    pending_session_delete: &Arc<Mutex<Option<PendingSessionDelete>>>,
    budget: &mut ActBudget,
) -> Option<String> {
    if !matches!(learning::is_soul_sealed(db).await, Ok(true)) {
        return Some("the soul is not sealed yet".to_string());
    }
    if session_mgr.read().await.active_session.is_none() {
        return Some("no session is active".to_string());
    }
    if in_turn.load(Ordering::SeqCst) {
        return Some("a turn is running".to_string());
    }
    if pending_key_setup.lock().await.is_some() || pending_openai_compat_setup.lock().await.is_some() {
        return Some("a provider setup is waiting for your answer".to_string());
    }
    if pending_session_delete.lock().await.is_some() {
        return Some("a session delete is waiting for your answer".to_string());
    }
    if !budget.allow(std::time::Instant::now()) {
        return Some(format!(
            "the cap of {ACT_TURNS_PER_HOUR_MAX} trigger turns an hour is reached"
        ));
    }
    None
}

/// Guided `/sessions delete <name>` flow state (the pending_key_setup
/// family). Armed by the `/sessions` slash arm right before it returns the
/// kickoff synthetic prompt; the UserMessage intercept advances/consumes
/// it; ANY slash command mid-flow cancels it (deliberately unlike
/// `pending_key_setup`, which a slash leaves armed — that asymmetry is a
/// documented wart, not a pattern to copy).
#[derive(Debug, Clone)]
pub struct PendingSessionDelete {
    pub name: String,
    pub phase: DeleteFlowPhase,
}

/// Two-phase intercept discriminator. `AwaitingKickoff`: the NEXT
/// UserMessage is the kickoff synthetic the slash arm itself dispatched —
/// advance in place and let it run as a normal turn (consuming it would
/// eat the kickoff as the operator's reason). `AwaitingReason`: the next
/// UserMessage is the operator's reason — take the state, swap in the
/// memorize prompt, and execute the deletion post-turn on the model's
/// READY marker.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeleteFlowPhase {
    AwaitingKickoff,
    AwaitingReason,
}

/// One user message against the delete flow. The first after the command is
/// the kickoff the command itself dispatched: it moves the flow on and is
/// not the reason. The second is the operator's reason: it takes the flow
/// out of the slot, for the turn to act on.
pub(super) fn advance_delete_flow(slot: &mut Option<PendingSessionDelete>) -> Option<PendingSessionDelete> {
    match slot.as_mut() {
        Some(state) if state.phase == DeleteFlowPhase::AwaitingKickoff => {
            state.phase = DeleteFlowPhase::AwaitingReason;
            None
        }
        Some(_) => slot.take(),
        None => None,
    }
}

/// Markers the memorize turn must end with — the learning-loop
/// `[PHASE_COMPLETE]` precedent. Scanned from the final assistant text;
/// the brain (not the model) executes the actual soft delete.
pub(crate) const SESSION_DELETE_READY_MARKER: &str = "[SESSION_DELETE_READY]";

pub(crate) const SESSION_DELETE_ABORT_MARKER: &str = "[SESSION_DELETE_ABORT]";

#[derive(Debug, PartialEq)]
pub(crate) enum DeleteVerdict {
    Ready,
    Abort,
    NoMarker,
}

/// Fail-closed marker scan: ABORT wins when both appear; no marker (turn
/// error, cap, refusal, or the model simply not emitting one) never
/// deletes.
pub(crate) fn delete_flow_verdict(final_text: &str) -> DeleteVerdict {
    if final_text.contains(SESSION_DELETE_ABORT_MARKER) {
        return DeleteVerdict::Abort;
    }
    if final_text.contains(SESSION_DELETE_READY_MARKER) {
        return DeleteVerdict::Ready;
    }
    DeleteVerdict::NoMarker
}

/// Kickoff synthetic prompt (turn 1 of the guided delete): summarize and
/// ask for the reason. Persisted verbatim as the user turn (the
/// /feedback-loop precedent), so it reads sensibly in history.
pub(crate) fn build_delete_kickoff_prompt(name: &str) -> String {
    format!(
        "[Session delete requested: {name}]\n\
         The operator has asked to delete the session '{name}'. Before anything is removed:\n\
         1. Call session_summarize with name \"{name}\" and follow its instructions \
         (generate the summary and persist it with session_summary_save).\n\
         2. Present the operator a concise summary of what that session contains.\n\
         3. Ask the operator why they are deleting it — the reason will be recorded.\n\
         Do NOT write memories yet, do NOT emit any completion marker, and do NOT \
         delete anything — deletion only proceeds after the operator replies."
    )
}

/// Memorize synthetic prompt (turn 2, built from the operator's reason
/// reply). The turn's raw persisted content stays the operator's reason;
/// this replaces it brain-facing only (the resume-briefing enrichment-swap
/// pattern).
pub(crate) fn build_delete_memorize_prompt(name: &str, reason: &str) -> String {
    format!(
        "The operator replied to your deletion question for session '{name}':\n\
         \"{reason}\"\n\n\
         If that reply declines, cancels, or clearly is not a reason to proceed, do not \
         save anything and end your response with {SESSION_DELETE_ABORT_MARKER} on its own line.\n\
         Otherwise treat it as the deletion reason and preserve the session's value before it is removed:\n\
         1. Call session_extract with name \"{name}\" and follow its instructions. The operator's \
         reply is the approval it asks for: save each durable learning with remember and its category \
         in this turn, without presenting them first. remember records each as coming from session '{name}'.\n\
         2. Tell the operator what you preserved, then end your response with \
         {SESSION_DELETE_READY_MARKER} on its own line.\n\
         After your turn the system deletes the session and records the deletion and the operator's \
         reason as a memory linked to what you saved; do not save one yourself. The deletion is soft \
         (a {days}-day grace period; /sessions restore {name} undoes it).",
        days = crate::sessions::SESSION_DELETE_RETENTION_DAYS,
    )
}

/// One row of `/sessions`: the name, the learning session's tag, the state,
/// the provider the session is pinned to, the turns and the last activity.
/// `learning` is never switched to and records no provider.
pub(super) fn session_list_line(s: &SessionMeta, is_sealed: bool, tz: &str) -> String {
    let (indicator, provider) = if s.name == "learning" {
        (if is_sealed { " [sealed]" } else { " [learning]" }, String::new())
    } else {
        ("", format!(" {} ·", crate::provider::provider_label(s.recorded_provider())))
    };
    let n = s.turn_count.unwrap_or(0);
    format!(
        "  {}{} [{:?}]{} {} turn{}, last active: {}",
        s.name,
        indicator,
        s.state,
        provider,
        n,
        if n == 1 { "" } else { "s" },
        format_local_timestamp(s.last_active, tz)
    )
}

/// The line `/sessions` closes with when a session it lists is pinned to
/// another provider than the active one: those providers as `/provider`
/// takes them, and the steps `cross_provider_block_message` gives. `None`
/// when every session is under the active provider.
pub(super) fn other_provider_footer(live: &[SessionMeta], active_provider: &str, attached: bool) -> Option<String> {
    let mut others: Vec<&str> = live
        .iter()
        .filter(|s| s.name != "learning")
        .map(SessionMeta::recorded_provider)
        .filter(|p| *p != active_provider)
        .collect();
    others.sort_unstable();
    others.dedup();
    let (under, kind) = match others.as_slice() {
        [] => return None,
        [one] => (one.to_string(), *one),
        several => (format!("another provider ({})", several.join(", ")), "<kind>"),
    };
    let close = if attached { "/close, then " } else { "" };
    Some(format!("  Under {under}: {close}/provider {kind}, then /switch <name>."))
}

/// Render a UTC timestamp in the operator's configured timezone for
/// display surfaces (the `/sessions` listing). Abbreviations resolve
/// through `tools::resolve_timezone` (BUG-007) before parsing; an
/// unparseable timezone falls back to UTC with an explicit label so
/// the reading is never ambiguous.
pub(super) fn format_local_timestamp(utc: chrono::DateTime<chrono::Utc>, config_tz: &str) -> String {
    match tools::resolve_timezone(config_tz).parse::<chrono_tz::Tz>() {
        Ok(tz) => utc.with_timezone(&tz).format("%Y-%m-%d %H:%M %Z").to_string(),
        Err(_) => utc.format("%Y-%m-%d %H:%M UTC").to_string(),
    }
}

// ============================================================
// OpenAI-compat post-wizard reconfigure (Sprint 5 follow-up)
// ============================================================

/// Check that `session_name`'s recorded provider matches the
/// currently-active provider. Returns `Ok(())` when compatible (or
/// when the active provider can't be loaded — fail-open on config
/// errors so the existing user-facing errors elsewhere surface them).
/// Returns `Err((session_provider, active_provider))` when the user
/// must take action — caller formats the error message to include
/// the session name and any UX-specific context.
///
/// Every session-activation path must call this before setting
/// `active_session`: `SessionAttach` handler, `/switch` slash command,
/// and the `switch_session` gRPC RPC. `SessionManager::create` does
/// not need it (just-created sessions are stamped from the active
/// provider by definition).
pub(super) async fn check_session_provider(
    db: &Arc<WardsonDbClient>,
    session_name: &str,
) -> Result<(), (String, String)> {
    let session_provider = read_session_provider(db, session_name).await;
    let active_provider = config::load_config(db)
        .await
        .map(|c| c.api_provider)
        .unwrap_or_else(|_| "anthropic".to_string());
    if providers_compatible(&session_provider, &active_provider) {
        Ok(())
    } else {
        Err((session_provider, active_provider))
    }
}

/// Read `meta.provider` for a session. Defaults to `"anthropic"`
/// when the field is absent — pre-Sprint-4 sessions are by definition
/// anthropic (no other provider existed). The v9 migration backfills
/// legacy docs and `SessionManager::create` stamps new sessions at
/// create time, so the default only applies to the rare edge case
/// where a meta doc exists with no provider field at all.
pub(super) async fn read_session_provider(db: &Arc<WardsonDbClient>, session_name: &str) -> String {
    // Session-meta docs are written via `db.write` with no explicit
    // `_id`, so WardSONDB auto-generates one — `db.read(collection, name)`
    // would 404. Every other reader of `sessions.<name>.meta`
    // (`SessionManager::update_state`, `::reattach`, `::list`) uses
    // `db.query` for this reason. Match that pattern here, then default
    // to `"anthropic"` if the doc lacks a provider field (legacy pre-Sprint-4
    // session that v9 migration also missed for the same _id reason).
    let collection = format!("sessions.{}.meta", session_name);
    match db.query(&collection, &crate::sessions::history_query_body()).await {
        Ok(results) => results
            .into_iter()
            .next()
            .and_then(|doc| {
                doc.get("provider")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| crate::sessions::LEGACY_SESSION_PROVIDER.to_string()),
        Err(_) => crate::sessions::LEGACY_SESSION_PROVIDER.to_string(),
    }
}

/// Strict equality. The caller (`read_session_provider`) is
/// responsible for normalising missing values to a concrete provider
/// before reaching this function — there is no wildcard.
pub(super) fn providers_compatible(a: &str, b: &str) -> bool {
    a == b
}

/// Operator-facing guidance when a session activation is blocked by
/// the cross-provider pin. Shared by all three block sites
/// (`SessionAttach`, `/switch`, and the `switch_session` RPC) so the
/// navigation stays consistent — and STATE-AWARE, because the naive
/// advice is a trap: `/provider <kind>` refuses to run while a session
/// is active, so with a session attached the operator must `/close`
/// FIRST. The old short message ("Use /provider {} to continue") sent
/// them straight into that second refusal.
pub(super) fn cross_provider_block_message(
    session_name: &str,
    session_provider: &str,
    active_provider: &str,
    active_session: Option<&str>,
) -> String {
    let open_steps = match active_session {
        Some(current) => format!(
            "/close (detaches the active session '{}'), then /provider {}, then /switch {}",
            current, session_provider, session_name
        ),
        None => format!(
            "/provider {}, then /switch {}",
            session_provider, session_name
        ),
    };
    format!(
        "Session '{}' was recorded under {}; the active provider is {}. \
         To open it: {}. To stay on {} instead: /new <name> for a fresh session.",
        session_name, session_provider, active_provider, open_steps, active_provider
    )
}

/// Operator-facing guidance when a plain prompt arrives with no session
/// attached (after `/close`, or a failed learning loop). State-aware like
/// `cross_provider_block_message`: names the most recent resumable session
/// when one exists. The turn is NOT sent to the model — the old fallback
/// ran it against a phantom "default" session whose persistence silently
/// no-opped (a ghost turn; and genuine cross-provider contamination if a
/// real session named "default" existed, since this path never runs
/// `check_session_provider`).
pub(super) fn no_session_block_message(most_recent: Option<&str>) -> String {
    match most_recent {
        Some(name) => format!(
            "No session is attached, so this message was not processed. \
             To resume '{}': /switch {}. To start fresh: /new <name>. \
             /sessions lists all sessions.",
            name, name
        ),
        None => "No session is attached, so this message was not processed. \
                 To start one: /new <name>. /sessions lists all sessions."
            .to_string(),
    }
}
