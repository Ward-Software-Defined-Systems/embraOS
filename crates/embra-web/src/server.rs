//! HTTPS router + a custom TLS `Listener` so `axum::serve` (which handles
//! WebSocket upgrades) runs straight over `tokio-rustls` — no hyper-util.
//!
//! `/` currently serves a throwaway xterm.js page (vendored Leptos UI
//! replaces it in a later step); it's a useful bring-up milestone for
//! verifying TLS + PTY + the real TUI in a browser.

use std::io;
use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use rustls::ServerConfig;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use crate::assets::static_handler;
use crate::config::WebConfig;
use crate::sessions::api_sessions_list;
use crate::stop::api_stop;
use crate::state::AppState;
use crate::status::api_status;
use crate::ws::ws_terminal;
use crate::ws_chat::ws_chat;

/// A TLS-terminating listener. `axum::serve::Listener::accept` has no
/// Result, so transient TCP/handshake errors are logged and skipped.
struct RustlsListener {
    tcp: TcpListener,
    acceptor: TlsAcceptor,
}

impl axum::serve::Listener for RustlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, addr) = match self.tcp.accept().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "tcp accept failed");
                    continue;
                }
            };
            match self.acceptor.accept(stream).await {
                Ok(tls) => return (tls, addr),
                Err(e) => {
                    tracing::debug!(%addr, error = %e, "tls handshake failed");
                    continue;
                }
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
        .route("/api/status", get(api_status))
        .route("/api/sessions", get(api_sessions_list))
        .route("/api/stop", post(api_stop))
        .fallback(static_handler)
        .with_state(state);

    let addr = format!("0.0.0.0:{}", cfg.port);
    let tcp = TcpListener::bind(&addr).await?;
    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    tracing::info!(%addr, "embra-web HTTPS listening");

    axum::serve(RustlsListener { tcp, acceptor }, app).await?;
    Ok(())
}
