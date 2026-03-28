//! embrad — PID 1 init for embraOS.
//!
//! Responsibilities:
//! 1. Mount /proc, /sys, /dev, /tmp (if not already mounted)
//! 2. Verify soul via embra-trustd
//! 3. Start services in dependency order
//! 4. Enter reconciliation loop
//! 5. Handle shutdown / reboot signals

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("embrad starting as PID {}", std::process::id());

    // TODO: Implement in Doc 02
    tracing::error!("embrad not yet implemented");
    Ok(())
}
