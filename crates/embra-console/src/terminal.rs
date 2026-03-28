//! TUI terminal stub.
//!
//! Will be adapted from Phase 0's src/terminal/ module.
//! For now, provides the entry point expected by main.rs.

use crate::grpc_client::BrainClient;
use anyhow::Result;
use tracing::info;

/// Run the TUI terminal.
/// In Phase 1, this connects to embra-brain via embra-apid gRPC
/// and renders the full conversational experience.
pub async fn run(mut client: BrainClient, device: Option<String>) -> Result<()> {
    info!("Terminal starting (device={:?})", device);

    // Open conversation stream
    let (_in_tx, mut out_rx) = client.open_conversation("").await?;

    // TODO: Initialize ratatui terminal on the specified device
    // TODO: Adapt Phase 0 terminal event loop:
    //   - Replace brain_rx with out_rx (ConsoleEvent receiver)
    //   - Replace direct Brain calls with in_tx.send(ConversationRequest)
    //   - Proactive notifications arrive via the same gRPC stream

    // Stub: just drain events
    info!("Terminal stub: draining events from gRPC stream");
    while let Some(event) = out_rx.recv().await {
        info!("Console event: {:?}", event);
    }

    info!("Terminal exiting");
    Ok(())
}
