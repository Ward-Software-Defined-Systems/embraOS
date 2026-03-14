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
        // Use stats endpoint for overall storage info
        let resp = self
            .http_client
            .get(format!("{}/_stats", self.base_url))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Ok(serde_json::json!({"error": "unavailable"}));
        }
        let stats: serde_json::Value = resp.json().await?;
        Ok(stats)
    }
}
