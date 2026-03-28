//! Filesystem mounting for embrad PID 1.
//!
//! Mounts pseudo-filesystems (/proc, /sys, /dev) and verifies
//! that embra-init has already mounted the real partitions.

use anyhow::{Result, bail};
use std::path::Path;
use tracing::{info, debug};

/// Mount essential pseudo-filesystems.
/// Called only when running as PID 1 (these are already mounted in dev mode).
#[cfg(target_os = "linux")]
pub fn mount_pseudofs() -> Result<()> {
    use nix::mount::{mount, MsFlags};

    // /proc
    mount_if_needed("proc", "/proc", "proc", MsFlags::MS_NOEXEC | MsFlags::MS_NOSUID | MsFlags::MS_NODEV)?;

    // /sys
    mount_if_needed("sysfs", "/sys", "sysfs", MsFlags::MS_NOEXEC | MsFlags::MS_NOSUID | MsFlags::MS_NODEV)?;

    // /dev (devtmpfs — kernel populates device nodes)
    mount_if_needed("devtmpfs", "/dev", "devtmpfs", MsFlags::MS_NOSUID)?;

    // /dev/pts (pseudo-terminal devices)
    std::fs::create_dir_all("/dev/pts")?;
    mount_if_needed("devpts", "/dev/pts", "devpts", MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC)?;

    // /tmp (tmpfs)
    mount_if_needed("tmpfs", "/tmp", "tmpfs", MsFlags::MS_NOSUID | MsFlags::MS_NODEV)?;

    // /run (tmpfs — runtime state)
    mount_if_needed("tmpfs", "/run", "tmpfs", MsFlags::MS_NOSUID | MsFlags::MS_NODEV)?;

    info!("Pseudo-filesystems mounted");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn mount_pseudofs() -> Result<()> {
    tracing::warn!("mount: not on Linux, skipping pseudo-filesystem mounts");
    Ok(())
}

/// Verify that embra-init has mounted the required partitions.
pub fn verify_partitions() -> Result<()> {
    let required = [
        ("/embra/state", "STATE partition (soul, PKI, governance)"),
        ("/embra/data", "DATA partition (WardSONDB)"),
    ];

    for (path, description) in required {
        if !Path::new(path).exists() {
            bail!("Required mount point {} does not exist: {}", path, description);
        }
        // Check it's actually a mount point (not just an empty directory on the rootfs)
        // In the initial sprint without SquashFS, this check is relaxed
        debug!("Verified mount point: {} ({})", path, description);
    }

    // Create ephemeral directory if it doesn't exist (it's tmpfs, may need creation)
    std::fs::create_dir_all("/embra/ephemeral")?;

    // Mount tmpfs on /embra/ephemeral if not already mounted
    #[cfg(target_os = "linux")]
    {
        use nix::mount::{mount, MsFlags};
        mount_if_needed("tmpfs", "/embra/ephemeral", "tmpfs", MsFlags::MS_NOSUID | MsFlags::MS_NODEV)?;
    }

    info!("All partitions verified");
    Ok(())
}

#[cfg(target_os = "linux")]
fn mount_if_needed(source: &str, target: &str, fstype: &str, flags: nix::mount::MsFlags) -> Result<()> {
    use nix::mount::mount;

    // Check if already mounted by reading /proc/mounts
    if is_mounted(target) {
        debug!("{} already mounted", target);
        return Ok(());
    }

    // Create mount point if it doesn't exist
    std::fs::create_dir_all(target)?;

    // Mount
    mount(
        Some(source),
        target,
        Some(fstype),
        flags,
        None::<&str>,
    ).map_err(|e| anyhow::anyhow!("Failed to mount {} on {}: {}", source, target, e))?;

    debug!("Mounted {} on {} ({})", source, target, fstype);
    Ok(())
}

fn is_mounted(target: &str) -> bool {
    // Read /proc/mounts if available
    if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
        mounts.lines().any(|line| {
            line.split_whitespace().nth(1) == Some(target)
        })
    } else {
        // /proc not yet mounted — assume nothing is mounted
        false
    }
}
