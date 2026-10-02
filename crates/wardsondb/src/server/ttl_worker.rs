use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::time::{Duration, interval};
use tracing::{error, warn};

use crate::engine::storage::Storage;
use crate::engine::ttl::TtlConfig;
use crate::error::AppError;
use crate::server::AppState;


/// Atomic timestamp of the last TTL cleanup run (unix seconds).
pub static LAST_TTL_RUN: AtomicU64 = AtomicU64::new(0);

/// Run the TTL cleanup loop. Intended to be called from a tokio::spawn.
pub async fn run_ttl_loop(state: Arc<AppState>, interval_secs: u64) {
    let mut tick = interval(Duration::from_secs(interval_secs));
    // Skip the first immediate tick
    tick.tick().await;

    loop {
        tick.tick().await;

        // Config load is a _meta prefix scan and each cleanup is a full
        // collection scan (delete_by_query) — all blocking KV work, so the
        // whole tick body runs on the blocking pool, mirroring the bitmap
        // persist task in main.rs. Only metrics/timestamp bookkeeping stays
        // on the async runtime.
        let st = state.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let configs = match st.storage.get_all_ttl_configs() {
                Ok(c) => c,
                Err(e) => {
                    error!(error = %e, "Failed to load TTL configs");
                    return 0u64;
                }
            };

            let mut total_deleted = 0u64;
            for (collection, config) in &configs {
                total_deleted += run_one_policy(&st.storage, collection, config);
            }
            total_deleted

        })
        .await;

        let total_deleted = match outcome {
            Ok(n) => n,
            Err(e) => {
                error!(error = %e, "TTL cleanup task panicked");
                0
            }
        };

        if total_deleted > 0 {
            state
                .metrics
                .lifetime_deletes
                .fetch_add(total_deleted, Ordering::Relaxed);
        }

        LAST_TTL_RUN.store(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            Ordering::Relaxed,
        );
    }
}

/// One policy, one tick: the number of documents removed.
///
/// A policy whose collection no longer exists is discarded and logged
/// once. Such a policy is left behind by a drop on a build before the drop
/// removed it, and it used to be logged as an error on every tick, forever.
pub fn run_one_policy(storage: &Storage, collection: &str, config: &TtlConfig) -> u64 {
    match storage.run_ttl_cleanup(collection, config) {
        Ok(deleted) => deleted,
        Err(AppError::CollectionNotFound(_)) => {
            match storage.discard_ttl(collection) {
                Ok(()) => warn!(
                    collection = collection,
                    "TTL policy dropped: its collection no longer exists"
                ),
                Err(e) => error!(
                    collection = collection,
                    error = %e,
                    "TTL policy of a missing collection could not be dropped"
                ),
            }
            0
        }
        Err(e) => {
            error!(
                collection = collection,
                error = %e,
                "TTL cleanup failed for collection"
            );
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A policy left behind by a drop on an older build is discarded on the
    /// first tick that meets it, instead of failing on every tick.
    #[test]
    fn a_policy_whose_collection_is_gone_is_discarded_on_the_first_tick() {
        let tmp = TempDir::new().unwrap();
        let storage = Storage::open(tmp.path()).unwrap();
        let config = TtlConfig {
            retention_days: 1,
            field: "_created_at".to_string(),
            enabled: true,
        };
        let mut batch = storage.write_batch();
        batch
            .insert(&storage.meta, b"ttl:ghost", &serde_json::to_vec(&config).unwrap())
            .unwrap();
        storage.commit_batch(batch).unwrap();
        assert_eq!(storage.get_all_ttl_configs().unwrap().len(), 1);

        assert_eq!(run_one_policy(&storage, "ghost", &config), 0);
        assert!(
            storage.get_all_ttl_configs().unwrap().is_empty(),
            "the stale policy is gone"
        );
        // The next tick has nothing to discard and nothing to fail on.
        assert_eq!(run_one_policy(&storage, "ghost", &config), 0);
    }

    /// A live collection keeps its policy through a tick that removes
    /// nothing.
    #[test]
    fn a_live_collection_keeps_its_policy() {
        let tmp = TempDir::new().unwrap();
        let storage = Storage::open(tmp.path()).unwrap();
        storage.create_collection("events").unwrap();
        let config = storage.set_ttl("events", 1, "_created_at").unwrap();
        assert_eq!(run_one_policy(&storage, "events", &config), 0);
        assert_eq!(storage.get_all_ttl_configs().unwrap().len(), 1);
    }
}
