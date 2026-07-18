pub mod metrics;
pub mod middleware;
pub mod response;
pub mod routes;
pub mod ttl_worker;

use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::routing::{delete, get, post};

use crate::config::Config;
use crate::engine::storage::Storage;

use self::metrics::Metrics;
use self::middleware::auth::auth_middleware;
use self::middleware::metrics_layer::inject_metrics;
use self::middleware::request_id::request_id_middleware;
use self::middleware::request_logger::request_logger;

pub struct AppState {
    pub storage: Storage,
    pub config: Config,
    pub started_at: Instant,
    pub metrics: Arc<Metrics>,
    pub api_keys: Vec<String>,
}

pub fn build_router(state: Arc<AppState>) -> Router {
    let metrics = state.metrics.clone();
    let auth_state = state.clone();
    // Axum's default body cap is 2 MB — below the 16 MB single-document
    // limit, and bulk inserts need headroom beyond that.
    let max_body_bytes = (state.config.max_body_mb as usize).saturating_mul(1024 * 1024);

    Router::new()
        // System
        .route("/", get(routes::system::info))
        .route("/_health", get(routes::system::health))
        .route("/_stats", get(routes::system::stats))
        .route("/_metrics", get(routes::system::metrics_endpoint))
        // Collections
        .route(
            "/_collections",
            get(routes::collections::list).post(routes::collections::create),
        )
        .route(
            "/{collection}",
            get(routes::collections::get_info).delete(routes::collections::drop_collection),
        )
        // Documents
        .route("/{collection}/docs", post(routes::documents::create))
        .route(
            "/{collection}/docs/_bulk",
            post(routes::documents::bulk_create),
        )
        .route(
            "/{collection}/docs/{id}",
            get(routes::documents::get_by_id)
                .put(routes::documents::replace)
                .patch(routes::documents::partial_update)
                .delete(routes::documents::delete),
        )
        // Bulk operations
        .route(
            "/{collection}/docs/_delete_by_query",
            post(routes::query::delete_by_query),
        )
        .route(
            "/{collection}/docs/_update_by_query",
            post(routes::query::update_by_query),
        )
        // Query
        .route("/{collection}/query", post(routes::query::search))
        .route("/{collection}/aggregate", post(routes::query::aggregate))
        .route("/{collection}/distinct", post(routes::query::distinct))
        // Indexes
        .route(
            "/{collection}/indexes",
            get(routes::indexes::list).post(routes::indexes::create),
        )
        .route(
            "/{collection}/indexes/{name}",
            delete(routes::indexes::drop_index),
        )
        // TTL
        .route(
            "/{collection}/ttl",
            get(routes::collections::get_ttl)
                .put(routes::collections::set_ttl)
                .delete(routes::collections::delete_ttl),
        )
        // Storage info
        .route(
            "/{collection}/storage",
            get(routes::collections::get_storage_info),
        )
        .layer(axum::extract::DefaultBodyLimit::max(max_body_bytes))
        .layer(axum::middleware::from_fn(request_logger))
        .layer(axum::middleware::from_fn(request_id_middleware))
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            auth_middleware,
        ))
        .layer(axum::middleware::from_fn(inject_metrics(metrics)))
        .with_state(state)
}
