//! embra-console — TUI client for serial console.

mod terminal;
mod grpc_client;

use grpc_client::BrainClient;

#[tokio::main]
async fn main() {
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

    println!("[embra-console] connecting to {}", apid_addr);
    let client = match BrainClient::connect(&apid_addr).await {
        Ok(c) => {
            println!("[embra-console] connected");
            c
        }
        Err(e) => {
            println!("[embra-console] connect failed: {}, retrying...", e);
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            match BrainClient::connect(&apid_addr).await {
                Ok(c) => c,
                Err(e2) => {
                    println!("[embra-console] FATAL: {}", e2);
                    loop { tokio::time::sleep(std::time::Duration::from_secs(3600)).await; }
                }
            }
        }
    };

    println!("[embra-console] launching TUI...");
    match terminal::run(client, device).await {
        Ok(()) => println!("[embra-console] exited"),
        Err(e) => {
            println!("[embra-console] TUI error: {}", e);
            loop { tokio::time::sleep(std::time::Duration::from_secs(3600)).await; }
        }
    }
}
