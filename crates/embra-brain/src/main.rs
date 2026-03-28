//! embra-brain — Headless AI runtime service for embraOS.
//!
//! This is the extracted Phase 0 runtime (Brain, tools, sessions, proactive engine)
//! running as a gRPC service instead of an in-process TUI application.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("embra-brain starting");

    // TODO: Implement in Doc 05
    tracing::error!("embra-brain not yet implemented");
    Ok(())
}
