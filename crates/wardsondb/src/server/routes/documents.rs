use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::Value;

use crate::error::AppError;
use crate::server::AppState;
use crate::server::middleware::error_handler::JsonBody;
use crate::server::response::{ApiResponse, ApiResponseWithStatus};

// Offload rule for every handler in routes/: KV writes run under
// with_query_timeout(0, ...) — off the async workers, no timeout (a
// spawn_blocking task isn't cancellable, and a mutation must never report
// failure for work that then completes). KV reads beyond a _meta point-get
// run under with_query_timeout(query_timeout, ...). Pure _meta point reads
// stay inline. Even O(1) writes stall for seconds behind a RocksDB
// write-stall or memtable flush — enough concurrent ones starve the runtime.

pub async fn create(
    State(state): State<Arc<AppState>>,
    Path(collection): Path<String>,
    JsonBody(body): JsonBody<Value>,
) -> Result<impl IntoResponse, AppError> {
    let st = state.clone();
    let doc =
        super::query::with_query_timeout(0, move || st.storage.insert_document(&collection, body))
            .await?;
    state.metrics.record_insert();
    Ok(ApiResponseWithStatus {
        status: StatusCode::CREATED,
        response: ApiResponse::success(doc),
    })
}

#[derive(Deserialize)]
pub struct BulkInsertRequest {
    pub documents: Vec<Value>,
}

pub async fn bulk_create(
    State(state): State<Arc<AppState>>,
    Path(collection): Path<String>,
    JsonBody(body): JsonBody<BulkInsertRequest>,
) -> Result<impl IntoResponse, AppError> {
    let st = state.clone();
    let (inserted, errors) = super::query::with_query_timeout(0, move || {
        st.storage
            .bulk_insert_documents(&collection, body.documents)
    })
    .await?;
    state.metrics.record_bulk_insert(inserted);
    let data = serde_json::json!({
        "inserted": inserted,
        "errors": errors,
    });
    Ok(ApiResponseWithStatus {
        status: StatusCode::CREATED,
        response: ApiResponse::success(data),
    })
}

pub async fn get_by_id(
    State(state): State<Arc<AppState>>,
    Path((collection, id)): Path<(String, String)>,
) -> Result<Json<ApiResponse>, AppError> {
    // Point reads are KV work that can stall behind compactions/scans —
    // keep them off the async workers like query/aggregate.
    let timeout_secs = state.config.query_timeout;
    let doc = super::query::with_query_timeout(timeout_secs, move || {
        state.storage.get_document(&collection, &id)
    })
    .await?;
    Ok(Json(ApiResponse::success(doc)))
}

pub async fn replace(
    State(state): State<Arc<AppState>>,
    Path((collection, id)): Path<(String, String)>,
    JsonBody(body): JsonBody<Value>,
) -> Result<Json<ApiResponse>, AppError> {
    let doc = super::query::with_query_timeout(0, move || {
        state.storage.replace_document(&collection, &id, body)
    })
    .await?;
    Ok(Json(ApiResponse::success(doc)))
}

pub async fn partial_update(
    State(state): State<Arc<AppState>>,
    Path((collection, id)): Path<(String, String)>,
    JsonBody(body): JsonBody<Value>,
) -> Result<Json<ApiResponse>, AppError> {
    let doc = super::query::with_query_timeout(0, move || {
        state
            .storage
            .partial_update_document(&collection, &id, body)
    })
    .await?;
    Ok(Json(ApiResponse::success(doc)))
}

pub async fn delete(
    State(state): State<Arc<AppState>>,
    Path((collection, id)): Path<(String, String)>,
) -> Result<Json<ApiResponse>, AppError> {
    let st = state.clone();
    super::query::with_query_timeout(0, move || st.storage.delete_document(&collection, &id))
        .await?;
    state.metrics.record_delete();
    let data = serde_json::json!({ "deleted": true });
    Ok(Json(ApiResponse::success(data)))
}
