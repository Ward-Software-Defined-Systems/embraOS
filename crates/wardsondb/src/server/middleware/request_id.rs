use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use uuid::Uuid;

pub static X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

pub async fn request_id_middleware(mut request: Request, next: Next) -> Response {
    let request_id = Uuid::now_v7().to_string();
    request.headers_mut().insert(
        X_REQUEST_ID.clone(),
        HeaderValue::from_str(&request_id).unwrap_or_else(|_| HeaderValue::from_static("unknown")),
    );

    let mut response = next.run(request).await;
    response.headers_mut().insert(
        X_REQUEST_ID.clone(),
        HeaderValue::from_str(&request_id).unwrap_or_else(|_| HeaderValue::from_static("unknown")),
    );

    response
}
