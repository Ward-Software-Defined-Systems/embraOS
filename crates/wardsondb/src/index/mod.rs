pub mod primary;
pub mod secondary;

use std::collections::HashMap;
use std::ops::ControlFlow;

use parking_lot::RwLock;

use serde_json::Value;

use crate::engine::backend::{Engine, PartitionId, StorageBackend, WriteBatchWrapper};
use crate::error::AppError;
use crate::query::filter::resolve_json_path;

/// One backend scan over an index partition, yielding entries in candidate
/// order. The executor drives these for streaming index reads; the
/// `lookup_*` id collectors below ride the same shapes.
pub enum IndexScanShape {
    Prefix(Vec<u8>),
    Range { start: Vec<u8>, end: Vec<u8> },
}

impl IndexScanShape {
    pub fn scan(
        &self,
        engine: &Engine,
        partition: &PartitionId,
        visit: &mut crate::engine::backend::ScanVisitor<'_>,
    ) -> Result<(), AppError> {
        match self {
            IndexScanShape::Prefix(p) => engine.scan_prefix(partition, p, visit)?,
            IndexScanShape::Range { start, end } => {
                engine.scan_range(partition, start, end, visit)?
            }
        }
        Ok(())
    }
}

use self::secondary::{
    IndexDef, RangeScanBounds, extract_doc_id_from_key, make_compound_index_key_into,
    make_index_key_into, range_scan_bounds, value_to_sortable_bytes,
};

/// Cached index: definition + opaque partition handle.
struct IndexEntry {
    def: IndexDef,
    partition: PartitionId,
}

pub struct IndexManager {
    /// (collection, index_name) → IndexEntry
    indexes: RwLock<HashMap<(String, String), IndexEntry>>,
}

impl Default for IndexManager {
    fn default() -> Self {
        Self::new()
    }
}

impl IndexManager {
    pub fn new() -> Self {
        IndexManager {
            indexes: RwLock::new(HashMap::new()),
        }
    }

    /// Load all index definitions from _meta on startup.
    pub fn load_indexes(&self, engine: &Engine, meta: &PartitionId) -> Result<(), AppError> {
        // Parse the defs during the registry scan (borrowed values, zero
        // copies); open partitions and fill the map after it returns.
        let mut defs: Vec<IndexDef> = Vec::new();
        let mut item_err: Option<AppError> = None;
        engine.scan_prefix(meta, b"index:", &mut |key, value| {
            if let Err(e) = std::str::from_utf8(key) {
                item_err = Some(AppError::Internal(format!("Invalid index meta key: {e}")));
                return ControlFlow::Break(());
            }
            match serde_json::from_slice::<IndexDef>(value) {
                Ok(mut def) => {
                    // Backward compat: old indexes stored `field` but not `fields`
                    if def.fields.is_empty() && !def.field.is_empty() {
                        def.fields = vec![def.field.clone()];
                    }
                    defs.push(def);
                    ControlFlow::Continue(())
                }
                Err(e) => {
                    item_err = Some(AppError::Internal(format!("Invalid index meta value: {e}")));
                    ControlFlow::Break(())
                }
            }
        })?;
        if let Some(e) = item_err {
            return Err(e);
        }

        let mut indexes = self.indexes.write();
        for def in defs {
            let partition_name = format!("{}#idx#{}", def.collection, def.name);
            let partition = engine.create_or_open_partition(&partition_name)?;
            indexes.insert(
                (def.collection.clone(), def.name.clone()),
                IndexEntry { def, partition },
            );
        }

        Ok(())
    }

    /// Register an index (called after backfill + meta write).
    pub fn register(&self, def: IndexDef, partition: PartitionId) {
        let mut indexes = self.indexes.write();
        indexes.insert(
            (def.collection.clone(), def.name.clone()),
            IndexEntry { def, partition },
        );
    }

    /// Remove an index from the cache.
    pub fn unregister(&self, collection: &str, name: &str) {
        let mut indexes = self.indexes.write();
        indexes.remove(&(collection.to_string(), name.to_string()));
    }

    /// Get all index definitions for a collection.
    pub fn get_indexes_for_collection(&self, collection: &str) -> Vec<IndexDef> {
        let indexes = self.indexes.read();
        indexes
            .iter()
            .filter(|((col, _), _)| col == collection)
            .map(|(_, entry)| entry.def.clone())
            .collect()
    }

    /// Get the SINGLE-FIELD index for a field path, if one exists.
    ///
    /// Deliberately never falls back to a compound index whose leading
    /// field matches: compound indexes only contain documents that have ALL
    /// component fields (missing-field docs are skipped at write time), so
    /// serving a single-field lookup from one silently drops every document
    /// lacking the other components — and WHICH compound index won the old
    /// fallback was HashMap iteration order, i.e. arbitrary per process
    /// (F2, found by the pre-merge rig: `{"event_type":"system"}` counted 0
    /// through idx_type_action because system events carry no
    /// network.action, while a fresh process picking idx_type_time answered
    /// correctly). Single-field lookups on compound-only fields fall
    /// through to the bitmap accelerator or a full scan; the remedy for hot
    /// paths is creating the real single-field index — which is no longer
    /// misdetected as a duplicate of the compound one.
    pub fn get_index_for_field(
        &self,
        collection: &str,
        field: &str,
    ) -> Option<(IndexDef, PartitionId)> {
        let indexes = self.indexes.read();
        indexes
            .iter()
            .find(|((col, _), entry)| {
                col == collection && entry.def.fields.len() == 1 && entry.def.fields[0] == field
            })
            .map(|(_, entry)| (entry.def.clone(), entry.partition.clone()))
    }

    /// Find a compound index whose leading fields match `eq_fields`, optionally followed by `sort_field`.
    pub fn find_compound_index(
        &self,
        collection: &str,
        eq_field_names: &[&str],
        sort_fields: &[&str],
    ) -> Option<(IndexDef, PartitionId, usize)> {
        let indexes = self.indexes.read();
        let eq_set: std::collections::HashSet<&str> = eq_field_names.iter().copied().collect();

        let mut best: Option<(IndexDef, PartitionId, usize)> = None;

        for ((col, _), entry) in indexes.iter() {
            if col != collection || !entry.def.is_compound() {
                continue;
            }

            let idx_fields = &entry.def.fields;

            let mut matched = 0;
            for f in idx_fields {
                if eq_set.contains(f.as_str()) {
                    matched += 1;
                } else {
                    break;
                }
            }

            if matched == 0 {
                continue;
            }

            if !sort_fields.is_empty() {
                // The index fields right after the matched eq prefix must be
                // exactly the sort fields, in order (extra trailing index
                // fields are allowed — they only affect within-tie order).
                let need = matched + sort_fields.len();
                if need <= idx_fields.len()
                    && idx_fields[matched..need]
                        .iter()
                        .map(String::as_str)
                        .eq(sort_fields.iter().copied())
                    && best.as_ref().is_none_or(|(_, _, bm)| matched > *bm)
                {
                    best = Some((entry.def.clone(), entry.partition.clone(), matched));
                }
            } else if matched >= 2 && best.as_ref().is_none_or(|(_, _, bm)| matched > *bm) {
                best = Some((entry.def.clone(), entry.partition.clone(), matched));
            }
        }

        best
    }

    pub fn find_compound_range_index(
        &self,
        collection: &str,
        eq_field_names: &[&str],
        range_field: &str,
    ) -> Option<(IndexDef, PartitionId, usize)> {
        let indexes = self.indexes.read();
        let eq_set: std::collections::HashSet<&str> = eq_field_names.iter().copied().collect();

        let mut best: Option<(IndexDef, PartitionId, usize)> = None;

        for ((col, _), entry) in indexes.iter() {
            if col != collection || !entry.def.is_compound() {
                continue;
            }

            let idx_fields = &entry.def.fields;

            let mut matched = 0;
            for f in idx_fields {
                if eq_set.contains(f.as_str()) {
                    matched += 1;
                } else {
                    break;
                }
            }

            if matched == 0 {
                continue;
            }

            if matched < idx_fields.len()
                && idx_fields[matched] == range_field
                && best.as_ref().is_none_or(|(_, _, bm)| matched > *bm)
            {
                best = Some((entry.def.clone(), entry.partition.clone(), matched));
            }
        }

        best
    }

    /// Get the partition handle for a specific index by name.
    pub fn get_index_partition(&self, collection: &str, name: &str) -> Option<PartitionId> {
        let indexes = self.indexes.read();
        indexes
            .get(&(collection.to_string(), name.to_string()))
            .map(|entry| entry.partition.clone())
    }

    /// Stage index inserts for a newly written document into the given batch.
    pub fn add_index_entries_to_batch(
        &self,
        batch: &mut WriteBatchWrapper,
        collection: &str,
        doc_id: &str,
        doc: &Value,
    ) -> Result<(), AppError> {
        let indexes = self.indexes.read();
        // One scratch across every index of the doc (the batch copies what
        // it stages, so the buffer is free to be reused immediately).
        let mut key = Vec::new();
        for ((col, _), entry) in indexes.iter() {
            if col != collection {
                continue;
            }
            if entry.def.is_compound() {
                let values: Vec<&Value> = entry
                    .def
                    .fields
                    .iter()
                    .filter_map(|f| resolve_json_path(doc, f))
                    .collect();
                if values.len() == entry.def.fields.len() {
                    make_compound_index_key_into(&values, doc_id, &mut key);
                    batch.insert(&entry.partition, &key, b"")?;
                }
            } else if let Some(field_val) = resolve_json_path(doc, &entry.def.fields[0]) {
                make_index_key_into(field_val, doc_id, &mut key);
                batch.insert(&entry.partition, &key, b"")?;
            }
        }
        Ok(())
    }

    /// Stage index removes for a document being deleted/updated into the given batch.
    pub fn remove_index_entries_from_batch(
        &self,
        batch: &mut WriteBatchWrapper,
        collection: &str,
        doc_id: &str,
        doc: &Value,
    ) -> Result<(), AppError> {
        let indexes = self.indexes.read();
        let mut key = Vec::new();
        for ((col, _), entry) in indexes.iter() {
            if col != collection {
                continue;
            }
            if entry.def.is_compound() {
                let values: Vec<&Value> = entry
                    .def
                    .fields
                    .iter()
                    .filter_map(|f| resolve_json_path(doc, f))
                    .collect();
                if values.len() == entry.def.fields.len() {
                    make_compound_index_key_into(&values, doc_id, &mut key);
                    batch.remove(&entry.partition, &key)?;
                }
            } else if let Some(field_val) = resolve_json_path(doc, &entry.def.fields[0]) {
                make_index_key_into(field_val, doc_id, &mut key);
                batch.remove(&entry.partition, &key)?;
            }
        }
        Ok(())
    }

    /// Resolve the (partition, encoded prefix) for an equality scan on
    /// `field`'s index. None = no usable index.
    pub fn eq_scan(
        &self,
        collection: &str,
        field: &str,
        value: &Value,
    ) -> Option<(PartitionId, Vec<u8>)> {
        let (def, partition) = self.get_index_for_field(collection, field)?;
        let separator = if def.is_compound() { 0x01 } else { 0x00 };
        let mut prefix = value_to_sortable_bytes(value);
        prefix.push(separator);
        Some((partition, prefix))
    }

    /// Resolve the (partition, bounds) for a range scan on `field`'s index.
    /// `RangeScanBounds::Empty` means the predicate can never match
    /// (type-bracketed open ends, null/array/object operands).
    pub fn range_scan(
        &self,
        collection: &str,
        field: &str,
        lower: Option<(&Value, bool)>,
        upper: Option<(&Value, bool)>,
    ) -> Option<(PartitionId, RangeScanBounds)> {
        let (_def, partition) = self.get_index_for_field(collection, field)?;
        let lower_bytes = lower.map(|(v, inclusive)| (value_to_sortable_bytes(v), inclusive));
        let upper_bytes = upper.map(|(v, inclusive)| (value_to_sortable_bytes(v), inclusive));
        let bounds = range_scan_bounds(
            &[],
            lower_bytes.as_ref().map(|(b, i)| (b.as_slice(), *i)),
            upper_bytes.as_ref().map(|(b, i)| (b.as_slice(), *i)),
        );
        Some((partition, bounds))
    }

    /// Per-value prefixes for `$in`, deduped by encoded value (a doc holds
    /// exactly one entry per index, so only equal-encoding duplicates could
    /// double-report ids), in first-occurrence order.
    pub fn in_scan(
        &self,
        collection: &str,
        field: &str,
        values: &[Value],
    ) -> Option<(PartitionId, Vec<Vec<u8>>)> {
        let (def, partition) = self.get_index_for_field(collection, field)?;
        let separator = if def.is_compound() { 0x01 } else { 0x00 };
        let mut prefixes: Vec<Vec<u8>> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for value in values {
            let mut prefix = value_to_sortable_bytes(value);
            prefix.push(separator);
            if seen.insert(prefix.clone()) {
                prefixes.push(prefix);
            }
        }
        Some((partition, prefixes))
    }

    /// Equality lookup: all doc ids where field == value, in index order.
    /// `Ok(None)` = no usable index. Mid-scan engine errors propagate — the
    /// old buffering path silently truncated the id list at the first error.
    pub fn lookup_eq(
        &self,
        engine: &Engine,
        collection: &str,
        field: &str,
        value: &Value,
    ) -> Result<Option<Vec<String>>, AppError> {
        let Some((partition, prefix)) = self.eq_scan(collection, field, value) else {
            return Ok(None);
        };
        let mut doc_ids = Vec::new();
        engine.scan_prefix(&partition, &prefix, &mut |k, _| {
            if let Some(id) = extract_doc_id_from_key(k) {
                doc_ids.push(id);
            }
            ControlFlow::Continue(())
        })?;
        Ok(Some(doc_ids))
    }

    /// Range lookup: all doc ids in the (type-bracketed) range, index order.
    /// Same `Ok(None)` / error semantics as `lookup_eq`.
    pub fn lookup_range(
        &self,
        engine: &Engine,
        collection: &str,
        field: &str,
        lower: Option<(&Value, bool)>,
        upper: Option<(&Value, bool)>,
    ) -> Result<Option<Vec<String>>, AppError> {
        let Some((partition, bounds)) = self.range_scan(collection, field, lower, upper) else {
            return Ok(None);
        };
        let RangeScanBounds::Span { start, end } = bounds else {
            return Ok(Some(Vec::new()));
        };
        let mut doc_ids = Vec::new();
        engine.scan_range(&partition, &start, &end, &mut |k, _| {
            if let Some(id) = extract_doc_id_from_key(k) {
                doc_ids.push(id);
            }
            ControlFlow::Continue(())
        })?;
        Ok(Some(doc_ids))
    }

    /// Count index entries for an equality match (optimized count_only).
    pub fn count_eq(
        &self,
        engine: &Engine,
        collection: &str,
        field: &str,
        value: &Value,
    ) -> Option<u64> {
        let (def, partition) = self.get_index_for_field(collection, field)?;
        let separator = if def.is_compound() { 0x01 } else { 0x00 };
        let prefix = {
            let mut p = value_to_sortable_bytes(value);
            p.push(separator);
            p
        };

        // Keys-only count; `.ok()` keeps the fall-back-to-slow-path contract
        // on engine errors (unlike the lookup_* methods, which propagate).
        engine.count_prefix(&partition, &prefix).ok()
    }

    /// Count all index entries in a range — same bounds as `lookup_range`,
    /// but counted in the backend without materializing entries.
    pub fn count_range(
        &self,
        engine: &Engine,
        collection: &str,
        field: &str,
        lower: Option<(&Value, bool)>,
        upper: Option<(&Value, bool)>,
    ) -> Option<u64> {
        let (_def, partition) = self.get_index_for_field(collection, field)?;

        let lower_bytes = lower.map(|(v, inclusive)| (value_to_sortable_bytes(v), inclusive));
        let upper_bytes = upper.map(|(v, inclusive)| (value_to_sortable_bytes(v), inclusive));
        let (start, end) = match range_scan_bounds(
            &[],
            lower_bytes.as_ref().map(|(b, i)| (b.as_slice(), *i)),
            upper_bytes.as_ref().map(|(b, i)| (b.as_slice(), *i)),
        ) {
            RangeScanBounds::Empty => return Some(0),
            RangeScanBounds::Span { start, end } => (start, end),
        };

        engine.count_range(&partition, &start, &end).ok()
    }

    /// $in: union of equality scans over ONE resolved index handle, deduped
    /// by encoded value (see `in_scan`), first-occurrence order. Engine
    /// errors now fail the `$in` instead of silently skipping the value.
    pub fn lookup_in(
        &self,
        engine: &Engine,
        collection: &str,
        field: &str,
        values: &[Value],
    ) -> Result<Option<Vec<String>>, AppError> {
        let Some((partition, prefixes)) = self.in_scan(collection, field, values) else {
            return Ok(None);
        };
        let mut all_ids = Vec::new();
        for prefix in &prefixes {
            engine.scan_prefix(&partition, prefix, &mut |k, _| {
                if let Some(id) = extract_doc_id_from_key(k) {
                    all_ids.push(id);
                }
                ControlFlow::Continue(())
            })?;
        }
        Ok(Some(all_ids))
    }
}
