use std::collections::HashSet;

use serde_json::Value;

use crate::engine::bitmap::ScanAccelerator;
use crate::index::IndexManager;
use crate::index::secondary::value_to_sortable_bytes;

use super::cursor::{Cursor, CursorValue};
use super::filter::{FilterNode, FilterOp};
use super::parser::ParsedQuery;
use super::sort::SortField;

/// What scan strategy to use for a query.
#[derive(Debug)]
pub enum ScanPlan {
    /// Full collection scan — no usable index.
    FullScan,

    /// Index equality scan — one or more exact values.
    IndexEq {
        index_name: String,
        field: String,
        value: Value,
    },

    /// Index $in scan — union of equality lookups.
    IndexIn {
        index_name: String,
        field: String,
        values: Vec<Value>,
    },

    /// Index range scan.
    IndexRange {
        index_name: String,
        field: String,
        lower: Option<(Value, bool)>, // (value, inclusive)
        upper: Option<(Value, bool)>,
    },

    /// Compound equality scan — multiple equality fields covered by one compound index.
    CompoundEq { index_name: String, prefix: Vec<u8> },

    /// Compound index: equality prefix + range on next field.
    /// Example: event_type = "firewall" AND received_at >= "2026-03-12"
    /// Uses idx_type_time to seek to "firewall" prefix, then range scan on received_at.
    CompoundRange {
        index_name: String,
        /// Serialized prefix from equality fields (WITH trailing 0x01 separator)
        eq_prefix: Vec<u8>,
        /// Lower bound for the range field (sortable bytes), and whether inclusive
        lower: Option<(Vec<u8>, bool)>,
        /// Upper bound for the range field (sortable bytes), and whether inclusive
        upper: Option<(Vec<u8>, bool)>,
    },

    /// Sorted index scan with early termination.
    /// A compound index covers both the filter field(s) and all sort fields,
    /// allowing us to iterate in sort order and stop after offset+limit docs.
    IndexSorted {
        index_name: String,
        prefix: Vec<u8>,
        reverse: bool,
        /// True when the eq prefix + sort fields span the ENTIRE index — the
        /// scan's within-tie order is then exactly the comparator's
        /// (`_id`-tiebreak) order, which is what makes cursors sound. With
        /// extra trailing index fields, ties order by (extras, _id) instead,
        /// so such plans neither emit nor accept cursors.
        exact_tail: bool,
    },

    /// Bitmap scan — scan accelerator covers the filter (or part of it).
    /// Carries the matching-positions bitmap computed at plan time so the
    /// executor doesn't recompute the same Roaring AND/OR set. BitmapScan
    /// does NOT help with sort order — after getting bitmap results,
    /// matching docs are loaded and sorted in memory. For queries with sort +
    /// limit where a compound index exists, IndexSorted (higher priority)
    /// will be chosen instead.
    BitmapScan { bitmap: roaring::RoaringBitmap },

    /// `$or` union — every arm of the `$or` is individually servable by a
    /// secondary index; the executor unions the per-arm candidate ids,
    /// dedups, and restores `_id` order so results are byte-identical to
    /// the full scan this replaces (same docs, same order, same pages and
    /// tiling) — only `docs_scanned`/`index_used`/`scan_strategy` change.
    /// Arms are the index-servable ScanPlan variants only (never FullScan,
    /// IndexSorted, BitmapScan, or a nested OrUnion). When any arm
    /// over-approximates its `$or` child (a residual-carrying And arm), the
    /// plan's post_filter is the ORIGINAL `$or` filter — one filter pass
    /// over loaded candidates, trivially equivalent to full-scan filtering.
    OrUnion { arms: Vec<ScanPlan> },
}

impl ScanPlan {
    /// The `scan_strategy` label for this plan. Exhaustive on purpose — a
    /// new variant must pick its wire label here at compile time (scattered
    /// literals plus wildcard matches are how labels silently went stale
    /// before, T8/S3-9). Call sites still decide Some vs None: doc-returning
    /// full scans and plain index scans deliberately report no strategy.
    pub fn name(&self) -> &'static str {
        match self {
            ScanPlan::FullScan => "full_scan",
            ScanPlan::IndexEq { .. } => "index_eq",
            ScanPlan::IndexIn { .. } => "index_in",
            ScanPlan::IndexRange { .. } => "index_range",
            ScanPlan::CompoundEq { .. } => "compound_eq",
            ScanPlan::CompoundRange { .. } => "compound_range",
            ScanPlan::IndexSorted { .. } => "index_sorted",
            ScanPlan::BitmapScan { .. } => "bitmap",
            ScanPlan::OrUnion { .. } => "or_union",
        }
    }
}

/// The result of planning: a scan strategy + optional residual filter.
pub struct QueryPlan {
    pub scan: ScanPlan,
    /// Residual filter to apply to documents after the index scan.
    pub post_filter: Option<FilterNode>,
    /// Full original filter (used for full-scan path).
    pub original_filter: Option<FilterNode>,
}

/// Plan the best scan strategy for a filter, given available indexes.
pub fn plan_query(
    query: &ParsedQuery,
    index_manager: &IndexManager,
    collection: &str,
    scan_accelerator: &ScanAccelerator,
) -> QueryPlan {
    let sort = &query.sort;
    let limit = query.limit;
    let count_only = query.count_only;

    let Some(filter) = &query.filter else {
        return QueryPlan {
            scan: ScanPlan::FullScan,
            post_filter: None,
            original_filter: None,
        };
    };

    // Try IndexSorted first (highest priority — enables early termination)
    // Only when: has sort, finite limit, not count_only.
    if !sort.is_empty()
        && limit < u64::MAX
        && !count_only
        && let Some(plan) = try_index_sorted(
            index_manager,
            collection,
            filter,
            sort,
            query.cursor.as_ref(),
        )
    {
        return plan;
    }

    // S3-14: compute the bitmap plan at most once per plan_query call. The
    // count_only gate below and the per-filter-shape fallback sites used to
    // each redo the Roaring AND/OR work for partially-covered filters; the
    // memo hands the gate's rejected plan to whichever fallback site runs
    // (exactly one does — every match arm returns).
    let mut bitmap_memo: Option<Option<QueryPlan>> = None;

    // For count_only queries, bitmap is ~2500x faster than index counting.
    // Try bitmap before indexes when all filter fields have bitmap columns.
    if count_only {
        match try_bitmap_scan(scan_accelerator, collection, filter) {
            Some(plan) if plan.post_filter.is_none() => return plan,
            other => bitmap_memo = Some(other),
        }
    }

    match filter {
        // Simple comparison — check if the field is indexed
        FilterNode::Comparison { field, op, value } => {
            if let Some(plan) = try_index_comparison(index_manager, collection, field, op, value) {
                return QueryPlan {
                    scan: plan,
                    post_filter: None,
                    original_filter: Some(filter.clone()),
                };
            }
            // Try bitmap scan before full scan
            if let Some(plan) = bitmap_memo
                .take()
                .unwrap_or_else(|| try_bitmap_scan(scan_accelerator, collection, filter))
            {
                return plan;
            }
            QueryPlan {
                scan: ScanPlan::FullScan,
                post_filter: None,
                original_filter: Some(filter.clone()),
            }
        }

        // AND — try compound multi-field eq, then single-field index
        FilterNode::And(children) => {
            // Try compound multi-field equality (Opt 3)
            if let Some(plan) = try_compound_eq(index_manager, collection, children, filter) {
                return plan;
            }

            // Try compound equality prefix + range suffix
            if let Some(plan) = try_compound_range(index_manager, collection, children, filter) {
                return plan;
            }

            // Try each child to see if it can use a single-field index
            for (i, child) in children.iter().enumerate() {
                if let FilterNode::Comparison { field, op, value } = child
                    && let Some(plan) =
                        try_index_comparison(index_manager, collection, field, op, value)
                {
                    let remaining: Vec<FilterNode> = children
                        .iter()
                        .enumerate()
                        .filter(|(j, _)| *j != i)
                        .map(|(_, c)| c.clone())
                        .collect();

                    let post = if remaining.is_empty() {
                        None
                    } else if remaining.len() == 1 {
                        Some(remaining.into_iter().next().unwrap())
                    } else {
                        Some(FilterNode::And(remaining))
                    };

                    return QueryPlan {
                        scan: plan,
                        post_filter: post,
                        original_filter: Some(filter.clone()),
                    };
                }
            }

            // Also try range combination: if we have both $gte and $lte on the same indexed field
            if let Some(plan) = try_range_from_and(index_manager, collection, children) {
                // Build post-filter excluding the range components
                let range_field = match &plan {
                    ScanPlan::IndexRange { field, .. } => field.clone(),
                    _ => String::new(),
                };
                let remaining: Vec<FilterNode> = children
                    .iter()
                    .filter(|c| {
                        if let FilterNode::Comparison { field, op, .. } = c {
                            !(field == &range_field
                                && matches!(
                                    op,
                                    FilterOp::Gt | FilterOp::Gte | FilterOp::Lt | FilterOp::Lte
                                ))
                        } else {
                            true
                        }
                    })
                    .cloned()
                    .collect();

                let post = if remaining.is_empty() {
                    None
                } else if remaining.len() == 1 {
                    Some(remaining.into_iter().next().unwrap())
                } else {
                    Some(FilterNode::And(remaining))
                };

                return QueryPlan {
                    scan: plan,
                    post_filter: post,
                    original_filter: Some(filter.clone()),
                };
            }

            // Try bitmap scan before full scan
            if let Some(plan) = bitmap_memo
                .take()
                .unwrap_or_else(|| try_bitmap_scan(scan_accelerator, collection, filter))
            {
                return plan;
            }
            QueryPlan {
                scan: ScanPlan::FullScan,
                post_filter: None,
                original_filter: Some(filter.clone()),
            }
        }

        // OR — bitmap first (a fully-covered $or is one Roaring union, and
        // partial coverage bails per S3-1), then the per-arm index union,
        // then full scan.
        FilterNode::Or(children) => {
            if let Some(plan) = bitmap_memo
                .take()
                .unwrap_or_else(|| try_bitmap_scan(scan_accelerator, collection, filter))
            {
                return plan;
            }
            if let Some(plan) = try_or_union(index_manager, collection, children, filter) {
                return plan;
            }
            QueryPlan {
                scan: ScanPlan::FullScan,
                post_filter: None,
                original_filter: Some(filter.clone()),
            }
        }

        // NOT cannot efficiently use indexes — try bitmap scan before full scan
        _ => {
            if let Some(plan) = bitmap_memo
                .take()
                .unwrap_or_else(|| try_bitmap_scan(scan_accelerator, collection, filter))
            {
                return plan;
            }
            QueryPlan {
                scan: ScanPlan::FullScan,
                post_filter: None,
                original_filter: Some(filter.clone()),
            }
        }
    }
}

/// Plan one `$or` arm onto an index-servable scan, returning the scan and
/// whether it is EXACT (selects precisely the arm's matches) or an
/// over-approximation (a superset — the caller then post-filters with the
/// original `$or`). Arms that can't be served by an index return None, which
/// disables the union entirely: one unindexable arm already forces a full
/// scan, so a partial union would only add index work on top of it.
fn plan_or_arm(
    index_manager: &IndexManager,
    collection: &str,
    arm: &FilterNode,
) -> Option<(ScanPlan, bool)> {
    match arm {
        FilterNode::Comparison { field, op, value } => {
            try_index_comparison(index_manager, collection, field, op, value)
                .map(|scan| (scan, true))
        }
        FilterNode::And(children) => {
            // Same ladder as the top-level And arm, minus bitmap/full-scan:
            // compound eq, compound eq-prefix + range, single-field eq with
            // the rest as residual, merged range with the rest as residual.
            // Exactness = the sub-plan needed no post-filter.
            if let Some(plan) = try_compound_eq(index_manager, collection, children, arm) {
                let exact = plan.post_filter.is_none();
                return Some((plan.scan, exact));
            }
            if let Some(plan) = try_compound_range(index_manager, collection, children, arm) {
                let exact = plan.post_filter.is_none();
                return Some((plan.scan, exact));
            }
            for child in children {
                if let FilterNode::Comparison { field, op, value } = child
                    && let Some(scan) =
                        try_index_comparison(index_manager, collection, field, op, value)
                {
                    // children.len() >= 2 (parser guarantees And has multiple
                    // nodes), so the untouched siblings make this inexact.
                    return Some((scan, false));
                }
            }
            try_range_from_and(index_manager, collection, children).map(|scan| (scan, false))
        }
        // Nested $or / $not / $regex arms: not index-servable (v1).
        _ => None,
    }
}

/// `$or` whose arms are ALL individually index-servable: plan the per-arm
/// union (see `ScanPlan::OrUnion`). The union is an over-approximation
/// whenever any arm is, in which case the original `$or` filter rides along
/// as the post-filter.
fn try_or_union(
    index_manager: &IndexManager,
    collection: &str,
    children: &[FilterNode],
    filter: &FilterNode,
) -> Option<QueryPlan> {
    if children.is_empty() {
        return None;
    }
    let mut arms = Vec::with_capacity(children.len());
    let mut exact_all = true;
    for child in children {
        let (scan, exact) = plan_or_arm(index_manager, collection, child)?;
        exact_all &= exact;
        arms.push(scan);
    }
    Some(QueryPlan {
        scan: ScanPlan::OrUnion { arms },
        post_filter: (!exact_all).then(|| filter.clone()),
        original_filter: Some(filter.clone()),
    })
}

/// Try to plan an IndexSorted scan using a compound index that covers
/// equality filter fields + the sort field.
fn try_index_sorted(
    index_manager: &IndexManager,
    collection: &str,
    filter: &FilterNode,
    sort: &[SortField],
    cursor: Option<&Cursor>,
) -> Option<QueryPlan> {
    if sort.is_empty() {
        return None;
    }

    // A single index scan direction can only serve uniform sort directions
    // (the scan is forward for all-asc, reverse for all-desc). Mixed
    // directions fall through to a materializing strategy + in-memory sort.
    if !sort.iter().all(|s| s.ascending == sort[0].ascending) {
        return None;
    }

    // Extract equality conditions from the filter
    let eq_pairs = extract_eq_pairs(filter);
    if eq_pairs.is_empty() {
        return None;
    }

    let eq_field_names: Vec<&str> = eq_pairs.iter().map(|(f, _)| f.as_str()).collect();
    let sort_field_names: Vec<&str> = sort.iter().map(|s| s.field.as_str()).collect();

    // The compound index must cover ALL sort fields, in order, right after
    // the eq prefix — otherwise secondary sort fields would be silently
    // ignored (the scan never re-sorts).
    let (idx_def, _, n_matched) =
        index_manager.find_compound_index(collection, &eq_field_names, &sort_field_names)?;

    let exact_tail = n_matched + sort.len() == idx_def.fields.len();

    if let Some(cur) = cursor {
        // A cursor seek needs index order == comparator order, which extra
        // trailing index fields break (ties order by extras, not _id).
        if !exact_tail {
            return None;
        }
        // Docs missing a sort field are not in the compound index at all, so
        // a Missing position cannot be expressed as a seek key.
        if cur
            .sort_values
            .iter()
            .any(|v| matches!(v, CursorValue::Missing))
        {
            return None;
        }
    }

    // Build prefix from matched eq values in index field order
    let mut prefix = Vec::new();
    for i in 0..n_matched {
        let idx_field = &idx_def.fields[i];
        let (_, val) = eq_pairs.iter().find(|(f, _)| f == idx_field)?;
        if i > 0 {
            prefix.push(0x01); // field separator
        }
        prefix.extend_from_slice(&value_to_sortable_bytes(val));
    }
    prefix.push(0x01); // separator before sort field values

    let reverse = !sort[0].ascending;

    // Build post-filter from conditions not covered by the compound index eq fields
    let covered: HashSet<&str> = idx_def.fields[..n_matched]
        .iter()
        .map(|s| s.as_str())
        .collect();
    let remaining = build_remaining_filter(filter, &covered);

    Some(QueryPlan {
        scan: ScanPlan::IndexSorted {
            index_name: idx_def.name,
            prefix,
            reverse,
            exact_tail,
        },
        post_filter: remaining,
        original_filter: Some(filter.clone()),
    })
}

/// Try to use a compound index for multi-field AND equality (Opt 3).
fn try_compound_eq(
    index_manager: &IndexManager,
    collection: &str,
    children: &[FilterNode],
    original: &FilterNode,
) -> Option<QueryPlan> {
    let eq_pairs: Vec<(String, Value)> = children
        .iter()
        .filter_map(|c| {
            if let FilterNode::Comparison {
                field,
                op: FilterOp::Eq,
                value,
            } = c
            {
                Some((field.clone(), value.clone()))
            } else {
                None
            }
        })
        .collect();

    if eq_pairs.len() < 2 {
        return None;
    }

    let eq_field_names: Vec<&str> = eq_pairs.iter().map(|(f, _)| f.as_str()).collect();
    let (idx_def, _, n_matched) =
        index_manager.find_compound_index(collection, &eq_field_names, &[])?;

    // Build compound prefix
    let all_matched = n_matched == idx_def.fields.len();
    let mut prefix = Vec::new();
    for i in 0..n_matched {
        let idx_field = &idx_def.fields[i];
        let (_, val) = eq_pairs.iter().find(|(f, _)| f == idx_field)?;
        if i > 0 {
            prefix.push(0x01); // field separator
        }
        prefix.extend_from_slice(&value_to_sortable_bytes(val));
    }
    prefix.push(if all_matched { 0x00 } else { 0x01 });

    // Build remaining filter
    let covered: HashSet<&str> = idx_def.fields[..n_matched]
        .iter()
        .map(|s| s.as_str())
        .collect();
    let remaining: Vec<FilterNode> = children
        .iter()
        .filter(|c| {
            if let FilterNode::Comparison {
                field,
                op: FilterOp::Eq,
                ..
            } = c
            {
                !covered.contains(field.as_str())
            } else {
                true
            }
        })
        .cloned()
        .collect();

    let post_filter = match remaining.len() {
        0 => None,
        1 => Some(remaining.into_iter().next().unwrap()),
        _ => Some(FilterNode::And(remaining)),
    };

    Some(QueryPlan {
        scan: ScanPlan::CompoundEq {
            index_name: idx_def.name,
            prefix,
        },
        post_filter,
        original_filter: Some(original.clone()),
    })
}

/// Extract (field, value) pairs from equality conditions in a filter.
fn extract_eq_pairs(filter: &FilterNode) -> Vec<(String, Value)> {
    match filter {
        FilterNode::Comparison {
            field,
            op: FilterOp::Eq,
            value,
        } => {
            vec![(field.clone(), value.clone())]
        }
        FilterNode::And(children) => children
            .iter()
            .filter_map(|c| {
                if let FilterNode::Comparison {
                    field,
                    op: FilterOp::Eq,
                    value,
                } = c
                {
                    Some((field.clone(), value.clone()))
                } else {
                    None
                }
            })
            .collect(),
        _ => vec![],
    }
}

/// Build a post-filter from conditions not covered by the given set of fields.
fn build_remaining_filter(filter: &FilterNode, covered: &HashSet<&str>) -> Option<FilterNode> {
    match filter {
        FilterNode::Comparison {
            field,
            op: FilterOp::Eq,
            ..
        } if covered.contains(field.as_str()) => None,
        FilterNode::And(children) => {
            let remaining: Vec<FilterNode> = children
                .iter()
                .filter(|c| {
                    if let FilterNode::Comparison {
                        field,
                        op: FilterOp::Eq,
                        ..
                    } = c
                    {
                        !covered.contains(field.as_str())
                    } else {
                        true
                    }
                })
                .cloned()
                .collect();
            match remaining.len() {
                0 => None,
                1 => Some(remaining.into_iter().next().unwrap()),
                _ => Some(FilterNode::And(remaining)),
            }
        }
        other => Some(other.clone()),
    }
}

/// Try to use a compound index for "eq prefix + range suffix" patterns.
/// Matches queries like: field1 = val1 AND field2 >= val2
/// where a compound index exists on (field1, field2).
fn try_compound_range(
    index_manager: &IndexManager,
    collection: &str,
    children: &[FilterNode],
    original: &FilterNode,
) -> Option<QueryPlan> {
    // Extract equality pairs
    let eq_pairs: Vec<(String, Value)> = children
        .iter()
        .filter_map(|c| {
            if let FilterNode::Comparison {
                field,
                op: FilterOp::Eq,
                value,
            } = c
            {
                Some((field.clone(), value.clone()))
            } else {
                None
            }
        })
        .collect();

    if eq_pairs.is_empty() {
        return None;
    }

    // Extract range conditions: field -> (lower, upper)
    type Bound = Option<(Value, bool)>;
    let mut range_bounds: std::collections::HashMap<String, (Bound, Bound)> =
        std::collections::HashMap::new();

    for child in children {
        if let FilterNode::Comparison { field, op, value } = child {
            match op {
                FilterOp::Gt | FilterOp::Gte => {
                    let inclusive = matches!(op, FilterOp::Gte);
                    range_bounds.entry(field.clone()).or_insert((None, None)).0 =
                        Some((value.clone(), inclusive));
                }
                FilterOp::Lt | FilterOp::Lte => {
                    let inclusive = matches!(op, FilterOp::Lte);
                    range_bounds.entry(field.clone()).or_insert((None, None)).1 =
                        Some((value.clone(), inclusive));
                }
                _ => {}
            }
        }
    }

    if range_bounds.is_empty() {
        return None;
    }

    let eq_field_names: Vec<&str> = eq_pairs.iter().map(|(f, _)| f.as_str()).collect();

    // Try each range field to find a compound index covering eq fields + range field
    let mut best: Option<(
        crate::index::secondary::IndexDef,
        usize,
        String,
        Bound,
        Bound,
    )> = None;

    for (range_field, (lower, upper)) in &range_bounds {
        if let Some((idx_def, _, n_matched)) =
            index_manager.find_compound_range_index(collection, &eq_field_names, range_field)
            && best.as_ref().is_none_or(|(_, bm, _, _, _)| n_matched > *bm)
        {
            best = Some((
                idx_def,
                n_matched,
                range_field.clone(),
                lower.clone(),
                upper.clone(),
            ));
        }
    }

    let (idx_def, n_matched, range_field, lower, upper) = best?;

    // Build eq_prefix from matched eq values in index field order
    let mut eq_prefix = Vec::new();
    for i in 0..n_matched {
        let idx_field = &idx_def.fields[i];
        let (_, val) = eq_pairs.iter().find(|(f, _)| f == idx_field)?;
        if i > 0 {
            eq_prefix.push(0x01); // field separator
        }
        eq_prefix.extend_from_slice(&value_to_sortable_bytes(val));
    }
    eq_prefix.push(0x01); // separator before range field

    // Convert range bounds to sortable bytes
    let lower_bytes = lower.map(|(v, incl)| (value_to_sortable_bytes(&v), incl));
    let upper_bytes = upper.map(|(v, incl)| (value_to_sortable_bytes(&v), incl));

    // Build post-filter from conditions not covered by the compound index
    let covered_eq: HashSet<&str> = idx_def.fields[..n_matched]
        .iter()
        .map(|s| s.as_str())
        .collect();
    let remaining: Vec<FilterNode> = children
        .iter()
        .filter(|c| {
            if let FilterNode::Comparison { field, op, .. } = c {
                // Skip covered eq fields
                if matches!(op, FilterOp::Eq) && covered_eq.contains(field.as_str()) {
                    return false;
                }
                // Skip range conditions on the covered range field
                if field == &range_field
                    && matches!(
                        op,
                        FilterOp::Gt | FilterOp::Gte | FilterOp::Lt | FilterOp::Lte
                    )
                {
                    return false;
                }
            }
            true
        })
        .cloned()
        .collect();

    let post_filter = match remaining.len() {
        0 => None,
        1 => Some(remaining.into_iter().next().unwrap()),
        _ => Some(FilterNode::And(remaining)),
    };

    Some(QueryPlan {
        scan: ScanPlan::CompoundRange {
            index_name: idx_def.name,
            eq_prefix,
            lower: lower_bytes,
            upper: upper_bytes,
        },
        post_filter,
        original_filter: Some(original.clone()),
    })
}

/// Try to use the bitmap scan accelerator for a filter, scoped to the
/// queried collection (the accelerator's position space is global — F1).
fn try_bitmap_scan(
    scan_accelerator: &ScanAccelerator,
    collection: &str,
    filter: &FilterNode,
) -> Option<QueryPlan> {
    if !scan_accelerator.is_ready() {
        return None;
    }
    let result = scan_accelerator.bitmap_scan(collection, filter)?;
    // Only use bitmap scan if the bitmap has a reasonable size (not empty)
    // or if it's a count_only query (where empty is a valid fast result)
    Some(QueryPlan {
        scan: ScanPlan::BitmapScan {
            bitmap: result.bitmap,
        },
        post_filter: result.residual_filter,
        original_filter: Some(filter.clone()),
    })
}

fn try_index_comparison(
    index_manager: &IndexManager,
    collection: &str,
    field: &str,
    op: &FilterOp,
    value: &Value,
) -> Option<ScanPlan> {
    let (def, _) = index_manager.get_index_for_field(collection, field)?;

    match op {
        FilterOp::Eq => Some(ScanPlan::IndexEq {
            index_name: def.name,
            field: field.to_string(),
            value: value.clone(),
        }),
        FilterOp::In => {
            if let Value::Array(values) = value {
                Some(ScanPlan::IndexIn {
                    index_name: def.name,
                    field: field.to_string(),
                    values: values.clone(),
                })
            } else {
                None
            }
        }
        FilterOp::Gt => Some(ScanPlan::IndexRange {
            index_name: def.name,
            field: field.to_string(),
            lower: Some((value.clone(), false)),
            upper: None,
        }),
        FilterOp::Gte => Some(ScanPlan::IndexRange {
            index_name: def.name,
            field: field.to_string(),
            lower: Some((value.clone(), true)),
            upper: None,
        }),
        FilterOp::Lt => Some(ScanPlan::IndexRange {
            index_name: def.name,
            field: field.to_string(),
            lower: None,
            upper: Some((value.clone(), false)),
        }),
        FilterOp::Lte => Some(ScanPlan::IndexRange {
            index_name: def.name,
            field: field.to_string(),
            lower: None,
            upper: Some((value.clone(), true)),
        }),
        _ => None,
    }
}

/// Try to combine $gte/$gt + $lte/$lt on the same indexed field into a single range scan.
fn try_range_from_and(
    index_manager: &IndexManager,
    collection: &str,
    children: &[FilterNode],
) -> Option<ScanPlan> {
    type Bound = Option<(Value, bool)>;
    let mut field_bounds: std::collections::HashMap<String, (Bound, Bound)> =
        std::collections::HashMap::new();

    for child in children {
        if let FilterNode::Comparison { field, op, value } = child {
            match op {
                FilterOp::Gt | FilterOp::Gte => {
                    let inclusive = matches!(op, FilterOp::Gte);
                    field_bounds.entry(field.clone()).or_insert((None, None)).0 =
                        Some((value.clone(), inclusive));
                }
                FilterOp::Lt | FilterOp::Lte => {
                    let inclusive = matches!(op, FilterOp::Lte);
                    field_bounds.entry(field.clone()).or_insert((None, None)).1 =
                        Some((value.clone(), inclusive));
                }
                _ => {}
            }
        }
    }

    // Find a field with both lower and upper bounds that has an index
    for (field, (lower, upper)) in &field_bounds {
        if lower.is_some()
            && upper.is_some()
            && let Some((def, _)) = index_manager.get_index_for_field(collection, field)
        {
            return Some(ScanPlan::IndexRange {
                index_name: def.name,
                field: field.clone(),
                lower: lower.clone(),
                upper: upper.clone(),
            });
        }
    }

    None
}
