//! `GET /api/sessions` — list active brain sessions.
//!
//! Opens a fresh tonic Channel per request and calls apid's
//! `ListSessions` RPC (pass-through to brain). The response carries an
//! opaque `bytes payload` containing `brain::ListSessionsResponse`,
//! decoded via prost.
//!
//! Returns JSON: `{ "sessions": [{ "name", "state", "turn_count",
//! "created_at", "last_active", "has_summary", "provider" }] }`, where
//! `provider` is the provider the session is pinned to, as `/provider`
//! takes it. On failure, returns
//! `{ "sessions": [], "error": "..." }` so the chat-mobile sheet can
//! render a graceful error instead of 500ing.
//!
//! Session *creation* and *switching* go through the existing
//! `/ws/chat` SlashCommand variant (`/new <name>` and `/switch <name>`)
//! — no new REST surface needed for those.

use axum::Json;
use axum::extract::State;
use embra_common::proto::apid::ListSessionsRequest;
use embra_common::proto::apid::embra_api_client::EmbraApiClient;
use embra_common::proto::brain;
use prost::Message as ProstMessage;
use serde_json::{Value, json};
use tonic::transport::Channel;

use crate::state::AppState;

pub async fn api_sessions_list(State(st): State<AppState>) -> Json<Value> {
    match fetch_sessions(&st.apid_addr).await {
        Ok(sessions) => Json(json!({ "sessions": sessions })),
        Err(e) => Json(json!({ "sessions": [], "error": e })),
    }
}

async fn fetch_sessions(apid_addr: &str) -> Result<Vec<Value>, String> {
    let endpoint = Channel::from_shared(apid_addr.to_string())
        .map_err(|e| format!("invalid apid endpoint: {e}"))?;
    let channel = endpoint.connect_lazy();
    let mut client = EmbraApiClient::new(channel);

    let response = client
        .list_sessions(ListSessionsRequest {})
        .await
        .map_err(|e| format!("ListSessions RPC failed: {e}"))?;

    let bytes = response.into_inner().payload;
    let brain_resp = brain::ListSessionsResponse::decode(bytes.as_slice())
        .map_err(|e| format!("decode brain response: {e}"))?;

    Ok(brain_resp.sessions.into_iter().map(session_json).collect())
}

/// One row of the sheet's JSON.
fn session_json(s: brain::SessionInfo) -> Value {
    json!({
        "name": s.name,
        "state": s.state,
        "turn_count": s.turn_count,
        "created_at": s.created_at.map(|t| t.iso8601).unwrap_or_default(),
        "last_active": s.last_active.map(|t| t.iso8601).unwrap_or_default(),
        "has_summary": s.has_summary,
        "provider": s.provider,
    })
}

#[cfg(test)]
mod tests {
    use super::session_json;
    use embra_common::proto::brain::SessionInfo;
    use serde_json::json;

    /// The mobile sheet shows the provider a session is pinned to; the
    /// field travels from the brain's `SessionInfo` unchanged.
    #[test]
    fn the_session_json_carries_the_provider() {
        let row = session_json(SessionInfo {
            name: "qwen-eval".into(),
            state: "Detached".into(),
            turn_count: 14,
            created_at: None,
            last_active: None,
            has_summary: false,
            provider: "lm_studio".into(),
        });
        assert_eq!(
            row,
            json!({
                "name": "qwen-eval", "state": "Detached", "turn_count": 14,
                "created_at": "", "last_active": "", "has_summary": false,
                "provider": "lm_studio",
            })
        );
    }
}
