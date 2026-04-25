//! REST API endpoints for embra-apid.
//! Provides a simple HTTP interface for health checks and basic management.

use axum::{routing::get, Router, Json};
use serde_json::json;

pub fn build_router() -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "data": {
            "status": "healthy",
            "service": "embra-apid"
        }
    }))
}

async fn version() -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "data": {
            "embraos_version": env!("CARGO_PKG_VERSION"),
            "service": "embra-apid"
        }
    }))
}
