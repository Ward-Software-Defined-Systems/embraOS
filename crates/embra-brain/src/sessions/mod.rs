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

pub struct SessionManager {
    db: WardsonDbClient,
    pub active_session: Option<String>,
}

impl SessionManager {
    pub fn new(db: WardsonDbClient) -> Self {
        Self {
            db,
            active_session: None,
        }
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

        let now = Utc::now();
        let meta = SessionMeta {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            state: SessionState::Active,
            created_at: now,
            last_active: now,
            provider: None,
            model: None,
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
        let results = self.db.query(&collection, &serde_json::json!({})).await?;
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
            .query(&collection, &serde_json::json!({}))
            .await
            .map_err(|e| SessionError::Io(e.into()))?;

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

            if let Some(id) = doc.get("_id").or(doc.get("id")).and_then(|v| v.as_str()) {
                self.db
                    .update(&collection, id, &serde_json::to_value(&history)
                        .map_err(|e| SessionError::Io(e.into()))?)
                    .await
                    .map_err(|e| SessionError::Io(e.into()))?;
            }
        }

        // Update last_active on meta. Non-fatal if it fails — the history
        // write is the source of truth for append_message's success.
        let meta_collection = format!("sessions.{}.meta", name);
        let meta_results = self
            .db
            .query(&meta_collection, &serde_json::json!({}))
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
        let collections = self.db.list_collections().await?;
        let mut sessions = Vec::new();

        for col in collections {
            if col.starts_with("sessions.") && col.ends_with(".meta") {
                let results = self.db.query(&col, &serde_json::json!({})).await?;
                for doc in results {
                    if let Ok(meta) = serde_json::from_value::<SessionMeta>(doc) {
                        sessions.push(meta);
                    }
                }
            }
        }

        Ok(sessions)
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
            .query(&meta_collection, &serde_json::json!({}))
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
