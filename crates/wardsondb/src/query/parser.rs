use serde::Deserialize;
use serde_json::Value;

use crate::error::AppError;

use super::cursor::{Cursor, decode_cursor};
use super::filter::{FilterNode, parse_filter};
use super::sort::{SortField, parse_sort_spec};

#[derive(Debug, Deserialize)]
pub struct QueryRequest {
    pub filter: Option<Value>,
    /// Array of single-field objects, or a single-field object (shared shape
    /// with the aggregate `$sort` stage — see `parse_sort_spec`).
    pub sort: Option<Value>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
    pub fields: Option<Vec<String>>,
    pub count_only: Option<bool>,
    /// Opaque pagination token from a previous response's `meta.next_cursor`.
    /// Mutually exclusive with `offset`.
    pub cursor: Option<String>,
}

#[derive(Debug)]
pub struct ParsedQuery {
    pub filter: Option<FilterNode>,
    pub sort: Vec<SortField>,
    pub limit: u64,
    pub offset: u64,
    pub fields: Option<Vec<String>>,
    pub count_only: bool,
    pub cursor: Option<Cursor>,
}

pub fn parse_query(
    req: QueryRequest,
    max_limit: u64,
    collection: &str,
) -> Result<ParsedQuery, AppError> {
    let filter = match req.filter {
        Some(f) => Some(parse_filter(&f)?),
        None => None,
    };

    let sort = match req.sort {
        Some(s) => parse_sort_spec(&s).map_err(|e| AppError::InvalidQuery(format!("sort {e}")))?,
        None => vec![],
    };

    let offset = req.offset.unwrap_or(0);
    let count_only = req.count_only.unwrap_or(false);

    let cursor = match &req.cursor {
        Some(token) => {
            if offset > 0 {
                return Err(AppError::InvalidQuery(
                    "cursor and offset are mutually exclusive; use one pagination mechanism"
                        .to_string(),
                ));
            }
            if count_only {
                return Err(AppError::InvalidQuery(
                    "cursor cannot be combined with count_only".to_string(),
                ));
            }
            if req.limit == Some(0) {
                return Err(AppError::InvalidQuery(
                    "limit must be at least 1 when using a cursor".to_string(),
                ));
            }
            Some(decode_cursor(token, collection, &sort)?)
        }
        None => None,
    };

    Ok(ParsedQuery {
        filter,
        sort,
        limit: req.limit.unwrap_or(100).min(max_limit),
        offset,
        fields: req.fields,
        count_only,
        cursor,
    })
}
