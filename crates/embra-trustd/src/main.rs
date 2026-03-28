//! embra-trustd — PKI and soul verification service.

mod config;
mod soul;
mod pki;

use crate::config::TrustdConfig;
use crate::soul::SoulVerifier;
use crate::pki::PKIManager;

use embra_common::proto::trust::trust_service_server::{TrustService, TrustServiceServer};
use embra_common::proto::trust::*;
use embra_common::proto::common;

use std::sync::Arc;
use tokio::sync::RwLock;
use tonic::{transport::Server, Request, Response, Status};
use tracing::info;

struct TrustServiceImpl {
    verifier: Arc<SoulVerifier>,
    pki: Arc<RwLock<PKIManager>>,
    start_time: std::time::Instant,
}

#[tonic::async_trait]
impl TrustService for TrustServiceImpl {
    async fn verify_soul(
        &self,
        request: Request<VerifySoulRequest>,
    ) -> Result<Response<VerifySoulResponse>, Status> {
        let req = request.into_inner();
        let (valid, computed, stored, error) = self.verifier.verify().await;

        // If caller provided an expected hash, also check against that
        if !req.expected_hash.is_empty() && computed != req.expected_hash {
            let error = format!("Expected hash {} but computed {}", req.expected_hash, computed);
            return Ok(Response::new(VerifySoulResponse {
                valid: false,
                computed_hash: computed,
                stored_hash: stored,
                error,
            }));
        }

        Ok(Response::new(VerifySoulResponse {
            valid,
            computed_hash: computed,
            stored_hash: stored,
            error,
        }))
    }

    async fn get_soul_status(
        &self,
        _request: Request<GetSoulStatusRequest>,
    ) -> Result<Response<GetSoulStatusResponse>, Status> {
        let (valid, hash, _, _) = self.verifier.verify().await;

        Ok(Response::new(GetSoulStatusResponse {
            status: Some(common::SoulStatus {
                sealed: !hash.is_empty(),
                sha256: hash,
                sealed_at: None,
                verified: valid,
            }),
        }))
    }

    async fn generate_certificate(
        &self,
        request: Request<GenerateCertRequest>,
    ) -> Result<Response<GenerateCertResponse>, Status> {
        let req = request.into_inner();
        let pki = self.pki.read().await;

        let (is_server, is_client) = match req.role() {
            CertRole::Server => (true, false),
            CertRole::Client => (false, true),
            CertRole::Peer => (true, true),
            _ => (true, true), // Default to peer
        };

        let (cert_pem, key_pem) = pki.generate_service_cert(
            &req.common_name,
            &req.san_dns,
            &req.san_ip,
            is_server,
            is_client,
        ).map_err(|e| Status::internal(format!("Certificate generation failed: {}", e)))?;

        Ok(Response::new(GenerateCertResponse {
            cert_pem,
            key_pem,
            serial: String::new(),
            expires_at: String::new(),
        }))
    }

    async fn get_ca_certificate(
        &self,
        _request: Request<GetCaRequest>,
    ) -> Result<Response<GetCaResponse>, Status> {
        let pki = self.pki.read().await;
        let ca_pem = pki.ca_cert_pem()
            .map_err(|e| Status::internal(format!("Failed to read CA cert: {}", e)))?;

        Ok(Response::new(GetCaResponse {
            ca_cert_pem: ca_pem,
        }))
    }

    async fn health_check(
        &self,
        _request: Request<common::HealthCheckRequest>,
    ) -> Result<Response<common::HealthCheckResponse>, Status> {
        Ok(Response::new(common::HealthCheckResponse {
            status: common::HealthStatus::Healthy as i32,
            service_name: "embra-trustd".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: self.start_time.elapsed().as_secs(),
            details: std::collections::HashMap::new(),
        }))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let config = TrustdConfig::from_args();

    info!("embra-trustd starting on port {}", config.port);

    // Initialize PKI
    let mut pki = PKIManager::new(config.pki_dir());
    pki.init()?;
    let pki = Arc::new(RwLock::new(pki));

    // Initialize soul verifier
    let verifier = Arc::new(SoulVerifier::new(
        config.wardsondb_url.clone(),
        config.soul_hash_path(),
    ));

    let service = TrustServiceImpl {
        verifier,
        pki,
        start_time: std::time::Instant::now(),
    };

    let addr = format!("0.0.0.0:{}", config.port).parse()?;
    info!("embra-trustd listening on {}", addr);

    Server::builder()
        .add_service(TrustServiceServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}
