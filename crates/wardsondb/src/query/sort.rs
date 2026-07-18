use serde_json::Value;

use super::filter::resolve_json_path;
use crate::index::secondary::compare_values_total;

#[derive(Debug, Clone)]
pub struct SortField {
    pub field: String,
    pub ascending: bool,
}

/// Parse a sort specification. Shared by the `/query` endpoint's `sort` field
/// and the aggregate `$sort` stage so both accept identical shapes:
///
/// - array form: `[{"field": dir}, ...]` — one field per element, priority in
///   array order (`[]` is a no-op)
/// - flat object form: `{"field": dir}` — exactly one field; multiple fields
///   are rejected because JSON object key order is not preserved after parsing
///
/// Directions: `"asc"`, `"desc"`, `1`, `-1` (also `1.0` / `-1.0`). Anything
/// else is an error naming the offending field.
///
/// Error messages are phrased to read naturally after the callers' prefixes
/// ("sort ..." / "Stage {i}: $sort ...").
pub fn parse_sort_spec(spec: &Value) -> Result<Vec<SortField>, String> {
    match spec {
        Value::Array(items) => {
            let mut fields = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                let Value::Object(obj) = item else {
                    return Err(format!(
                        "element {i} must be an object with exactly one field"
                    ));
                };
                if obj.len() != 1 {
                    return Err(format!(
                        "element {i} must have exactly one field (got {}); use one object per field: [{{\"a\": \"asc\"}}, {{\"b\": \"desc\"}}]",
                        obj.len()
                    ));
                }
                let (field, direction) = obj.iter().next().unwrap();
                fields.push(SortField {
                    field: field.clone(),
                    ascending: parse_direction(field, direction)?,
                });
            }
            Ok(fields)
        }
        Value::Object(obj) => match obj.len() {
            1 => {
                let (field, direction) = obj.iter().next().unwrap();
                Ok(vec![SortField {
                    field: field.clone(),
                    ascending: parse_direction(field, direction)?,
                }])
            }
            0 => Err("must not be an empty object".to_string()),
            n => Err(format!(
                "object with {n} fields is ambiguous (JSON object key order is not preserved); use the array form: [{{\"a\": \"asc\"}}, {{\"b\": \"desc\"}}]"
            )),
        },
        _ => Err("must be an array of single-field objects or a single-field object".to_string()),
    }
}

fn parse_direction(field: &str, direction: &Value) -> Result<bool, String> {
    let invalid = || {
        format!(
            "direction for field '{field}' must be \"asc\", \"desc\", 1, or -1 (got {})",
            serde_json::to_string(direction).unwrap_or_else(|_| "?".to_string())
        )
    };
    match direction {
        Value::String(s) if s == "asc" => Ok(true),
        Value::String(s) if s == "desc" => Ok(false),
        // as_f64 covers integer and float encodings; 1.0 and -1.0 are exact.
        Value::Number(n) => match n.as_f64() {
            Some(1.0) => Ok(true),
            Some(-1.0) => Ok(false),
            _ => Err(invalid()),
        },
        _ => Err(invalid()),
    }
}

pub fn sort_documents(docs: &mut [Value], sort_fields: &[SortField]) {
    if sort_fields.is_empty() {
        return;
    }
    docs.sort_by(|a, b| compare_docs(a, b, sort_fields));
}

/// Direction of the implicit `_id` tiebreak for a sort spec: the last sort
/// field's direction (ascending when the spec is empty). This matches index
/// key order — within a full sort-value tie, a forward (all-asc) index scan
/// yields doc ids ascending and a reverse (all-desc) scan yields them
/// descending — so in-memory and index-order results agree byte for byte.
pub fn tiebreak_ascending(sort_fields: &[SortField]) -> bool {
    sort_fields.last().map(|s| s.ascending).unwrap_or(true)
}

/// Total-order comparator: user sort fields, then `_id` tiebreak (see
/// `tiebreak_ascending`). With an empty spec this orders by `_id` ascending.
pub fn compare_docs(a: &Value, b: &Value, sort_fields: &[SortField]) -> std::cmp::Ordering {
    for sf in sort_fields {
        let va = resolve_json_path(a, &sf.field);
        let vb = resolve_json_path(b, &sf.field);

        let ordering = compare_json_values(va, vb);
        let ordering = if sf.ascending {
            ordering
        } else {
            ordering.reverse()
        };

        if ordering != std::cmp::Ordering::Equal {
            return ordering;
        }
    }

    // _id tiebreak. Group results from $group compare here too, but their
    // _ids are unique group keys, so this never reorders them.
    let ia = a.get("_id").and_then(Value::as_str);
    let ib = b.get("_id").and_then(Value::as_str);
    let ordering = ia.cmp(&ib);
    if tiebreak_ascending(sort_fields) {
        ordering
    } else {
        ordering.reverse()
    }
}

/// Missing (`None`) sorts before every present value; present values compare
/// in the database's total order (`compare_values_total`, byte-identical to
/// the index key encoding). Missing stays distinct from present `null`.
pub(crate) fn compare_json_values(a: Option<&Value>, b: Option<&Value>) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(a), Some(b)) => compare_values_total(a, b),
    }
}

/// Sort key extracted once per document (decorate-sort-undecorate): one owned
/// value per sort field plus the `_id` tiebreak. Streaming sorts compare
/// these instead of re-resolving field paths on every comparison.
#[derive(Clone)]
pub struct DocSortKey {
    vals: Vec<Option<Value>>,
    id: Option<String>,
}

pub fn extract_sort_key(doc: &Value, sort_fields: &[SortField]) -> DocSortKey {
    DocSortKey {
        vals: sort_fields
            .iter()
            .map(|sf| resolve_json_path(doc, &sf.field).cloned())
            .collect(),
        id: doc.get("_id").and_then(Value::as_str).map(str::to_string),
    }
}

/// `compare_docs`' exact order over pre-extracted keys: same per-field
/// direction flips, same Missing<present rule, same `_id` tiebreak
/// direction. Pinned lockstep against `compare_docs` by
/// `decorated_matches_compare_docs`.
pub fn compare_decorated(
    a: &DocSortKey,
    b: &DocSortKey,
    sort_fields: &[SortField],
) -> std::cmp::Ordering {
    for (i, sf) in sort_fields.iter().enumerate() {
        let ordering = compare_json_values(a.vals[i].as_ref(), b.vals[i].as_ref());
        let ordering = if sf.ascending {
            ordering
        } else {
            ordering.reverse()
        };
        if ordering != std::cmp::Ordering::Equal {
            return ordering;
        }
    }
    let ordering = a.id.as_deref().cmp(&b.id.as_deref());
    if tiebreak_ascending(sort_fields) {
        ordering
    } else {
        ordering.reverse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Lockstep pin: the decorated comparator IS compare_docs, for every
    /// pair over a corpus covering all value buckets, missing fields,
    /// duplicate values, and a missing `_id`.
    #[test]
    fn decorated_matches_compare_docs() {
        let docs = vec![
            json!({"_id": "a", "v": null, "w": 1}),
            json!({"_id": "b", "v": true}),
            json!({"_id": "c", "v": 5, "w": "x"}),
            json!({"_id": "d", "v": 5.5}),
            json!({"_id": "e", "v": -0.0}),
            json!({"_id": "f", "v": 0.0}),
            json!({"_id": "g", "v": "abc"}),
            json!({"_id": "h", "v": ""}),
            json!({"_id": "i", "v": [1, 2]}),
            json!({"_id": "j", "v": {"k": 1}}),
            json!({"_id": "k", "w": 2}),
            json!({"v": 3}),
            json!({"_id": "m", "v": 5}),
        ];
        let specs: Vec<Vec<SortField>> = vec![
            vec![SortField {
                field: "v".into(),
                ascending: true,
            }],
            vec![SortField {
                field: "v".into(),
                ascending: false,
            }],
            vec![
                SortField {
                    field: "v".into(),
                    ascending: true,
                },
                SortField {
                    field: "w".into(),
                    ascending: false,
                },
            ],
            vec![],
        ];
        for spec in &specs {
            let keys: Vec<DocSortKey> = docs.iter().map(|d| extract_sort_key(d, spec)).collect();
            for (i, a) in docs.iter().enumerate() {
                for (j, b) in docs.iter().enumerate() {
                    assert_eq!(
                        compare_decorated(&keys[i], &keys[j], spec),
                        compare_docs(a, b, spec),
                        "docs {i} vs {j} under spec {spec:?}"
                    );
                }
            }
        }
    }
}
