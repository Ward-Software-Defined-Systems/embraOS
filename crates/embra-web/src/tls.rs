//! TLS via embra-trustd.
//!
//! `embra-web` asks trustd's `GenerateCertificate` gRPC for a server cert
//! chained to the embraOS CA and serves HTTPS with it. The certificate
//! names this machine: `localhost`, the machine's name, the addresses its
//! cards hold in the private ranges; trustd fills them in. So it is asked
//! for again when those change (a lease that came after boot, a Wi-Fi
//! address, a name the operator set), after the clock was set (a
//! certificate dated on a clock that stood at the build time ends a year
//! from that time), and thirty days before it ends ([`keep_current`]). The
//! certificate is held in memory only. The supervisor starts `embra-web`
//! after trustd, but the first request still retries to cover the warm-up
//! window.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use embra_boot::names::{self, CaSet, LeafNames};
use embra_common::proto::trust::trust_service_client::TrustServiceClient;
use embra_common::proto::trust::{CertRole, GenerateCertRequest, GetCaRequest};
use rustls::ServerConfig;
// PEM is read with rustls' own `pem` module. `rustls-pemfile` did it before;
// it is unmaintained and out of the tree, and does not come back.
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use tonic::transport::Channel;

/// How often the machine's names, the clock and the certificate's end are
/// looked at.
const LOOK_EVERY: Duration = Duration::from_secs(5);
/// How long after a request that failed the next one waits.
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// How long before its end a certificate is asked for again.
const RENEW_BEFORE: i64 = 30 * 86_400;
/// How far the wall clock may move against the steady one between two
/// looks before it counts as set: a slew is far below it, a step that
/// matters to a certificate's dates far above.
const CLOCK_SET_BEYOND: i64 = 60;

/// The certificate in use: every handshake takes the one that is there at
/// its first message, so a new one serves the next connection and the
/// connections that are open keep theirs.
#[derive(Debug)]
pub struct Leaf {
    slot: RwLock<Arc<CertifiedKey>>,
}

impl Leaf {
    fn new(key: CertifiedKey) -> Arc<Leaf> {
        Arc::new(Leaf { slot: RwLock::new(Arc::new(key)) })
    }

    fn current(&self) -> Arc<CertifiedKey> {
        // A poisoned lock means a thread panicked while it swapped a
        // pointer; the pointer that is there is still a certificate.
        self.slot.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn replace(&self, key: CertifiedKey) {
        *self.slot.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(key);
    }
}

impl ResolvesServerCert for Leaf {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current())
    }
}

/// What the last certificate was asked for: the machine's names as they
/// stood, when it ends, and the two clocks' readings at that moment.
#[derive(Clone, Debug)]
pub struct Asked {
    view: LeafNames,
    expires_at: Option<i64>,
    steady: Instant,
    wall: i64,
}

impl Asked {
    fn at(view: LeafNames, expires_at: Option<i64>) -> Asked {
        Asked { view, expires_at, steady: Instant::now(), wall: wall_now() }
    }
}

fn wall_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// The machine's names as embra-web watches them: its name and the private
/// addresses its cards hold. What the certificate carries of them is
/// trustd's to say; a change here is a reason to ask.
fn this_machine() -> LeafNames {
    names::leaf_names(&CaSet::Unconstrained, names::hostname().as_deref(), &names::addresses())
}

/// [`this_machine`], asked on the blocking pool: the list of addresses
/// waits for the kernel's network lock, which a card's starting driver
/// holds for seconds, and a look that stood still on one of the runtime's
/// own threads would hold up the connections that thread serves. `None`
/// when the look could not be made.
async fn look_at_this_machine() -> Option<LeafNames> {
    tokio::task::spawn_blocking(this_machine).await.ok()
}

/// Why a new certificate is asked for now, if it is: the names changed,
/// the clock was set, or the certificate ends within [`RENEW_BEFORE`].
fn due(view: &LeafNames, wall: i64, steady_elapsed: Duration, last: &Asked) -> Option<&'static str> {
    if *view != last.view {
        return Some("the machine's name or addresses changed");
    }
    let expected = last.wall.saturating_add(steady_elapsed.as_secs() as i64);
    if (wall - expected).abs() > CLOCK_SET_BEYOND {
        return Some("the clock was set");
    }
    if last.expires_at.is_some_and(|end| wall >= end - RENEW_BEFORE) {
        return Some("the certificate ends within thirty days");
    }
    None
}

/// Connect to trustd with bounded retry/backoff (same shape as the
/// embra-console connect loop and the apid proxy).
async fn connect_trust(trust_addr: &str) -> anyhow::Result<TrustServiceClient<Channel>> {
    let mut delay = Duration::from_millis(500);
    let mut last_err = None;
    for attempt in 1..=20u32 {
        match Channel::from_shared(trust_addr.to_string())?.connect().await {
            Ok(channel) => {
                tracing::info!(attempt, "connected to embra-trustd");
                return Ok(TrustServiceClient::new(channel));
            }
            Err(e) => {
                tracing::warn!(attempt, error = %e, "trustd not ready; retrying");
                last_err = Some(e);
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(5));
            }
        }
    }
    Err(anyhow::anyhow!(
        "could not reach embra-trustd at {trust_addr} after 20 attempts: {last_err:?}"
    ))
}

/// One connection attempt, for a request while the service runs: a trustd
/// that is away is tried again a minute later, not twenty times now.
async fn connect_once(trust_addr: &str) -> anyhow::Result<TrustServiceClient<Channel>> {
    let channel = Channel::from_shared(trust_addr.to_string())?
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .connect()
        .await
        .with_context(|| format!("embra-trustd at {trust_addr} did not answer"))?;
    Ok(TrustServiceClient::new(channel))
}

/// A certificate for this machine's names. The lists are left empty:
/// trustd fills in the names the machine has and the CA may sign, so the
/// two processes cannot disagree about a name that changed between two
/// readings (a request that lists a name the machine no longer holds is
/// refused, and embra-web is a service the boot cannot do without).
async fn ask(client: &mut TrustServiceClient<Channel>) -> anyhow::Result<(CertifiedKey, Option<i64>)> {
    let req = GenerateCertRequest {
        common_name: "embra-web".to_string(),
        san_dns: Vec::new(),
        san_ip: Vec::new(),
        role: CertRole::Server as i32,
    };
    let resp = client.generate_certificate(req).await.context("trustd GenerateCertificate failed")?.into_inner();
    Ok((certified_key(&resp.cert_pem, &resp.key_pem)?, resp.expires_at.parse().ok()))
}

/// The serving certificate from trustd, the rustls `ServerConfig` that
/// serves whatever certificate is current, and what [`keep_current`] needs
/// to keep it so.
pub async fn acquire(trust_addr: &str) -> anyhow::Result<(ServerConfig, Arc<Leaf>, Asked)> {
    let mut client = connect_trust(trust_addr).await?;
    let view = look_at_this_machine().await.context("the machine's names could not be read")?;
    let (key, expires_at) = ask(&mut client).await?;
    let leaf = Leaf::new(key);
    Ok((server_config(leaf.clone())?, leaf, Asked::at(view, expires_at)))
}

/// Keeps the certificate current for the life of the process: a look every
/// [`LOOK_EVERY`], a request to trustd when [`due`] names a reason, the new
/// certificate swapped in for the next handshake. A request that fails
/// leaves the certificate in use where it is and is made again after
/// [`RETRY_AFTER`]; nothing here ends the process.
pub async fn keep_current(trust_addr: String, leaf: Arc<Leaf>, mut last: Asked) {
    let mut wait_until: Option<Instant> = None;
    loop {
        tokio::time::sleep(LOOK_EVERY).await;
        let Some(view) = look_at_this_machine().await else { continue };
        let Some(why) = due(&view, wall_now(), last.steady.elapsed(), &last) else { continue };
        if wait_until.is_some_and(|t| Instant::now() < t) {
            continue;
        }
        match renew(&trust_addr, &leaf).await {
            Ok(expires_at) => {
                tracing::info!(why, names = %view.listed().join(" "), "a new certificate from embra-trustd serves the next connection");
                last = Asked::at(view, expires_at);
                wait_until = None;
            }
            Err(e) => {
                tracing::warn!(why, error = %format!("{e:#}"), "no new certificate from embra-trustd; the one in use stays");
                wait_until = Some(Instant::now() + RETRY_AFTER);
            }
        }
    }
}

/// One request for a new certificate, swapped in when it came.
async fn renew(trust_addr: &str, leaf: &Leaf) -> anyhow::Result<Option<i64>> {
    let mut client = connect_once(trust_addr).await?;
    let (key, expires_at) = ask(&mut client).await?;
    leaf.replace(key);
    Ok(expires_at)
}

/// The CA certificate as trustd distributes it, for `GET /api/ca.crt`: the
/// file a browser imports once to trust this console.
pub async fn ca_certificate(trust_addr: &str) -> anyhow::Result<Vec<u8>> {
    let mut client = connect_once(trust_addr).await?;
    let resp = client.get_ca_certificate(GetCaRequest {}).await.context("trustd GetCACertificate failed")?.into_inner();
    anyhow::ensure!(!resp.ca_cert_pem.is_empty(), "trustd returned no CA certificate");
    Ok(resp.ca_cert_pem)
}

/// The certificate chain and the private key out of the two PEM blobs
/// trustd returns. Every CERTIFICATE section counts, in order; the key is
/// the first PKCS#8, PKCS#1 or SEC1 section. Sections of any other kind are
/// skipped.
fn parse_chain_and_key(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> anyhow::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<_, _>>()
        .context("parse cert PEM from trustd")?;
    anyhow::ensure!(!certs.is_empty(), "trustd returned an empty cert chain");

    let key = match PrivateKeyDer::from_pem_slice(key_pem) {
        Ok(key) => key,
        Err(rustls::pki_types::pem::Error::NoItemsFound) => {
            anyhow::bail!("trustd returned no private key")
        }
        Err(e) => return Err(e).context("parse key PEM from trustd"),
    };
    Ok((certs, key))
}

/// Explicit provider (aws-lc-rs is the resolved rustls default in this
/// workspace) so we never depend on a process-global being installed.
fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// A certificate and its key as rustls serves them; a key that is not the
/// certificate's is refused here, as `with_single_cert` refused it.
fn certified_key(cert_pem: &[u8], key_pem: &[u8]) -> anyhow::Result<CertifiedKey> {
    let (certs, key) = parse_chain_and_key(cert_pem, key_pem)?;
    let signing = provider().key_provider.load_private_key(key).context("rustls load_private_key")?;
    let certified = CertifiedKey::new(certs, signing);
    certified.keys_match().context("the key is not the certificate's")?;
    Ok(certified)
}

fn server_config(leaf: Arc<Leaf>) -> anyhow::Result<ServerConfig> {
    Ok(ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .context("rustls protocol versions")?
        .with_no_client_auth()
        .with_cert_resolver(leaf))
}

/// A `ServerConfig` over one certificate, for the listener's test.
#[cfg(test)]
pub(crate) fn build_server_config(cert_pem: &[u8], key_pem: &[u8]) -> anyhow::Result<ServerConfig> {
    server_config(Leaf::new(certified_key(cert_pem, key_pem)?))
}

/// A CA and a server certificate it signed, minted the way trustd mints
/// them: `KeyPair::generate()`, `cert.pem()`, `key.serialize_pem()`; the
/// leaf names `localhost`. Returns (leaf cert PEM, CA cert PEM, leaf key
/// PEM). Shared with the listener's test in `server.rs`.
#[cfg(test)]
pub(crate) fn mint_for_tests() -> (String, String, String) {
    use rcgen::{CertificateParams, DnType, IsCa, KeyPair};

    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(DnType::CommonName, "embraOS test CA");
    let ca = ca_params.self_signed(&ca_key).unwrap();

    let leaf_key = KeyPair::generate().unwrap();
    let leaf_params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    let leaf = leaf_params.signed_by(&leaf_key, &ca, &ca_key).unwrap();
    (leaf.pem(), ca.pem(), leaf_key.serialize_pem())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::mint_for_tests as mint;

    #[test]
    fn what_trustd_mints_becomes_a_server_config() {
        let (leaf, _ca, key) = mint();
        let (certs, parsed_key) = parse_chain_and_key(leaf.as_bytes(), key.as_bytes()).unwrap();
        assert_eq!(certs.len(), 1);
        // rcgen serializes PKCS#8, the form trustd hands out.
        assert!(matches!(parsed_key, PrivateKeyDer::Pkcs8(_)));
        build_server_config(leaf.as_bytes(), key.as_bytes()).expect("server config");
    }

    #[test]
    fn a_chain_is_kept_whole_and_in_order() {
        let (leaf, ca, key) = mint();
        let chain = format!("{leaf}{ca}");
        let (certs, _) = parse_chain_and_key(chain.as_bytes(), key.as_bytes()).unwrap();
        assert_eq!(certs.len(), 2);
        let (only_leaf, _) = parse_chain_and_key(leaf.as_bytes(), key.as_bytes()).unwrap();
        let (only_ca, _) = parse_chain_and_key(ca.as_bytes(), key.as_bytes()).unwrap();
        assert_eq!(certs[0], only_leaf[0]);
        assert_eq!(certs[1], only_ca[0]);
    }

    #[test]
    fn sections_of_another_kind_are_skipped() {
        let (leaf, ca, key) = mint();
        // A key blob that leads with a certificate, a cert blob that ends
        // with a key: each side takes what is its own.
        let key_blob = format!("{ca}{key}");
        let cert_blob = format!("{leaf}{key}");
        let (certs, parsed_key) =
            parse_chain_and_key(cert_blob.as_bytes(), key_blob.as_bytes()).unwrap();
        assert_eq!(certs.len(), 1);
        let (_, plain_key) = parse_chain_and_key(leaf.as_bytes(), key.as_bytes()).unwrap();
        assert_eq!(parsed_key.secret_der(), plain_key.secret_der());
    }

    #[test]
    fn a_blob_without_its_section_is_refused() {
        let (leaf, _ca, key) = mint();
        let no_chain = parse_chain_and_key(b"", key.as_bytes()).unwrap_err();
        assert!(format!("{no_chain:#}").contains("empty cert chain"), "{no_chain:#}");
        // A certificate where the key should be: no private key in it.
        let no_key = parse_chain_and_key(leaf.as_bytes(), leaf.as_bytes()).unwrap_err();
        assert!(format!("{no_key:#}").contains("no private key"), "{no_key:#}");
        assert!(parse_chain_and_key(leaf.as_bytes(), b"not pem at all").is_err());
    }

    #[test]
    fn a_damaged_section_is_an_error_not_a_shorter_chain() {
        let (leaf, ca, key) = mint();
        // Break the base64 body of the second certificate.
        let broken_ca = ca.replacen("MII", "M!!", 1);
        assert_ne!(broken_ca, ca);
        let chain = format!("{leaf}{broken_ca}");
        assert!(parse_chain_and_key(chain.as_bytes(), key.as_bytes()).is_err());
    }

    #[test]
    fn a_key_that_does_not_match_the_certificate_is_refused() {
        let (leaf, _ca, _key) = mint();
        let (_other_leaf, _other_ca, other_key) = mint();
        assert!(build_server_config(leaf.as_bytes(), other_key.as_bytes()).is_err());
        assert!(certified_key(leaf.as_bytes(), other_key.as_bytes()).is_err());
    }

    /// One TLS handshake in memory against `config`, by a client that
    /// trusts `ca_pem`; the certificate the server showed.
    fn shown_to_a_client(config: &Arc<ServerConfig>, ca_pem: &str) -> CertificateDer<'static> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from_pem_slice(ca_pem.as_bytes()).unwrap()).unwrap();
        let client_config = rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let mut client = rustls::ClientConnection::new(Arc::new(client_config), "localhost".try_into().unwrap()).unwrap();
        let mut server = rustls::ServerConnection::new(config.clone()).unwrap();
        for _ in 0..20 {
            let mut wire = Vec::new();
            while client.wants_write() {
                client.write_tls(&mut wire).unwrap();
            }
            if !wire.is_empty() {
                server.read_tls(&mut wire.as_slice()).unwrap();
                server.process_new_packets().unwrap();
            }
            let mut wire = Vec::new();
            while server.wants_write() {
                server.write_tls(&mut wire).unwrap();
            }
            if !wire.is_empty() {
                client.read_tls(&mut wire.as_slice()).unwrap();
                client.process_new_packets().unwrap();
            }
            if !client.is_handshaking() && !server.is_handshaking() {
                break;
            }
        }
        assert!(!client.is_handshaking(), "the handshake finished");
        client.peer_certificates().unwrap()[0].clone().into_owned()
    }

    #[test]
    fn a_new_leaf_is_served_to_the_next_handshake() {
        let (first_pem, first_ca, first_key) = mint();
        let (second_pem, second_ca, second_key) = mint();
        let leaf = Leaf::new(certified_key(first_pem.as_bytes(), first_key.as_bytes()).unwrap());
        let config = Arc::new(server_config(leaf.clone()).unwrap());
        let der = |pem: &str| CertificateDer::from_pem_slice(pem.as_bytes()).unwrap();

        assert_eq!(shown_to_a_client(&config, &first_ca), der(&first_pem));
        // The same listener, the same configuration: the next connection
        // is shown the certificate that was swapped in.
        leaf.replace(certified_key(second_pem.as_bytes(), second_key.as_bytes()).unwrap());
        assert_eq!(shown_to_a_client(&config, &second_ca), der(&second_pem));
    }

    #[tokio::test]
    async fn a_failed_request_keeps_the_old_leaf() {
        let (pem, ca, key) = mint();
        let leaf = Leaf::new(certified_key(pem.as_bytes(), key.as_bytes()).unwrap());
        let config = Arc::new(server_config(leaf.clone()).unwrap());
        // Nothing listens there: the request fails, and at once.
        let started = Instant::now();
        let err = renew("http://127.0.0.1:1", &leaf).await.expect_err("no trustd");
        assert!(format!("{err:#}").contains("did not answer"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(shown_to_a_client(&config, &ca), CertificateDer::from_pem_slice(pem.as_bytes()).unwrap());
        assert!(ca_certificate("http://127.0.0.1:1").await.is_err());
    }

    #[test]
    fn a_certificate_is_asked_for_again_when_the_names_change_the_clock_is_set_or_its_end_is_near() {
        let view = |name: &str, addresses: &[&str]| {
            names::leaf_names(&CaSet::Unconstrained, Some(name), &addresses.iter().map(|a| a.parse().unwrap()).collect::<Vec<_>>())
        };
        let wall = 1_800_000_000;
        let year = 365 * 86_400;
        let last = Asked { view: view("embraos", &["10.0.2.15"]), expires_at: Some(wall + year), steady: Instant::now(), wall };
        let minutes = |n: u64| Duration::from_secs(n * 60);

        // Nothing changed: an hour on, the two clocks in step.
        assert_eq!(due(&last.view, wall + 3600, minutes(60), &last), None);
        // A slew or a few seconds of drift is not the clock being set.
        assert_eq!(due(&last.view, wall + 3600 + 45, minutes(60), &last), None);
        // An address that came (Wi-Fi after boot), one that went, a name the operator set.
        assert_eq!(due(&view("embraos", &["10.0.2.15", "192.168.1.42"]), wall + 60, minutes(1), &last), Some("the machine's name or addresses changed"));
        assert_eq!(due(&view("embraos", &[]), wall + 60, minutes(1), &last), Some("the machine's name or addresses changed"));
        assert_eq!(due(&view("axiom", &["10.0.2.15"]), wall + 60, minutes(1), &last), Some("the machine's name or addresses changed"));
        // A public address is not on any certificate: its coming is no reason.
        assert_eq!(due(&view("embraos", &["10.0.2.15", "203.0.113.7"]), wall + 60, minutes(1), &last), None);
        // The clock stepped forwards by years (a board without a battery
        // that met a time server), or back.
        assert_eq!(due(&last.view, wall + 3 * year, minutes(1), &last), Some("the clock was set"));
        assert_eq!(due(&last.view, wall - 7200, minutes(1), &last), Some("the clock was set"));
        // Thirty days before the end, with the clocks in step.
        let near = Duration::from_secs((year - RENEW_BEFORE) as u64);
        assert_eq!(due(&last.view, wall + year - RENEW_BEFORE, near, &last), Some("the certificate ends within thirty days"));
        assert_eq!(due(&last.view, wall + year - RENEW_BEFORE - 120, near - Duration::from_secs(120), &last), None);
        // A trustd that names no end is never asked on account of it.
        let endless = Asked { expires_at: None, ..last.clone() };
        assert_eq!(due(&endless.view, wall + 2 * year, Duration::from_secs(2 * year as u64), &endless), None);
    }
}
