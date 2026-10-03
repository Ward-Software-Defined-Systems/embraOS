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

    /// True for a document that is not there: the read's own variant, and a
    /// 404 from any other call. Every other failure — a timeout, a 500 —
    /// says nothing about the document.
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            WardsonDbError::DocumentNotFound { .. } | WardsonDbError::Api { status: 404, .. }
        )
    }
}

/// `WardsonDbError::is_not_found` through the `anyhow::Error` the client
/// returns.
pub fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<WardsonDbError>().is_some_and(WardsonDbError::is_not_found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_missing_document_reads_as_not_found() {
        let missing: anyhow::Error =
            WardsonDbError::DocumentNotFound { collection: "memory.semantic".into(), id: "n1".into() }.into();
        let gone: anyhow::Error = WardsonDbError::Api { status: 404, body: String::new() }.into();
        let broken: anyhow::Error = WardsonDbError::Api { status: 500, body: String::new() }.into();
        assert!(is_not_found(&missing));
        assert!(is_not_found(&gone));
        assert!(!is_not_found(&broken));
        assert!(!is_not_found(&anyhow::anyhow!("connection reset")));
    }
}
