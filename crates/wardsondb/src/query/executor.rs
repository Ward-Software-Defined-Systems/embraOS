use serde_json::Value;

use crate::engine::backend::{Engine, PartitionId, StorageBackend};
use crate::engine::storage::Storage;
use crate::error::AppError;
use crate::index::IndexScanShape;
use crate::index::secondary::{
    RangeScanBounds, extract_doc_id_from_key, make_compound_index_key, prefix_successor,
    range_scan_bounds, value_to_sortable_bytes,
};

use super::cursor::{Cursor, CursorValue, compare_doc_to_cursor, encode_cursor};
use super::filter::resolve_json_path;
use super::parser::ParsedQuery;
use super::planner::{QueryPlan, ScanPlan, plan_query};
use super::sort::{DocSortKey, SortField, compare_decorated, extract_sort_key};

use std::ops::ControlFlow;

#[derive(Debug)]
pub struct QueryResult {
    pub docs: Vec<Value>,
    pub total_count: Option<u64>,
    pub docs_scanned: u64,
    pub index_used: Option<String>,
    pub scan_strategy: Option<String>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
}

pub fn execute_query(
    storage: &Storage,
    collection: &str,
    query: &ParsedQuery,
) -> Result<QueryResult, AppError> {
    // The 404 contract holds for EVERY plan shape. Only execute_full_scan
    // gated it before, so bitmap/index-planned queries on a missing
    // collection answered 200-with-empty instead of COLLECTION_NOT_FOUND
    // (F3 — observed right after the rig's rotation drop).
    storage.ensure_collection_exists(collection)?;

    // Unfiltered count: DocCounters is authoritative (seeded by a full count
    // at startup, maintained on every insert/delete path including bulk,
    // delete_by_query, and TTL cleanup), so the O(n) scan-and-parse the full
    // scan would do is pure waste — ~335 ms on a 100k-doc collection.
    if query.count_only && query.filter.is_none() {
        return Ok(QueryResult {
            docs: vec![],
            total_count: Some(storage.doc_counts.get(collection).max(0) as u64),
            docs_scanned: 0,
            index_used: None,
            scan_strategy: Some("doc_counter".to_string()),
            has_more: false,
            next_cursor: None,
        });
    }

    let plan = plan_query(
        query,
        &storage.index_manager,
        collection,
        &storage.scan_accelerator,
    );

    match &plan.scan {
        ScanPlan::FullScan => execute_full_scan(storage, collection, query, &plan),
        ScanPlan::IndexEq { .. }
        | ScanPlan::IndexIn { .. }
        | ScanPlan::IndexRange { .. }
        | ScanPlan::CompoundEq { .. } => execute_index_scan(storage, collection, query, &plan),
        ScanPlan::IndexSorted { .. } => execute_index_sorted(storage, collection, query, &plan),
        ScanPlan::CompoundRange { .. } => execute_compound_range(storage, collection, query, &plan),
        ScanPlan::BitmapScan { .. } => execute_bitmap_scan(storage, collection, query, &plan),
        ScanPlan::OrUnion { .. } => execute_or_union(storage, collection, query, &plan),
    }
}

/// Hydrate an ordered id list: one batched read + parse. Ids whose doc
/// vanished in the scan→get gap (or whose read failed) are skipped — the
/// snapshot-gap tolerance every hydration path has always had (S2-8 owns any
/// skip-vs-fail policy change). The returned length is what `docs_scanned`
/// reports on the materializing paths: documents actually loaded and parsed.
fn load_docs_by_ids<S: AsRef<str>>(
    storage: &Storage,
    docs_partition: &PartitionId,
    ids: &[S],
) -> Result<Vec<Value>, AppError> {
    let key_refs: Vec<&[u8]> = ids.iter().map(|id| id.as_ref().as_bytes()).collect();
    let mut docs = Vec::with_capacity(ids.len());
    for bytes in storage
        .engine
        .get_many(docs_partition, &key_refs)?
        .into_iter()
        .flatten()
    {
        if let Ok(doc) = serde_json::from_slice::<Value>(&bytes) {
            docs.push(doc);
        }
    }
    Ok(docs)
}

/// Shared projection tail: with `fields` present, project every doc (always
/// keeping `_id`); otherwise pass the docs through untouched.
fn apply_projection(docs: Vec<Value>, fields: &Option<Vec<String>>) -> Vec<Value> {
    match fields {
        Some(fields) => docs.iter().map(|doc| project_fields(doc, fields)).collect(),
        None => docs,
    }
}

/// Streaming page collector for unsorted match streams: skips `offset`
/// matches without keeping them, keeps limit+1 (the probe row makes
/// `has_more` exact), then asks the scan to break. `total_count` is exact
/// only when the scan ran to exhaustion — omitted exactly when `has_more`
/// is true.
struct UnsortedPage {
    to_skip: usize,
    keep: usize,
    matches: u64,
    page: Vec<Value>,
    broke_early: bool,
}

impl UnsortedPage {
    fn new(offset: usize, limit: usize) -> Self {
        UnsortedPage {
            to_skip: offset,
            // Saturating: aggregate feeds limit = u64::MAX, which must stream
            // to exhaustion (exact counts), never overflow.
            keep: limit.saturating_add(1),
            matches: 0,
            page: Vec::new(),
            broke_early: false,
        }
    }

    fn push(&mut self, doc: Value) -> ControlFlow<()> {
        self.matches += 1;
        if self.to_skip > 0 {
            self.to_skip -= 1;
            return ControlFlow::Continue(());
        }
        self.page.push(doc);
        if self.page.len() >= self.keep {
            self.broke_early = true;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }

    /// (page truncated to limit, has_more, exhaustion-exact total)
    fn finish(mut self, limit: usize) -> (Vec<Value>, bool, Option<u64>) {
        let has_more = self.page.len() > limit;
        self.page.truncate(limit);
        (
            self.page,
            has_more,
            (!self.broke_early).then_some(self.matches),
        )
    }
}

/// Bounded selection for sorted match streams: keeps the offset+limit+1
/// smallest rows in comparator order without materializing every match.
/// Never breaks the scan — every match is seen, so `total_count` stays exact
/// like the materializing sort this replaces. Memory: min(2·k, matches)
/// rows, never worse than full materialization.
struct TopK<'q> {
    k: usize,
    sort: &'q [SortField],
    buf: Vec<(DocSortKey, Value)>,
    /// The k-th smallest key once k rows are known: strictly-greater rows
    /// are rejected on arrival instead of buffered. Equal-to-cutoff rows are
    /// buffered so ties keep the stable sort's arrival order.
    cutoff: Option<DocSortKey>,
    matches: u64,
}

impl<'q> TopK<'q> {
    fn new(offset: usize, limit: usize, sort: &'q [SortField]) -> Self {
        TopK {
            k: offset.saturating_add(limit).saturating_add(1),
            sort,
            buf: Vec::new(),
            cutoff: None,
            matches: 0,
        }
    }

    fn push(&mut self, doc: Value) {
        self.matches += 1;
        let key = extract_sort_key(&doc, self.sort);
        if let Some(cutoff) = &self.cutoff
            && compare_decorated(&key, cutoff, self.sort) == std::cmp::Ordering::Greater
        {
            return;
        }
        self.buf.push((key, doc));
        if self.buf.len() >= self.k.saturating_mul(2) {
            self.compact();
        }
    }

    fn compact(&mut self) {
        // sort_by is stable, so equal keys keep arrival order — identical
        // tie behavior to the full sort this replaces.
        self.buf
            .sort_by(|a, b| compare_decorated(&a.0, &b.0, self.sort));
        self.buf.truncate(self.k);
        if self.buf.len() == self.k {
            self.cutoff = Some(self.buf[self.k - 1].0.clone());
        }
    }

    /// Rows in comparator order (at most 2·k of the smallest) + the exact
    /// count of every match pushed.
    fn finish(mut self) -> (Vec<(DocSortKey, Value)>, u64) {
        self.buf
            .sort_by(|a, b| compare_decorated(&a.0, &b.0, self.sort));
        (self.buf, self.matches)
    }
}

/// Shared tail for decorated top-K paths: page window, exact `has_more`,
/// cursor emission from the last page row, projection. `pre_cursor` counts
/// matches at-or-before the cursor position (kept out of the heap but part
/// of the exact total, mirroring the materializing sort's framing).
fn finish_topk(
    topk: TopK<'_>,
    pre_cursor: u64,
    query: &ParsedQuery,
    collection: &str,
) -> (Vec<Value>, Option<u64>, bool, Option<String>) {
    let offset = query.offset as usize;
    let limit = query.limit as usize;
    let (rows, kept) = topk.finish();
    let total_count = Some(pre_cursor + kept);
    let start = offset.min(rows.len());
    let end = start.saturating_add(limit).min(rows.len());
    // Exact: the top-K saw every match (cursor pages have offset == 0, so
    // the window start is never beyond the kept rows).
    let has_more = kept > offset.saturating_add(limit) as u64;
    // Every caller has a sort or a cursor (the old emission gate), so the
    // gate reduces to a non-empty page with more rows behind it.
    let next_cursor = if has_more && end > start {
        encode_cursor(&rows[end - 1].1, &query.sort, collection)
    } else {
        None
    };
    let page: Vec<Value> = rows
        .into_iter()
        .skip(start)
        .take(end - start)
        .map(|(_, doc)| doc)
        .collect();
    (
        apply_projection(page, &query.fields),
        total_count,
        has_more,
        next_cursor,
    )
}

/// Hydrate one index entry's doc from its borrowed key: id suffix → get →
/// parse. Missing docs (snapshot gap) and corrupt docs skip-and-continue —
/// the id-loop policy every index path has always had (S2-8 owns changing
/// it). Increments `docs_scanned` only for docs actually loaded+parsed.
fn hydrate_index_entry(
    storage: &Storage,
    docs_partition: &PartitionId,
    key: &[u8],
    docs_scanned: &mut u64,
) -> Option<Value> {
    let doc_id = extract_doc_id_from_key(key)?;
    let bytes = storage
        .engine
        .get(docs_partition, doc_id.as_bytes())
        .ok()??;
    let doc = serde_json::from_slice::<Value>(&bytes).ok()?;
    *docs_scanned += 1;
    Some(doc)
}

/// Collect every candidate id for the given scan shapes, in candidate order.
fn collect_ids(
    engine: &Engine,
    partition: &PartitionId,
    shapes: &[IndexScanShape],
) -> Result<Vec<String>, AppError> {
    let mut ids = Vec::new();
    for shape in shapes {
        shape.scan(engine, partition, &mut |k, _| {
            if let Some(id) = extract_doc_id_from_key(k) {
                ids.push(id);
            }
            ControlFlow::Continue(())
        })?;
    }
    Ok(ids)
}

/// Stream index entries shape-by-shape in candidate order, hydrating each
/// entry's doc and paging matches without ever materializing the candidate
/// set: residual counts evaluate everything and keep nothing; sorted (or
/// cursor-resumed) pages ride the decorated top-K; unsorted residual pages
/// early-exit at the limit+1 probe. Bare pages don't come here — they take
/// the windowed id path.
#[allow(clippy::too_many_arguments)]
fn execute_index_stream(
    storage: &Storage,
    collection: &str,
    query: &ParsedQuery,
    plan: &QueryPlan,
    index_name: &str,
    partition: &PartitionId,
    shapes: &[IndexScanShape],
    label_docs: bool,
) -> Result<QueryResult, AppError> {
    let docs_partition = storage.get_docs_partition(collection)?;
    let mut docs_scanned = 0u64;
    let doc_strategy = label_docs.then(|| plan.scan.name().to_string());

    if query.count_only {
        // Materialized count (residual present): evaluate every candidate,
        // keep nothing resident.
        let mut matches = 0u64;
        for shape in shapes {
            shape.scan(&storage.engine, partition, &mut |k, _| {
                let Some(doc) = hydrate_index_entry(storage, &docs_partition, k, &mut docs_scanned)
                else {
                    return ControlFlow::Continue(());
                };
                if plan.post_filter.as_ref().is_none_or(|f| f.matches(&doc)) {
                    matches += 1;
                }
                ControlFlow::Continue(())
            })?;
        }
        return Ok(QueryResult {
            docs: vec![],
            total_count: Some(matches),
            docs_scanned,
            index_used: Some(index_name.to_string()),
            scan_strategy: Some(plan.scan.name().to_string()),
            has_more: false,
            next_cursor: None,
        });
    }

    if query.cursor.is_some() || !query.sort.is_empty() {
        // Decorated top-K; a cursor with an empty sort orders by _id,
        // exactly like the materializing sort did.
        let mut topk = TopK::new(query.offset as usize, query.limit as usize, &query.sort);
        let mut pre_cursor = 0u64;
        for shape in shapes {
            shape.scan(&storage.engine, partition, &mut |k, _| {
                let Some(doc) = hydrate_index_entry(storage, &docs_partition, k, &mut docs_scanned)
                else {
                    return ControlFlow::Continue(());
                };
                if let Some(ref pf) = plan.post_filter
                    && !pf.matches(&doc)
                {
                    return ControlFlow::Continue(());
                }
                if let Some(cursor) = &query.cursor
                    && compare_doc_to_cursor(&doc, cursor, &query.sort)
                        != std::cmp::Ordering::Greater
                {
                    pre_cursor += 1;
                    return ControlFlow::Continue(());
                }
                topk.push(doc);
                ControlFlow::Continue(())
            })?;
        }
        let (docs, total_count, has_more, next_cursor) =
            finish_topk(topk, pre_cursor, query, collection);
        return Ok(QueryResult {
            docs,
            total_count,
            docs_scanned,
            index_used: Some(index_name.to_string()),
            scan_strategy: doc_strategy,
            has_more,
            next_cursor,
        });
    }

    // Unsorted residual page: early exit at the probe row, shape by shape.
    let mut sink = UnsortedPage::new(query.offset as usize, query.limit as usize);
    for shape in shapes {
        shape.scan(&storage.engine, partition, &mut |k, _| {
            let Some(doc) = hydrate_index_entry(storage, &docs_partition, k, &mut docs_scanned)
            else {
                return ControlFlow::Continue(());
            };
            if let Some(ref pf) = plan.post_filter
                && !pf.matches(&doc)
            {
                return ControlFlow::Continue(());
            }
            sink.push(doc)
        })?;
        if sink.broke_early {
            break;
        }
    }
    let (page, has_more, total_count) = sink.finish(query.limit as usize);
    Ok(QueryResult {
        docs: apply_projection(page, &query.fields),
        total_count,
        docs_scanned,
        index_used: Some(index_name.to_string()),
        scan_strategy: doc_strategy,
        has_more,
        // Index no-sort pages never emitted cursors (their order isn't _id).
        next_cursor: None,
    })
}

/// Page an ordered id list through the streaming sinks (bitmap and $or-union
/// paths, whose candidates are id lists rather than live index scans):
/// unsorted residual pages hydrate per id and stop at the probe row;
/// sorted/cursor pages and residual counts batch-hydrate (every id gets
/// evaluated regardless) and feed the same sinks. Meta labels come from the
/// caller.
fn execute_id_stream<S: AsRef<str>>(
    storage: &Storage,
    collection: &str,
    query: &ParsedQuery,
    plan: &QueryPlan,
    ids: &[S],
    index_used: Option<String>,
) -> Result<QueryResult, AppError> {
    let docs_partition = storage.get_docs_partition(collection)?;

    if query.count_only {
        let docs = load_docs_by_ids(storage, &docs_partition, ids)?;
        let docs_scanned = docs.len() as u64;
        let matches = docs
            .iter()
            .filter(|d| plan.post_filter.as_ref().is_none_or(|f| f.matches(d)))
            .count() as u64;
        return Ok(QueryResult {
            docs: vec![],
            total_count: Some(matches),
            docs_scanned,
            index_used,
            scan_strategy: Some(plan.scan.name().to_string()),
            has_more: false,
            next_cursor: None,
        });
    }

    if query.cursor.is_some() || !query.sort.is_empty() {
        let docs = load_docs_by_ids(storage, &docs_partition, ids)?;
        let docs_scanned = docs.len() as u64;
        let mut topk = TopK::new(query.offset as usize, query.limit as usize, &query.sort);
        let mut pre_cursor = 0u64;
        for doc in docs {
            if let Some(ref pf) = plan.post_filter
                && !pf.matches(&doc)
            {
                continue;
            }
            if let Some(cursor) = &query.cursor
                && compare_doc_to_cursor(&doc, cursor, &query.sort) != std::cmp::Ordering::Greater
            {
                pre_cursor += 1;
                continue;
            }
            topk.push(doc);
        }
        let (docs, total_count, has_more, next_cursor) =
            finish_topk(topk, pre_cursor, query, collection);
        return Ok(QueryResult {
            docs,
            total_count,
            docs_scanned,
            index_used,
            scan_strategy: Some(plan.scan.name().to_string()),
            has_more,
            next_cursor,
        });
    }

    // Unsorted residual page: per-id hydration, early exit at the probe row.
    let mut sink = UnsortedPage::new(query.offset as usize, query.limit as usize);
    let mut docs_scanned = 0u64;
    for doc_id in ids {
        let Ok(Some(bytes)) = storage
            .engine
            .get(&docs_partition, doc_id.as_ref().as_bytes())
        else {
            continue;
        };
        let Ok(doc) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        docs_scanned += 1;
        if let Some(ref pf) = plan.post_filter
            && !pf.matches(&doc)
        {
            continue;
        }
        if sink.push(doc).is_break() {
            break;
        }
    }
    let (page, has_more, total_count) = sink.finish(query.limit as usize);
    Ok(QueryResult {
        docs: apply_projection(page, &query.fields),
        total_count,
        docs_scanned,
        index_used,
        scan_strategy: Some(plan.scan.name().to_string()),
        has_more,
        next_cursor: None,
    })
}

/// Bare-page fast path without candidate materialization (single-shape
/// plans): the exact total comes from the keys-only backend count, offset
/// entries are skipped at the KEY level, and only the window's ids are
/// collected and hydrated. `has_more` derives from the count, so it always
/// agrees with `total_count`; the count and the id scan are two reads
/// instants apart — the documented snapshot-gap semantic of this path. An
/// entry whose id can't be extracted (unreachable via the write path) or
/// whose doc vanished shortens the page rather than shifting it, as before.
fn windowed_bare_page(
    storage: &Storage,
    collection: &str,
    query: &ParsedQuery,
    index_name: &str,
    partition: &PartitionId,
    shape: &IndexScanShape,
    doc_strategy: Option<String>,
) -> Result<QueryResult, AppError> {
    let total = match shape {
        IndexScanShape::Prefix(p) => storage.engine.count_prefix(partition, p)?,
        IndexScanShape::Range { start, end } => {
            storage.engine.count_range(partition, start, end)?
        }
    };
    let offset = query.offset as usize;
    let limit = query.limit as usize;
    let mut to_skip = offset;
    let mut ids: Vec<String> = Vec::new();
    shape.scan(&storage.engine, partition, &mut |k, _| {
        if to_skip > 0 {
            to_skip -= 1;
            return ControlFlow::Continue(());
        }
        if ids.len() >= limit {
            return ControlFlow::Break(());
        }
        if let Some(id) = extract_doc_id_from_key(k) {
            ids.push(id);
        }
        ControlFlow::Continue(())
    })?;
    let docs_partition = storage.get_docs_partition(collection)?;
    let docs = load_docs_by_ids(storage, &docs_partition, &ids)?;
    let docs_scanned = ids.len() as u64;
    let has_more = total > (offset as u64).saturating_add(limit as u64);
    Ok(QueryResult {
        docs: apply_projection(docs, &query.fields),
        total_count: Some(total),
        docs_scanned,
        index_used: Some(index_name.to_string()),
        scan_strategy: doc_strategy,
        has_more,
        next_cursor: None,
    })
}

/// The no-usable-index result (the planner resolved one moments ago, so at
/// worst this is a race with a concurrent index drop): zero candidates,
/// mirroring the old empty-id-list output on every sub-path.
fn empty_index_result(
    index_name: &str,
    query: &ParsedQuery,
    plan: &QueryPlan,
    label_docs: bool,
) -> Result<QueryResult, AppError> {
    Ok(QueryResult {
        docs: vec![],
        total_count: Some(0),
        docs_scanned: 0,
        index_used: Some(index_name.to_string()),
        scan_strategy: (query.count_only || label_docs).then(|| plan.scan.name().to_string()),
        has_more: false,
        next_cursor: None,
    })
}

fn execute_full_scan(
    storage: &Storage,
    collection: &str,
    query: &ParsedQuery,
    plan: &QueryPlan,
) -> Result<QueryResult, AppError> {
    // Cursor + no sort: the total order is _id ascending, which is exactly
    // the docs partition's key order — seek straight to the position instead
    // of materializing everything before it.
    if let Some(cursor) = &query.cursor
        && query.sort.is_empty()
        && !query.count_only
    {
        return execute_full_scan_id_seek(storage, collection, query, plan, cursor);
    }

    // The 404 contract for queries comes from the _meta registry, which the
    // replaced scan_all_documents call used to consult.
    storage.ensure_collection_exists(collection)?;
    let docs_partition = storage.get_docs_partition(collection)?;
    let filter = plan.original_filter.as_ref();

    // Corrupt-doc policy parity with the scan_all_documents path this
    // replaces: a document that fails to parse fails the whole query (the
    // id-hydration loops skip instead — S2-8 owns unifying that).
    let mut parse_err: Option<AppError> = None;

    if query.count_only {
        // Full evaluation is inherent to an exact filtered count; streaming
        // only removes the whole-collection buffer.
        let mut docs_scanned = 0u64;
        let mut matches = 0u64;
        storage.engine.scan_full(&docs_partition, &mut |_, v| {
            let doc: Value = match serde_json::from_slice(v) {
                Ok(doc) => doc,
                Err(e) => {
                    parse_err = Some(e.into());
                    return ControlFlow::Break(());
                }
            };
            docs_scanned += 1;
            if filter.is_none_or(|f| f.matches(&doc)) {
                matches += 1;
            }
            ControlFlow::Continue(())
        })?;
        if let Some(e) = parse_err {
            return Err(e);
        }
        return Ok(QueryResult {
            docs: vec![],
            total_count: Some(matches),
            docs_scanned,
            index_used: None,
            scan_strategy: Some(plan.scan.name().to_string()),
            has_more: false,
            next_cursor: None,
        });
    }

    if query.sort.is_empty() {
        // Unsorted page: filter during the scan, skip offset matches without
        // keeping them, stop at the limit+1 probe row. Unfiltered scans skip
        // offset entries WITHOUT parsing (every entry matches) and take
        // total_count from DocCounters (authoritative, O(1)); filtered scans
        // report an exact total only when the scan ran out (UnsortedPage).
        let mut sink = UnsortedPage::new(query.offset as usize, query.limit as usize);
        let mut docs_scanned = 0u64;
        storage.engine.scan_full(&docs_partition, &mut |_, v| {
            if filter.is_none() && sink.to_skip > 0 {
                sink.to_skip -= 1;
                sink.matches += 1;
                return ControlFlow::Continue(());
            }
            let doc: Value = match serde_json::from_slice(v) {
                Ok(doc) => doc,
                Err(e) => {
                    parse_err = Some(e.into());
                    return ControlFlow::Break(());
                }
            };
            docs_scanned += 1;
            match filter {
                Some(f) if !f.matches(&doc) => ControlFlow::Continue(()),
                _ => sink.push(doc),
            }
        })?;
        if let Some(e) = parse_err {
            return Err(e);
        }
        let (page, has_more, streamed_total) = sink.finish(query.limit as usize);
        let total_count = if filter.is_none() {
            Some(storage.doc_counts.get(collection).max(0) as u64)
        } else {
            streamed_total
        };
        // Bootstrap cursor for no-sort walks: a full scan streams in _id
        // order (the docs partition key), so the last page doc's id resumes
        // the walk. Index and bitmap scans must NOT do this — their no-sort
        // order isn't _id.
        let next_cursor = if has_more {
            page.last()
                .and_then(|doc| encode_cursor(doc, &[], collection))
        } else {
            None
        };
        let docs = apply_projection(page, &query.fields);
        return Ok(QueryResult {
            docs,
            total_count,
            docs_scanned,
            index_used: None,
            scan_strategy: None,
            has_more,
            next_cursor,
        });
    }

    // Sorted page: decorate-and-select. Every match is still seen (exact
    // total_count, exact has_more), but only the offset+limit+1 smallest
    // rows stay resident instead of the whole match set.
    let mut topk = TopK::new(query.offset as usize, query.limit as usize, &query.sort);
    let mut pre_cursor = 0u64;
    let mut docs_scanned = 0u64;
    storage.engine.scan_full(&docs_partition, &mut |_, v| {
        let doc: Value = match serde_json::from_slice(v) {
            Ok(doc) => doc,
            Err(e) => {
                parse_err = Some(e.into());
                return ControlFlow::Break(());
            }
        };
        docs_scanned += 1;
        if let Some(f) = filter
            && !f.matches(&doc)
        {
            return ControlFlow::Continue(());
        }
        // Cursor pages keep total_count parity with the materializing sort:
        // matches at-or-before the cursor are counted but never kept.
        if let Some(cursor) = &query.cursor
            && compare_doc_to_cursor(&doc, cursor, &query.sort) != std::cmp::Ordering::Greater
        {
            pre_cursor += 1;
            return ControlFlow::Continue(());
        }
        topk.push(doc);
        ControlFlow::Continue(())
    })?;
    if let Some(e) = parse_err {
        return Err(e);
    }

    let (docs, total_count, has_more, next_cursor) =
        finish_topk(topk, pre_cursor, query, collection);
    Ok(QueryResult {
        docs,
        total_count,
        docs_scanned,
        index_used: None,
        scan_strategy: None,
        has_more,
        next_cursor,
    })
}

/// Cursor-resumed full scan with an empty sort: seek the docs partition
/// (key = `_id`) to just after the cursor's id and stream forward with a
/// limit+1 probe.
fn execute_full_scan_id_seek(
    storage: &Storage,
    collection: &str,
    query: &ParsedQuery,
    plan: &QueryPlan,
    cursor: &Cursor,
) -> Result<QueryResult, AppError> {
    let docs_partition = storage.get_docs_partition(collection)?;
    let limit = query.limit as usize;

    // Strictly after last_id: ids are NUL-free, so last_id ++ 0x00 is the
    // smallest key greater than last_id.
    let mut lo = cursor.last_id.clone().into_bytes();
    lo.push(0x00);
    // _ids are UTF-8 strings and 0xFF never occurs in UTF-8, so a single
    // 0xFF byte sorts above every doc key.
    let hi = [0xFFu8];

    let mut results: Vec<Value> = Vec::new();
    let mut docs_scanned = 0u64;
    storage
        .engine
        .scan_range(&docs_partition, &lo, &hi, &mut |_, v| {
            // Parse failures skip-and-continue — this path's long-standing
            // policy (unlike the main full scan's fail), preserved verbatim.
            let Ok(doc) = serde_json::from_slice::<Value>(v) else {
                return ControlFlow::Continue(());
            };
            docs_scanned += 1;
            if let Some(filter) = &plan.original_filter
                && !filter.matches(&doc)
            {
                return ControlFlow::Continue(());
            }
            results.push(doc);
            if results.len() > limit {
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        })?;

    let has_more = results.len() > limit;
    results.truncate(limit);
    let next_cursor = if has_more {
        results
            .last()
            .and_then(|doc| encode_cursor(doc, &[], collection))
    } else {
        None
    };

    let docs = if let Some(ref fields) = query.fields {
        results
            .iter()
            .map(|doc| project_fields(doc, fields))
            .collect()
    } else {
        results
    };

    Ok(QueryResult {
        docs,
        total_count: None, // the seek never sees the full match set
        docs_scanned,
        index_used: None,
        scan_strategy: None,
        has_more,
        next_cursor,
    })
}

/// Load only the `offset..offset+limit` window of an ordered candidate id
/// list — the bare-page fast path where no post-filter, sort, or cursor can
/// change which ids form the page. Framing matches the streaming sinks
/// exactly (`start = offset.min(len)`, `end = (start+limit).min(len)`,
/// `has_more = len > end`). Ids whose doc vanished in the index-read→get gap
/// shorten the page rather than shifting it — the same snapshot-gap semantic
/// as the count fast paths. Returns `(docs, docs_scanned, has_more)`;
/// `docs_scanned` counts the gets performed (the window), not the candidates.
fn load_id_window(
    storage: &Storage,
    collection: &str,
    query: &ParsedQuery,
    candidate_ids: &[String],
) -> Result<(Vec<Value>, u64, bool), AppError> {
    let total = candidate_ids.len();
    let start = (query.offset as usize).min(total);
    let end = start.saturating_add(query.limit as usize).min(total);

    let docs_partition = storage.get_docs_partition(collection)?;
    let docs = load_docs_by_ids(storage, &docs_partition, &candidate_ids[start..end])?;
    let docs_scanned = (end - start) as u64;
    let has_more = total > end;

    Ok((
        apply_projection(docs, &query.fields),
        docs_scanned,
        has_more,
    ))
}

/// True when nothing after the scan can change which candidates form the
/// page: no residual filter, no sort, no cursor, and docs are wanted.
fn bare_page(query: &ParsedQuery, post_filter: &Option<crate::query::filter::FilterNode>) -> bool {
    !query.count_only && post_filter.is_none() && query.sort.is_empty() && query.cursor.is_none()
}

fn execute_index_scan(
    storage: &Storage,
    collection: &str,
    query: &ParsedQuery,
    plan: &QueryPlan,
) -> Result<QueryResult, AppError> {
    let (index_name, partition, shapes) = match &plan.scan {
        ScanPlan::IndexEq {
            index_name,
            field,
            value,
        } => {
            // Optimized count_only: count index keys without loading docs
            if query.count_only
                && plan.post_filter.is_none()
                && let Some(count) =
                    storage
                        .index_manager
                        .count_eq(&storage.engine, collection, field, value)
            {
                return Ok(QueryResult {
                    docs: vec![],
                    total_count: Some(count),
                    docs_scanned: 0,
                    index_used: Some(index_name.clone()),
                    scan_strategy: Some(plan.scan.name().to_string()),
                    has_more: false,
                    next_cursor: None,
                });
            }

            match storage.index_manager.eq_scan(collection, field, value) {
                Some((partition, prefix)) => (
                    index_name.clone(),
                    partition,
                    vec![IndexScanShape::Prefix(prefix)],
                ),
                None => return empty_index_result(index_name, query, plan, false),
            }
        }
        ScanPlan::IndexIn {
            index_name,
            field,
            values,
        } => {
            // Optimized count_only for $in
            if query.count_only && plan.post_filter.is_none() {
                let mut total = 0u64;
                let has_index = storage
                    .index_manager
                    .get_index_for_field(collection, field)
                    .is_some();
                if has_index {
                    // Dedup by encoded value — the identity the index prefix
                    // uses — or $in: ["a","a"] double-counts (the non-count
                    // path dedups by doc id and never had this).
                    let mut seen = std::collections::HashSet::new();
                    for value in values {
                        if !seen.insert(value_to_sortable_bytes(value)) {
                            continue;
                        }
                        if let Some(count) = storage.index_manager.count_eq(
                            &storage.engine,
                            collection,
                            field,
                            value,
                        ) {
                            total += count;
                        }
                    }
                    return Ok(QueryResult {
                        docs: vec![],
                        total_count: Some(total),
                        docs_scanned: 0,
                        index_used: Some(index_name.clone()),
                        scan_strategy: Some(plan.scan.name().to_string()),
                        has_more: false,
                        next_cursor: None,
                    });
                }
            }

            match storage.index_manager.in_scan(collection, field, values) {
                Some((partition, prefixes)) => (
                    index_name.clone(),
                    partition,
                    prefixes.into_iter().map(IndexScanShape::Prefix).collect(),
                ),
                None => return empty_index_result(index_name, query, plan, false),
            }
        }
        ScanPlan::IndexRange {
            index_name,
            field,
            lower,
            upper,
        } => {
            // Optimized count_only for range
            if query.count_only && plan.post_filter.is_none() {
                let lower_ref = lower.as_ref().map(|(v, i)| (v, *i));
                let upper_ref = upper.as_ref().map(|(v, i)| (v, *i));
                if let Some(count) = storage.index_manager.count_range(
                    &storage.engine,
                    collection,
                    field,
                    lower_ref,
                    upper_ref,
                ) {
                    return Ok(QueryResult {
                        docs: vec![],
                        total_count: Some(count),
                        docs_scanned: 0,
                        index_used: Some(index_name.clone()),
                        scan_strategy: Some(plan.scan.name().to_string()),
                        has_more: false,
                        next_cursor: None,
                    });
                }
            }

            let lower_ref = lower.as_ref().map(|(v, i)| (v, *i));
            let upper_ref = upper.as_ref().map(|(v, i)| (v, *i));
            match storage
                .index_manager
                .range_scan(collection, field, lower_ref, upper_ref)
            {
                Some((partition, RangeScanBounds::Span { start, end })) => (
                    index_name.clone(),
                    partition,
                    vec![IndexScanShape::Range { start, end }],
                ),
                // Type-bracketed empty bounds or no index: zero matches on
                // every sub-path, same as the old empty id list.
                Some((_, RangeScanBounds::Empty)) | None => {
                    return empty_index_result(index_name, query, plan, false);
                }
            }
        }
        ScanPlan::CompoundEq { index_name, prefix } => {
            // Compound equality: prefix scan on compound index
            if query.count_only && plan.post_filter.is_none() {
                let partition = storage
                    .index_manager
                    .get_index_partition(collection, index_name)
                    .ok_or_else(|| {
                        AppError::Internal(format!("Index partition not found: {index_name}"))
                    })?;
                // Keys-only backend count; errors propagate instead of the
                // old flatten().count() silently undercounting on them.
                let count = storage.engine.count_prefix(&partition, prefix)?;
                return Ok(QueryResult {
                    docs: vec![],
                    total_count: Some(count),
                    docs_scanned: 0,
                    index_used: Some(index_name.clone()),
                    scan_strategy: Some(plan.scan.name().to_string()),
                    has_more: false,
                    next_cursor: None,
                });
            }

            let partition = storage
                .index_manager
                .get_index_partition(collection, index_name)
                .ok_or_else(|| {
                    AppError::Internal(format!("Index partition not found: {index_name}"))
                })?;
            (
                index_name.clone(),
                partition,
                vec![IndexScanShape::Prefix(prefix.clone())],
            )
        }
        ScanPlan::FullScan
        | ScanPlan::IndexSorted { .. }
        | ScanPlan::CompoundRange { .. }
        | ScanPlan::BitmapScan { .. }
        | ScanPlan::OrUnion { .. } => unreachable!(),
    };

    // Bare page: the page is exactly candidates[offset..offset+limit] in
    // index order. Single-shape plans never materialize the candidates —
    // keys-only count + key-level window (C5b); multi-shape $in still
    // collects (cross-shape offset math isn't worth it) and windows the load.
    if bare_page(query, &plan.post_filter) {
        if let [shape] = shapes.as_slice() {
            return windowed_bare_page(
                storage,
                collection,
                query,
                &index_name,
                &partition,
                shape,
                None,
            );
        }
        let candidate_ids = collect_ids(&storage.engine, &partition, &shapes)?;
        let total = candidate_ids.len() as u64;
        let (docs, docs_scanned, has_more) =
            load_id_window(storage, collection, query, &candidate_ids)?;
        return Ok(QueryResult {
            docs,
            total_count: Some(total),
            docs_scanned,
            index_used: Some(index_name),
            scan_strategy: None,
            has_more,
            next_cursor: None,
        });
    }

    execute_index_stream(
        storage,
        collection,
        query,
        plan,
        &index_name,
        &partition,
        &shapes,
        false,
    )
}

/// Rebuild the exact index key for a cursor position under this plan's
/// prefix: the cursor's (sort values, last_id) tail IS a compound index key,
/// so reuse `make_compound_index_key` — the planner prefix already carries
/// its trailing 0x01 separator.
fn index_cursor_key(prefix: &[u8], cursor: &Cursor) -> Vec<u8> {
    // Missing is unreachable on this path (the planner rejects such cursors
    // before choosing an index seek); filter_map keeps the function total and
    // the debug_assert pins the invariant.
    let values: Vec<&Value> = cursor
        .sort_values
        .iter()
        .filter_map(|cv| match cv {
            CursorValue::Present(v) => Some(v),
            CursorValue::Missing => None,
        })
        .collect();
    debug_assert_eq!(
        values.len(),
        cursor.sort_values.len(),
        "cursor with Missing sort values must not reach the index seek path"
    );
    let mut key = prefix.to_vec();
    key.extend_from_slice(&make_compound_index_key(&values, &cursor.last_id));
    key
}

/// Execute a sorted index scan with early termination.
/// Uses a compound index that covers both filter and sort fields.
fn execute_index_sorted(
    storage: &Storage,
    collection: &str,
    query: &ParsedQuery,
    plan: &QueryPlan,
) -> Result<QueryResult, AppError> {
    let (index_name, prefix, reverse, exact_tail) = match &plan.scan {
        ScanPlan::IndexSorted {
            index_name,
            prefix,
            reverse,
            exact_tail,
        } => (
            index_name.as_str(),
            prefix.as_slice(),
            *reverse,
            *exact_tail,
        ),
        _ => unreachable!(),
    };

    let partition = storage
        .index_manager
        .get_index_partition(collection, index_name)
        .ok_or_else(|| AppError::Internal(format!("Index partition not found: {index_name}")))?;

    let docs_partition = storage.get_docs_partition(collection)?;

    let offset = query.offset as usize;
    let limit = query.limit as usize;

    // Scan bounds: the whole prefix window, tightened to strictly after
    // (forward) or strictly before (reverse) the cursor position. The planner
    // only routes cursors here when exact_tail holds and no sort value is
    // Missing, so the cursor maps onto an exact index key.
    let prefix_end = prefix_successor(prefix);
    let (lo, hi) = match &query.cursor {
        Some(cursor) => {
            let cursor_key = index_cursor_key(prefix, cursor);
            if reverse {
                (prefix.to_vec(), cursor_key) // end-exclusive = strictly below
            } else {
                // Doc ids are NUL-free, so no real key equals cursor_key ++
                // 0x00 — appending it yields the smallest strictly-greater key.
                let mut lo = cursor_key;
                lo.push(0x00);
                (lo, prefix_end)
            }
        }
        None => (prefix.to_vec(), prefix_end),
    };

    // Collect UNPROJECTED docs up to limit+1: the extra probe row makes
    // has_more exact (the scan breaks on it — the streaming equivalent of
    // the old bounded page-probe read), and the cursor must see sort-field
    // values that a projection might strip. An index entry whose doc
    // vanished between the scan and the `get` consumes a slot and could
    // understate has_more, but doc + index entries are deleted in one atomic
    // batch, so the window is only the snapshot gap.
    let mut results: Vec<Value> = Vec::new();
    let mut skipped = 0usize;
    let mut docs_scanned = 0u64;
    // With no post-filter every entry is a match, so offset rows are skipped
    // at the KEY level — previously they were hydrated and then discarded.
    // With a post-filter, skipping must count post-filter MATCHES, so the
    // match-level skip below applies instead.
    let mut entry_skip = if plan.post_filter.is_none() {
        offset
    } else {
        0
    };

    let mut visit = |k: &[u8], _v: &[u8]| -> ControlFlow<()> {
        if entry_skip > 0 {
            entry_skip -= 1;
            return ControlFlow::Continue(());
        }
        let Some(doc) = hydrate_index_entry(storage, &docs_partition, k, &mut docs_scanned) else {
            return ControlFlow::Continue(());
        };
        if let Some(ref pf) = plan.post_filter {
            if !pf.matches(&doc) {
                return ControlFlow::Continue(());
            }
            if skipped < offset {
                skipped += 1;
                return ControlFlow::Continue(());
            }
        }
        results.push(doc);
        if results.len() > limit {
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    };
    if reverse {
        storage
            .engine
            .scan_range_rev(&partition, &lo, &hi, &mut visit)?;
    } else {
        storage
            .engine
            .scan_range(&partition, &lo, &hi, &mut visit)?;
    }

    let has_more = results.len() > limit;
    results.truncate(limit);

    let next_cursor = if has_more && exact_tail {
        results
            .last()
            .and_then(|doc| encode_cursor(doc, &query.sort, collection))
    } else {
        None
    };

    let docs = if let Some(ref fields) = query.fields {
        results
            .iter()
            .map(|doc| project_fields(doc, fields))
            .collect()
    } else {
        results
    };

    Ok(QueryResult {
        docs,
        total_count: None, // Unknown with early termination
        docs_scanned,
        index_used: Some(index_name.to_string()),
        scan_strategy: Some(plan.scan.name().to_string()),
        has_more,
        next_cursor,
    })
}

/// Execute a compound range scan: equality prefix + range on next field.
fn execute_compound_range(
    storage: &Storage,
    collection: &str,
    query: &ParsedQuery,
    plan: &QueryPlan,
) -> Result<QueryResult, AppError> {
    let (index_name, eq_prefix, lower, upper) = match &plan.scan {
        ScanPlan::CompoundRange {
            index_name,
            eq_prefix,
            lower,
            upper,
        } => (
            index_name.as_str(),
            eq_prefix.as_slice(),
            lower.as_ref(),
            upper.as_ref(),
        ),
        _ => unreachable!(),
    };

    let partition = storage
        .index_manager
        .get_index_partition(collection, index_name)
        .ok_or_else(|| AppError::Internal(format!("Index partition not found: {index_name}")))?;

    // Shared bounds builder: eq_prefix (with its trailing 0x01) glued to the
    // range-field bounds, open ends closed over the operand's type bracket,
    // exclusive lower folded into the start key (no skip loop needed).
    let (start_key, end_key) = match range_scan_bounds(
        eq_prefix,
        lower.map(|(b, i)| (b.as_slice(), *i)),
        upper.map(|(b, i)| (b.as_slice(), *i)),
    ) {
        RangeScanBounds::Empty => {
            // The range predicate can never match (null/array/object operand
            // or cross-bucket bounds) — serve every path without a scan.
            return Ok(QueryResult {
                docs: vec![],
                total_count: Some(0),
                docs_scanned: 0,
                index_used: Some(index_name.to_string()),
                scan_strategy: Some(plan.scan.name().to_string()),
                has_more: false,
                next_cursor: None,
            });
        }
        RangeScanBounds::Span { start, end } => (start, end),
    };

    // count_only optimization: count index keys without loading docs.
    if query.count_only && plan.post_filter.is_none() {
        let count = storage
            .engine
            .count_range(&partition, &start_key, &end_key)?;
        return Ok(QueryResult {
            docs: vec![],
            total_count: Some(count),
            docs_scanned: 0,
            index_used: Some(index_name.to_string()),
            scan_strategy: Some(plan.scan.name().to_string()),
            has_more: false,
            next_cursor: None,
        });
    }

    let shapes = vec![IndexScanShape::Range {
        start: start_key,
        end: end_key,
    }];

    // Bare page: keys-only count + key-level window over the range (C5b);
    // compound_range labels its doc pages, unlike the single-field paths.
    if bare_page(query, &plan.post_filter) {
        return windowed_bare_page(
            storage,
            collection,
            query,
            index_name,
            &partition,
            &shapes[0],
            Some(plan.scan.name().to_string()),
        );
    }

    execute_index_stream(
        storage, collection, query, plan, index_name, &partition, &shapes, true,
    )
}

/// Candidate doc ids for one `$or` arm, plus the arm's index name. Arms are
/// only ever the index-servable variants (`plan_or_arm` builds them).
fn or_arm_ids<'a>(
    storage: &Storage,
    collection: &str,
    arm: &'a ScanPlan,
) -> Result<(&'a str, Vec<String>), AppError> {
    match arm {
        ScanPlan::IndexEq {
            index_name,
            field,
            value,
        } => {
            let ids = storage
                .index_manager
                .lookup_eq(&storage.engine, collection, field, value)?
                .unwrap_or_default();
            Ok((index_name, ids))
        }
        ScanPlan::IndexIn {
            index_name,
            field,
            values,
        } => {
            let ids = storage
                .index_manager
                .lookup_in(&storage.engine, collection, field, values)?
                .unwrap_or_default();
            Ok((index_name, ids))
        }
        ScanPlan::IndexRange {
            index_name,
            field,
            lower,
            upper,
        } => {
            let lower_ref = lower.as_ref().map(|(v, i)| (v, *i));
            let upper_ref = upper.as_ref().map(|(v, i)| (v, *i));
            let ids = storage
                .index_manager
                .lookup_range(&storage.engine, collection, field, lower_ref, upper_ref)?
                .unwrap_or_default();
            Ok((index_name, ids))
        }
        ScanPlan::CompoundEq { index_name, prefix } => {
            let partition = storage
                .index_manager
                .get_index_partition(collection, index_name)
                .ok_or_else(|| {
                    AppError::Internal(format!("Index partition not found: {index_name}"))
                })?;
            let ids = collect_ids(
                &storage.engine,
                &partition,
                &[IndexScanShape::Prefix(prefix.clone())],
            )?;
            Ok((index_name, ids))
        }
        ScanPlan::CompoundRange {
            index_name,
            eq_prefix,
            lower,
            upper,
        } => {
            let bounds = range_scan_bounds(
                eq_prefix,
                lower.as_ref().map(|(b, i)| (b.as_slice(), *i)),
                upper.as_ref().map(|(b, i)| (b.as_slice(), *i)),
            );
            let (start, end) = match bounds {
                // This arm alone matches nothing; other arms still contribute.
                RangeScanBounds::Empty => return Ok((index_name, Vec::new())),
                RangeScanBounds::Span { start, end } => (start, end),
            };
            let partition = storage
                .index_manager
                .get_index_partition(collection, index_name)
                .ok_or_else(|| {
                    AppError::Internal(format!("Index partition not found: {index_name}"))
                })?;
            let ids = collect_ids(
                &storage.engine,
                &partition,
                &[IndexScanShape::Range { start, end }],
            )?;
            Ok((index_name, ids))
        }
        ScanPlan::FullScan
        | ScanPlan::IndexSorted { .. }
        | ScanPlan::BitmapScan { .. }
        | ScanPlan::OrUnion { .. } => unreachable!("or_union arms are index scans"),
    }
}

/// Execute a `$or` union of per-arm index lookups. Ids are unioned across
/// arms (deduped) and re-sorted to `_id` order — the docs-partition order a
/// full scan yields — so pages, counts, and offset tiling are byte-identical
/// to the full-scan path this replaces; only `docs_scanned`, `index_used`,
/// and `scan_strategy` differ. When any arm over-approximates, the plan's
/// post-filter is the original `$or` (see `ScanPlan::OrUnion`).
fn execute_or_union(
    storage: &Storage,
    collection: &str,
    query: &ParsedQuery,
    plan: &QueryPlan,
) -> Result<QueryResult, AppError> {
    let ScanPlan::OrUnion { arms } = &plan.scan else {
        unreachable!()
    };

    let mut seen = std::collections::HashSet::new();
    let mut ids: Vec<String> = Vec::new();
    let mut index_names: Vec<&str> = Vec::new();
    for arm in arms {
        let (index_name, arm_ids) = or_arm_ids(storage, collection, arm)?;
        if !index_names.contains(&index_name) {
            index_names.push(index_name);
        }
        for id in arm_ids {
            if seen.insert(id.clone()) {
                ids.push(id);
            }
        }
    }
    // _id order == docs-partition key order == full-scan output order.
    ids.sort_unstable();
    let index_used = Some(index_names.join("+"));

    // Exact arms: the union IS the match set — count without loading a doc.
    if query.count_only && plan.post_filter.is_none() {
        return Ok(QueryResult {
            docs: vec![],
            total_count: Some(ids.len() as u64),
            docs_scanned: 0,
            index_used,
            scan_strategy: Some(plan.scan.name().to_string()),
            has_more: false,
            next_cursor: None,
        });
    }

    // Bare page: window the _id-ordered union (same semantics as the other
    // windowed index paths — total_count is the full union size).
    if bare_page(query, &plan.post_filter) {
        let total = ids.len() as u64;
        let (docs, docs_scanned, has_more) = load_id_window(storage, collection, query, &ids)?;
        return Ok(QueryResult {
            docs,
            total_count: Some(total),
            docs_scanned,
            index_used,
            scan_strategy: Some(plan.scan.name().to_string()),
            has_more,
            next_cursor: None,
        });
    }

    execute_id_stream(storage, collection, query, plan, &ids, index_used)
}

/// Execute a query using the bitmap scan accelerator.
fn execute_bitmap_scan(
    storage: &Storage,
    collection: &str,
    query: &ParsedQuery,
    plan: &QueryPlan,
) -> Result<QueryResult, AppError> {
    // The plan carries the bitmap computed during planning (Roaring AND/OR
    // over per-value bitmaps is not free — recomputing it here doubled that
    // work) and its residual lives in plan.post_filter. Position resolution
    // below tolerates staleness relative to concurrent writes the same way
    // the old plan-then-recompute window did.
    let ScanPlan::BitmapScan { bitmap } = &plan.scan else {
        unreachable!()
    };

    // count_only optimization — zero doc reads when no residual filter
    if query.count_only && plan.post_filter.is_none() {
        return Ok(QueryResult {
            docs: vec![],
            total_count: Some(bitmap.len()),
            docs_scanned: 0,
            index_used: None,
            scan_strategy: Some(plan.scan.name().to_string()),
            has_more: false,
            next_cursor: None,
        });
    }

    // Bare page: the page is the offset..offset+limit window of the
    // ascending-position order — resolve and load only that window, with a
    // +1 probe id so has_more stays exact even across transient holes.
    // total_count is the bitmap cardinality, matching the count fast path
    // above (same snapshot-gap semantic as the index windows).
    if bare_page(query, &plan.post_filter) {
        let total = bitmap.len();
        let offset = query.offset as usize;
        let limit = query.limit as usize;
        let mut ids = storage.scan_accelerator.positions.resolve_window(
            bitmap,
            offset,
            limit.saturating_add(1),
        );
        let has_more = ids.len() > limit;
        ids.truncate(limit);

        let docs_partition = storage.get_docs_partition(collection)?;
        let mut docs = Vec::with_capacity(ids.len());
        for doc_id in &ids {
            if let Ok(Some(bytes)) = storage.engine.get(&docs_partition, doc_id.as_bytes())
                && let Ok(doc) = serde_json::from_slice::<Value>(&bytes)
            {
                docs.push(doc);
            }
        }
        let docs_scanned = ids.len() as u64;
        let docs = if let Some(ref fields) = query.fields {
            docs.iter().map(|doc| project_fields(doc, fields)).collect()
        } else {
            docs
        };
        return Ok(QueryResult {
            docs,
            total_count: Some(total),
            docs_scanned,
            index_used: None,
            scan_strategy: Some(plan.scan.name().to_string()),
            has_more,
            next_cursor: None,
        });
    }

    // Resolve every matching position to its id under ONE short guard —
    // the per-position lookup took the position read lock once per doc, and
    // holding any single guard across the get() IO loop is the b965de5
    // deadlock pattern. The id list (not the docs) is the only thing
    // materialized; hydration streams through the sinks.
    let ids = storage
        .scan_accelerator
        .positions
        .resolve_window(bitmap, 0, usize::MAX);

    execute_id_stream(storage, collection, query, plan, &ids, None)
}

fn project_fields(doc: &Value, fields: &[String]) -> Value {
    let mut result = serde_json::Map::new();

    // Always include _id
    if let Some(id) = doc.get("_id") {
        result.insert("_id".to_string(), id.clone());
    }

    for field in fields {
        if let Some(val) = resolve_json_path(doc, field) {
            result.insert(field.clone(), val.clone());
        }
    }

    Value::Object(result)
}
