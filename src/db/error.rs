use thiserror::Error;

#[derive(Error, Debug)]
pub enum WardsonDbError {
    #[error("WardSONDB HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("WardSONDB not healthy after {0}s")]
    StartupTimeout(u64),

    #[error("WardSONDB process exited unexpectedly")]
    ProcessExited,

    #[error("WardSONDB returned error {status}: {body}")]
    Api { status: u16, body: String },

    #[error("Collection not found: {0}")]
    CollectionNotFound(String),

    #[error("Document not found: {collection}/{id}")]
    DocumentNotFound { collection: String, id: String },
}

impl WardsonDbError {
    /// Returns true if this error represents an HTTP 409 DOCUMENT_CONFLICT.
    pub fn is_conflict(&self) -> bool {
        matches!(self, WardsonDbError::Api { status: 409, .. })
    }

    /// Returns true if this error represents an HTTP 404 (document or collection not found).
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            WardsonDbError::Api { status: 404, .. }
                | WardsonDbError::DocumentNotFound { .. }
                | WardsonDbError::CollectionNotFound(_)
        )
    }
}
