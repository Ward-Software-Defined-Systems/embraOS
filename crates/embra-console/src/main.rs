//! embra-console — TUI client for serial console.
//!
//! Connects to embra-brain via embra-apid (gRPC) and renders the full
//! conversational experience using ratatui.

mod terminal;
mod grpc_client;

use grpc_client::BrainClient;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // Parse args
    let args: Vec<String> = std::env::args().collect();
    let mut apid_addr = "http://127.0.0.1:50000".to_string();
    let mut device = None; // None = use current TTY

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--apid-addr" => { apid_addr = args[i+1].clone(); i += 2; }
            "--device" => { device = Some(args[i+1].clone()); i += 2; }
            _ => { i += 1; }
        }
    }

    // Connect to embra-apid
    info!("Connecting to embra-apid at {}", apid_addr);
    let client = BrainClient::connect(&apid_addr).await?;

    // Run the TUI
    terminal::run(client, device).await?;

    Ok(())
}
