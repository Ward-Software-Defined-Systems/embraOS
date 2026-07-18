use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize)]
pub struct ApiResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    pub meta: ResponseMeta,
}

#[derive(Debug, Serialize, Default)]
pub struct ResponseMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub returned_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docs_scanned: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub groups: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_used: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scan_strategy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

impl ApiResponse {
    pub fn success(data: Value) -> Self {
        ApiResponse {
            ok: true,
            data: Some(data),
            meta: ResponseMeta::default(),
        }
    }

    pub fn success_with_meta(data: Value, meta: ResponseMeta) -> Self {
        ApiResponse {
            ok: true,
            data: Some(data),
            meta,
        }
    }

    #[allow(dead_code)]
    pub fn ok_no_data() -> Self {
        ApiResponse {
            ok: true,
            data: None,
            meta: ResponseMeta::default(),
        }
    }
}

impl IntoResponse for ApiResponse {
    fn into_response(self) -> Response {
        (StatusCode::OK, Json(self)).into_response()
    }
}

/// Helper to create a response with a specific status code
pub struct ApiResponseWithStatus {
    pub status: StatusCode,
    pub response: ApiResponse,
}

impl IntoResponse for ApiResponseWithStatus {
    fn into_response(self) -> Response {
        (self.status, Json(self.response)).into_response()
    }
}
