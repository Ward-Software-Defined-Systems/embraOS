use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum AppError {
    #[error("Collection not found: {0}")]
    CollectionNotFound(String),

    #[error("Document not found: {0}")]
    DocumentNotFound(String),

    #[error("Collection already exists: {0}")]
    CollectionExists(String),

    #[error("Index already exists: {0}")]
    IndexExists(String),

    #[error("Invalid document: {0}")]
    InvalidDocument(String),

    #[error("Invalid query: {0}")]
    InvalidQuery(String),

    #[error("Invalid index: {0}")]
    InvalidIndex(String),

    #[error("Index not found: {0}")]
    IndexNotFound(String),

    #[error("Schema violation: {0}")]
    SchemaViolation(String),

    #[error("Document conflict: {0}")]
    DocumentConflict(String),

    #[error("Invalid pipeline: {0}")]
    InvalidPipeline(String),

    #[error("Document too large")]
    DocumentTooLarge,

    #[error("Query exceeded timeout of {0} seconds")]
    QueryTimeout(u64),

    #[error(
        "Storage engine poisoned: a fatal flush/compaction error occurred. Server must be restarted."
    )]
    StoragePoisoned,

    #[error("Internal error: {0}")]
    Internal(String),
}

impl AppError {
    fn status_and_code(&self) -> (StatusCode, &'static str) {
        match self {
            AppError::CollectionNotFound(_) => (StatusCode::NOT_FOUND, "COLLECTION_NOT_FOUND"),
            AppError::DocumentNotFound(_) => (StatusCode::NOT_FOUND, "DOCUMENT_NOT_FOUND"),
            AppError::CollectionExists(_) => (StatusCode::CONFLICT, "COLLECTION_EXISTS"),
            AppError::IndexExists(_) => (StatusCode::CONFLICT, "INDEX_EXISTS"),
            AppError::InvalidDocument(_) => (StatusCode::BAD_REQUEST, "INVALID_DOCUMENT"),
            AppError::InvalidQuery(_) => (StatusCode::BAD_REQUEST, "INVALID_QUERY"),
            AppError::InvalidIndex(_) => (StatusCode::BAD_REQUEST, "INVALID_INDEX"),
            AppError::IndexNotFound(_) => (StatusCode::NOT_FOUND, "INDEX_NOT_FOUND"),
            AppError::SchemaViolation(_) => (StatusCode::BAD_REQUEST, "SCHEMA_VIOLATION"),
            AppError::DocumentConflict(_) => (StatusCode::CONFLICT, "DOCUMENT_CONFLICT"),
            AppError::InvalidPipeline(_) => (StatusCode::BAD_REQUEST, "INVALID_PIPELINE"),
            AppError::DocumentTooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "DOCUMENT_TOO_LARGE"),
            AppError::QueryTimeout(_) => (StatusCode::REQUEST_TIMEOUT, "QUERY_TIMEOUT"),
            AppError::StoragePoisoned => (StatusCode::SERVICE_UNAVAILABLE, "STORAGE_POISONED"),
            AppError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR"),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code) = self.status_and_code();
        let message = self.to_string();
        let body = serde_json::json!({
            "ok": false,
            "error": {
                "code": code,
                "message": message,
            },
            "meta": {}
        });
        (status, Json(body)).into_response()
    }
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        AppError::InvalidDocument(format!("JSON error: {e}"))
    }
}
