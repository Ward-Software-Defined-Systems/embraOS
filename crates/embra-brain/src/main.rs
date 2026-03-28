//! embra-brain — Headless AI runtime service.
//!
//! Extracted from Phase 0 embrad. Contains:
//! - Brain (Anthropic API integration)
//! - Tool dispatch (~63 tools)
//! - Session manager
//! - Proactive engine
//! - WardSONDB client
//! - Schema migrations
//!
//! Exposes all functionality via gRPC on port 50002.

mod brain;
mod tools;
mod db;
mod sessions;
mod proactive;
mod migrations;
mod config;
mod grpc_service;

use config::BrainConfig;
use grpc_service::BrainGrpcService;

use embra_common::proto::brain::brain_service_server::BrainServiceServer;
use tonic::transport::Server;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let config = BrainConfig::from_args();

    info!("embra-brain starting on port {}", config.port);

    // Initialize WardSONDB client
    let db_client = db::client::WardsonClient::new(&config.wardsondb_url);

    // Wait for WardSONDB to be ready
    info!("Waiting for WardSONDB...");
    db_client.wait_for_health(std::time::Duration::from_secs(30)).await?;
    info!("WardSONDB is ready");

    // Run schema migrations (same as Phase 0)
    migrations::run_migrations(&db_client).await?;
    info!("Migrations complete");

    // Create the gRPC service (holds Brain, sessions, tools, proactive engine)
    let service = BrainGrpcService::new(db_client, config.clone()).await?;

    let addr = format!("0.0.0.0:{}", config.port).parse()?;
    info!("embra-brain listening on {}", addr);

    Server::builder()
        .add_service(BrainServiceServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}
