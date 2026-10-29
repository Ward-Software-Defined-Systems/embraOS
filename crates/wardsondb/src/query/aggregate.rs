use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use serde_json::Value;

const COLLECT_CAP: usize = 1000;
const MAX_PIPELINE_STAGES: usize = 100;

use crate::engine::backend::StorageBackend;
use crate::engine::storage::Storage;
use crate::error::AppError;
use crate::index::secondary::compare_values_total;

use super::executor::execute_query;
use super::filter::{parse_filter, resolve_json_path};
use super::parser::ParsedQuery;
use super::sort::sort_documents;

#[derive(Debug, Deserialize)]
pub struct AggregateRequest {
    pub pipeline: Vec<Value>,
}

#[derive(Debug)]
pub struct AggregateResult {
    pub docs: Vec<Value>,
    pub docs_scanned: u64,
    pub groups: u64,
    pub index_used: Option<String>,
    pub scan_strategy: Option<String>,
}

pub fn execute_aggregate(
    storage: &Storage,
    collection: &str,
    request: &AggregateRequest,
) -> Result<AggregateResult, AppError> {
    if request.pipeline.is_empty() {
        return Err(AppError::InvalidPipeline("Pipeline cannot be empty".into()));
    }
    // Uniform 404 contract regardless of which path serves the pipeline —
    // the bitmap/index-only fast paths never touch the doc partition (F3).
    storage.ensure_collection_exists(collection)?;

    if request.pipeline.len() > MAX_PIPELINE_STAGES {
        return Err(AppError::InvalidPipeline(format!(
            "Pipeline limited to {MAX_PIPELINE_STAGES} stages"
        )));
    }

    // Try bitmap-only aggregation (group by bitmap field with $count only)
    if let Some(result) = try_bitmap_aggregate(storage, collection, &request.pipeline)? {
        return Ok(result);
    }

    // Try index-only aggregation (Opt 2: group by indexed field with $count only)
    if let Some(result) = try_index_only_aggregate(storage, collection, &request.pipeline)? {
        return Ok(result);
    }

    // The first stage decides what stays resident: a $match goes through
    // the query planner (index lookups); a $group streams the scan into its
    // accumulators, so the groups are the resident set and never the
    // collection; anything else materializes the collection as before.
    let (mut docs, docs_scanned, index_used, skip_first, mut groups) = {
        let (first_name, first_spec) = stage_entry(&request.pipeline[0], 0)?;
        match first_name {
            "$match" => {
                let filter = parse_filter(first_spec)?;
                let parsed = ParsedQuery {
                    filter: Some(filter),
                    sort: vec![],
                    limit: u64::MAX,
                    offset: 0,
                    fields: None,
                    count_only: false,
                    cursor: None,
                };
                let result = execute_query(storage, collection, &parsed)?;
                (result.docs, result.docs_scanned, result.index_used, true, 0)
            }
            "$group" => {
                let mut group = GroupAccumulator::new(first_spec, 0)?;
                let mut scanned = 0u64;
                let mut group_err: Option<AppError> = None;
                storage.for_each_document(collection, &mut |doc| {
                    scanned += 1;
                    match group.accumulate(&doc) {
                        Ok(()) => std::ops::ControlFlow::Continue(()),
                        Err(e) => {
                            group_err = Some(e);
                            std::ops::ControlFlow::Break(())
                        }
                    }
                })?;
                if let Some(e) = group_err {
                    return Err(e);
                }
                let docs = group.finish();
                let groups = docs.len() as u64;
                (docs, scanned, None, true, groups)
            }
            _ => {
                let all_docs = storage.scan_all_documents(collection)?;
                let scanned = all_docs.len() as u64;
                (all_docs, scanned, None, false, 0)
            }
        }
    };

    let stages = if skip_first {
        &request.pipeline[1..]
    } else {
        &request.pipeline[..]
    };

    for (raw_i, stage) in stages.iter().enumerate() {
        let i = if skip_first { raw_i + 1 } else { raw_i };
        let (stage_name, stage_spec) = stage_entry(stage, i)?;

        match stage_name {
            "$match" => {
                let filter = parse_filter(stage_spec)?;
                docs.retain(|d| filter.matches(d));
            }
            "$group" => {
                docs = execute_group(stage_spec, &docs, i)?;
                groups = docs.len() as u64;
            }
            name => {
                if !apply_terminal_stage(name, stage_spec, &mut docs, i)? {
                    return Err(AppError::InvalidPipeline(format!(
                        "Stage {i}: unknown stage '{name}'"
                    )));
                }
            }
        }
    }

    Ok(AggregateResult {
        docs,
        docs_scanned,
        groups,
        index_used,
        scan_strategy: None,
    })
}

/// A pipeline stage as `{"$name": spec}`, checked where it stands.
fn stage_entry(stage: &Value, i: usize) -> Result<(&str, &Value), AppError> {
    let obj = stage
        .as_object()
        .ok_or_else(|| AppError::InvalidPipeline(format!("Stage {i} must be an object")))?;
    if obj.len() != 1 {
        return Err(AppError::InvalidPipeline(format!(
            "Stage {i} must have exactly one key"
        )));
    }
    let (name, spec) = obj.iter().next().unwrap();
    Ok((name.as_str(), spec))
}

/// Apply a `$sort`, `$limit` or `$skip` stage to `docs`. `Ok(false)` for
/// any other stage, which the caller names as unknown where it stands.
/// The three paths that end on these stages — the document scan, the
/// index-only and the bitmap aggregates — share it, so a stage behaves the
/// same whichever served the groups.
fn apply_terminal_stage(
    stage_name: &str,
    stage_spec: &Value,
    docs: &mut Vec<Value>,
    i: usize,
) -> Result<bool, AppError> {
    match stage_name {
        "$sort" => {
            let sort_spec = parse_sort_stage(stage_spec, i)?;
            sort_documents(docs, &sort_spec);
        }
        "$limit" => {
            let limit = stage_spec.as_u64().ok_or_else(|| {
                AppError::InvalidPipeline(format!(
                    "Stage {i}: $limit must be a positive integer"
                ))
            })? as usize;
            docs.truncate(limit);
        }
        "$skip" => {
            let skip = stage_spec.as_u64().ok_or_else(|| {
                AppError::InvalidPipeline(format!(
                    "Stage {i}: $skip must be a positive integer"
                ))
            })? as usize;
            // In place: the old `into_iter().skip(n).collect()` reallocated
            // the whole vector to drop a prefix.
            docs.drain(..skip.min(docs.len()));
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// A `$group` stage's state while documents stream through it: the parsed
/// accumulators, one `GroupState` per key seen, the keys in first-seen
/// order. `execute_group` feeds it a materialized slice; the `$group`-first
/// path of `execute_aggregate` feeds it the scan.
struct GroupAccumulator {
    id_spec: Value,
    accumulators: Vec<(String, AccumulatorDef)>,
    group_map: HashMap<String, GroupState>,
    group_order: Vec<String>,
}

impl GroupAccumulator {
    fn new(spec: &Value, stage_idx: usize) -> Result<Self, AppError> {
        let obj = spec.as_object().ok_or_else(|| {
            AppError::InvalidPipeline(format!("Stage {stage_idx}: $group must be an object"))
        })?;

        let id_spec = obj.get("_id").ok_or_else(|| {
            AppError::InvalidPipeline(format!("Stage {stage_idx}: $group requires '_id' field"))
        })?;

        // Parse accumulators (everything except _id)
        let mut accumulators: Vec<(String, AccumulatorDef)> = Vec::new();
        for (key, value) in obj {
            if key == "_id" {
                continue;
            }
            let acc = parse_accumulator(value, stage_idx, key)?;
            accumulators.push((key.clone(), acc));
        }

        Ok(GroupAccumulator {
            id_spec: id_spec.clone(),
            accumulators,
            group_map: HashMap::new(),
            group_order: Vec::new(),
        })
    }

    fn accumulate(&mut self, doc: &Value) -> Result<(), AppError> {
        let group_key = extract_group_key(&self.id_spec, doc);
        // Serialize failure is practically unreachable for a Value, but "" as a
        // fallback key would silently merge every failing doc into one group.
        let key_str = serde_json::to_string(&group_key)
            .map_err(|e| AppError::Internal(format!("failed to serialize $group key: {e}")))?;

        let accumulators = &self.accumulators;
        let group_order = &mut self.group_order;
        let state = self.group_map.entry(key_str.clone()).or_insert_with(|| {
            group_order.push(key_str.clone());
            GroupState {
                id_value: group_key,
                accumulators: accumulators
                    .iter()
                    .map(|(_, def)| AccumulatorState::new(def))
                    .collect(),
            }
        });

        for (idx, (_, def)) in accumulators.iter().enumerate() {
            state.accumulators[idx].accumulate(doc, def)?;
        }
        Ok(())
    }

    /// The result documents, in insertion order.
    fn finish(self) -> Vec<Value> {
        let mut result = Vec::with_capacity(self.group_order.len());
        for key_str in &self.group_order {
            let state = &self.group_map[key_str];
            let mut doc = serde_json::Map::new();
            doc.insert("_id".to_string(), state.id_value.clone());

            for (idx, (name, _)) in self.accumulators.iter().enumerate() {
                doc.insert(name.clone(), state.accumulators[idx].finalize());
            }

            result.push(Value::Object(doc));
        }
        result
    }
}

fn execute_group(spec: &Value, docs: &[Value], stage_idx: usize) -> Result<Vec<Value>, AppError> {
    let mut group = GroupAccumulator::new(spec, stage_idx)?;
    for doc in docs {
        group.accumulate(doc)?;
    }
    Ok(group.finish())
}

fn extract_group_key(id_spec: &Value, doc: &Value) -> Value {
    match id_spec {
        Value::Null => Value::Null,
        Value::String(field) => {
            // Field path reference
            resolve_json_path(doc, field)
                .cloned()
                .unwrap_or(Value::Null)
        }
        Value::Object(obj) => {
            // Multi-field grouping: {"type": "event_type", "action": "network.action"}
            let mut result = serde_json::Map::new();
            for (alias, field_spec) in obj {
                let val = if let Some(field) = field_spec.as_str() {
                    resolve_json_path(doc, field)
                        .cloned()
                        .unwrap_or(Value::Null)
                } else {
                    field_spec.clone()
                };
                result.insert(alias.clone(), val);
            }
            Value::Object(result)
        }
        // Literal value as group key
        other => other.clone(),
    }
}

#[derive(Debug)]
enum AccumulatorDef {
    Count,
    Sum(String),     // field path
    Avg(String),     // field path
    Min(String),     // field path
    Max(String),     // field path
    Collect(String), // field path — collect unique values into array
}

fn parse_accumulator(
    value: &Value,
    stage_idx: usize,
    field_name: &str,
) -> Result<AccumulatorDef, AppError> {
    let obj = value.as_object().ok_or_else(|| {
        AppError::InvalidPipeline(format!(
            "Stage {stage_idx}: accumulator '{field_name}' must be an object"
        ))
    })?;

    if obj.len() != 1 {
        return Err(AppError::InvalidPipeline(format!(
            "Stage {stage_idx}: accumulator '{field_name}' must have exactly one operator"
        )));
    }

    let (op, operand) = obj.iter().next().unwrap();

    match op.as_str() {
        "$count" => Ok(AccumulatorDef::Count),
        "$sum" => {
            let field = operand.as_str().ok_or_else(|| {
                AppError::InvalidPipeline(format!(
                    "Stage {stage_idx}: $sum requires a field path string"
                ))
            })?;
            Ok(AccumulatorDef::Sum(field.to_string()))
        }
        "$avg" => {
            let field = operand.as_str().ok_or_else(|| {
                AppError::InvalidPipeline(format!(
                    "Stage {stage_idx}: $avg requires a field path string"
                ))
            })?;
            Ok(AccumulatorDef::Avg(field.to_string()))
        }
        "$min" => {
            let field = operand.as_str().ok_or_else(|| {
                AppError::InvalidPipeline(format!(
                    "Stage {stage_idx}: $min requires a field path string"
                ))
            })?;
            Ok(AccumulatorDef::Min(field.to_string()))
        }
        "$max" => {
            let field = operand.as_str().ok_or_else(|| {
                AppError::InvalidPipeline(format!(
                    "Stage {stage_idx}: $max requires a field path string"
                ))
            })?;
            Ok(AccumulatorDef::Max(field.to_string()))
        }
        "$collect" => {
            let field = operand.as_str().ok_or_else(|| {
                AppError::InvalidPipeline(format!(
                    "Stage {stage_idx}: $collect requires a field path string"
                ))
            })?;
            Ok(AccumulatorDef::Collect(field.to_string()))
        }
        other => Err(AppError::InvalidPipeline(format!(
            "Stage {stage_idx}: unknown accumulator '{other}'"
        ))),
    }
}

#[derive(Debug)]
enum AccumulatorState {
    Count(u64),
    Sum(f64),
    Avg {
        sum: f64,
        count: u64,
    },
    Min(Option<Value>),
    Max(Option<Value>),
    Collect {
        values: HashSet<String>,
        truncated: bool,
    },
}

impl AccumulatorState {
    fn new(def: &AccumulatorDef) -> Self {
        match def {
            AccumulatorDef::Count => AccumulatorState::Count(0),
            AccumulatorDef::Sum(_) => AccumulatorState::Sum(0.0),
            AccumulatorDef::Avg(_) => AccumulatorState::Avg { sum: 0.0, count: 0 },
            AccumulatorDef::Min(_) => AccumulatorState::Min(None),
            AccumulatorDef::Max(_) => AccumulatorState::Max(None),
            AccumulatorDef::Collect(_) => AccumulatorState::Collect {
                values: HashSet::new(),
                truncated: false,
            },
        }
    }

    fn accumulate(&mut self, doc: &Value, def: &AccumulatorDef) -> Result<(), AppError> {
        match (self, def) {
            (AccumulatorState::Count(c), AccumulatorDef::Count) => {
                *c += 1;
            }
            (AccumulatorState::Sum(s), AccumulatorDef::Sum(field)) => {
                if let Some(val) = resolve_json_path(doc, field)
                    && let Some(n) = val.as_f64()
                {
                    *s += n;
                }
            }
            (AccumulatorState::Avg { sum, count }, AccumulatorDef::Avg(field)) => {
                if let Some(val) = resolve_json_path(doc, field)
                    && let Some(n) = val.as_f64()
                {
                    *sum += n;
                    *count += 1;
                }
            }
            (AccumulatorState::Min(current), AccumulatorDef::Min(field)) => {
                // Compare borrowed, clone only when the value becomes the new
                // extreme — not once per document.
                if let Some(val) = resolve_json_path(doc, field) {
                    match current {
                        None => *current = Some(val.clone()),
                        Some(cur) => {
                            if compare_values_total(val, cur) == std::cmp::Ordering::Less {
                                *current = Some(val.clone());
                            }
                        }
                    }
                }
            }
            (AccumulatorState::Max(current), AccumulatorDef::Max(field)) => {
                if let Some(val) = resolve_json_path(doc, field) {
                    match current {
                        None => *current = Some(val.clone()),
                        Some(cur) => {
                            if compare_values_total(val, cur) == std::cmp::Ordering::Greater {
                                *current = Some(val.clone());
                            }
                        }
                    }
                }
            }
            (AccumulatorState::Collect { values, truncated }, AccumulatorDef::Collect(field)) => {
                if !*truncated && let Some(val) = resolve_json_path(doc, field) {
                    // "" as a fallback key would silently collapse distinct
                    // failing values into one collected entry.
                    let key = serde_json::to_string(val).map_err(|e| {
                        AppError::Internal(format!("failed to serialize $collect value: {e}"))
                    })?;
                    if values.len() < COLLECT_CAP {
                        values.insert(key);
                    } else if !values.contains(&key) {
                        *truncated = true;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn finalize(&self) -> Value {
        match self {
            AccumulatorState::Count(c) => Value::Number((*c).into()),
            AccumulatorState::Sum(s) => serde_json::Number::from_f64(*s)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            AccumulatorState::Avg { sum, count } => {
                if *count == 0 {
                    Value::Null
                } else {
                    let avg = *sum / *count as f64;
                    serde_json::Number::from_f64(avg)
                        .map(Value::Number)
                        .unwrap_or(Value::Null)
                }
            }
            AccumulatorState::Min(val) => val.clone().unwrap_or(Value::Null),
            AccumulatorState::Max(val) => val.clone().unwrap_or(Value::Null),
            AccumulatorState::Collect { values, truncated } => {
                let mut sorted: Vec<Value> = values
                    .iter()
                    .filter_map(|s| serde_json::from_str(s).ok())
                    .collect();
                sorted.sort_by(compare_values_total);
                if *truncated {
                    serde_json::json!({"values": sorted, "_truncated": true})
                } else {
                    Value::Array(sorted)
                }
            }
        }
    }
}

struct GroupState {
    id_value: Value,
    accumulators: Vec<AccumulatorState>,
}

/// Try to execute an aggregation entirely from an index (0 docs scanned).
/// Works when: pipeline starts with $group, _id is a single indexed field,
/// all accumulators are $count only.
fn try_index_only_aggregate(
    storage: &Storage,
    collection: &str,
    pipeline: &[Value],
) -> Result<Option<AggregateResult>, AppError> {
    // Find $group stage — must be first (no $match for index-only)
    let first = pipeline.first().and_then(|v| v.as_object());
    let first = match first {
        Some(o) if o.len() == 1 => o,
        _ => return Ok(None),
    };

    let group_spec = match first.get("$group") {
        Some(spec) => spec,
        None => return Ok(None),
    };

    let group_obj = match group_spec.as_object() {
        Some(o) => o,
        None => return Ok(None),
    };

    // _id must be a string (single field path)
    let group_field = match group_obj.get("_id").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => return Ok(None),
    };

    // All accumulators must be $count
    let mut acc_names: Vec<String> = Vec::new();
    for (key, value) in group_obj {
        if key == "_id" {
            continue;
        }
        let acc_obj = match value.as_object() {
            Some(o) if o.len() == 1 && o.contains_key("$count") => o,
            _ => return Ok(None),
        };
        let _ = acc_obj;
        acc_names.push(key.clone());
    }

    // The group field must have a single-field index
    let (def, partition) = match storage
        .index_manager
        .get_index_for_field(collection, group_field)
    {
        Some(pair) => pair,
        None => return Ok(None),
    };
    if def.is_compound() {
        return Ok(None);
    }

    // Execute index-only aggregation: iterate index keys, group by value bytes
    let mut group_counts: Vec<(Vec<u8>, u64)> = Vec::new();
    let mut current_value: Option<Vec<u8>> = None;
    let mut current_count: u64 = 0;
    let mut seen = 0u64;
    let mut scan_err: Option<AppError> = None;

    storage.engine.scan_full(&partition, &mut |key, _| {
        if let Err(e) = super::cancel::checkpoint(&mut seen) {
            scan_err = Some(e);
            return std::ops::ControlFlow::Break(());
        }
        // Single-field index key: {value_bytes}\x00{doc_id}. Doc ids are
        // NUL-free (validate_custom_id) but NOT fixed-width — custom _ids can
        // be 1..512 bytes — so the separator is the LAST 0x00, never a fixed
        // offset. The value's own encoding may legitimately contain 0x00
        // bytes (null's type prefix, number payloads, embedded NULs in
        // strings), which is why we split on the last one, exactly like
        // extract_doc_id_from_key.
        let Some(sep) = key.iter().rposition(|&b| b == 0x00) else {
            return std::ops::ControlFlow::Continue(()); // malformed key — no separator
        };
        let value_part = &key[..sep];

        match &current_value {
            Some(cv) if cv.as_slice() == value_part => {
                current_count += 1;
            }
            _ => {
                if let Some(cv) = current_value.take() {
                    group_counts.push((cv, current_count));
                }
                // Only group CHANGES copy the value bytes; within a run the
                // comparison stays on the borrowed key.
                current_value = Some(value_part.to_vec());
                current_count = 1;
            }
        }
        std::ops::ControlFlow::Continue(())
    })?;
    if let Some(e) = scan_err {
        return Err(e);
    }
    if let Some(cv) = current_value {
        group_counts.push((cv, current_count));
    }

    // Build result documents
    let mut docs: Vec<Value> = group_counts
        .iter()
        .map(|(encoded_value, count)| {
            let group_value = crate::index::secondary::decode_sortable_bytes(encoded_value)
                .unwrap_or(Value::Null);
            let mut doc = serde_json::Map::new();
            doc.insert("_id".to_string(), group_value);
            for name in &acc_names {
                doc.insert(name.clone(), Value::Number((*count).into()));
            }
            Value::Object(doc)
        })
        .collect();

    let groups = docs.len() as u64;

    // Apply remaining pipeline stages ($sort, $limit, $skip)
    apply_terminal_stages(&pipeline[1..], 1, &mut docs)?;

    Ok(Some(AggregateResult {
        docs,
        docs_scanned: 0,
        groups,
        index_used: Some(def.name),
        scan_strategy: Some("index_only_aggregate".to_string()),
    }))
}

/// Try to execute an aggregation entirely from bitmap columns (0 docs scanned).
/// Handles two patterns:
/// 1. $group by bitmap field with $count only (no $match)
/// 2. $match on bitmap field(s) + $group by bitmap field with $count only
fn try_bitmap_aggregate(
    storage: &Storage,
    collection: &str,
    pipeline: &[Value],
) -> Result<Option<AggregateResult>, AppError> {
    let accelerator = &storage.scan_accelerator;
    if !accelerator.is_ready() {
        return Ok(None);
    }

    // Determine if first stage is $match or $group
    let first = pipeline.first().and_then(|v| v.as_object());
    let first = match first {
        Some(o) if o.len() == 1 => o,
        _ => return Ok(None),
    };

    let (match_filter, group_stage_idx) = if let Some(match_spec) = first.get("$match") {
        // Pattern 2: $match + $group
        if pipeline.len() < 2 {
            return Ok(None);
        }
        let filter = parse_filter(match_spec)?;
        (Some(filter), 1)
    } else if first.contains_key("$group") {
        // Pattern 1: $group only
        (None, 0)
    } else {
        return Ok(None);
    };

    // Get the $group spec
    let group_stage = pipeline
        .get(group_stage_idx)
        .and_then(|v| v.as_object())
        .and_then(|o| if o.len() == 1 { o.get("$group") } else { None });
    let group_obj = match group_stage.and_then(|v| v.as_object()) {
        Some(o) => o,
        None => return Ok(None),
    };

    // _id must be a string (single field path)
    let group_field = match group_obj.get("_id").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => return Ok(None),
    };

    // Must have a bitmap column for the group field
    if !accelerator.has_column(group_field) {
        return Ok(None);
    }

    // All accumulators must be $count
    let mut acc_names: Vec<String> = Vec::new();
    for (key, value) in group_obj {
        if key == "_id" {
            continue;
        }
        let acc_obj = match value.as_object() {
            Some(o) if o.len() == 1 && o.contains_key("$count") => o,
            _ => return Ok(None),
        };
        let _ = acc_obj;
        acc_names.push(key.clone());
    }

    // Execute bitmap aggregation
    let counts = if let Some(ref filter) = match_filter {
        // Pattern 2: $match + $group
        let bitmap_result = match accelerator.bitmap_scan(collection, filter) {
            Some(r) if r.residual_filter.is_none() => r,
            _ => return Ok(None), // Can't fully resolve $match via bitmaps
        };
        match accelerator.count_by_field_filtered(group_field, &bitmap_result.bitmap) {
            Some(c) => c,
            None => return Ok(None),
        }
    } else {
        // Pattern 1: $group only
        match accelerator.count_by_field(collection, group_field) {
            Some(c) => c,
            None => return Ok(None),
        }
    };

    // Build result documents
    let mut docs: Vec<Value> = counts
        .iter()
        .map(|(value_key, count)| {
            let group_value = string_key_to_value(value_key);
            let mut doc = serde_json::Map::new();
            doc.insert("_id".to_string(), group_value);
            for name in &acc_names {
                doc.insert(name.clone(), Value::Number((*count).into()));
            }
            Value::Object(doc)
        })
        .collect();

    let groups = docs.len() as u64;

    let strategy = if match_filter.is_some() {
        "bitmap_filtered_aggregate"
    } else {
        "bitmap_aggregate"
    };

    // Apply remaining pipeline stages ($sort, $limit, $skip)
    let remaining_start = group_stage_idx + 1;
    apply_terminal_stages(&pipeline[remaining_start..], remaining_start, &mut docs)?;

    Ok(Some(AggregateResult {
        docs,
        docs_scanned: 0,
        groups,
        index_used: None,
        scan_strategy: Some(strategy.to_string()),
    }))
}

// Bitmap group keys decode through the exact inverse of the encoder — the
// old local re-parse guessed types and returned a string "123" group as the
// number 123 (S2-4), diverging from the doc-scan aggregate path.
use crate::engine::bitmap::string_key_to_value;

/// The stages after a fast path's `$group`: terminal ones only, each named
/// by its position in the whole pipeline.
fn apply_terminal_stages(
    stages: &[Value],
    first_index: usize,
    docs: &mut Vec<Value>,
) -> Result<(), AppError> {
    for (raw_i, stage) in stages.iter().enumerate() {
        let i = raw_i + first_index;
        let (stage_name, stage_spec) = stage_entry(stage, i)?;
        if !apply_terminal_stage(stage_name, stage_spec, docs, i)? {
            return Err(AppError::InvalidPipeline(format!(
                "Stage {i}: unknown stage '{stage_name}'"
            )));
        }
    }
    Ok(())
}

fn parse_sort_stage(
    spec: &Value,
    stage_idx: usize,
) -> Result<Vec<super::sort::SortField>, AppError> {
    super::sort::parse_sort_spec(spec)
        .map_err(|e| AppError::InvalidPipeline(format!("Stage {stage_idx}: $sort {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::storage::MemoryConfig;
    use serde_json::json;

    fn storage_with(docs: usize) -> (tempfile::TempDir, Storage) {
        let tmp = tempfile::TempDir::new().unwrap();
        let storage =
            Storage::open_with_config(tmp.path(), "fjall", MemoryConfig::default()).unwrap();
        storage.create_collection("c").unwrap();
        let docs: Vec<_> = (0..docs)
            .map(|i| {
                let kind = ["a", "b", "c"][i % 3];
                json!({
                    "kind": kind,
                    "n": i,
                    "tag": format!("t{i}"),
                })
            })
            .collect();
        storage.bulk_insert_documents("c", docs).unwrap();
        (tmp, storage)
    }

    fn aggregate(storage: &Storage, pipeline: Vec<Value>) -> AggregateResult {
        execute_aggregate(storage, "c", &AggregateRequest { pipeline }).unwrap()
    }

    /// A `$group`-first pipeline streams the scan into its accumulators; the
    /// groups, their order, every accumulator (the `$collect` cap included)
    /// and `docs_scanned` are what the materialized stage gives for the same
    /// documents — `execute_group` over `scan_all_documents` is the reference.
    #[test]
    fn a_group_first_pipeline_streams_and_matches_the_materialized_groups() {
        let total = 4 * crate::query::cancel::CHECK_EVERY as usize + 700;
        let (_tmp, storage) = storage_with(total);
        let spec = json!({
            "_id": "kind",
            "count": {"$count": {}},
            "sum": {"$sum": "n"},
            "avg": {"$avg": "n"},
            "min": {"$min": "n"},
            "max": {"$max": "n"},
            "tags": {"$collect": "tag"},
        });

        let streamed = aggregate(&storage, vec![json!({"$group": spec})]);
        let all_docs = storage.scan_all_documents("c").unwrap();
        let reference = execute_group(&spec, &all_docs, 0).unwrap();

        assert_eq!(streamed.docs, reference);
        assert_eq!(streamed.docs_scanned, total as u64);
        assert_eq!(streamed.groups, 3);
        assert_eq!(streamed.index_used, None);
        assert!(streamed.docs[0]["tags"]["_truncated"] == json!(true), "{:?}", streamed.docs[0]);

        // The terminal stages after it read the same way on every path.
        let sorted = aggregate(
            &storage,
            vec![
                json!({"$group": spec}),
                json!({"$sort": [{"count": "desc"}, {"_id": "asc"}]}),
                json!({"$skip": 1}),
                json!({"$limit": 1}),
            ],
        );
        let mut expected = reference.clone();
        sort_documents(
            &mut expected,
            &super::super::sort::parse_sort_spec(&json!([{"count": "desc"}, {"_id": "asc"}])).unwrap(),
        );
        assert_eq!(sorted.docs, vec![expected[1].clone()]);
        assert_eq!(sorted.groups, 3);
    }

    /// `$skip` past the end empties the set; an unknown terminal stage is
    /// named where it stands, whichever path served the groups.
    #[test]
    fn terminal_stages_skip_in_place_and_name_an_unknown_stage() {
        let (_tmp, storage) = storage_with(30);
        let group = json!({"$group": {"_id": "kind", "count": {"$count": {}}}});
        let skipped = aggregate(&storage, vec![group.clone(), json!({"$skip": 10})]);
        assert!(skipped.docs.is_empty());
        let err = execute_aggregate(
            &storage,
            "c",
            &AggregateRequest {
                pipeline: vec![group, json!({"$explode": 1})],
            },
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "Invalid pipeline: Stage 1: unknown stage '$explode'");
    }
}
