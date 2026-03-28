//! Manages gRPC connections to backend services (embra-brain, embra-trustd).

use embra_common::proto::brain::brain_service_client::BrainServiceClient;
use embra_common::proto::trust::trust_service_client::TrustServiceClient;
use tonic::transport::Channel;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

#[derive(Clone)]
pub struct BackendConnections {
    brain_addr: String,
    trust_addr: String,
    brain: Arc<RwLock<Option<BrainServiceClient<Channel>>>>,
    trust: Arc<RwLock<Option<TrustServiceClient<Channel>>>>,
}

impl BackendConnections {
    pub fn new(brain_addr: String, trust_addr: String) -> Self {
        Self {
            brain_addr,
            trust_addr,
            brain: Arc::new(RwLock::new(None)),
            trust: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn brain_client(&self) -> Result<BrainServiceClient<Channel>, tonic::Status> {
        // Try cached connection first
        {
            let guard = self.brain.read().await;
            if let Some(client) = guard.as_ref() {
                return Ok(client.clone());
            }
        }

        // Connect
        let channel = Channel::from_shared(self.brain_addr.clone())
            .map_err(|e| tonic::Status::internal(format!("Invalid brain address: {}", e)))?
            .connect()
            .await
            .map_err(|e| tonic::Status::unavailable(format!("Brain service unavailable: {}", e)))?;

        let client = BrainServiceClient::new(channel);
        *self.brain.write().await = Some(client.clone());
        info!("Connected to embra-brain at {}", self.brain_addr);
        Ok(client)
    }

    pub async fn trust_client(&self) -> Result<TrustServiceClient<Channel>, tonic::Status> {
        {
            let guard = self.trust.read().await;
            if let Some(client) = guard.as_ref() {
                return Ok(client.clone());
            }
        }

        let channel = Channel::from_shared(self.trust_addr.clone())
            .map_err(|e| tonic::Status::internal(format!("Invalid trust address: {}", e)))?
            .connect()
            .await
            .map_err(|e| tonic::Status::unavailable(format!("Trust service unavailable: {}", e)))?;

        let client = TrustServiceClient::new(channel);
        *self.trust.write().await = Some(client.clone());
        info!("Connected to embra-trustd at {}", self.trust_addr);
        Ok(client)
    }

    /// Clear cached connections (e.g., after a backend restart)
    pub async fn _reset(&self) {
        *self.brain.write().await = None;
        *self.trust.write().await = None;
        warn!("Backend connections reset");
    }
}
