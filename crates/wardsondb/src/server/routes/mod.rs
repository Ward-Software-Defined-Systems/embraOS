pub mod collections;
pub mod documents;
pub mod indexes;
pub mod query;
pub mod system;

use crate::error::AppError;

/// A collection name, a document id or an index name taken from the URL
/// path is checked before any engine call: a client that failed to encode
/// an id answers 400 here, never a document or a partition it did not
/// mean (`engine::document::segment_fault` holds the rule; #25).
pub(crate) fn valid_segment(what: &str, value: &str) -> Result<(), AppError> {
    match crate::engine::document::segment_fault(value) {
        Some(why) => Err(AppError::InvalidDocument(format!("{what} {why}"))),
        None => Ok(()),
    }
}
