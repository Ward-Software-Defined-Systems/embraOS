//! embra-trustd — PKI and soul verification service for embraOS.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("embra-trustd starting");

    // TODO: Implement in Doc 03
    tracing::error!("embra-trustd not yet implemented");
    Ok(())
}
