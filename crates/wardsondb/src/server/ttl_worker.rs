use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::time::{Duration, interval};
use tracing::error;

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
                match st.storage.run_ttl_cleanup(collection, config) {
                    Ok(deleted) => {
                        total_deleted += deleted;
                    }
                    Err(e) => {
                        error!(
                            collection = collection,
                            error = %e,
                            "TTL cleanup failed for collection"
                        );
                    }
                }
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
