use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use parking_lot::RwLock;
use roaring::RoaringBitmap;
use serde_json::Value;
use tracing::info;

use crate::query::filter::{FilterNode, FilterOp, resolve_json_path};

/// Bumped when the value-key encoding (or any persisted layout) changes:
/// `load_from_disk` refuses other formats, which routes startup through a
/// storage rebuild instead of serving keys the query side can't match.
/// v3: per-collection membership bitmaps (F1 — collection-scoped answers).
const BITMAP_SNAPSHOT_FORMAT: u64 = 3;

/// Collision-free, type-tagged string key for a value. One prefix byte
/// keeps every JSON type in a disjoint key space — under the old untagged
/// scheme the string "123" shared a key with the number 123 (bitmap eq
/// scans cross-matched them) and "__null__" with null, and the aggregate
/// path could only guess types back. `string_key_to_value` is the exact
/// inverse.
pub fn value_to_string_key(value: &Value) -> String {
    match value {
        Value::Null => "z".to_string(),
        Value::Bool(true) => "t".to_string(),
        Value::Bool(false) => "f".to_string(),
        Value::Number(n) => format!("n{n}"),
        Value::String(s) => format!("s{s}"),
        other => format!("j{}", serde_json::to_string(other).unwrap_or_default()),
    }
}

/// Exact inverse of `value_to_string_key` (numbers and containers decode
/// through serde, which produced their text in the first place).
pub fn string_key_to_value(key: &str) -> Value {
    match key.as_bytes().first() {
        Some(b'z') => Value::Null,
        Some(b't') => Value::Bool(true),
        Some(b'f') => Value::Bool(false),
        Some(b'n') | Some(b'j') => serde_json::from_str(&key[1..]).unwrap_or(Value::Null),
        Some(b's') => Value::String(key[1..].to_string()),
        // Unreachable for keys this module produced.
        _ => Value::Null,
    }
}

// ── RowPositionMap ──────────────────────────────────────────────────────────

/// Bidirectional mapping between document IDs and row positions (u32).
pub struct RowPositionMap {
    id_to_pos: RwLock<HashMap<Arc<str>, u32>>,
    pos_to_id: RwLock<Vec<Option<Arc<str>>>>,
    /// Which positions belong to which collection. The position space and
    /// the value bitmaps are GLOBAL across collections, so every bitmap
    /// answer must be intersected with the queried collection's membership
    /// (F1 — un-scoped answers returned other collections' documents), and
    /// drop_collection subtracts one membership instead of wiping the
    /// accelerator. This guard is only ever taken alone or after the id/pos
    /// guards drop — never nested under `columns` — so it adds no
    /// lock-order edge to the positions-before-columns discipline.
    by_collection: RwLock<HashMap<String, RoaringBitmap>>,
    next_pos: AtomicU32,
    hole_count: AtomicU32,
}

impl Default for RowPositionMap {
    fn default() -> Self {
        Self::new()
    }
}

impl RowPositionMap {
    pub fn new() -> Self {
        RowPositionMap {
            id_to_pos: RwLock::new(HashMap::new()),
            pos_to_id: RwLock::new(Vec::new()),
            by_collection: RwLock::new(HashMap::new()),
            next_pos: AtomicU32::new(0),
            hole_count: AtomicU32::new(0),
        }
    }

    /// Assign the next row position to a document ID. Membership is set
    /// last (sequential guards, never nested): a scan between the steps
    /// sees a position outside every collection, which simply doesn't
    /// match — the benign direction.
    pub fn assign(&self, doc_id: &str, collection: &str) -> Option<u32> {
        let pos = self.next_pos.fetch_add(1, Ordering::Relaxed);
        let shared: Arc<str> = Arc::from(doc_id);
        self.id_to_pos.write().insert(Arc::clone(&shared), pos);
        {
            let mut vec = self.pos_to_id.write();
            if pos as usize >= vec.len() {
                vec.resize(pos as usize + 1, None);
            }
            vec[pos as usize] = Some(shared);
        }
        self.by_collection
            .write()
            .entry(collection.to_string())
            .or_default()
            .insert(pos);
        Some(pos)
    }

    /// Lookup row position by document ID.
    pub fn get_position(&self, doc_id: &str) -> Option<u32> {
        self.id_to_pos.read().get(doc_id).copied()
    }

    /// Lookup document ID by row position. Query paths batch through
    /// `resolve_window` instead; kept for single-position callers.
    #[allow(dead_code)]
    pub fn get_doc_id(&self, pos: u32) -> Option<Arc<str>> {
        let vec = self.pos_to_id.read();
        vec.get(pos as usize).and_then(|opt| opt.clone())
    }

    /// Remove a document from id_to_pos (position stays allocated; bitmap handles the hole).
    pub fn remove(&self, doc_id: &str, collection: &str) {
        let pos = self.id_to_pos.write().remove(doc_id);
        if let Some(pos) = pos {
            {
                let mut vec = self.pos_to_id.write();
                if let Some(slot) = vec.get_mut(pos as usize) {
                    *slot = None;
                }
            }
            if let Some(members) = self.by_collection.write().get_mut(collection) {
                members.remove(pos);
            }
            self.hole_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Restrict a global bitmap answer to one collection's positions (F1).
    /// A collection with no membership entry has no documents: the result
    /// empties rather than leaking other collections' positions.
    pub fn scope_to_collection(&self, bitmap: &mut RoaringBitmap, collection: &str) {
        match self.by_collection.read().get(collection) {
            Some(members) => *bitmap &= members,
            None => bitmap.clear(),
        }
    }

    /// `len(bitmap ∩ collection membership)` without materializing.
    pub fn scoped_len(&self, bitmap: &RoaringBitmap, collection: &str) -> u64 {
        self.by_collection
            .read()
            .get(collection)
            .map(|m| bitmap.intersection_len(m))
            .unwrap_or(0)
    }

    /// Remove every position belonging to `collection` (drop_collection).
    /// The membership entry is taken out FIRST, so scoped scans exclude the
    /// collection immediately; the id/pos cleanup then proceeds in bounded
    /// chunks so concurrent writers never stall behind one long guard.
    /// Returns the removed positions for the caller's column subtraction.
    pub fn remove_collection(&self, collection: &str) -> RoaringBitmap {
        let removed = match self.by_collection.write().remove(collection) {
            Some(m) => m,
            None => return RoaringBitmap::new(),
        };
        let mut holes = 0u32;
        let mut iter = removed.iter();
        loop {
            let chunk: Vec<u32> = iter.by_ref().take(65_536).collect();
            if chunk.is_empty() {
                break;
            }
            // id_to_pos then pos_to_id — the same acquisition order as
            // assign/remove/persist (one global order, no ABBA).
            let mut ids = self.id_to_pos.write();
            let mut vec = self.pos_to_id.write();
            for &pos in &chunk {
                if let Some(slot) = vec.get_mut(pos as usize)
                    && let Some(id) = slot.take()
                {
                    ids.remove(&*id);
                    holes += 1;
                }
            }
        }
        self.hole_count.fetch_add(holes, Ordering::Relaxed);
        removed
    }

    /// Number of active mappings.
    pub fn len(&self) -> u32 {
        self.id_to_pos.read().len() as u32
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.id_to_pos.read().is_empty()
    }

    pub fn clear(&self) {
        self.id_to_pos.write().clear();
        self.pos_to_id.write().clear();
        self.by_collection.write().clear();
        self.next_pos.store(0, Ordering::Relaxed);
        self.hole_count.store(0, Ordering::Relaxed);
    }

    /// Ratio of deleted (None) holes to total allocated positions.
    pub fn hole_ratio(&self) -> f32 {
        let total = self.next_pos.load(Ordering::Relaxed);
        if total == 0 {
            return 0.0;
        }
        let holes = self.hole_count.load(Ordering::Relaxed);
        holes as f32 / total as f32
    }

    /// Resolve bitmap positions (ascending order) to doc ids under ONE short
    /// guard, dropped before the caller does any IO — never hold a position
    /// or column guard across blocking work (the b965de5 rule). `skip`/`take`
    /// window the *resolved ids* (matching the materialized paths, which
    /// offset over docs), so hole positions from transient delete races are
    /// skipped without consuming the window.
    pub fn resolve_window(
        &self,
        positions: &RoaringBitmap,
        skip: usize,
        take: usize,
    ) -> Vec<Arc<str>> {
        let vec = self.pos_to_id.read();
        positions
            .iter()
            .filter_map(|pos| vec.get(pos as usize).and_then(|slot| slot.clone()))
            .skip(skip)
            .take(take)
            .collect()
    }

    /// Estimated memory usage in bytes, accounting for variable-length IDs.
    pub fn memory_bytes(&self) -> usize {
        let id_map = self.id_to_pos.read();
        let pos_vec = self.pos_to_id.read();
        // HashMap per-entry overhead: hash + bucket pointer + key (Arc ptr 8 bytes) + u32 value ≈ 48 bytes
        // Plus actual string bytes + Arc header (16 bytes) per unique ID
        let mut id_bytes: usize = id_map.len() * 48;
        for key in id_map.keys() {
            id_bytes += key.len() + 16; // string bytes + Arc header
        }
        // Vec: each slot is Option<Arc<str>> = 8 bytes (pointer-sized)
        let vec_bytes = pos_vec.len() * std::mem::size_of::<Option<Arc<str>>>();
        drop(pos_vec);
        drop(id_map);
        let member_bytes: usize = self
            .by_collection
            .read()
            .iter()
            .map(|(name, bm)| name.len() + bm.serialized_size())
            .sum();
        id_bytes + vec_bytes + member_bytes
    }
}

// ── BitmapColumn ────────────────────────────────────────────────────────────

/// One bitmap per distinct value for a tracked field.
pub struct BitmapColumn {
    #[allow(dead_code)]
    pub field_path: String,
    /// value_key -> RoaringBitmap of row positions
    pub value_bitmaps: RwLock<HashMap<String, RoaringBitmap>>,
    /// All positions that have this field (for $exists and $ne)
    pub exists_bitmap: RwLock<RoaringBitmap>,
    pub cardinality: AtomicU32,
}

impl BitmapColumn {
    pub fn new(field_path: String) -> Self {
        BitmapColumn {
            field_path,
            value_bitmaps: RwLock::new(HashMap::new()),
            exists_bitmap: RwLock::new(RoaringBitmap::new()),
            cardinality: AtomicU32::new(0),
        }
    }

    /// Estimated memory usage in bytes.
    pub fn memory_bytes(&self) -> usize {
        let bitmaps = self.value_bitmaps.read();
        let mut total = 0usize;
        for bitmap in bitmaps.values() {
            total += bitmap.serialized_size();
        }
        total += self.exists_bitmap.read().serialized_size();
        total
    }
}

/// Immutable per-column snapshot used by `persist_to_disk` so guards are
/// never held across blocking I/O. Fields mirror `BitmapColumn` but everything
/// is already cloned out from under the locks.
struct ColumnSnapshot {
    field_path: String,
    cardinality: u32,
    value_bitmaps: HashMap<String, RoaringBitmap>,
    exists_bitmap: RoaringBitmap,
}

// ── ScanAccelerator ─────────────────────────────────────────────────────────

/// Result of a bitmap scan: the matching positions + any residual filter.
pub struct BitmapScanResult {
    /// Bitmap of matching row positions.
    pub bitmap: RoaringBitmap,
    /// Filter conditions NOT covered by the bitmap (need post-filtering on loaded docs).
    pub residual_filter: Option<FilterNode>,
}

pub struct AcceleratorConfig {
    /// Fields to track with bitmaps.
    pub bitmap_fields: Vec<String>,
    /// Maximum distinct values per column before disabling that column.
    pub max_cardinality: u32,
    /// Maximum memory budget in bytes for all bitmap data. 0 = unlimited.
    pub max_memory_bytes: u64,
}

impl Default for AcceleratorConfig {
    fn default() -> Self {
        AcceleratorConfig {
            bitmap_fields: Vec::new(),
            max_cardinality: 1000,
            max_memory_bytes: 0,
        }
    }
}

/// One CRUD delta captured while a rebuild owns the maps (S2-1). The doc
/// values are cloned — the cost only exists during rebuild windows.
/// Fields: collection, doc id, doc value(s).
enum PendingOp {
    Insert(String, String, Value),
    Delete(String, String, Value),
    Update(String, String, Value, Value),
    /// A drop_collection that arrived while a rebuild owned the maps. The
    /// partition is already gone from storage, so the rebuild scan can't
    /// see its docs — draining this cleans up whatever the scan indexed
    /// before the drop landed (no-op when that's nothing). Drained in
    /// arrival order, so a same-name recreate's inserts replay after it.
    DropCollection(String),
}

pub struct ScanAccelerator {
    /// One BitmapColumn per tracked field.
    columns: RwLock<HashMap<String, BitmapColumn>>,
    /// Row position <-> document ID mapping.
    pub positions: RowPositionMap,
    /// True while a rebuild owns the maps: CRUD hooks queue their deltas in
    /// `pending` instead of touching state the rebuild is clearing and
    /// re-filling (S2-1 — hooks used to keep assigning positions from the
    /// live counter while `clear()` reset it, dropping or colliding
    /// positions for the whole rebuild window). Only `finish_rebuild` clears
    /// the flag, under the `pending` lock.
    rebuild_active: AtomicBool,
    /// Deltas that arrived during the rebuild window, drained idempotently
    /// by `finish_rebuild` before the accelerator serves again.
    pending: parking_lot::Mutex<Vec<PendingOp>>,
    /// Configuration.
    config: RwLock<AcceleratorConfig>,
    /// Cached copy of `config.max_cardinality` — read on every insert/update,
    /// so the hot path takes an atomic load instead of the config lock. Kept
    /// in sync by `set_max_cardinality`.
    max_cardinality: AtomicU32,
    /// false during rebuild; queries fall back to full scan.
    ready: AtomicBool,
    /// Cardinality profiler for auto-detection.
    profiler: CardinalityProfiler,
    /// true when memory budget is exceeded; skips bitmap column tracking.
    over_budget: AtomicBool,
    /// Cached total memory usage, refreshed by the background persist task.
    /// Read on the `on_insert` hot path to avoid the 4-lock `total_memory_bytes()` chain.
    cached_memory_bytes: AtomicU64,
}

impl ScanAccelerator {
    pub fn new(config: AcceleratorConfig) -> Self {
        let columns = RwLock::new(HashMap::new());
        // Pre-create columns for configured fields
        {
            let mut cols = columns.write();
            for field in &config.bitmap_fields {
                cols.insert(field.clone(), BitmapColumn::new(field.clone()));
            }
        }
        let has_fields = !config.bitmap_fields.is_empty();
        let max_cardinality = AtomicU32::new(config.max_cardinality);
        ScanAccelerator {
            columns,
            positions: RowPositionMap::new(),
            rebuild_active: AtomicBool::new(false),
            pending: parking_lot::Mutex::new(Vec::new()),
            config: RwLock::new(config),
            max_cardinality,
            ready: AtomicBool::new(false),
            profiler: CardinalityProfiler::new(has_fields),
            over_budget: AtomicBool::new(false),
            cached_memory_bytes: AtomicU64::new(0),
        }
    }

    /// True when the hook must queue instead of applying live. The re-check
    /// under the queue lock closes the drain boundary: `finish_rebuild`
    /// drains and clears the flag while holding `pending`, so an op can
    /// neither land in a drained queue nor race the flag.
    fn queue_if_rebuilding(&self, op: impl FnOnce() -> PendingOp) -> bool {
        if !self.rebuild_active.load(Ordering::Acquire) {
            return false;
        }
        let mut q = self.pending.lock();
        if self.rebuild_active.load(Ordering::Acquire) {
            q.push(op());
            return true;
        }
        false
    }

    /// Set the per-column cardinality cap, keeping the hot-path atomic cache
    /// in sync with the config (guard-based mutation would bypass the cache).
    pub fn set_max_cardinality(&self, v: u32) {
        self.config.write().max_cardinality = v;
        self.max_cardinality.store(v, Ordering::Relaxed);
    }

    pub fn set_max_memory_bytes(&self, v: u64) {
        self.config.write().max_memory_bytes = v;
    }

    /// Number of inserts the cardinality profiler samples before it reports
    /// its `--bitmap-fields` recommendation (`--bitmap-sample-size`).
    pub fn set_sample_size(&self, n: u32) {
        self.profiler.set_sample_target(n);
    }

    pub fn config_read(&self) -> parking_lot::RwLockReadGuard<'_, AcceleratorConfig> {
        self.config.read()
    }

    /// Total estimated memory usage across all bitmap data.
    pub fn total_memory_bytes(&self) -> usize {
        let columns = self.columns.read();
        let mut total: usize = self.positions.memory_bytes();
        for column in columns.values() {
            total += column.memory_bytes();
        }
        total
    }

    /// Refresh `cached_memory_bytes`. Called from the background persist task
    /// (inside `spawn_blocking`) so the `on_insert` hot path can read a cached
    /// value via a single atomic load instead of acquiring four RwLock guards.
    pub fn recompute_cached_memory(&self) {
        let bytes = self.total_memory_bytes() as u64;
        self.cached_memory_bytes.store(bytes, Ordering::Relaxed);
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::Release);
    }

    pub fn is_over_budget(&self) -> bool {
        self.over_budget.load(Ordering::Relaxed)
    }

    /// Configure bitmap fields and create columns.
    pub fn configure_fields(&self, fields: Vec<String>) {
        let mut cols = self.columns.write();
        for field in &fields {
            if !cols.contains_key(field) {
                cols.insert(field.clone(), BitmapColumn::new(field.clone()));
            }
        }
        self.config.write().bitmap_fields = fields;
    }

    /// Check if the accelerator has any bitmap columns configured.
    pub fn has_columns(&self) -> bool {
        !self.columns.read().is_empty()
    }

    /// Check if position map has excessive holes from TTL deletes (>25%).
    pub fn needs_compaction(&self) -> bool {
        self.positions.hole_ratio() > 0.25
    }

    // ── CRUD Hooks ──────────────────────────────────────────────────────

    /// Called after a document insert transaction commits.
    pub fn on_insert(&self, collection: &str, doc_id: &str, doc: &Value) {
        if self.queue_if_rebuilding(|| {
            PendingOp::Insert(collection.to_string(), doc_id.to_string(), doc.clone())
        }) {
            return;
        }
        self.insert_live(collection, doc_id, doc);
    }

    fn insert_live(&self, collection: &str, doc_id: &str, doc: &Value) {
        let pos = match self.positions.assign(doc_id, collection) {
            Some(p) => p,
            None => return,
        };

        // Feed the profiler during sampling phase
        if !self.profiler.is_done() {
            self.profiler.observe(doc);
            // Check if profiling just completed (we hit sample_target)
            if self.profiler.is_done() && self.columns.read().is_empty() {
                let max_card = self.max_cardinality.load(Ordering::Relaxed);
                let detected = self.profiler.analyze(max_card);
                if !detected.is_empty() {
                    let field_info: Vec<String> = detected
                        .iter()
                        .map(|(f, c)| format!("{f} ({c} values)"))
                        .collect();
                    let flag_value: Vec<String> = detected.iter().map(|(f, _)| f.clone()).collect();
                    // Recommendation ONLY — never create columns here. Docs
                    // inserted before detection were only profiled, so columns
                    // born now would be missing them forever (no safe live
                    // rebuild exists yet), and create_collection's re-arm
                    // (set_ready on has_columns) would start serving those
                    // incomplete bitmaps: silent false negatives. Activation
                    // requires --bitmap-fields at startup, which rebuilds from
                    // storage before serving.
                    info!(
                        fields = %field_info.join(", "),
                        flag = %format!("--bitmap-fields {}", flag_value.join(",")),
                        "Scan accelerator: low-cardinality fields detected — \
                         inactive; restart with the suggested flag to enable"
                    );
                }
                self.profiler.finish();
            }
        }

        // Check memory budget every 1000 inserts.
        // Reads `cached_memory_bytes` (a single atomic) instead of calling
        // `total_memory_bytes()` — the latter takes four RwLock reader guards
        // (config + columns + positions.id_to_pos + positions.pos_to_id) and
        // contributed to a write-halt deadlock against `persist_to_disk`.
        // The cache is refreshed by the background persist task every 60 s.
        if pos % 1000 == 0 {
            let budget = self.config.read().max_memory_bytes;
            if budget > 0 {
                let used = self.cached_memory_bytes.load(Ordering::Relaxed) as usize;
                let was_over = self.over_budget.load(Ordering::Relaxed);
                let is_over = used as u64 > budget;
                if is_over != was_over {
                    self.over_budget.store(is_over, Ordering::Relaxed);
                    if is_over {
                        tracing::warn!(
                            used_mb = used / (1024 * 1024),
                            budget_mb = budget / (1024 * 1024),
                            "Bitmap memory budget exceeded, pausing column tracking"
                        );
                    }
                }
            }
        }

        if self.over_budget.load(Ordering::Relaxed) {
            return;
        }

        let columns = self.columns.read();
        let max_card = self.max_cardinality.load(Ordering::Relaxed);

        for (field_path, column) in columns.iter() {
            if let Some(value) = resolve_json_path(doc, field_path) {
                let value_key = value_to_string_key(value);

                if column.cardinality.load(Ordering::Relaxed) < max_card
                    || column.value_bitmaps.read().contains_key(&value_key)
                {
                    let mut bitmaps = column.value_bitmaps.write();
                    let is_new = !bitmaps.contains_key(&value_key);
                    bitmaps
                        .entry(value_key)
                        .or_insert_with(RoaringBitmap::new)
                        .insert(pos);
                    if is_new {
                        column
                            .cardinality
                            .store(bitmaps.len() as u32, Ordering::Relaxed);
                    }
                }

                column.exists_bitmap.write().insert(pos);
            }
        }
    }

    /// Called after a document delete transaction commits.
    pub fn on_delete(&self, collection: &str, doc_id: &str, doc: &Value) {
        if self.queue_if_rebuilding(|| {
            PendingOp::Delete(collection.to_string(), doc_id.to_string(), doc.clone())
        }) {
            return;
        }
        self.delete_live(collection, doc_id, doc);
    }

    fn delete_live(&self, collection: &str, doc_id: &str, doc: &Value) {
        let pos = match self.positions.get_position(doc_id) {
            Some(p) => p,
            None => return,
        };

        // Positions BEFORE columns — the one lock order every path uses
        // (insert/update/persist all touch positions first), so no
        // columns→positions hold can form an ABBA cycle against the
        // persist snapshot. A reader between the two steps sees column bits
        // for a position with no id, which `resolve_window` skips — the
        // benign direction.
        self.positions.remove(doc_id, collection);

        let columns = self.columns.read();
        for (field_path, column) in columns.iter() {
            if let Some(value) = resolve_json_path(doc, field_path) {
                let value_key = value_to_string_key(value);
                let mut bitmaps = column.value_bitmaps.write();
                let mut remove_key = false;
                if let Some(bitmap) = bitmaps.get_mut(&value_key) {
                    bitmap.remove(pos);
                    if bitmap.is_empty() {
                        remove_key = true;
                    }
                }
                if remove_key {
                    bitmaps.remove(&value_key);
                    column.cardinality.fetch_sub(1, Ordering::Relaxed);
                }
                drop(bitmaps);
                // Inside the field-present guard (symmetric with on_insert):
                // exists_bitmap only ever holds positions whose doc had the
                // field, so absent fields need no write lock here.
                column.exists_bitmap.write().remove(pos);
            }
        }
    }

    /// Called after a document update transaction commits.
    /// Uses a single write lock acquisition per column. `collection` rides
    /// along for the rebuild queue (an update whose doc the rebuild scan
    /// never saw drains as an insert, which needs the membership home);
    /// a live in-place update never moves a position between collections.
    pub fn on_update(&self, collection: &str, doc_id: &str, old_doc: &Value, new_doc: &Value) {
        if self.queue_if_rebuilding(|| {
            PendingOp::Update(
                collection.to_string(),
                doc_id.to_string(),
                old_doc.clone(),
                new_doc.clone(),
            )
        }) {
            return;
        }
        self.update_live(doc_id, old_doc, new_doc);
    }

    fn update_live(&self, doc_id: &str, old_doc: &Value, new_doc: &Value) {
        let pos = match self.positions.get_position(doc_id) {
            Some(p) => p,
            None => return,
        };

        let columns = self.columns.read();
        let max_card = self.max_cardinality.load(Ordering::Relaxed);

        for (field_path, column) in columns.iter() {
            // Compare the resolved values first: an update that doesn't touch
            // this column costs zero allocations. Only genuine changes pay
            // for the string keys (whose comparison stays authoritative —
            // distinct Values can share a key, e.g. null vs "__null__", and
            // must keep no-oping).
            let old_ref = resolve_json_path(old_doc, field_path);
            let new_ref = resolve_json_path(new_doc, field_path);
            if old_ref == new_ref {
                continue;
            }
            let old_val = old_ref.map(value_to_string_key);
            let new_val = new_ref.map(value_to_string_key);

            if old_val != new_val {
                // Single write lock for both remove and insert
                let mut bitmaps = column.value_bitmaps.write();

                // Remove from old bitmap
                if let Some(old_key) = &old_val {
                    let mut remove_key = false;
                    if let Some(bitmap) = bitmaps.get_mut(old_key) {
                        bitmap.remove(pos);
                        if bitmap.is_empty() {
                            remove_key = true;
                        }
                    }
                    if remove_key {
                        bitmaps.remove(old_key);
                        column.cardinality.fetch_sub(1, Ordering::Relaxed);
                    }
                }

                // Add to new bitmap
                if let Some(new_key) = &new_val {
                    let card = column.cardinality.load(Ordering::Relaxed);
                    if card < max_card || bitmaps.contains_key(new_key) {
                        let is_new = !bitmaps.contains_key(new_key);
                        bitmaps
                            .entry(new_key.clone())
                            .or_insert_with(RoaringBitmap::new)
                            .insert(pos);
                        if is_new {
                            column
                                .cardinality
                                .store(bitmaps.len() as u32, Ordering::Relaxed);
                        }
                    }
                }

                drop(bitmaps); // Release write lock before exists_bitmap lock

                // Update exists bitmap
                let mut exists = column.exists_bitmap.write();
                if old_val.is_some() && new_val.is_none() {
                    exists.remove(pos);
                } else if old_val.is_none() && new_val.is_some() {
                    exists.insert(pos);
                }
            }
        }
    }

    /// Called when a collection is dropped: surgically remove its positions
    /// and column bits. Acceleration for every other collection stays live
    /// — this path used to `clear()` the WHOLE accelerator and flip ready
    /// off with nothing to re-arm it until restart (F1d). During a rebuild
    /// the drop queues like any other delta.
    pub fn on_drop_collection(&self, collection: &str) {
        if self.queue_if_rebuilding(|| PendingOp::DropCollection(collection.to_string())) {
            return;
        }
        self.drop_collection_live(collection);
    }

    fn drop_collection_live(&self, collection: &str) {
        // Positions before columns (the global lock order); membership goes
        // first inside remove_collection, so scoped scans exclude the
        // collection before any bits move.
        let removed = self.positions.remove_collection(collection);
        if removed.is_empty() {
            return;
        }
        let columns = self.columns.read();
        for column in columns.values() {
            let mut bitmaps = column.value_bitmaps.write();
            bitmaps.retain(|_, bm| {
                *bm -= &removed;
                !bm.is_empty()
            });
            column
                .cardinality
                .store(bitmaps.len() as u32, Ordering::Relaxed);
            drop(bitmaps);
            *column.exists_bitmap.write() -= &removed;
        }
        // over_budget may now read stale-high; the 60s persist task's
        // recompute corrects it. hole_count rose by the removed count, so
        // a large drop naturally schedules the >25%-holes compaction
        // rebuild, which reclaims the position space.
    }

    /// Reset ALL accelerator state. Only the rebuild protocol
    /// (`begin_rebuild`) uses this — collection drops go through the
    /// surgical `on_drop_collection` and never disable acceleration.
    pub fn clear(&self) {
        self.positions.clear();
        let mut cols = self.columns.write();
        for column in cols.values_mut() {
            column.value_bitmaps.write().clear();
            *column.exists_bitmap.write() = RoaringBitmap::new();
            column.cardinality.store(0, Ordering::Relaxed);
        }
        self.ready.store(false, Ordering::Release);
        self.over_budget.store(false, Ordering::Relaxed);
        self.profiler.reset();
    }

    /// Enter rebuild mode: queries already fall back (ready flips false),
    /// CRUD hooks switch to the pending queue, and only then are the maps
    /// cleared — nothing can assign a position against a counter that is
    /// about to reset (S2-1).
    pub fn begin_rebuild(&self) {
        self.set_ready(false);
        {
            let mut q = self.pending.lock();
            q.clear();
            self.rebuild_active.store(true, Ordering::Release);
        }
        self.clear();
    }

    /// Drain the deltas that arrived during the rebuild window
    /// (idempotently — the scan may have already indexed some of them),
    /// leave rebuild mode, and start serving. The drain holds the queue
    /// lock, so hooks block for its (bounded) duration and then observe
    /// `rebuild_active == false`; nothing can slip between drain and flag.
    pub fn finish_rebuild(&self) {
        let mut q = self.pending.lock();
        for op in q.drain(..) {
            match op {
                PendingOp::Insert(coll, id, doc) => {
                    // The scan indexes any doc committed before it visited
                    // the doc's key — insert-if-absent keeps one position.
                    if self.positions.get_position(&id).is_none() {
                        self.insert_live(&coll, &id, &doc);
                    }
                }
                // delete_live no-ops when the scan never saw the doc.
                PendingOp::Delete(coll, id, doc) => self.delete_live(&coll, &id, &doc),
                PendingOp::Update(coll, id, old, new) => {
                    if self.positions.get_position(&id).is_some() {
                        // Bitmap ops are idempotent, so this is safe whether
                        // the scan saw the old or the new value.
                        self.update_live(&id, &old, &new);
                    } else {
                        // The scan never saw the doc (inserted mid-scan past
                        // its region, then updated) — index its new state.
                        self.insert_live(&coll, &id, &new);
                    }
                }
                // Idempotent: no-ops on an absent membership entry.
                PendingOp::DropCollection(coll) => self.drop_collection_live(&coll),
            }
        }
        self.rebuild_active.store(false, Ordering::Release);
        drop(q);
        self.set_ready(true);
    }

    /// Rebuild the accelerator from one collection's documents (used by
    /// benchmarks; the server's batched rebuild loop lives in main.rs).
    #[allow(dead_code)]
    pub fn rebuild_from_storage(&self, collection: &str, docs: &[(String, Value)]) {
        self.begin_rebuild();

        let start = std::time::Instant::now();
        self.rebuild_batch(collection, docs);

        let elapsed = start.elapsed();
        let count = docs.len();
        {
            let cols = self.columns.read();
            let col_names: Vec<&str> = cols.keys().map(|s| s.as_str()).collect();
            info!(
                docs = count,
                elapsed_ms = elapsed.as_millis(),
                fields = ?col_names,
                "Scan accelerator rebuilt"
            );
        }

        self.finish_rebuild();
    }

    /// Process one collection's batch during incremental rebuild. Applies
    /// LIVE (this is the rebuild's own feed — it must not queue against
    /// itself). Stops early if the memory budget is exceeded.
    pub fn rebuild_batch(&self, collection: &str, docs: &[(String, Value)]) {
        for (doc_id, doc) in docs {
            if self.over_budget.load(Ordering::Relaxed) {
                return;
            }
            self.insert_live(collection, doc_id, doc);
        }
    }

    // ── Query (bitmap_scan) ─────────────────────────────────────────────

    /// Attempt to resolve a filter entirely or partially via bitmaps,
    /// scoped to `collection`. Returns None if the filter cannot be handled
    /// by bitmaps at all. The position space and value bitmaps are GLOBAL
    /// across collections; the final membership intersection is what keeps
    /// counts, bare-page windows, and aggregate inputs collection-correct
    /// (F1 — un-scoped answers counted and paged other collections' docs).
    pub fn bitmap_scan(&self, collection: &str, filter: &FilterNode) -> Option<BitmapScanResult> {
        if !self.is_ready() {
            return None;
        }
        let mut result = self.bitmap_scan_inner(filter)?;
        self.positions
            .scope_to_collection(&mut result.bitmap, collection);
        Some(result)
    }

    fn bitmap_scan_inner(&self, filter: &FilterNode) -> Option<BitmapScanResult> {
        match filter {
            FilterNode::Comparison {
                field,
                op: FilterOp::Eq,
                value,
            } => {
                let columns = self.columns.read();
                let column = columns.get(field)?;
                let key = value_to_string_key(value);
                let bitmaps = column.value_bitmaps.read();
                let bitmap = bitmaps.get(&key).cloned().unwrap_or_default();
                Some(BitmapScanResult {
                    bitmap,
                    residual_filter: None,
                })
            }

            FilterNode::Comparison {
                field,
                op: FilterOp::Ne,
                value,
            } => {
                let columns = self.columns.read();
                let column = columns.get(field)?;
                let key = value_to_string_key(value);
                let bitmaps = column.value_bitmaps.read();
                let exists = column.exists_bitmap.read().clone();
                let bitmap = match bitmaps.get(&key) {
                    Some(eq_bitmap) => &exists - eq_bitmap,
                    None => exists,
                };
                Some(BitmapScanResult {
                    bitmap,
                    residual_filter: None,
                })
            }

            FilterNode::Comparison {
                field,
                op: FilterOp::In,
                value,
            } => {
                let columns = self.columns.read();
                let column = columns.get(field)?;
                let values = value.as_array()?;
                let bitmaps = column.value_bitmaps.read();
                let mut result = RoaringBitmap::new();
                for v in values {
                    let key = value_to_string_key(v);
                    if let Some(bitmap) = bitmaps.get(&key) {
                        result |= bitmap;
                    }
                }
                Some(BitmapScanResult {
                    bitmap: result,
                    residual_filter: None,
                })
            }

            FilterNode::Comparison {
                field,
                op: FilterOp::Exists,
                value,
            } => {
                let should_exist = value.as_bool().unwrap_or(true);
                let columns = self.columns.read();
                let column = columns.get(field)?;
                let exists = column.exists_bitmap.read().clone();
                if should_exist {
                    Some(BitmapScanResult {
                        bitmap: exists,
                        residual_filter: None,
                    })
                } else {
                    // $exists: false — need all positions minus exists bitmap.
                    // We don't have a "universe" bitmap, so fall back.
                    None
                }
            }

            FilterNode::And(children) => {
                let mut result: Option<RoaringBitmap> = None;
                let mut residual_children: Vec<FilterNode> = Vec::new();

                for child in children {
                    match self.bitmap_scan_inner(child) {
                        Some(BitmapScanResult {
                            bitmap,
                            residual_filter,
                        }) => {
                            result = Some(match result {
                                Some(existing) => existing & &bitmap,
                                None => bitmap,
                            });
                            if let Some(residual) = residual_filter {
                                residual_children.push(residual);
                            }
                        }
                        None => {
                            residual_children.push(child.clone());
                        }
                    }
                }

                let bitmap = result?; // At least one child must be bitmap-resolvable

                let residual = match residual_children.len() {
                    0 => None,
                    1 => Some(residual_children.into_iter().next().unwrap()),
                    _ => Some(FilterNode::And(residual_children)),
                };

                Some(BitmapScanResult {
                    bitmap,
                    residual_filter: residual,
                })
            }

            FilterNode::Or(children) => {
                // A $or is bitmap-servable ONLY with full coverage: the plan's
                // residual_filter is applied as a CONJUNCTION over bitmap-matched
                // docs, so a partially-covered $or would execute as an intersection
                // (covered-arm docs post-filtered by the uncovered arm; uncovered-arm
                // docs never loaded). Partial coverage bails to the fallback
                // strategies; per-arm union planning is H-P3.1 territory.
                if children.is_empty() {
                    return None;
                }
                let mut bitmap_result = RoaringBitmap::new();
                for child in children {
                    match self.bitmap_scan_inner(child) {
                        Some(BitmapScanResult {
                            bitmap,
                            residual_filter: None,
                        }) => {
                            bitmap_result |= &bitmap;
                        }
                        _ => return None,
                    }
                }
                Some(BitmapScanResult {
                    bitmap: bitmap_result,
                    residual_filter: None,
                })
            }

            // $not and other ops — not bitmap-eligible
            _ => None,
        }
    }

    // ── Aggregation helpers ─────────────────────────────────────────────

    /// Count documents per value for a bitmap field within `collection`
    /// (for $group + $count aggregation). Values with no documents in this
    /// collection are omitted — their bits belong to other collections.
    /// Returns None if the field doesn't have a bitmap column.
    pub fn count_by_field(&self, collection: &str, field: &str) -> Option<Vec<(String, u64)>> {
        let columns = self.columns.read();
        let column = columns.get(field)?;
        let bitmaps = column.value_bitmaps.read();
        let mut result = Vec::new();
        for (value, bitmap) in bitmaps.iter() {
            let count = self.positions.scoped_len(bitmap, collection);
            if count > 0 {
                result.push((value.clone(), count));
            }
        }
        Some(result)
    }

    /// Count documents per value for a bitmap field, filtered by a match bitmap.
    /// For $match + $group + $count aggregation. The caller must pass a
    /// bitmap that is already collection-scoped (i.e. one produced by
    /// `bitmap_scan`) — the per-value intersections inherit its scope.
    pub fn count_by_field_filtered(
        &self,
        field: &str,
        match_bitmap: &RoaringBitmap,
    ) -> Option<Vec<(String, u64)>> {
        let columns = self.columns.read();
        let column = columns.get(field)?;
        let bitmaps = column.value_bitmaps.read();
        let mut result = Vec::new();
        for (value, bitmap) in bitmaps.iter() {
            let count = (bitmap & match_bitmap).len();
            if count > 0 {
                result.push((value.clone(), count));
            }
        }
        Some(result)
    }

    /// Get a column reference for a field (checks if it exists).
    pub fn has_column(&self, field: &str) -> bool {
        self.columns.read().contains_key(field)
    }

    // ── Stats ───────────────────────────────────────────────────────────

    pub fn stats(&self) -> AcceleratorStats {
        // Single `columns` guard scope: calling total_memory_bytes() while
        // holding it re-acquires `columns.read()`, and parking_lot readers
        // queued behind a waiting writer (configure_fields / auto-detect /
        // clear / load_from_disk) deadlock on re-entry. Sum column memory in
        // the same pass instead and touch every other lock after the guard
        // drops.
        let mut column_stats = Vec::new();
        let mut columns_memory = 0usize;
        {
            let columns = self.columns.read();
            for (field, column) in columns.iter() {
                let memory_bytes = column.memory_bytes();
                columns_memory += memory_bytes;
                column_stats.push(ColumnStat {
                    field: field.clone(),
                    cardinality: column.cardinality.load(Ordering::Relaxed),
                    memory_bytes,
                });
            }
        }
        let mut collection_positions: Vec<CollectionPositions> = self
            .positions
            .by_collection
            .read()
            .iter()
            .map(|(name, bm)| CollectionPositions {
                collection: name.clone(),
                positions: bm.len(),
            })
            .collect();
        collection_positions.sort_by(|a, b| a.collection.cmp(&b.collection));
        AcceleratorStats {
            ready: self.is_ready(),
            total_positions: self.positions.len(),
            collections: collection_positions,
            columns: column_stats,
            memory_bytes: columns_memory + self.positions.memory_bytes(),
            memory_budget_bytes: self.config.read().max_memory_bytes,
            over_budget: self.over_budget.load(Ordering::Relaxed),
        }
    }

    #[allow(dead_code)]
    pub fn profiler(&self) -> &CardinalityProfiler {
        &self.profiler
    }

    // ── Disk Persistence ────────────────────────────────────────────────

    /// Persist all bitmaps and position map to disk.
    pub fn persist_to_disk(&self, data_dir: &Path, collection: &str) -> Result<(), std::io::Error> {
        if !self.is_ready() || !self.has_columns() {
            return Ok(());
        }

        let bitmap_dir = data_dir.join("bitmap").join(collection);
        fs::create_dir_all(&bitmap_dir)?;

        // Snapshot everything we need under the shortest possible guard scopes,
        // then drop all guards before any blocking I/O. Earlier revisions held
        // reader guards on `pos_to_id` + `id_to_pos` + `columns` + per-column
        // `value_bitmaps` + `exists_bitmap` across `fs::write` calls. Under
        // parking_lot writer-fairness, a queued writer from `on_insert` /
        // `on_update` would cause every subsequent reader in this function to
        // block, producing a hard process-wide deadlock under sustained ingest.
        //
        // Peak transient allocation at 2M docs × ~11 fields: ~300-400 MB
        // (dominated by the positions Vec + HashMap clones). Budget for this
        // if pushing past 10M docs on small hosts.
        // Both position guards held TOGETHER (S2-3): the id↔position pair is
        // cloned at one instant and can never disagree with itself on disk.
        // Acquisition order matches `assign`/`remove`/`clear`/`memory_bytes`
        // (id_to_pos first, then pos_to_id) — one global order, no ABBA.
        // Column bits are cloned after the pair drops: a position added in
        // the gap lacks bits in the snapshot (benign — resolve skips it);
        // the reverse direction can't happen because every hook takes
        // positions before columns.
        let (id_map_snapshot, pos_vec_snapshot, next_pos) = {
            let id_guard = self.positions.id_to_pos.read();
            let pos_guard = self.positions.pos_to_id.read();
            (
                id_guard.clone(),
                pos_guard.clone(),
                self.positions.next_pos.load(Ordering::Relaxed),
            )
        };

        // Membership snapshots after the pair drops (same benign-skew
        // analysis as columns: a doc inserted in the gap is in membership
        // but has no id/bits — resolve skips it and no value bitmap counts
        // it; a doc deleted in the gap is correctly absent from membership).
        let membership_snapshot: Vec<(String, RoaringBitmap)> = {
            let members = self.positions.by_collection.read();
            members
                .iter()
                .map(|(name, bm)| (name.clone(), bm.clone()))
                .collect()
        };

        let columns_snapshot: Vec<ColumnSnapshot> = {
            let cols = self.columns.read();
            cols.iter()
                .map(|(field, col)| ColumnSnapshot {
                    field_path: field.clone(),
                    cardinality: col.cardinality.load(Ordering::Relaxed),
                    value_bitmaps: col.value_bitmaps.read().clone(),
                    exists_bitmap: col.exists_bitmap.read().clone(),
                })
                .collect()
        };

        // From here on, no RwLock guards are held. All I/O runs lock-free.

        let mut membership_meta = Vec::new();
        for (i, (coll, bm)) in membership_snapshot.iter().enumerate() {
            let mut bytes = Vec::new();
            bm.serialize_into(&mut bytes)
                .map_err(std::io::Error::other)?;
            let filename = format!("membership_{i}.roaring");
            fs::write(bitmap_dir.join(&filename), &bytes)?;
            membership_meta.push(serde_json::json!({"collection": coll, "file": filename}));
        }

        let meta = serde_json::json!({
            "format": BITMAP_SNAPSHOT_FORMAT,
            "next_pos": next_pos,
            "count": pos_vec_snapshot.len(),
            "memberships": membership_meta,
        });
        fs::write(
            bitmap_dir.join("positions.meta.json"),
            serde_json::to_string_pretty(&meta).unwrap_or_default(),
        )?;

        // Serde's `Arc<T>` Serialize impl is gated behind the `rc` feature,
        // which we don't enable. Convert to borrowed `&str` references at
        // serialize time — cheap, no allocations, and produces the same
        // JSON bytes as the previous implementation.
        let pos_entries: Vec<Option<&str>> = pos_vec_snapshot
            .iter()
            .map(|slot| slot.as_deref())
            .collect();
        fs::write(
            bitmap_dir.join("positions.map.json"),
            serde_json::to_string(&pos_entries).unwrap_or_default(),
        )?;

        let id_entries: HashMap<&str, u32> =
            id_map_snapshot.iter().map(|(k, v)| (&**k, *v)).collect();
        fs::write(
            bitmap_dir.join("positions.ids.json"),
            serde_json::to_string(&id_entries).unwrap_or_default(),
        )?;

        let mut columns_meta = Vec::new();
        for snapshot in &columns_snapshot {
            let safe_field = snapshot.field_path.replace('.', "_DOT_");

            // Write exists bitmap
            let mut exists_bytes = Vec::new();
            snapshot
                .exists_bitmap
                .serialize_into(&mut exists_bytes)
                .map_err(std::io::Error::other)?;
            fs::write(
                bitmap_dir.join(format!("{safe_field}.exists.roaring")),
                &exists_bytes,
            )?;

            // Write each value bitmap
            let mut value_keys = Vec::new();
            for (i, (value_key, bitmap)) in snapshot.value_bitmaps.iter().enumerate() {
                let mut bitmap_bytes = Vec::new();
                bitmap
                    .serialize_into(&mut bitmap_bytes)
                    .map_err(std::io::Error::other)?;
                let filename = format!("{safe_field}_v{i}.roaring");
                fs::write(bitmap_dir.join(&filename), &bitmap_bytes)?;
                value_keys.push((value_key.clone(), filename));
            }

            columns_meta.push(serde_json::json!({
                "field_path": &snapshot.field_path,
                "safe_field": safe_field,
                "cardinality": snapshot.cardinality,
                "values": value_keys.iter().map(|(k, f)| serde_json::json!({"key": k, "file": f})).collect::<Vec<_>>(),
            }));
        }

        let columns_meta_json = serde_json::json!({ "columns": columns_meta });
        fs::write(
            bitmap_dir.join("columns.meta.json"),
            serde_json::to_string_pretty(&columns_meta_json).unwrap_or_default(),
        )?;

        Ok(())
    }

    /// S2-2 restart reconciliation: a persisted snapshot is at most one
    /// persist-interval stale, so docs written after the last persist are
    /// silently absent from it — serving it unreconciled means
    /// false-negative query results until an unrelated >25%-hole rebuild.
    /// Checked PER COLLECTION against storage's counts (F1 made the
    /// per-collection grain available): a total that happens to balance
    /// across collections can no longer mask skew, and a snapshot carrying
    /// membership for a since-dropped collection is rejected too. On any
    /// mismatch the caller must rebuild from storage instead of serving.
    pub fn snapshot_matches(&self, expected: &HashMap<String, u64>) -> bool {
        let members = self.positions.by_collection.read();
        for (coll, want) in expected {
            let have = members.get(coll.as_str()).map(|m| m.len()).unwrap_or(0);
            if have != *want {
                return false;
            }
        }
        members
            .iter()
            .all(|(coll, m)| m.is_empty() || expected.contains_key(coll))
    }

    /// Try to load bitmaps from disk. Returns true on success.
    pub fn load_from_disk(&self, data_dir: &Path, collection: &str) -> bool {
        let bitmap_dir = data_dir.join("bitmap").join(collection);
        if !bitmap_dir.exists() {
            return false;
        }

        // Load position metadata
        let meta_path = bitmap_dir.join("positions.meta.json");
        let meta_str = match fs::read_to_string(&meta_path) {
            Ok(s) => s,
            Err(_) => return false,
        };
        let meta: Value = match serde_json::from_str(&meta_str) {
            Ok(v) => v,
            Err(_) => return false,
        };
        // Reject other formats (pre-format snapshots default to 1): their
        // value keys don't match what the query side now produces, so
        // serving them would silently return wrong results. The caller
        // rebuilds from storage instead.
        let format = meta.get("format").and_then(|v| v.as_u64()).unwrap_or(1);
        if format != BITMAP_SNAPSHOT_FORMAT {
            info!(
                found = format,
                expected = BITMAP_SNAPSHOT_FORMAT,
                "Bitmap snapshot format outdated; rebuilding from storage"
            );
            return false;
        }
        let next_pos = meta.get("next_pos").and_then(|v| v.as_u64()).unwrap_or(0) as u32;

        // Load position map (JSON — variable-length string IDs). The
        // pre-format binary fallback died with the v2 gate: any snapshot
        // old enough to carry .bin files reports format 1 and was refused
        // above.
        let json_data = match fs::read_to_string(bitmap_dir.join("positions.map.json")) {
            Ok(d) => d,
            Err(_) => return false,
        };
        let pos_vec: Vec<Option<String>> = match serde_json::from_str(&json_data) {
            Ok(v) => v,
            Err(_) => return false,
        };
        let id_json = match fs::read_to_string(bitmap_dir.join("positions.ids.json")) {
            Ok(d) => d,
            Err(_) => return false,
        };
        let id_map: HashMap<String, u32> = match serde_json::from_str(&id_json) {
            Ok(v) => v,
            Err(_) => return false,
        };

        // Membership map (v3): refuse a snapshot without it — serving one
        // would leave every collection scoped to empty.
        let mut memberships: HashMap<String, RoaringBitmap> = HashMap::new();
        let entries = match meta.get("memberships").and_then(|v| v.as_array()) {
            Some(a) => a,
            None => return false,
        };
        for entry in entries {
            let (Some(coll), Some(file)) = (
                entry.get("collection").and_then(|v| v.as_str()),
                entry.get("file").and_then(|v| v.as_str()),
            ) else {
                return false;
            };
            let bytes = match fs::read(bitmap_dir.join(file)) {
                Ok(d) => d,
                Err(_) => return false,
            };
            let bm = match RoaringBitmap::deserialize_from(&bytes[..]) {
                Ok(b) => b,
                Err(_) => return false,
            };
            memberships.insert(coll.to_string(), bm);
        }

        // Install position data (convert String → Arc<str>)
        *self.positions.id_to_pos.write() = id_map
            .into_iter()
            .map(|(k, v)| (Arc::from(k.as_str()), v))
            .collect();
        *self.positions.pos_to_id.write() = pos_vec
            .into_iter()
            .map(|opt| opt.map(|s| Arc::from(s.as_str())))
            .collect();
        *self.positions.by_collection.write() = memberships;
        self.positions.next_pos.store(next_pos, Ordering::Relaxed);

        // Load columns metadata
        let cols_meta_str = match fs::read_to_string(bitmap_dir.join("columns.meta.json")) {
            Ok(s) => s,
            Err(_) => return false,
        };
        let cols_meta: Value = match serde_json::from_str(&cols_meta_str) {
            Ok(v) => v,
            Err(_) => return false,
        };

        let cols_arr = match cols_meta.get("columns").and_then(|v| v.as_array()) {
            Some(a) => a,
            None => return false,
        };

        let mut columns = self.columns.write();

        for col_meta in cols_arr {
            let field_path = match col_meta.get("field_path").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let safe_field = match col_meta.get("safe_field").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let cardinality = col_meta
                .get("cardinality")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;

            // Load exists bitmap
            let exists_bytes =
                match fs::read(bitmap_dir.join(format!("{safe_field}.exists.roaring"))) {
                    Ok(d) => d,
                    Err(_) => continue,
                };
            let exists_bitmap = match RoaringBitmap::deserialize_from(&exists_bytes[..]) {
                Ok(b) => b,
                Err(_) => continue,
            };

            // Load value bitmaps
            let values = match col_meta.get("values").and_then(|v| v.as_array()) {
                Some(a) => a,
                None => continue,
            };

            let mut value_bitmaps = HashMap::new();
            for val_entry in values {
                let key = match val_entry.get("key").and_then(|v| v.as_str()) {
                    Some(s) => s.to_string(),
                    None => continue,
                };
                let file = match val_entry.get("file").and_then(|v| v.as_str()) {
                    Some(s) => s,
                    None => continue,
                };
                let bitmap_bytes = match fs::read(bitmap_dir.join(file)) {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                let bitmap = match RoaringBitmap::deserialize_from(&bitmap_bytes[..]) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                value_bitmaps.insert(key, bitmap);
            }

            let column = columns
                .entry(field_path.clone())
                .or_insert_with(|| BitmapColumn::new(field_path));
            *column.value_bitmaps.write() = value_bitmaps;
            *column.exists_bitmap.write() = exists_bitmap;
            column.cardinality.store(cardinality, Ordering::Relaxed);
        }

        info!(
            collection = collection,
            positions = next_pos,
            columns = cols_arr.len(),
            "Scan accelerator loaded from disk"
        );

        true
    }
}

// ── AcceleratorStats ────────────────────────────────────────────────────────

pub struct AcceleratorStats {
    pub ready: bool,
    pub total_positions: u32,
    /// Live positions per collection (the F1 scoping data, surfaced so a
    /// deployment can verify per-collection counts against `/_collections`).
    pub collections: Vec<CollectionPositions>,
    pub columns: Vec<ColumnStat>,
    pub memory_bytes: usize,
    pub memory_budget_bytes: u64,
    pub over_budget: bool,
}

pub struct CollectionPositions {
    pub collection: String,
    pub positions: u64,
}

pub struct ColumnStat {
    pub field: String,
    pub cardinality: u32,
    pub memory_bytes: usize,
}

// ── CardinalityProfiler ─────────────────────────────────────────────────────

/// Profiles field cardinality during the first N inserts to auto-detect
/// which fields are suitable for bitmap tracking.
pub struct CardinalityProfiler {
    /// field_path -> set of observed distinct values
    observed: RwLock<HashMap<String, HashSet<String>>>,
    sample_count: AtomicU32,
    sample_target: AtomicU32,
    done: AtomicBool,
    /// If true, skip profiling (fields were explicitly configured)
    skip: bool,
}

impl CardinalityProfiler {
    pub fn new(skip: bool) -> Self {
        CardinalityProfiler {
            observed: RwLock::new(HashMap::new()),
            sample_count: AtomicU32::new(0),
            sample_target: AtomicU32::new(10_000),
            done: AtomicBool::new(skip),
            skip,
        }
    }

    /// Set how many inserts to profile before detection completes
    /// (`--bitmap-sample-size`). Was a no-op placeholder until the flag was
    /// wired up; a zero target is clamped to 1 so detection still terminates.
    pub fn set_sample_target(&self, target: u32) {
        self.sample_target.store(target.max(1), Ordering::Relaxed);
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }

    pub fn reset(&self) {
        if !self.skip {
            self.observed.write().clear();
            self.sample_count.store(0, Ordering::Relaxed);
            self.done.store(false, Ordering::Relaxed);
        }
    }

    /// Observe a document during the profiling phase.
    fn observe(&self, doc: &Value) {
        if self.done.load(Ordering::Relaxed) {
            return;
        }

        let count = self.sample_count.fetch_add(1, Ordering::Relaxed) + 1;

        if let Some(obj) = doc.as_object() {
            let mut observed = self.observed.write();
            for (key, value) in obj {
                // Skip system fields
                if key.starts_with('_') {
                    continue;
                }

                // Top-level field
                let val_key = value_to_string_key(value);
                observed.entry(key.clone()).or_default().insert(val_key);

                // One level of nesting
                if let Value::Object(inner) = value {
                    for (inner_key, inner_value) in inner {
                        if inner_key.starts_with('_') {
                            continue;
                        }
                        let path = format!("{key}.{inner_key}");
                        let val_key = value_to_string_key(inner_value);
                        observed.entry(path).or_default().insert(val_key);
                    }
                }
            }
        }

        if count >= self.sample_target.load(Ordering::Relaxed) {
            self.done.store(true, Ordering::Relaxed);
        }
    }

    /// Analyze profiled data and return fields suitable for bitmap tracking.
    pub fn analyze(&self, max_cardinality: u32) -> Vec<(String, u32)> {
        let observed = self.observed.read();
        let mut results: Vec<(String, u32)> = Vec::new();

        for (field, values) in observed.iter() {
            let card = values.len() as u32;
            if card > 0 && card < max_cardinality {
                results.push((field.clone(), card));
            } else if card >= max_cardinality {
                info!(
                    field = field,
                    cardinality = card,
                    "Skipping bitmap for high-cardinality field"
                );
            }
        }

        // Sort by cardinality ascending (most selective first)
        results.sort_by_key(|(_, c)| *c);
        results
    }

    /// Consume the profiler's observed data (frees memory).
    pub fn finish(&self) {
        self.observed.write().clear();
        self.done.store(true, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn sample_count(&self) -> u32 {
        self.sample_count.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn accel(fields: &[&str]) -> ScanAccelerator {
        let a = ScanAccelerator::new(AcceleratorConfig {
            bitmap_fields: fields.iter().map(|s| s.to_string()).collect(),
            max_cardinality: 1000,
            max_memory_bytes: 0,
        });
        a.set_ready(true);
        a
    }

    fn exists_len(a: &ScanAccelerator, field: &str) -> u64 {
        a.columns
            .read()
            .get(field)
            .unwrap()
            .exists_bitmap
            .read()
            .len()
    }

    fn eq_filter(field: &str, value: Value) -> crate::query::filter::FilterNode {
        crate::query::filter::FilterNode::Comparison {
            field: field.to_string(),
            op: crate::query::filter::FilterOp::Eq,
            value,
        }
    }

    /// S2-1: CRUD hooks firing during a rebuild land in the pending queue —
    /// never in the maps the rebuild is clearing — and the drain applies
    /// them idempotently before the accelerator serves again.
    #[test]
    fn rebuild_queues_hooks_and_drains_idempotently() {
        let a = accel(&["f"]);
        a.on_insert("c1", "a", &json!({"f": "x"}));
        a.on_insert("c1", "b", &json!({"f": "y"}));

        a.begin_rebuild();
        assert!(!a.is_ready());

        // Concurrent-writer stand-ins while the rebuild owns the maps:
        // a brand-new doc the scan will never see, a delete of a doc the
        // scan WILL see, and an update of a doc the scan sees (new value).
        a.on_insert("c1", "c", &json!({"f": "x"}));
        a.on_delete("c1", "b", &json!({"f": "y"}));
        a.on_update("c1", "a", &json!({"f": "x"}), &json!({"f": "z"}));
        assert_eq!(
            a.positions.len(),
            0,
            "hooks must not touch the cleared maps mid-rebuild"
        );

        // The "scan": storage at visit time still shows a (already updated
        // to z — scans read post-commit state) and b (its delete raced in
        // after the scan passed it).
        a.rebuild_batch(
            "c1",
            &[
                ("a".to_string(), json!({"f": "z"})),
                ("b".to_string(), json!({"f": "y"})),
            ],
        );
        a.finish_rebuild();
        assert!(a.is_ready());

        // a: seen by scan, update drained idempotently (stays at z, once).
        // b: drained delete removed it. c: drained insert added it.
        assert_eq!(a.positions.len(), 2);
        assert!(a.positions.get_position("a").is_some());
        assert!(a.positions.get_position("b").is_none());
        assert!(a.positions.get_position("c").is_some());

        let z = a.bitmap_scan("c1", &eq_filter("f", json!("z"))).unwrap();
        assert_eq!(z.bitmap.len(), 1, "a counted exactly once under z");
        let x = a.bitmap_scan("c1", &eq_filter("f", json!("x"))).unwrap();
        assert_eq!(x.bitmap.len(), 1, "c counted exactly once under x");
        let y = a.bitmap_scan("c1", &eq_filter("f", json!("y"))).unwrap();
        assert_eq!(y.bitmap.len(), 0, "b fully gone");
    }

    /// S2-1 dedup: a doc whose insert-hook fired mid-rebuild AND was seen by
    /// the scan (committed before the scan reached it) gets exactly one
    /// position — the drain inserts only if absent.
    #[test]
    fn rebuild_drain_dedups_scan_seen_insert() {
        let a = accel(&["f"]);
        a.begin_rebuild();
        a.on_insert("c1", "dup", &json!({"f": "x"}));
        a.rebuild_batch("c1", &[("dup".to_string(), json!({"f": "x"}))]);
        a.finish_rebuild();

        assert_eq!(a.positions.len(), 1);
        let x = a.bitmap_scan("c1", &eq_filter("f", json!("x"))).unwrap();
        assert_eq!(x.bitmap.len(), 1, "one position, one bit");
    }

    /// S2-4: value keys are collision-free across types and decode back
    /// exactly. The old untagged keys collapsed number 123 onto string
    /// "123" (and null onto the string "__null__"), cross-matching bitmap
    /// eq scans and re-typing aggregate group keys.
    #[test]
    fn value_keys_round_trip_and_stay_type_disjoint() {
        let corpus = [
            json!(null),
            json!(true),
            json!(false),
            json!(123),
            json!(-1.5),
            json!("123"),
            json!("__null__"),
            json!("true"),
            json!(""),
            json!([1, 2]),
            json!({"k": "v"}),
        ];
        let mut keys = std::collections::HashSet::new();
        for v in &corpus {
            let key = value_to_string_key(v);
            assert!(keys.insert(key.clone()), "key collision for {v}: {key}");
            assert_eq!(&string_key_to_value(&key), v, "round trip for {v}");
        }
    }

    /// S2-4 (live half): a bitmap eq scan on the string "123" must not
    /// match documents holding the number 123, and vice versa.
    #[test]
    fn bitmap_eq_does_not_cross_match_types() {
        let a = accel(&["f"]);
        a.on_insert("c1", "num", &json!({"f": 123}));
        a.on_insert("c1", "str", &json!({"f": "123"}));
        a.on_insert("c1", "nul", &json!({"f": null}));
        a.on_insert("c1", "marker", &json!({"f": "__null__"}));

        let s = a.bitmap_scan("c1", &eq_filter("f", json!("123"))).unwrap();
        assert_eq!(s.bitmap.len(), 1, "string eq matches only the string doc");
        let n = a.bitmap_scan("c1", &eq_filter("f", json!(123))).unwrap();
        assert_eq!(n.bitmap.len(), 1, "number eq matches only the number doc");
        let z = a.bitmap_scan("c1", &eq_filter("f", json!(null))).unwrap();
        assert_eq!(z.bitmap.len(), 1, "null eq matches only the null doc");
        let m = a
            .bitmap_scan("c1", &eq_filter("f", json!("__null__")))
            .unwrap();
        assert_eq!(
            m.bitmap.len(),
            1,
            "the old sentinel string is just a string"
        );
    }

    /// S2-2: a reloaded snapshot only passes reconciliation when its
    /// position count matches storage's document count — a doc written
    /// after the last persist makes the snapshot detectably stale.
    #[test]
    fn stale_snapshot_fails_reconciliation_on_load() {
        let tmp = tempfile::TempDir::new().unwrap();
        let a = accel(&["f"]);
        a.on_insert("c1", "a", &json!({"f": "x"}));
        a.on_insert("c1", "b", &json!({"f": "y"}));
        a.persist_to_disk(tmp.path(), "_all").unwrap();
        // Written after the persist — the snapshot no longer covers storage.
        a.on_insert("c1", "c", &json!({"f": "x"}));

        let restarted = accel(&["f"]);
        assert!(restarted.load_from_disk(tmp.path(), "_all"));
        let expect = |n: u64| HashMap::from([("c1".to_string(), n)]);
        assert!(
            restarted.snapshot_matches(&expect(2)),
            "snapshot agrees with its own era"
        );
        assert!(
            !restarted.snapshot_matches(&expect(3)),
            "snapshot must fail reconciliation against post-persist storage"
        );
        assert!(
            !restarted.snapshot_matches(&HashMap::new()),
            "membership for a since-dropped collection must fail reconciliation"
        );
    }

    /// The invariant that lets on_delete keep the exists write lock inside
    /// the field-present guard: exists_bitmap never contains positions of
    /// docs lacking the field, so absent fields have nothing to remove.
    #[test]
    fn on_delete_missing_field_leaves_other_columns_alone() {
        let a = accel(&["kind", "tier"]);
        a.on_insert("c1", "doc-1", &json!({"kind": "a"})); // no tier
        a.on_insert("c1", "doc-2", &json!({"kind": "b", "tier": "x"}));
        assert_eq!(exists_len(&a, "kind"), 2);
        assert_eq!(exists_len(&a, "tier"), 1);

        a.on_delete("c1", "doc-1", &json!({"kind": "a"}));
        assert_eq!(exists_len(&a, "kind"), 1);
        assert_eq!(exists_len(&a, "tier"), 1); // untouched — doc-1 had none
    }

    /// An update that doesn't change a column's value must leave its bitmaps
    /// byte-identical (the zero-allocation fast path is a pure no-op).
    #[test]
    fn on_update_unchanged_column_is_noop() {
        let a = accel(&["kind", "tier"]);
        a.on_insert("c1", "doc-1", &json!({"kind": "a", "tier": "x"}));
        a.on_insert("c1", "doc-2", &json!({"kind": "a", "tier": "y"}));

        // Same values on both sides for kind; only tier changes.
        a.on_update(
            "c1",
            "doc-1",
            &json!({"kind": "a", "tier": "x"}),
            &json!({"kind": "a", "tier": "z"}),
        );

        let key = |v: &str| value_to_string_key(&json!(v));
        let cols = a.columns.read();
        let kind = cols.get("kind").unwrap();
        assert_eq!(kind.cardinality.load(Ordering::Relaxed), 1);
        assert_eq!(kind.value_bitmaps.read().get(&key("a")).unwrap().len(), 2);
        let tier = cols.get("tier").unwrap();
        let tiers = tier.value_bitmaps.read();
        assert!(!tiers.contains_key(&key("x")));
        assert_eq!(tiers.get(&key("z")).unwrap().len(), 1);
        assert_eq!(tiers.get(&key("y")).unwrap().len(), 1);
    }

    /// set_max_cardinality must be visible to subsequent inserts (the hot
    /// path reads the atomic cache, not the config lock).
    #[test]
    fn set_max_cardinality_applies_to_next_insert() {
        let a = accel(&["kind"]);
        a.set_max_cardinality(2);
        a.on_insert("c1", "d1", &json!({"kind": "a"}));
        a.on_insert("c1", "d2", &json!({"kind": "b"}));
        a.on_insert("c1", "d3", &json!({"kind": "c"})); // over the cap — not tracked

        let cols = a.columns.read();
        let kind = cols.get("kind").unwrap();
        assert_eq!(kind.cardinality.load(Ordering::Relaxed), 2);
        assert!(!kind.value_bitmaps.read().contains_key("c"));
        // Presence is still tracked past the cap.
        assert_eq!(kind.exists_bitmap.read().len(), 3);
    }

    /// resolve_window windows over resolved ids (holes from deletes are
    /// skipped without consuming the window) in ascending position order.
    #[test]
    fn resolve_window_skips_holes_without_consuming_window() {
        let a = accel(&["kind"]);
        for i in 0..5 {
            a.on_insert("c1", &format!("doc-{i}"), &json!({"kind": "a"}));
        }
        a.on_delete("c1", "doc-2", &json!({"kind": "a"})); // hole at position 2

        let all: RoaringBitmap = (0u32..5).collect();
        let ids = a.positions.resolve_window(&all, 0, usize::MAX);
        assert_eq!(
            ids.iter().map(|s| s.as_ref()).collect::<Vec<_>>(),
            ["doc-0", "doc-1", "doc-3", "doc-4"]
        );

        let ids = a.positions.resolve_window(&all, 1, 2);
        assert_eq!(
            ids.iter().map(|s| s.as_ref()).collect::<Vec<_>>(),
            ["doc-1", "doc-3"]
        );
    }

    /// F1: the position space and value bitmaps are global across
    /// collections, so every answer must be intersected with the queried
    /// collection's membership — un-scoped answers counted (and bare pages
    /// ghosted) other collections' documents.
    #[test]
    fn bitmap_scan_is_collection_scoped() {
        let a = accel(&["kind"]);
        a.on_insert("c1", "a1", &json!({"kind": "x"}));
        a.on_insert("c1", "a2", &json!({"kind": "x"}));
        a.on_insert("c2", "b1", &json!({"kind": "x"}));

        let c1 = a.bitmap_scan("c1", &eq_filter("kind", json!("x"))).unwrap();
        assert_eq!(c1.bitmap.len(), 2);
        let c2 = a.bitmap_scan("c2", &eq_filter("kind", json!("x"))).unwrap();
        assert_eq!(c2.bitmap.len(), 1);
        let none = a
            .bitmap_scan("absent", &eq_filter("kind", json!("x")))
            .unwrap();
        assert_eq!(none.bitmap.len(), 0, "unknown collection scopes to empty");

        // Deletes maintain membership, not just the value bitmaps.
        a.on_delete("c1", "a2", &json!({"kind": "x"}));
        let c1 = a.bitmap_scan("c1", &eq_filter("kind", json!("x"))).unwrap();
        assert_eq!(c1.bitmap.len(), 1);
    }

    /// F1c: aggregate group counts are per-collection; values with no
    /// documents in the queried collection are omitted entirely.
    #[test]
    fn count_by_field_is_collection_scoped() {
        let a = accel(&["kind"]);
        a.on_insert("c1", "a1", &json!({"kind": "x"}));
        a.on_insert("c2", "b1", &json!({"kind": "x"}));
        a.on_insert("c2", "b2", &json!({"kind": "y"}));

        let mut c2 = a.count_by_field("c2", "kind").unwrap();
        c2.sort();
        assert_eq!(c2, vec![("sx".to_string(), 1), ("sy".to_string(), 1)]);
        let c1 = a.count_by_field("c1", "kind").unwrap();
        assert_eq!(
            c1,
            vec![("sx".to_string(), 1)],
            "y omitted — zero docs in c1"
        );
    }

    /// F1d: dropping a collection removes exactly its positions and bits —
    /// other collections stay accelerated and ready never flips.
    #[test]
    fn drop_collection_is_surgical() {
        let a = accel(&["kind"]);
        a.on_insert("c1", "a1", &json!({"kind": "x"}));
        a.on_insert("c2", "b1", &json!({"kind": "x"}));
        a.on_insert("c2", "b2", &json!({"kind": "y"}));

        a.on_drop_collection("c2");

        assert!(a.is_ready(), "drop must not disable acceleration");
        assert_eq!(a.positions.len(), 1, "only c1's position survives");
        assert!(a.positions.get_position("b1").is_none());
        let c2 = a.bitmap_scan("c2", &eq_filter("kind", json!("x"))).unwrap();
        assert_eq!(c2.bitmap.len(), 0, "dropped collection scans empty");
        let c1 = a.bitmap_scan("c1", &eq_filter("kind", json!("x"))).unwrap();
        assert_eq!(c1.bitmap.len(), 1, "surviving collection intact");

        // "y" existed only in c2: its bitmap emptied and was removed, so
        // cardinality reflects live values only.
        let cols = a.columns.read();
        let kind = cols.get("kind").unwrap();
        assert_eq!(kind.cardinality.load(Ordering::Relaxed), 1);
        drop(cols);

        // Idempotent.
        a.on_drop_collection("c2");
        assert_eq!(a.positions.len(), 1);
    }

    /// A drop_collection landing mid-rebuild queues like any other delta
    /// and drains after the scan — cleaning up whatever the scan indexed
    /// for the dropped collection before the drop arrived.
    #[test]
    fn drop_during_rebuild_queues_and_drains() {
        let a = accel(&["kind"]);
        a.begin_rebuild();
        a.rebuild_batch("c1", &[("a1".to_string(), json!({"kind": "x"}))]);
        a.rebuild_batch("c2", &[("b1".to_string(), json!({"kind": "x"}))]);
        a.on_drop_collection("c1"); // queued — not applied yet
        assert_eq!(a.positions.len(), 2, "drop is queued during rebuild");
        a.finish_rebuild();

        assert!(a.is_ready());
        assert_eq!(a.positions.len(), 1);
        assert!(a.positions.get_position("a1").is_none());
        let c2 = a.bitmap_scan("c2", &eq_filter("kind", json!("x"))).unwrap();
        assert_eq!(c2.bitmap.len(), 1);
    }

    /// v3 snapshots round-trip the membership map: scoped answers and the
    /// per-collection reconcile both work on a freshly loaded accelerator.
    #[test]
    fn membership_persists_and_reloads_v3() {
        let tmp = tempfile::TempDir::new().unwrap();
        let a = accel(&["kind"]);
        a.on_insert("c1", "a1", &json!({"kind": "x"}));
        a.on_insert("c2", "b1", &json!({"kind": "x"}));
        a.on_insert("c2", "b2", &json!({"kind": "y"}));
        a.persist_to_disk(tmp.path(), "_all").unwrap();

        let r = accel(&["kind"]);
        assert!(r.load_from_disk(tmp.path(), "_all"));
        let c1 = r.bitmap_scan("c1", &eq_filter("kind", json!("x"))).unwrap();
        assert_eq!(c1.bitmap.len(), 1);
        let c2 = r.bitmap_scan("c2", &eq_filter("kind", json!("x"))).unwrap();
        assert_eq!(c2.bitmap.len(), 1);
        let expected = HashMap::from([("c1".to_string(), 1u64), ("c2".to_string(), 2u64)]);
        assert!(r.snapshot_matches(&expected));
    }

    /// Pre-v3 snapshots (no membership map) are refused on load, routing
    /// startup through a storage rebuild — the S2-4 format-gate precedent.
    #[test]
    fn pre_v3_snapshot_is_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        let a = accel(&["kind"]);
        a.on_insert("c1", "a1", &json!({"kind": "x"}));
        a.persist_to_disk(tmp.path(), "_all").unwrap();

        let meta_path = tmp
            .path()
            .join("bitmap")
            .join("_all")
            .join("positions.meta.json");
        let mut meta: Value =
            serde_json::from_str(&fs::read_to_string(&meta_path).unwrap()).unwrap();
        meta["format"] = json!(2);
        fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();

        let r = accel(&["kind"]);
        assert!(
            !r.load_from_disk(tmp.path(), "_all"),
            "v2 snapshots must route to a rebuild"
        );
    }
}
