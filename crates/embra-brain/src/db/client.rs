//! WardSONDB REST client.
//!
//! Stub that provides the interface expected by grpc_service.rs and other modules.
//! Will be replaced with Phase 0's full WardsonDbClient implementation.

use std::time::Duration;
use anyhow::Result;
use tracing::{info, warn};

#[derive(Clone)]
pub struct WardsonClient {
    base_url: String,
    http_client: reqwest::Client,
}

impl WardsonClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.to_string(),
            http_client: reqwest::Client::new(),
        }
    }

    /// Wait for WardSONDB to become healthy.
    pub async fn wait_for_health(&self, timeout: Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;

        while std::time::Instant::now() < deadline {
            match self.http_client
                .get(format!("{}/_health", self.base_url))
                .timeout(Duration::from_secs(2))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    info!("WardSONDB health check passed");
                    return Ok(());
                }
                Ok(_) => {
                    warn!("WardSONDB not ready yet");
                }
                Err(_) => {
                    warn!("WardSONDB not reachable yet");
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        anyhow::bail!("WardSONDB did not become healthy within {:?}", timeout)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}
