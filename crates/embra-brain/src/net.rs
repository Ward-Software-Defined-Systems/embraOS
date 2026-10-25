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
//! Every client built here also trusts the operator's CA drop-ins beside
//! the compiled-in webpki roots ([`operator_roots`]:
//! `/embra/state/ca-certificates/`, or `EMBRA_CA_DIR` alone when set, read
//! once per process), so a host behind the operator's own certificate
//! authority — a self-hosted GitLab, the provider gateway over HTTPS —
//! verifies. The database client is built apart: loopback, plain HTTP, no
//! roots.

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
/// Replaces [`OPERATOR_CA_DIR`] when set and non-empty (development).
pub(crate) const OPERATOR_CA_DIR_ENV: &str = "EMBRA_CA_DIR";

/// The drop-in directory in force: the environment's, else the STATE one.
fn operator_ca_dir() -> PathBuf {
    std::env::var(OPERATOR_CA_DIR_ENV)
        .ok()
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(OPERATOR_CA_DIR))
}

/// The operator's CA drop-ins, read once per process at the first client
/// built, and shared by every client of the brain and by the Guardian
/// transport. The directory is seeded while the OS is stopped, so a read at
/// every client build would see the same files; a seeded certificate takes
/// effect at the next boot.
pub(crate) fn operator_roots() -> &'static [reqwest::Certificate] {
    static ROOTS: OnceLock<Vec<reqwest::Certificate>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        let dir = operator_ca_dir();
        let roots = operator_roots_in(&dir);
        if !roots.is_empty() {
            tracing::info!(
                "operator CA drop-ins: {} certificate(s) from {}",
                roots.len(),
                dir.display()
            );
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
/// The Gemini and OpenAI-compat chat clients and the GitHub and GitLab
/// clients are this.
pub(crate) fn client() -> reqwest::Client {
    bounded()
        .build()
        .expect("a reqwest client with a redirect policy and the two bounds builds")
}

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

#[cfg(test)]
mod tests {
    use super::*;
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
