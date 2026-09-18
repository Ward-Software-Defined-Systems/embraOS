//! REST API endpoints for embra-apid.
//! Provides a simple HTTP interface for health checks and basic management.

use std::time::Duration;

use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
use embra_common::proto::brain::GetSystemStatusRequest;
use serde_json::{json, Value};

use crate::proxy::BackendConnections;

/// How long `/status` waits for the brain (connect + RPC) before answering
/// 503 — the web console polls this every 5 s, so it must never hang.
const STATUS_TIMEOUT: Duration = Duration::from_secs(2);

pub fn build_router(backends: BackendConnections) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/status", get(status))
        .with_state(backends)
}

async fn health() -> Json<Value> {
    Json(json!({
        "ok": true,
        "data": {
            "status": "healthy",
            "service": "embra-apid"
        }
    }))
}

async fn version() -> Json<Value> {
    Json(json!({
        "ok": true,
        "data": {
            "embraos_version": env!("CARGO_PKG_VERSION"),
            "service": "embra-apid"
        }
    }))
}

/// `GET /status` — the brain's `GetSystemStatus` as JSON: version, uptime,
/// WardSONDB health and the `services` map (which carries the active LLM
/// provider's endpoint probe as `llm-provider` / `llm-provider.detail`).
/// 503 `{ok:false}` when the brain is unreachable or slow.
async fn status(State(backends): State<BackendConnections>) -> (StatusCode, Json<Value>) {
    match tokio::time::timeout(STATUS_TIMEOUT, fetch_brain_status(&backends)).await {
        Ok(Ok(data)) => (StatusCode::OK, Json(json!({ "ok": true, "data": data }))),
        Ok(Err(e)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "ok": false, "error": e })),
        ),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "ok": false, "error": "embra-brain status timed out" })),
        ),
    }
}

async fn fetch_brain_status(backends: &BackendConnections) -> Result<Value, String> {
    let mut brain = backends.brain_client().await.map_err(|s| s.to_string())?;
    let resp = brain
        .get_system_status(GetSystemStatusRequest {})
        .await
        .map_err(|s| s.to_string())?
        .into_inner();
    Ok(json!({
        "version": resp.version,
        "uptime_seconds": resp.uptime_seconds,
        "wardsondb_status": resp.wardsondb_status,
        "services": resp.services,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn status_answers_503_when_the_brain_is_unreachable() {
        // Nothing listens on port 1: connect fails fast, the handler must
        // report it instead of hanging or panicking.
        let backends = BackendConnections::new(
            "http://127.0.0.1:1".to_string(),
            "http://127.0.0.1:1".to_string(),
        );
        let (code, Json(body)) = status(State(backends)).await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ok"], false);
        assert!(body["error"].as_str().is_some_and(|e| !e.is_empty()));
    }
}
