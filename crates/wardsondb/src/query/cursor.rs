//! Opaque keyset-pagination cursors for the `/query` endpoint.
//!
//! A cursor identifies the position of the last returned document in the
//! query's total order (sort fields with their directions, then the `_id`
//! tiebreak — see `sort::tiebreak_ascending`). The next page contains exactly
//! the matching documents that compare strictly greater in that order, so
//! pages never repeat or skip surviving documents even when documents are
//! inserted or deleted between requests.
//!
//! The token is URL-safe base64 of a small JSON payload. A fingerprint binds
//! it to the collection and the canonical sort spec (fields + directions) so
//! that replaying a cursor against a different query fails with a clean 400
//! instead of returning garbage positions. It is deliberately NOT bound to
//! the filter: the cursor is purely positional, so resuming with a narrowed
//! or widened filter is well-defined (membership per the new filter, position
//! unchanged) and legitimately useful.

use std::cmp::Ordering;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::AppError;

use super::filter::resolve_json_path;
use super::sort::{SortField, compare_json_values, tiebreak_ascending};

/// Token length cap (base64 characters), enforced symmetrically: decode
/// rejects longer inbound tokens, and encode omits rather than emit one
/// (sort values are scalars in practice; anything larger than this is not a
/// token we produced).
pub const MAX_CURSOR_LEN: usize = 4096;

/// One sort-key value captured in a cursor. `Missing` (field absent on the
/// document) must stay distinct from `Present(Value::Null)` — missing sorts
/// before null (`None < Some` in `compare_json_values`).
#[derive(Debug, Clone)]
pub enum CursorValue {
    Missing,
    Present(Value),
}

/// A decoded, fingerprint-validated cursor position.
#[derive(Debug, Clone)]
pub struct Cursor {
    /// One entry per sort field, in sort-spec order.
    pub sort_values: Vec<CursorValue>,
    pub last_id: String,
}

/// Wire format (version 1). `s` holds one entry per sort field: `[]` for a
/// missing field, `[value]` for a present one (`[null]` ≠ `[]`).
#[derive(Serialize, Deserialize)]
struct CursorPayloadV1 {
    v: u8,
    f: u32,
    s: Vec<Value>,
    i: String,
}

/// Build the opaque token for the position of `doc` (the last document of a
/// page, pre-projection). Returns `None` — the response simply omits
/// `next_cursor` — if the doc has no string `_id`, or if the token would
/// exceed `MAX_CURSOR_LEN` and so be rejected by our own `decode_cursor` on
/// replay (oversize sort values; `has_more` stays exact either way).
pub fn encode_cursor(doc: &Value, sort_fields: &[SortField], collection: &str) -> Option<String> {
    let id = doc.get("_id")?.as_str()?.to_string();
    let s: Vec<Value> = sort_fields
        .iter()
        .map(|sf| match resolve_json_path(doc, &sf.field) {
            Some(v) => Value::Array(vec![v.clone()]),
            None => Value::Array(vec![]),
        })
        .collect();
    let payload = CursorPayloadV1 {
        v: 1,
        f: cursor_fingerprint(collection, sort_fields),
        s,
        i: id,
    };
    let bytes = serde_json::to_vec(&payload).ok()?;
    let token = URL_SAFE_NO_PAD.encode(bytes);
    if token.len() > MAX_CURSOR_LEN {
        return None;
    }
    Some(token)
}

/// Decode and validate a token against the request's collection + sort spec.
pub fn decode_cursor(
    token: &str,
    collection: &str,
    sort_fields: &[SortField],
) -> Result<Cursor, AppError> {
    // Never echo token contents back — a garbled token could be arbitrarily
    // large or contain unrelated user data.
    let malformed =
        || AppError::InvalidQuery("invalid cursor: malformed or corrupted token".to_string());

    if token.len() > MAX_CURSOR_LEN {
        return Err(malformed());
    }
    let bytes = URL_SAFE_NO_PAD.decode(token).map_err(|_| malformed())?;
    let payload: CursorPayloadV1 = serde_json::from_slice(&bytes).map_err(|_| malformed())?;
    if payload.v != 1 || payload.s.len() != sort_fields.len() {
        return Err(malformed());
    }
    if payload.f != cursor_fingerprint(collection, sort_fields) {
        return Err(AppError::InvalidQuery(
            "cursor does not match this query: a cursor must be reused with the same collection and the same sort specification"
                .to_string(),
        ));
    }

    let mut sort_values = Vec::with_capacity(payload.s.len());
    for entry in payload.s {
        match entry {
            Value::Array(a) if a.is_empty() => sort_values.push(CursorValue::Missing),
            Value::Array(mut a) if a.len() == 1 => {
                sort_values.push(CursorValue::Present(a.pop().unwrap()));
            }
            _ => return Err(malformed()),
        }
    }

    Ok(Cursor {
        sort_values,
        last_id: payload.i,
    })
}

/// Where `doc` sits relative to the cursor position in the query's total
/// order. Documents belonging to the next page are exactly those returning
/// `Ordering::Greater`. Consistent with `sort::compare_docs` by construction
/// (same field resolution, same per-field directions, same `_id` tiebreak).
pub fn compare_doc_to_cursor(doc: &Value, cursor: &Cursor, sort_fields: &[SortField]) -> Ordering {
    for (sf, cv) in sort_fields.iter().zip(&cursor.sort_values) {
        let dv = resolve_json_path(doc, &sf.field);
        let cvv = match cv {
            CursorValue::Missing => None,
            CursorValue::Present(v) => Some(v),
        };
        let ordering = compare_json_values(dv, cvv);
        let ordering = if sf.ascending {
            ordering
        } else {
            ordering.reverse()
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }

    let doc_id = doc.get("_id").and_then(Value::as_str);
    let ordering = doc_id.cmp(&Some(cursor.last_id.as_str()));
    if tiebreak_ascending(sort_fields) {
        ordering
    } else {
        ordering.reverse()
    }
}

/// FNV-1a 32-bit over length-prefixed components (length prefixes prevent
/// concatenation collisions like ("ab","c") vs ("a","bc")).
fn cursor_fingerprint(collection: &str, sort_fields: &[SortField]) -> u32 {
    fn feed(h: u32, bytes: &[u8]) -> u32 {
        bytes
            .iter()
            .fold(h, |h, &b| (h ^ u32::from(b)).wrapping_mul(16_777_619))
    }

    let mut h: u32 = 2_166_136_261;
    h = feed(h, &(collection.len() as u32).to_le_bytes());
    h = feed(h, collection.as_bytes());
    for sf in sort_fields {
        h = feed(h, &(sf.field.len() as u32).to_le_bytes());
        h = feed(h, sf.field.as_bytes());
        h = feed(h, &[if sf.ascending { b'a' } else { b'd' }]);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sf(field: &str, ascending: bool) -> SortField {
        SortField {
            field: field.to_string(),
            ascending,
        }
    }

    #[test]
    fn roundtrip_present_and_missing() {
        let sort = vec![sf("price", false), sf("stock", true)];
        // "stock" is absent; "price" is present-and-null — the two must stay
        // distinct through the roundtrip.
        let doc = json!({"_id": "doc-1", "price": null});

        let token = encode_cursor(&doc, &sort, "products").unwrap();
        let cursor = decode_cursor(&token, "products", &sort).unwrap();

        assert_eq!(cursor.last_id, "doc-1");
        assert!(matches!(
            &cursor.sort_values[0],
            CursorValue::Present(Value::Null)
        ));
        assert!(matches!(&cursor.sort_values[1], CursorValue::Missing));
    }

    #[test]
    fn fingerprint_binds_collection_and_sort_spec() {
        let sort = vec![sf("price", true)];
        let doc = json!({"_id": "doc-1", "price": 5});
        let token = encode_cursor(&doc, &sort, "products").unwrap();

        // Same everything → decodes.
        assert!(decode_cursor(&token, "products", &sort).is_ok());
        // Different collection → rejected.
        assert!(decode_cursor(&token, "orders", &sort).is_err());
        // Different direction → rejected.
        assert!(decode_cursor(&token, "products", &[sf("price", false)]).is_err());
        // Different field → rejected.
        assert!(decode_cursor(&token, "products", &[sf("cost", true)]).is_err());
    }

    #[test]
    fn garbage_tokens_rejected() {
        let sort = vec![sf("price", true)];
        assert!(decode_cursor("!!!not-base64!!!", "products", &sort).is_err());
        // Valid base64, wrong payload shape.
        let bogus = URL_SAFE_NO_PAD.encode(b"{\"hello\":\"world\"}");
        assert!(decode_cursor(&bogus, "products", &sort).is_err());
        // Oversize.
        let huge = "A".repeat(MAX_CURSOR_LEN + 1);
        assert!(decode_cursor(&huge, "products", &sort).is_err());
    }

    /// R3: encode never emits a token decode would reject; near-max legal
    /// positions still roundtrip (DT-9).
    #[test]
    fn oversize_positions_omitted_at_encode() {
        let sort = vec![sf("blob", true)];
        let doc = json!({"_id": "d1", "blob": "y".repeat(5000)});
        assert_eq!(encode_cursor(&doc, &sort, "c"), None);

        let doc = json!({"_id": "d1", "blob": "y".repeat(2500)});
        let token = encode_cursor(&doc, &sort, "c").expect("under-limit token must emit");
        assert!(token.len() <= MAX_CURSOR_LEN);
        let cursor = decode_cursor(&token, "c", &sort).unwrap();
        assert_eq!(cursor.last_id, "d1");
    }

    #[test]
    fn compare_doc_to_cursor_matches_total_order() {
        let sort = vec![sf("price", false)]; // desc → _id tiebreak desc
        let anchor = json!({"_id": "m", "price": 5});
        let token = encode_cursor(&anchor, &sort, "c").unwrap();
        let cursor = decode_cursor(&token, "c", &sort).unwrap();

        // Higher price sorts earlier under desc → Less (already returned side).
        let earlier = json!({"_id": "a", "price": 9});
        assert_eq!(
            compare_doc_to_cursor(&earlier, &cursor, &sort),
            Ordering::Less
        );
        // Lower price → strictly after.
        let later = json!({"_id": "z", "price": 1});
        assert_eq!(
            compare_doc_to_cursor(&later, &cursor, &sort),
            Ordering::Greater
        );
        // Equal price: _id tiebreak is DESC, so a smaller _id is after.
        let tie_after = json!({"_id": "a", "price": 5});
        assert_eq!(
            compare_doc_to_cursor(&tie_after, &cursor, &sort),
            Ordering::Greater
        );
        let tie_before = json!({"_id": "z", "price": 5});
        assert_eq!(
            compare_doc_to_cursor(&tie_before, &cursor, &sort),
            Ordering::Less
        );
        // The anchor itself is never on the next page.
        assert_eq!(
            compare_doc_to_cursor(&anchor, &cursor, &sort),
            Ordering::Equal
        );
    }

    /// Cross-type positions follow the total encoding order (T3/R2): with an
    /// ascending sort anchored at a number, every lower bucket is on the
    /// already-returned side and every higher bucket strictly after —
    /// independent of `_id`s (chosen adversarially here).
    #[test]
    fn compare_doc_to_cursor_mixed_types() {
        let sort = vec![sf("val", true)];
        let anchor = json!({"_id": "m", "val": 5});
        let token = encode_cursor(&anchor, &sort, "c").unwrap();
        let cursor = decode_cursor(&token, "c", &sort).unwrap();

        // _ids all sort AFTER the anchor's "m", so a comparator that
        // collapses cross-type pairs onto the _id tiebreak would call every
        // one of these Greater; the encoding order must win instead.
        for before in [
            json!({"_id": "z1", "val": null}),
            json!({"_id": "z2", "val": false}),
            json!({"_id": "z3", "val": true}),
            json!({"_id": "z4", "val": 4}),
        ] {
            assert_eq!(
                compare_doc_to_cursor(&before, &cursor, &sort),
                Ordering::Less,
                "{before} sorts before the anchor"
            );
        }
        // _ids all sort BEFORE "m"; encoding order must still place these after.
        for after in [
            json!({"_id": "a1", "val": "text"}),
            json!({"_id": "a2", "val": [1]}),
            json!({"_id": "a3", "val": {"k": 1}}),
        ] {
            assert_eq!(
                compare_doc_to_cursor(&after, &cursor, &sort),
                Ordering::Greater,
                "{after} sorts after the anchor"
            );
        }
    }
}
