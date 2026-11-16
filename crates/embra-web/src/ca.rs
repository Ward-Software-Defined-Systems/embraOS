//! `GET /api/ca.crt`: the certificate authority's certificate.
//!
//! The web console's certificate is signed by a certificate authority this
//! machine made for itself, which no browser knows: every browser warns
//! until its operator has imported that authority's certificate once. This
//! route hands it out, behind the boot's token like every route. With it
//! imported, the console opens without a warning at every name its
//! certificate carries: `localhost`, the machine's name, and the private
//! addresses its cards hold (`embra_boot::names::leaf_names`).
//!
//! The certificate is public by its nature (every handshake shows what it
//! signed); the private key never leaves the trust service. trustd is asked
//! at each request, since it issues the certificate again when the operator
//! names the machine.

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

/// What a client is told when trustd does not answer.
pub(crate) const AWAY: &str = "The trust service did not answer: the CA certificate cannot be handed out now. Try again in a moment.\n";

pub async fn api_ca_cert(State(st): State<AppState>) -> Response {
    match crate::tls::ca_certificate(&st.trust_addr).await {
        Ok(pem) => attachment(pem),
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "the CA certificate could not be had from embra-trustd");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8"), (header::CACHE_CONTROL, "no-store")],
                AWAY,
            )
                .into_response()
        }
    }
}

/// The certificate as a file to keep: a browser saves it, and does not try
/// to install it into whatever store it thinks fit.
fn attachment(pem: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/x-pem-file"),
            (header::CONTENT_DISPOSITION, "attachment; filename=\"embraos-ca.crt\""),
            (header::CACHE_CONTROL, "no-store"),
        ],
        pem,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_ca_certificate_is_a_file_to_keep_and_a_trust_service_that_is_away_is_said() {
        // Nothing listens at the stub's trust address.
        let (state, _input) = AppState::stub();
        let resp = api_ca_cert(State(state)).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-store");
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        assert_eq!(body, AWAY.as_bytes());

        let resp = attachment(b"-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n".to_vec());
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()[header::CONTENT_DISPOSITION], "attachment; filename=\"embraos-ca.crt\"");
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "application/x-pem-file");
        assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-store");
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        assert!(body.starts_with(b"-----BEGIN CERTIFICATE-----"));
    }
}
