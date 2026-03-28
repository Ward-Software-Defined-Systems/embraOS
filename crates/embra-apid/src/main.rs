//! embra-apid — gRPC + REST API gateway for embraOS.

mod config;
mod grpc;
mod rest;
mod proxy;

use config::ApidConfig;
use grpc::EmbraApiImpl;
use proxy::BackendConnections;

use embra_common::proto::apid::embra_api_server::EmbraApiServer;
use tonic::transport::Server as TonicServer;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let config = ApidConfig::from_args();

    info!("embra-apid starting (gRPC={}, REST={})", config.grpc_port, config.rest_port);

    let backends = BackendConnections::new(
        config.brain_addr.clone(),
        config.trust_addr.clone(),
    );

    // gRPC server
    let grpc_addr = format!("0.0.0.0:{}", config.grpc_port).parse()?;
    let grpc_service = EmbraApiImpl::new(backends.clone());

    let grpc_handle = tokio::spawn(async move {
        info!("gRPC server listening on {}", grpc_addr);
        TonicServer::builder()
            .add_service(EmbraApiServer::new(grpc_service))
            .serve(grpc_addr)
            .await
            .expect("gRPC server failed");
    });

    // REST server
    let rest_addr: std::net::SocketAddr = format!("0.0.0.0:{}", config.rest_port).parse()?;
    let rest_router = rest::build_router();

    let rest_handle = tokio::spawn(async move {
        info!("REST server listening on {}", rest_addr);
        let listener = tokio::net::TcpListener::bind(rest_addr).await.expect("REST bind failed");
        axum::serve(listener, rest_router).await.expect("REST server failed");
    });

    // Wait for either server to exit (shouldn't happen)
    tokio::select! {
        _ = grpc_handle => { info!("gRPC server exited"); }
        _ = rest_handle => { info!("REST server exited"); }
    }

    Ok(())
}
