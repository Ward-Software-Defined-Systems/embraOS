//! embra-web — embraOS self-hosted web console.
//!
//! Serves an enterprise web shell (Leptos/WASM) that wraps the *real*
//! `embra-console` ratatui TUI in an xterm.js pane, bridged over a
//! PTY→WebSocket connection. The brain is single-conversation by
//! construction, so there is exactly one shared PTY + one `embra-console`
//! child per process; multiple browsers attach to it with single-writer +
//! read-only-observer + takeover arbitration (see `arbiter`).
//!
//! See the approved plan for the full architecture.

mod access;
mod activity_bridge;
mod activity_feed;
mod api_tools;
mod arbiter;
mod assets;
mod chat_bridge;
mod files;
mod media;
mod config;
mod metrics;
mod pty_bridge;
mod server;
mod sessions;
mod stop;
mod state;
mod status;
mod tls;
mod tool_bridge;
mod tool_feed;
mod ws;
mod ws_activity;
mod ws_chat;
mod ws_tools;

use std::sync::{Arc, Mutex};

use activity_feed::ActivityFeed;
use arbiter::Arbiter;
use config::WebConfig;
use pty_bridge::PtyBridge;
use state::AppState;
use tool_feed::ToolFeed;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "embra_web=info".into()),
        )
        .init();

    let cfg = WebConfig::from_args();
    tracing::info!(
        port = cfg.port,
        apid = %cfg.apid_addr,
        trust = %cfg.trust_addr,
        console = %cfg.console_bin,
        "embra-web starting"
    );

    // Serving cert from trustd (retries through its warm-up window).
    let server_config = tls::acquire_server_config(&cfg.trust_addr).await?;
    tracing::info!("obtained serving cert from embra-trustd");

    // This boot's access token (`access.rs`): embrad's, or one of our own
    // on the host. In the OS it is never logged; the boot banner shows it.
    let from_env = cfg.token.is_some();
    let token: std::sync::Arc<str> = match cfg.token.clone() {
        Some(t) => t.into(),
        None => access::generate_token()?.into(),
    };
    if from_env {
        tracing::info!("{}", access::startup_line(true, cfg.port, &token));
    } else {
        println!("{}", access::startup_line(false, cfg.port, &token));
    }

    // One shared PTY + one embra-console child for the process lifetime;
    // arbiter governs which connected browser may type.
    let bridge = PtyBridge::spawn(cfg.console_bin.clone(), cfg.apid_addr.clone());
    let state = AppState {
        bridge,
        arbiter: Arbiter::new(),
        cpu_snap: Arc::new(Mutex::new(None)),
        apid_addr: cfg.apid_addr.clone(),
        // One subscription to the brain's activity feed per process,
        // fanned out to every `/ws/activity` socket.
        activity: ActivityFeed::spawn(cfg.apid_addr.clone()),
        // One subscription to the brain's tool-call feed per process, kept
        // as a backlog and fanned out to every `/ws/tools` socket.
        tools: ToolFeed::spawn(cfg.apid_addr.clone()),
        token,
    };

    server::serve(&cfg, state, server_config).await
}
