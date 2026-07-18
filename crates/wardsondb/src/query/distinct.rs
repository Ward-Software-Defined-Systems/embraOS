use std::collections::HashSet;
use std::ops::ControlFlow;

use serde_json::Value;

use crate::engine::backend::StorageBackend;
use crate::engine::storage::Storage;
use crate::error::AppError;

use super::filter::{FilterNode, resolve_json_path};

pub struct DistinctResult {
    pub values: Vec<Value>,
    pub count: usize,
    pub truncated: bool,
    pub docs_scanned: u64,
    pub index_used: Option<String>,
}

pub fn execute_distinct(
    storage: &Storage,
    collection: &str,
    field: &str,
    filter: Option<&FilterNode>,
    limit: usize,
) -> Result<DistinctResult, AppError> {
    // Uniform 404 contract regardless of which path serves the distinct (F3).
    storage.ensure_collection_exists(collection)?;

    // If no filter, try index-only scan on the distinct field
    if filter.is_none()
        && let Some(result) = try_index_only_distinct(storage, collection, field, limit)?
    {
        return Ok(result);
    }

    // Fall back to a streamed full scan; stops as soon as `limit` distinct
    // values are in hand, so docs_scanned reports the documents actually
    // visited (previously always the whole collection).
    let mut unique_keys: HashSet<String> = HashSet::new();
    let mut values: Vec<Value> = Vec::new();
    let mut truncated = false;
    let mut docs_scanned = 0u64;

    storage.for_each_document(collection, &mut |doc| {
        docs_scanned += 1;
        if let Some(f) = filter
            && !f.matches(&doc)
        {
            return ControlFlow::Continue(());
        }
        if let Some(val) = resolve_json_path(&doc, field) {
            let key = serde_json::to_string(val).unwrap_or_default();
            if unique_keys.insert(key) {
                values.push(val.clone());
                if values.len() >= limit {
                    truncated = true;
                    return ControlFlow::Break(());
                }
            }
        }
        ControlFlow::Continue(())
    })?;

    let count = values.len();
    Ok(DistinctResult {
        values,
        count,
        truncated,
        docs_scanned,
        index_used: None,
    })
}

/// Try to get distinct values directly from an index without loading documents.
fn try_index_only_distinct(
    storage: &Storage,
    collection: &str,
    field: &str,
    limit: usize,
) -> Result<Option<DistinctResult>, AppError> {
    let (def, partition) = match storage.index_manager.get_index_for_field(collection, field) {
        Some(pair) => pair,
        None => return Ok(None),
    };

    // Only works for single-field indexes (not compound)
    if def.is_compound() {
        return Ok(None);
    }

    let mut unique_keys: HashSet<Vec<u8>> = HashSet::new();
    let mut values: Vec<Value> = Vec::new();
    let mut truncated = false;

    // Streamed: repeated values cost nothing (borrowed-key contains check —
    // only first-seen values are copied), and the scan breaks at `limit`
    // instead of buffering the whole index.
    storage.engine.scan_full(&partition, &mut |key, _| {
        // Key format: {value_bytes}\x00{doc_id}
        if let Some(sep_pos) = key.iter().rposition(|&b| b == 0x00) {
            let value_part = &key[..sep_pos];
            if !unique_keys.contains(value_part) {
                unique_keys.insert(value_part.to_vec());
                // Decode the value from sortable bytes
                if let Some(val) = crate::index::secondary::decode_sortable_bytes(value_part) {
                    values.push(val);
                    if values.len() >= limit {
                        truncated = true;
                        return ControlFlow::Break(());
                    }
                }
            }
        }
        ControlFlow::Continue(())
    })?;

    let count = values.len();
    Ok(Some(DistinctResult {
        values,
        count,
        truncated,
        docs_scanned: 0,
        index_used: Some(def.name.clone()),
    }))
}
