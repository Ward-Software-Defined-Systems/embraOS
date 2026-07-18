use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Request};
use serde::de::DeserializeOwned;

use crate::error::AppError;

/// Custom JSON extractor that returns our AppError envelope on parse failures
/// instead of Axum's default plaintext error.
pub struct JsonBody<T>(pub T);

impl<S, T> FromRequest<S> for JsonBody<T>
where
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(JsonBody(value)),
            // Body-limit overruns (DefaultBodyLimit / --max-body-mb) surface
            // as a buffering failure mentioning the length limit — report
            // them as 413 rather than a generic 400 parse error.
            Err(rejection) if rejection.body_text().contains("length limit") => {
                Err(AppError::DocumentTooLarge)
            }
            Err(rejection) => Err(AppError::InvalidDocument(rejection.body_text())),
        }
    }
}
