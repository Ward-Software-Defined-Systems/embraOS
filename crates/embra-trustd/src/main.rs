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

use embra_boot::names::{self, LeafNames};
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tonic::{transport::Server, Request, Response, Status};
use tracing::info;

/// What trustd reads of the machine when it mints: the kernel's name for
/// it, the addresses its cards hold, and the name the operator stored
/// (`/network hostname`).
#[derive(Clone, Debug, Default)]
struct Seen {
    hostname: Option<String>,
    addresses: Vec<IpAddr>,
    operator: Option<String>,
}

impl Seen {
    fn now() -> Seen {
        let (settings, _) = embra_boot::net_settings::read(std::path::Path::new(embra_boot::net_settings::SETTINGS_PATH));
        Seen { hostname: names::hostname(), addresses: names::addresses(), operator: settings.hostname }
    }
}

struct TrustServiceImpl {
    verifier: Arc<SoulVerifier>,
    pki: Arc<RwLock<PKIManager>>,
    start_time: std::time::Instant,
    /// The machine as it is at a request: [`Seen::now`], or a test's own.
    seen: Arc<dyn Fn() -> Seen + Send + Sync>,
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
        // On the blocking pool: the machine's addresses are read under the
        // kernel's network lock, which a card's starting driver holds for
        // seconds, and this is one of the runtime's own threads.
        let look = self.seen.clone();
        let seen = tokio::task::spawn_blocking(move || look()).await.map_err(|e| Status::internal(format!("The machine's names could not be read: {e}")))?;
        let mut pki = self.pki.write().await;
        // The operator's name for the machine is a name the CA signs from
        // here on: its certificate is issued again when it does not admit it.
        pki.admit(seen.operator.as_deref())
            .map_err(|e| Status::internal(format!("The CA certificate could not be issued again: {e}")))?;
        let own = names::leaf_names(pki.set(), seen.hostname.as_deref(), &seen.addresses);
        let (dns, ips) = requested(&req, &own).map_err(Status::invalid_argument)?;

        let (is_server, is_client) = usages_for(req.role());

        let minted = pki
            .generate_service_cert(&req.common_name, &dns, &ips, is_server, is_client)
            .map_err(|e| Status::internal(format!("Certificate generation failed: {}", e)))?;
        info!(
            "A certificate for {}: {}",
            req.common_name,
            dns.iter().cloned().chain(ips.iter().map(|ip| ip.to_string())).collect::<Vec<_>>().join(" ")
        );

        Ok(Response::new(GenerateCertResponse {
            cert_pem: minted.cert_pem,
            key_pem: minted.key_pem,
            serial: minted.serial,
            expires_at: minted.expires_at.to_string(),
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

/// The default log filter: this crate and every other at INFO. `RUST_LOG`
/// overrides it (`log_filter`). A bare `tracing_subscriber::fmt::init()`
/// read `RUST_LOG` too, and when it was unset — embrad sets it for no
/// child — fell back to ERROR alone, so the log embrad captured for this
/// service stayed empty on the image.
const DEFAULT_LOG_DIRECTIVE: &str = "info";

/// The filter `main` installs: `RUST_LOG` when it is set and parses,
/// otherwise the default.
fn log_filter() -> tracing_subscriber::EnvFilter {
    log_filter_from(std::env::var("RUST_LOG").ok().as_deref())
}

fn log_filter_from(rust_log: Option<&str>) -> tracing_subscriber::EnvFilter {
    rust_log
        .and_then(|spec| tracing_subscriber::EnvFilter::try_new(spec).ok())
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new(DEFAULT_LOG_DIRECTIVE))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // The subscriber never prints its own complaint: that is an eprintln!, a
    // panic when the log stream refuses a write (a full tmpfs, a dead console).
    tracing_subscriber::fmt().with_env_filter(log_filter()).with_ansi(false).log_internal_errors(false).init();
    let config = TrustdConfig::from_args();

    info!("embra-trustd starting on port {}", config.port);

    // Initialize PKI
    let mut pki = PKIManager::new(config.pki_dir());
    pki.init(Seen::now().operator.as_deref())?;
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
        seen: Arc::new(Seen::now),
    };

    let addr = format!("{}:{}", config.bind, config.port).parse()?;
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
/// OS's own service identities (`pki::SERVICE_COMMON_NAMES`) and carries
/// this machine's own names (`own`: `localhost`, its name when the CA
/// admits it, the loopback addresses and the private addresses its cards
/// hold). A request without names gets them all, which is what embra-web
/// sends: the list is then read in one place, and a name that changed
/// between two readings cannot fail the request of a service the boot
/// cannot do without. A request that lists names gets those, and the first
/// one the machine does not hold is named in the refusal. The port is
/// unauthenticated by the Phase-1 posture, so the request's list is never
/// the list of what the CA signs.
fn requested(req: &GenerateCertRequest, own: &LeafNames) -> Result<(Vec<String>, Vec<IpAddr>), String> {
    let cn = req.common_name.as_str();
    if !pki::SERVICE_COMMON_NAMES.contains(&cn) {
        return Err(format!("common name {cn:?} is not a service identity of this OS"));
    }
    if req.san_dns.is_empty() && req.san_ip.is_empty() {
        return Ok((own.dns.clone(), own.ips.clone()));
    }
    if let Some(dns) = req.san_dns.iter().find(|d| !own.dns.iter().any(|have| have.eq_ignore_ascii_case(d))) {
        return Err(format!("DNS name {dns:?} is not a name of this machine"));
    }
    let mut ips = Vec::with_capacity(req.san_ip.len());
    for text in &req.san_ip {
        match text.parse::<IpAddr>() {
            Ok(ip) if own.ips.contains(&ip) => ips.push(ip),
            _ => return Err(format!("address {text:?} is not an address this machine holds")),
        }
    }
    Ok((req.san_dns.clone(), ips))
}

#[cfg(test)]
mod minting_gate_tests {
    use super::*;
    use embra_boot::names::CaSet;
    use rustls::client::danger::ServerCertVerifier;
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

    fn request(cn: &str, dns: &[&str], ips: &[&str]) -> GenerateCertRequest {
        GenerateCertRequest {
            common_name: cn.to_string(),
            san_dns: dns.iter().map(|s| s.to_string()).collect(),
            san_ip: ips.iter().map(|s| s.to_string()).collect(),
            role: CertRole::Server as i32,
        }
    }

    fn seen(hostname: Option<&str>, operator: Option<&str>, addresses: &[&str]) -> Seen {
        Seen { hostname: hostname.map(str::to_string), operator: operator.map(str::to_string), addresses: addresses.iter().map(|a| a.parse().unwrap()).collect() }
    }

    /// The service over a fresh CA in a directory of the test's own, on a
    /// machine that is what `machine` says.
    fn service_on(tag: &str, machine: Seen) -> (TrustServiceImpl, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("embra-trustd-gate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut pki = PKIManager::new(dir.clone());
        pki.init(None).expect("a fresh CA");
        let service = TrustServiceImpl {
            verifier: Arc::new(SoulVerifier::new("http://127.0.0.1:1".to_string(), dir.join("soul.sha256"))),
            pki: Arc::new(RwLock::new(pki)),
            start_time: std::time::Instant::now(),
            seen: Arc::new(move || machine.clone()),
        };
        (service, dir)
    }

    /// Whether `name` verifies on `leaf` for a browser that imported `ca`.
    fn verifies(ca_pem: &[u8], leaf_pem: &[u8], name: &str) -> bool {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from_pem_slice(ca_pem).unwrap()).unwrap();
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let browser = rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider).build().unwrap();
        let leaf = CertificateDer::from_pem_slice(leaf_pem).unwrap();
        browser.verify_server_cert(&leaf, &[], &ServerName::try_from(name.to_string()).unwrap(), &[], UnixTime::now()).is_ok()
    }

    /// embra-web's request names nothing: trustd reads the machine and the
    /// certificate carries `localhost`, the machine's name, the loopback
    /// addresses and the private addresses its cards hold, under a CA that
    /// was issued again for the operator's name. A public address the
    /// machine holds is on no certificate.
    #[tokio::test]
    async fn this_machines_names_are_filled_in_by_trustd() {
        let (service, dir) = service_on("filled", seen(Some("axiom"), Some("axiom"), &["127.0.0.1", "192.168.1.42", "203.0.113.7", "fe80::5054:ff:fe12:3456"]));
        let before = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        let minted = service.generate_certificate(Request::new(request("embra-web", &[], &[]))).await.expect("the web console's request is minted").into_inner();
        let ca = service.pki.read().await.ca_cert_pem().unwrap();
        for held in ["localhost", "axiom", "127.0.0.1", "::1", "192.168.1.42"] {
            assert!(verifies(&ca, &minted.cert_pem, held), "{held}");
        }
        for not_named in ["203.0.113.7", "fe80::5054:ff:fe12:3456", "embraos", "buildroot", "10.0.2.15"] {
            assert!(!verifies(&ca, &minted.cert_pem, not_named), "{not_named}");
        }
        // The CA admits the operator's name from this request on, and says so in its record.
        assert_eq!(CaSet::read(&dir), Some(CaSet::Private { hostname: Some("axiom".into()) }));
        // When it ends and which one it is.
        let ends: i64 = minted.expires_at.parse().expect("seconds after the epoch");
        assert!((ends - before - 365 * 86_400).abs() < 60, "{ends}");
        assert_eq!(minted.serial.len(), 32);
        let _ = std::fs::remove_dir_all(&dir);

        // A name a DHCP server assigned is the machine's name and is not on
        // the certificate: the CA was not made with it and is not issued
        // again for a name that changes with the network.
        let (service, dir) = service_on("assigned", seen(Some("node-7"), None, &["10.0.2.15"]));
        let first_ca = service.pki.read().await.ca_cert_pem().unwrap();
        let minted = service.generate_certificate(Request::new(request("embra-web", &[], &[]))).await.unwrap().into_inner();
        assert_eq!(service.pki.read().await.ca_cert_pem().unwrap(), first_ca, "the CA stays as it is");
        assert!(verifies(&first_ca, &minted.cert_pem, "localhost") && verifies(&first_ca, &minted.cert_pem, "10.0.2.15"));
        assert!(!verifies(&first_ca, &minted.cert_pem, "node-7"));
        let _ = std::fs::remove_dir_all(&dir);

        // The default name is admitted by every CA made here.
        let (service, dir) = service_on("default", seen(Some("embraos"), None, &[]));
        let minted = service.generate_certificate(Request::new(request("embra-web", &[], &[]))).await.unwrap().into_inner();
        let ca = service.pki.read().await.ca_cert_pem().unwrap();
        assert!(verifies(&ca, &minted.cert_pem, "embraos") && verifies(&ca, &minted.cert_pem, "127.0.0.1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A request that lists names is held to the machine's own: a common
    /// name, a DNS name or an address the machine does not hold is refused,
    /// naming it, before the CA signs anything — on the pure gate and
    /// through the RPC (#10).
    #[tokio::test]
    async fn a_listed_name_the_machine_does_not_hold_is_refused() {
        let own = names::leaf_names(&CaSet::Private { hostname: None }, Some("embraos"), &["10.0.2.15".parse().unwrap()]);
        let listed = requested(&request("embra-web", &["localhost", "EmbraOS"], &["127.0.0.1", "10.0.2.15"]), &own).expect("names the machine holds");
        assert_eq!(listed.0, ["localhost", "EmbraOS"]);
        assert_eq!(listed.1, ["127.0.0.1".parse::<IpAddr>().unwrap(), "10.0.2.15".parse().unwrap()]);
        assert_eq!(requested(&request("embra-web", &[], &[]), &own), Ok((own.dns.clone(), own.ips.clone())), "no list: the machine's own");
        let cases = [
            (request("evil.example", &["localhost"], &[]), "common name \"evil.example\""),
            (request("localhost", &[], &[]), "common name \"localhost\""),
            (request("embra-web", &["localhost", "evil.example"], &[]), "DNS name \"evil.example\""),
            (request("embra-web", &["buildroot"], &[]), "DNS name \"buildroot\""),
            (request("embra-web", &[], &["192.168.1.9"]), "address \"192.168.1.9\""),
            (request("embra-web", &[], &["not an address"]), "address \"not an address\""),
            (request("embra-web", &["sub.localhost"], &[]), "DNS name \"sub.localhost\""),
        ];
        for (req, named) in cases {
            let err = requested(&req, &own).expect_err(named);
            assert!(err.starts_with(named), "{err}");
        }

        let (service, dir) = service_on("refused", seen(Some("embraos"), None, &["10.0.2.15"]));
        let refused = service.generate_certificate(Request::new(request("embra-web", &["evil.example"], &[]))).await.expect_err("a name outside the machine's own");
        assert_eq!(refused.code(), tonic::Code::InvalidArgument);
        assert!(refused.message().contains("\"evil.example\""), "{}", refused.message());
        let held = service.generate_certificate(Request::new(request("embra-web", &["localhost"], &["10.0.2.15"]))).await.expect("names the machine holds").into_inner();
        assert!(String::from_utf8_lossy(&held.cert_pem).contains("BEGIN CERTIFICATE"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod log_filter_tests {
    use super::log_filter_from;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    /// A writer the subscriber under test appends to.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The lines a subscriber built like `main`'s writes under a filter,
    /// with the sink in place of stderr.
    fn logged_under(rust_log: Option<&str>, emit: impl FnOnce()) -> String {
        let sink = Sink::default();
        let writer = sink.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(log_filter_from(rust_log))
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, emit);
        String::from_utf8(sink.0.lock().unwrap().clone()).unwrap()
    }

    /// Without `RUST_LOG`, the crate's INFO lines are written and nothing
    /// quieter; another crate's warnings still reach the log. The bare
    /// `fmt::init()` wrote ERROR alone here, and the service's log on the
    /// image was empty.
    #[test]
    fn the_default_filter_passes_the_crates_info_lines_and_nothing_quieter() {
        let out = logged_under(None, || {
            tracing::info!(target: "embra_trustd", "an info line of the service");
            tracing::debug!(target: "embra_trustd", "a debug line of the service");
            tracing::warn!(target: "h2::proto", "another crate's warning");
            tracing::debug!(target: "h2::proto", "another crate's debug line");
        });
        assert!(out.contains("an info line of the service"), "{out}");
        assert!(!out.contains("a debug line of the service"), "{out}");
        assert!(out.contains("another crate's warning"), "{out}");
        assert!(!out.contains("another crate's debug line"), "{out}");
    }

    /// `RUST_LOG` wins when it parses; one that does not falls back to the
    /// default instead of silencing the service.
    #[test]
    fn rust_log_overrides_the_default_and_an_unparseable_one_falls_back() {
        let out = logged_under(Some("embra_trustd=debug"), || {
            tracing::debug!(target: "embra_trustd", "a debug line of the service");
        });
        assert!(out.contains("a debug line of the service"), "{out}");
        let out = logged_under(Some("=not a directive="), || {
            tracing::info!(target: "embra_trustd", "still logged");
        });
        assert!(out.contains("still logged"), "{out}");
    }
}
