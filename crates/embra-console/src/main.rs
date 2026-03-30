//! embra-console — TUI client for serial console.
//!
//! Connects to embra-brain via embra-apid (gRPC) and renders the full
//! conversational experience using ratatui.

mod terminal;
mod grpc_client;

use grpc_client::BrainClient;

#[tokio::main]
async fn main() {
    // Use println (stdout) for diagnostics since stderr goes to log file
    println!("[embra-console] starting");

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

    println!("[embra-console] connecting to embra-apid at {}", apid_addr);
    let client = match BrainClient::connect(&apid_addr).await {
        Ok(c) => {
            println!("[embra-console] connected");
            c
        }
        Err(e) => {
            println!("[embra-console] FATAL: failed to connect: {}", e);
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            match BrainClient::connect(&apid_addr).await {
                Ok(c) => c,
                Err(e2) => {
                    println!("[embra-console] FATAL: still can't connect: {}", e2);
                    // Keep process alive so embrad doesn't restart-loop
                    loop { tokio::time::sleep(std::time::Duration::from_secs(3600)).await; }
                }
            }
        }
    };

    println!("[embra-console] launching TUI");
    match terminal::run(client, device).await {
        Ok(()) => println!("[embra-console] exited cleanly"),
        Err(e) => {
            println!("[embra-console] TUI error: {}", e);
            // Keep process alive
            loop { tokio::time::sleep(std::time::Duration::from_secs(3600)).await; }
        }
    }
}
