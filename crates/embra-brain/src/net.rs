//! The reqwest clients of the brain, built one way.
//!
//! Every client the brain talks to a provider or a git host with follows a
//! redirect to the same origin only ([`same_origin_redirect_policy`]).
//! reqwest's default follows up to ten hops and strips `Authorization`,
//! `Cookie` and `Proxy-Authorization` on a cross-host hop — and nothing
//! else, so `PRIVATE-TOKEN`, `x-api-key` and `x-goog-api-key` would travel
//! to a third host. A same-origin redirect (an endpoint behind a proxy)
//! still works; a cross-origin one comes back as the 3xx it is, and the
//! caller reports it. The database client follows no redirect at all: a
//! loopback API that redirects is a fault.
//!
//! Every client built here also trusts the certificate authorities the
//! image was built with and the operator's CA drop-ins beside the
//! compiled-in webpki roots ([`operator_roots`]:
//! `/usr/share/embra/ca-certificates/` and `/embra/state/ca-certificates/`,
//! or `EMBRA_CA_DIR` alone when set, read once per process), so a host
//! behind the operator's own certificate authority — a self-hosted GitLab,
//! the provider gateway over HTTPS — verifies. The database client is built
//! apart: loopback, plain HTTP, no roots.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use reqwest::redirect::{Attempt, Policy};

/// The most same-origin hops a request follows, reqwest's own default.
const MAX_REDIRECTS: usize = 10;

/// A redirect is followed when its target has the scheme, host and port of
/// the URL that redirected, and the chain is under [`MAX_REDIRECTS`].
pub(crate) fn same_origin_redirect_policy() -> Policy {
    Policy::custom(|attempt: Attempt<'_>| {
        let from = attempt.previous().last();
        let to = attempt.url();
        let same_origin = from.is_some_and(|f| {
            f.scheme() == to.scheme()
                && f.host_str() == to.host_str()
                && f.port_or_known_default() == to.port_or_known_default()
        });
        if same_origin && attempt.previous().len() <= MAX_REDIRECTS {
            attempt.follow()
        } else {
            attempt.stop()
        }
    })
}

/// TCP + TLS establishment bound for every client built here.
pub(crate) const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Per-read idle bound for every client built here (#26). It resets on
/// every byte, so a response that keeps flowing never trips it; it only
/// unsticks a connection that has gone dead. Long on purpose: a local
/// model's prompt processing on a large context is silent for minutes
/// before the first token, so this is the database client's figure, set
/// for a measured 5–8 min worst case. Never a total `.timeout()`: a turn
/// legitimately runs many minutes.
pub(crate) const READ_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// The operator's CA drop-in directory on STATE, seeded by `seed-state.sh
/// --ca-dir`. Kept in sync with `crates/embrad/src/ca_bundle.rs`, which
/// merges the same files into the bundle git reads.
pub(crate) const OPERATOR_CA_DIR: &str = "/embra/state/ca-certificates";
/// The certificate authorities the image was built with: the `*.pem` and
/// `*.crt` files of `Imported_CAs/` at build time, baked read-only into the
/// root filesystem (`scripts/lib/imported-cas.sh`), the same on every
/// machine of the build. A machine installed from the ISO, a hypervisor's
/// disk or bare metal has no step that fills STATE, so a CA it needs
/// travels in the image. Kept in sync with the build's
/// `IMPORTED_CAS_ROOTFS_DIR` and embrad's `CA_IMAGE_DIR`; guard
/// `the_image_s_ca_directory_is_the_one_the_build_bakes_and_embrad_merges`.
pub(crate) const IMAGE_CA_DIR: &str = "/usr/share/embra/ca-certificates";
/// Replaces [`IMAGE_CA_DIR`] and [`OPERATOR_CA_DIR`] when set and non-empty
/// (development).
pub(crate) const OPERATOR_CA_DIR_ENV: &str = "EMBRA_CA_DIR";

/// The directories in force, in reading order: the environment's alone
/// when it names one, else the image's and then the STATE one.
fn ca_dirs(dev_override: Option<String>) -> Vec<PathBuf> {
    match dev_override.filter(|d| !d.is_empty()) {
        Some(dir) => vec![PathBuf::from(dir)],
        None => vec![PathBuf::from(IMAGE_CA_DIR), PathBuf::from(OPERATOR_CA_DIR)],
    }
}

/// The certificate authorities of the image and the operator's CA
/// drop-ins, read once per process at the first client built, and shared by
/// every client of the brain and by the Guardian transport. The image's
/// directory is read-only and STATE's is seeded while the OS is stopped, so
/// a read at every client build would see the same files; a seeded
/// certificate takes effect at the next boot.
pub(crate) fn operator_roots() -> &'static [reqwest::Certificate] {
    static ROOTS: OnceLock<Vec<reqwest::Certificate>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        let mut roots = Vec::new();
        for dir in ca_dirs(std::env::var(OPERATOR_CA_DIR_ENV).ok()) {
            let found = operator_roots_in(&dir);
            if !found.is_empty() {
                // The operator's line reads as it always has; the image's
                // has a line of its own, so the journal says which trust
                // came with the build.
                let whose = if dir == Path::new(IMAGE_CA_DIR) {
                    "image CA certificates"
                } else {
                    "operator CA drop-ins"
                };
                tracing::info!("{whose}: {} certificate(s) from {}", found.len(), dir.display());
            }
            roots.extend(found);
        }
        roots
    })
}

/// The certificates of the `*.pem` and `*.crt` files in `dir`. A missing
/// directory is no drop-in. A file that does not parse, and a certificate
/// the root store refuses (PEM framing around something that is not a
/// certificate), are skipped with a warning and never fail a client's
/// build: `builder()` feeds every client of the brain, and a trust anchor
/// that is truly missing shows as the TLS error of the request itself.
fn operator_roots_in(dir: &Path) -> Vec<reqwest::Certificate> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut roots = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_cert_ext = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("pem") | Some("crt")
        );
        if !path.is_file() || !is_cert_ext {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let parsed = match reqwest::Certificate::from_pem_bundle(&bytes) {
            Ok(parsed) => parsed,
            Err(e) => {
                tracing::warn!("operator CA file skipped: {}: {e}", path.display());
                continue;
            }
        };
        for cert in parsed {
            // `from_pem_bundle` checks the PEM framing; the root store
            // checks the certificate when a client is built. A throwaway
            // client finds that out here, so no `build().expect(..)` of
            // the brain is reachable through a bad drop-in.
            match reqwest::Client::builder().add_root_certificate(cert.clone()).build() {
                Ok(_) => roots.push(cert),
                Err(e) => tracing::warn!(
                    "operator CA certificate skipped: {}: {e}",
                    path.display()
                ),
            }
        }
    }
    roots
}

/// A client builder with the redirect policy and the operator's roots set;
/// the caller adds its timeouts (`bounded()` for the two standard ones).
/// Guard `every_net_builder_sets_a_timeout`.
pub(crate) fn builder() -> reqwest::ClientBuilder {
    builder_with(operator_roots())
}

/// `builder()` on the roots given; `builder()` passes the drop-ins.
fn builder_with(roots: &[reqwest::Certificate]) -> reqwest::ClientBuilder {
    let mut builder = reqwest::Client::builder().redirect(same_origin_redirect_policy());
    for cert in roots {
        builder = builder.add_root_certificate(cert.clone());
    }
    builder
}

/// `builder()` with the two standard bounds.
pub(crate) fn bounded() -> reqwest::ClientBuilder {
    builder().connect_timeout(CONNECT_TIMEOUT).read_timeout(READ_IDLE_TIMEOUT)
}

/// A client with the redirect policy, the roots and the two bounds (#26).
/// The chat, GitHub and GitLab clients take `shared`, one per process; this
/// builds a fresh one for a caller that needs its own. The roots are read
/// once per process either way (`operator_roots`), so a CA drop-in seeded
/// while the brain runs takes effect at the next boot.
pub(crate) fn client() -> reqwest::Client {
    bounded()
        .build()
        .expect("a reqwest client with a redirect policy and the two bounds builds")
}

/// The brain's long-lived `client()`, built on first use (#36). `reqwest`'s
/// connection pool lives inside the client: a client built per turn threw
/// every warm connection away, and the first call of the next turn paid a
/// new TCP and TLS handshake. The key is a per-request header on every
/// provider and the bounds are fixed, so one client serves Gemini and its
/// cache manager, the OpenAI-compatible presets and the GitHub tools. The
/// Anthropic client keeps its own pair of bounds and its own instance
/// (`provider::anthropic::shared_client`). The roots are read once per
/// process either way.
pub(crate) fn shared() -> reqwest::Client {
    static SHARED: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    SHARED
        .get_or_init(|| {
            #[cfg(test)]
            SHARED_BUILDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            client()
        })
        .clone()
}

/// How many times `shared` built its client: at most once per process.
#[cfg(test)]
pub(crate) static SHARED_BUILDS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// What a request error says, its causes joined. reqwest's own line names
/// the kind and the URL and keeps the cause behind `source()` ("client
/// error (Connect)", then "invalid peer certificate: UnknownIssuer"), and
/// the operator needs the cause: the causes without the top line when there
/// are any, the top line otherwise. A cause already quoted by the one
/// before it is not repeated.
pub(crate) fn error_chain(e: &reqwest::Error) -> String {
    use std::error::Error as _;
    let mut parts: Vec<String> = Vec::new();
    let mut cause = e.source();
    while let Some(c) = cause {
        let text = c.to_string();
        if !parts.last().is_some_and(|prev| prev.contains(&text)) {
            parts.push(text);
        }
        cause = c.source();
    }
    if parts.is_empty() {
        e.to_string()
    } else {
        parts.join(": ")
    }
}

/// A certificate refused on its dates, as rustls reports it: "certificate
/// not valid yet: verification time N (UNIX), but certificate is not valid
/// before M (…)" and "certificate expired: verification time N (UNIX), but
/// certificate is not valid after M (…)", or the bare `NotValidYet` and
/// `Expired`. `at` is the clock the check ran against and `bound` the
/// certificate's first or last second, when the text carries them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DateRefusal {
    NotYet { at: Option<i64>, bound: Option<i64> },
    Expired { at: Option<i64>, bound: Option<i64> },
}

/// Reads a request error's cause (`error_chain`) for a date refusal. The
/// date check comes before the issuer check, so this is the whole reason:
/// such a cause says nothing about whether the certificate's CA is trusted.
pub(crate) fn date_refusal(cause: &str) -> Option<DateRefusal> {
    let lower = cause.to_ascii_lowercase();
    let number_after = |marker: &str| -> Option<i64> {
        let rest = &lower[lower.find(marker)? + marker.len()..];
        let digits = &rest[..rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len())];
        digits.parse().ok()
    };
    // rustls prints a variant without its times by name; a longer name that
    // starts the same (`ExpiredRevocationList`) is another error.
    let bare = |name: &str| lower.match_indices(name).any(|(i, m)| !lower[i + m.len()..].starts_with(|c: char| c.is_ascii_alphanumeric()));
    let at = number_after("verification time ");
    if lower.contains("certificate not valid yet") || bare("certificate: notvalidyet") {
        Some(DateRefusal::NotYet { at, bound: number_after("not valid before ") })
    } else if lower.contains("certificate expired") || bare("certificate: expired") {
        Some(DateRefusal::Expired { at, bound: number_after("not valid after ") })
    } else {
        None
    }
}

/// What a date refusal means on this machine and what fixes it, for the
/// line an operator reads; `None` for any other cause. It reads what embrad
/// made of the clock at boot (`/run/embra/clock-status`).
pub(crate) fn certificate_date_hint(cause: &str) -> Option<String> {
    let refusal = date_refusal(cause)?;
    Some(date_hint(refusal, embra_boot::ClockStatus::read().as_ref(), chrono::Utc::now().timestamp()))
}

/// The hint itself. Which clock is wrong depends on what is known of this
/// machine's: a time server set it (the certificate's dates are the
/// fault), it is the image's build time (certainly behind: a certificate
/// that begins later may well be in date, one that ended earlier has
/// expired), or nothing confirmed it (either).
pub(crate) fn date_hint(refusal: DateRefusal, clock: Option<&embra_boot::ClockStatus>, now: i64) -> String {
    use embra_boot::clock::{format_utc, Sync};
    const FIX: &str = "Set the date in the firmware setup (in UTC), or boot on a network where a time server answers (embra.ntp=<host> names one).";
    let (at, bound) = match refusal {
        DateRefusal::NotYet { at, bound } | DateRefusal::Expired { at, bound } => (format_utc(at.unwrap_or(now)), bound.map(format_utc)),
    };
    let set_from = clock.and_then(|c| match &c.sync {
        Sync::Synchronized { server, .. } => Some(server.as_str()),
        _ => None,
    });
    let guess = clock.is_some_and(|c| c.is_a_guess());
    // Why nothing confirmed the clock, as a sentence of its own.
    let unconfirmed = match clock {
        Some(c) => {
            let asked = match &c.sync {
                Sync::Failed { server, reason } => format!("no time came from {server} ({reason})"),
                Sync::Off => "embra.ntp=off asks no time server".to_string(),
                Sync::Synchronized { .. } => String::new(),
            };
            match c.floored_from {
                Some(was) => format!(" The hardware clock read {} at boot, earlier than this image was built, and {asked}.", format_utc(was)),
                None => format!(" At boot {asked}."),
            }
        }
        None => String::new(),
    };
    match (refusal, set_from) {
        (DateRefusal::NotYet { .. }, Some(server)) => {
            let begins = bound.map(|b| format!("begins {b}, after this machine's clock ({at})")).unwrap_or_else(|| format!("is not valid yet by this machine's clock ({at})"));
            format!(" The certificate {begins}, and that clock was set from {server} at boot: the certificate is dated in the future. Check the clock of the machine that issued it.")
        }
        (DateRefusal::NotYet { .. }, None) => {
            let begins = bound.map(|b| format!(" and the certificate begins {b}")).unwrap_or_default();
            let verdict = if guess { "the clock is known to be behind, so the refusal says nothing against the certificate" } else { "either the clock is behind or the certificate is dated in the future, and nothing confirmed the clock" };
            format!(" This machine's clock reads {at}{begins}: {verdict}.{unconfirmed} {FIX}")
        }
        (DateRefusal::Expired { .. }, Some(server)) => {
            let ended = bound.map(|b| format!("ended {b}, before this machine's clock ({at})")).unwrap_or_else(|| format!("has ended by this machine's clock ({at})"));
            format!(" The certificate {ended}, and that clock was set from {server} at boot: the certificate has expired, and the listener needs a new one.")
        }
        (DateRefusal::Expired { .. }, None) if guess => {
            let ended = bound.map(|b| format!("ended {b}")).unwrap_or_else(|| "has ended".to_string());
            format!(" The certificate {ended}, and this machine's clock ({at}) is behind if anything: the certificate has expired, and the listener needs a new one.{unconfirmed}")
        }
        (DateRefusal::Expired { .. }, None) => {
            let ended = bound.map(|b| format!(" and the certificate ended {b}")).unwrap_or_default();
            format!(" This machine's clock reads {at}{ended}: either the certificate has expired or the clock is ahead, and nothing confirmed the clock.{unconfirmed} {FIX}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #36: the shared client is one per process, however many turns and
    /// tools ask for it.
    #[test]
    fn a_shared_client_is_built_once() {
        for _ in 0..3 {
            let _ = shared();
        }
        assert!(SHARED_BUILDS.load(std::sync::atomic::Ordering::SeqCst) <= 1);
    }
    use std::sync::Arc;

    use rcgen::{
        BasicConstraints, CertificateParams, DnType, DnValue, ExtendedKeyUsagePurpose, IsCa,
        KeyPair, KeyUsagePurpose,
    };
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A CA and a leaf for `127.0.0.1` under it: the CA's PEM (what an
    /// operator drops in), the leaf's DER and its PKCS#8 key (what a server
    /// presents).
    fn mint_ca_and_leaf() -> (String, rustls::pki_types::CertificateDer<'static>, Vec<u8>) {
        mint_ca_and_leaf_dated(None)
    }

    /// The same, the leaf valid from the first of January of one year to
    /// the first of January of another when `years` names them.
    fn mint_ca_and_leaf_dated(years: Option<(i32, i32)>) -> (String, rustls::pki_types::CertificateDer<'static>, Vec<u8>) {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        ca_params
            .distinguished_name
            .push(DnType::CommonName, DnValue::Utf8String("test root".to_string()));
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let leaf_key = KeyPair::generate().unwrap();
        let mut leaf_params = CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap();
        leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        if let Some((from, to)) = years {
            leaf_params.not_before = rcgen::date_time_ymd(from, 1, 1);
            leaf_params.not_after = rcgen::date_time_ymd(to, 1, 1);
        }
        let leaf = leaf_params.signed_by(&leaf_key, &ca, &ca_key).unwrap();
        (ca.pem(), leaf.der().clone(), leaf_key.serialize_der())
    }

    fn fresh_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("brain-ca-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A drop-in directory with a real CA, a file that is not PEM, PEM
    /// framing around bytes that are not a certificate, and a file of
    /// another extension: the CA alone is a root. A directory that does not
    /// exist is no root at all.
    #[test]
    fn operator_roots_skip_what_is_not_a_certificate_and_a_missing_dir() {
        let (ca_pem, _, _) = mint_ca_and_leaf();
        let dir = fresh_dir("skip");
        std::fs::write(dir.join("gateway-ca.pem"), &ca_pem).unwrap();
        std::fs::write(dir.join("garbage.pem"), "not a certificate").unwrap();
        // base64 of "not a certificate": the PEM parses, the DER does not.
        std::fs::write(
            dir.join("framed.crt"),
            "-----BEGIN CERTIFICATE-----\nbm90IGEgY2VydGlmaWNhdGU=\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        std::fs::write(dir.join("ignored.txt"), &ca_pem).unwrap();
        let roots = operator_roots_in(&dir);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(roots.len(), 1, "the one real certificate, nothing else");
        assert!(operator_roots_in(&dir).is_empty(), "a missing directory is no drop-in");
        assert!(builder_with(&roots).timeout(CONNECT_TIMEOUT).build().is_ok());
    }

    /// The point of the drop-ins: a TLS server under a CA the operator
    /// dropped in is reached by a client built here, and a client without
    /// that root refuses the same server with a certificate error.
    #[tokio::test]
    async fn a_client_built_here_trusts_an_operator_root_and_refuses_an_unknown_issuer() {
        let (ca_pem, leaf_der, leaf_key) = mint_ca_and_leaf();
        let dir = fresh_dir("trust");
        std::fs::write(dir.join("gateway-ca.pem"), &ca_pem).unwrap();
        let roots = operator_roots_in(&dir);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(roots.len(), 1);

        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![leaf_der], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key)))
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut served = 0usize;
            for _ in 0..2 {
                let (tcp, _) = listener.accept().await.unwrap();
                // The untrusting client ends the handshake with an alert;
                // that connection serves nothing.
                let Ok(mut tls) = acceptor.accept(tcp).await else { continue };
                let mut head = vec![0u8; 4096];
                let _ = tls.read(&mut head).await;
                tls.write_all(
                    b"HTTP/1.1 200 OK\r\nx-local-ai-gateway: 0.1.0\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                )
                .await
                .unwrap();
                tls.shutdown().await.ok();
                served += 1;
            }
            served
        });

        let trusting = builder_with(&roots).timeout(READ_IDLE_TIMEOUT).build().unwrap();
        let r = trusting.get(format!("https://{addr}/v1/models")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["x-local-ai-gateway"], "0.1.0");

        let untrusting = builder_with(&[]).timeout(READ_IDLE_TIMEOUT).build().unwrap();
        let err = untrusting.get(format!("https://{addr}/v1/models")).send().await.unwrap_err();
        let cause = error_chain(&err);
        assert!(cause.to_ascii_lowercase().contains("certificate"), "{cause}");
        assert!(!cause.contains("https://"), "the causes, not reqwest's URL line: {cause}");

        assert_eq!(server.await.unwrap(), 1, "one connection served, one handshake refused");
    }

    /// A TLS listener on the loopback presenting `leaf`, for `connections`
    /// handshakes, each of which the client ends.
    async fn refused_tls_listener(leaf: rustls::pki_types::CertificateDer<'static>, key: Vec<u8>, connections: usize) -> std::net::SocketAddr {
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![leaf], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)))
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..connections {
                let Ok((tcp, _)) = listener.accept().await else { return };
                let _ = acceptor.accept(tcp).await;
            }
        });
        addr
    }

    /// The words are rustls's own: the errors are built here and printed by
    /// it, so a release that rewords them fails this test and not an
    /// operator's hint. The first is the line of the bare-metal run of
    /// 2026-11-11 (a board that woke in 2018, a certificate from 2026-11-04).
    #[test]
    fn a_date_refusal_is_read_from_rustls_own_words() {
        use rustls::pki_types::UnixTime;
        use rustls::{CertificateError, Error};
        let at = |secs: u64| UnixTime::since_unix_epoch(std::time::Duration::from_secs(secs));
        let chain = |e: CertificateError| format!("client error (Connect): {}", Error::InvalidCertificate(e));

        let not_yet = chain(CertificateError::NotValidYetContext { time: at(1_514_765_221), not_before: at(1_793_750_400) });
        assert_eq!(
            not_yet,
            "client error (Connect): invalid peer certificate: certificate not valid yet: verification time 1514765221 (UNIX), but certificate is not valid before 1793750400 (278985179 seconds in future)"
        );
        assert_eq!(date_refusal(&not_yet), Some(DateRefusal::NotYet { at: Some(1_514_765_221), bound: Some(1_793_750_400) }));
        let expired = chain(CertificateError::ExpiredContext { time: at(1_793_750_400), not_after: at(1_514_765_221) });
        assert_eq!(date_refusal(&expired), Some(DateRefusal::Expired { at: Some(1_793_750_400), bound: Some(1_514_765_221) }));
        // Without the times rustls prints the variant's name.
        assert_eq!(date_refusal(&chain(CertificateError::NotValidYet)), Some(DateRefusal::NotYet { at: None, bound: None }));
        assert_eq!(date_refusal(&chain(CertificateError::Expired)), Some(DateRefusal::Expired { at: None, bound: None }));
        // Everything else is some other refusal: the issuer, the name, a revocation list's own date.
        for other in [
            CertificateError::UnknownIssuer,
            CertificateError::BadSignature,
            CertificateError::NotValidForName,
            CertificateError::ExpiredRevocationList,
            CertificateError::ExpiredRevocationListContext { time: at(1_793_750_400), next_update: at(1_514_765_221) },
        ] {
            let cause = chain(other);
            assert_eq!(date_refusal(&cause), None, "{cause}");
            assert_eq!(certificate_date_hint(&cause), None, "{cause}");
        }
        assert_eq!(date_refusal("error sending request: connection refused"), None);
    }

    fn clock(floored_from: Option<i64>, sync: embra_boot::clock::Sync) -> embra_boot::ClockStatus {
        embra_boot::ClockStatus { floored_from, built: Some(1_794_452_322), sync, rtc: embra_boot::clock::Rtc::Unchanged }
    }

    /// Which clock is wrong depends on what is known of this machine's.
    #[test]
    fn the_date_hint_names_the_clock_or_the_certificate_by_what_the_boot_made_of_the_clock() {
        use embra_boot::clock::Sync;
        const FIX: &str = " Set the date in the firmware setup (in UTC), or boot on a network where a time server answers (embra.ntp=<host> names one).";
        let not_yet = DateRefusal::NotYet { at: Some(1_794_452_922), bound: Some(1_794_500_000) };
        let expired = DateRefusal::Expired { at: Some(1_794_452_922), bound: Some(1_514_765_221) };
        let synced = clock(None, Sync::Synchronized { server: "time.cloudflare.com".into(), address: "162.159.200.1:123".into(), offset: 0.01, stratum: 3 });
        let no_answer = Sync::Failed { server: "time.cloudflare.com".into(), reason: "no answer within 5 s".into() };
        let guess = clock(Some(1_514_764_803), no_answer.clone());
        let unconfirmed = clock(None, no_answer);
        let off = clock(None, Sync::Off);

        // The bare-metal case: the board woke in 2018, the clock is the build time, the certificate is newer than the build.
        assert_eq!(
            date_hint(not_yet, Some(&guess), 0),
            format!(" This machine's clock reads 2026-11-12 03:08:42 UTC and the certificate begins 2026-11-12 16:13:20 UTC: the clock is known to be behind, so the refusal says nothing against the certificate. The hardware clock read 2018-01-01 00:00:03 UTC at boot, earlier than this image was built, and no time came from time.cloudflare.com (no answer within 5 s).{FIX}")
        );
        // A time server set the clock: the certificate's own dates are the fault, and no clock advice follows.
        assert_eq!(
            date_hint(not_yet, Some(&synced), 0),
            " The certificate begins 2026-11-12 16:13:20 UTC, after this machine's clock (2026-11-12 03:08:42 UTC), and that clock was set from time.cloudflare.com at boot: the certificate is dated in the future. Check the clock of the machine that issued it."
        );
        assert_eq!(
            date_hint(expired, Some(&synced), 0),
            " The certificate ended 2018-01-01 00:07:01 UTC, before this machine's clock (2026-11-12 03:08:42 UTC), and that clock was set from time.cloudflare.com at boot: the certificate has expired, and the listener needs a new one."
        );
        // A clock that can only be behind does not excuse a certificate that has ended.
        let h = date_hint(expired, Some(&guess), 0);
        assert!(h.starts_with(" The certificate ended 2018-01-01 00:07:01 UTC, and this machine's clock (2026-11-12 03:08:42 UTC) is behind if anything: the certificate has expired"), "{h}");
        assert!(!h.contains("firmware setup"), "{h}");
        // Nothing confirmed the clock and nothing called it into question: either.
        let h = date_hint(not_yet, Some(&unconfirmed), 0);
        assert!(h.contains(": either the clock is behind or the certificate is dated in the future, and nothing confirmed the clock. At boot no time came from time.cloudflare.com (no answer within 5 s)."), "{h}");
        assert!(h.ends_with(FIX), "{h}");
        let h = date_hint(expired, Some(&off), 0);
        assert!(h.contains(": either the certificate has expired or the clock is ahead, and nothing confirmed the clock. At boot embra.ntp=off asks no time server."), "{h}");
        // No record (a development host): the same verdict without a reason, and the clock read now when rustls gave no time.
        assert_eq!(
            date_hint(DateRefusal::NotYet { at: None, bound: None }, None, 1_794_452_922),
            format!(" This machine's clock reads 2026-11-12 03:08:42 UTC: either the clock is behind or the certificate is dated in the future, and nothing confirmed the clock.{FIX}")
        );
        for hint in [date_hint(not_yet, Some(&guess), 0), date_hint(expired, Some(&unconfirmed), 0), date_hint(not_yet, None, 0)] {
            assert!(!hint.contains("ca-certificates") && !hint.contains("Imported_CAs"), "a date refusal says nothing about the CA: {hint}");
        }
    }

    /// End to end: a listener whose certificate begins in 2090 is refused on
    /// its date, by a client that does not trust its CA either — the date
    /// check comes first — and the gateway's identification names the clock,
    /// never the CA directory. An expired one likewise.
    #[tokio::test]
    async fn a_listener_refused_on_its_certificates_dates_is_told_about_the_clock_and_not_the_ca() {
        let (_, leaf, key) = mint_ca_and_leaf_dated(Some((2090, 2091)));
        let addr = refused_tls_listener(leaf, key, 1).await;
        let err = crate::provider::gateway::identify(&format!("https://{addr}")).await.unwrap_err();
        assert!(err.contains("certificate not valid yet: verification time "), "{err}");
        assert!(err.contains(" and the certificate begins 2090-01-01 00:00:00 UTC") || err.contains("The certificate begins 2090-01-01 00:00:00 UTC"), "{err}");
        assert!(!err.contains("ca-certificates") && !err.contains("not trusted"), "{err}");

        let (_, leaf, key) = mint_ca_and_leaf_dated(Some((2001, 2002)));
        let addr = refused_tls_listener(leaf, key, 1).await;
        let err = crate::provider::gateway::identify(&format!("https://{addr}")).await.unwrap_err();
        assert!(err.contains("certificate expired: verification time "), "{err}");
        assert!(err.contains("ended 2002-01-01 00:00:00 UTC"), "{err}");
        assert!(!err.contains("ca-certificates") && !err.contains("not trusted"), "{err}");

        // A certificate in date under a CA nobody dropped in: the CA directory, as before.
        let (_, leaf, key) = mint_ca_and_leaf();
        let addr = refused_tls_listener(leaf, key, 1).await;
        let err = crate::provider::gateway::identify(&format!("https://{addr}")).await.unwrap_err();
        assert!(err.contains("/embra/state/ca-certificates/") && err.contains("is not trusted"), "{err}");
        assert!(!err.contains("firmware setup"), "{err}");
    }

    /// The image's directory and then STATE's, and the development override
    /// alone when it names one. A build's certificate and the operator's are
    /// both roots: an installed machine with nothing on STATE trusts the
    /// image's, and an image built with none trusts the operator's.
    #[test]
    fn the_image_s_directory_is_read_before_state_s_and_the_override_replaces_both() {
        let both = vec![PathBuf::from(IMAGE_CA_DIR), PathBuf::from(OPERATOR_CA_DIR)];
        assert_eq!(ca_dirs(None), both);
        assert_eq!(ca_dirs(Some(String::new())), both, "an empty override is no override");
        assert_eq!(ca_dirs(Some("/tmp/dev-ca".into())), vec![PathBuf::from("/tmp/dev-ca")]);

        let (image_ca, _, _) = mint_ca_and_leaf();
        let (state_ca, _, _) = mint_ca_and_leaf();
        let image = fresh_dir("image");
        let state = fresh_dir("state");
        std::fs::write(image.join("build.pem"), &image_ca).unwrap();
        std::fs::write(state.join("operator.crt"), &state_ca).unwrap();
        let absent = image.join("absent");
        let count = |dirs: &[&Path]| dirs.iter().map(|d| operator_roots_in(d).len()).sum::<usize>();
        assert_eq!(count(&[&image, &state]), 2, "the image's and the operator's");
        assert_eq!(count(&[&image, &absent]), 1, "nothing on STATE: the image's alone");
        assert_eq!(count(&[&absent, &state]), 1, "an image built with none: the operator's alone");
        std::fs::remove_dir_all(&image).ok();
        std::fs::remove_dir_all(&state).ok();
    }

    /// One directory in three places: the build bakes it
    /// (`scripts/lib/imported-cas.sh`), embrad merges it into the bundle git
    /// and the updater read (`crates/embrad/src/ca_bundle.rs`), and the
    /// clients built here trust it. A drift would leave a baked certificate
    /// that one of them never reads; the same holds for STATE's directory
    /// and for the size above which embrad skips a file, which the build
    /// refuses to bake.
    #[test]
    fn the_image_s_ca_directory_is_the_one_the_build_bakes_and_embrad_merges() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let lib = std::fs::read_to_string(root.join("scripts/lib/imported-cas.sh")).unwrap();
        let shell_value = |name: &str| -> String {
            lib.lines()
                .find_map(|l| l.strip_prefix(&format!("{name}=")))
                .unwrap_or_else(|| panic!("{name} is not assigned in imported-cas.sh"))
                .trim_matches('"')
                .to_string()
        };
        assert_eq!(format!("/{}", shell_value("IMPORTED_CAS_ROOTFS_DIR")), IMAGE_CA_DIR);

        let embrad = std::fs::read_to_string(root.join("crates/embrad/src/ca_bundle.rs")).unwrap();
        assert!(
            embrad.contains(&format!("const CA_IMAGE_DIR: &str = \"{IMAGE_CA_DIR}\";")),
            "embrad merges another image directory than {IMAGE_CA_DIR}"
        );
        assert!(
            embrad.contains(&format!("const CA_DROPIN_DIR: &str = \"{OPERATOR_CA_DIR}\";")),
            "embrad merges another STATE directory than {OPERATOR_CA_DIR}"
        );
        assert!(
            embrad.contains("const MAX_CA_FILE_SIZE: u64 = 256 * 1024;"),
            "embrad's per-file cap moved: the build's IMPORTED_CAS_MAX_BYTES follows it"
        );
        assert_eq!(shell_value("IMPORTED_CAS_MAX_BYTES"), (256 * 1024).to_string());

        let hook = std::fs::read_to_string(root.join("buildroot/board/embraos/post_build.sh")).unwrap();
        assert!(
            hook.contains("imported_cas_install \"${REPO_ROOT}\" \"${TARGET_DIR}\""),
            "post_build.sh no longer bakes Imported_CAs/ into the target"
        );
    }

    #[tokio::test]
    async fn a_cross_origin_redirect_is_returned_and_a_same_origin_one_is_followed() {
        let home = MockServer::start().await;
        let away = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/leave"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", format!("{}/landed", away.uri())))
            .mount(&home)
            .await;
        Mock::given(method("GET"))
            .and(path("/stay"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/landed"))
            .mount(&home)
            .await;
        Mock::given(method("GET"))
            .and(path("/landed"))
            .respond_with(ResponseTemplate::new(200).set_body_string("home"))
            .mount(&home)
            .await;
        Mock::given(method("GET"))
            .and(path("/landed"))
            .respond_with(ResponseTemplate::new(200).set_body_string("away"))
            .mount(&away)
            .await;

        let c = client();
        let r = c
            .get(format!("{}/leave", home.uri()))
            .header("x-api-key", "k-0123456789")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 302, "the cross-origin redirect is handed back");
        assert!(away.received_requests().await.unwrap().is_empty(), "the other origin saw nothing");

        let r = c.get(format!("{}/stay", home.uri())).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.text().await.unwrap(), "home");
    }

    #[tokio::test]
    async fn the_database_client_follows_no_redirect() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/memory.entries/docs/e1"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/memory.entries/docs/e2"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/memory.entries/docs/e2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true, "data": {"_id": "e2"}, "meta": {}})))
            .mount(&server)
            .await;
        let db = crate::db::WardsonDbClient::from_url(&server.uri());
        let err = db.read("memory.entries", "e1").await.unwrap_err().to_string();
        assert!(err.contains("302"), "the redirect is an error, not a document: {err}");
        assert_eq!(server.received_requests().await.unwrap().len(), 1, "one request, no hop");
    }

    fn rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// #26: a client built from the bare `builder()` sets a timeout itself —
    /// the validate and probe clients a total one, the streaming and API
    /// clients the two bounds — so no reqwest client of the brain can hang
    /// on a connection that has gone dead. `client()` and `bounded()` carry
    /// the bounds already; this reads every other use.
    #[test]
    fn every_net_builder_sets_a_timeout() {
        let mut files = Vec::new();
        rust_sources(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
        // Spelled in two pieces so this test does not read itself.
        let call = ["crate::net::", "builder()"].concat();
        let mut seen = 0;
        for file in files {
            let source = std::fs::read_to_string(&file).unwrap();
            for (at, _) in source.match_indices(&call) {
                seen += 1;
                let rest = &source[at..];
                let statement = &rest[..rest.find(".build()").unwrap_or(rest.len().min(600))];
                assert!(
                    statement.contains("timeout"),
                    "{}: a client built from net::builder() sets no timeout:\n{statement}",
                    file.display()
                );
            }
        }
        assert!(seen >= 5, "the scan found {seen} builder sites; the pattern or the tree moved");
    }
}
