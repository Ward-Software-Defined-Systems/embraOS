use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::brain::Message;
use crate::db::WardsonDbClient;

/// Session document format version.
///
/// Set to `CURRENT_SESSION_FORMAT` on every session created post-v7
/// migration. Sessions written pre-v7 (pre-NATIVE-TOOLS-01) have an
/// absent or `1` marker — those are frozen read-only. Future bumps
/// for typed-block persistence or other schema changes increment this
/// without requiring a new format_version ladder.
pub const CURRENT_SESSION_FORMAT: u32 = 2;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(
        "session '{0}' is legacy (format_version < {}); it is readable but cannot be appended to. Create a new session to continue.",
        CURRENT_SESSION_FORMAT
    )]
    LegacyReadOnly(String),
    #[error("session IO: {0}")]
    Io(#[from] anyhow::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SessionState {
    Active,
    Detached,
    Closed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub name: String,
    pub state: SessionState,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    /// Active provider when the session was last written. `None` on
    /// pre-v9 docs; v9 migration backfills with `"anthropic"` for
    /// every existing meta. Sprint 4 adds the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Display model name (e.g. `"opus-4.7"`, `"gemini-3.1-pro"`).
    /// Same defaulting rule as `provider`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Message count of the session's history (`SessionHistory.turns.len()`),
    /// stamped by `create` and re-stamped on every successful
    /// `append_message`. `None` on pre-fix docs; `list_with_counts`
    /// derives those from the history doc and backfills the meta.
    /// Serde-additive — no schema-version bump (anthropic_model precedent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_count: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHistory {
    pub session_name: String,
    /// Schema marker. See [`CURRENT_SESSION_FORMAT`].
    #[serde(default = "default_legacy_format_version")]
    pub format_version: u32,
    pub turns: Vec<Message>,
}

fn default_legacy_format_version() -> u32 {
    // Pre-v7 session docs lack this field; serde fills with 1 (legacy)
    // so the read-only guard in append_message correctly rejects them.
    1
}

/// Single-doc query body (FIX-5): explicit limit + oldest-first sort so the
/// FIRST document is deterministically the canonical doc (both the
/// `sessions.<name>.history` and `sessions.<name>.meta` collections store
/// one doc per session; an empty body would ride the server's default
/// limit and key order).
pub(crate) fn history_query_body() -> serde_json::Value {
    serde_json::json!({ "limit": 10, "sort": [{"_created_at": "asc"}] })
}

pub struct SessionManager {
    db: WardsonDbClient,
    pub active_session: Option<String>,
    /// Set by the SessionAttach handler when a resume briefing should fire
    /// on the next user turn (existing session, prior history, Operational
    /// mode). The UserMessage handler reads-and-clears this flag and
    /// substitutes the `<session_resumption>` wrapper for the brain-facing
    /// call; the synthetic UserMessage's raw `content` (`[Session resumed]`)
    /// is what gets persisted to history. Pure runtime state — not
    /// serialized, no schema bump.
    pub pending_resume_briefing: bool,
    /// When a resume briefing was last STARTED per session (runtime only,
    /// never serialized). Read/written by the briefing dispatch sites in
    /// grpc_service.rs to enforce `RESUME_BRIEFING_ATTEMPT_COOLDOWN_SECS`.
    /// Suppressed attempts do NOT re-stamp — the semantics are "at most
    /// one briefing start per session per cooldown window".
    briefing_attempts: std::collections::HashMap<String, std::time::Instant>,
}

impl SessionManager {
    pub fn new(db: WardsonDbClient) -> Self {
        Self {
            db,
            active_session: None,
            pending_resume_briefing: false,
            briefing_attempts: std::collections::HashMap::new(),
        }
    }

    /// True when a resume briefing was started for `name` less than
    /// `cooldown` ago. `Instant` is monotonic, so wall-clock jumps can't
    /// spoof the window.
    pub fn briefing_attempt_recent(&self, name: &str, cooldown: std::time::Duration) -> bool {
        self.briefing_attempts
            .get(name)
            .is_some_and(|t| t.elapsed() < cooldown)
    }

    /// Record that a resume briefing is starting for `name` now.
    pub fn record_briefing_attempt(&mut self, name: &str) {
        self.briefing_attempts
            .insert(name.to_string(), std::time::Instant::now());
    }

    pub async fn session_exists(&self, name: &str) -> Result<bool> {
        let meta = format!("sessions.{}.meta", name);
        self.db.collection_exists(&meta).await
    }

    pub async fn create(&mut self, name: &str) -> Result<SessionMeta> {
        let meta_collection = format!("sessions.{}.meta", name);
        let history_collection = format!("sessions.{}.history", name);

        // Create collections
        if !self.db.collection_exists(&meta_collection).await? {
            self.db.create_collection(&meta_collection).await?;
        }
        if !self.db.collection_exists(&history_collection).await? {
            self.db.create_collection(&history_collection).await?;
        }

        // Stamp the active provider on the new session so cross-provider
        // attach checks on subsequent boots have a real value to match
        // against (Sprint 4: previously left as None, which read_session_provider
        // defaulted to "anthropic" — incorrect for sessions created under
        // a Gemini-configured brain).
        let (provider, model) = match crate::config::load_config(&self.db).await {
            Ok(cfg) => {
                let m = match cfg.api_provider.as_str() {
                    "gemini" => "gemini-3.1-pro".to_string(),
                    // Anthropic: reflect the persisted model alias (set by
                    // the wizard / `/model`), falling back to the default's
                    // display. Display metadata only — the resolver in
                    // grpc_service.rs owns the request-time id.
                    _ => cfg
                        .anthropic_model
                        .clone()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "opus-4.8".to_string()),
                };
                (Some(cfg.api_provider), Some(m))
            }
            Err(_) => (None, None),
        };

        let now = Utc::now();
        let meta = SessionMeta {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            state: SessionState::Active,
            created_at: now,
            last_active: now,
            provider,
            model,
            turn_count: Some(0),
        };

        let history = SessionHistory {
            session_name: name.to_string(),
            format_version: CURRENT_SESSION_FORMAT,
            turns: Vec::new(),
        };

        self.db
            .write(&meta_collection, &serde_json::to_value(&meta)?)
            .await?;
        self.db
            .write(&history_collection, &serde_json::to_value(&history)?)
            .await?;

        self.active_session = Some(name.to_string());
        Ok(meta)
    }

    pub async fn load_history(&self, name: &str) -> Result<Vec<Message>> {
        let collection = format!("sessions.{}.history", name);
        let results = self.db.query(&collection, &history_query_body()).await?;
        if let Some(doc) = results.into_iter().next() {
            let history: SessionHistory = serde_json::from_value(doc)?;
            Ok(history.turns)
        } else {
            Ok(Vec::new())
        }
    }

    pub async fn append_message(
        &self,
        name: &str,
        message: &Message,
    ) -> Result<(), SessionError> {
        let collection = format!("sessions.{}.history", name);
        let results = self
            .db
            .query(&collection, &history_query_body())
            .await
            .map_err(|e| SessionError::Io(e.into()))?;

        // Stamped after a successful push so the meta update below can
        // mirror the history length; stays None when no history doc
        // exists (the meta rewrite still runs for last_active).
        let mut appended_turn_count: Option<u32> = None;

        if let Some(doc) = results.into_iter().next() {
            let history: SessionHistory = serde_json::from_value(doc.clone())
                .map_err(|e| SessionError::Io(e.into()))?;

            // Legacy session freeze (NATIVE-TOOLS-01 Stage 8). Sessions
            // created before v7 migration have format_version = 1 (either
            // stamped by run_v7_native_tools or defaulted via serde).
            // Their content may contain [TOOL:...] strings from the
            // legacy dispatcher; appending new turns would interleave
            // incompatible formats. Read-only is the locked policy.
            if history.format_version < CURRENT_SESSION_FORMAT {
                return Err(SessionError::LegacyReadOnly(name.to_string()));
            }

            let mut history = history;
            history.turns.push(message.clone());
            appended_turn_count = Some(history.turns.len() as u32);

            if let Some(id) = doc.get("_id").or(doc.get("id")).and_then(|v| v.as_str()) {
                self.db
                    .update(&collection, id, &serde_json::to_value(&history)
                        .map_err(|e| SessionError::Io(e.into()))?)
                    .await
                    .map_err(|e| SessionError::Io(e.into()))?;
            }
        }

        // Update last_active (and the turn-count mirror) on meta. Non-fatal
        // if it fails — the history write is the source of truth for
        // append_message's success.
        let meta_collection = format!("sessions.{}.meta", name);
        let meta_results = self
            .db
            .query(&meta_collection, &history_query_body())
            .await
            .map_err(|e| SessionError::Io(e.into()))?;
        if let Some(meta_doc) = meta_results.into_iter().next() {
            let id = meta_doc
                .get("_id")
                .or(meta_doc.get("id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if let Some(id) = id {
                let mut meta: SessionMeta = serde_json::from_value(meta_doc)
                    .map_err(|e| SessionError::Io(e.into()))?;
                meta.last_active = Utc::now();
                if let Some(n) = appended_turn_count {
                    meta.turn_count = Some(n);
                }
                self.db
                    .update(&meta_collection, &id, &serde_json::to_value(&meta)
                        .map_err(|e| SessionError::Io(e.into()))?)
                    .await
                    .map_err(|e| SessionError::Io(e.into()))?;
            }
        }

        Ok(())
    }

    pub async fn detach(&mut self, name: &str) -> Result<()> {
        self.update_state(name, SessionState::Detached).await?;
        if self.active_session.as_deref() == Some(name) {
            self.active_session = None;
        }
        Ok(())
    }

    pub async fn reattach(&mut self, name: &str) -> Result<Vec<Message>> {
        self.update_state(name, SessionState::Active).await?;
        self.active_session = Some(name.to_string());
        self.load_history(name).await
    }

    pub async fn close(&mut self, name: &str) -> Result<()> {
        self.update_state(name, SessionState::Closed).await?;
        if self.active_session.as_deref() == Some(name) {
            self.active_session = None;
        }
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<SessionMeta>> {
        Ok(self.list_docs().await?.into_iter().map(|(_, _, m)| m).collect())
    }

    /// `list()` plus the stored-doc address per meta, so callers that
    /// write back (the `list_with_counts` backfill) can target the
    /// WardSONDB document. The doc id is the server-assigned `_id`
    /// (falling back to `id` per the append_message pattern) — NOT
    /// `SessionMeta.id`, which is an internal uuid.
    async fn list_docs(&self) -> Result<Vec<(String, Option<String>, SessionMeta)>> {
        let collections = self.db.list_collections().await?;
        let mut sessions = Vec::new();

        for col in collections {
            if col.starts_with("sessions.") && col.ends_with(".meta") {
                let results = self.db.query(&col, &history_query_body()).await?;
                for doc in results {
                    let doc_id = doc
                        .get("_id")
                        .or(doc.get("id"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    if let Ok(meta) = serde_json::from_value::<SessionMeta>(doc) {
                        sessions.push((col.clone(), doc_id, meta));
                    }
                }
            }
        }

        Ok(sessions)
    }

    /// `list()` with `turn_count` guaranteed `Some` on every returned meta.
    /// Metas already stamped (created or appended-to post-fix) pass through;
    /// unstamped pre-fix metas get the count derived from their history doc,
    /// and the derived value is backfilled onto the stored meta
    /// fire-and-forget (spawn_access_touches precedent; `patch_document`
    /// so a concurrent `append_message` full-doc update can't be clobbered
    /// on other fields). A failed derivation displays as 0 but is NOT
    /// backfilled — never persist a guess.
    pub async fn list_with_counts(&self) -> Result<Vec<SessionMeta>> {
        let mut out = Vec::new();
        for (meta_collection, doc_id, mut meta) in self.list_docs().await? {
            if meta.turn_count.is_none() {
                match self.fetch_turn_count(&meta.name).await {
                    Ok(n) => {
                        meta.turn_count = Some(n);
                        if let Some(id) = doc_id {
                            Self::spawn_turn_count_backfill(
                                self.db.clone(),
                                meta_collection,
                                id,
                                n,
                            );
                        }
                    }
                    Err(_) => meta.turn_count = Some(0),
                }
            }
            out.push(meta);
        }
        Ok(out)
    }

    /// Count the turns in a session's canonical history doc (raw JSON
    /// `turns` array length — robust to legacy format_version 1 shapes
    /// that `SessionHistory` deserialization would reject).
    async fn fetch_turn_count(&self, name: &str) -> Result<u32> {
        let collection = format!("sessions.{}.history", name);
        let results = self.db.query(&collection, &history_query_body()).await?;
        Ok(results
            .into_iter()
            .next()
            .and_then(|doc| doc.get("turns").and_then(|t| t.as_array()).map(|a| a.len()))
            .unwrap_or(0) as u32)
    }

    /// Fire-and-forget: persist a derived turn count onto a pre-fix meta
    /// doc so subsequent listings skip the history fetch.
    fn spawn_turn_count_backfill(
        db: WardsonDbClient,
        meta_collection: String,
        doc_id: String,
        count: u32,
    ) {
        tokio::spawn(async move {
            let patch = serde_json::json!({ "turn_count": count });
            if let Err(e) = db.patch_document(&meta_collection, &doc_id, &patch).await {
                tracing::debug!(
                    target: "sessions",
                    collection = %meta_collection,
                    error = %e,
                    "turn_count backfill patch failed (will retry on next listing)"
                );
            }
        });
    }

    /// Read one session's meta doc. `Ok(None)` when the collection is
    /// missing/empty or the doc doesn't deserialize (same tolerance as
    /// `list`). Query body = `history_query_body()` — meta collections
    /// follow the same one-doc-per-session model.
    pub async fn get_meta(&self, name: &str) -> Result<Option<SessionMeta>> {
        let meta_collection = format!("sessions.{}.meta", name);
        let results = self.db.query(&meta_collection, &history_query_body()).await?;
        Ok(results
            .into_iter()
            .next()
            .and_then(|doc| serde_json::from_value::<SessionMeta>(doc).ok()))
    }

    pub async fn get_most_recent_active(&self) -> Result<Option<SessionMeta>> {
        let sessions = self.list().await?;
        Ok(sessions
            .into_iter()
            .filter(|s| s.state == SessionState::Active || s.state == SessionState::Detached)
            .max_by_key(|s| s.last_active))
    }

    async fn update_state(&self, name: &str, state: SessionState) -> Result<()> {
        let meta_collection = format!("sessions.{}.meta", name);
        let results = self
            .db
            .query(&meta_collection, &history_query_body())
            .await?;
        if let Some(doc) = results.into_iter().next() {
            let id = doc
                .get("_id")
                .or(doc.get("id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if let Some(id) = id {
                let mut meta: SessionMeta = serde_json::from_value(doc)?;
                meta.state = state;
                meta.last_active = Utc::now();
                self.db
                    .update(&meta_collection, &id, &serde_json::to_value(&meta)?)
                    .await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod format_version_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pre_v7_doc_without_format_version_defaults_to_legacy_marker() {
        let raw = json!({
            "session_name": "old-session",
            "turns": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "hi"},
            ],
        });
        let h: SessionHistory = serde_json::from_value(raw).unwrap();
        assert_eq!(
            h.format_version, 1,
            "legacy docs (no format_version field) must deserialize to 1"
        );
        assert!(
            h.format_version < CURRENT_SESSION_FORMAT,
            "legacy marker must be strictly below CURRENT_SESSION_FORMAT"
        );
    }

    #[test]
    fn post_v7_doc_round_trips_format_version() {
        let h = SessionHistory {
            session_name: "new-session".into(),
            format_version: CURRENT_SESSION_FORMAT,
            turns: vec![],
        };
        let serialized = serde_json::to_value(&h).unwrap();
        assert_eq!(serialized["format_version"], CURRENT_SESSION_FORMAT);
        let restored: SessionHistory = serde_json::from_value(serialized).unwrap();
        assert_eq!(restored.format_version, CURRENT_SESSION_FORMAT);
    }

    #[test]
    fn explicitly_stamped_legacy_format_version_deserializes() {
        // Matches what run_v7_native_tools writes when stamping
        // pre-migration sessions.
        let raw = json!({
            "session_name": "stamped",
            "format_version": 1,
            "turns": [],
        });
        let h: SessionHistory = serde_json::from_value(raw).unwrap();
        assert_eq!(h.format_version, 1);
    }
}

#[cfg(test)]
mod history_query_tests {
    // Relocated from tools/sessions.rs with its builder (session-ux-fixes
    // wave); the assertions are unchanged — the body shape is load-bearing
    // for the FIX-5 canonical-first-doc guarantee.
    use super::history_query_body;
    use serde_json::json;

    #[test]
    fn history_body_limit_10_oldest_first() {
        let body = history_query_body();
        assert_eq!(body["limit"], json!(10));
        assert_eq!(body["sort"], json!([{"_created_at": "asc"}]));
    }
}

#[cfg(test)]
mod turn_count_meta_tests {
    //! Serde shape of the additive `SessionMeta.turn_count` field. The
    //! stamp-on-append and derive-and-backfill paths need a live
    //! WardSONDB, so they are QEMU-verified rather than unit-tested
    //! (same convention as pending_resume_briefing_tests below).
    use super::*;
    use serde_json::json;

    fn pre_fix_meta_json() -> serde_json::Value {
        json!({
            "id": "0e0f9c1a-aaaa-bbbb-cccc-000000000001",
            "name": "main",
            "state": "Active",
            "created_at": "2026-05-01T00:00:00Z",
            "last_active": "2026-07-01T00:00:00Z",
            "provider": "anthropic",
        })
    }

    #[test]
    fn pre_fix_meta_without_turn_count_deserializes_to_none() {
        let meta: SessionMeta = serde_json::from_value(pre_fix_meta_json()).unwrap();
        assert_eq!(
            meta.turn_count, None,
            "missing turn_count must read as None so list_with_counts knows to derive it"
        );
    }

    #[test]
    fn stamped_turn_count_round_trips() {
        let mut meta: SessionMeta = serde_json::from_value(pre_fix_meta_json()).unwrap();
        meta.turn_count = Some(42);
        let serialized = serde_json::to_value(&meta).unwrap();
        assert_eq!(serialized["turn_count"], json!(42));
        let restored: SessionMeta = serde_json::from_value(serialized).unwrap();
        assert_eq!(restored.turn_count, Some(42));
    }

    #[test]
    fn none_turn_count_is_absent_from_serialized_meta() {
        // skip_serializing_if keeps pre-fix doc shapes byte-stable when a
        // meta is round-tripped without ever being stamped (e.g.
        // update_state on a legacy session).
        let meta: SessionMeta = serde_json::from_value(pre_fix_meta_json()).unwrap();
        let serialized = serde_json::to_value(&meta).unwrap();
        assert!(
            serialized.get("turn_count").is_none(),
            "None must serialize to an absent field, not null"
        );
    }
}

#[cfg(test)]
mod pending_resume_briefing_tests {
    //! Verifies the in-memory `pending_resume_briefing` flag added for
    //! the SessionAttach → UserMessage resume-briefing handshake. The
    //! full briefing loop (SessionAttach → synthetic dispatch → model
    //! call → stream → persist) depends on a live WardSONDB and LLM
    //! provider, so it is QEMU-verified rather than unit-tested.
    use super::*;

    #[test]
    fn pending_resume_briefing_defaults_false() {
        // Dummy client — never connected; these tests don't touch the DB.
        let db = WardsonDbClient::from_url("http://127.0.0.1:1");
        let mgr = SessionManager::new(db);
        assert!(
            !mgr.pending_resume_briefing,
            "default must be false so the first turn after construction isn't accidentally treated as a resumption"
        );
    }

    #[test]
    fn pending_resume_briefing_replaces_cleanly() {
        let db = WardsonDbClient::from_url("http://127.0.0.1:1");
        let mut mgr = SessionManager::new(db);
        mgr.pending_resume_briefing = true;
        // Mirrors the read-and-clear pattern in grpc_service.rs's
        // UserMessage handler — std::mem::replace returns the prior
        // value and stores `false`, so the flag is one-shot.
        let was = std::mem::replace(&mut mgr.pending_resume_briefing, false);
        assert!(was, "replace must return the prior `true`");
        assert!(
            !mgr.pending_resume_briefing,
            "after replace, the flag must be cleared so subsequent turns are not briefings"
        );
    }
}

#[cfg(test)]
mod briefing_attempt_cooldown_tests {
    //! Verifies the runtime `briefing_attempts` map that backs the
    //! resume-briefing attempt cooldown (session-ux-fixes). The full
    //! gate (idle check + cooldown + synthetic dispatch) depends on a
    //! live WardSONDB and LLM provider, so it is QEMU-verified — same
    //! convention as pending_resume_briefing_tests above.
    use super::*;
    use std::time::Duration;

    fn mgr() -> SessionManager {
        // Dummy client — never connected; these tests don't touch the DB.
        SessionManager::new(WardsonDbClient::from_url("http://127.0.0.1:1"))
    }

    #[test]
    fn unknown_session_is_never_recent() {
        let m = mgr();
        assert!(
            !m.briefing_attempt_recent("main", Duration::from_secs(3600)),
            "a session with no recorded attempt must not be suppressed"
        );
    }

    #[test]
    fn recorded_attempt_is_recent_within_a_large_window() {
        let mut m = mgr();
        m.record_briefing_attempt("main");
        assert!(
            m.briefing_attempt_recent("main", Duration::from_secs(3600)),
            "an attempt recorded just now must be inside a 1h window"
        );
    }

    #[test]
    fn recorded_attempt_is_stale_under_a_zero_window() {
        let mut m = mgr();
        m.record_briefing_attempt("main");
        assert!(
            !m.briefing_attempt_recent("main", Duration::ZERO),
            "elapsed() >= 0 must never be < a zero cooldown — the window boundary is exclusive"
        );
    }

    #[test]
    fn attempts_are_per_session() {
        let mut m = mgr();
        m.record_briefing_attempt("a");
        assert!(m.briefing_attempt_recent("a", Duration::from_secs(3600)));
        assert!(
            !m.briefing_attempt_recent("b", Duration::from_secs(3600)),
            "recording session 'a' must not suppress session 'b'"
        );
    }
}
