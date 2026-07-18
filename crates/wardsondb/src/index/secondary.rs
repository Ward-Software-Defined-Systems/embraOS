use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexDef {
    pub name: String,
    pub collection: String,
    /// Fields covered by this index. Single-field indexes have one entry;
    /// compound indexes have multiple fields in order.
    #[serde(default)]
    pub fields: Vec<String>,
    pub created_at: String,
    /// Backward-compat: single-field indexes also expose `field`.
    /// For compound indexes this is the first field.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub field: String,
}

impl IndexDef {
    /// Create a new index definition.
    pub fn new(name: String, collection: String, fields: Vec<String>, created_at: String) -> Self {
        let field = fields.first().cloned().unwrap_or_default();
        IndexDef {
            name,
            collection,
            fields,
            field,
            created_at,
        }
    }

    /// Whether this is a compound (multi-field) index.
    pub fn is_compound(&self) -> bool {
        self.fields.len() > 1
    }
}

/// Encode a JSON value into bytes that sort lexicographically in the correct order.
///
/// Encoding scheme (type prefix byte ensures cross-type ordering):
///   0x00 = null
///   0x01 = false, 0x02 = true
///   0x03 = number (IEEE 754 with sign-flip for correct ordering)
///   0x04 = string (raw UTF-8 bytes)
///   0x05 = array/object (serialized JSON text)
///
/// Cross-type order: null < false < true < number < string < array/object.
/// `compare_values_total` below must stay byte-for-byte consistent with this
/// encoding — the lockstep test in this file enforces it.
pub fn value_to_sortable_bytes(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    value_to_sortable_bytes_append(value, &mut out);
    out
}

/// Append `value`'s sortable encoding to `out` — the composing form the key
/// builders share, so multi-field keys and reusable scratch buffers never
/// pay per-field intermediate allocations.
pub fn value_to_sortable_bytes_append(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.push(0x00),
        Value::Bool(false) => out.push(0x01),
        Value::Bool(true) => out.push(0x02),
        Value::Number(n) => {
            let f = n.as_f64().unwrap_or(0.0);
            out.push(0x03);
            let bits = f.to_bits();
            // Flip sign bit for positive numbers; flip all bits for negative
            let sortable = if f.is_sign_negative() {
                !bits
            } else {
                bits ^ (1u64 << 63)
            };
            out.extend_from_slice(&sortable.to_be_bytes());
        }
        Value::String(s) => {
            out.push(0x04);
            out.extend_from_slice(s.as_bytes());
        }
        // Arrays/objects: serialize to JSON string for consistent ordering
        other => {
            out.push(0x05);
            out.extend_from_slice(serde_json::to_string(other).unwrap_or_default().as_bytes());
        }
    }
}

/// The encoding's type prefix byte for a value (see `value_to_sortable_bytes`).
fn type_prefix_byte(v: &Value) -> u8 {
    match v {
        Value::Null => 0x00,
        Value::Bool(false) => 0x01,
        Value::Bool(true) => 0x02,
        Value::Number(_) => 0x03,
        Value::String(_) => 0x04,
        Value::Array(_) | Value::Object(_) => 0x05,
    }
}

/// Total order over JSON values, byte-for-byte identical to the order of
/// `value_to_sortable_bytes` output (the lockstep test below enforces this).
/// This is the database's ONE collation for ordering surfaces: /query sort,
/// cursor positions, aggregate $sort, $min/$max, $collect. Range FILTERS
/// deliberately do not use it — see `query::filter::compare_values`.
pub fn compare_values_total(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        // total_cmp is the same transform as the encoding's sign-flip bit
        // order for every f64, including -0.0 < 0.0; as_f64 mirrors the
        // encoding's lossy conversion (>2^53 collisions stay consistent).
        (Value::Number(x), Value::Number(y)) => x
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&y.as_f64().unwrap_or(0.0)),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        // Arrays/objects share prefix 0x05 and order by serialized JSON text.
        // This arm must catch (Array, Object) pairs too — '[' < '{' — so they
        // never reach the prefix-byte arm and compare as a spurious Equal.
        (Value::Array(_) | Value::Object(_), Value::Array(_) | Value::Object(_)) => {
            serde_json::to_string(a)
                .unwrap_or_default()
                .cmp(&serde_json::to_string(b).unwrap_or_default())
        }
        // Cross-bucket: only differing prefix bytes reach here (every
        // same-bucket pair is handled above), so this never returns Equal.
        _ => type_prefix_byte(a).cmp(&type_prefix_byte(b)),
    }
}

/// Build an index key for a single-field index: {encoded_value}\x00{doc_id}
pub fn make_index_key(value: &Value, doc_id: &str) -> Vec<u8> {
    let mut key = Vec::new();
    make_index_key_into(value, doc_id, &mut key);
    key
}

/// `make_index_key` into a reusable buffer (cleared first) — the index write
/// path encodes one entry per doc per index and reuses one scratch.
pub fn make_index_key_into(value: &Value, doc_id: &str, out: &mut Vec<u8>) {
    out.clear();
    value_to_sortable_bytes_append(value, out);
    out.push(0x00);
    out.extend_from_slice(doc_id.as_bytes());
}

/// Build an index key for a compound index: {encoded_v1}\x01{encoded_v2}\x01...\x00{doc_id}
/// Uses \x01 as field separator (distinct from \x00 doc_id separator).
pub fn make_compound_index_key(values: &[&Value], doc_id: &str) -> Vec<u8> {
    let mut key = Vec::new();
    make_compound_index_key_into(values, doc_id, &mut key);
    key
}

/// `make_compound_index_key` into a reusable buffer (cleared first).
pub fn make_compound_index_key_into(values: &[&Value], doc_id: &str, out: &mut Vec<u8>) {
    out.clear();
    for (i, value) in values.iter().enumerate() {
        if i > 0 {
            out.push(0x01); // field separator
        }
        value_to_sortable_bytes_append(value, out);
    }
    out.push(0x00); // doc_id separator
    out.extend_from_slice(doc_id.as_bytes());
}

/// Smallest byte string greater than every key that has `prefix` as a
/// prefix — the exclusive upper bound of a prefix window, or the inclusive
/// start that skips exactly that prefix's keys. Planner-built prefixes always
/// end with a 0x00/0x01 separator, so in practice this bumps that byte; the
/// loop handles trailing 0xFF for generality. An all-0xFF prefix has no
/// finite successor (unreachable here), so a maximal sentinel keeps the
/// function total.
pub fn prefix_successor(prefix: &[u8]) -> Vec<u8> {
    let mut p = prefix.to_vec();
    while let Some(&last) = p.last() {
        if last == 0xFF {
            p.pop();
        } else {
            *p.last_mut().unwrap() = last + 1;
            return p;
        }
    }
    vec![0xFF; prefix.len() + 1]
}

/// `[start, end)` prefix-byte window of the type bucket a range operand can
/// match, mirroring the in-memory filter's type bracketing (see
/// `query::filter::compare_values`): a comparison only matches values of the
/// operand's own comparable type. Bool spans BOTH prefixes (`$gt: false`
/// must match `true`, which encodes under a different prefix byte). Null,
/// arrays, and objects match nothing — even same-type — so they have no
/// bracket.
fn type_bracket(first_byte: u8) -> Option<(u8, u8)> {
    match first_byte {
        0x01 | 0x02 => Some((0x01, 0x03)), // bool
        0x03 => Some((0x03, 0x04)),        // number
        0x04 => Some((0x04, 0x05)),        // string
        _ => None,                         // null (0x00), array/object (0x05)
    }
}

/// Index scan bounds for a range predicate.
#[derive(Debug, PartialEq, Eq)]
pub enum RangeScanBounds {
    /// The predicate can never match (null/array/object operand, or bounds
    /// from two different type buckets) — skip the scan entirely.
    Empty,
    /// Scan the half-open key window `[start, end)`.
    Span { start: Vec<u8>, end: Vec<u8> },
}

/// The one bounds builder for every index range scan (single-field and
/// compound). `prefix` is empty for single-field scans, or the equality
/// prefix INCLUDING its trailing 0x01 separator for compound ones; `lower`/
/// `upper` are `(value_to_sortable_bytes output, inclusive)`.
///
/// Present bounds keep the byte recipes the scans have always used:
/// inclusive upper appends `0x00 ++ [0xFF; 37]` (UTF-8 doc ids never contain
/// 0xFF, so this sits above every id of that exact value); exclusive lower
/// starts at `prefix_successor(bound ++ 0x00)`, which skips exactly the
/// bound's own entries (set-equivalent to the historical skip loop,
/// including its compound-partition boundary behavior). ABSENT bounds close
/// over the operand's type bracket instead of sweeping to the start/end of
/// the partition — an open-ended `$gt: 5` must not return strings, which
/// sort above every number.
pub fn range_scan_bounds(
    prefix: &[u8],
    lower: Option<(&[u8], bool)>,
    upper: Option<(&[u8], bool)>,
) -> RangeScanBounds {
    let bracket_of = |bound: Option<(&[u8], bool)>| {
        bound.map(|(bytes, _)| bytes.first().copied().and_then(type_bracket))
    };
    // A present bound whose operand type has no bracket ⇒ nothing matches.
    let lower_bracket = match bracket_of(lower) {
        Some(None) => return RangeScanBounds::Empty,
        other => other.flatten(),
    };
    let upper_bracket = match bracket_of(upper) {
        Some(None) => return RangeScanBounds::Empty,
        other => other.flatten(),
    };
    // Two bounds from different buckets ⇒ nothing matches on any path (the
    // in-memory filter can't satisfy both predicates with one value either).
    if let (Some(l), Some(u)) = (lower_bracket, upper_bracket)
        && l != u
    {
        return RangeScanBounds::Empty;
    }
    let bracket = lower_bracket.or(upper_bracket);
    // The planner never builds a range without at least one bound; fall back
    // to the whole prefix window to keep the function total.
    debug_assert!(bracket.is_some(), "range with no bounds");

    let start = match lower {
        Some((bytes, true)) => {
            let mut k = prefix.to_vec();
            k.extend_from_slice(bytes);
            k
        }
        Some((bytes, false)) => {
            let mut k = prefix.to_vec();
            k.extend_from_slice(bytes);
            k.push(0x00);
            prefix_successor(&k)
        }
        None => match bracket {
            Some((lo, _)) => {
                let mut k = prefix.to_vec();
                k.push(lo);
                k
            }
            None => prefix.to_vec(),
        },
    };
    let end = match upper {
        Some((bytes, false)) => {
            let mut k = prefix.to_vec();
            k.extend_from_slice(bytes);
            k
        }
        Some((bytes, true)) => {
            let mut k = prefix.to_vec();
            k.extend_from_slice(bytes);
            k.push(0x00);
            k.extend_from_slice(&[0xFF; 37]);
            k
        }
        None => match bracket {
            Some((_, hi)) => {
                let mut k = prefix.to_vec();
                k.push(hi);
                k
            }
            None => prefix_successor(prefix),
        },
    };
    RangeScanBounds::Span { start, end }
}

/// Decode sortable bytes back into a JSON value (inverse of value_to_sortable_bytes).
pub fn decode_sortable_bytes(bytes: &[u8]) -> Option<Value> {
    if bytes.is_empty() {
        return None;
    }
    match bytes[0] {
        0x00 => Some(Value::Null),
        0x01 => Some(Value::Bool(false)),
        0x02 => Some(Value::Bool(true)),
        0x03 => {
            if bytes.len() < 9 {
                return None;
            }
            let mut be = [0u8; 8];
            be.copy_from_slice(&bytes[1..9]);
            let sortable = u64::from_be_bytes(be);
            let bits = if sortable & (1u64 << 63) != 0 {
                sortable ^ (1u64 << 63) // positive: flip sign bit back
            } else {
                !sortable // negative: flip all bits back
            };
            let f = f64::from_bits(bits);
            serde_json::Number::from_f64(f).map(Value::Number)
        }
        0x04 => {
            let s = std::str::from_utf8(&bytes[1..]).ok()?;
            Some(Value::String(s.to_string()))
        }
        0x05 => {
            let s = std::str::from_utf8(&bytes[1..]).ok()?;
            serde_json::from_str(s).ok()
        }
        _ => None,
    }
}

/// Extract the doc_id from an index key by splitting on the \x00 separator.
/// The doc_id is everything after the last \x00.
pub fn extract_doc_id_from_key(key: &[u8]) -> Option<String> {
    // Key format: {encoded_value}\x00{doc_id}
    // Find the last \x00 separator (doc_id is guaranteed to not contain \x00)
    let sep_pos = key.iter().rposition(|&b| b == 0x00)?;
    if sep_pos + 1 >= key.len() {
        return None;
    }
    String::from_utf8(key[sep_pos + 1..].to_vec()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Corpus spanning every encoding bucket plus the known edge values:
    /// signed zeros, negative/fractional numbers, the 2^53/2^53+1 lossy
    /// collision, empty string/array/object, nesting, '[' vs '{' text order.
    fn corpus() -> Vec<Value> {
        vec![
            json!(null),
            json!(false),
            json!(true),
            json!(-3.5),
            json!(-0.0),
            json!(0.0),
            json!(1),
            json!(1.5),
            json!(42),
            json!(9007199254740992u64), // 2^53
            json!(9007199254740993u64), // 2^53 + 1: as_f64-collides with 2^53
            json!(""),
            json!("A"),
            json!("a"),
            json!("ab"),
            json!("z"),
            json!([]),
            json!([1]),
            json!([1, 2]),
            json!(["a"]),
            json!({}),
            json!({"a": 1}),
            json!({"a": {"b": [1]}}),
            json!({"b": 1}),
        ]
    }

    /// THE invariant of this module: the in-memory comparator and the key
    /// encoding define the same order, for every ordered pair. Any future
    /// change to `value_to_sortable_bytes` or `compare_values_total` that
    /// breaks byte parity fails here.
    #[test]
    fn compare_values_total_matches_encoding_byte_order() {
        let values = corpus();
        for a in &values {
            for b in &values {
                assert_eq!(
                    compare_values_total(a, b),
                    value_to_sortable_bytes(a).cmp(&value_to_sortable_bytes(b)),
                    "comparator vs encoding disagree for {a} vs {b}"
                );
            }
        }
    }

    /// The property whose absence caused R2: transitivity across buckets.
    #[test]
    fn total_order_transitive_on_mixed_sample() {
        let values = corpus();
        for a in &values {
            for b in &values {
                for c in &values {
                    use std::cmp::Ordering::Greater;
                    if compare_values_total(a, b) != Greater
                        && compare_values_total(b, c) != Greater
                    {
                        assert_ne!(
                            compare_values_total(a, c),
                            Greater,
                            "intransitive: {a} <= {b} <= {c} but {a} > {c}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn type_bracket_table() {
        assert_eq!(type_bracket(0x00), None, "null operands match nothing");
        assert_eq!(type_bracket(0x01), Some((0x01, 0x03)), "false spans bool");
        assert_eq!(type_bracket(0x02), Some((0x01, 0x03)), "true spans bool");
        assert_eq!(type_bracket(0x03), Some((0x03, 0x04)));
        assert_eq!(type_bracket(0x04), Some((0x04, 0x05)));
        assert_eq!(
            type_bracket(0x05),
            None,
            "array/object operands match nothing"
        );
    }

    /// Present bounds must keep the exact byte recipes the scans have always
    /// used — only the open-bound sentinels changed.
    #[test]
    fn range_scan_bounds_preserves_present_bound_recipes() {
        let five = value_to_sortable_bytes(&json!(5));
        let nine = value_to_sortable_bytes(&json!(9));

        // Inclusive lower + exclusive upper: bounds pass through verbatim.
        assert_eq!(
            range_scan_bounds(&[], Some((&five, true)), Some((&nine, false))),
            RangeScanBounds::Span {
                start: five.clone(),
                end: nine.clone()
            }
        );

        // Exclusive lower: successor of `bound ++ 0x00` (== bound ++ 0x01),
        // the same set the historical skip loop excluded.
        let mut after_five = five.clone();
        after_five.push(0x01);
        let RangeScanBounds::Span { start, .. } =
            range_scan_bounds(&[], Some((&five, false)), Some((&nine, false)))
        else {
            panic!("expected a span");
        };
        assert_eq!(start, after_five);

        // Inclusive upper: bound ++ 0x00 ++ [0xFF; 37].
        let mut through_nine = nine.clone();
        through_nine.push(0x00);
        through_nine.extend_from_slice(&[0xFF; 37]);
        let RangeScanBounds::Span { end, .. } =
            range_scan_bounds(&[], Some((&five, true)), Some((&nine, true)))
        else {
            panic!("expected a span");
        };
        assert_eq!(end, through_nine);
    }

    /// Open bounds close over the operand's type bucket instead of sweeping
    /// the partition; the compound eq-prefix stays glued to every bound.
    #[test]
    fn range_scan_bounds_brackets_open_ends() {
        let five = value_to_sortable_bytes(&json!(5));

        // Open upper on a number: end at the string bucket's first byte.
        assert_eq!(
            range_scan_bounds(&[], Some((&five, true)), None),
            RangeScanBounds::Span {
                start: five.clone(),
                end: vec![0x04]
            }
        );
        // Open lower on a number: start at the number bucket's first byte.
        assert_eq!(
            range_scan_bounds(&[], None, Some((&five, false))),
            RangeScanBounds::Span {
                start: vec![0x03],
                end: five.clone()
            }
        );
        // Bool bracket spans both prefixes: $gt:false must reach true (0x02).
        let false_bytes = value_to_sortable_bytes(&json!(false));
        let RangeScanBounds::Span { end, .. } =
            range_scan_bounds(&[], Some((&false_bytes, false)), None)
        else {
            panic!("expected a span");
        };
        assert_eq!(end, vec![0x03], "bool bracket must include true");

        // Compound: the eq prefix (with its trailing 0x01) wraps both ends.
        let prefix = [0xAA, 0x01];
        let mut expected_start = prefix.to_vec();
        expected_start.extend_from_slice(&five);
        assert_eq!(
            range_scan_bounds(&prefix, Some((&five, true)), None),
            RangeScanBounds::Span {
                start: expected_start,
                end: vec![0xAA, 0x01, 0x04]
            }
        );
    }

    /// Null/array/object operands and cross-bucket bound pairs match nothing
    /// anywhere (the in-memory filter returns `None` for them, even
    /// same-type) — the scan must not run at all.
    #[test]
    fn range_scan_bounds_empty_cases() {
        let null_b = value_to_sortable_bytes(&json!(null));
        let arr_b = value_to_sortable_bytes(&json!([1]));
        let obj_b = value_to_sortable_bytes(&json!({"k": 1}));
        let five = value_to_sortable_bytes(&json!(5));
        let zed = value_to_sortable_bytes(&json!("z"));

        for b in [&null_b, &arr_b, &obj_b] {
            assert_eq!(
                range_scan_bounds(&[], Some((b, true)), None),
                RangeScanBounds::Empty
            );
            assert_eq!(
                range_scan_bounds(&[], None, Some((b, false))),
                RangeScanBounds::Empty
            );
        }
        // $gt:5 combined with $lt:"z": no single value satisfies both.
        assert_eq!(
            range_scan_bounds(&[], Some((&five, false)), Some((&zed, false))),
            RangeScanBounds::Empty
        );
    }

    /// DT-7: the general trailing-0xFF loop and the all-0xFF sentinel.
    /// Planner-built prefixes always end with a 0x00/0x01 separator (the
    /// plain-bump case); the rest is pinned here so a future caller with
    /// arbitrary prefixes inherits documented behavior.
    #[test]
    fn prefix_successor_bumps_pops_and_sentinels() {
        // Plain bump of the last byte.
        assert_eq!(prefix_successor(&[0x01, 0x02]), vec![0x01, 0x03]);
        assert_eq!(prefix_successor(&[0x00]), vec![0x01]);
        // Trailing 0xFF bytes pop until a bumpable byte is found.
        assert_eq!(prefix_successor(&[0x01, 0xFF]), vec![0x02]);
        assert_eq!(prefix_successor(&[0x01, 0xFF, 0xFF]), vec![0x02]);
        // All-0xFF has no finite successor: maximal sentinel, one byte longer.
        assert_eq!(prefix_successor(&[0xFF]), vec![0xFF, 0xFF]);
        assert_eq!(prefix_successor(&[0xFF, 0xFF]), vec![0xFF; 3]);
        // Empty prefix (every key matches it) also takes the sentinel arm.
        assert_eq!(prefix_successor(&[]), vec![0xFF]);

        // The load-bearing property for the bumpable cases: the successor is
        // strictly greater than every key extending the prefix.
        for prefix in [&[0x01u8, 0x02][..], &[0x01, 0xFF], &[0x00]] {
            let succ = prefix_successor(prefix);
            assert!(succ.as_slice() > prefix);
            for ext in [&[0x00u8][..], &[0x7F], &[0xFF], &[0xFF, 0xFF]] {
                let mut key = prefix.to_vec();
                key.extend_from_slice(ext);
                assert!(
                    key < succ,
                    "extension {key:?} must sort below successor {succ:?}"
                );
            }
        }
    }
}
