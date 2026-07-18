use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::Json;
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};

use crate::error::AppError;
use crate::server::AppState;
use crate::server::response::ApiResponse;
use crate::server::ttl_worker::LAST_TTL_RUN;

pub async fn info(State(state): State<Arc<AppState>>) -> Result<Json<ApiResponse>, AppError> {
    let uptime = state.started_at.elapsed().as_secs();
    let data = serde_json::json!({
        "name": "WardSONDB",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": uptime,
        "data_directory": state.config.data_dir,
    });
    Ok(Json(ApiResponse::success(data)))
}

pub async fn health(State(state): State<Arc<AppState>>) -> Json<ApiResponse> {
    let poisoned = state.storage.is_poisoned();
    let status = if poisoned { "degraded" } else { "healthy" };
    let write_pressure = state.metrics.write_pressure();
    let scan_accel_ready = state.storage.scan_accelerator.is_ready();
    let mut data = serde_json::json!({
        "status": status,
        "write_pressure": write_pressure,
        "scan_accelerator_ready": scan_accel_ready,
    });

    if poisoned {
        data["warning"] = serde_json::json!(
            "Storage engine poisoned: a fatal flush/compaction error occurred. \
             Reads may work, writes will fail. Restart required."
        );
    }

    Json(ApiResponse::success(data))
}

pub async fn stats(State(state): State<Arc<AppState>>) -> Result<Json<ApiResponse>, AppError> {
    let collections = state.storage.list_collections()?;
    let total_docs: u64 = collections.iter().map(|c| c.doc_count).sum();
    let m = &state.metrics;
    let mem = &state.storage.memory_config;

    let ttl_configs = state.storage.get_all_ttl_configs().unwrap_or_default();
    let last_ttl_run = LAST_TTL_RUN.load(Ordering::Relaxed);
    let accel_stats = state.storage.scan_accelerator.stats();

    let data = serde_json::json!({
        "collection_count": collections.len(),
        "total_documents": total_docs,
        "uptime_seconds": state.started_at.elapsed().as_secs(),
        "storage_poisoned": state.storage.is_poisoned(),
        "memory_config": {
            "cache_size_mb": mem.cache_size / (1024 * 1024),
            "max_write_buffer_mb": mem.max_write_buffer_size / (1024 * 1024),
            "max_memtable_mb": mem.max_memtable_size / (1024 * 1024),
            "flush_workers": mem.flush_workers,
            "compaction_workers": mem.compaction_workers,
        },
        "lifetime": {
            "requests": m.lifetime_requests.load(Ordering::Relaxed),
            "inserts": m.lifetime_inserts.load(Ordering::Relaxed),
            "queries": m.lifetime_queries.load(Ordering::Relaxed),
            "deletes": m.lifetime_deletes.load(Ordering::Relaxed),
        },
        "ttl": {
            "active_policies": ttl_configs.len(),
            "last_cleanup_run": if last_ttl_run > 0 { Some(last_ttl_run) } else { None },
        },
        "scan_accelerator": {
            "ready": accel_stats.ready,
            "total_positions": accel_stats.total_positions,
            "positions_by_collection": accel_stats.collections.iter().map(|c| serde_json::json!({
                "collection": c.collection,
                "positions": c.positions,
            })).collect::<Vec<_>>(),
            "memory_bytes": accel_stats.memory_bytes,
            "memory_budget_bytes": accel_stats.memory_budget_bytes,
            "over_budget": accel_stats.over_budget,
            "bitmap_columns": accel_stats.columns.iter().map(|c| serde_json::json!({
                "field": c.field,
                "cardinality": c.cardinality,
                "memory_bytes": c.memory_bytes,
            })).collect::<Vec<_>>(),
        }
    });
    Ok(Json(ApiResponse::success(data)))
}

pub async fn metrics_endpoint(State(state): State<Arc<AppState>>) -> Result<Response, AppError> {
    let m = &state.metrics;
    let uptime = state.started_at.elapsed().as_secs();

    let collections = state.storage.list_collections()?;
    let total_docs: u64 = collections.iter().map(|c| c.doc_count).sum();

    let ttl_configs = state.storage.get_all_ttl_configs().unwrap_or_default();

    let poisoned: u64 = if state.storage.is_poisoned() { 1 } else { 0 };

    let body = format!(
        "# HELP wardsondb_uptime_seconds Server uptime in seconds\n\
         # TYPE wardsondb_uptime_seconds gauge\n\
         wardsondb_uptime_seconds {uptime}\n\
         # HELP wardsondb_documents_total Total documents across all collections\n\
         # TYPE wardsondb_documents_total gauge\n\
         wardsondb_documents_total {total_docs}\n\
         # HELP wardsondb_collections_total Total number of collections\n\
         # TYPE wardsondb_collections_total gauge\n\
         wardsondb_collections_total {collections}\n\
         # HELP wardsondb_requests_total Lifetime request count\n\
         # TYPE wardsondb_requests_total counter\n\
         wardsondb_requests_total {requests}\n\
         # HELP wardsondb_inserts_total Lifetime insert count\n\
         # TYPE wardsondb_inserts_total counter\n\
         wardsondb_inserts_total {inserts}\n\
         # HELP wardsondb_queries_total Lifetime query count\n\
         # TYPE wardsondb_queries_total counter\n\
         wardsondb_queries_total {queries}\n\
         # HELP wardsondb_deletes_total Lifetime delete count\n\
         # TYPE wardsondb_deletes_total counter\n\
         wardsondb_deletes_total {deletes}\n\
         # HELP wardsondb_storage_poisoned Whether the storage engine is poisoned (0 or 1)\n\
         # TYPE wardsondb_storage_poisoned gauge\n\
         wardsondb_storage_poisoned {poisoned}\n\
         # HELP wardsondb_ttl_active_policies Number of active TTL policies\n\
         # TYPE wardsondb_ttl_active_policies gauge\n\
         wardsondb_ttl_active_policies {ttl_policies}\n",
        collections = collections.len(),
        requests = m.lifetime_requests.load(Ordering::Relaxed),
        inserts = m.lifetime_inserts.load(Ordering::Relaxed),
        queries = m.lifetime_queries.load(Ordering::Relaxed),
        deletes = m.lifetime_deletes.load(Ordering::Relaxed),
        ttl_policies = ttl_configs.len(),
    );

    Ok((
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response())
}
