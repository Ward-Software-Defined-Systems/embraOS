use std::ops::ControlFlow;

use serde::{Deserialize, Serialize};

use crate::engine::backend::{PartitionId, StorageBackend, collect_keys};
use crate::error::AppError;

use super::storage::Storage;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionInfo {
    pub name: String,
    pub doc_count: u64,
    pub indexes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CollectionMeta {
    name: String,
    created_at: String,
}

impl Storage {
    pub fn create_collection(&self, name: &str) -> Result<CollectionInfo, AppError> {
        self.check_not_poisoned()?;
        let meta_key = format!("collection:{name}");
        if self.engine.get(&self.meta, meta_key.as_bytes())?.is_some() {
            return Err(AppError::CollectionExists(name.to_string()));
        }

        // Create the docs partition
        let ks_name = format!("{name}#docs");
        self.create_partition(&ks_name)?;

        // Store collection metadata
        let meta = CollectionMeta {
            name: name.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        let meta_bytes = serde_json::to_vec(&meta)?;

        let mut batch = self.write_batch();
        batch.insert(&self.meta, meta_key.as_bytes(), &meta_bytes)?;
        // Seed before the collection becomes visible: initialize() overwrites,
        // so seeding after commit could stomp a counter a racing insert already
        // upserted. Pre-commit nothing can observe the collection, and a stale
        // zero from a failed commit is unreadable (collection_exists gates
        // every read/write path).
        self.doc_counts.initialize(name, 0);
        self.commit_batch(batch)?;
        // Cache only after the commit: a pre-commit insert could let a
        // racing write land docs for a collection whose meta commit fails.
        self.collections.write().insert(name.to_string());

        self.persist()?;

        if self.scan_accelerator.has_columns() {
            self.scan_accelerator.set_ready(true);
        }

        Ok(CollectionInfo {
            name: name.to_string(),
            doc_count: 0,
            indexes: vec![],
        })
    }

    /// Every registered collection name, from the `_meta` registry.
    pub(crate) fn collection_names(&self) -> Result<Vec<String>, AppError> {
        let mut names = Vec::new();
        let mut key_err: Option<AppError> = None;
        self.engine.scan_prefix(
            &self.meta,
            b"collection:",
            &mut |key, _| match std::str::from_utf8(key) {
                Ok(s) => {
                    names.push(s.strip_prefix("collection:").unwrap_or(s).to_string());
                    ControlFlow::Continue(())
                }
                Err(e) => {
                    key_err = Some(AppError::Internal(format!("Invalid key: {e}")));
                    ControlFlow::Break(())
                }
            },
        )?;
        if let Some(e) = key_err {
            return Err(e);
        }
        Ok(names)
    }

    pub fn list_collections(&self) -> Result<Vec<CollectionInfo>, AppError> {
        // Names first (borrowed-key scan), per-collection lock reads after
        // the scan's snapshot is released.
        let names = self.collection_names()?;
        let mut collections = Vec::new();
        for col_name in names {
            let doc_count = self.doc_counts.get(&col_name).max(0) as u64;
            let indexes = self
                .index_manager
                .get_indexes_for_collection(&col_name)
                .iter()
                .map(|d| d.name.clone())
                .collect();

            collections.push(CollectionInfo {
                name: col_name,
                doc_count,
                indexes,
            });
        }
        Ok(collections)
    }

    pub fn get_collection_info(&self, name: &str) -> Result<CollectionInfo, AppError> {
        let meta_key = format!("collection:{name}");
        if self.engine.get(&self.meta, meta_key.as_bytes())?.is_none() {
            return Err(AppError::CollectionNotFound(name.to_string()));
        }

        let doc_count = self.doc_counts.get(name).max(0) as u64;
        let indexes = self
            .index_manager
            .get_indexes_for_collection(name)
            .iter()
            .map(|d| d.name.clone())
            .collect();

        Ok(CollectionInfo {
            name: name.to_string(),
            doc_count,
            indexes,
        })
    }

    pub fn drop_collection(&self, name: &str) -> Result<(), AppError> {
        self.check_not_poisoned()?;
        let meta_key = format!("collection:{name}");
        if self.engine.get(&self.meta, meta_key.as_bytes())?.is_none() {
            return Err(AppError::CollectionNotFound(name.to_string()));
        }

        let docs_partition = self.get_docs_partition(name)?;

        // Collect all doc keys (owned, before starting the batch) — keys
        // only, values never copied. A mid-scan engine error now aborts the
        // whole drop instead of silently dropping a truncated key set (which
        // left orphan keys behind a "successful" drop).
        let keys = collect_keys(&self.engine, &docs_partition)?;

        let index_defs = self.index_manager.get_indexes_for_collection(name);

        let mut batch = self.write_batch();

        for key in &keys {
            batch.remove(&docs_partition, key)?;
        }

        for idx_def in &index_defs {
            if let Some(idx_partition) = self.index_manager.get_index_partition(name, &idx_def.name)
            {
                let idx_keys = collect_keys(&self.engine, &idx_partition)?;
                for key in &idx_keys {
                    batch.remove(&idx_partition, key)?;
                }
            }
            let idx_meta_key = format!("index:{}:{}", name, idx_def.name);
            batch.remove(&self.meta, idx_meta_key.as_bytes())?;
        }

        batch.remove(&self.meta, meta_key.as_bytes())?;
        // Uncache BEFORE the commit (re-cache if it fails): once the commit
        // lands, no writer may slip a doc into the dropped collection via a
        // stale existence hit. NOTE the partition itself is never dropped —
        // only emptied — which is what keeps Storage::partitions and every
        // cached PartitionId valid across drops; a re-created collection
        // reuses the same partition.
        self.collections.write().remove(name);
        if let Err(e) = self.commit_batch(batch) {
            self.collections.write().insert(name.to_string());
            return Err(e);
        }

        for idx_def in &index_defs {
            self.index_manager.unregister(name, &idx_def.name);
        }

        self.doc_counts.remove(name);
        // Surgical: only this collection's positions and bits leave the
        // accelerator — every other collection stays accelerated (this used
        // to clear() the whole thing and delete the persisted snapshot,
        // leaving bitmaps off until restart). The snapshot on disk is
        // rewritten by the 60s persist task; a crash before that leaves a
        // stale snapshot whose membership for this collection the
        // per-collection load reconcile rejects — rebuild, not wrong data.
        self.scan_accelerator.on_drop_collection(name);

        self.persist()?;
        Ok(())
    }

    pub fn collection_exists(&self, name: &str) -> Result<bool, AppError> {
        // Read-mostly cache: this runs on EVERY document op and query, and
        // used to be an engine point read each time. Misses still consult
        // `_meta` (and self-heal the cache), so a false negative is
        // impossible; a false positive is prevented by the drop path
        // removing the entry BEFORE its commit.
        if self.collections.read().contains(name) {
            return Ok(true);
        }
        let meta_key = format!("collection:{name}");
        let exists = self.engine.get(&self.meta, meta_key.as_bytes())?.is_some();
        if exists {
            self.collections.write().insert(name.to_string());
        }
        Ok(exists)
    }

    pub fn get_docs_partition(&self, collection: &str) -> Result<PartitionId, AppError> {
        let ks_name = format!("{collection}#docs");
        self.create_partition(&ks_name)
    }

    #[allow(dead_code)]
    pub fn collection_doc_count(&self, name: &str) -> Result<u64, AppError> {
        Ok(self.doc_counts.get(name).max(0) as u64)
    }
}
