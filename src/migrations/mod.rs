use anyhow::Result;
use chrono::Utc;
use tracing::info;

use crate::db::WardsonDbClient;

const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Run all pending migrations. Each migration is idempotent.
pub async fn run_migrations(db: &WardsonDbClient) -> Result<()> {
    // Ensure system.migrations collection exists
    if !db.collection_exists("system.migrations").await.unwrap_or(false) {
        db.create_collection("system.migrations").await?;
    }

    let current_version = get_schema_version(db).await;
    info!("Current schema version: {}, target: {}", current_version, CURRENT_SCHEMA_VERSION);

    if current_version < 0 {
        // v0: cleanup phantom data from BUG-001
        run_v0_cleanup(db).await?;
        set_schema_version(db, 0).await?;
    }

    if current_version < 1 {
        // v1: ensure baseline collections exist
        run_v1_baseline(db).await?;
        set_schema_version(db, 1).await?;
    }

    info!("Migrations complete. Schema version: {}", CURRENT_SCHEMA_VERSION);
    Ok(())
}

async fn get_schema_version(db: &WardsonDbClient) -> i32 {
    let docs = db
        .query("system.migrations", &serde_json::json!({}))
        .await
        .unwrap_or_default();

    docs.iter()
        .filter_map(|doc| doc.get("schema_version").and_then(|v| v.as_i64()))
        .max()
        .map(|v| v as i32)
        .unwrap_or(-1)
}

async fn set_schema_version(db: &WardsonDbClient, version: u32) -> Result<()> {
    let doc = serde_json::json!({
        "schema_version": version,
        "applied_at": Utc::now().to_rfc3339(),
        "embraos_version": env!("CARGO_PKG_VERSION"),
    });
    db.write("system.migrations", &doc).await?;
    Ok(())
}

/// Migration v0: Clean up BUG-001 phantom data.
/// Deletes all unfired reminders older than 24 hours (stale phantoms from runaway loop).
async fn run_v0_cleanup(db: &WardsonDbClient) -> Result<()> {
    info!("Running migration v0: BUG-001 phantom data cleanup");

    if !db.collection_exists("reminders").await.unwrap_or(false) {
        info!("No reminders collection — skipping v0 cleanup");
        return Ok(());
    }

    let reminders = db
        .query("reminders", &serde_json::json!({}))
        .await
        .unwrap_or_default();

    let cutoff = (Utc::now() - chrono::Duration::hours(24)).to_rfc3339();
    let mut deleted = 0u32;

    for doc in &reminders {
        let fired = doc.get("fired").and_then(|v| v.as_bool()).unwrap_or(false);
        if fired {
            continue;
        }

        let created = doc
            .get("created_at")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if !created.is_empty() && created < cutoff.as_str() {
            if let Some(id) = doc.get("_id").or(doc.get("id")).and_then(|v| v.as_str()) {
                let _ = db.delete("reminders", id).await;
                deleted += 1;
            }
        }
    }

    info!("Migration v0: deleted {} stale phantom reminders", deleted);
    Ok(())
}

/// Migration v1: Ensure all expected collections exist.
async fn run_v1_baseline(db: &WardsonDbClient) -> Result<()> {
    info!("Running migration v1: schema baseline");

    let required_collections = [
        "memory.entries",
        "reminders",
        "knowledge.definitions",
        "drafts",
        "system.migrations",
        "plans",
        "tasks",
    ];

    for collection in &required_collections {
        if !db.collection_exists(collection).await.unwrap_or(false) {
            info!("Creating collection: {}", collection);
            let _ = db.create_collection(collection).await;
        }
    }

    info!("Migration v1: baseline collections ensured");
    Ok(())
}
