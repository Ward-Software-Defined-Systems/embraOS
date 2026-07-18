use std::sync::Arc;
use std::time::Instant;

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use tracing::{info, warn};

use crate::server::metrics::Metrics;

/// Target name for per-request logs. The terminal layer filters this out
/// unless --verbose is set; the file layer always includes it.
const REQUEST_LOG_TARGET: &str = "wardsondb::requests";

pub async fn request_logger(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let uri = request.uri().path().to_string();
    let start = Instant::now();

    // Try to get metrics from extensions (set by the metrics layer)
    let metrics = request.extensions().get::<Arc<Metrics>>().cloned();

    let response = next.run(request).await;

    let duration = start.elapsed();
    let duration_us = duration.as_micros() as u64;
    let duration_ms = duration_us as f64 / 1000.0;
    let status = response.status().as_u16();

    // Record in metrics (always, regardless of log filtering)
    if let Some(m) = metrics {
        m.record_request(status, duration_us);
    }

    // Structured log per request — uses a specific target so terminal
    // can filter it out while the log file keeps it.
    if status >= 500 {
        warn!(
            target: REQUEST_LOG_TARGET,
            method = %method,
            path = %uri,
            status = status,
            duration_ms = format!("{duration_ms:.2}"),
            "{method} {uri} {status} {duration_ms:.2}ms",
        );
    } else {
        info!(
            target: REQUEST_LOG_TARGET,
            method = %method,
            path = %uri,
            status = status,
            duration_ms = format!("{duration_ms:.2}"),
            "{method} {uri} {status} {duration_ms:.2}ms",
        );
    }

    response
}
