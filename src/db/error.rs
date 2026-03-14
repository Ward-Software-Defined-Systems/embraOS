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
