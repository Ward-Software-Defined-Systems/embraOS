use anyhow::Result;
use chrono::Utc;
use tracing::{error, info, warn};

use crate::db::{WardsonDbClient, WardsonDbError};

const CURRENT_SCHEMA_VERSION: u32 = 8;

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

    if current_version < 2 {
        // v2: consolidation log collection
        run_v2_consolidation(db).await?;
        set_schema_version(db, 2).await?;
    }

    if current_version < 3 {
        // v3: migrate singleton documents to well-known _id values
        run_v3_singleton_ids(db).await?;
        set_schema_version(db, 3).await?;
    }

    if current_version < 4 {
        // v4: TTL policies for auto-cleanup
        run_v4_ttl_policies(db).await?;
        set_schema_version(db, 4).await?;
    }

    if current_version < 5 {
        // v5: knowledge graph — new collections, indexes, tag array migration, KG config
        run_v5_knowledge_graph(db).await?;
        set_schema_version(db, 5).await?;
    }

    if current_version < 6 {
        // v6: expression panel (EXPR-01) — singleton ui.expression
        run_v6_expression_panel(db).await?;
        set_schema_version(db, 6).await?;
    }

    if current_version < 7 {
        // v7 (NATIVE-TOOLS-01): migrate crons to {command_name, command_args}
        // and stamp format_version: 1 on pre-migration sessions (legacy,
        // read-only post-v7). New sessions created post-migration write
        // format_version: 2.
        run_v7_native_tools(db).await?;
        set_schema_version(db, 7).await?;
    }

    if current_version < 8 {
        // v8: reset tools.turn_trace after the Embra_Debug #44 fix. Pre-fix
        // docs were persisted with `turn_index = history.len()` (message
        // count, incrementing by 2 per logical turn). Post-fix docs use
        // `history.len() / 2` (logical turn count). Mixing both schemes in a
        // single collection means `turn_trace back=1` can return documents
        // from a wrong turn (even-value hit under the new math that actually
        // belongs to a different turn) or empty (odd-value miss). Since
        // these are diagnostic logs with no downstream referents, a one-shot
        // clear is simpler than trying to re-label.
        run_v8_turn_trace_reset(db).await?;
        set_schema_version(db, 8).await?;
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

    let cutoff = (Utc::now() - chrono::Duration::hours(24)).to_rfc3339();
    let filter = serde_json::json!({
        "$and": [
            {"fired": false},
            {"created_at": {"$lt": cutoff}}
        ]
    });

    let deleted = db
        .delete_by_query("reminders", &filter)
        .await
        .unwrap_or(0);

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

/// Migration v2: Create consolidation log collection for session/memory consolidation tools.
async fn run_v2_consolidation(db: &WardsonDbClient) -> Result<()> {
    info!("Running migration v2: consolidation log");

    if !db
        .collection_exists("system.consolidation_log")
        .await
        .unwrap_or(false)
    {
        info!("Creating collection: system.consolidation_log");
        let _ = db.create_collection("system.consolidation_log").await;
    }

    info!("Migration v2: consolidation log collection ensured");
    Ok(())
}

/// Migration v3: Migrate singleton documents to well-known `_id` values.
/// Enables direct GET by ID instead of query-then-take-first.
///
/// Handles crash recovery (State D: two docs after partial migration) and
/// performs soul integrity verification after copying `soul.invariant`.
async fn run_v3_singleton_ids(db: &WardsonDbClient) -> Result<()> {
    info!("Running migration v3: singleton document IDs");

    // Process soul.invariant last — most critical document
    let singletons: &[(&str, &str)] = &[
        ("config.system", "config"),
        ("memory.identity", "identity"),
        ("memory.user", "user"),
        ("soul.invariant", "soul"),
    ];

    for (collection, well_known_id) in singletons {
        if !db.collection_exists(collection).await.unwrap_or(false) {
            info!("Migration v3: {} does not exist, skipping", collection);
            continue;
        }

        // Step 1: Query all documents (limit 10 — should be 0, 1, or 2)
        let docs = db
            .query(collection, &serde_json::json!({"limit": 10}))
            .await
            .unwrap_or_default();

        // Step 2: Classify the state
        let has_wellknown = docs.iter().any(|d| {
            d.get("_id")
                .or_else(|| d.get("id"))
                .and_then(|v| v.as_str())
                == Some(well_known_id)
        });
        let uuid_docs: Vec<&serde_json::Value> = docs
            .iter()
            .filter(|d| {
                d.get("_id")
                    .or_else(|| d.get("id"))
                    .and_then(|v| v.as_str())
                    != Some(well_known_id)
            })
            .collect();

        match (docs.len(), has_wellknown, uuid_docs.len()) {
            // State A: empty collection
            (0, _, _) => {
                info!("Migration v3: {} is empty, skipping", collection);
            }
            // State B: already migrated
            (1, true, 0) => {
                info!(
                    "Migration v3: {} already has well-known _id, skipping",
                    collection
                );
            }
            // State C: normal migration needed
            (1, false, 1) => {
                let original = uuid_docs[0];
                let original_id = original
                    .get("_id")
                    .or_else(|| original.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");

                // Step 3: Copy with well-known _id, stripping system fields
                let mut new_doc = original.clone();
                if let Some(obj) = new_doc.as_object_mut() {
                    for key in &[
                        "_id",
                        "_rev",
                        "_created_at",
                        "_updated_at",
                        "_received_at",
                        "id",
                    ] {
                        obj.remove(*key);
                    }
                    obj.insert("_id".into(), serde_json::json!(well_known_id));
                }

                let insert_ok = match db.write(collection, &new_doc).await {
                    Ok(_) => true,
                    Err(e) => {
                        // Check if it's a 409 conflict (well-known doc created by another process)
                        if let Some(db_err) = e.downcast_ref::<WardsonDbError>() {
                            if db_err.is_conflict() {
                                info!(
                                    "Migration v3: {} well-known doc already exists (409), proceeding to cleanup",
                                    collection
                                );
                                true
                            } else {
                                error!(
                                    "Migration v3: failed to insert {}/{}: {}",
                                    collection, well_known_id, e
                                );
                                false
                            }
                        } else {
                            error!(
                                "Migration v3: failed to insert {}/{}: {}",
                                collection, well_known_id, e
                            );
                            false
                        }
                    }
                };

                if !insert_ok {
                    continue; // Leave original intact
                }

                // Step 4: Verify the copy
                let copied = match db.read(collection, well_known_id).await {
                    Ok(doc) => doc,
                    Err(e) => {
                        error!(
                            "Migration v3: failed to read back {}/{} after insert: {}",
                            collection, well_known_id, e
                        );
                        continue; // Leave original intact
                    }
                };

                // Soul-specific integrity check
                if *collection == "soul.invariant" {
                    if let (Some(soul_obj), Some(stored_hash)) = (
                        copied.get("soul"),
                        copied.get("sha256").and_then(|v| v.as_str()),
                    ) {
                        match crate::learning::compute_soul_hash(soul_obj) {
                            Ok(computed) if computed == stored_hash => {
                                info!("Migration v3: soul integrity check passed");
                            }
                            Ok(computed) => {
                                error!(
                                    "Migration v3: SOUL INTEGRITY CHECK FAILED after copy. \
                                     Computed={}, Stored={}. Rolling back.",
                                    computed, stored_hash
                                );
                                let _ = db.delete(collection, well_known_id).await;
                                continue; // Leave original intact
                            }
                            Err(e) => {
                                error!(
                                    "Migration v3: soul hash computation failed: {}. Rolling back.",
                                    e
                                );
                                let _ = db.delete(collection, well_known_id).await;
                                continue;
                            }
                        }
                    } else {
                        error!(
                            "Migration v3: soul document missing 'soul' or 'sha256' field. Rolling back."
                        );
                        let _ = db.delete(collection, well_known_id).await;
                        continue;
                    }
                }

                // Step 5: Delete old UUID document
                match db.delete(collection, original_id).await {
                    Ok(_) => {
                        info!(
                            "Migration v3: {} migrated _id={} → _id={}",
                            collection, original_id, well_known_id
                        );
                    }
                    Err(e) => {
                        warn!(
                            "Migration v3: {} copied but failed to delete old doc {}: {}. \
                             Will retry on next startup (State D recovery).",
                            collection, original_id, e
                        );
                    }
                }
            }
            // State D: crash recovery — well-known exists + UUID leftover
            (2, true, 1) => {
                let stale_id = uuid_docs[0]
                    .get("_id")
                    .or_else(|| uuid_docs[0].get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                match db.delete(collection, stale_id).await {
                    Ok(_) => {
                        info!(
                            "Migration v3: {} crash recovery — deleted stale doc _id={}",
                            collection, stale_id
                        );
                    }
                    Err(e) => {
                        warn!(
                            "Migration v3: {} crash recovery delete failed for {}: {}",
                            collection, stale_id, e
                        );
                    }
                }
            }
            // State E/F: unexpected document count
            _ => {
                error!(
                    "Migration v3: {} has unexpected document count={} \
                     (well_known={}, uuid={}). Skipping — manual inspection required.",
                    collection,
                    docs.len(),
                    has_wellknown,
                    uuid_docs.len()
                );
            }
        }
    }

    info!("Migration v3: singleton IDs migration complete");
    Ok(())
}

/// Migration v4: Set TTL policies for auto-cleanup of ephemeral collections.
async fn run_v4_ttl_policies(db: &WardsonDbClient) -> Result<()> {
    info!("Running migration v4: TTL policies");

    // 7-day retention on reminders (fired reminders cleaned up automatically)
    if db.collection_exists("reminders").await.unwrap_or(false) {
        match db.set_ttl("reminders", 7, "created_at").await {
            Ok(_) => info!("Migration v4: set 7-day TTL on reminders"),
            Err(e) => info!("Migration v4: TTL on reminders skipped: {}", e),
        }
    }

    // 90-day retention on consolidation log
    if db
        .collection_exists("system.consolidation_log")
        .await
        .unwrap_or(false)
    {
        match db
            .set_ttl("system.consolidation_log", 90, "timestamp")
            .await
        {
            Ok(_) => info!("Migration v4: set 90-day TTL on system.consolidation_log"),
            Err(e) => info!("Migration v4: TTL on consolidation_log skipped: {}", e),
        }
    }

    info!("Migration v4: TTL policies complete");
    Ok(())
}

/// Migration v5: Knowledge graph setup.
/// - Creates 3 new collections: memory.semantic, memory.procedural, memory.edges
/// - Creates 7 indexes
/// - Migrates memory.entries `tags` field from comma-separated strings to JSON arrays
/// - PATCHes config.system with KG configuration fields
async fn run_v5_knowledge_graph(db: &WardsonDbClient) -> Result<()> {
    info!("Running migration v5: knowledge graph");

    // Step 1: Create new collections (idempotent)
    for collection in &["memory.semantic", "memory.procedural", "memory.edges"] {
        if !db.collection_exists(collection).await.unwrap_or(false) {
            info!("Creating collection: {}", collection);
            if let Err(e) = db.create_collection(collection).await {
                // 409 COLLECTION_EXISTS is idempotent success
                if !is_conflict_err(&e) {
                    error!("Migration v5: failed to create collection {}: {}", collection, e);
                    return Err(e);
                }
            }
        }
    }

    // Step 2: Create indexes (idempotent — create_index treats 409 as success)
    // (collection, index_name, body)
    let indexes: &[(&str, &str, serde_json::Value)] = &[
        (
            "memory.edges",
            "idx_edge_source",
            serde_json::json!({"name": "idx_edge_source", "fields": ["source_id", "edge_type"]}),
        ),
        (
            "memory.edges",
            "idx_edge_target",
            serde_json::json!({"name": "idx_edge_target", "field": "target_id"}),
        ),
        (
            "memory.edges",
            "idx_edge_source_target",
            serde_json::json!({"name": "idx_edge_source_target", "fields": ["source_id", "target_id"]}),
        ),
        (
            "memory.semantic",
            "idx_semantic_category",
            serde_json::json!({"name": "idx_semantic_category", "field": "category"}),
        ),
        (
            "memory.semantic",
            "idx_semantic_source",
            serde_json::json!({"name": "idx_semantic_source", "field": "source_entry_id"}),
        ),
        (
            "memory.procedural",
            "idx_procedural_source",
            serde_json::json!({"name": "idx_procedural_source", "field": "source_entry_id"}),
        ),
        (
            "memory.entries",
            "idx_entries_session",
            serde_json::json!({"name": "idx_entries_session", "field": "session"}),
        ),
    ];

    for (collection, idx_name, body) in indexes {
        match db.create_index(collection, body).await {
            Ok(_) => info!("Migration v5: index {} on {} ensured", idx_name, collection),
            Err(e) => warn!("Migration v5: index {} on {} failed: {}", idx_name, collection, e),
        }
    }

    // Step 3: Tag array migration — convert comma-separated strings to JSON arrays
    if db.collection_exists("memory.entries").await.unwrap_or(false) {
        let all_entries = db
            .query(
                "memory.entries",
                &serde_json::json!({ "filter": {}, "limit": 10000 }),
            )
            .await
            .unwrap_or_default();

        let mut migrated = 0usize;
        let mut skipped = 0usize;
        for doc in all_entries {
            let id = match doc.get("_id").and_then(|v| v.as_str()) {
                Some(id) => id.to_string(),
                None => continue,
            };

            let tags_val = doc.get("tags");
            let new_tags: Vec<String> = match tags_val {
                Some(v) if v.is_array() => {
                    skipped += 1;
                    continue;
                }
                Some(v) if v.is_string() => v
                    .as_str()
                    .unwrap_or("")
                    .split(", ")
                    .map(|t| t.trim().trim_start_matches('#').trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect(),
                _ => Vec::new(),
            };

            let patch = serde_json::json!({ "tags": new_tags });
            match db.patch_document("memory.entries", &id, &patch).await {
                Ok(_) => migrated += 1,
                Err(e) => warn!("Migration v5: failed to patch tags on {}: {}", id, e),
            }
        }
        info!(
            "Migration v5: tag array migration — {} migrated, {} already array",
            migrated, skipped
        );
    }

    // Step 4: PATCH config.system with KG configuration fields
    let kg_config_patch = serde_json::json!({
        "kg_temporal_window_secs": 1800,
        "kg_max_traversal_depth": 3,
        "kg_traversal_depth_ceiling": 5,
        "kg_edge_candidate_limit": 50,
    });
    match db
        .patch_document("config.system", "config", &kg_config_patch)
        .await
    {
        Ok(_) => info!("Migration v5: KG config fields added to config.system"),
        Err(e) => warn!("Migration v5: failed to PATCH config.system: {}", e),
    }

    info!("Migration v5: knowledge graph complete");
    Ok(())
}

fn is_conflict_err(e: &anyhow::Error) -> bool {
    e.downcast_ref::<WardsonDbError>()
        .map(|w| w.is_conflict())
        .unwrap_or(false)
}

/// Migration v6: Expression panel (EXPR-01).
/// - Creates `ui` collection (idempotent).
/// - Seeds singleton `ui.expression` with `content = ""`, `version = 0` if absent.
async fn run_v6_expression_panel(db: &WardsonDbClient) -> Result<()> {
    info!("Running migration v6: expression panel");

    if !db.collection_exists("ui").await.unwrap_or(false) {
        info!("Creating collection: ui");
        if let Err(e) = db.create_collection("ui").await {
            if !is_conflict_err(&e) {
                error!("Migration v6: failed to create collection ui: {}", e);
                return Err(e);
            }
        }
    }

    // Seed singleton — only if it does not already exist.
    let existing = db.read("ui", "expression").await.ok();
    if existing.is_none() {
        let seed = serde_json::json!({
            "_id": "expression",
            "content": "",
            "version": 0u64,
            "updated_at": Utc::now().to_rfc3339(),
        });
        // EXPR-01 diagnostic: log the migration write under the same target
        // the express tool uses, so journalctl --grep 'ui.expression' captures
        // every authorized writer.
        info!(
            target: "ui.expression",
            actor = "migration_v6",
            new_bytes = 0,
            empty = true,
            "seeding ui.expression singleton"
        );
        match db.write("ui", &seed).await {
            Ok(_) => info!("Migration v6: seeded ui.expression singleton"),
            Err(e) if is_conflict_err(&e) => {
                info!("Migration v6: ui.expression already exists (409)");
            }
            Err(e) => {
                error!("Migration v6: failed to seed ui.expression: {}", e);
                return Err(e);
            }
        }
    }

    info!("Migration v6: expression panel complete");
    Ok(())
}

/// Migration v7 (NATIVE-TOOLS-01): two steps.
///
/// 1. `crons` collection: every doc with a legacy `command` string field
///    gets `command_name` + `command_args` fields. Naked tool names
///    ("system_status") map to `command_args: {}`. Legacy strings with
///    args ("recall alerts") are parsed as name + space-joined rest and
///    stored as `command_args: {"_legacy_raw": "<rest>"}` for the operator
///    to re-schedule after inspecting. Already-migrated docs
///    (command_name present) are skipped.
///
/// 2. `sessions.*.history` collections: every doc without a
///    `format_version` field gets stamped with `format_version: 1`. This
///    is the legacy marker — post-migration SessionManager rejects writes
///    to any session whose format_version is < 2.
async fn run_v7_native_tools(db: &WardsonDbClient) -> Result<()> {
    info!("Running migration v7: native tool-use");

    // Step 1: crons schema migration.
    if db.collection_exists("crons").await.unwrap_or(false) {
        let docs = db
            .query("crons", &serde_json::json!({}))
            .await
            .unwrap_or_default();
        let mut migrated = 0usize;
        let mut legacy_raw = 0usize;
        for doc in docs {
            if doc.get("command_name").is_some() {
                continue; // already migrated
            }
            let Some(id) = doc.get("_id").and_then(|v| v.as_str()) else {
                warn!("Migration v7: cron doc without _id, skipping");
                continue;
            };
            let command = doc
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let (name, args) = match command.split_once(' ') {
                None => (command.clone(), serde_json::json!({})),
                Some((n, rest)) => (
                    n.to_string(),
                    serde_json::json!({ "_legacy_raw": rest.trim() }),
                ),
            };
            if args.get("_legacy_raw").is_some() {
                legacy_raw += 1;
            }
            let patch = serde_json::json!({
                "command_name": name,
                "command_args": args,
            });
            if let Err(e) = db.patch_document("crons", id, &patch).await {
                warn!("Migration v7: failed to patch cron {}: {}", id, e);
                continue;
            }
            migrated += 1;
        }
        info!(
            "Migration v7: crons migrated (patched={}, legacy_raw={})",
            migrated, legacy_raw
        );
    }

    // Step 2: session format_version stamping.
    let collections = db.list_collections().await.unwrap_or_default();
    let mut stamped = 0usize;
    for col in collections {
        if !col.starts_with("sessions.") || !col.ends_with(".history") {
            continue;
        }
        let docs = db.query(&col, &serde_json::json!({})).await.unwrap_or_default();
        for doc in docs {
            if doc.get("format_version").is_some() {
                continue;
            }
            let Some(id) = doc.get("_id").and_then(|v| v.as_str()) else {
                continue;
            };
            let patch = serde_json::json!({ "format_version": 1 });
            if let Err(e) = db.patch_document(&col, id, &patch).await {
                warn!("Migration v7: failed to stamp {}/{}: {}", col, id, e);
                continue;
            }
            stamped += 1;
        }
    }
    info!("Migration v7: stamped {} legacy session(s) with format_version=1", stamped);

    info!("Migration v7: native tool-use complete");
    Ok(())
}

/// Migration v8: clear `tools.turn_trace` after the Embra_Debug #44 fix.
///
/// Pre-fix docs were keyed by `turn_index = history.len()` (message count,
/// step 2/turn). Post-fix docs use `history.len() / 2` (logical turn count,
/// step 1/turn). A session that spans the upgrade ends up with both schemes
/// mingled in the same collection — `turn_trace back=N` then lands on
/// documents from the wrong logical turn (an even-index hit under the new
/// math that was actually written for a different turn under the old math)
/// or on nothing at all (odd-index miss). Both failure modes look like
/// "turn_trace is broken" to the operator.
///
/// These docs are purely diagnostic — no downstream referents, no history
/// replay depends on them — so a one-shot clear is the cheapest resolution.
/// New traces written after this migration are all logical-turn-indexed and
/// agree with the read path.
async fn run_v8_turn_trace_reset(db: &WardsonDbClient) -> Result<()> {
    info!("Running migration v8: tools.turn_trace reset (Embra_Debug #44 follow-up)");

    if !db.collection_exists("tools.turn_trace").await.unwrap_or(false) {
        info!("Migration v8: tools.turn_trace does not exist — nothing to reset");
        return Ok(());
    }

    match db
        .delete_by_query("tools.turn_trace", &serde_json::json!({}))
        .await
    {
        Ok(n) => {
            info!(
                "Migration v8: cleared {} pre-fix turn_trace document(s)",
                n
            );
        }
        Err(e) => {
            warn!(
                "Migration v8: delete_by_query on tools.turn_trace failed: {} \
                 (continuing — stale docs will cause wrong-turn hits until resolved manually)",
                e
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod v7_tests {
    use serde_json::{json, Value};

    fn split_legacy_command(command: &str) -> (String, Value) {
        match command.split_once(' ') {
            None => (command.to_string(), json!({})),
            Some((n, rest)) => (
                n.to_string(),
                json!({ "_legacy_raw": rest.trim() }),
            ),
        }
    }

    #[test]
    fn naked_command_no_legacy_args() {
        let (name, args) = split_legacy_command("system_status");
        assert_eq!(name, "system_status");
        assert_eq!(args, json!({}));
    }

    #[test]
    fn command_with_arg_preserves_raw() {
        let (name, args) = split_legacy_command("recall alerts");
        assert_eq!(name, "recall");
        assert_eq!(args["_legacy_raw"], "alerts");
    }

    #[test]
    fn command_with_multiple_args_preserves_trailing() {
        let (name, args) = split_legacy_command("port_scan localhost web");
        assert_eq!(name, "port_scan");
        assert_eq!(args["_legacy_raw"], "localhost web");
    }
}
