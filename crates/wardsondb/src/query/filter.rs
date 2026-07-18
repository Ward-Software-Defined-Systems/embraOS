use serde_json::Value;

use crate::error::AppError;

const MAX_FILTER_DEPTH: usize = 20;
const MAX_FILTER_BRANCHES: usize = 1000;
const MAX_REGEX_PATTERN_LEN: usize = 1024;
const MAX_DOT_DEPTH: usize = 20;

#[derive(Debug, Clone)]
pub enum FilterNode {
    /// Simple field comparison: field_path, operator, value
    Comparison {
        field: String,
        op: FilterOp,
        value: Value,
    },
    /// `$regex` with a string pattern, compiled once at parse time —
    /// recompiling per document dominated `$regex` scans. Clone shares the
    /// compiled program through the Arc.
    Regex {
        field: String,
        regex: std::sync::Arc<regex::Regex>,
    },
    And(Vec<FilterNode>),
    Or(Vec<FilterNode>),
    Not(Box<FilterNode>),
}

#[derive(Debug, Clone)]
pub enum FilterOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    In,
    Nin,
    Exists,
    Regex,
    Contains,
}

impl FilterNode {
    pub fn matches(&self, doc: &Value) -> bool {
        match self {
            FilterNode::Comparison { field, op, value } => {
                let field_val = resolve_json_path(doc, field);
                evaluate_op(op, field_val, value)
            }
            FilterNode::Regex { field, regex } => match resolve_json_path(doc, field) {
                Some(Value::String(s)) => regex.is_match(s),
                _ => false,
            },
            FilterNode::And(nodes) => nodes.iter().all(|n| n.matches(doc)),
            FilterNode::Or(nodes) => nodes.iter().any(|n| n.matches(doc)),
            FilterNode::Not(node) => !node.matches(doc),
        }
    }
}

pub fn resolve_json_path<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    // Flat field names dominate real filters and need no split machinery.
    // (Sorts already resolve once per doc via DocSortKey decoration; this is
    // the per-predicate filter path.)
    if !path.contains('.') {
        return doc.get(path);
    }
    let mut current = doc;
    for (i, segment) in path.split('.').enumerate() {
        if i >= MAX_DOT_DEPTH {
            return None;
        }
        current = current.get(segment)?;
    }
    Some(current)
}

/// Validate that a dot-notation path doesn't exceed the depth limit.
/// Returns Err for paths deeper than MAX_DOT_DEPTH.
pub fn validate_path_depth(path: &str) -> Result<(), AppError> {
    if path.matches('.').count() >= MAX_DOT_DEPTH {
        return Err(AppError::InvalidQuery(format!(
            "Field path depth exceeds maximum of {MAX_DOT_DEPTH}"
        )));
    }
    Ok(())
}

fn evaluate_op(op: &FilterOp, field_val: Option<&Value>, operand: &Value) -> bool {
    match op {
        FilterOp::Exists => {
            let should_exist = operand.as_bool().unwrap_or(true);
            field_val.is_some() == should_exist
        }
        _ => {
            let Some(field_val) = field_val else {
                return false;
            };
            match op {
                FilterOp::Eq => values_equal(field_val, operand),
                FilterOp::Ne => !values_equal(field_val, operand),
                FilterOp::Gt => {
                    compare_values(field_val, operand) == Some(std::cmp::Ordering::Greater)
                }
                FilterOp::Gte => matches!(
                    compare_values(field_val, operand),
                    Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
                ),
                FilterOp::Lt => {
                    compare_values(field_val, operand) == Some(std::cmp::Ordering::Less)
                }
                FilterOp::Lte => matches!(
                    compare_values(field_val, operand),
                    Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
                ),
                FilterOp::In => {
                    if let Value::Array(arr) = operand {
                        arr.iter().any(|v| values_equal(field_val, v))
                    } else {
                        false
                    }
                }
                FilterOp::Nin => {
                    if let Value::Array(arr) = operand {
                        !arr.iter().any(|v| values_equal(field_val, v))
                    } else {
                        true
                    }
                }
                FilterOp::Regex => {
                    // String patterns compile at parse time into
                    // FilterNode::Regex; a Comparison only carries this op
                    // for non-string operands, which can never match.
                    false
                }
                FilterOp::Contains => {
                    if let Value::Array(arr) = field_val {
                        arr.iter().any(|v| values_equal(v, operand))
                    } else {
                        false
                    }
                }
                FilterOp::Exists => unreachable!(),
            }
        }
    }
}

fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => {
            // Compare numerically to handle int/float comparison
            if let (Some(af), Some(bf)) = (a.as_f64(), b.as_f64()) {
                (af - bf).abs() < f64::EPSILON
            } else {
                a == b
            }
        }
        _ => a == b,
    }
}

// DELIBERATE divergence from `index::secondary::compare_values_total`: range
// FILTERS are type-bracketed — a comparison only ever matches values of the
// operand's own comparable type (numbers, strings, bools), and null/array/
// object operands match nothing, even same-type (`None` here means "no
// match"). Ordering surfaces (sort, cursors, $min/$max, $collect) instead use
// the total encoding order. The indexed range path must preserve this
// bracketing — see `index/mod.rs` `lookup_range`/`count_range`.
fn compare_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => {
            let af = a.as_f64()?;
            let bf = b.as_f64()?;
            af.partial_cmp(&bf)
        }
        (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

pub fn parse_filter(filter: &Value) -> Result<FilterNode, AppError> {
    parse_filter_node(filter, 0)
}

fn parse_filter_node(filter: &Value, depth: usize) -> Result<FilterNode, AppError> {
    if depth > MAX_FILTER_DEPTH {
        return Err(AppError::InvalidQuery(format!(
            "Filter nesting depth exceeds maximum of {MAX_FILTER_DEPTH}"
        )));
    }

    let obj = filter
        .as_object()
        .ok_or_else(|| AppError::InvalidQuery("Filter must be a JSON object".into()))?;

    let mut conditions: Vec<FilterNode> = Vec::new();

    for (key, value) in obj {
        match key.as_str() {
            "$and" => {
                let arr = value
                    .as_array()
                    .ok_or_else(|| AppError::InvalidQuery("$and must be an array".into()))?;
                if arr.len() > MAX_FILTER_BRANCHES {
                    return Err(AppError::InvalidQuery(format!(
                        "Filter branch count exceeds maximum of {MAX_FILTER_BRANCHES}"
                    )));
                }
                let nodes: Result<Vec<FilterNode>, _> = arr
                    .iter()
                    .map(|v| parse_filter_node(v, depth + 1))
                    .collect();
                conditions.push(FilterNode::And(nodes?));
            }
            "$or" => {
                let arr = value
                    .as_array()
                    .ok_or_else(|| AppError::InvalidQuery("$or must be an array".into()))?;
                if arr.len() > MAX_FILTER_BRANCHES {
                    return Err(AppError::InvalidQuery(format!(
                        "Filter branch count exceeds maximum of {MAX_FILTER_BRANCHES}"
                    )));
                }
                let nodes: Result<Vec<FilterNode>, _> = arr
                    .iter()
                    .map(|v| parse_filter_node(v, depth + 1))
                    .collect();
                conditions.push(FilterNode::Or(nodes?));
            }
            "$not" => {
                let node = parse_filter_node(value, depth + 1)?;
                conditions.push(FilterNode::Not(Box::new(node)));
            }
            field => {
                // Validate field path depth
                validate_path_depth(field)?;
                // Field-level filter
                let field_conditions = parse_field_filter(field, value)?;
                conditions.extend(field_conditions);
            }
        }
    }

    if conditions.len() == 1 {
        Ok(conditions.into_iter().next().unwrap())
    } else {
        Ok(FilterNode::And(conditions))
    }
}

fn parse_field_filter(field: &str, value: &Value) -> Result<Vec<FilterNode>, AppError> {
    match value {
        Value::Object(ops) => {
            let mut conditions = Vec::new();
            for (op, operand) in ops {
                let filter_op = match op.as_str() {
                    "$eq" => FilterOp::Eq,
                    "$ne" => FilterOp::Ne,
                    "$gt" => FilterOp::Gt,
                    "$gte" => FilterOp::Gte,
                    "$lt" => FilterOp::Lt,
                    "$lte" => FilterOp::Lte,
                    "$in" => FilterOp::In,
                    "$nin" => FilterOp::Nin,
                    "$exists" => FilterOp::Exists,
                    "$regex" => {
                        // Validate AND compile at parse time (the `regex`
                        // crate guarantees linear-time matching); execution
                        // reuses the compiled program.
                        if let Some(pat) = operand.as_str() {
                            if pat.len() > MAX_REGEX_PATTERN_LEN {
                                return Err(AppError::InvalidQuery(format!(
                                    "Regex pattern exceeds maximum length of {MAX_REGEX_PATTERN_LEN}"
                                )));
                            }
                            match regex::Regex::new(pat) {
                                Ok(re) => {
                                    conditions.push(FilterNode::Regex {
                                        field: field.to_string(),
                                        regex: std::sync::Arc::new(re),
                                    });
                                    continue;
                                }
                                Err(_) => {
                                    return Err(AppError::InvalidQuery(format!(
                                        "Invalid regex pattern: {pat}"
                                    )));
                                }
                            }
                        }
                        // Non-string operand keeps the always-false
                        // Comparison, matching pre-compile behavior.
                        FilterOp::Regex
                    }
                    "$contains" => FilterOp::Contains,
                    other => {
                        return Err(AppError::InvalidQuery(format!("Unknown operator: {other}")));
                    }
                };
                conditions.push(FilterNode::Comparison {
                    field: field.to_string(),
                    op: filter_op,
                    value: operand.clone(),
                });
            }
            Ok(conditions)
        }
        // Implicit $eq
        _ => Ok(vec![FilterNode::Comparison {
            field: field.to_string(),
            op: FilterOp::Eq,
            value: value.clone(),
        }]),
    }
}
