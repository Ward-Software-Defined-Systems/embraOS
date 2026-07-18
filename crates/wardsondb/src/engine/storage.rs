use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};

use parking_lot::RwLock;

use tracing::info;

use uuid::Uuid;

use crate::engine::backend::{
    Engine, EngineConfig, PartitionId, StorageBackend, WriteBatchWrapper,
};
use crate::engine::bitmap::{AcceleratorConfig, ScanAccelerator};
use crate::error::AppError;
use crate::index::IndexManager;

/// Memory configuration for the storage backend.
/// Prevents unbounded memory growth under sustained write load.
pub struct MemoryConfig {
    /// Unified block + blob cache size (default: 64 MiB)
    pub cache_size: u64,
    /// Total memory cap for all write buffers across all partitions (default: 64 MiB)
    pub max_write_buffer_size: u64,
    /// Per-partition memtable size limit (default: 8 MiB)
    pub max_memtable_size: u32,
    /// Number of background flush worker threads (default: 2)
    pub flush_workers: usize,
    /// Number of background compaction worker threads (default: 2)
    pub compaction_workers: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        MemoryConfig {
            cache_size: 64 * 1024 * 1024,
            max_write_buffer_size: 64 * 1024 * 1024,
            max_memtable_size: 8 * 1024 * 1024,
            flush_workers: 2,
            compaction_workers: 2,
        }
    }
}

/// Atomic document counters per collection — O(1) doc count reads.
pub struct DocCounters {
    counts: RwLock<HashMap<String, AtomicI64>>,
}

impl Default for DocCounters {
    fn default() -> Self {
        Self::new()
    }
}

impl DocCounters {
    pub fn new() -> Self {
        DocCounters {
            counts: RwLock::new(HashMap::new()),
        }
    }

    pub fn initialize(&self, collection: &str, count: i64) {
        let mut map = self.counts.write();
        map.insert(collection.to_string(), AtomicI64::new(count));
    }

    pub fn increment(&self, collection: &str, delta: i64) {
        {
            let map = self.counts.read();
            if let Some(counter) = map.get(collection) {
                counter.fetch_add(delta, Ordering::Relaxed);
                return;
            }
        }
        // Unseeded counter (a write racing collection creation, or any path
        // the startup seed missed): dropping the delta would undercount
        // forever — count_only serves straight from these counters.
        self.counts
            .write()
            .entry(collection.to_string())
            .or_insert_with(|| AtomicI64::new(0))
            .fetch_add(delta, Ordering::Relaxed);
    }

    pub fn get(&self, collection: &str) -> i64 {
        let map = self.counts.read();
        map.get(collection)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    pub fn remove(&self, collection: &str) {
        let mut map = self.counts.write();
        map.remove(collection);
    }
}

pub struct Storage {
    pub engine: Engine,
    pub meta: PartitionId,
    pub index_manager: IndexManager,
    pub doc_counts: DocCounters,
    pub memory_config: MemoryConfig,
    pub scan_accelerator: ScanAccelerator,
    pub engine_name: &'static str,
    /// Collections known to exist — replaces the `_meta` point read that
    /// every document op and query paid. Seeded at startup; insert happens
    /// AFTER a successful create commit, removal BEFORE the drop commit
    /// (re-inserted if that commit fails), so a miss never claims existence
    /// for a half-dropped collection. Misses fall back to `_meta`.
    pub(crate) collections: RwLock<HashSet<String>>,
    /// Partition handles by name. Staleness-safe because partitions are
    /// NEVER dropped — drop_collection/drop_index empty them and reuse them
    /// on re-create (invariant noted at drop_collection).
    pub(crate) partitions: RwLock<HashMap<String, PartitionId>>,
}

impl Storage {
    #[allow(dead_code)]
    pub fn open(data_dir: &Path) -> Result<Self, AppError> {
        Self::open_with_config(data_dir, "rocksdb", MemoryConfig::default())
    }

    pub fn open_with_config(
        data_dir: &Path,
        engine_type: &str,
        mem: MemoryConfig,
    ) -> Result<Self, AppError> {
        info!(
            engine = engine_type,
            cache_mb = mem.cache_size / (1024 * 1024),
            write_buffer_mb = mem.max_write_buffer_size / (1024 * 1024),
            memtable_mb = mem.max_memtable_size / (1024 * 1024),
            flush_workers = mem.flush_workers,
            compaction_workers = mem.compaction_workers,
            "Opening database"
        );

        check_engine_marker(data_dir, engine_type)?;

        let engine_config = EngineConfig {
            cache_size_bytes: mem.cache_size,
            write_buffer_bytes: mem.max_write_buffer_size,
            memtable_bytes: mem.max_memtable_size,
            flush_workers: mem.flush_workers,
            compaction_workers: mem.compaction_workers,
        };

        let engine = Engine::open(engine_type, data_dir, &engine_config)?;
        let engine_name = engine.engine_name();

        write_engine_marker(data_dir, engine_name)?;

        let meta = engine.create_or_open_partition("_meta")?;

        let index_manager = IndexManager::new();
        let doc_counts = DocCounters::new();
        let scan_accelerator = ScanAccelerator::new(AcceleratorConfig::default());

        let storage = Storage {
            engine,
            meta,
            index_manager,
            doc_counts,
            memory_config: mem,
            scan_accelerator,
            engine_name,
            collections: RwLock::new(HashSet::new()),
            partitions: RwLock::new(HashMap::new()),
        };

        storage
            .index_manager
            .load_indexes(&storage.engine, &storage.meta)?;
        storage.initialize_doc_counts()?;

        Ok(storage)
    }

    /// Check if the storage engine is in a poisoned state.
    pub fn is_poisoned(&self) -> bool {
        self.engine.is_poisoned()
    }

    fn initialize_doc_counts(&self) -> Result<(), AppError> {
        // Names first, partition opens + counts after the registry scan's
        // snapshot is released. Counts are keys-only and exact — required:
        // these counters are authoritative for count_only. This pass also
        // warms the existence and partition caches.
        for col_name in self.collection_names()? {
            let docs_partition = self.create_partition(&format!("{col_name}#docs"))?;
            let count = self.engine.count_prefix(&docs_partition, b"")? as i64;
            self.doc_counts.initialize(&col_name, count);
            self.collections.write().insert(col_name);
        }
        Ok(())
    }

    pub fn create_partition(&self, name: &str) -> Result<PartitionId, AppError> {
        if let Some(p) = self.partitions.read().get(name) {
            return Ok(p.clone());
        }
        let p = self.engine.create_or_open_partition(name)?;
        self.partitions.write().insert(name.to_string(), p.clone());
        Ok(p)
    }

    pub fn write_batch(&self) -> WriteBatchWrapper {
        self.engine.write_batch()
    }

    pub fn commit_batch(&self, batch: WriteBatchWrapper) -> Result<(), AppError> {
        self.engine.commit_batch(batch)?;
        Ok(())
    }

    /// Guard that rejects writes immediately if the storage is already poisoned.
    pub fn check_not_poisoned(&self) -> Result<(), AppError> {
        if self.is_poisoned() {
            return Err(AppError::StoragePoisoned);
        }
        Ok(())
    }

    /// Get the oldest and newest document timestamps from first/last keys.
    /// Tries UUIDv7 timestamp extraction first; falls back to reading `_received_at`.
    pub fn get_doc_time_range(
        &self,
        collection: &str,
    ) -> Result<(Option<String>, Option<String>), AppError> {
        if self.doc_counts.get(collection) <= 0 {
            return Ok((None, None));
        }

        let docs_partition = self.get_docs_partition(collection)?;

        let oldest = self
            .engine
            .first_key(&docs_partition)?
            .and_then(|k| self.key_to_timestamp(&k, &docs_partition));
        let newest = self
            .engine
            .last_key(&docs_partition)?
            .and_then(|k| self.key_to_timestamp(&k, &docs_partition));

        Ok((oldest, newest))
    }

    fn key_to_timestamp(&self, key_bytes: &[u8], partition: &PartitionId) -> Option<String> {
        let key_str = std::str::from_utf8(key_bytes).ok()?;
        if let Ok(uuid) = Uuid::parse_str(key_str)
            && let Some(ts) = uuid.get_timestamp()
        {
            let (secs, _nanos) = ts.to_unix();
            if let Some(dt) = chrono::DateTime::from_timestamp(secs as i64, 0) {
                return Some(dt.to_rfc3339());
            }
        }
        if let Ok(Some(doc_bytes)) = self.engine.get(partition, key_str.as_bytes())
            && let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&doc_bytes)
        {
            return doc
                .get("_received_at")
                .and_then(|v| v.as_str())
                .map(String::from);
        }
        None
    }

    pub fn persist(&self) -> Result<(), AppError> {
        self.engine.flush()?;
        Ok(())
    }
}

/// Check that the engine marker matches what the caller asked for.
/// Hard error on mismatch — never silently switch engines on existing data.
fn check_engine_marker(data_dir: &Path, engine_type: &str) -> Result<(), AppError> {
    let marker = data_dir.join(".engine");
    if !marker.exists() {
        return Ok(());
    }
    let existing = std::fs::read_to_string(&marker)
        .map_err(|e| AppError::Internal(format!("Failed to read engine marker: {e}")))?;
    let existing = existing.trim();
    if existing != engine_type {
        return Err(AppError::Internal(format!(
            "Data directory was created with engine '{existing}' but '{engine_type}' was requested. \
             Cannot switch engines on existing data; use a different --data-dir or re-create with \
             the desired engine."
        )));
    }
    Ok(())
}

fn write_engine_marker(data_dir: &Path, engine_name: &str) -> Result<(), AppError> {
    let marker = data_dir.join(".engine");
    if marker.exists() {
        return Ok(());
    }
    std::fs::write(&marker, engine_name)
        .map_err(|e| AppError::Internal(format!("Failed to write engine marker: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_counters_upsert_on_unseeded_increment() {
        let counters = DocCounters::new();
        counters.increment("logs", 1); // no initialize — must not be dropped
        counters.increment("logs", 1);
        assert_eq!(counters.get("logs"), 2);
    }

    #[test]
    fn doc_counters_initialize_overrides_and_remove_clears() {
        let counters = DocCounters::new();
        counters.increment("logs", 5);
        counters.initialize("logs", 100); // startup seeding is authoritative
        assert_eq!(counters.get("logs"), 100);
        counters.increment("logs", -1);
        assert_eq!(counters.get("logs"), 99);
        counters.remove("logs");
        assert_eq!(counters.get("logs"), 0);
    }
}
