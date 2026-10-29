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
        permitted(&req).map_err(Status::invalid_argument)?;
        let pki = self.pki.read().await;

        let (is_server, is_client) = usages_for(req.role());

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

/// The extended key usages a certificate role asks for, as
/// `(is_server, is_client)`: `Server` is server-auth alone, `Client` is
/// client-auth alone, `Peer` and an unspecified role are both. The leaf's
/// EKU follows this pair (`pki.rs::generate_service_cert`).
fn usages_for(role: CertRole) -> (bool, bool) {
    match role {
        CertRole::Server => (true, false),
        CertRole::Client => (false, true),
        CertRole::Peer | CertRole::Unspecified => (true, true),
    }
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

#[cfg(test)]
mod role_tests {
    use super::{CertRole, usages_for};

    /// The role on the wire maps to the usages the leaf carries (#52 item
    /// 4): a server role is server-auth alone, a client role client-auth
    /// alone, a peer or an unspecified role both — the default the proto's
    /// zero value takes.
    #[test]
    fn a_role_maps_to_its_usages() {
        assert_eq!(usages_for(CertRole::Server), (true, false));
        assert_eq!(usages_for(CertRole::Client), (false, true));
        assert_eq!(usages_for(CertRole::Peer), (true, true));
        assert_eq!(usages_for(CertRole::Unspecified), (true, true));
        assert_eq!(usages_for(CertRole::try_from(0).unwrap()), (true, true), "the wire's zero value");
    }
}
/// The gate of `GenerateCertificate` (#10): a certificate is minted for the
/// OS's own service identities (`pki::SERVICE_COMMON_NAMES`,
/// `SERVICE_DNS_NAMES`, `SERVICE_IPS`) and for nothing a request names
/// outside them. The port is unauthenticated by the Phase-1 posture, so
/// the request's list is never the list of what the CA signs; the first
/// name outside the set is named in the refusal.
fn permitted(req: &GenerateCertRequest) -> Result<(), String> {
    let cn = req.common_name.as_str();
    if !pki::SERVICE_COMMON_NAMES.contains(&cn) && !pki::SERVICE_DNS_NAMES.contains(&cn) {
        return Err(format!("common name {cn:?} is not a service identity of this OS"));
    }
    if let Some(dns) = req.san_dns.iter().find(|d| !pki::SERVICE_DNS_NAMES.contains(&d.as_str())) {
        return Err(format!("DNS name {dns:?} is not a service identity of this OS"));
    }
    if let Some(ip) = req.san_ip.iter().find(|i| !pki::SERVICE_IPS.contains(&i.as_str())) {
        return Err(format!("address {ip:?} is not a service identity of this OS"));
    }
    Ok(())
}

#[cfg(test)]
mod minting_gate_tests {
    use super::*;

    fn request(cn: &str, dns: &[&str], ips: &[&str]) -> GenerateCertRequest {
        GenerateCertRequest {
            common_name: cn.to_string(),
            san_dns: dns.iter().map(|s| s.to_string()).collect(),
            san_ip: ips.iter().map(|s| s.to_string()).collect(),
            role: CertRole::Server as i32,
        }
    }

    /// The request embra-web sends passes; a common name, a DNS name or an
    /// address outside the service set is refused, naming it, before the
    /// CA signs anything — on the pure gate and through the RPC (#10).
    #[tokio::test]
    async fn a_name_outside_the_service_set_is_refused_before_minting() {
        let web = request("embra-web", &["localhost", "buildroot", "embraos"], &["127.0.0.1", "10.0.2.15"]);
        assert_eq!(permitted(&web), Ok(()));
        assert_eq!(permitted(&request("localhost", &[], &[])), Ok(()), "a service name as the CN");
        let cases = [
            (request("evil.example", &["localhost"], &[]), "common name \"evil.example\""),
            (request("embra-web", &["localhost", "evil.example"], &[]), "DNS name \"evil.example\""),
            (request("embra-web", &[], &["192.168.1.9"]), "address \"192.168.1.9\""),
            (request("embra-web", &["sub.localhost"], &[]), "DNS name \"sub.localhost\""),
        ];
        for (req, named) in cases {
            let err = permitted(&req).expect_err(named);
            assert!(err.starts_with(named), "{err}");
        }

        let dir = std::env::temp_dir().join(format!("embra-trustd-gate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut pki = PKIManager::new(dir.clone());
        pki.init().expect("a fresh CA");
        let service = TrustServiceImpl {
            verifier: Arc::new(SoulVerifier::new("http://127.0.0.1:1".to_string(), dir.join("soul.sha256"))),
            pki: Arc::new(RwLock::new(pki)),
            start_time: std::time::Instant::now(),
        };
        let minted = service
            .generate_certificate(Request::new(web))
            .await
            .expect("the web console's request is minted")
            .into_inner();
        assert!(String::from_utf8_lossy(&minted.cert_pem).contains("BEGIN CERTIFICATE"));
        let refused = service
            .generate_certificate(Request::new(request("embra-web", &["evil.example"], &[])))
            .await
            .expect_err("a name outside the set");
        assert_eq!(refused.code(), tonic::Code::InvalidArgument);
        assert!(refused.message().contains("\"evil.example\""), "{}", refused.message());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
