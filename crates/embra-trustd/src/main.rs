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
        Ok(Response::new(apply_expected_hash(&req.expected_hash, valid, computed, stored, error)))
    }

    async fn get_soul_status(
        &self,
        _request: Request<GetSoulStatusRequest>,
    ) -> Result<Response<GetSoulStatusResponse>, Status> {
        let (valid, computed, stored, _) = self.verifier.verify().await;

        // `sealed`: a hash is stored for this instance, so the operator
        // sealed it. `verified`: the soul as stored matches it now. Both
        // false on a first run; sealed and not verified is a tampered or
        // lost soul (#15: `sealed` used to mean "a document exists").
        Ok(Response::new(GetSoulStatusResponse {
            status: Some(common::SoulStatus {
                sealed: !stored.is_empty(),
                sha256: computed,
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

/// The reply to `VerifySoul`: the verifier's verdict, unless the caller
/// named the hash it expects and the computed one differs, which fails the
/// verdict with both hashes kept for the caller's diagnosis. An empty
/// expectation is no expectation.
fn apply_expected_hash(
    expected: &str,
    valid: bool,
    computed: String,
    stored: String,
    error: String,
) -> VerifySoulResponse {
    if !expected.is_empty() && computed != expected {
        return VerifySoulResponse {
            valid: false,
            error: format!("Expected hash {} but computed {}", expected, computed),
            computed_hash: computed,
            stored_hash: stored,
        };
    }
    VerifySoulResponse { valid, computed_hash: computed, stored_hash: stored, error }
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

#[cfg(test)]
mod expected_hash_tests {
    use super::apply_expected_hash;

    #[test]
    fn an_expectation_overrides_the_verdict_only_when_it_differs() {
        let ok = apply_expected_hash("", true, "aa".into(), "aa".into(), String::new());
        assert!(ok.valid && ok.error.is_empty(), "no expectation keeps the verdict");
        let same = apply_expected_hash("aa", true, "aa".into(), "aa".into(), String::new());
        assert!(same.valid, "an expectation that matches changes nothing");
        let differs = apply_expected_hash("bb", true, "aa".into(), "aa".into(), String::new());
        assert!(!differs.valid);
        assert!(differs.error.contains("Expected hash bb but computed aa"), "{}", differs.error);
        assert_eq!((differs.computed_hash.as_str(), differs.stored_hash.as_str()), ("aa", "aa"));
        let failed = apply_expected_hash("", false, "aa".into(), "bb".into(), "Hash mismatch".into());
        assert!(!failed.valid && failed.error == "Hash mismatch");
    }
}
