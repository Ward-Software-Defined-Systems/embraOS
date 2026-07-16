use anyhow::Result;
use chrono::Utc;
use tracing::{error, info, warn};

use crate::db::{WardsonDbClient, WardsonDbError};

const CURRENT_SCHEMA_VERSION: u32 = 12;

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

    if current_version < 9 {
        // v9 (GEMINI-PROVIDER-01): create the provider.gemini_cache
        // collection and stamp pluggable-provider defaults on
        // existing config.system + sessions.<name>.meta docs so the
        // post-v9 code paths that read api_provider / meta.provider
        // see consistent values everywhere.
        run_v9_pluggable_provider(db).await?;
        set_schema_version(db, 9).await?;
    }

    if current_version < 10 {
        // v10 (Sprint 4 D2): backfill per-provider API key fields on
        // config.system from the legacy `api_key` + `api_provider`
        // pair. Pre-D2, only one key was stored at a time; switching
        // providers required re-running the wizard. Post-D2, the
        // wizard and /provider --setup populate per-provider fields
        // so /provider <kind> switches can pick the right one
        // without losing the other.
        run_v10_per_provider_keys(db).await?;
        set_schema_version(db, 10).await?;
    }

    if current_version < 11 {
        // v11 (Sprint 5 OPENAI-COMPAT-PROVIDER-01): stamp empty
        // `openai_compat: {ollama_endpoint, ollama_model,
        // lm_studio_endpoint, lm_studio_model}` on existing
        // config.system docs. Pre-v11 docs deserialize fine via
        // #[serde(default)], but stamping the field on disk makes it
        // visible to operators inspecting the doc directly. No
        // session-doc walk needed — pre-v11 sessions are always
        // anthropic / gemini and naturally remain so.
        run_v11_openai_compat(db).await?;
        set_schema_version(db, 11).await?;
    }

    if current_version < 12 {
        // v12 (embra-guardian-v1): dynamic-tool manifest collection.
        run_v12_guardian(db).await?;
        set_schema_version(db, 12).await?;
    }

    // Hot-path index assertions ride EVERY boot, after the versioned ladder
    // (unversioned on purpose — see ensure_hot_path_indexes).
    ensure_hot_path_indexes(db).await;

    // Ghost-sweep for TTL-reaped soft-deleted sessions — also every boot.
    sweep_reaped_sessions(db).await;

    info!("Migrations complete. Schema version: {}", CURRENT_SCHEMA_VERSION);
    Ok(())
}

/// A session's collections are reap residue exactly when BOTH canonical
/// docs are gone: WardSONDB's TTL worker reaps the `deleted_at`-stamped
/// docs of a soft-deleted session but never drops collections, and the
/// name-pattern session listings would enumerate the empty ghosts forever.
/// Either doc present = live, mid-create, or still in the grace period —
/// never drop.
fn session_is_reaped(meta_doc_present: bool, history_doc_present: bool) -> bool {
    !meta_doc_present && !history_doc_present
}

/// Drop the collection residue of TTL-reaped soft-deleted sessions. Runs
/// on every boot beside `ensure_hot_path_indexes`; warn-don't-fail. Costs
/// one single-doc query per session (only reaped ones pay more). Drop
/// order is summary → history → meta LAST: the loop keys on `.meta`
/// collections, so a crash mid-sweep leaves the meta collection behind
/// and the next boot retries (meta-first would strand the siblings).
async fn sweep_reaped_sessions(db: &WardsonDbClient) {
    let collections = match db.list_collections().await {
        Ok(c) => c,
        Err(e) => {
            warn!("Reaped-session sweep skipped (list_collections failed): {}", e);
            return;
        }
    };
    for coll in collections {
        let Some(name) = coll
            .strip_prefix("sessions.")
            .and_then(|r| r.strip_suffix(".meta"))
        else {
            continue;
        };
        // A query error reads as "present" — conservative: never drop on
        // uncertainty.
        let meta_present = doc_present(db, &coll).await;
        if meta_present {
            continue;
        }
        let history_coll = format!("sessions.{}.history", name);
        let history_present = doc_present(db, &history_coll).await;
        if !session_is_reaped(meta_present, history_present) {
            continue;
        }
        let summary_coll = format!("sessions.{}.summary", name);
        let mut all_dropped = true;
        for target in [&summary_coll, &history_coll, &coll] {
            if let Err(e) = db.drop_collection(target).await {
                warn!("Reaped-session sweep: drop {} failed (retrying next boot): {}", target, e);
                all_dropped = false;
                break; // keep .meta alive as the retry key
            }
        }
        if all_dropped {
            info!("Reaped-session sweep: dropped collections of TTL-reaped session '{}'", name);
        }
    }
}

/// True when the collection's canonical single doc exists. Errors read as
/// present (see sweep).
async fn doc_present(db: &WardsonDbClient, collection: &str) -> bool {
    match db
        .query(collection, &crate::sessions::history_query_body())
        .await
    {
        Ok(docs) => !docs.is_empty(),
        Err(_) => true,
    }
}

/// Hot-path secondary indexes as `(collection, create_index body)`.
///
/// idx_edge_source_id (single-field `source_id`): the traversal source arm
/// queries eq `{source_id, source_collection}` (`traversal.rs`). WardSONDB
/// builds with the F2 planner fix refuse to serve single-field lookups from
/// compound indexes (compounds exclude docs missing any component field),
/// so without a real single-field index every hop's source arm plans as a
/// full memory.edges scan. The target arm already rides the single-field
/// `idx_edge_target` (v5).
fn hot_path_index_specs() -> Vec<(&'static str, serde_json::Value)> {
    vec![(
        "memory.edges",
        serde_json::json!({"name": "idx_edge_source_id", "field": "source_id"}),
    )]
}

/// Assert hot-path indexes exist — warn-don't-fail, runs on every boot.
///
/// Deliberately NOT a versioned migration: pre-F2 WardSONDB builds
/// misdetect a single-field create shadowed by a compound index as a
/// duplicate (409, which `create_index` treats as idempotent success), so a
/// one-shot migration would record itself applied with the index never
/// created — and never re-run once a fixed server build arrives.
/// Re-asserting per boot self-heals across server upgrades; on pre-F2
/// builds the missing index is harmless (that planner still serves the
/// lookup from a `source_id`-leading compound).
async fn ensure_hot_path_indexes(db: &WardsonDbClient) {
    for (collection, body) in hot_path_index_specs() {
        let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        match db.create_index(collection, &body).await {
            Ok(_) => info!("Hot-path index {} on {} ensured", name, collection),
            Err(e) => warn!(
                "Hot-path index {} on {} failed (continuing without it): {}",
                name, collection, e
            ),
        }
    }
}

#[cfg(test)]
mod reaped_sweep_tests {
    use super::session_is_reaped;

    /// Only the both-gone state is reap residue; either doc present means
    /// live / mid-create / grace period — never droppable.
    #[test]
    fn only_both_docs_gone_is_reaped() {
        assert!(session_is_reaped(false, false));
        assert!(!session_is_reaped(true, false));
        assert!(!session_is_reaped(false, true));
        assert!(!session_is_reaped(true, true));
    }

    /// Dotted session names round-trip the meta-collection parse verbatim
    /// (dots are legal in names — never parse them).
    #[test]
    fn dotted_names_strip_verbatim() {
        let name = "sessions.foo.bar.meta"
            .strip_prefix("sessions.")
            .and_then(|r| r.strip_suffix(".meta"));
        assert_eq!(name, Some("foo.bar"));
    }
}

#[cfg(test)]
mod hot_path_index_tests {
    use super::hot_path_index_specs;

    /// The traversal source arm depends on this exact spec: a SINGLE-FIELD
    /// index (`field`, not `fields`) on memory.edges.source_id. A compound
    /// here would be refused for single-field lookups by post-F2 WardSONDB
    /// planners and the arm would silently fall back to a full scan.
    #[test]
    fn source_id_index_is_single_field_on_edges() {
        let specs = hot_path_index_specs();
        let (collection, body) = specs
            .iter()
            .find(|(_, b)| b["name"] == "idx_edge_source_id")
            .expect("idx_edge_source_id spec present");
        assert_eq!(*collection, "memory.edges");
        assert_eq!(body["field"], "source_id");
        assert!(
            body.get("fields").is_none(),
            "must stay single-field — compound form regresses the arm to full scan"
        );
    }
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

/// Migration v12 (embra-guardian-v1): create the `guardian.tools`
/// collection that persists dynamic-tool manifests (one doc per tool,
/// `_id == name`). Idempotent — brand-new feature, no data walk.
async fn run_v12_guardian(db: &WardsonDbClient) -> Result<()> {
    info!("Running migration v12: guardian.tools collection");
    if !db.collection_exists("guardian.tools").await.unwrap_or(false) {
        info!("Creating collection: guardian.tools");
        let _ = db.create_collection("guardian.tools").await;
    }
    info!("Migration v12: guardian.tools collection ensured");
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

/// Migration v9 (GEMINI-PROVIDER-01): pluggable provider scaffolding.
///
/// Three idempotent passes:
/// 1. Create `provider.gemini_cache` collection (singleton handle
///    storage; 409 conflict on create is success).
/// 2. Stamp `config.system:config` with `api_provider = "anthropic"`
///    if the field is missing. Pre-v9 configs had no provider field;
///    every existing install becomes Anthropic-by-default.
/// 3. Iterate every `sessions.<name>.meta` doc; if `provider` or
///    `model` is missing, patch with the Anthropic defaults.
///
/// All three steps tolerate already-applied state — re-running v9
/// after a clean run is a no-op.
async fn run_v9_pluggable_provider(db: &WardsonDbClient) -> Result<()> {
    info!("Running migration v9: pluggable provider scaffolding");

    // 1. Provider cache collection.
    if !db
        .collection_exists("provider.gemini_cache")
        .await
        .unwrap_or(false)
    {
        match db.create_collection("provider.gemini_cache").await {
            Ok(_) => info!("Migration v9: created provider.gemini_cache"),
            Err(e) => {
                let already_exists = matches!(
                    e.downcast_ref::<WardsonDbError>(),
                    Some(WardsonDbError::Api { status: 409, .. })
                );
                if !already_exists {
                    error!("Migration v9: create_collection failed: {e}");
                    return Err(e);
                }
                info!("Migration v9: provider.gemini_cache already exists");
            }
        }
    }

    // 2. Stamp config.system:config with api_provider if missing.
    if let Ok(mut cfg_doc) = db.read("config.system", "config").await {
        let needs_patch = cfg_doc.get("api_provider").and_then(|v| v.as_str()).is_none();
        if needs_patch {
            if let Some(obj) = cfg_doc.as_object_mut() {
                obj.insert(
                    "api_provider".into(),
                    serde_json::Value::String("anthropic".to_string()),
                );
            }
            match db.update("config.system", "config", &cfg_doc).await {
                Ok(_) => info!("Migration v9: stamped config.system api_provider=anthropic"),
                Err(e) => warn!("Migration v9: config.system update failed: {e}"),
            }
        }
    }

    // 3. Iterate sessions.<name>.meta. WardSONDB exposes
    //    list_collections to enumerate; meta collections are named
    //    `sessions.<name>.meta`.
    if let Ok(collections) = db.list_collections().await {
        let meta_collections: Vec<&String> = collections
            .iter()
            .filter(|c| c.starts_with("sessions.") && c.ends_with(".meta"))
            .collect();
        info!(
            "Migration v9: found {} session meta collection(s) to inspect",
            meta_collections.len()
        );
        for collection in meta_collections {
            // Session-meta docs are written without an explicit `_id`,
            // so WardSONDB auto-generates one — `db.read(_, session_name)`
            // would 404. Query the collection for its single doc and
            // update by the discovered `_id` (matches the pattern in
            // `SessionManager::update_state` / `::reattach`).
            let results = match db.query(collection, &serde_json::json!({})).await {
                Ok(r) => r,
                Err(_) => continue,
            };
            let mut meta_doc = match results.into_iter().next() {
                Some(d) => d,
                None => continue,
            };
            let id = meta_doc
                .get("_id")
                .or_else(|| meta_doc.get("id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let id = match id {
                Some(i) => i,
                None => continue,
            };
            let mut mutated = false;
            if let Some(obj) = meta_doc.as_object_mut() {
                if obj.get("provider").and_then(|v| v.as_str()).is_none() {
                    obj.insert(
                        "provider".into(),
                        serde_json::Value::String("anthropic".to_string()),
                    );
                    mutated = true;
                }
                if obj.get("model").and_then(|v| v.as_str()).is_none() {
                    obj.insert(
                        "model".into(),
                        serde_json::Value::String("opus-4.7".to_string()),
                    );
                    mutated = true;
                }
            }
            if mutated {
                if let Err(e) = db.update(collection, &id, &meta_doc).await {
                    warn!(
                        "Migration v9: failed to update {}: {}",
                        collection, e
                    );
                }
            }
        }
    } else {
        warn!("Migration v9: list_collections failed; skipping session meta backfill");
    }

    Ok(())
}

/// Migration v10 (Sprint 4 D2): backfill per-provider API key fields
/// on `config.system:config`. Reads the existing `api_key` +
/// `api_provider` and writes into the matching `<provider>_api_key`
/// field. Idempotent — re-running on a post-v10 doc is a no-op
/// because the per-provider field is already populated.
async fn run_v10_per_provider_keys(db: &WardsonDbClient) -> Result<()> {
    info!("Running migration v10: backfill per-provider API keys");

    let mut cfg_doc = match db.read("config.system", "config").await {
        Ok(doc) => doc,
        Err(_) => {
            info!("Migration v10: no config.system:config — nothing to backfill");
            return Ok(());
        }
    };

    let api_key = cfg_doc
        .get("api_key")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let api_provider = cfg_doc
        .get("api_provider")
        .and_then(|v| v.as_str())
        .unwrap_or("anthropic")
        .to_string();

    if api_key.is_empty() {
        info!("Migration v10: api_key is empty; skipping per-provider backfill");
        return Ok(());
    }

    let target_field = match api_provider.as_str() {
        "gemini" => "gemini_api_key",
        _ => "anthropic_api_key",
    };

    let already_set = cfg_doc
        .get(target_field)
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if already_set {
        info!(
            "Migration v10: {} already populated — no-op",
            target_field
        );
        return Ok(());
    }

    if let Some(obj) = cfg_doc.as_object_mut() {
        obj.insert(
            target_field.to_string(),
            serde_json::Value::String(api_key),
        );
    }

    if let Err(e) = db.update("config.system", "config", &cfg_doc).await {
        warn!("Migration v10: config.system update failed: {e}");
    } else {
        info!(
            "Migration v10: backfilled {} from legacy api_key + api_provider",
            target_field
        );
    }

    Ok(())
}

#[cfg(test)]
mod v10_tests {
    use serde_json::json;

    /// Pure-fn test of the backfill transform applied to a JSON doc.
    /// Mirrors what `run_v10_per_provider_keys` does in-memory before
    /// writing.
    fn backfill(cfg_doc: &mut serde_json::Value) -> bool {
        let api_key = cfg_doc
            .get("api_key")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let api_provider = cfg_doc
            .get("api_provider")
            .and_then(|v| v.as_str())
            .unwrap_or("anthropic")
            .to_string();
        if api_key.is_empty() {
            return false;
        }
        let target_field = match api_provider.as_str() {
            "gemini" => "gemini_api_key",
            _ => "anthropic_api_key",
        };
        let already_set = cfg_doc
            .get(target_field)
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if already_set {
            return false;
        }
        if let Some(obj) = cfg_doc.as_object_mut() {
            obj.insert(
                target_field.to_string(),
                serde_json::Value::String(api_key),
            );
        }
        true
    }

    #[test]
    fn v10_backfills_anthropic_key_when_provider_is_anthropic() {
        let mut doc = json!({
            "api_key": "sk-test-key",
            "api_provider": "anthropic"
        });
        assert!(backfill(&mut doc));
        assert_eq!(doc["anthropic_api_key"], "sk-test-key");
        assert!(doc.get("gemini_api_key").is_none());
    }

    #[test]
    fn v10_backfills_gemini_key_when_provider_is_gemini() {
        let mut doc = json!({
            "api_key": "AIza-test-key",
            "api_provider": "gemini"
        });
        assert!(backfill(&mut doc));
        assert_eq!(doc["gemini_api_key"], "AIza-test-key");
        assert!(doc.get("anthropic_api_key").is_none());
    }

    #[test]
    fn v10_defaults_to_anthropic_when_provider_missing() {
        let mut doc = json!({
            "api_key": "sk-legacy-key"
        });
        assert!(backfill(&mut doc));
        assert_eq!(doc["anthropic_api_key"], "sk-legacy-key");
    }

    #[test]
    fn v10_skip_when_target_already_populated() {
        let mut doc = json!({
            "api_key": "sk-active",
            "api_provider": "anthropic",
            "anthropic_api_key": "sk-existing"
        });
        assert!(!backfill(&mut doc));
        // Existing value preserved.
        assert_eq!(doc["anthropic_api_key"], "sk-existing");
    }

    #[test]
    fn v10_skip_when_api_key_empty() {
        let mut doc = json!({
            "api_key": "",
            "api_provider": "anthropic"
        });
        assert!(!backfill(&mut doc));
        assert!(doc.get("anthropic_api_key").is_none());
    }

    #[test]
    fn v10_is_idempotent() {
        let mut doc = json!({
            "api_key": "sk-active",
            "api_provider": "anthropic"
        });
        assert!(backfill(&mut doc));
        // Second run is a no-op — target is already populated.
        assert!(!backfill(&mut doc));
    }
}

#[cfg(test)]
mod v9_tests {
    use serde_json::json;

    /// Pure-fn tests covering the v9 transform shape on an isolated
    /// JSON document. Integration with WardSONDB lives in Stage 11
    /// QEMU smoke (the migration runs once at brain startup against
    /// real state).
    fn patch_meta_doc(meta: &mut serde_json::Value) -> bool {
        let mut mutated = false;
        if let Some(obj) = meta.as_object_mut() {
            if obj.get("provider").and_then(|v| v.as_str()).is_none() {
                obj.insert("provider".into(), json!("anthropic"));
                mutated = true;
            }
            if obj.get("model").and_then(|v| v.as_str()).is_none() {
                obj.insert("model".into(), json!("opus-4.7"));
                mutated = true;
            }
        }
        mutated
    }

    #[test]
    fn v9_meta_patch_stamps_defaults_on_legacy_doc() {
        let mut doc = json!({
            "id": "abc",
            "name": "main",
            "state": "Active",
            "created_at": "2026-04-01T00:00:00Z",
            "last_active": "2026-04-01T00:00:00Z"
        });
        assert!(patch_meta_doc(&mut doc));
        assert_eq!(doc["provider"], "anthropic");
        assert_eq!(doc["model"], "opus-4.7");
    }

    #[test]
    fn v9_meta_patch_preserves_existing_provider() {
        let mut doc = json!({
            "id": "abc",
            "name": "main",
            "provider": "gemini",
            "model": "gemini-3.1-pro"
        });
        assert!(!patch_meta_doc(&mut doc));
        assert_eq!(doc["provider"], "gemini");
        assert_eq!(doc["model"], "gemini-3.1-pro");
    }

    #[test]
    fn v9_meta_patch_partial_only_fills_missing() {
        let mut doc = json!({
            "id": "abc",
            "name": "main",
            "provider": "gemini"
        });
        assert!(patch_meta_doc(&mut doc));
        assert_eq!(doc["provider"], "gemini"); // preserved
        assert_eq!(doc["model"], "opus-4.7"); // added
    }

    #[test]
    fn v9_meta_patch_is_idempotent() {
        let mut doc = json!({
            "id": "abc",
            "name": "main",
        });
        assert!(patch_meta_doc(&mut doc));
        // Second run is a no-op.
        assert!(!patch_meta_doc(&mut doc));
    }
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

/// Migration v11 (Sprint 5 OPENAI-COMPAT-PROVIDER-01): stamp the empty
/// `openai_compat` sub-document on `config.system:config` so
/// post-migration reads see the field with all four leaf strings
/// (Ollama / LM Studio endpoint + model). Pre-v11 docs deserialize via
/// `#[serde(default)]`, but stamping makes the schema visible to
/// operators inspecting the doc directly.
///
/// Idempotent: re-running on a doc that already carries the field
/// (or any subset of its leaves) is a no-op.
async fn run_v11_openai_compat(db: &WardsonDbClient) -> Result<()> {
    info!("Running migration v11: stamp openai_compat on config.system");

    let mut cfg_doc = match db.read("config.system", "config").await {
        Ok(doc) => doc,
        Err(_) => {
            info!("Migration v11: no config.system:config — nothing to stamp");
            return Ok(());
        }
    };

    let changed = stamp_openai_compat(&mut cfg_doc);
    if !changed {
        info!("Migration v11: openai_compat already complete — no-op");
        return Ok(());
    }

    if let Err(e) = db.update("config.system", "config", &cfg_doc).await {
        warn!("Migration v11: config.system update failed: {e}");
    } else {
        info!("Migration v11: stamped empty openai_compat on config.system");
    }
    Ok(())
}

/// Pure transform behind `run_v11_openai_compat`. Inserts any of the
/// four leaf string fields that are missing under `openai_compat`,
/// creating the parent object if absent. Returns true when the doc
/// was modified, false when it was already complete (idempotent).
fn stamp_openai_compat(cfg_doc: &mut serde_json::Value) -> bool {
    let Some(obj) = cfg_doc.as_object_mut() else {
        return false;
    };
    // Ensure parent exists.
    let parent_was_missing = !obj.contains_key("openai_compat");
    let parent = obj
        .entry("openai_compat".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let Some(parent_obj) = parent.as_object_mut() else {
        // Parent exists but isn't an object — overwrite with a fresh
        // empty object. This shouldn't happen in practice; defensive.
        *parent = serde_json::Value::Object(serde_json::Map::new());
        // Re-borrow.
        let parent_obj = parent.as_object_mut().expect("just inserted");
        parent_obj.insert("ollama_endpoint".into(), serde_json::Value::String(String::new()));
        parent_obj.insert("ollama_model".into(), serde_json::Value::String(String::new()));
        parent_obj.insert("lm_studio_endpoint".into(), serde_json::Value::String(String::new()));
        parent_obj.insert("lm_studio_model".into(), serde_json::Value::String(String::new()));
        return true;
    };

    let mut changed = parent_was_missing;
    for key in [
        "ollama_endpoint",
        "ollama_model",
        "lm_studio_endpoint",
        "lm_studio_model",
    ] {
        if !parent_obj.contains_key(key) {
            parent_obj.insert(key.into(), serde_json::Value::String(String::new()));
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod v11_tests {
    use super::stamp_openai_compat;
    use serde_json::json;

    #[test]
    fn missing_field_gets_full_stamp() {
        let mut doc = json!({"name": "Embra", "api_provider": "anthropic"});
        let changed = stamp_openai_compat(&mut doc);
        assert!(changed);
        let oc = &doc["openai_compat"];
        assert_eq!(oc["ollama_endpoint"], "");
        assert_eq!(oc["ollama_model"], "");
        assert_eq!(oc["lm_studio_endpoint"], "");
        assert_eq!(oc["lm_studio_model"], "");
    }

    #[test]
    fn partial_field_only_fills_missing_leaves() {
        let mut doc = json!({
            "openai_compat": {
                "ollama_endpoint": "http://localhost:11434",
                "ollama_model": "gpt-oss:20b",
            }
        });
        let changed = stamp_openai_compat(&mut doc);
        assert!(changed);
        let oc = &doc["openai_compat"];
        // Existing values preserved.
        assert_eq!(oc["ollama_endpoint"], "http://localhost:11434");
        assert_eq!(oc["ollama_model"], "gpt-oss:20b");
        // Missing leaves stamped with empty string.
        assert_eq!(oc["lm_studio_endpoint"], "");
        assert_eq!(oc["lm_studio_model"], "");
    }

    #[test]
    fn full_field_is_idempotent_no_op() {
        let mut doc = json!({
            "openai_compat": {
                "ollama_endpoint": "http://localhost:11434",
                "ollama_model": "gpt-oss:20b",
                "lm_studio_endpoint": "http://localhost:1234",
                "lm_studio_model": "qwen3.6:35b",
            }
        });
        let before = doc.clone();
        let changed = stamp_openai_compat(&mut doc);
        assert!(!changed, "full doc should be no-op");
        assert_eq!(doc, before, "no-op must not mutate");
    }

    #[test]
    fn second_invocation_after_first_is_idempotent() {
        let mut doc = json!({"name": "Embra"});
        let changed1 = stamp_openai_compat(&mut doc);
        assert!(changed1);
        let after_first = doc.clone();
        let changed2 = stamp_openai_compat(&mut doc);
        assert!(!changed2, "second pass must be no-op");
        assert_eq!(doc, after_first);
    }
}
