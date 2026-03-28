//! embra-console — TUI client for embraOS serial console.
//!
//! Connects to embra-brain via embra-apid (gRPC) and renders the full
//! conversational terminal experience over /dev/ttyS0 (serial) or any TTY.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("embra-console starting");

    // TODO: Implement in Doc 05
    tracing::error!("embra-console not yet implemented");
    Ok(())
}
