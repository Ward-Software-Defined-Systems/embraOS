use thiserror::Error;

#[derive(Error, Debug)]
pub enum WardsonDbError {
    #[error("WardSONDB HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("WardSONDB returned error {status}: {body}")]
    Api { status: u16, body: String },

    #[error("Document not found: {collection}/{id}")]
    DocumentNotFound { collection: String, id: String },
}

impl WardsonDbError {
    /// Returns true if this error represents an HTTP 409 DOCUMENT_CONFLICT.
    pub fn is_conflict(&self) -> bool {
        matches!(self, WardsonDbError::Api { status: 409, .. })
    }
}
