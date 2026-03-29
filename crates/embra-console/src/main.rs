//! embra-console — TUI client for serial console.
//!
//! Connects to embra-brain via embra-apid (gRPC) and renders the full
//! conversational experience using ratatui.

mod terminal;
mod grpc_client;

use grpc_client::BrainClient;

#[tokio::main]
async fn main() {
    eprintln!("[embra-console] starting");

    // Parse args
    let args: Vec<String> = std::env::args().collect();
    let mut apid_addr = "http://127.0.0.1:50000".to_string();
    let mut device = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--apid-addr" => { apid_addr = args[i+1].clone(); i += 2; }
            "--device" => { device = Some(args[i+1].clone()); i += 2; }
            _ => { i += 1; }
        }
    }

    eprintln!("[embra-console] connecting to embra-apid at {}", apid_addr);
    let client = match BrainClient::connect(&apid_addr).await {
        Ok(c) => {
            eprintln!("[embra-console] connected to embra-apid");
            c
        }
        Err(e) => {
            eprintln!("[embra-console] FATAL: failed to connect to embra-apid: {}", e);
            // Retry loop — apid might not be ready yet
            eprintln!("[embra-console] retrying connection...");
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            match BrainClient::connect(&apid_addr).await {
                Ok(c) => {
                    eprintln!("[embra-console] connected on retry");
                    c
                }
                Err(e2) => {
                    eprintln!("[embra-console] FATAL: still can't connect: {}", e2);
                    return;
                }
            }
        }
    };

    eprintln!("[embra-console] launching terminal");
    match terminal::run(client, device).await {
        Ok(()) => eprintln!("[embra-console] terminal exited cleanly"),
        Err(e) => eprintln!("[embra-console] terminal error: {}", e),
    }
}
