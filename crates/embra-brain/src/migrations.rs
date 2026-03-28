//! Schema migration stub.
//!
//! Will be replaced with Phase 0's migration framework (v0-v4).

use crate::db::client::WardsonClient;
use anyhow::Result;

pub async fn run_migrations(_db: &WardsonClient) -> Result<()> {
    // TODO: Wire to Phase 0 migrations (v0-v4)
    tracing::info!("Migrations: stub (no-op)");
    Ok(())
}
