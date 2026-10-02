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

/// Window for the collections a tool owns outright — tasks, plans, drafts,
/// definitions, crons, Guardian tools, the migration ledger. They are small
/// by nature, but "small" is not a limit: asked with an empty body the
/// server answers with its default window, the first 100 documents in key
/// order, and the 101st task was invisible to the tool that created it.
pub const TOOL_COLLECTION_WINDOW: usize = 1_000;

/// Body for reading the ONE document of a single-document collection
/// (soul, identity, operator profile, config, a session's meta / history /
/// summary): explicit limit, oldest first, so the first document returned
/// is deterministically the canonical one.
pub(crate) fn first_doc_query_body() -> serde_json::Value {
    serde_json::json!({ "limit": 10, "sort": [{"_created_at": "asc"}] })
}

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
struct CollectionInfo {
    name: String,
    #[serde(default)]
    doc_count: u64,
}

#[derive(Debug, Serialize)]
struct CreateCollectionRequest {
    name: String,
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

impl WardsonDbClient {
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

    pub async fn list_collections(&self) -> Result<Vec<String>> {
        Ok(self.fetch_collections().await?.into_iter().map(|c| c.name).collect())
    }

    /// Every collection with its document count, from `GET /_collections`.
    /// The activity feed's sampler reads it; no query body is involved.
    pub async fn list_collections_with_counts(&self) -> Result<Vec<(String, u64)>> {
        Ok(self
            .fetch_collections()
            .await?
            .into_iter()
            .map(|c| (c.name, c.doc_count))
            .collect())
    }

    async fn fetch_collections(&self) -> Result<Vec<CollectionInfo>> {
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
        Ok(envelope.data)
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

    /// Drop an entire collection (docs, indexes, and registry entry).
    /// 404 is idempotent success — the ghost-sweep caller retries partially
    /// swept sessions, and a lazily-created `sessions.{name}.summary` may
    /// never have existed at all. Destructive and unrecoverable server-side;
    /// the only sanctioned caller is the boot-time reaped-session sweep
    /// (`migrations::sweep_reaped_sessions`).
    pub async fn drop_collection(&self, name: &str) -> Result<()> {
        let resp = self
            .http_client
            .delete(format!("{}/{}", self.base_url, name))
            .send()
            .await?;
        if resp.status().is_success() || resp.status().as_u16() == 404 {
            return Ok(());
        }
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Err(WardsonDbError::Api { status, body }.into())
    }

    pub async fn write(
        &self,
        collection: &str,
        doc: &serde_json::Value,
    ) -> Result<String> {
        let started = std::time::Instant::now();
        let resp = self
            .http_client
            .post(format!("{}/{}/docs", self.base_url, collection))
            .json(doc)
            .send()
            .await?;
        crate::activity::db_op(collection, crate::activity::DbVerb::Write, started);
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
        let started = std::time::Instant::now();
        let resp = self
            .http_client
            .get(format!("{}/{}/docs/{}", self.base_url, collection, id))
            .send()
            .await?;
        crate::activity::db_op(collection, crate::activity::DbVerb::Read, started);
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
        Ok(self.query_with_meta(collection, query).await?.0)
    }

    /// `query` that also returns the response envelope's `meta` object.
    /// Callers that page with `meta.next_cursor` (the maintenance scans)
    /// need it; everything else should keep using `query()`. Same status
    /// handling and slow-query warn.
    pub async fn query_with_meta(
        &self,
        collection: &str,
        query: &serde_json::Value,
    ) -> Result<(Vec<serde_json::Value>, serde_json::Value)> {
        let started = std::time::Instant::now();
        let resp = self
            .http_client
            .post(format!("{}/{}/query", self.base_url, collection))
            .json(query)
            .send()
            .await?;
        crate::activity::db_op(collection, crate::activity::DbVerb::Query, started);
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(WardsonDbError::Api { status, body }.into());
        }
        let envelope: WardsonEnvelope<Vec<serde_json::Value>> = resp.json().await?;
        maybe_warn_slow_query(collection, &envelope.meta, envelope.data.len());
        Ok((envelope.data, envelope.meta))
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

    /// Every document of a collection a tool owns, in creation order — the
    /// order an empty query body returned them in, without its silent limit
    /// of 100. The window is the newest `TOOL_COLLECTION_WINDOW`; filling it
    /// warns (`fetch_recent`).
    pub async fn fetch_collection(&self, collection: &str) -> Result<Vec<serde_json::Value>> {
        let mut docs = self.fetch_recent(collection, TOOL_COLLECTION_WINDOW).await?;
        docs.reverse();
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
        let started = std::time::Instant::now();
        let resp = self
            .http_client
            .put(format!("{}/{}/docs/{}", self.base_url, collection, id))
            .json(doc)
            .send()
            .await?;
        crate::activity::db_op(collection, crate::activity::DbVerb::Write, started);
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(WardsonDbError::Api { status, body }.into());
        }
        Ok(())
    }

    pub async fn delete(&self, collection: &str, id: &str) -> Result<()> {
        let started = std::time::Instant::now();
        let resp = self
            .http_client
            .delete(format!("{}/{}/docs/{}", self.base_url, collection, id))
            .send()
            .await?;
        crate::activity::db_op(collection, crate::activity::DbVerb::Delete, started);
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(WardsonDbError::Api { status, body }.into());
        }
        Ok(())
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
        let started = std::time::Instant::now();
        let resp = self
            .http_client
            .patch(format!("{}/{}/docs/{}", self.base_url, collection, id))
            .json(patch)
            .send()
            .await?;
        crate::activity::db_op(collection, crate::activity::DbVerb::Write, started);
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(WardsonDbError::Api { status, body }.into());
        }
        Ok(())
    }

    /// Delete every document the filter matches; returns how many.
    ///
    /// The filter is the whole condition, so an empty one matches the whole
    /// collection. Migration v8 is the one caller that passes it, to clear a
    /// diagnostic collection. The cascades that pass `$or` are cold paths:
    /// the server answers that shape with a full scan, and it never goes
    /// into a `query()` on a hot path.
    pub async fn delete_by_query(
        &self,
        collection: &str,
        filter: &serde_json::Value,
    ) -> Result<u64> {
        let started = std::time::Instant::now();
        let url = format!("{}/{}/docs/_delete_by_query", self.base_url, collection);
        let body = serde_json::json!({"filter": filter});
        let resp = self
            .http_client
            .post(&url)
            .json(&body)
            .send()
            .await?;
        crate::activity::db_op(collection, crate::activity::DbVerb::Delete, started);
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

    /// Remove a collection's TTL policy. 404 is success: the policy, or the
    /// collection, is already gone. The reaped-session sweep calls this
    /// BEFORE `drop_collection`: the server keeps a dropped collection's
    /// policy and refuses this route once the collection is gone, and its
    /// TTL worker then logs an error for the ghost on every tick, forever
    /// (Embra#15).
    pub async fn delete_ttl(&self, collection: &str) -> Result<()> {
        let resp = self
            .http_client
            .delete(format!("{}/{}/ttl", self.base_url, collection))
            .send()
            .await?;
        if resp.status().is_success() || resp.status().as_u16() == 404 {
            return Ok(());
        }
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Err(WardsonDbError::Api { status, body }.into())
    }

    pub async fn query_with_options(
        &self,
        collection: &str,
        query_body: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let started = std::time::Instant::now();
        let resp = self
            .http_client
            .post(format!("{}/{}/query", self.base_url, collection))
            .json(query_body)
            .send()
            .await?;
        crate::activity::db_op(collection, crate::activity::DbVerb::Query, started);
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
        let started = std::time::Instant::now();
        let resp = self
            .http_client
            .post(format!("{}/{}/docs/_bulk", self.base_url, collection))
            .json(&serde_json::json!({ "documents": documents }))
            .send()
            .await?;
        crate::activity::db_op(collection, crate::activity::DbVerb::Write, started);
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
        let started = std::time::Instant::now();
        let resp = self
            .http_client
            .post(format!("{}/{}/aggregate", self.base_url, collection))
            .json(&serde_json::json!({ "pipeline": pipeline }))
            .send()
            .await?;
        crate::activity::db_op(collection, crate::activity::DbVerb::Query, started);
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
    fn first_doc_body_is_limited_and_oldest_first() {
        let body = super::first_doc_query_body();
        assert_eq!(body["limit"], json!(10));
        assert_eq!(body["sort"], json!([{"_created_at": "asc"}]));
    }

    /// No query leaves this crate without a window. An empty body is not
    /// "everything": the server answers it with its default window, the
    /// first 100 documents in key order — the defect behind the 2026-07
    /// memory search freeze, and behind the 101st task, draft or definition
    /// going invisible. This reads the crate's own source, so a new call
    /// site cannot bring the pattern back unnoticed.
    ///
    /// Use `fetch_collection` / `fetch_recent` for a collection,
    /// `first_doc_query_body()` for a single-document one, or a body of
    /// your own with `limit` and `sort`.
    #[test]
    fn no_query_is_sent_with_an_empty_body() {
        fn visit(dir: &std::path::Path, hits: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(&path, hits);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                // Spelled in two pieces so this file does not find itself.
                let empty_body = ["json!(", "{})"].concat();
                let mut from = 0;
                while let Some(found) = text[from..].find(".query") {
                    let start = from + found;
                    from = start + 1;
                    // `.query(`, `.query_with_meta(`, `.query_with_options(`
                    let Some(open) = text[start..].find('(').map(|p| start + p) else {
                        continue;
                    };
                    let name = &text[start + 1..open];
                    if !matches!(name, "query" | "query_with_meta" | "query_with_options") {
                        continue;
                    }
                    // The call's arguments: up to the parenthesis that closes it.
                    let mut depth = 0usize;
                    let mut end = open;
                    for (i, c) in text[open..].char_indices() {
                        match c {
                            '(' => depth += 1,
                            ')' => {
                                depth -= 1;
                                if depth == 0 {
                                    end = open + i;
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    if text[open..end].contains(&empty_body) {
                        let line = text[..start].matches('\n').count() + 1;
                        hits.push(format!("{}:{line}", path.display()));
                    }
                }
            }
        }
        let mut hits = Vec::new();
        visit(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut hits,
        );
        assert!(
            hits.is_empty(),
            "queries sent with an empty body (server default: the first 100 \
             documents in key order):\n{}",
            hits.join("\n")
        );
    }

    /// Where a `{`-delimited literal that opens at `open` closes. Braces
    /// inside string literals do not count.
    fn literal_end(text: &str, open: usize) -> usize {
        let bytes = text.as_bytes();
        let (mut depth, mut i) = (0usize, open);
        while i < bytes.len() {
            match bytes[i] {
                b'"' => {
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'"' {
                        if bytes[i] == b'\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                }
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return i;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        bytes.len().saturating_sub(1)
    }

    /// The keys a JSON object literal has at its top level.
    fn top_level_keys(literal: &str) -> Vec<&str> {
        let bytes = literal.as_bytes();
        let (mut keys, mut depth, mut i) = (Vec::new(), 0usize, 0usize);
        while i < bytes.len() {
            match bytes[i] {
                b'"' => {
                    let start = i + 1;
                    i = start;
                    while i < bytes.len() && bytes[i] != b'"' {
                        if bytes[i] == b'\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                    let mut after = i + 1;
                    while after < bytes.len() && bytes[after].is_ascii_whitespace() {
                        after += 1;
                    }
                    if depth == 1 && bytes.get(after) == Some(&b':') {
                        keys.push(&literal[start..i.min(bytes.len())]);
                    }
                }
                b'{' | b'[' | b'(' => depth += 1,
                b'}' | b']' | b')' => depth = depth.saturating_sub(1),
                _ => {}
            }
            i += 1;
        }
        keys
    }

    #[test]
    fn the_keys_of_a_literal_are_read_at_its_top_level_only() {
        let literal = r#"{ "filter": {"limit": 1, "a": "}"}, "fields": ["x"], "limit": n }"#;
        assert_eq!(literal_end(literal, 0), literal.len() - 1);
        assert_eq!(top_level_keys(literal), ["filter", "fields", "limit"]);
    }

    /// The guard above sees an empty body in a call's arguments. A body with
    /// a projection, a filter or a sort and NO `limit` is answered with the
    /// server's default window just the same — `changelog` asked for three
    /// fields of `memory.entries` that way and was given the oldest 100
    /// entries. This reads every JSON object literal of the crate, wherever
    /// it is bound, and asks those shaped like a query for a `limit`.
    #[test]
    fn every_query_body_carries_a_limit() {
        fn visit(dir: &std::path::Path, hits: &mut Vec<String>) {
            // Spelled in two pieces so this file does not find itself.
            let opener = ["json!", "("].concat();
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(&path, hits);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                let mut from = 0;
                while let Some(found) = text[from..].find(&opener) {
                    let at = from + found;
                    from = at + opener.len();
                    let rest = text[from..].trim_start();
                    if !rest.starts_with('{') {
                        continue;
                    }
                    let open = text.len() - rest.len();
                    let literal = &text[open..=literal_end(&text, open)];
                    let keys = top_level_keys(literal);
                    let is_query = ["filter", "fields", "sort"].iter().any(|k| keys.contains(k));
                    let bounded = keys.contains(&"limit") || keys.contains(&"count_only");
                    // Two literals have such a key and are no query: an index
                    // specification (`name` + `fields`), and the request that
                    // `delete_by_query` wraps around the caller's filter.
                    let index_spec = keys.contains(&"name");
                    let delete_request =
                        literal.split_whitespace().collect::<String>() == r#"{"filter":filter}"#;
                    if is_query && !bounded && !index_spec && !delete_request {
                        let line = text[..at].matches('\n').count() + 1;
                        hits.push(format!("{}:{line}", path.display()));
                    }
                }
            }
        }
        let mut hits = Vec::new();
        visit(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut hits,
        );
        assert!(
            hits.is_empty(),
            "query bodies without a `limit` (server default: the first 100 \
             documents in key order):\n{}",
            hits.join("\n")
        );
    }

    /// Whether a manifest line makes the vendored crate a dependency.
    fn names_the_database_crate(line: &str) -> bool {
        let line = line.trim();
        if line.starts_with('#') {
            return false;
        }
        let key = line.split(['=', ' ']).next().unwrap_or("");
        key == "wardsondb"
            || line.contains("dependencies.wardsondb")
            || (line.contains("package") && line.contains("\"wardsondb\""))
    }

    #[test]
    fn a_manifest_line_that_names_the_database_crate_is_recognized() {
        for line in [
            r#"wardsondb = { path = "../wardsondb" }"#,
            r#"wardsondb={ path = "../wardsondb" }"#,
            r#"  wardsondb = "0.9""#,
            "[dependencies.wardsondb]",
            "[dev-dependencies.wardsondb]",
            r#"db = { package = "wardsondb", path = "../wardsondb" }"#,
        ] {
            assert!(names_the_database_crate(line), "{line}");
        }
        for line in [
            r#"# wardsondb = { path = "../wardsondb" }"#,
            r#"    "crates/wardsondb","#,
            "# HTTP client (for WardSONDB REST API)",
            r#"reqwest = { version = "0.12" }"#,
            r#"name = "wardsondb-client""#,
        ] {
            assert!(!names_the_database_crate(line), "{line}");
        }
    }

    /// The services reach WardSONDB over REST, through this client. The
    /// vendored crate has a library target and would compile as a
    /// dependency; it would bring the database's allocator and both storage
    /// engines into a service.
    #[test]
    fn no_crate_depends_on_the_database_library() {
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates/");
        let mut manifests = vec![crates.parent().expect("workspace root").join("Cargo.toml")];
        for entry in std::fs::read_dir(crates).unwrap() {
            let dir = entry.unwrap().path();
            if dir.file_name().and_then(|n| n.to_str()) != Some("wardsondb") {
                manifests.push(dir.join("Cargo.toml"));
            }
        }
        let mut hits = Vec::new();
        for manifest in manifests.iter().filter(|m| m.is_file()) {
            let text = std::fs::read_to_string(manifest).unwrap();
            for (n, line) in text.lines().enumerate() {
                if names_the_database_crate(line) {
                    hits.push(format!("{}:{}: {}", manifest.display(), n + 1, line.trim()));
                }
            }
        }
        assert!(manifests.len() > 10, "manifests found: {}", manifests.len());
        assert!(hits.is_empty(), "a crate depends on wardsondb:\n{}", hits.join("\n"));
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

#[cfg(test)]
mod activity_tap_tests {
    //! The first test that points `WardsonDbClient` at a stub server: the
    //! activity feed counts what the client does, per collection and verb.
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn count_of(
        totals: &embra_common::proto::brain::ActivityTotals,
        collection: &str,
        verb: &str,
    ) -> u64 {
        totals
            .db_by_collection
            .iter()
            .find(|d| d.collection == collection && d.verb == verb)
            .map(|d| d.count)
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn a_write_and_a_query_are_counted_per_collection_and_verb() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/activity.tap/docs"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "data": {"_id": "x"}})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/activity.tap/query"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "data": [], "meta": {}})),
            )
            .mount(&server)
            .await;
        let db = WardsonDbClient::from_url(&server.uri());

        let before = crate::activity::totals();
        db.write("activity.tap", &json!({"a": 1})).await.unwrap();
        db.query("activity.tap", &recent_query_body(1, None)).await.unwrap();
        let after = crate::activity::totals();

        assert_eq!(
            count_of(&after, "activity.tap", "write") - count_of(&before, "activity.tap", "write"),
            1
        );
        assert_eq!(
            count_of(&after, "activity.tap", "query") - count_of(&before, "activity.tap", "query"),
            1
        );
        assert!(after.db_ops - before.db_ops >= 2, "totals count every request");
    }
}

#[cfg(test)]
mod ttl_policy_tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A policy that is already gone, or a collection that is, is not an
    /// error for the sweep; anything else is surfaced with its status.
    #[tokio::test]
    async fn delete_ttl_treats_404_as_success_and_surfaces_other_errors() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/gone/ttl"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/kept/ttl"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": true, "data": {"deleted": true}, "meta": {}})),
            )
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/broken/ttl"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let db = WardsonDbClient::from_url(&server.uri());

        assert!(db.delete_ttl("gone").await.is_ok(), "404 is success");
        assert!(db.delete_ttl("kept").await.is_ok());
        let err = db.delete_ttl("broken").await.unwrap_err().to_string();
        assert!(err.contains("500"), "the status is in the error: {err}");
    }
}
