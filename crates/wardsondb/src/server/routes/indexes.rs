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
pub struct CreateIndexRequest {
    pub name: String,
    /// Single field (backward compat).
    pub field: Option<String>,
    /// Multiple fields for compound indexes.
    pub fields: Option<Vec<String>>,
}

/// List indexes for a collection
pub async fn list(
    State(state): State<Arc<AppState>>,
    Path(collection): Path<String>,
) -> Result<Json<ApiResponse>, AppError> {
    let indexes = state.storage.list_indexes(&collection)?;
    let data = serde_json::to_value(&indexes)?;
    Ok(Json(ApiResponse::success(data)))
}

/// Create an index on one or more fields
pub async fn create(
    State(state): State<Arc<AppState>>,
    Path(collection): Path<String>,
    JsonBody(body): JsonBody<CreateIndexRequest>,
) -> Result<impl IntoResponse, AppError> {
    // Validate index name
    if body.name.is_empty() {
        return Err(AppError::InvalidIndex("Index name cannot be empty".into()));
    }
    // Only allow safe characters in index name
    if !body
        .name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        return Err(AppError::InvalidIndex(
            "Index name may only contain alphanumeric characters, underscores, and hyphens".into(),
        ));
    }

    // Resolve fields: accept `fields` array or `field` string
    let fields: Vec<String> = if let Some(fields) = body.fields {
        fields
    } else if let Some(field) = body.field {
        vec![field]
    } else {
        return Err(AppError::InvalidIndex(
            "Either 'field' or 'fields' must be provided".into(),
        ));
    };

    // Validate fields
    if fields.is_empty() {
        return Err(AppError::InvalidIndex(
            "At least one field path is required".into(),
        ));
    }
    for f in &fields {
        if f.is_empty() {
            return Err(AppError::InvalidIndex("Field path cannot be empty".into()));
        }
    }

    // Backfill scans the whole collection — never run it on an async worker.
    let name = body.name;
    let def = super::query::with_query_timeout(0, move || {
        state.storage.create_index(&collection, &name, &fields)
    })
    .await?;

    let data = serde_json::to_value(&def)?;
    Ok(ApiResponseWithStatus {
        status: StatusCode::CREATED,
        response: ApiResponse::success(data),
    })
}

/// Drop an index by name
pub async fn drop_index(
    State(state): State<Arc<AppState>>,
    Path((collection, name)): Path<(String, String)>,
) -> Result<Json<ApiResponse>, AppError> {
    // Drops scan and delete the whole index partition — offload like create.
    let op_name = name.clone();
    super::query::with_query_timeout(0, move || state.storage.drop_index(&collection, &op_name))
        .await?;
    let data = serde_json::json!({ "dropped": true, "name": name });
    Ok(Json(ApiResponse::success(data)))
}
