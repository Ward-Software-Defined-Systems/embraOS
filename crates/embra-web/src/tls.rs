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

fn build_server_config(cert_pem: &[u8], key_pem: &[u8]) -> anyhow::Result<ServerConfig> {
    let mut cert_rd: &[u8] = cert_pem;
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_rd)
        .collect::<Result<_, _>>()
        .context("parse cert PEM from trustd")?;
    anyhow::ensure!(!certs.is_empty(), "trustd returned an empty cert chain");

    let mut key_rd: &[u8] = key_pem;
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_rd)
        .context("parse key PEM from trustd")?
        .ok_or_else(|| anyhow::anyhow!("trustd returned no private key"))?;

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
