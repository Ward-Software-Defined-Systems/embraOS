use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::error::WardsonDbError;

/// Recency window for memory search/scan fetches (FIX-2/3/4/6).
///
/// Every windowed fetch over the memory collections goes through
/// `fetch_recent`/`fetch_recent_with_fields` with this limit so the window
/// covers the *most recent* documents (WardSONDB's server default is
/// `limit: 100` in UUIDv7 key order — oldest first — which froze search
/// over the oldest ~100 docs once collections grew past it). `system_status`
/// compares live collection counts against this same constant and raises
/// SEARCH_WINDOW_SATURATED when a collection outgrows it.
pub const MEMORY_FETCH_WINDOW: usize = 10_000;

/// Body for a most-recent-first windowed fetch. Sort keys are one-per-array-
/// element (WardSONDB requirement — a multi-key object degrades to
/// alphabetical priority); `_id` (UUIDv7) breaks sub-second `_created_at`
/// ties so the window edge is a total order.
pub(crate) fn recent_query_body(limit: usize, fields: Option<&[&str]>) -> serde_json::Value {
    let mut body = serde_json::json!({
        "sort": [{"_created_at": "desc"}, {"_id": "desc"}],
        "limit": limit,
    });
    if let Some(fields) = fields {
        body["fields"] = serde_json::json!(fields);
    }
    body
}

/// A window is saturated when it came back full — results past the limit
/// were silently pruned. Callers log this loudly; silent truncation is the
/// defect class the windowed helpers exist to eliminate.
pub(crate) fn window_saturated(returned: usize, limit: usize) -> bool {
    limit > 0 && returned >= limit
}

/// Body for a `count_only` query, optionally filtered. Counts are computed
/// server-side over ALL matching documents — no window, no limit needed.
pub(crate) fn count_query_body(filter: Option<&serde_json::Value>) -> serde_json::Value {
    match filter {
        Some(f) => serde_json::json!({"count_only": true, "filter": f}),
        None => serde_json::json!({"count_only": true}),
    }
}

/// Slow-query observability thresholds. WardSONDB reports per-query cost in
/// the response envelope's `meta` (`duration_ms`, `docs_scanned`,
/// `index_used`); a query at/over `SLOW_QUERY_MS` server-side, or one that
/// scanned `SLOW_QUERY_SCAN_RATIO`× more docs than it returned (floor keeps
/// tiny-collection ratio noise out), warns loudly. An unindexed filter shape
/// on a hot path is the defect class that put 5–8 min knowledge_query
/// latencies into production at 99k edges (`$or` → full scan, 2026-07-04).
const SLOW_QUERY_MS: f64 = 100.0;
const SLOW_QUERY_SCAN_FLOOR: u64 = 1_000;
const SLOW_QUERY_SCAN_RATIO: u64 = 10;

/// Why a query is considered slow, if it is. Pure — unit-tested; `None`
/// when meta fields are absent (older server builds omit them).
pub(crate) fn slow_query_reason(
    duration_ms: Option<f64>,
    docs_scanned: Option<u64>,
    returned: usize,
) -> Option<&'static str> {
    if duration_ms.is_some_and(|d| d >= SLOW_QUERY_MS) {
        return Some("duration");
    }
    if docs_scanned.is_some_and(|s| {
        s >= SLOW_QUERY_SCAN_FLOOR && s >= SLOW_QUERY_SCAN_RATIO * (returned.max(1) as u64)
    }) {
        return Some("scan_ratio");
    }
    None
}

/// Warn (target `wardsondb::slowquery`) when the server-reported query cost
/// crosses `slow_query_reason`'s thresholds. Deliberate maintenance
/// full-scans (orphan sweep / dump pages) will trip this — that is honest
/// observability, not noise ("every window is observable").
fn maybe_warn_slow_query(collection: &str, meta: &serde_json::Value, returned: usize) {
    let duration_ms = meta.get("duration_ms").and_then(|v| v.as_f64());
    let docs_scanned = meta.get("docs_scanned").and_then(|v| v.as_u64());
    let Some(reason) = slow_query_reason(duration_ms, docs_scanned, returned) else {
        return;
    };
    tracing::warn!(
        target: "wardsondb::slowquery",
        collection,
        duration_ms = duration_ms.unwrap_or(0.0),
        docs_scanned = docs_scanned.unwrap_or(0),
        returned,
        index_used = meta.get("index_used").and_then(|v| v.as_str()).unwrap_or("none"),
        scan_strategy = meta.get("scan_strategy").and_then(|v| v.as_str()).unwrap_or(""),
        reason,
        "slow WardSONDB query — unindexed filter shape on a hot path?"
    );
}

#[derive(Clone)]
pub struct WardsonDbClient {
    base_url: String,
    http_client: reqwest::Client,
}

#[derive(Debug, Deserialize)]
struct WardsonEnvelope<T> {
    #[allow(dead_code)]
    ok: bool,
    data: T,
    #[serde(default)]
    meta: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct InsertData {
    #[serde(alias = "id")]
    _id: String,
}

#[derive(Debug, Deserialize)]
struct CollectionInfo {
    name: String,
}

#[derive(Debug, Serialize)]
struct CreateCollectionRequest {
    name: String,
}

#[derive(Debug, Deserialize)]
struct HealthResponse {
    pub status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HealthData {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    write_pressure: Option<String>,
    #[serde(default)]
    warning: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HealthDetail {
    pub up: bool,
    pub status: String,
    pub write_pressure: Option<String>,
    pub warning: Option<String>,
}

#[derive(Debug, Deserialize)]
struct QueryResponse {
    data: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ServerInfo {
    version: Option<String>,
}

impl WardsonDbClient {
    pub fn new(port: u16) -> Self {
        Self {
            base_url: format!("http://localhost:{}", port),
            http_client: reqwest::Client::new(),
        }
    }

    /// Create a client from a full URL (Phase 1: embrad passes --wardsondb-url)
    pub fn from_url(url: &str) -> Self {
        Self {
            base_url: url.trim_end_matches('/').to_string(),
            http_client: reqwest::Client::new(),
        }
    }

    pub async fn health(&self) -> Result<bool> {
        let resp = self
            .http_client
            .get(format!("{}/_health", self.base_url))
            .send()
            .await;
        match resp {
            Ok(r) => Ok(r.status().is_success()),
            Err(_) => Ok(false),
        }
    }

    pub async fn version(&self) -> Result<String> {
        let resp = self
            .http_client
            .get(&self.base_url)
            .send()
            .await?;
        let envelope: WardsonEnvelope<ServerInfo> = resp.json().await?;
        Ok(envelope.data.version.unwrap_or_else(|| "unknown".into()))
    }

    pub async fn list_collections(&self) -> Result<Vec<String>> {
        let resp = self
            .http_client
            .get(format!("{}/_collections", self.base_url))
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(WardsonDbError::Api { status, body }.into());
        }
        let envelope: WardsonEnvelope<Vec<CollectionInfo>> = resp.json().await?;
        let collections = envelope.data;
        Ok(collections.into_iter().map(|c| c.name).collect())
    }

    pub async fn create_collection(&self, name: &str) -> Result<()> {
        let resp = self
            .http_client
            .post(format!("{}/_collections", self.base_url))
            .json(&CreateCollectionRequest {
                name: name.to_string(),
            })
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(WardsonDbError::Api { status, body }.into());
        }
        Ok(())
    }

    pub async fn collection_exists(&self, name: &str) -> Result<bool> {
        let resp = self
            .http_client
            .get(format!("{}/{}", self.base_url, name))
            .send()
            .await?;
        Ok(resp.status().is_success())
    }

    pub async fn write(
        &self,
        collection: &str,
        doc: &serde_json::Value,
    ) -> Result<String> {
        let resp = self
            .http_client
            .post(format!("{}/{}/docs", self.base_url, collection))
            .json(doc)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(WardsonDbError::Api { status, body }.into());
        }
        let envelope: WardsonEnvelope<serde_json::Value> = resp.json().await?;
        let id = envelope.data
            .get("_id")
            .or_else(|| envelope.data.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        Ok(id)
    }

    pub async fn read(
        &self,
        collection: &str,
        id: &str,
    ) -> Result<serde_json::Value> {
        let resp = self
            .http_client
            .get(format!("{}/{}/docs/{}", self.base_url, collection, id))
            .send()
            .await?;
        if resp.status().as_u16() == 404 {
            return Err(WardsonDbError::DocumentNotFound {
                collection: collection.into(),
                id: id.into(),
            }
            .into());
        }
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(WardsonDbError::Api { status, body }.into());
        }
        let envelope: WardsonEnvelope<serde_json::Value> = resp.json().await?;
        Ok(envelope.data)
    }

    pub async fn query(
        &self,
        collection: &str,
        query: &serde_json::Value,
    ) -> Result<Vec<serde_json::Value>> {
        let resp = self
            .http_client
            .post(format!("{}/{}/query", self.base_url, collection))
            .json(query)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(WardsonDbError::Api { status, body }.into());
        }
        let envelope: WardsonEnvelope<Vec<serde_json::Value>> = resp.json().await?;
        maybe_warn_slow_query(collection, &envelope.meta, envelope.data.len());
        Ok(envelope.data)
    }

    /// Fetch up to `limit` most-recent documents (sorted `_created_at desc,
    /// _id desc`). Logs a saturation warning when the window fills, so
    /// silent truncation can never recur (FIX-1).
    pub async fn fetch_recent(
        &self,
        collection: &str,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        self.fetch_recent_with_fields(collection, limit, None).await
    }

    /// `fetch_recent` with an optional server-side projection (`fields`).
    pub async fn fetch_recent_with_fields(
        &self,
        collection: &str,
        limit: usize,
        fields: Option<&[&str]>,
    ) -> Result<Vec<serde_json::Value>> {
        let body = recent_query_body(limit, fields);
        let docs = self.query(collection, &body).await?;
        if window_saturated(docs.len(), limit) {
            tracing::warn!(
                target: "wardsondb::window",
                collection,
                limit,
                "fetch_recent window saturated — results may be incomplete; raise limit or page"
            );
        }
        Ok(docs)
    }

    /// Authoritative document count for a collection via `count_only`
    /// (FIX-6). Uses `query_with_options` because the count response's
    /// `data` is an object (`{"count": N}`), not the array `query()`
    /// expects.
    pub async fn count(&self, collection: &str) -> Result<u64> {
        self.count_with_body(collection, count_query_body(None)).await
    }

    /// `count` with a server-side filter — exact matched-document counts at
    /// any scale (windowless maintenance stats ride this).
    pub async fn count_filtered(
        &self,
        collection: &str,
        filter: &serde_json::Value,
    ) -> Result<u64> {
        self.count_with_body(collection, count_query_body(Some(filter))).await
    }

    async fn count_with_body(
        &self,
        collection: &str,
        body: serde_json::Value,
    ) -> Result<u64> {
        let data = self.query_with_options(collection, &body).await?;
        data.get("count").and_then(|v| v.as_u64()).ok_or_else(|| {
            anyhow::anyhow!(
                "count_only response for '{}' missing numeric count",
                collection
            )
        })
    }

    pub async fn update(
        &self,
        collection: &str,
        id: &str,
        doc: &serde_json::Value,
    ) -> Result<()> {
        let resp = self
            .http_client
            .put(format!("{}/{}/docs/{}", self.base_url, collection, id))
            .json(doc)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(WardsonDbError::Api { status, body }.into());
        }
        Ok(())
    }

    pub async fn delete(&self, collection: &str, id: &str) -> Result<()> {
        let resp = self
            .http_client
            .delete(format!("{}/{}/docs/{}", self.base_url, collection, id))
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(WardsonDbError::Api { status, body }.into());
        }
        Ok(())
    }

    pub async fn disk_usage(&self) -> Result<serde_json::Value> {
        self.stats().await
    }

    pub async fn stats(&self) -> Result<serde_json::Value> {
        let resp = self
            .http_client
            .get(format!("{}/_stats", self.base_url))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Ok(serde_json::json!({"error": "unavailable"}));
        }
        let envelope: WardsonEnvelope<serde_json::Value> = resp.json().await?;
        Ok(envelope.data)
    }

    pub async fn health_detailed(&self) -> Result<HealthDetail> {
        let resp = self
            .http_client
            .get(format!("{}/_health", self.base_url))
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let envelope: WardsonEnvelope<HealthData> = r.json().await?;
                Ok(HealthDetail {
                    up: true,
                    status: envelope.data.status.unwrap_or_else(|| "healthy".into()),
                    write_pressure: envelope.data.write_pressure,
                    warning: envelope.data.warning,
                })
            }
            Ok(_) => Ok(HealthDetail {
                up: false,
                status: "unreachable".into(),
                write_pressure: None,
                warning: None,
            }),
            Err(_) => Ok(HealthDetail {
                up: false,
                status: "unreachable".into(),
                write_pressure: None,
                warning: None,
            }),
        }
    }

    pub async fn patch_document(
        &self,
        collection: &str,
        id: &str,
        patch: &serde_json::Value,
    ) -> Result<()> {
        let resp = self
            .http_client
            .patch(format!("{}/{}/docs/{}", self.base_url, collection, id))
            .json(patch)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(WardsonDbError::Api { status, body }.into());
        }
        Ok(())
    }

    pub async fn delete_by_query(
        &self,
        collection: &str,
        filter: &serde_json::Value,
    ) -> Result<u64> {
        let url = format!("{}/{}/docs/_delete_by_query", self.base_url, collection);
        let body = serde_json::json!({"filter": filter});
        let resp = self
            .http_client
            .post(&url)
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(WardsonDbError::Api { status, body: body_text }.into());
        }
        let envelope: WardsonEnvelope<serde_json::Value> = resp.json().await?;
        Ok(envelope
            .data
            .get("deleted")
            .and_then(|v| v.as_u64())
            .unwrap_or(0))
    }

    pub async fn set_ttl(
        &self,
        collection: &str,
        retention_days: u64,
        field: &str,
    ) -> Result<()> {
        let url = format!("{}/{}/ttl", self.base_url, collection);
        let body = serde_json::json!({"retention_days": retention_days, "field": field});
        let resp = self
            .http_client
            .put(&url)
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(WardsonDbError::Api { status, body: body_text }.into());
        }
        Ok(())
    }

    pub async fn query_with_options(
        &self,
        collection: &str,
        query_body: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let resp = self
            .http_client
            .post(format!("{}/{}/query", self.base_url, collection))
            .json(query_body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(WardsonDbError::Api { status, body }.into());
        }
        let envelope: WardsonEnvelope<serde_json::Value> = resp.json().await?;
        let returned = envelope.data.as_array().map(|a| a.len()).unwrap_or(1);
        maybe_warn_slow_query(collection, &envelope.meta, returned);
        Ok(envelope.data)
    }

    /// Create an index on a collection. Body shape:
    ///   single-field:  {"name": "...", "field": "..."}
    ///   compound:      {"name": "...", "fields": ["a", "b"]}
    /// Returns Ok(()) on both 2xx and 409 INDEX_EXISTS (idempotent).
    pub async fn create_index(
        &self,
        collection: &str,
        body: &serde_json::Value,
    ) -> Result<()> {
        let resp = self
            .http_client
            .post(format!("{}/{}/indexes", self.base_url, collection))
            .json(body)
            .send()
            .await?;
        if resp.status().is_success() || resp.status().as_u16() == 409 {
            return Ok(());
        }
        let status = resp.status().as_u16();
        let body_text = resp.text().await.unwrap_or_default();
        Err(WardsonDbError::Api { status, body: body_text }.into())
    }

    /// Bulk insert documents (max 10,000 per request). Partial-success semantics:
    /// invalid documents are skipped with per-document errors.
    /// Returns the count of successfully inserted documents.
    pub async fn bulk_write(
        &self,
        collection: &str,
        documents: &[serde_json::Value],
    ) -> Result<u64> {
        let resp = self
            .http_client
            .post(format!("{}/{}/docs/_bulk", self.base_url, collection))
            .json(&serde_json::json!({ "documents": documents }))
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(WardsonDbError::Api { status, body }.into());
        }
        let envelope: WardsonEnvelope<serde_json::Value> = resp.json().await?;
        // Response shape: { "inserted": <int>, "errors": [...] }
        let inserted = envelope
            .data
            .get("inserted")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        Ok(inserted)
    }

    /// Run an aggregation pipeline.
    pub async fn aggregate(
        &self,
        collection: &str,
        pipeline: &serde_json::Value,
    ) -> Result<Vec<serde_json::Value>> {
        let resp = self
            .http_client
            .post(format!("{}/{}/aggregate", self.base_url, collection))
            .json(&serde_json::json!({ "pipeline": pipeline }))
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(WardsonDbError::Api { status, body }.into());
        }
        let envelope: WardsonEnvelope<Vec<serde_json::Value>> = resp.json().await?;
        Ok(envelope.data)
    }
}

#[cfg(test)]
mod window_query_tests {
    //! FIX-1 query-body shape guards. There is no DB mock in this crate, so
    //! the windowed-fetch contract is enforced at the body-builder level:
    //! every windowed fetch must carry an explicit limit and a recency sort
    //! with `_id` tiebreak (one key per sort-array element).
    use super::{recent_query_body, window_saturated};
    use serde_json::json;

    #[test]
    fn recent_body_sorts_created_desc_then_id_desc() {
        let body = recent_query_body(100, None);
        assert_eq!(
            body["sort"],
            json!([{"_created_at": "desc"}, {"_id": "desc"}])
        );
    }

    #[test]
    fn recent_body_carries_exact_limit() {
        let body = recent_query_body(10_000, None);
        assert_eq!(body["limit"], json!(10_000));
    }

    #[test]
    fn recent_body_includes_projection_when_given() {
        let body = recent_query_body(50, Some(&["content", "tags"]));
        assert_eq!(body["fields"], json!(["content", "tags"]));
        let bare = recent_query_body(50, None);
        assert!(bare.get("fields").is_none());
    }

    #[test]
    fn window_saturated_fires_at_limit_not_below() {
        assert!(!window_saturated(9, 10));
        assert!(window_saturated(10, 10));
        assert!(!window_saturated(0, 0)); // zero-limit guard: never "saturated"
    }

    #[test]
    fn count_body_is_count_only() {
        let body = super::count_query_body(None);
        assert_eq!(body, json!({"count_only": true}));
    }

    #[test]
    fn count_body_includes_filter_when_given() {
        let filter = json!({"promoted_to": {"$ne": null}});
        let body = super::count_query_body(Some(&filter));
        assert_eq!(body["count_only"], json!(true));
        assert_eq!(body["filter"], filter);
    }
}

#[cfg(test)]
mod slow_query_tests {
    //! P5 observability guards: thresholds are pure and unit-enforced here;
    //! the warn itself is a tracing side effect exercised in production.
    use super::slow_query_reason;

    #[test]
    fn slow_query_reason_fires_over_100ms() {
        assert_eq!(slow_query_reason(Some(100.0), None, 10), Some("duration"));
        assert_eq!(slow_query_reason(Some(418.6), Some(99_417), 250), Some("duration"));
        assert_eq!(slow_query_reason(Some(99.9), None, 10), None);
    }

    #[test]
    fn slow_query_reason_fires_on_scan_ratio_above_floor() {
        // 5000 scanned for 20 returned: ratio 250x, above the 1000-doc floor.
        assert_eq!(slow_query_reason(Some(5.0), Some(5_000), 20), Some("scan_ratio"));
        // Zero returned still fires (the no-match full-scan case).
        assert_eq!(slow_query_reason(None, Some(99_417), 0), Some("scan_ratio"));
        // Under the floor: a tiny collection full-scan is not noise-worthy.
        assert_eq!(slow_query_reason(None, Some(500), 3), None);
        // Above the floor but ratio not met (healthy windowed fetch).
        assert_eq!(slow_query_reason(None, Some(1_500), 400), None);
    }

    #[test]
    fn slow_query_reason_silent_when_meta_absent() {
        assert_eq!(slow_query_reason(None, None, 0), None);
    }
}
