use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::brain::Message;
use crate::db::WardsonDbClient;

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHistory {
    pub session_name: String,
    pub turns: Vec<Message>,
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
        };

        let history = SessionHistory {
            session_name: name.to_string(),
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

    pub async fn append_message(&self, name: &str, message: &Message) -> Result<()> {
        let collection = format!("sessions.{}.history", name);
        let results = self.db.query(&collection, &serde_json::json!({})).await?;

        if let Some(doc) = results.into_iter().next() {
            let mut history: SessionHistory = serde_json::from_value(doc.clone())?;
            history.turns.push(message.clone());

            // Get the document ID
            if let Some(id) = doc.get("_id").or(doc.get("id")).and_then(|v| v.as_str()) {
                self.db
                    .update(&collection, id, &serde_json::to_value(&history)?)
                    .await?;
            }
        }

        // Update last_active on meta
        let meta_collection = format!("sessions.{}.meta", name);
        let meta_results = self
            .db
            .query(&meta_collection, &serde_json::json!({}))
            .await?;
        if let Some(meta_doc) = meta_results.into_iter().next() {
            let id = meta_doc
                .get("_id")
                .or(meta_doc.get("id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if let Some(id) = id {
                let mut meta: SessionMeta = serde_json::from_value(meta_doc)?;
                meta.last_active = Utc::now();
                self.db
                    .update(&meta_collection, &id, &serde_json::to_value(&meta)?)
                    .await?;
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
