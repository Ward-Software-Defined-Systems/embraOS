use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;

use crate::error::AppError;
use crate::server::AppState;
use crate::server::middleware::error_handler::JsonBody;
use crate::server::response::{ApiResponse, ApiResponseWithStatus};

#[derive(Deserialize)]
pub struct SetTtlRequest {
    pub retention_days: u64,
    #[serde(default = "default_ttl_field")]
    pub field: String,
}

fn default_ttl_field() -> String {
    "_created_at".to_string()
}

#[derive(Deserialize)]
pub struct CreateCollectionRequest {
    pub name: String,
}

const RESERVED_NAMES: &[&str] = &["_collections", "_health", "_stats"];

fn validate_collection_name(name: &str) -> Result<(), AppError> {
    if name.is_empty() {
        return Err(AppError::InvalidDocument(
            "Collection name cannot be empty".into(),
        ));
    }
    if name.starts_with('_') {
        return Err(AppError::InvalidDocument(
            "Collection name cannot start with underscore".into(),
        ));
    }
    if RESERVED_NAMES.contains(&name) {
        return Err(AppError::InvalidDocument(format!(
            "'{name}' is a reserved name"
        )));
    }
    // fjall partition names: alphanumerics, underscore, dash, dot, hash, dollar
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || "_-.#$".contains(c))
    {
        return Err(AppError::InvalidDocument(
            "Collection name can only contain alphanumerics, underscore, dash, dot, hash, or dollar"
                .into(),
        ));
    }
    Ok(())
}

pub async fn list(State(state): State<Arc<AppState>>) -> Result<Json<ApiResponse>, AppError> {
    let collections = state.storage.list_collections()?;
    let data = serde_json::to_value(&collections)?;
    Ok(Json(ApiResponse::success(data)))
}

pub async fn create(
    State(state): State<Arc<AppState>>,
    JsonBody(body): JsonBody<CreateCollectionRequest>,
) -> Result<impl IntoResponse, AppError> {
    validate_collection_name(&body.name)?;
    // create_collection commits and flushes the WAL — blocking KV work.
    let info =
        super::query::with_query_timeout(0, move || state.storage.create_collection(&body.name))
            .await?;
    let data = serde_json::to_value(&info)?;
    Ok(ApiResponseWithStatus {
        status: StatusCode::CREATED,
        response: ApiResponse::success(data),
    })
}

pub async fn get_info(
    State(state): State<Arc<AppState>>,
    Path(collection): Path<String>,
) -> Result<Json<ApiResponse>, AppError> {
    let info = state.storage.get_collection_info(&collection)?;
    let data = serde_json::to_value(&info)?;
    Ok(Json(ApiResponse::success(data)))
}

pub async fn drop_collection(
    State(state): State<Arc<AppState>>,
    Path(collection): Path<String>,
) -> Result<Json<ApiResponse>, AppError> {
    // Drop scans every doc and every index entry to delete them — one of
    // the two O(collection) admin ops (with index backfill). Offload.
    let coll = collection.clone();
    super::query::with_query_timeout(0, move || state.storage.drop_collection(&coll)).await?;
    let data = serde_json::json!({ "dropped": true, "name": collection });
    Ok(Json(ApiResponse::success(data)))
}

pub async fn get_ttl(
    State(state): State<Arc<AppState>>,
    Path(collection): Path<String>,
) -> Result<Json<ApiResponse>, AppError> {
    let ttl = state.storage.get_ttl(&collection)?;
    let data = match ttl {
        Some(config) => serde_json::to_value(&config)?,
        None => serde_json::json!({"enabled": false}),
    };
    Ok(Json(ApiResponse::success(data)))
}

pub async fn set_ttl(
    State(state): State<Arc<AppState>>,
    Path(collection): Path<String>,
    JsonBody(body): JsonBody<SetTtlRequest>,
) -> Result<Json<ApiResponse>, AppError> {
    if body.retention_days == 0 {
        return Err(AppError::InvalidDocument(
            "retention_days must be greater than 0".into(),
        ));
    }
    let config = super::query::with_query_timeout(0, move || {
        state
            .storage
            .set_ttl(&collection, body.retention_days, &body.field)
    })
    .await?;
    let data = serde_json::to_value(&config)?;
    Ok(Json(ApiResponse::success(data)))
}

pub async fn delete_ttl(
    State(state): State<Arc<AppState>>,
    Path(collection): Path<String>,
) -> Result<Json<ApiResponse>, AppError> {
    super::query::with_query_timeout(0, move || state.storage.delete_ttl(&collection)).await?;
    let data = serde_json::json!({"deleted": true});
    Ok(Json(ApiResponse::success(data)))
}

pub async fn get_storage_info(
    State(state): State<Arc<AppState>>,
    Path(collection): Path<String>,
) -> Result<Json<ApiResponse>, AppError> {
    // Meta gets + first/last-key probes are KV work — keep them off the async
    // workers under the read-path timeout, like the other KV read handlers.
    let timeout_secs = state.config.query_timeout;
    let st = state.clone();
    let (info, ttl, oldest_doc, newest_doc) =
        super::query::with_query_timeout(timeout_secs, move || {
            let info = st.storage.get_collection_info(&collection)?;
            let ttl = st.storage.get_ttl(&collection)?;

            // Get oldest/newest doc timestamps from first/last UUIDv7 keys
            // doc_count guard in get_doc_time_range prevents hang on empty partitions
            let (oldest_doc, newest_doc) = st
                .storage
                .get_doc_time_range(&collection)
                .unwrap_or((None, None));
            Ok((info, ttl, oldest_doc, newest_doc))
        })
        .await?;

    let mut data = serde_json::json!({
        "name": info.name,
        "doc_count": info.doc_count,
        "index_count": info.indexes.len(),
        "indexes": info.indexes,
        "oldest_doc": oldest_doc,
        "newest_doc": newest_doc,
    });

    if let Some(ttl_config) = ttl {
        data["ttl"] = serde_json::to_value(&ttl_config)?;
    }

    // Add bitmap scan accelerator info
    if state.storage.scan_accelerator.is_ready() {
        let stats = state.storage.scan_accelerator.stats();
        let columns: Vec<serde_json::Value> = stats
            .columns
            .iter()
            .map(|c| {
                serde_json::json!({
                    "field": c.field,
                    "cardinality": c.cardinality,
                    "memory_bytes": c.memory_bytes,
                })
            })
            .collect();
        data["scan_accelerator"] = serde_json::json!({
            "total_positions": stats.total_positions,
            "bitmap_columns": columns,
        });
    }

    Ok(Json(ApiResponse::success(data)))
}
