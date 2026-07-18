//! Regression tests for the scan-accelerator `persist_to_disk` + hot-path
//! lock deadlock.
//!
//! Before the fix, `ScanAccelerator::persist_to_disk` held four nested
//! `parking_lot::RwLock` reader guards (`positions.pos_to_id`,
//! `positions.id_to_pos`, `columns`, per-column `value_bitmaps` +
//! `exists_bitmap`) across blocking `fs::write` calls. Under parking_lot's
//! writer-fair semantics, a queued writer from `on_insert` / `on_update` would
//! cause every subsequent reader in the function to block, producing a
//! process-wide deadlock under sustained ingest. These tests run on the
//! current code in about 3–5 s; against the pre-fix code they hang until the
//! `tokio::time::timeout` deadline fires.
//!
//! The tests operate on `ScanAccelerator` directly (no HTTP, no storage
//! engine) because that isolates the code under test — the deadlock lives
//! entirely above the backend layer.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::task::JoinSet;
use tokio::time::timeout;
use uuid::Uuid;

use wardsondb::engine::bitmap::{AcceleratorConfig, ScanAccelerator};

const CATEGORIES: &[&str] = &["firewall", "dhcp", "dns", "threat"];
const STATUSES: &[&str] = &["allow", "block", "alert"];
const SEVERITIES: &[&str] = &["low", "medium", "high", "critical"];
const TIERS: &[&str] = &["edge", "core", "lab"];

fn make_accelerator(fields: &[&str]) -> Arc<ScanAccelerator> {
    let config = AcceleratorConfig {
        bitmap_fields: fields.iter().map(|s| s.to_string()).collect(),
        max_cardinality: 1000,
        max_memory_bytes: 0,
    };
    let accel = Arc::new(ScanAccelerator::new(config));
    accel.set_ready(true);
    accel
}

fn synthetic_doc(seed: u64) -> (String, Value) {
    let id = Uuid::now_v7().to_string();
    let doc = json!({
        "_id": id.clone(),
        "category": CATEGORIES[(seed % CATEGORIES.len() as u64) as usize],
        "status": STATUSES[(seed % STATUSES.len() as u64) as usize],
        "severity": SEVERITIES[(seed % SEVERITIES.len() as u64) as usize],
        "tier": TIERS[(seed % TIERS.len() as u64) as usize],
    });
    (id, doc)
}

fn doc_for_id(id: &str, seed: u64) -> Value {
    json!({
        "_id": id,
        "category": CATEGORIES[(seed % CATEGORIES.len() as u64) as usize],
        "status": STATUSES[(seed % STATUSES.len() as u64) as usize],
        "severity": SEVERITIES[(seed % SEVERITIES.len() as u64) as usize],
        "tier": TIERS[(seed % TIERS.len() as u64) as usize],
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_concurrent_workload(
    accel: Arc<ScanAccelerator>,
    preloaded_ids: Arc<Vec<String>>,
    tempdir: Arc<TempDir>,
    insert_threads: usize,
    inserts_per_thread: usize,
    update_threads: usize,
    updates_per_thread: usize,
    stats_ticks: usize,
    persist_ticks: usize,
) {
    let mut set = JoinSet::new();

    for thread_idx in 0..insert_threads {
        let accel = accel.clone();
        set.spawn(async move {
            for i in 0..inserts_per_thread {
                let seed = (thread_idx * 1_000_000 + i) as u64;
                let (id, doc) = synthetic_doc(seed);
                accel.on_insert("c1", &id, &doc);
            }
        });
    }

    for thread_idx in 0..update_threads {
        let accel = accel.clone();
        let ids = preloaded_ids.clone();
        set.spawn(async move {
            for i in 0..updates_per_thread {
                let seed = (thread_idx * 1_000_000 + i) as u64;
                let id = &ids[(seed as usize) % ids.len()];
                let old_doc = doc_for_id(id, seed);
                let new_doc = doc_for_id(id, seed.wrapping_add(17));
                accel.on_update("c1", id, &old_doc, &new_doc);
            }
        });
    }

    {
        let accel = accel.clone();
        set.spawn(async move {
            for _ in 0..stats_ticks {
                let _ = accel.stats();
                tokio::task::yield_now().await;
            }
        });
    }

    {
        let accel = accel.clone();
        let tempdir = tempdir.clone();
        set.spawn(async move {
            for _ in 0..persist_ticks {
                let _ = accel.persist_to_disk(tempdir.path(), "_all");
                tokio::task::yield_now().await;
            }
        });
    }

    while set.join_next().await.is_some() {}
}

/// Main acceptance gate: 10k preload, 8k concurrent inserts, 4k concurrent
/// updates, 300 stats reads, 20 persist cycles — all within 30 s.
///
/// Before the fix, this hangs indefinitely on `parking_lot::lock_shared_slow`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_bitmap_no_deadlock_under_concurrent_load() {
    let accel = make_accelerator(&["category", "status", "severity", "tier"]);
    let tempdir = Arc::new(TempDir::new().unwrap());

    let mut preloaded_ids = Vec::with_capacity(10_000);
    for i in 0..10_000u64 {
        let (id, doc) = synthetic_doc(i);
        accel.on_insert("c1", &id, &doc);
        preloaded_ids.push(id);
    }
    let preloaded = Arc::new(preloaded_ids);

    let result = timeout(
        Duration::from_secs(30),
        run_concurrent_workload(
            accel.clone(),
            preloaded,
            tempdir.clone(),
            4,
            2_000,
            2,
            2_000,
            300,
            20,
        ),
    )
    .await;

    assert!(
        result.is_ok(),
        "workload did not complete in 30s — the scan-accelerator likely \
         deadlocked on parking_lot guards held across fs::write (pre-fix behavior)"
    );

    let stats = accel.stats();
    assert!(
        stats.total_positions >= 18_000,
        "expected >=18000 positions after 10k preload + 8k new inserts, got {}",
        stats.total_positions
    );
}

/// Regression: `stats()` used to call `total_memory_bytes()` while already
/// holding `columns.read()`, re-acquiring the same lock. parking_lot readers
/// queued behind a waiting writer are not reentrant, so `stats()` racing any
/// `columns` writer (`configure_fields`, auto-detect completion, `clear`,
/// `load_from_disk`) deadlocked the process. The workload tests above never
/// caught this: with pre-configured fields the profiler is already done, so
/// nothing on their insert/update path ever takes the `columns` write lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_stats_under_concurrent_columns_writer() {
    let accel = make_accelerator(&["category", "status"]);

    for i in 0..1_000u64 {
        let (id, doc) = synthetic_doc(i);
        accel.on_insert("c1", &id, &doc);
    }

    let mut set = JoinSet::new();
    {
        let accel = accel.clone();
        set.spawn(async move {
            for _ in 0..2_000 {
                let _ = accel.stats();
                tokio::task::yield_now().await;
            }
        });
    }
    {
        let accel = accel.clone();
        set.spawn(async move {
            for _ in 0..2_000 {
                accel.configure_fields(vec!["category".into(), "status".into()]);
                tokio::task::yield_now().await;
            }
        });
    }

    let result = timeout(Duration::from_secs(15), async {
        while set.join_next().await.is_some() {}
    })
    .await;

    assert!(
        result.is_ok(),
        "stats() under a concurrent columns writer deadlocked (recursive \
         columns.read() inside total_memory_bytes — pre-fix behavior)"
    );

    // Value sanity: the single-pass memory total must equal the sum of the
    // per-column stats plus the position map, and positions must be intact.
    let stats = accel.stats();
    let column_sum: usize = stats.columns.iter().map(|c| c.memory_bytes).sum();
    assert_eq!(
        stats.memory_bytes,
        column_sum + accel.positions.memory_bytes()
    );
    assert_eq!(stats.total_positions, 1_000);
}

/// Smaller smoke variant — catches regressions in ~2 s on CI.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_bitmap_no_deadlock_smoke() {
    let accel = make_accelerator(&["category", "status"]);
    let tempdir = Arc::new(TempDir::new().unwrap());

    let mut preloaded_ids = Vec::with_capacity(2_000);
    for i in 0..2_000u64 {
        let (id, doc) = synthetic_doc(i);
        accel.on_insert("c1", &id, &doc);
        preloaded_ids.push(id);
    }

    let result = timeout(
        Duration::from_secs(15),
        run_concurrent_workload(
            accel,
            Arc::new(preloaded_ids),
            tempdir,
            2,
            500,
            1,
            500,
            50,
            5,
        ),
    )
    .await;

    assert!(result.is_ok(), "smoke test deadlocked or timed out in 15s");
}
