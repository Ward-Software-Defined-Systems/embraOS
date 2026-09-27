//! TLS via embra-trustd.
//!
//! At boot, `embra-web` asks trustd's existing `GenerateCertificate` gRPC
//! for a server cert chained to the embraOS CA, then serves HTTPS with it.
//! The cert is held in memory only (cheap to re-mint each boot; no STATE
//! rotation problem). The supervisor starts `embra-web` after trustd, but
//! we still retry to cover the warm-up window.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use embra_common::proto::trust::{CertRole, GenerateCertRequest};
use embra_common::proto::trust::trust_service_client::TrustServiceClient;
use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tonic::transport::Channel;

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

/// Obtain a serving cert from trustd and build a rustls `ServerConfig`.
pub async fn acquire_server_config(trust_addr: &str) -> anyhow::Result<ServerConfig> {
    let mut client = connect_trust(trust_addr).await?;

    // SANs cover every path the operator's browser can reach the box on:
    // QEMU hostfwd (localhost/127.0.0.1), the SLIRP guest IP, and the
    // hostname. The browser will still warn (private embraOS CA) — the
    // operator installs the CA once to trust all services.
    let req = GenerateCertRequest {
        common_name: "embra-web".to_string(),
        san_dns: vec![
            "localhost".to_string(),
            "buildroot".to_string(),
            "embraos".to_string(),
        ],
        san_ip: vec!["127.0.0.1".to_string(), "10.0.2.15".to_string()],
        role: CertRole::Server as i32,
    };

    let resp = client
        .generate_certificate(req)
        .await
        .context("trustd GenerateCertificate failed")?
        .into_inner();

    build_server_config(&resp.cert_pem, &resp.key_pem)
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

fn build_server_config(cert_pem: &[u8], key_pem: &[u8]) -> anyhow::Result<ServerConfig> {
    let (certs, key) = parse_chain_and_key(cert_pem, key_pem)?;

    // Explicit provider (aws-lc-rs is the resolved rustls default in this
    // workspace) so we never depend on a process-global being installed.
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("rustls protocol versions")?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("rustls with_single_cert")?;

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{CertificateParams, IsCa, KeyPair};

    /// A CA and a server certificate it signed, minted the way trustd mints
    /// them: `KeyPair::generate()`, `cert.pem()`, `key.serialize_pem()`.
    /// Returns (leaf cert PEM, CA cert PEM, leaf key PEM).
    fn mint() -> (String, String, String) {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca = ca_params.self_signed(&ca_key).unwrap();

        let leaf_key = KeyPair::generate().unwrap();
        let leaf_params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        let leaf = leaf_params.signed_by(&leaf_key, &ca, &ca_key).unwrap();
        (leaf.pem(), ca.pem(), leaf_key.serialize_pem())
    }

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
    }
}
