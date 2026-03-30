//! embrad — PID 1 init for embraOS.
//!
//! Boot sequence:
//! 1. Install signal handlers
//! 2. Mount pseudo-filesystems
//! 3. Verify partition mounts
//! 4. Start services in dependency order
//! 5. Verify soul (HALT on failure)
//! 6. Enter reconciliation loop

mod config;
mod mount;
mod reconcile;
mod supervisor;

use anyhow::Result;
use tracing::{info, error, warn};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging — write to /dev/kmsg if available (kernel log),
    // fall back to stderr. In a real OS, this is the only reliable output
    // before services are up.
    init_logging();

    let pid = std::process::id();
    info!("embrad starting as PID {}", pid);

    if pid != 1 {
        warn!("embrad is not PID 1 (pid={}). Running in development mode.", pid);
        // In development mode, skip filesystem mounts but still run services.
        // This allows testing the supervisor without being actual PID 1.
    }

    // Step 1: Mount pseudo-filesystems
    if pid == 1 {
        info!("Mounting pseudo-filesystems");
        mount::mount_pseudofs().map_err(|e| {
            error!("Failed to mount pseudo-filesystems: {}", e);
            e
        })?;
    }

    // Step 2: Verify partition mounts
    info!("Verifying partition mounts");
    mount::verify_partitions().map_err(|e| {
        error!("Partition verification failed: {}", e);
        e
    })?;

    // Step 3: Start services in dependency order
    info!("Starting services");
    let mut supervisor = supervisor::Supervisor::new();

    // Define services in dependency order
    supervisor.register_services();

    // Start all services — this calls embra-trustd for soul verification
    // and HALTs the system if it fails
    supervisor.start_all().await.map_err(|e| {
        error!("Service startup failed: {}", e);
        if pid == 1 {
            // Give tracing time to flush to serial before halting
            std::thread::sleep(std::time::Duration::from_secs(2));
            halt_system("Service startup failure");
        }
        e
    })?;

    info!("All services started. Entering reconciliation loop.");

    // Step 4: Reconciliation loop (runs forever)
    // Note: embrad's stdout/stderr are already redirected to log file
    // (done in supervisor before spawning embra-console)
    reconcile::run(&mut supervisor).await;

    // If we get here, we're shutting down
    info!("embrad shutting down");
    supervisor.stop_all().await;

    if pid == 1 {
        // PID 1 should reboot or halt
        reboot_system();
    }

    Ok(())
}

fn init_logging() {
    // Try /dev/kmsg first (kernel log ring buffer)
    // Fall back to stderr
    tracing_subscriber::fmt()
        .with_target(true)
        .with_level(true)
        .with_ansi(false) // No ANSI in kernel log
        .init();
}

#[cfg(target_os = "linux")]
fn halt_system(reason: &str) -> ! {
    error!("SYSTEM HALT: {}", reason);
    // Write reason to STATE partition for post-mortem
    let _ = std::fs::write("/embra/state/halt_reason", reason);
    // Sync filesystems
    unsafe { libc::sync(); }
    // Halt
    unsafe { libc::reboot(libc::LINUX_REBOOT_CMD_HALT); }
    // If reboot() fails, loop forever
    loop { std::thread::sleep(std::time::Duration::from_secs(3600)); }
}

#[cfg(not(target_os = "linux"))]
fn halt_system(reason: &str) -> ! {
    error!("SYSTEM HALT (dev mode, would halt on Linux): {}", reason);
    let _ = std::fs::write("/tmp/embra-halt-reason", reason);
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn reboot_system() -> ! {
    info!("Rebooting system");
    unsafe { libc::sync(); }
    unsafe { libc::reboot(libc::LINUX_REBOOT_CMD_RESTART); }
    loop { std::thread::sleep(std::time::Duration::from_secs(3600)); }
}

#[cfg(not(target_os = "linux"))]
fn reboot_system() -> ! {
    info!("Reboot requested (dev mode, would reboot on Linux)");
    std::process::exit(0);
}
