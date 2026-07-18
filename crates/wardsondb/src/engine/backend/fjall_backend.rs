//! Fjall backend implementation.

use std::ops::ControlFlow;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use fjall::{Batch, Config, PartitionCreateOptions, PersistMode, TxKeyspace};

use super::{
    BackendError, BackendResult, EngineConfig, PartitionId, ScanVisitor, StorageBackend,
    WriteBatchWrapper,
};

pub struct FjallBackend {
    db: TxKeyspace,
    partition_opts: PartitionCreateOptions,
    poisoned: AtomicBool,
}

impl FjallBackend {
    pub fn open(path: &Path, config: &EngineConfig) -> BackendResult<Self> {
        let db = Config::new(path)
            .cache_size(config.cache_size_bytes)
            .max_write_buffer_size(config.write_buffer_bytes)
            .flush_workers(config.flush_workers)
            .compaction_workers(config.compaction_workers)
            .open_transactional()
            .map_err(|e| BackendError::Internal(format!("fjall open failed: {e}")))?;

        let partition_opts =
            PartitionCreateOptions::default().max_memtable_size(config.memtable_bytes);

        Ok(FjallBackend {
            db,
            partition_opts,
            poisoned: AtomicBool::new(false),
        })
    }

    fn check_poison<T>(&self, r: Result<T, fjall::Error>) -> BackendResult<T> {
        match r {
            Ok(v) => Ok(v),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("oison") {
                    self.poisoned.store(true, Ordering::Relaxed);
                    return Err(BackendError::Poisoned(msg));
                }
                Err(BackendError::Internal(msg))
            }
        }
    }
}

impl StorageBackend for FjallBackend {
    fn create_or_open_partition(&self, name: &str) -> BackendResult<PartitionId> {
        let handle = self
            .db
            .open_partition(name, self.partition_opts.clone())
            .map_err(|e| BackendError::Internal(format!("open partition '{name}': {e}")))?;
        Ok(PartitionId::Fjall(handle))
    }

    fn get(&self, partition: &PartitionId, key: &[u8]) -> BackendResult<Option<Vec<u8>>> {
        let PartitionId::Fjall(handle) = partition else {
            return Err(BackendError::Internal(
                "PartitionId/backend mismatch".into(),
            ));
        };
        match handle.get(key) {
            Ok(Some(v)) => Ok(Some(v.to_vec())),
            Ok(None) => Ok(None),
            Err(e) => Err(BackendError::Internal(e.to_string())),
        }
    }

    fn get_many(
        &self,
        partition: &PartitionId,
        keys: &[&[u8]],
    ) -> BackendResult<Vec<Option<Vec<u8>>>> {
        let PartitionId::Fjall(handle) = partition else {
            return Err(BackendError::Internal(
                "PartitionId/backend mismatch".into(),
            ));
        };
        // fjall has no native multi-get; per-key gets match the read
        // semantics the hydration loops have always had.
        Ok(keys
            .iter()
            .map(|key| match handle.get(key) {
                Ok(Some(v)) => Some(v.to_vec()),
                _ => None,
            })
            .collect())
    }

    fn scan_prefix(
        &self,
        partition: &PartitionId,
        prefix: &[u8],
        visit: &mut ScanVisitor<'_>,
    ) -> BackendResult<()> {
        let PartitionId::Fjall(handle) = partition else {
            return Err(BackendError::Internal(
                "PartitionId/backend mismatch".into(),
            ));
        };
        let rtx = self.db.read_tx();
        for item in rtx.prefix(handle, prefix) {
            let (k, v) = item.map_err(|e| BackendError::Internal(e.to_string()))?;
            if visit(&k, &v).is_break() {
                break;
            }
        }
        Ok(())
    }

    fn scan_range(
        &self,
        partition: &PartitionId,
        start: &[u8],
        end: &[u8],
        visit: &mut ScanVisitor<'_>,
    ) -> BackendResult<()> {
        if start >= end {
            // fjall's RangeBounds would panic on an inverted range.
            return Ok(());
        }
        let PartitionId::Fjall(handle) = partition else {
            return Err(BackendError::Internal(
                "PartitionId/backend mismatch".into(),
            ));
        };
        let rtx = self.db.read_tx();
        for item in rtx.range(handle, start.to_vec()..end.to_vec()) {
            let (k, v) = item.map_err(|e| BackendError::Internal(e.to_string()))?;
            if visit(&k, &v).is_break() {
                break;
            }
        }
        Ok(())
    }

    fn scan_range_rev(
        &self,
        partition: &PartitionId,
        start: &[u8],
        end: &[u8],
        visit: &mut ScanVisitor<'_>,
    ) -> BackendResult<()> {
        if start >= end {
            // fjall's RangeBounds would panic on an inverted range.
            return Ok(());
        }
        let PartitionId::Fjall(handle) = partition else {
            return Err(BackendError::Internal(
                "PartitionId/backend mismatch".into(),
            ));
        };
        let rtx = self.db.read_tx();
        for item in rtx.range(handle, start.to_vec()..end.to_vec()).rev() {
            let (k, v) = item.map_err(|e| BackendError::Internal(e.to_string()))?;
            if visit(&k, &v).is_break() {
                break;
            }
        }
        Ok(())
    }

    fn scan_full(&self, partition: &PartitionId, visit: &mut ScanVisitor<'_>) -> BackendResult<()> {
        let PartitionId::Fjall(handle) = partition else {
            return Err(BackendError::Internal(
                "PartitionId/backend mismatch".into(),
            ));
        };
        let rtx = self.db.read_tx();
        for item in rtx.iter(handle) {
            let (k, v) = item.map_err(|e| BackendError::Internal(e.to_string()))?;
            if visit(&k, &v).is_break() {
                break;
            }
        }
        Ok(())
    }

    fn count_prefix(&self, partition: &PartitionId, prefix: &[u8]) -> BackendResult<u64> {
        // Counting is a visitor scan that keeps nothing — slices stay
        // refcounted views, no to_vec copies. Snapshot-exact (MVCC tx).
        let mut count = 0u64;
        self.scan_prefix(partition, prefix, &mut |_, _| {
            count += 1;
            ControlFlow::Continue(())
        })?;
        Ok(count)
    }

    fn count_range(&self, partition: &PartitionId, start: &[u8], end: &[u8]) -> BackendResult<u64> {
        let mut count = 0u64;
        self.scan_range(partition, start, end, &mut |_, _| {
            count += 1;
            ControlFlow::Continue(())
        })?;
        Ok(count)
    }

    fn first_key(&self, partition: &PartitionId) -> BackendResult<Option<Vec<u8>>> {
        let mut first = None;
        self.scan_full(partition, &mut |k, _| {
            first = Some(k.to_vec());
            ControlFlow::Break(())
        })?;
        Ok(first)
    }

    fn last_key(&self, partition: &PartitionId) -> BackendResult<Option<Vec<u8>>> {
        let PartitionId::Fjall(handle) = partition else {
            return Err(BackendError::Internal(
                "PartitionId/backend mismatch".into(),
            ));
        };
        let rtx = self.db.read_tx();
        match rtx.iter(handle).next_back() {
            Some(Ok((k, _))) => Ok(Some(k.to_vec())),
            Some(Err(e)) => Err(BackendError::Internal(e.to_string())),
            None => Ok(None),
        }
    }

    fn write_batch(&self) -> WriteBatchWrapper {
        let batch: Batch = self
            .db
            .inner()
            .batch()
            .durability(Some(PersistMode::Buffer));
        WriteBatchWrapper::Fjall(batch)
    }

    fn commit_batch(&self, batch: WriteBatchWrapper) -> BackendResult<()> {
        let WriteBatchWrapper::Fjall(b) = batch else {
            return Err(BackendError::Internal(
                "WriteBatchWrapper/backend mismatch".into(),
            ));
        };
        self.check_poison(b.commit())
    }

    fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Relaxed)
    }

    fn flush(&self) -> BackendResult<()> {
        self.db
            .persist(PersistMode::Buffer)
            .map_err(|e| BackendError::Internal(format!("persist failed: {e}")))
    }

    fn engine_name(&self) -> &'static str {
        "fjall"
    }
}
