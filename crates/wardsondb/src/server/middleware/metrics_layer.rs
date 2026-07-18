use std::sync::Arc;

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;

use crate::server::metrics::Metrics;

/// Injects the shared Metrics handle into request extensions
/// so downstream middleware (request_logger) can access it.
pub fn inject_metrics(
    metrics: Arc<Metrics>,
) -> impl Fn(Request, Next) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>
+ Clone
+ Send {
    move |mut request: Request, next: Next| {
        let metrics = metrics.clone();
        Box::pin(async move {
            request.extensions_mut().insert(metrics);
            next.run(request).await
        })
    }
}
