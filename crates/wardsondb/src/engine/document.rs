use std::collections::HashSet;
use std::ops::ControlFlow;

use chrono::Utc;
use serde_json::Value;
use uuid::Uuid;

use crate::engine::backend::StorageBackend;
use crate::error::AppError;
use crate::query::filter::FilterNode;

use super::storage::Storage;

const MAX_DOCUMENT_SIZE: usize = 16 * 1024 * 1024; // 16 MB
const MAX_BULK_INSERT: usize = 10_000;
const MAX_CUSTOM_ID_LENGTH: usize = 512;

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

        // Determine ID: use custom _id if provided, otherwise generate UUIDv7
        let id = if let Some(raw_id) = obj.remove("_id") {
            let custom_id = validate_custom_id(&raw_id)?;
            // Check for duplicate
            if self
                .engine
                .get(&docs_partition, custom_id.as_bytes())?
                .is_some()
            {
                return Err(AppError::DocumentConflict(format!(
                    "Document already exists: {custom_id}"
                )));
            }
            custom_id
        } else {
            Uuid::now_v7().to_string()
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
        mut doc: Value,
    ) -> Result<Value, AppError> {
        self.check_not_poisoned()?;
        self.ensure_collection_exists(collection)?;

        let docs_partition = self.get_docs_partition(collection)?;
        let existing_bytes = self
            .engine
            .get(&docs_partition, id.as_bytes())?
            .ok_or_else(|| AppError::DocumentNotFound(id.to_string()))?;
        let existing_doc: Value = serde_json::from_slice(&existing_bytes)?;

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
        batch.insert(&docs_partition, id.as_bytes(), &bytes)?;
        self.index_manager
            .add_index_entries_to_batch(&mut batch, collection, id, &doc)?;
        self.commit_batch(batch)?;

        self.scan_accelerator
            .on_update(collection, id, &existing_doc, &doc);

        Ok(doc)
    }

    pub fn partial_update_document(
        &self,
        collection: &str,
        id: &str,
        patch: Value,
    ) -> Result<Value, AppError> {
        let mut existing = self.get_document(collection, id)?;
        json_merge_patch(&mut existing, &patch);
        self.replace_document(collection, id, existing)
    }

    pub fn delete_document(&self, collection: &str, id: &str) -> Result<(), AppError> {
        self.check_not_poisoned()?;
        self.ensure_collection_exists(collection)?;

        let docs_partition = self.get_docs_partition(collection)?;
        let existing_bytes = self
            .engine
            .get(&docs_partition, id.as_bytes())?
            .ok_or_else(|| AppError::DocumentNotFound(id.to_string()))?;
        let existing_doc: Value = serde_json::from_slice(&existing_bytes)?;

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
        let mut inserted = 0u64;
        let mut errors = Vec::new();
        // (id, bytes, doc) — keep doc around for index writes
        let mut to_write: Vec<(String, Vec<u8>, Value)> = Vec::new();
        let mut seen_ids: HashSet<String> = HashSet::new();

        for (i, mut doc) in documents.into_iter().enumerate() {
            let obj = match doc.as_object_mut() {
                Some(obj) => obj,
                None => {
                    errors.push(format!("Document {i}: must be a JSON object"));
                    continue;
                }
            };

            // Determine ID: use custom _id if provided, otherwise generate UUIDv7
            let id = if let Some(raw_id) = obj.remove("_id") {
                match validate_custom_id(&raw_id) {
                    Ok(custom_id) => {
                        // Check for intra-batch duplicate
                        if seen_ids.contains(&custom_id) {
                            errors
                                .push(format!("Document {i}: duplicate _id in batch: {custom_id}"));
                            continue;
                        }
                        // Check for existing document in collection
                        match self.engine.get(&docs_partition, custom_id.as_bytes()) {
                            Ok(Some(_)) => {
                                errors.push(format!(
                                    "Document {i}: document already exists: {custom_id}"
                                ));
                                continue;
                            }
                            Ok(None) => {}
                            Err(e) => {
                                errors.push(format!("Document {i}: {e}"));
                                continue;
                            }
                        }
                        custom_id
                    }
                    Err(e) => {
                        errors.push(format!("Document {i}: {e}"));
                        continue;
                    }
                }
            } else {
                Uuid::now_v7().to_string()
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
                        errors.push(format!("Document {i}: exceeds 16 MB size limit"));
                    } else {
                        seen_ids.insert(id.clone());
                        to_write.push((id, bytes, doc));
                        inserted += 1;
                    }
                }
                Err(e) => errors.push(format!("Document {i}: {e}")),
            }
        }

        if !to_write.is_empty() {
            let mut batch = self.write_batch();
            for (id, bytes, doc) in &to_write {
                batch.insert(&docs_partition, id.as_bytes(), bytes.as_slice())?;
                self.index_manager
                    .add_index_entries_to_batch(&mut batch, collection, id, doc)?;
            }
            self.commit_batch(batch)?;
            self.doc_counts.increment(collection, inserted as i64);

            for (id, _bytes, doc) in &to_write {
                self.scan_accelerator.on_insert(collection, id, doc);
            }
        }

        Ok((inserted, errors))
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

    /// Delete all documents matching a filter. Returns the count of deleted documents.
    pub fn delete_by_query(&self, collection: &str, filter: &FilterNode) -> Result<u64, AppError> {
        self.check_not_poisoned()?;

        // Stream the match phase: only matching docs stay resident (each is
        // needed for index-entry removal and the accelerator hooks). The
        // scan's read snapshot is released before the write below — the
        // delete is still ONE atomic batch.
        let mut matching: Vec<Value> = Vec::new();
        self.for_each_document(collection, &mut |doc| {
            if filter.matches(&doc) {
                matching.push(doc);
            }
            ControlFlow::Continue(())
        })?;

        let count = matching.len() as u64;
        if count == 0 {
            return Ok(0);
        }

        let docs_partition = self.get_docs_partition(collection)?;
        let mut batch = self.write_batch();

        for doc in &matching {
            if let Some(id) = doc.get("_id").and_then(|v| v.as_str()) {
                self.index_manager
                    .remove_index_entries_from_batch(&mut batch, collection, id, doc)?;
                batch.remove(&docs_partition, id.as_bytes())?;
            }
        }

        self.commit_batch(batch)?;
        self.doc_counts.increment(collection, -(count as i64));

        for doc in &matching {
            if let Some(id) = doc.get("_id").and_then(|v| v.as_str()) {
                self.scan_accelerator.on_delete(collection, id, doc);
            }
        }

        Ok(count)
    }

    /// Update all documents matching a filter with $set operations.
    /// Returns the count of updated documents.
    pub fn update_by_query(
        &self,
        collection: &str,
        filter: &FilterNode,
        update: &Value,
    ) -> Result<u64, AppError> {
        self.check_not_poisoned()?;

        let set_fields = parse_set_updates(update)?;

        // Stream the match phase (matches-only resident; single atomic
        // batch below, after the scan's snapshot is released).
        let mut matching: Vec<Value> = Vec::new();
        self.for_each_document(collection, &mut |doc| {
            if filter.matches(&doc) {
                matching.push(doc);
            }
            ControlFlow::Continue(())
        })?;

        let count = matching.len() as u64;
        if count == 0 {
            return Ok(0);
        }

        let docs_partition = self.get_docs_partition(collection)?;
        let now = Utc::now().to_rfc3339();
        let mut batch = self.write_batch();

        // Track (id, old_doc, new_doc) for bitmap accelerator
        let mut update_pairs: Vec<(String, Value, Value)> = Vec::new();

        for doc in matching {
            let old_doc = doc.clone();
            let mut new_doc = doc;
            let id = new_doc
                .get("_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let old_rev = new_doc.get("_rev").and_then(|v| v.as_u64()).unwrap_or(0);

            self.index_manager
                .remove_index_entries_from_batch(&mut batch, collection, &id, &old_doc)?;

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
                .add_index_entries_to_batch(&mut batch, collection, &id, &new_doc)?;

            update_pairs.push((id, old_doc, new_doc));
        }

        self.commit_batch(batch)?;

        for (id, old_doc, new_doc) in &update_pairs {
            self.scan_accelerator
                .on_update(collection, id, old_doc, new_doc);
        }

        Ok(count)
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
