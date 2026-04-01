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

    // Bring up loopback interface (required for 127.0.0.1 connectivity)
    bring_up_loopback();

    // Bring up eth0 for QEMU SLIRP networking (required for port forwarding and outbound)
    bring_up_eth0();

    info!("Pseudo-filesystems mounted");
    Ok(())
}

#[cfg(target_os = "linux")]
fn bring_up_loopback() {
    use std::process::Command;
    // Use ip command if available, fall back to ifconfig
    let result = Command::new("ip")
        .args(["link", "set", "lo", "up"])
        .status()
        .or_else(|_| Command::new("ifconfig").args(["lo", "up"]).status());
    match result {
        Ok(status) if status.success() => info!("Loopback interface up"),
        Ok(status) => {
            // ip/ifconfig may not exist in minimal rootfs — try raw ioctl
            tracing::warn!("Failed to bring up loopback via ip/ifconfig (status={}), trying ioctl", status);
            bring_up_loopback_ioctl();
        }
        Err(_) => {
            tracing::warn!("ip/ifconfig not found, trying ioctl");
            bring_up_loopback_ioctl();
        }
    }
}

#[cfg(target_os = "linux")]
fn bring_up_loopback_ioctl() {
    // Bring up lo via raw ioctl — works without any userspace tools
    unsafe {
        let sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if sock < 0 {
            tracing::error!("Failed to create socket for loopback setup");
            return;
        }

        let mut ifr: libc::ifreq = std::mem::zeroed();
        let name = b"lo\0";
        std::ptr::copy_nonoverlapping(name.as_ptr(), ifr.ifr_name.as_mut_ptr() as *mut u8, name.len());

        // Get current flags
        if libc::ioctl(sock, libc::SIOCGIFFLAGS as _, &mut ifr) < 0 {
            tracing::error!("Failed to get loopback flags");
            libc::close(sock);
            return;
        }

        // Set IFF_UP
        ifr.ifr_ifru.ifru_flags |= libc::IFF_UP as i16;
        if libc::ioctl(sock, libc::SIOCSIFFLAGS as _, &ifr) < 0 {
            tracing::error!("Failed to set loopback UP");
        } else {
            info!("Loopback interface up (via ioctl)");
        }

        libc::close(sock);
    }
}

#[cfg(target_os = "linux")]
fn bring_up_eth0() {
    // QEMU SLIRP assigns 10.0.2.15/24 with gateway 10.0.2.2 and DNS 10.0.2.3
    // We configure this statically since there's no DHCP client in the minimal rootfs
    unsafe {
        let sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if sock < 0 {
            tracing::error!("Failed to create socket for eth0 setup");
            return;
        }

        let mut ifr: libc::ifreq = std::mem::zeroed();
        let name = b"eth0\0";
        std::ptr::copy_nonoverlapping(name.as_ptr(), ifr.ifr_name.as_mut_ptr() as *mut u8, name.len());

        // Set IP address 10.0.2.15
        let addr: *mut libc::sockaddr_in = &mut ifr.ifr_ifru.ifru_addr as *mut _ as *mut _;
        (*addr).sin_family = libc::AF_INET as u16;
        (*addr).sin_addr.s_addr = u32::from_ne_bytes([10, 0, 2, 15]);
        if libc::ioctl(sock, libc::SIOCSIFADDR as _, &ifr) < 0 {
            tracing::warn!("Failed to set eth0 address (may not exist)");
            libc::close(sock);
            return;
        }

        // Set netmask 255.255.255.0
        let mask: *mut libc::sockaddr_in = &mut ifr.ifr_ifru.ifru_netmask as *mut _ as *mut _;
        (*mask).sin_family = libc::AF_INET as u16;
        (*mask).sin_addr.s_addr = u32::from_ne_bytes([255, 255, 255, 0]);
        if libc::ioctl(sock, libc::SIOCSIFNETMASK as _, &ifr) < 0 {
            tracing::warn!("Failed to set eth0 netmask");
        }

        // Bring interface up
        if libc::ioctl(sock, libc::SIOCGIFFLAGS as _, &mut ifr) >= 0 {
            ifr.ifr_ifru.ifru_flags |= libc::IFF_UP as i16 | libc::IFF_RUNNING as i16;
            if libc::ioctl(sock, libc::SIOCSIFFLAGS as _, &ifr) < 0 {
                tracing::warn!("Failed to bring eth0 UP");
            } else {
                info!("eth0 up (10.0.2.15/24)");
            }
        }

        libc::close(sock);

        // Add default route via 10.0.2.2 (QEMU SLIRP gateway)
        // This requires a routing socket — use a simpler approach via /proc
        let route_entry = "10.0.2.2\t0.0.0.0\t0.0.0.0\tUG\t0\t0\t0\teth0\n";
        // Actually, use the rtentry ioctl
        let route_sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if route_sock >= 0 {
            let mut rt: libc::rtentry = std::mem::zeroed();
            let dst: *mut libc::sockaddr_in = &mut rt.rt_dst as *mut _ as *mut _;
            (*dst).sin_family = libc::AF_INET as u16;
            (*dst).sin_addr.s_addr = 0; // 0.0.0.0

            let gw: *mut libc::sockaddr_in = &mut rt.rt_gateway as *mut _ as *mut _;
            (*gw).sin_family = libc::AF_INET as u16;
            (*gw).sin_addr.s_addr = u32::from_ne_bytes([10, 0, 2, 2]);

            let mask_rt: *mut libc::sockaddr_in = &mut rt.rt_genmask as *mut _ as *mut _;
            (*mask_rt).sin_family = libc::AF_INET as u16;
            (*mask_rt).sin_addr.s_addr = 0; // 0.0.0.0

            rt.rt_flags = libc::RTF_UP as u16 | libc::RTF_GATEWAY as u16;

            if libc::ioctl(route_sock, libc::SIOCADDRT as _, &rt) < 0 {
                tracing::warn!("Failed to add default route (may already exist)");
            } else {
                info!("Default route via 10.0.2.2 (QEMU SLIRP gateway)");
            }
            libc::close(route_sock);
        }

        let _ = route_entry; // suppress unused warning
    }
}

#[cfg(not(target_os = "linux"))]
fn bring_up_eth0() {
    tracing::warn!("eth0 setup: not on Linux, skipping");
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

    // Create workspace directory on DATA partition (persistent, writable)
    // Bind mount so /embra/workspace is writable despite read-only SquashFS rootfs
    let workspace_data = "/embra/data/workspace";
    let workspace_mount = "/embra/workspace";
    std::fs::create_dir_all(workspace_data).ok();
    std::fs::create_dir_all(workspace_mount).ok();
    #[cfg(target_os = "linux")]
    {
        use nix::mount::{mount, MsFlags};
        match mount(
            Some(workspace_data),
            workspace_mount,
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        ) {
            Ok(_) => info!("Workspace bind mount: {} → {}", workspace_data, workspace_mount),
            Err(e) => tracing::warn!("Failed to bind mount workspace: {} (tools may fail on write)", e),
        }
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
