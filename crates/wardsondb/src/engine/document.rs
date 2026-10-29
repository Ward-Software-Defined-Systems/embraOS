use std::collections::HashSet;
use std::ops::ControlFlow;

use chrono::Utc;
use serde_json::Value;
use uuid::Uuid;

use crate::engine::backend::{PartitionId, StorageBackend};
use crate::error::AppError;
use crate::query::filter::FilterNode;

use super::storage::Storage;

const MAX_DOCUMENT_SIZE: usize = 16 * 1024 * 1024; // 16 MB
const MAX_BULK_INSERT: usize = 10_000;
const MAX_CUSTOM_ID_LENGTH: usize = 512;
/// Documents a bulk mutation commits per batch, under the shards of that
/// chunk alone, so a whole-collection `delete_by_query` never holds every
/// writer off for the length of one commit.
const BULK_MUTATION_CHUNK: usize = 256;

/// Validate a custom _id value. Returns the validated string or an error.
fn validate_custom_id(value: &Value) -> Result<String, AppError> {
    let s = match value {
        Value::String(s) if !s.is_empty() => s,
        _ => {
            return Err(AppError::InvalidDocument(
                "_id must be a non-empty string".into(),
            ));
        }
    };
    if s.len() > MAX_CUSTOM_ID_LENGTH {
        return Err(AppError::InvalidDocument(
            "_id exceeds maximum length of 512 bytes".into(),
        ));
    }
    if s.starts_with('_') {
        return Err(AppError::InvalidDocument(
            "_id must not start with underscore".into(),
        ));
    }
    if s.contains('\x00') {
        return Err(AppError::InvalidDocument(
            "_id contains invalid characters".into(),
        ));
    }
    Ok(s.clone())
}

impl Storage {
    pub fn insert_document(&self, collection: &str, mut doc: Value) -> Result<Value, AppError> {
        self.check_not_poisoned()?;
        self.ensure_collection_exists(collection)?;

        let obj = doc
            .as_object_mut()
            .ok_or_else(|| AppError::InvalidDocument("Document must be a JSON object".into()))?;

        // Resolved once — the custom-id duplicate check and the write below
        // share it instead of re-deriving the handle per use.
        let docs_partition = self.get_docs_partition(collection)?;

        // Determine ID: use custom _id if provided, otherwise generate UUIDv7.
        // A custom id holds its shard from the duplicate check to the commit:
        // two inserts of one id both passed the check otherwise, and the
        // second overwrote the first with a 201 of its own. A generated
        // UUIDv7 collides with nothing, so it takes no shard.
        let (id, _shard) = if let Some(raw_id) = obj.remove("_id") {
            let custom_id = validate_custom_id(&raw_id)?;
            let shard = self.lock_document(collection, &custom_id);
            if self
                .engine
                .get(&docs_partition, custom_id.as_bytes())?
                .is_some()
            {
                return Err(AppError::DocumentConflict(format!(
                    "Document already exists: {custom_id}"
                )));
            }
            (custom_id, Some(shard))
        } else {
            (Uuid::now_v7().to_string(), None)
        };

        let now = Utc::now().to_rfc3339();
        obj.insert("_id".to_string(), Value::String(id.clone()));
        obj.insert("_rev".to_string(), Value::Number(1.into()));
        obj.insert("_created_at".to_string(), Value::String(now.clone()));
        obj.insert("_updated_at".to_string(), Value::String(now.clone()));
        obj.insert("_received_at".to_string(), Value::String(now));

        let bytes = serde_json::to_vec(&doc)?;
        if bytes.len() > MAX_DOCUMENT_SIZE {
            return Err(AppError::DocumentTooLarge);
        }

        let mut batch = self.write_batch();
        batch.insert(&docs_partition, id.as_bytes(), &bytes)?;
        self.index_manager
            .add_index_entries_to_batch(&mut batch, collection, &id, &doc)?;
        self.commit_batch(batch)?;

        self.doc_counts.increment(collection, 1);
        self.scan_accelerator.on_insert(collection, &id, &doc);

        Ok(doc)
    }

    pub fn get_document(&self, collection: &str, id: &str) -> Result<Value, AppError> {
        self.ensure_collection_exists(collection)?;

        let docs_partition = self.get_docs_partition(collection)?;
        match self.engine.get(&docs_partition, id.as_bytes())? {
            Some(bytes) => {
                let doc: Value = serde_json::from_slice(&bytes)?;
                Ok(doc)
            }
            None => Err(AppError::DocumentNotFound(id.to_string())),
        }
    }

    pub fn replace_document(
        &self,
        collection: &str,
        id: &str,
        doc: Value,
    ) -> Result<Value, AppError> {
        self.check_not_poisoned()?;
        self.ensure_collection_exists(collection)?;

        let docs_partition = self.get_docs_partition(collection)?;
        let _shard = self.lock_document(collection, id);
        let existing_doc = self.read_existing(&docs_partition, id)?;
        self.replace_loaded(collection, id, &docs_partition, existing_doc, doc)
    }

    /// The stored document, for a write that is about to replace it.
    fn read_existing(&self, docs_partition: &PartitionId, id: &str) -> Result<Value, AppError> {
        let existing_bytes = self
            .engine
            .get(docs_partition, id.as_bytes())?
            .ok_or_else(|| AppError::DocumentNotFound(id.to_string()))?;
        Ok(serde_json::from_slice(&existing_bytes)?)
    }

    /// Replace `existing_doc` with `doc`: the revision check, the stamps,
    /// one batch that swaps the index entries with the document, the
    /// accelerator hook. The caller holds the document's shard and read
    /// `existing_doc` under it, which is what makes the revision it carries
    /// the one the commit supersedes.
    fn replace_loaded(
        &self,
        collection: &str,
        id: &str,
        docs_partition: &PartitionId,
        existing_doc: Value,
        mut doc: Value,
    ) -> Result<Value, AppError> {
        let old_rev = existing_doc
            .get("_rev")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        // Check for revision conflict if client sends _rev
        if let Some(client_rev) = doc.get("_rev").and_then(|v| v.as_u64())
            && client_rev != old_rev
        {
            return Err(AppError::DocumentConflict(format!(
                "Expected rev {client_rev}, current is {old_rev}"
            )));
        }

        let now = Utc::now().to_rfc3339();
        let created_at = existing_doc
            .get("_created_at")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let received_at = existing_doc
            .get("_received_at")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let obj = doc
            .as_object_mut()
            .ok_or_else(|| AppError::InvalidDocument("Document must be a JSON object".into()))?;
        obj.insert("_id".to_string(), Value::String(id.to_string()));
        obj.insert("_rev".to_string(), Value::Number((old_rev + 1).into()));
        obj.insert("_created_at".to_string(), Value::String(created_at));
        obj.insert("_updated_at".to_string(), Value::String(now));
        obj.insert("_received_at".to_string(), Value::String(received_at));

        let bytes = serde_json::to_vec(&doc)?;
        if bytes.len() > MAX_DOCUMENT_SIZE {
            return Err(AppError::DocumentTooLarge);
        }

        let mut batch = self.write_batch();
        self.index_manager.remove_index_entries_from_batch(
            &mut batch,
            collection,
            id,
            &existing_doc,
        )?;
        batch.insert(docs_partition, id.as_bytes(), &bytes)?;
        self.index_manager
            .add_index_entries_to_batch(&mut batch, collection, id, &doc)?;
        self.commit_batch(batch)?;

        self.scan_accelerator
            .on_update(collection, id, &existing_doc, &doc);

        Ok(doc)
    }

    /// Merge `patch` into the stored document and replace it, one read under
    /// the document's shard: the merge and the commit see one revision, so
    /// the second read `replace_document` used to make is gone with the
    /// window it guarded.
    pub fn partial_update_document(
        &self,
        collection: &str,
        id: &str,
        patch: Value,
    ) -> Result<Value, AppError> {
        self.check_not_poisoned()?;
        self.ensure_collection_exists(collection)?;

        let docs_partition = self.get_docs_partition(collection)?;
        let _shard = self.lock_document(collection, id);
        let existing_doc = self.read_existing(&docs_partition, id)?;
        let mut merged = existing_doc.clone();
        json_merge_patch(&mut merged, &patch);
        self.replace_loaded(collection, id, &docs_partition, existing_doc, merged)
    }

    pub fn delete_document(&self, collection: &str, id: &str) -> Result<(), AppError> {
        self.check_not_poisoned()?;
        self.ensure_collection_exists(collection)?;

        let docs_partition = self.get_docs_partition(collection)?;
        let _shard = self.lock_document(collection, id);
        let existing_doc = self.read_existing(&docs_partition, id)?;

        let mut batch = self.write_batch();
        self.index_manager.remove_index_entries_from_batch(
            &mut batch,
            collection,
            id,
            &existing_doc,
        )?;
        batch.remove(&docs_partition, id.as_bytes())?;
        self.commit_batch(batch)?;

        self.doc_counts.increment(collection, -1);
        self.scan_accelerator
            .on_delete(collection, id, &existing_doc);

        Ok(())
    }

    pub fn bulk_insert_documents(
        &self,
        collection: &str,
        documents: Vec<Value>,
    ) -> Result<(u64, Vec<String>), AppError> {
        if documents.len() > MAX_BULK_INSERT {
            return Err(AppError::InvalidDocument(format!(
                "Bulk insert limited to {MAX_BULK_INSERT} documents per request"
            )));
        }
        self.check_not_poisoned()?;
        self.ensure_collection_exists(collection)?;

        let docs_partition = self.get_docs_partition(collection)?;
        // (document index, message): reported in document order whichever
        // pass found the fault.
        let mut errors: Vec<(usize, String)> = Vec::new();
        // (index, id, bytes, doc, custom) — the doc stays for the index
        // writes, `custom` marks an id the collection must be checked for.
        let mut to_write: Vec<(usize, String, Vec<u8>, Value, bool)> = Vec::new();
        let mut seen_ids: HashSet<String> = HashSet::new();

        // Pass one, no engine reads: ids, stamps and sizes.
        for (i, mut doc) in documents.into_iter().enumerate() {
            let obj = match doc.as_object_mut() {
                Some(obj) => obj,
                None => {
                    errors.push((i, format!("Document {i}: must be a JSON object")));
                    continue;
                }
            };

            // Determine ID: use custom _id if provided, otherwise generate UUIDv7
            let (id, custom) = if let Some(raw_id) = obj.remove("_id") {
                match validate_custom_id(&raw_id) {
                    Ok(custom_id) => {
                        // Check for intra-batch duplicate
                        if seen_ids.contains(&custom_id) {
                            errors.push((
                                i,
                                format!("Document {i}: duplicate _id in batch: {custom_id}"),
                            ));
                            continue;
                        }
                        (custom_id, true)
                    }
                    Err(e) => {
                        errors.push((i, format!("Document {i}: {e}")));
                        continue;
                    }
                }
            } else {
                (Uuid::now_v7().to_string(), false)
            };

            let now = Utc::now().to_rfc3339();
            obj.insert("_id".to_string(), Value::String(id.clone()));
            obj.insert("_rev".to_string(), Value::Number(1.into()));
            obj.insert("_created_at".to_string(), Value::String(now.clone()));
            obj.insert("_updated_at".to_string(), Value::String(now.clone()));
            obj.insert("_received_at".to_string(), Value::String(now));

            match serde_json::to_vec(&doc) {
                Ok(bytes) => {
                    if bytes.len() > MAX_DOCUMENT_SIZE {
                        errors.push((i, format!("Document {i}: exceeds 16 MB size limit")));
                    } else {
                        seen_ids.insert(id.clone());
                        to_write.push((i, id, bytes, doc, custom));
                    }
                }
                Err(e) => errors.push((i, format!("Document {i}: {e}"))),
            }
        }

        // Pass two, under the shards of the custom ids (taken in index order,
        // each once): the existence checks and the one commit, so an insert
        // of one of these ids on another thread sees the batch or is seen by
        // it, never both passing the check.
        let mut inserted = 0u64;
        if !to_write.is_empty() {
            let _shards = self.lock_documents(
                collection,
                to_write
                    .iter()
                    .filter(|(_, _, _, _, custom)| *custom)
                    .map(|(_, id, _, _, _)| id.as_str()),
            );
            let mut existing = Vec::new();
            for (i, id, _, _, custom) in &to_write {
                if !custom {
                    continue;
                }
                match self.engine.get(&docs_partition, id.as_bytes()) {
                    Ok(Some(_)) => {
                        errors.push((*i, format!("Document {i}: document already exists: {id}")));
                        existing.push(*i);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        errors.push((*i, format!("Document {i}: {e}")));
                        existing.push(*i);
                    }
                }
            }
            to_write.retain(|(i, ..)| !existing.contains(i));

            if !to_write.is_empty() {
                let mut batch = self.write_batch();
                for (_, id, bytes, doc, _) in &to_write {
                    batch.insert(&docs_partition, id.as_bytes(), bytes.as_slice())?;
                    self.index_manager
                        .add_index_entries_to_batch(&mut batch, collection, id, doc)?;
                }
                self.commit_batch(batch)?;
                inserted = to_write.len() as u64;
                self.doc_counts.increment(collection, inserted as i64);

                for (_, id, _bytes, doc, _) in &to_write {
                    self.scan_accelerator.on_insert(collection, id, doc);
                }
            }
        }

        errors.sort_by_key(|(i, _)| *i);
        Ok((inserted, errors.into_iter().map(|(_, m)| m).collect()))
    }

    /// Stream every document in the collection through `visit`, parsing
    /// during the scan (nothing else stays resident). A document that fails
    /// to parse fails the whole scan — the mutation-path policy (S2-8 owns
    /// any skip-vs-fail change).
    pub(crate) fn for_each_document(
        &self,
        collection: &str,
        visit: &mut dyn FnMut(Value) -> ControlFlow<()>,
    ) -> Result<(), AppError> {
        self.ensure_collection_exists(collection)?;
        let docs_partition = self.get_docs_partition(collection)?;
        let mut parse_err: Option<AppError> = None;
        self.engine.scan_full(
            &docs_partition,
            &mut |_, v| match serde_json::from_slice::<Value>(v) {
                Ok(doc) => visit(doc),
                Err(e) => {
                    parse_err = Some(e.into());
                    ControlFlow::Break(())
                }
            },
        )?;
        if let Some(e) = parse_err {
            return Err(e);
        }
        Ok(())
    }

    pub fn scan_all_documents(&self, collection: &str) -> Result<Vec<Value>, AppError> {
        let mut docs = Vec::new();
        self.for_each_document(collection, &mut |doc| {
            docs.push(doc);
            ControlFlow::Continue(())
        })?;
        Ok(docs)
    }

    /// Delete all documents matching a filter. Returns the count of deleted
    /// documents.
    ///
    /// The scan keeps only the ids of the matches. The deletes run in
    /// chunks of `BULK_MUTATION_CHUNK`, each under the shards of its ids:
    /// every document is read again there and matched again, so a document
    /// another writer changed or removed since the scan is left alone, and
    /// its index entries are removed from the revision that is stored, never
    /// from the scan's stale copy. One batch commits each chunk.
    pub fn delete_by_query(&self, collection: &str, filter: &FilterNode) -> Result<u64, AppError> {
        self.check_not_poisoned()?;

        let ids = self.matching_ids(collection, filter)?;
        if ids.is_empty() {
            return Ok(0);
        }

        let docs_partition = self.get_docs_partition(collection)?;
        let mut deleted = 0u64;
        for chunk in ids.chunks(BULK_MUTATION_CHUNK) {
            let _shards = self.lock_documents(collection, chunk.iter().map(String::as_str));
            let mut batch = self.write_batch();
            let mut removed: Vec<(&str, Value)> = Vec::new();
            for id in chunk {
                let Some(bytes) = self.engine.get(&docs_partition, id.as_bytes())? else {
                    continue;
                };
                let doc: Value = serde_json::from_slice(&bytes)?;
                if !filter.matches(&doc) {
                    continue;
                }
                self.index_manager
                    .remove_index_entries_from_batch(&mut batch, collection, id, &doc)?;
                batch.remove(&docs_partition, id.as_bytes())?;
                removed.push((id, doc));
            }
            if removed.is_empty() {
                continue;
            }
            self.commit_batch(batch)?;
            self.doc_counts
                .increment(collection, -(removed.len() as i64));
            for (id, doc) in &removed {
                self.scan_accelerator.on_delete(collection, id, doc);
            }
            deleted += removed.len() as u64;
        }

        Ok(deleted)
    }

    /// The ids of the documents `filter` matches, from one streamed scan
    /// that keeps nothing else resident.
    fn matching_ids(&self, collection: &str, filter: &FilterNode) -> Result<Vec<String>, AppError> {
        let mut ids: Vec<String> = Vec::new();
        self.for_each_document(collection, &mut |doc| {
            if filter.matches(&doc)
                && let Some(id) = doc.get("_id").and_then(|v| v.as_str())
            {
                ids.push(id.to_string());
            }
            ControlFlow::Continue(())
        })?;
        Ok(ids)
    }

    /// Update all documents matching a filter with $set operations.
    /// Returns the count of updated documents.
    ///
    /// Chunked under the shards like `delete_by_query`: each document is
    /// read and matched again under its shard, the `$set` applies to the
    /// stored revision, and one batch commits the chunk.
    pub fn update_by_query(
        &self,
        collection: &str,
        filter: &FilterNode,
        update: &Value,
    ) -> Result<u64, AppError> {
        self.check_not_poisoned()?;

        let set_fields = parse_set_updates(update)?;

        let ids = self.matching_ids(collection, filter)?;
        if ids.is_empty() {
            return Ok(0);
        }

        let docs_partition = self.get_docs_partition(collection)?;
        let now = Utc::now().to_rfc3339();
        let mut updated = 0u64;

        for chunk in ids.chunks(BULK_MUTATION_CHUNK) {
            let _shards = self.lock_documents(collection, chunk.iter().map(String::as_str));
            let mut batch = self.write_batch();
            // (id, old_doc, new_doc) for the accelerator hooks after the commit
            let mut update_pairs: Vec<(&str, Value, Value)> = Vec::new();

            for id in chunk {
                let Some(bytes) = self.engine.get(&docs_partition, id.as_bytes())? else {
                    continue;
                };
                let old_doc: Value = serde_json::from_slice(&bytes)?;
                if !filter.matches(&old_doc) {
                    continue;
                }
                let mut new_doc = old_doc.clone();
                let old_rev = new_doc.get("_rev").and_then(|v| v.as_u64()).unwrap_or(0);

                self.index_manager
                    .remove_index_entries_from_batch(&mut batch, collection, id, &old_doc)?;

                if let Some(obj) = new_doc.as_object_mut() {
                    for (path, value) in &set_fields {
                        set_nested_field(obj, path, value.clone());
                    }
                    obj.insert("_rev".to_string(), Value::Number((old_rev + 1).into()));
                    obj.insert("_updated_at".to_string(), Value::String(now.clone()));
                }

                let bytes = serde_json::to_vec(&new_doc)?;
                batch.insert(&docs_partition, id.as_bytes(), bytes.as_slice())?;

                self.index_manager
                    .add_index_entries_to_batch(&mut batch, collection, id, &new_doc)?;

                update_pairs.push((id, old_doc, new_doc));
            }
            if update_pairs.is_empty() {
                continue;
            }

            self.commit_batch(batch)?;

            for (id, old_doc, new_doc) in &update_pairs {
                self.scan_accelerator
                    .on_update(collection, id, old_doc, new_doc);
            }
            updated += update_pairs.len() as u64;
        }

        Ok(updated)
    }

    /// Create a secondary index on one or more fields, backfilling existing documents.
    pub fn create_index(
        &self,
        collection: &str,
        name: &str,
        fields: &[String],
    ) -> Result<crate::index::secondary::IndexDef, AppError> {
        self.check_not_poisoned()?;
        self.ensure_collection_exists(collection)?;

        if fields.is_empty() {
            return Err(AppError::InvalidIndex(
                "Index must have at least one field".into(),
            ));
        }

        // Check for duplicate name
        let meta_key = format!("index:{collection}:{name}");
        if self.engine.get(&self.meta, meta_key.as_bytes())?.is_some() {
            return Err(AppError::IndexExists(name.to_string()));
        }

        // For single-field indexes, check if an index on this field already exists
        if fields.len() == 1
            && self
                .index_manager
                .get_index_for_field(collection, &fields[0])
                .is_some()
        {
            return Err(AppError::IndexExists(format!(
                "An index on field '{}' already exists",
                fields[0]
            )));
        }

        let def = crate::index::secondary::IndexDef::new(
            name.to_string(),
            collection.to_string(),
            fields.to_vec(),
            Utc::now().to_rfc3339(),
        );

        // Create the index partition
        let partition_name = format!("{collection}#idx#{name}");
        let partition = self.create_partition(&partition_name)?;

        // Backfill: stream all existing documents and stage their entries
        // (a parse failure still fails the backfill).
        let mut entries: Vec<Vec<u8>> = Vec::new();
        let is_compound = fields.len() > 1;

        self.for_each_document(collection, &mut |doc| {
            if let Some(doc_id) = doc.get("_id").and_then(|v| v.as_str()) {
                if is_compound {
                    let values: Vec<&Value> = fields
                        .iter()
                        .filter_map(|f| crate::query::filter::resolve_json_path(&doc, f))
                        .collect();
                    if values.len() == fields.len() {
                        let key = crate::index::secondary::make_compound_index_key(&values, doc_id);
                        entries.push(key);
                    }
                } else if let Some(field_val) =
                    crate::query::filter::resolve_json_path(&doc, &fields[0])
                {
                    let key = crate::index::secondary::make_index_key(field_val, doc_id);
                    entries.push(key);
                }
            }
            ControlFlow::Continue(())
        })?;

        let meta_bytes = serde_json::to_vec(&def)?;
        let mut batch = self.write_batch();
        for key in &entries {
            batch.insert(&partition, key.as_slice(), b"")?;
        }
        batch.insert(&self.meta, meta_key.as_bytes(), &meta_bytes)?;
        self.commit_batch(batch)?;

        // Register in cache
        self.index_manager.register(def.clone(), partition);

        self.persist()?;

        Ok(def)
    }

    /// Drop a secondary index.
    pub fn drop_index(&self, collection: &str, name: &str) -> Result<(), AppError> {
        self.check_not_poisoned()?;
        self.ensure_collection_exists(collection)?;

        let meta_key = format!("index:{collection}:{name}");
        if self.engine.get(&self.meta, meta_key.as_bytes())?.is_none() {
            return Err(AppError::IndexNotFound(format!("{collection}/{name}")));
        }

        let partition = self
            .index_manager
            .get_index_partition(collection, name)
            .ok_or_else(|| AppError::IndexNotFound(format!("{collection}/{name}")))?;

        // Keys only, values never copied; a mid-scan engine error aborts the
        // drop instead of committing a truncated removal.
        let keys = crate::engine::backend::collect_keys(&self.engine, &partition)?;

        let mut batch = self.write_batch();
        for key in &keys {
            batch.remove(&partition, key.as_slice())?;
        }
        batch.remove(&self.meta, meta_key.as_bytes())?;
        self.commit_batch(batch)?;

        self.index_manager.unregister(collection, name);
        self.persist()?;

        Ok(())
    }

    /// List all indexes for a collection.
    pub fn list_indexes(
        &self,
        collection: &str,
    ) -> Result<Vec<crate::index::secondary::IndexDef>, AppError> {
        self.ensure_collection_exists(collection)?;
        Ok(self.index_manager.get_indexes_for_collection(collection))
    }

    pub fn ensure_collection_exists(&self, collection: &str) -> Result<(), AppError> {
        if !self.collection_exists(collection)? {
            return Err(AppError::CollectionNotFound(collection.to_string()));
        }
        Ok(())
    }
}

/// Parse $set updates from the update spec: {"$set": {"field": value, ...}}
fn parse_set_updates(update: &Value) -> Result<Vec<(String, Value)>, AppError> {
    let obj = update
        .as_object()
        .ok_or_else(|| AppError::InvalidQuery("Update must be a JSON object".into()))?;

    let set_obj = obj
        .get("$set")
        .and_then(|v| v.as_object())
        .ok_or_else(|| AppError::InvalidQuery("Update must contain a '$set' object".into()))?;

    Ok(set_obj
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect())
}

/// Set a nested field using dot-notation path, creating intermediate objects as needed.
fn set_nested_field(obj: &mut serde_json::Map<String, Value>, path: &str, value: Value) {
    // Depth limit: silently skip if too deep (validation happens at parse layer)
    if path.matches('.').count() >= 20 {
        return;
    }
    let parts: Vec<&str> = path.split('.').collect();
    if parts.len() == 1 {
        obj.insert(path.to_string(), value);
        return;
    }

    let mut current = obj;
    for part in &parts[..parts.len() - 1] {
        let entry = current
            .entry(part.to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Value::Object(inner) = entry {
            current = inner;
        } else {
            // Overwrite non-object with an object
            *entry = Value::Object(serde_json::Map::new());
            if let Value::Object(inner) = entry {
                current = inner;
            } else {
                return;
            }
        }
    }

    let last = parts.last().unwrap();
    current.insert(last.to_string(), value);
}

fn json_merge_patch(target: &mut Value, patch: &Value) {
    if let Value::Object(patch_obj) = patch
        && let Value::Object(target_obj) = target
    {
        for (key, value) in patch_obj {
            if value.is_null() {
                target_obj.remove(key);
            } else {
                let entry = target_obj.entry(key.clone()).or_insert(Value::Null);
                json_merge_patch(entry, value);
            }
        }
        return;
    }
    *target = patch.clone();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::storage::MemoryConfig;
    use crate::query::executor::execute_query;
    use crate::query::filter::parse_filter;
    use crate::query::parser::ParsedQuery;
    use serde_json::json;

    fn open() -> (tempfile::TempDir, Storage) {
        let tmp = tempfile::TempDir::new().unwrap();
        let storage =
            Storage::open_with_config(tmp.path(), "fjall", MemoryConfig::default()).unwrap();
        storage.create_collection("c").unwrap();
        (tmp, storage)
    }

    fn patch(field: &str, value: u64) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert(field.to_string(), json!(value));
        Value::Object(obj)
    }

    fn query(storage: &Storage, filter: Value) -> Vec<Value> {
        let parsed = ParsedQuery {
            filter: Some(parse_filter(&filter).unwrap()),
            sort: vec![],
            limit: 10,
            offset: 0,
            fields: None,
            count_only: false,
            cursor: None,
        };
        execute_query(storage, "c", &parsed).unwrap().docs
    }

    /// #64-1: two writers of one document, each patching its own field.
    /// Without the shard both read one revision and the second commit
    /// drops the first's field, and the index keeps the key of a value the
    /// document no longer holds.
    #[test]
    fn concurrent_patches_of_one_document_lose_no_update_and_leave_no_stale_index_key() {
        let (_tmp, storage) = open();
        storage
            .create_index("c", "idx_a", &["a".to_string()])
            .unwrap();
        storage
            .insert_document("c", json!({"_id": "x", "a": 0, "b": 0}))
            .unwrap();
        const ROUNDS: u64 = 500;
        std::thread::scope(|s| {
            for field in ["a", "b"] {
                let storage = &storage;
                s.spawn(move || {
                    for i in 1..=ROUNDS {
                        storage
                            .partial_update_document("c", "x", patch(field, i))
                            .unwrap();
                    }
                });
            }
        });

        let doc = storage.get_document("c", "x").unwrap();
        assert_eq!(doc["a"], ROUNDS, "every patch of a landed");
        assert_eq!(doc["b"], ROUNDS, "every patch of b landed");
        assert_eq!(doc["_rev"], 1 + 2 * ROUNDS, "every commit moved the revision");

        let (_, partition) = storage
            .index_manager
            .get_index_for_field("c", "a")
            .unwrap();
        let keys = crate::engine::backend::collect_keys(&storage.engine, &partition).unwrap();
        assert_eq!(keys.len(), 1, "the index holds the current value and no other");
        assert_eq!(
            crate::index::secondary::extract_doc_id_from_key(&keys[0]).as_deref(),
            Some("x")
        );
        assert!(
            query(&storage, json!({"a": ROUNDS - 1})).is_empty(),
            "a superseded value finds nothing through the index"
        );
        assert_eq!(query(&storage, json!({"a": ROUNDS})).len(), 1);
    }

    /// #64-2: the custom-id duplicate check and the commit are one critical
    /// section, so of two inserts of one id exactly one is a document.
    #[test]
    fn concurrent_inserts_of_one_custom_id_give_one_conflict_and_count_one() {
        let (_tmp, storage) = open();
        let outcomes: Vec<Result<Value, AppError>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..2)
                .map(|n| {
                    let storage = &storage;
                    s.spawn(move || storage.insert_document("c", json!({"_id": "dup", "n": n})))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let ok = outcomes.iter().filter(|r| r.is_ok()).count();
        let conflicts = outcomes
            .iter()
            .filter(|r| matches!(r, Err(AppError::DocumentConflict(_))))
            .count();
        assert_eq!((ok, conflicts), (1, 1), "{outcomes:?}");
        assert_eq!(storage.doc_counts.get("c"), 1);
        let docs = storage.get_docs_partition("c").unwrap();
        assert_eq!(storage.engine.count_prefix(&docs, b"").unwrap(), 1);
    }

    /// A patch that names a revision other than the stored one is a
    /// conflict and changes nothing.
    #[test]
    fn a_patch_with_a_stale_rev_is_a_conflict_and_writes_nothing() {
        let (_tmp, storage) = open();
        storage
            .insert_document("c", json!({"_id": "x", "a": 1}))
            .unwrap();
        storage
            .partial_update_document("c", "x", json!({"a": 2}))
            .unwrap();
        let err = storage
            .partial_update_document("c", "x", json!({"_rev": 1, "a": 9}))
            .unwrap_err();
        assert!(matches!(err, AppError::DocumentConflict(_)), "{err:?}");
        let doc = storage.get_document("c", "x").unwrap();
        assert_eq!(doc["a"], 2);
        assert_eq!(doc["_rev"], 2);
        let doc = storage
            .partial_update_document("c", "x", json!({"_rev": 2, "a": 3}))
            .unwrap();
        assert_eq!(doc["a"], 3);
        assert_eq!(doc["_rev"], 3);
    }

    /// The bulk mutations commit in chunks under their shards; every match
    /// across the chunk boundary is mutated and counted.
    #[test]
    fn bulk_mutations_cover_every_match_across_their_chunks() {
        let (_tmp, storage) = open();
        let total = BULK_MUTATION_CHUNK * 2 + 17;
        let docs: Vec<Value> = (0..total)
            .map(|i| json!({"kind": if i % 3 == 0 { "a" } else { "b" }, "n": i}))
            .collect();
        let (inserted, errors) = storage.bulk_insert_documents("c", docs).unwrap();
        assert_eq!((inserted as usize, errors.len()), (total, 0));

        let filter = parse_filter(&json!({"kind": "a"})).unwrap();
        let expected = total.div_ceil(3) as u64;
        let updated = storage
            .update_by_query("c", &filter, &json!({"$set": {"seen": true}}))
            .unwrap();
        assert_eq!(updated, expected);
        assert_eq!(
            storage
                .matching_ids("c", &parse_filter(&json!({"seen": true})).unwrap())
                .unwrap()
                .len() as u64,
            expected
        );

        let deleted = storage.delete_by_query("c", &filter).unwrap();
        assert_eq!(deleted, expected);
        assert_eq!(storage.doc_counts.get("c") as u64, total as u64 - expected);
        assert!(storage.matching_ids("c", &filter).unwrap().is_empty());
    }

    /// A bulk insert reports its faults in document order whichever pass
    /// found them, and an id the collection holds is refused under its
    /// shard while the rest of the batch lands.
    #[test]
    fn a_bulk_insert_keeps_its_errors_in_document_order() {
        let (_tmp, storage) = open();
        storage
            .insert_document("c", json!({"_id": "taken"}))
            .unwrap();
        let (inserted, errors) = storage
            .bulk_insert_documents(
                "c",
                vec![
                    json!({"_id": "taken", "n": 0}),
                    json!("not an object"),
                    json!({"_id": "fresh", "n": 2}),
                    json!({"_id": "fresh", "n": 3}),
                ],
            )
            .unwrap();
        assert_eq!(inserted, 1);
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(errors[0].starts_with("Document 0:") && errors[0].contains("already exists"));
        assert!(errors[1].starts_with("Document 1:"));
        assert!(errors[2].starts_with("Document 3:") && errors[2].contains("duplicate _id"));
        assert_eq!(storage.get_document("c", "fresh").unwrap()["n"], 2);
        assert_eq!(storage.doc_counts.get("c"), 2);
    }
}
