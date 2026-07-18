//! Storage backend abstraction.
//!
//! WardSONDB supports two pluggable KV engines: fjall and RocksDB. This module
//! defines the [`StorageBackend`] trait plus the enum-dispatched [`Engine`] type
//! that the rest of the codebase talks to. Enum dispatch avoids propagating a
//! generic parameter through every module while still monomorphizing calls.

pub mod fjall_backend;
pub mod rocksdb_backend;

use std::path::Path;

use crate::error::AppError;

pub use fjall_backend::FjallBackend;
pub use rocksdb_backend::RocksDbBackend;

/// Tunables passed to a backend on open. Sizes are in bytes.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub cache_size_bytes: u64,
    pub write_buffer_bytes: u64,
    pub memtable_bytes: u32,
    pub flush_workers: usize,
    pub compaction_workers: usize,
}

/// Errors returned by the storage backend.
#[derive(Debug)]
pub enum BackendError {
    /// Generic I/O or internal engine error.
    Internal(String),
    /// Engine has entered a fatal state — no more writes should be accepted.
    Poisoned(String),
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendError::Internal(m) => write!(f, "backend internal: {m}"),
            BackendError::Poisoned(m) => write!(f, "backend poisoned: {m}"),
        }
    }
}

impl std::error::Error for BackendError {}

impl From<BackendError> for AppError {
    fn from(e: BackendError) -> Self {
        match e {
            BackendError::Poisoned(msg) => {
                tracing::error!(
                    error = %msg,
                    "FATAL: Storage engine poisoned — background worker failed. \
                     All writes will be rejected. Restart required."
                );
                AppError::StoragePoisoned
            }
            BackendError::Internal(msg) => {
                tracing::error!(error = %msg, "Storage engine error");
                AppError::Internal(format!("Storage error: {msg}"))
            }
        }
    }
}

pub type BackendResult<T> = Result<T, BackendError>;

/// Visitor over borrowed key/value slices for the `scan_*` methods. Return
/// `ControlFlow::Break(())` to stop the scan early. The slices are only valid
/// for the duration of the call — copy what you keep.
pub type ScanVisitor<'a> = dyn FnMut(&[u8], &[u8]) -> std::ops::ControlFlow<()> + 'a;

/// Opaque handle to a partition (fjall partition / RocksDB column family).
///
/// Clone is cheap — both variants wrap an Arc-like internal handle.
#[derive(Clone)]
pub enum PartitionId {
    Fjall(::fjall::TxPartitionHandle),
    RocksDb {
        db: std::sync::Arc<::rust_rocksdb::DB>,
        cf_name: std::sync::Arc<str>,
    },
}

/// Atomic write batch. All staged mutations commit together.
///
/// For fjall we use `Batch` (non-serialized but atomic on commit) rather than
/// `WriteTransaction` — the transaction type borrows a `MutexGuard` from the
/// keyspace, which would force a lifetime parameter all the way up through
/// `Storage`. WardSONDB never uses read-modify-write inside a transaction, so
/// the weaker atomicity of `Batch` is sufficient.
///
/// RocksDB resolves the CF handle per staged key. A stage-then-resolve-once
/// design was tried and REVERTED on bench evidence: buffering (key, value)
/// pairs before the WriteBatch double-copied every payload and cost
/// bulk_insert_500 +33%, while the name-hash lookups it saved measure in
/// microseconds per batch. Don't reintroduce staging without a bench win.
pub enum WriteBatchWrapper {
    Fjall(::fjall::Batch),
    RocksDb {
        batch: ::rust_rocksdb::WriteBatch,
        db: std::sync::Arc<::rust_rocksdb::DB>,
    },
}

impl WriteBatchWrapper {
    // Both failure arms are unreachable today (nothing drops a CF, one engine
    // per process) — surfaced as BackendError rather than a mid-batch abort so
    // an eventual violation fails the request, not the process.
    pub fn insert(
        &mut self,
        partition: &PartitionId,
        key: &[u8],
        value: &[u8],
    ) -> BackendResult<()> {
        match (self, partition) {
            (WriteBatchWrapper::Fjall(batch), PartitionId::Fjall(handle)) => {
                batch.insert(handle.inner(), key, value);
                Ok(())
            }
            (WriteBatchWrapper::RocksDb { batch, db }, PartitionId::RocksDb { cf_name, .. }) => {
                let cf = db.cf_handle(cf_name).ok_or_else(|| {
                    BackendError::Internal(format!(
                        "column family '{cf_name}' missing at batch insert"
                    ))
                })?;
                batch.put_cf(&cf, key, value);
                Ok(())
            }
            _ => Err(BackendError::Internal(
                "WriteBatchWrapper / PartitionId backend mismatch".to_string(),
            )),
        }
    }

    pub fn remove(&mut self, partition: &PartitionId, key: &[u8]) -> BackendResult<()> {
        match (self, partition) {
            (WriteBatchWrapper::Fjall(batch), PartitionId::Fjall(handle)) => {
                batch.remove(handle.inner(), key);
                Ok(())
            }
            (WriteBatchWrapper::RocksDb { batch, db }, PartitionId::RocksDb { cf_name, .. }) => {
                let cf = db.cf_handle(cf_name).ok_or_else(|| {
                    BackendError::Internal(format!(
                        "column family '{cf_name}' missing at batch remove"
                    ))
                })?;
                batch.delete_cf(&cf, key);
                Ok(())
            }
            _ => Err(BackendError::Internal(
                "WriteBatchWrapper / PartitionId backend mismatch".to_string(),
            )),
        }
    }
}

/// Collect every key in a partition, values never copied. Used by the DDL
/// paths that stage whole-partition deletes; a mid-scan engine error aborts
/// the caller instead of truncating the key set.
pub(crate) fn collect_keys(
    engine: &Engine,
    partition: &PartitionId,
) -> BackendResult<Vec<Vec<u8>>> {
    let mut keys = Vec::new();
    engine.scan_full(partition, &mut |k, _| {
        keys.push(k.to_vec());
        std::ops::ControlFlow::Continue(())
    })?;
    Ok(keys)
}

// ─── Trait ───────────────────────────────────────────────────────────

pub trait StorageBackend: Send + Sync {
    fn create_or_open_partition(&self, name: &str) -> BackendResult<PartitionId>;
    fn get(&self, partition: &PartitionId, key: &[u8]) -> BackendResult<Option<Vec<u8>>>;
    /// Point-get many keys, one `Option` per key in input order. A per-key
    /// read failure maps to `None` — the same tolerance the doc-hydration
    /// loops have always had (`if let Ok(Some(bytes)) = get(...)`); S2-8 owns
    /// any future skip-vs-fail policy change.
    fn get_many(
        &self,
        partition: &PartitionId,
        keys: &[&[u8]],
    ) -> BackendResult<Vec<Option<Vec<u8>>>>;
    /// Visit every pair with the given prefix in ascending key order, without
    /// materializing anything.
    ///
    /// Contract for all four `scan_*` methods:
    /// - One consistent view per call (fjall: one `read_tx` snapshot; RocksDB:
    ///   one iterator pin), held for the duration of the call.
    /// - Keys/values are borrowed; entries the visitor skips are never copied.
    /// - The visitor MAY read re-entrantly through `get`/`get_many` or a
    ///   nested scan on the same engine (interleaved hydration — pinned by
    ///   `scan_visitor_reentrant_reads` below; fjall's `read_tx` is a
    ///   lock-free snapshot capture and its point reads bypass tx machinery).
    ///   It MUST NOT commit write batches — mutation paths collect their
    ///   matches first and write after the scan returns.
    /// - A mid-iteration engine error returns `Err` like `count_prefix` does,
    ///   never a silent truncation.
    fn scan_prefix(
        &self,
        partition: &PartitionId,
        prefix: &[u8],
        visit: &mut ScanVisitor<'_>,
    ) -> BackendResult<()>;
    /// Visit keys `start <= k < end` ascending. `start >= end` visits nothing
    /// (guarded — fjall's RangeBounds panics on an inverted range).
    fn scan_range(
        &self,
        partition: &PartitionId,
        start: &[u8],
        end: &[u8],
        visit: &mut ScanVisitor<'_>,
    ) -> BackendResult<()>;
    /// Visit the same half-open key set `start <= k < end` DESCENDING,
    /// starting from the largest key strictly below `end`.
    fn scan_range_rev(
        &self,
        partition: &PartitionId,
        start: &[u8],
        end: &[u8],
        visit: &mut ScanVisitor<'_>,
    ) -> BackendResult<()>;
    /// Visit the whole partition in ascending key order.
    fn scan_full(&self, partition: &PartitionId, visit: &mut ScanVisitor<'_>) -> BackendResult<()>;
    /// Count keys with the given prefix without materializing keys or
    /// values. Exact, under `scan_prefix`'s snapshot semantics; an empty
    /// prefix counts the whole partition. Iteration errors surface as `Err`.
    fn count_prefix(&self, partition: &PartitionId, prefix: &[u8]) -> BackendResult<u64>;
    /// Count keys `start <= k < end` without materializing. `start >= end`
    /// counts zero (guarded — an inverted range must not reach the engine).
    fn count_range(&self, partition: &PartitionId, start: &[u8], end: &[u8]) -> BackendResult<u64>;
    fn first_key(&self, partition: &PartitionId) -> BackendResult<Option<Vec<u8>>>;
    fn last_key(&self, partition: &PartitionId) -> BackendResult<Option<Vec<u8>>>;
    fn write_batch(&self) -> WriteBatchWrapper;
    fn commit_batch(&self, batch: WriteBatchWrapper) -> BackendResult<()>;
    fn is_poisoned(&self) -> bool;
    fn flush(&self) -> BackendResult<()>;
    fn engine_name(&self) -> &'static str;
}

// ─── Engine enum (delegating dispatch) ───────────────────────────────

pub enum Engine {
    Fjall(FjallBackend),
    RocksDb(RocksDbBackend),
}

impl Engine {
    pub fn open(engine_type: &str, path: &Path, config: &EngineConfig) -> BackendResult<Self> {
        match engine_type {
            "fjall" => Ok(Engine::Fjall(FjallBackend::open(path, config)?)),
            "rocksdb" => Ok(Engine::RocksDb(RocksDbBackend::open(path, config)?)),
            other => Err(BackendError::Internal(format!(
                "Unknown storage engine '{other}' (expected 'rocksdb' or 'fjall')"
            ))),
        }
    }
}

impl StorageBackend for Engine {
    fn create_or_open_partition(&self, name: &str) -> BackendResult<PartitionId> {
        match self {
            Engine::Fjall(b) => b.create_or_open_partition(name),
            Engine::RocksDb(b) => b.create_or_open_partition(name),
        }
    }
    fn get(&self, partition: &PartitionId, key: &[u8]) -> BackendResult<Option<Vec<u8>>> {
        match self {
            Engine::Fjall(b) => b.get(partition, key),
            Engine::RocksDb(b) => b.get(partition, key),
        }
    }
    fn get_many(
        &self,
        partition: &PartitionId,
        keys: &[&[u8]],
    ) -> BackendResult<Vec<Option<Vec<u8>>>> {
        match self {
            Engine::Fjall(b) => b.get_many(partition, keys),
            Engine::RocksDb(b) => b.get_many(partition, keys),
        }
    }
    fn scan_prefix(
        &self,
        partition: &PartitionId,
        prefix: &[u8],
        visit: &mut ScanVisitor<'_>,
    ) -> BackendResult<()> {
        match self {
            Engine::Fjall(b) => b.scan_prefix(partition, prefix, visit),
            Engine::RocksDb(b) => b.scan_prefix(partition, prefix, visit),
        }
    }
    fn scan_range(
        &self,
        partition: &PartitionId,
        start: &[u8],
        end: &[u8],
        visit: &mut ScanVisitor<'_>,
    ) -> BackendResult<()> {
        match self {
            Engine::Fjall(b) => b.scan_range(partition, start, end, visit),
            Engine::RocksDb(b) => b.scan_range(partition, start, end, visit),
        }
    }
    fn scan_range_rev(
        &self,
        partition: &PartitionId,
        start: &[u8],
        end: &[u8],
        visit: &mut ScanVisitor<'_>,
    ) -> BackendResult<()> {
        match self {
            Engine::Fjall(b) => b.scan_range_rev(partition, start, end, visit),
            Engine::RocksDb(b) => b.scan_range_rev(partition, start, end, visit),
        }
    }
    fn scan_full(&self, partition: &PartitionId, visit: &mut ScanVisitor<'_>) -> BackendResult<()> {
        match self {
            Engine::Fjall(b) => b.scan_full(partition, visit),
            Engine::RocksDb(b) => b.scan_full(partition, visit),
        }
    }
    fn count_prefix(&self, partition: &PartitionId, prefix: &[u8]) -> BackendResult<u64> {
        match self {
            Engine::Fjall(b) => b.count_prefix(partition, prefix),
            Engine::RocksDb(b) => b.count_prefix(partition, prefix),
        }
    }
    fn count_range(&self, partition: &PartitionId, start: &[u8], end: &[u8]) -> BackendResult<u64> {
        match self {
            Engine::Fjall(b) => b.count_range(partition, start, end),
            Engine::RocksDb(b) => b.count_range(partition, start, end),
        }
    }
    fn first_key(&self, partition: &PartitionId) -> BackendResult<Option<Vec<u8>>> {
        match self {
            Engine::Fjall(b) => b.first_key(partition),
            Engine::RocksDb(b) => b.first_key(partition),
        }
    }
    fn last_key(&self, partition: &PartitionId) -> BackendResult<Option<Vec<u8>>> {
        match self {
            Engine::Fjall(b) => b.last_key(partition),
            Engine::RocksDb(b) => b.last_key(partition),
        }
    }
    fn write_batch(&self) -> WriteBatchWrapper {
        match self {
            Engine::Fjall(b) => b.write_batch(),
            Engine::RocksDb(b) => b.write_batch(),
        }
    }
    fn commit_batch(&self, batch: WriteBatchWrapper) -> BackendResult<()> {
        match self {
            Engine::Fjall(b) => b.commit_batch(batch),
            Engine::RocksDb(b) => b.commit_batch(batch),
        }
    }
    fn is_poisoned(&self) -> bool {
        match self {
            Engine::Fjall(b) => b.is_poisoned(),
            Engine::RocksDb(b) => b.is_poisoned(),
        }
    }
    fn flush(&self) -> BackendResult<()> {
        match self {
            Engine::Fjall(b) => b.flush(),
            Engine::RocksDb(b) => b.flush(),
        }
    }
    fn engine_name(&self) -> &'static str {
        match self {
            Engine::Fjall(b) => b.engine_name(),
            Engine::RocksDb(b) => b.engine_name(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::ControlFlow;

    type KvPair = (Vec<u8>, Vec<u8>);

    fn test_engine_config() -> EngineConfig {
        EngineConfig {
            cache_size_bytes: 8 * 1024 * 1024,
            write_buffer_bytes: 8 * 1024 * 1024,
            memtable_bytes: 1024 * 1024,
            flush_workers: 1,
            compaction_workers: 1,
        }
    }

    fn for_each_engine(f: impl Fn(&Engine)) {
        for name in ["rocksdb", "fjall"] {
            let tmp = tempfile::TempDir::new().unwrap();
            let engine = Engine::open(name, tmp.path(), &test_engine_config()).unwrap();
            f(&engine);
        }
    }

    /// 20 pairs: `a:00`..`a:09` -> `va0`..`va9`, `b:00`..`b:09` -> `vb0`..`vb9`.
    fn seed(engine: &Engine) -> PartitionId {
        let p = engine.create_or_open_partition("scan_test").unwrap();
        let mut batch = engine.write_batch();
        for i in 0..10 {
            batch
                .insert(
                    &p,
                    format!("a:{i:02}").as_bytes(),
                    format!("va{i}").as_bytes(),
                )
                .unwrap();
            batch
                .insert(
                    &p,
                    format!("b:{i:02}").as_bytes(),
                    format!("vb{i}").as_bytes(),
                )
                .unwrap();
        }
        engine.commit_batch(batch).unwrap();
        p
    }

    fn collect_visited(
        scan: impl FnOnce(&mut ScanVisitor<'_>) -> BackendResult<()>,
    ) -> Vec<KvPair> {
        let mut got: Vec<KvPair> = Vec::new();
        scan(&mut |k, v| {
            got.push((k.to_vec(), v.to_vec()));
            ControlFlow::Continue(())
        })
        .unwrap();
        got
    }

    /// The four scan shapes against explicit expectations over the seeded
    /// fixture: prefix bounds, half-open range window, descending ==
    /// reversed ascending, full order.
    #[test]
    fn scan_shapes_visit_expected_windows() {
        let pair = |group: char, i: usize| {
            (
                format!("{group}:0{i}").into_bytes(),
                format!("v{group}{i}").into_bytes(),
            )
        };
        for_each_engine(|engine| {
            let p = seed(engine);

            let got = collect_visited(|v| engine.scan_prefix(&p, b"a:", v));
            let want: Vec<KvPair> = (0..10).map(|i| pair('a', i)).collect();
            assert_eq!(got, want);

            // Half-open window a:03..a:07.
            let got = collect_visited(|v| engine.scan_range(&p, b"a:03", b"a:07", v));
            let want: Vec<KvPair> = (3..7).map(|i| pair('a', i)).collect();
            assert_eq!(got, want);

            // Descending == the forward window reversed.
            let got = collect_visited(|v| engine.scan_range_rev(&p, b"a:03", b"a:07", v));
            let mut want = collect_visited(|v| engine.scan_range(&p, b"a:03", b"a:07", v));
            want.reverse();
            assert_eq!(got, want);
            assert_eq!(
                got.first().map(|(k, _)| k.as_slice()),
                Some(b"a:06".as_ref())
            );

            let got = collect_visited(|v| engine.scan_full(&p, v));
            let want: Vec<KvPair> = (0..10)
                .map(|i| pair('a', i))
                .chain((0..10).map(|i| pair('b', i)))
                .collect();
            assert_eq!(got, want);
        });
    }

    /// DT-6/DT-21 remainders: window edges beyond the data, and the count
    /// methods' documented empty-prefix / inverted-range semantics.
    #[test]
    fn scan_and_count_edges() {
        for_each_engine(|engine| {
            let p = seed(engine);

            // end beyond the last key: forward covers the tail, reverse
            // starts from the actual last key.
            let got = collect_visited(|v| engine.scan_range(&p, b"b:05", b"zzzz", v));
            assert_eq!(got.len(), 5, "b:05..b:09");
            let got = collect_visited(|v| engine.scan_range_rev(&p, b"a:05", b"zzzz", v));
            assert_eq!(got.len(), 15, "a:05..a:09 + all of b: descending");
            assert_eq!(
                got.first().map(|(k, _)| k.as_slice()),
                Some(b"b:09".as_ref())
            );
            assert_eq!(
                got.last().map(|(k, _)| k.as_slice()),
                Some(b"a:05".as_ref())
            );

            // Empty window between two real keys.
            let got = collect_visited(|v| engine.scan_range(&p, b"a:05a", b"a:06", v));
            assert!(got.is_empty());

            // Counts: empty prefix = whole partition; no-match prefix = 0;
            // half-open range; inverted range guarded to zero.
            assert_eq!(engine.count_prefix(&p, b"").unwrap(), 20);
            assert_eq!(engine.count_prefix(&p, b"a:").unwrap(), 10);
            assert_eq!(engine.count_prefix(&p, b"zz").unwrap(), 0);
            assert_eq!(engine.count_range(&p, b"a:03", b"a:07").unwrap(), 4);
            assert_eq!(engine.count_range(&p, b"a:07", b"a:03").unwrap(), 0);
            assert_eq!(engine.count_range(&p, b"a:05", b"a:05").unwrap(), 0);
        });
    }

    #[test]
    fn scan_break_stops_early() {
        for_each_engine(|engine| {
            let p = seed(engine);
            let mut seen = 0;
            engine
                .scan_full(&p, &mut |_, _| {
                    seen += 1;
                    if seen == 3 {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                })
                .unwrap();
            assert_eq!(seen, 3);
        });
    }

    #[test]
    fn scan_range_inverted_visits_nothing() {
        for_each_engine(|engine| {
            let p = seed(engine);
            let mut seen = 0;
            let mut count = |_: &[u8], _: &[u8]| {
                seen += 1;
                ControlFlow::Continue(())
            };
            engine.scan_range(&p, b"a:07", b"a:03", &mut count).unwrap();
            engine
                .scan_range_rev(&p, b"a:07", b"a:03", &mut count)
                .unwrap();
            engine.scan_range(&p, b"a:05", b"a:05", &mut count).unwrap();
            assert_eq!(seen, 0);
        });
    }

    /// The load-bearing H-P2 probe: visitors hydrate re-entrantly while the
    /// scan's snapshot is live. fjall 2.11.2's `read_tx()` is a lock-free
    /// snapshot capture and `TxPartitionHandle::get` bypasses tx machinery,
    /// so this cannot deadlock — if a future fjall changes that, this test
    /// hangs/fails instead of production.
    #[test]
    fn scan_visitor_reentrant_reads() {
        for_each_engine(|engine| {
            let p = seed(engine);
            let mut checked = 0;
            engine
                .scan_prefix(&p, b"a:", &mut |k, v| {
                    let got = engine.get(&p, k).unwrap().unwrap();
                    assert_eq!(got, v);
                    let many = engine.get_many(&p, &[k]).unwrap();
                    assert_eq!(many[0].as_deref(), Some(v));
                    let mut inner = 0;
                    engine
                        .scan_prefix(&p, b"b:", &mut |_, _| {
                            inner += 1;
                            ControlFlow::Break(())
                        })
                        .unwrap();
                    assert_eq!(inner, 1);
                    checked += 1;
                    ControlFlow::Continue(())
                })
                .unwrap();
            assert_eq!(checked, 10);
        });
    }

    #[test]
    fn get_many_hits_misses_preserve_order() {
        for_each_engine(|engine| {
            let p = seed(engine);
            let keys: Vec<&[u8]> = vec![b"a:00", b"missing", b"a:05", b"also-missing", b"b:09"];
            let got = engine.get_many(&p, &keys).unwrap();
            assert_eq!(got.len(), 5);
            assert_eq!(got[0].as_deref(), Some(b"va0".as_ref()));
            assert!(got[1].is_none());
            assert_eq!(got[2].as_deref(), Some(b"va5".as_ref()));
            assert!(got[3].is_none());
            assert_eq!(got[4].as_deref(), Some(b"vb9".as_ref()));

            let empty = engine.get_many(&p, &[]).unwrap();
            assert!(empty.is_empty());
        });
    }
}
