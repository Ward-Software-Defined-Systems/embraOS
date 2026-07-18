use std::sync::Arc;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;
use tracing::warn;

use crate::server::AppState;

/// Constant-time API key comparison to prevent timing attacks.
fn key_matches(candidate: &str, stored: &str) -> bool {
    if candidate.len() != stored.len() {
        // Compare against stored key anyway to avoid leaking length info via timing
        let dummy = vec![0u8; candidate.len()];
        let _ = candidate.as_bytes().ct_eq(&dummy);
        return false;
    }
    candidate.as_bytes().ct_eq(stored.as_bytes()).into()
}

/// Authentication middleware. If API keys are configured, all endpoints except
/// `/_health` and optionally `/_metrics` require a valid key.
pub async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    // If no API keys configured, auth is disabled
    if state.api_keys.is_empty() {
        return next.run(request).await;
    }

    let path = request.uri().path();

    // /_health is always unauthenticated (for load balancer health checks)
    if path == "/_health" {
        return next.run(request).await;
    }

    // /_metrics is unauthenticated if --metrics-public is set
    if path == "/_metrics" && state.config.metrics_public {
        return next.run(request).await;
    }

    // Check for API key in Authorization header or X-API-Key header
    let api_key = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string())
        .or_else(|| {
            request
                .headers()
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        });

    let authorized = match &api_key {
        Some(key) => state.api_keys.iter().any(|stored| key_matches(key, stored)),
        None => false,
    };

    if authorized {
        next.run(request).await
    } else {
        warn!(
            path = path,
            "Unauthorized request — invalid or missing API key"
        );
        let body = serde_json::json!({
            "ok": false,
            "error": {
                "code": "UNAUTHORIZED",
                "message": "Valid API key required"
            },
            "meta": {}
        });
        (StatusCode::UNAUTHORIZED, Json(body)).into_response()
    }
}
