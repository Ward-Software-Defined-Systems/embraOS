//! embrad — PID 1 init for embraOS.
//!
//! Boot sequence:
//! 1. Install the panic hook (PID 1 only)
//! 2. Mount pseudo-filesystems
//! 3. Verify partition mounts
//! 4. Set the environment every service inherits (one thread, no runtime yet)
//! 5. Start services in dependency order
//! 6. Verify soul (HALT on failure)
//! 7. Enter reconciliation loop
//!
//! PID 1 never exits. A fatal boot error and a panic both end in
//! `supervisor::halt_system`, with the reason written to
//! `/embra/state/halt_reason` on STATE, where the intelligence can read it
//! with `file_read` and the operator by loop-mounting the partition. An
//! exit of PID 1 is a kernel panic ("Attempted to kill init!") with the
//! reason lost in the kernel log. In development mode (not PID 1) a fatal
//! error is returned as before.

mod ca_bundle;
mod mount;
mod reconcile;
mod supervisor;

use anyhow::Result;
use tracing::{error, info, warn};

use supervisor::{halt_system, reboot_system, HALT_REASON_PATH};

fn main() -> Result<()> {
    init_logging();

    let pid = std::process::id();
    if pid == 1 {
        install_panic_hook();
    }

    // The mounts and the environment come before the runtime: one thread
    // runs here, which is what makes the `set_var` calls in
    // `supervisor::prepare_environment` sound (#29). A failure is a halt
    // with its reason, as below.
    match prepare(pid) {
        Ok(()) => {}
        Err(e) if pid == 1 => halt_system(&fatal_reason(&e)),
        Err(e) => return Err(e),
    }

    // The runtime is built by hand, so a failure is a halt with its reason
    // and not an unwinding panic inside `#[tokio::main]`.
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) if pid == 1 => halt_system(&format!("boot failed: tokio runtime: {e}")),
        Err(e) => return Err(e.into()),
    };

    match runtime.block_on(run()) {
        Ok(()) => {}
        Err(e) if pid == 1 => halt_system(&fatal_reason(&e)),
        Err(e) => return Err(e),
    }

    if pid == 1 {
        // PID 1 should reboot or halt
        reboot_system();
    }

    Ok(())
}

/// Steps 2 to 4 of the boot, synchronous and before the runtime: the
/// pseudo-filesystems (PID 1 only), the partition mounts, the environment.
fn prepare(pid: u32) -> Result<()> {
    info!("embrad starting as PID {}", pid);

    if pid != 1 {
        warn!("embrad is not PID 1 (pid={}). Running in development mode.", pid);
        // In development mode, skip filesystem mounts but still run services.
        // This allows testing the supervisor without being actual PID 1.
    }

    // Step 2: Mount pseudo-filesystems
    if pid == 1 {
        info!("Mounting pseudo-filesystems");
        mount::mount_pseudofs().map_err(|e| {
            error!("Failed to mount pseudo-filesystems: {}", e);
            e
        })?;
    }

    // Step 3: Verify partition mounts
    info!("Verifying partition mounts");
    mount::verify_partitions().map_err(|e| {
        error!("Partition verification failed: {}", e);
        e
    })?;

    // Step 4: The environment every service inherits (TZ, the operator CA
    // trust). The ephemeral tmpfs the CA bundle is written to exists now.
    supervisor::prepare_environment();
    Ok(())
}

/// The services and the reconciliation loop. An `Err` is fatal: PID 1
/// halts with it as the reason, development mode returns it.
async fn run() -> Result<()> {
    // Step 5: Start services in dependency order
    info!("Starting services");
    let mut supervisor = supervisor::Supervisor::new();

    // Define services in dependency order
    supervisor.register_services();

    // Start all services — this calls embra-trustd for soul verification
    // and HALTs the system if it fails. Any other startup failure comes
    // back here and halts PID 1 with its reason.
    supervisor.start_all().await.map_err(|e| {
        error!("Service startup failed: {}", e);
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
    Ok(())
}

/// The panic hook for PID 1. The reason goes to STATE and is synced before
/// anything else: `halt_system` logs through tracing, and a panic raised
/// inside the subscriber must not re-enter it before the reason is on disk.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let reason = panic_reason(
            info.payload_as_str(),
            info.location().map(|l| (l.file(), l.line())),
        );
        let _ = std::fs::write(HALT_REASON_PATH, &reason);
        // SAFETY: sync(2) takes no arguments and cannot fail.
        unsafe { libc::sync() };
        halt_system(&reason);
    }));
}

/// The halt reason for a panic: its message and where it was raised.
fn panic_reason(message: Option<&str>, location: Option<(&str, u32)>) -> String {
    let message = message.unwrap_or("no message");
    match location {
        Some((file, line)) => format!("panic: {message} ({file}:{line})"),
        None => format!("panic: {message}"),
    }
}

/// The halt reason for a fatal boot error: the error with its context chain.
fn fatal_reason(e: &anyhow::Error) -> String {
    format!("boot failed: {e:#}")
}

fn init_logging() {
    // Try /dev/kmsg first (kernel log ring buffer)
    // Fall back to stderr
    // `try_init`: a subscriber already set must not panic PID 1.
    let _ = tracing_subscriber::fmt()
        .with_target(true)
        .with_level(true)
        .with_ansi(false) // No ANSI in kernel log
        .try_init();
}

#[cfg(test)]
mod halt_reason_tests {
    use super::{fatal_reason, panic_reason};

    #[test]
    fn panic_reason_names_the_message_and_the_location() {
        assert_eq!(
            panic_reason(Some("mount failed"), Some(("crates/embrad/src/mount.rs", 42))),
            "panic: mount failed (crates/embrad/src/mount.rs:42)"
        );
        assert_eq!(panic_reason(Some("x"), None), "panic: x");
        assert_eq!(panic_reason(None, None), "panic: no message");
    }

    #[test]
    fn a_fatal_boot_error_is_a_halt_reason_with_its_chain() {
        let e = anyhow::anyhow!("No such file or directory").context("mount /embra/state");
        assert_eq!(fatal_reason(&e), "boot failed: mount /embra/state: No such file or directory");
    }
}
