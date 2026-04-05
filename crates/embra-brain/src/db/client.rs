use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::error::WardsonDbError;

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
        Ok(envelope.data)
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
