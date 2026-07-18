use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, State};
use serde::Deserialize;
use serde_json::Value;

use crate::error::AppError;
use crate::query::aggregate::{AggregateRequest, execute_aggregate};
use crate::query::executor::execute_query;
use crate::query::filter::parse_filter;
use crate::query::parser::{QueryRequest, parse_query};
use crate::server::AppState;
use crate::server::middleware::error_handler::JsonBody;
use crate::server::response::{ApiResponse, ResponseMeta};

/// Run a blocking closure off the async workers, with an optional timeout
/// (`timeout_secs == 0` = no timeout). NOTE: a timed-out spawn_blocking task
/// is NOT cancelled — it keeps running to completion. Reads use the
/// configured query timeout; mutations must pass 0 so the client never sees
/// a failure for work that then completes server-side.
pub(crate) async fn with_query_timeout<F, R>(timeout_secs: u64, f: F) -> Result<R, AppError>
where
    F: FnOnce() -> Result<R, AppError> + Send + 'static,
    R: Send + 'static,
{
    if timeout_secs == 0 {
        return tokio::task::spawn_blocking(f)
            .await
            .map_err(|e| AppError::Internal(format!("Task join error: {e}")))?;
    }

    let timeout = Duration::from_secs(timeout_secs);
    match tokio::time::timeout(timeout, tokio::task::spawn_blocking(f)).await {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => Err(AppError::Internal(format!("Task join error: {e}"))),
        Err(_) => Err(AppError::QueryTimeout(timeout_secs)),
    }
}

pub async fn search(
    State(state): State<Arc<AppState>>,
    Path(collection): Path<String>,
    JsonBody(body): JsonBody<QueryRequest>,
) -> Result<Json<ApiResponse>, AppError> {
    let start = Instant::now();
    let query = parse_query(body, state.config.max_query_limit, &collection)?;
    let count_only = query.count_only;
    let timeout_secs = state.config.query_timeout;

    let st = state.clone();
    let coll = collection.clone();
    let result = with_query_timeout(timeout_secs, move || {
        execute_query(&st.storage, &coll, &query)
    })
    .await?;

    state.metrics.record_query();
    let duration_ms = (start.elapsed().as_secs_f64() * 1_000_000.0).round() / 1000.0;

    let returned_count = result.docs.len() as u64;

    if count_only {
        let count = result.total_count.unwrap_or(0);
        let data = serde_json::json!({ "count": count });
        Ok(Json(ApiResponse::success_with_meta(
            data,
            ResponseMeta {
                duration_ms: Some(duration_ms),
                total_count: Some(count),
                docs_scanned: Some(result.docs_scanned),
                index_used: result.index_used,
                scan_strategy: result.scan_strategy,
                ..Default::default()
            },
        )))
    } else {
        // The docs are already Values — move them into the response instead
        // of deep-cloning the whole page through to_value.
        let data = Value::Array(result.docs);
        Ok(Json(ApiResponse::success_with_meta(
            data,
            ResponseMeta {
                duration_ms: Some(duration_ms),
                total_count: result.total_count,
                returned_count: Some(returned_count),
                docs_scanned: Some(result.docs_scanned),
                index_used: result.index_used,
                scan_strategy: result.scan_strategy,
                has_more: if result.has_more { Some(true) } else { None },
                next_cursor: result.next_cursor,
                ..Default::default()
            },
        )))
    }
}

pub async fn aggregate(
    State(state): State<Arc<AppState>>,
    Path(collection): Path<String>,
    JsonBody(body): JsonBody<AggregateRequest>,
) -> Result<Json<ApiResponse>, AppError> {
    let start = Instant::now();
    let timeout_secs = state.config.query_timeout;

    let st = state.clone();
    let coll = collection.clone();
    let result = with_query_timeout(timeout_secs, move || {
        execute_aggregate(&st.storage, &coll, &body)
    })
    .await?;

    state.metrics.record_query();
    let duration_ms = (start.elapsed().as_secs_f64() * 1_000_000.0).round() / 1000.0;

    let data = Value::Array(result.docs);
    Ok(Json(ApiResponse::success_with_meta(
        data,
        ResponseMeta {
            duration_ms: Some(duration_ms),
            docs_scanned: Some(result.docs_scanned),
            groups: Some(result.groups),
            index_used: result.index_used,
            scan_strategy: result.scan_strategy,
            ..Default::default()
        },
    )))
}

#[derive(Deserialize)]
pub struct DeleteByQueryRequest {
    pub filter: Value,
}

pub async fn delete_by_query(
    State(state): State<Arc<AppState>>,
    Path(collection): Path<String>,
    JsonBody(body): JsonBody<DeleteByQueryRequest>,
) -> Result<Json<ApiResponse>, AppError> {
    let start = Instant::now();
    let filter = parse_filter(&body.filter)?;
    let st = state.clone();
    let deleted =
        with_query_timeout(0, move || st.storage.delete_by_query(&collection, &filter)).await?;
    state
        .metrics
        .lifetime_deletes
        .fetch_add(deleted, std::sync::atomic::Ordering::Relaxed);
    let duration_ms = (start.elapsed().as_secs_f64() * 1_000_000.0).round() / 1000.0;

    let data = serde_json::json!({ "deleted": deleted });
    Ok(Json(ApiResponse::success_with_meta(
        data,
        ResponseMeta {
            duration_ms: Some(duration_ms),
            ..Default::default()
        },
    )))
}

#[derive(Deserialize)]
pub struct DistinctRequest {
    pub field: String,
    pub filter: Option<Value>,
    #[serde(default = "default_distinct_limit")]
    pub limit: usize,
}

fn default_distinct_limit() -> usize {
    1000
}

pub async fn distinct(
    State(state): State<Arc<AppState>>,
    Path(collection): Path<String>,
    JsonBody(body): JsonBody<DistinctRequest>,
) -> Result<Json<ApiResponse>, AppError> {
    let start = Instant::now();

    let filter = match &body.filter {
        Some(f) => Some(parse_filter(f)?),
        None => None,
    };

    let timeout_secs = state.config.query_timeout;
    let st = state.clone();
    let coll = collection.clone();
    let field = body.field.clone();
    let limit = body.limit;
    let result = with_query_timeout(timeout_secs, move || {
        crate::query::distinct::execute_distinct(&st.storage, &coll, &field, filter.as_ref(), limit)
    })
    .await?;

    state.metrics.record_query();
    let duration_ms = (start.elapsed().as_secs_f64() * 1_000_000.0).round() / 1000.0;

    // Built by hand: json! expands interpolations through to_value, which
    // would deep-clone the (up to 1000-entry) values Vec. Same key order.
    let mut data = serde_json::Map::with_capacity(4);
    data.insert("field".into(), Value::String(body.field));
    data.insert("values".into(), Value::Array(result.values));
    data.insert("count".into(), Value::from(result.count));
    data.insert("truncated".into(), Value::Bool(result.truncated));
    let data = Value::Object(data);

    Ok(Json(ApiResponse::success_with_meta(
        data,
        ResponseMeta {
            duration_ms: Some(duration_ms),
            docs_scanned: Some(result.docs_scanned),
            index_used: result.index_used,
            ..Default::default()
        },
    )))
}

#[derive(Deserialize)]
pub struct UpdateByQueryRequest {
    pub filter: Value,
    pub update: Value,
}

pub async fn update_by_query(
    State(state): State<Arc<AppState>>,
    Path(collection): Path<String>,
    JsonBody(body): JsonBody<UpdateByQueryRequest>,
) -> Result<Json<ApiResponse>, AppError> {
    let start = Instant::now();
    let filter = parse_filter(&body.filter)?;
    let st = state.clone();
    let update = body.update;
    let updated = with_query_timeout(0, move || {
        st.storage.update_by_query(&collection, &filter, &update)
    })
    .await?;
    let duration_ms = (start.elapsed().as_secs_f64() * 1_000_000.0).round() / 1000.0;

    let data = serde_json::json!({ "updated": updated });
    Ok(Json(ApiResponse::success_with_meta(
        data,
        ResponseMeta {
            duration_ms: Some(duration_ms),
            ..Default::default()
        },
    )))
}
