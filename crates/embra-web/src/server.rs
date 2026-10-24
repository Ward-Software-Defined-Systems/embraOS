//! HTTPS router + a custom TLS `Listener` so `axum::serve` (which handles
//! WebSocket upgrades) runs straight over `tokio-rustls` — no hyper-util.
//!
//! The router: the four sockets, the `/api/*` doors and the embedded
//! frontend under `/embraOS` (`assets.rs`), every route behind the token
//! gate (`access.rs`).

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post, put};
use rustls::ServerConfig;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use crate::access;
use crate::api_tools::api_tools_list;
use crate::assets::static_handler;
use crate::config::WebConfig;
use crate::media::{MEDIA_BODY_LIMIT, api_media_get, api_media_put};
use crate::files::api_file_get;
use crate::sessions::api_sessions_list;
use crate::stop::api_stop;
use crate::state::AppState;
use crate::status::api_status;
use crate::ws::ws_terminal;
use crate::ws_activity::ws_activity;
use crate::ws_chat::ws_chat;
use crate::ws_tools::ws_tools;

/// How long a client has to finish its TLS handshake. A client that
/// connects and says nothing holds its own task for this long and nothing
/// else (#22).
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// The pause after a failed `accept(2)` (out of descriptors, a connection
/// reset in the backlog), so the error is logged per attempt and not in a
/// hot loop.
const ACCEPT_ERROR_PAUSE: Duration = Duration::from_millis(100);
/// Finished handshakes waiting for `axum::serve` to take them.
const READY_QUEUE: usize = 64;

type Accepted = (TlsStream<TcpStream>, SocketAddr);

/// A TLS-terminating listener. `axum::serve::Listener::accept` has no
/// Result, so transient TCP/handshake errors are logged and skipped.
///
/// Each handshake runs in a task of its own under `TLS_HANDSHAKE_TIMEOUT`
/// and hands the finished stream back through the ready queue: a client
/// that connects and never speaks TLS (a port scan, a stalled network path)
/// holds one task, not the accept loop, and the console keeps answering
/// everyone else (#22).
struct RustlsListener {
    tcp: TcpListener,
    acceptor: TlsAcceptor,
    ready_tx: mpsc::Sender<Accepted>,
    ready_rx: mpsc::Receiver<Accepted>,
}

impl RustlsListener {
    fn new(tcp: TcpListener, acceptor: TlsAcceptor) -> Self {
        let (ready_tx, ready_rx) = mpsc::channel(READY_QUEUE);
        Self { tcp, acceptor, ready_tx, ready_rx }
    }

    /// Start one accepted connection's handshake in its own task.
    fn handshake(&self, stream: TcpStream, addr: SocketAddr) {
        let acceptor = self.acceptor.clone();
        let ready = self.ready_tx.clone();
        tokio::spawn(async move {
            match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                // A closed queue means the listener is gone; nothing to serve.
                Ok(Ok(tls)) => drop(ready.send((tls, addr)).await),
                Ok(Err(e)) => tracing::debug!(%addr, error = %e, "tls handshake failed"),
                Err(_) => tracing::debug!(%addr, "tls handshake timed out"),
            }
        });
    }
}

impl axum::serve::Listener for RustlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            tokio::select! {
                ready = self.ready_rx.recv() => {
                    // `self` holds a sender, so the queue never closes.
                    if let Some(accepted) = ready {
                        return accepted;
                    }
                }
                accepted = self.tcp.accept() => match accepted {
                    Ok((stream, addr)) => self.handshake(stream, addr),
                    Err(e) => {
                        tracing::warn!(error = %e, "tcp accept failed");
                        tokio::time::sleep(ACCEPT_ERROR_PAUSE).await;
                    }
                },
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

pub async fn serve(
    cfg: &WebConfig,
    state: AppState,
    server_config: ServerConfig,
) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/ws/terminal", get(ws_terminal))
        .route("/ws/chat", get(ws_chat))
        .route("/ws/activity", get(ws_activity))
        .route("/ws/tools", get(ws_tools))
        .route("/api/status", get(api_status))
        .route("/api/tools", get(api_tools_list))
        .route("/api/sessions", get(api_sessions_list))
        .route("/api/stop", post(api_stop))
        // Media store door. Registered explicitly — the SPA fallback
        // below answers any unknown path with index.html/200. The body
        // limit overrides axum's 2 MiB default so the brain's own size
        // cap is what an oversize upload hits (with a clear message).
        .route(
            "/api/media",
            put(api_media_put).layer(DefaultBodyLimit::max(MEDIA_BODY_LIMIT)),
        )
        .route("/api/media/{id}", get(api_media_get))
        // A workspace file as a download (`file_offer`, `/download`).
        .route("/api/files/{*path}", get(api_file_get))
        .fallback(static_handler)
        // The gate (`access.rs`) over every route and the fallback: the
        // boot's token in the query once, the cookie after, 401 otherwise.
        .layer(axum::middleware::from_fn_with_state(state.clone(), access::require_token))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", cfg.port);
    let tcp = TcpListener::bind(&addr).await?;
    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    tracing::info!(%addr, "embra-web HTTPS listening");

    axum::serve(RustlsListener::new(tcp, acceptor), app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::serve::Listener;
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, ServerName};
    use tokio_rustls::TlsConnector;

    /// A browser's side: a client that trusts the test CA alone.
    fn connector(ca_pem: &str) -> TlsConnector {
        let mut roots = rustls::RootCertStore::empty();
        for cert in CertificateDer::pem_slice_iter(ca_pem.as_bytes()) {
            roots.add(cert.unwrap()).unwrap();
        }
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        TlsConnector::from(Arc::new(config))
    }

    /// A client that connects and never starts its handshake holds nothing
    /// but its own task: the next client's handshake completes and that
    /// connection is accepted while the first is still silent (#22). With
    /// the handshake inline in the accept loop, the second client waited
    /// behind the first for good.
    #[tokio::test]
    async fn a_stalled_handshake_does_not_hold_up_the_next_connection() {
        let (leaf, ca, key) = crate::tls::mint_for_tests();
        let server_config = crate::tls::build_server_config(leaf.as_bytes(), key.as_bytes()).unwrap();
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        let mut listener = RustlsListener::new(tcp, TlsAcceptor::from(Arc::new(server_config)));

        // Connected at the TCP level, never a ClientHello.
        let stalled = TcpStream::connect(addr).await.unwrap();
        let accepted = tokio::spawn(async move { listener.accept().await });

        let speaking = TcpStream::connect(addr).await.unwrap();
        let speaking_addr = speaking.local_addr().unwrap();
        let server_name = ServerName::try_from("localhost").unwrap();
        let handshake = connector(&ca).connect(server_name, speaking);
        let (client_side, server_side) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(5), handshake),
            tokio::time::timeout(Duration::from_secs(5), accepted),
        );
        let _client = client_side.expect("the handshake completed while another client stalled").expect("tls client");
        let (_server, peer) = server_side.expect("accepted while another client stalled").expect("accept task");
        assert_eq!(peer, speaking_addr, "the accepted connection is the one that spoke");
        drop(stalled);
    }
}
