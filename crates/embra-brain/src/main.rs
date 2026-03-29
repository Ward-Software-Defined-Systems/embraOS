//! embra-brain — Headless AI runtime service.
//!
//! Extracted from Phase 0 embrad. Contains:
//! - Brain (Anthropic API integration)
//! - Tool dispatch (~63 tools)
//! - Session manager
//! - Proactive engine
//! - WardSONDB client
//! - Schema migrations
//! - Learning Mode
//!
//! Exposes all functionality via gRPC on port 50002.

mod brain;
mod tools;
mod db;
mod sessions;
mod proactive;
mod migrations;
mod config;
mod learning;
mod grpc_service;

use grpc_service::BrainGrpcService;

use embra_common::proto::brain::brain_service_server::BrainServiceServer;
use tonic::transport::Server;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // Parse service args
    let args: Vec<String> = std::env::args().collect();
    let mut port = 50002u16;
    let mut wardsondb_url = "http://127.0.0.1:8090".to_string();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => { port = args[i+1].parse().expect("Invalid port"); i += 2; }
            "--wardsondb-url" => { wardsondb_url = args[i+1].clone(); i += 2; }
            _ => { i += 1; }
        }
    }

    info!("embra-brain starting on port {}", port);

    // Initialize start time for uptime tracking
    tools::init_start_time();

    // Initialize WardSONDB client
    let db = db::WardsonDbClient::from_url(&wardsondb_url);

    // Wait for WardSONDB to be ready
    info!("Waiting for WardSONDB...");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if db.health().await.unwrap_or(false) {
            break;
        }
        if std::time::Instant::now() > deadline {
            anyhow::bail!("WardSONDB did not become healthy within 30s");
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    info!("WardSONDB is ready");

    // Run schema migrations
    migrations::run_migrations(&db).await?;
    info!("Migrations complete");

    // Load config (or default for first run)
    let config_tz = match config::load_config(&db).await {
        Ok(cfg) => cfg.timezone.clone(),
        Err(_) => "UTC".to_string(),
    };

    // Start proactive engine
    let proactive_rx = proactive::start_proactive_engine(&db, &config_tz);

    // Create the gRPC service
    let service = BrainGrpcService::new(db, config_tz, proactive_rx);

    let addr = format!("0.0.0.0:{}", port).parse()?;
    info!("embra-brain listening on {}", addr);

    Server::builder()
        .add_service(BrainServiceServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}
