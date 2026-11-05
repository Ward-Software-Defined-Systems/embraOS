//! embra-init — Initramfs early boot for embraOS.
//!
//! This is a minimal, synchronous Rust binary. No tokio, no async.
//! It must work before the full OS is available.

fn main() {
    eprintln!("[embra-init] embraOS early boot starting");

    #[cfg(target_os = "linux")]
    {
        if let Err(e) = boot() {
            // The reason goes to the serial line, then the machine halts:
            // an initramfs that cannot mount its root has nothing to wait
            // for, and a halt is what the operator sees in QEMU, where a
            // sleep forever looked like a hang (#29).
            eprintln!("[embra-init] FATAL: {}", e);
            eprintln!("[embra-init] Halting: the boot cannot continue");
            std::thread::sleep(std::time::Duration::from_secs(2));
            let _ = nix::sys::reboot::reboot(nix::sys::reboot::RebootMode::RB_HALT_SYSTEM);
            // reboot(2) returned: stay here rather than exit, which would
            // panic the kernel ("Attempted to kill init!").
            loop { std::thread::sleep(std::time::Duration::from_secs(3600)); }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("[embra-init] Not running on Linux — this binary is for initramfs only");
        eprintln!("[embra-init] Boot sequence would be:");
        eprintln!("[embra-init]   1. Mount /proc, /sys, /dev");
        eprintln!("[embra-init]   2. Mount the SquashFS rootfs of the slot the kernel line names (embra.slot=a|b,");
        eprintln!("[embra-init]      partition rootfs_a|rootfs_b by GPT name; the Phase 1 name rootfs, then /dev/vda2) → /mnt/root");
        eprintln!("[embra-init]   3. Mount STATE (GPT name state, then /dev/vda3) → /mnt/root/embra/state");
        eprintln!("[embra-init]   4. Mount DATA (GPT name data, then /dev/vda4) → /mnt/root/embra/data");
        eprintln!("[embra-init]   5. Mount ephemeral tmpfs → /mnt/root/embra/ephemeral");
        eprintln!("[embra-init]   6. Switch root to /mnt/root (move the mount, chroot; pivot_root fails from an initramfs)");
        eprintln!("[embra-init]   7. exec /sbin/embrad");
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
fn boot() -> Result<(), Box<dyn std::error::Error>> {
    use embra_boot::{Cmdline, Slot, slot};
    use nix::mount::{mount, MsFlags};
    use nix::unistd::{chdir, execv};
    use std::ffi::CString;

    const NEWROOT: &str = "/mnt/root";

    // Step 1: Mount essential pseudo-filesystems
    eprintln!("[embra-init] Mounting /proc");
    std::fs::create_dir_all("/proc")?;
    mount(Some("proc"), "/proc", Some("proc"), MsFlags::empty(), None::<&str>)?;

    eprintln!("[embra-init] Mounting /sys");
    std::fs::create_dir_all("/sys")?;
    mount(Some("sysfs"), "/sys", Some("sysfs"), MsFlags::empty(), None::<&str>)?;

    eprintln!("[embra-init] Mounting /dev");
    std::fs::create_dir_all("/dev")?;
    mount(Some("devtmpfs"), "/dev", Some("devtmpfs"), MsFlags::empty(), None::<&str>)?;

    // Step 2: Find and mount the SquashFS rootfs. Partitions are found by
    // their GPT names (embra-boot reads the kernel's uevents, no udev): the
    // slot GRUB booted is on the kernel line as embra.slot=a|b, and its
    // partition is rootfs_a or rootfs_b. A line without it (the direct-kernel
    // path of run-qemu.sh, a Phase 1 disk) tries slot A and then the Phase 1
    // name `rootfs`; /dev/vda2 is the last resort. A kernel from one slot
    // never gets the rootfs of another: an invalid slot value is fatal.
    let cmdline = Cmdline::read();
    let booted = cmdline.slot()?;
    let rootfs_names: Vec<&str> = match booted {
        Some(s) => vec![s.partition_name()],
        None => vec![Slot::A.partition_name(), slot::LEGACY_ROOTFS_PARTITION],
    };
    match booted {
        Some(s) => eprintln!("[embra-init] Waiting for block devices (slot {s})..."),
        None => eprintln!("[embra-init] Waiting for block devices (no slot on the kernel line)..."),
    }
    let rootfs_dev = resolve_partition(&rootfs_names, "/dev/vda2")?;

    std::fs::create_dir_all(NEWROOT)?;

    // Read-only, and it stays so: the root filesystem is immutable. What
    // is written at run time lives on STATE, DATA or tmpfs.
    eprintln!("[embra-init] Mounting SquashFS rootfs from {}", rootfs_dev.display());
    mount(
        Some(rootfs_dev.as_path()),
        NEWROOT,
        Some("squashfs"),
        MsFlags::MS_RDONLY,
        None::<&str>,
    )?;

    // Step 3: Mount STATE partition
    let state_path = format!("{}/embra/state", NEWROOT);
    std::fs::create_dir_all(&state_path)?;

    // STATE and DATA hold files, never programs: `nosuid,nodev,noexec`
    // (#6, #29). The Guardian's build writes under DATA and executes
    // nothing there (rustc and rust-lld run from the rootfs), wasmtime
    // runs a tool from memory, and git runs no hook (the hooks path is
    // pinned to an empty directory under STATE).
    let data_flags = MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC;

    let state_dev = resolve_partition(&[slot::STATE_PARTITION], "/dev/vda3")?;
    eprintln!("[embra-init] Mounting STATE partition from {} (nosuid,nodev,noexec)", state_dev.display());
    mount(
        Some(state_dev.as_path()),
        state_path.as_str(),
        Some("ext4"),
        data_flags,
        None::<&str>,
    )?;

    // Step 4: Mount DATA partition
    let data_path = format!("{}/embra/data", NEWROOT);
    std::fs::create_dir_all(&data_path)?;

    let data_dev = resolve_partition(&[slot::DATA_PARTITION], "/dev/vda4")?;
    eprintln!("[embra-init] Mounting DATA partition from {} (nosuid,nodev,noexec)", data_dev.display());
    mount(
        Some(data_dev.as_path()),
        data_path.as_str(),
        Some("ext4"),
        data_flags,
        None::<&str>,
    )?;

    // Step 5: Mount EPHEMERAL (tmpfs). size= is a CAP, not a reservation —
    // pages allocate only on write. 512M (was 256M, 2026-07-31) gives the
    // service logs headroom for their self-diagnostics role (system_logs)
    // on months-long uptimes, while staying at 12.5% of the 4G reference
    // VM; the one realistic fill scenario is a long-lived debug log spec.
    let eph_path = format!("{}/embra/ephemeral", NEWROOT);
    std::fs::create_dir_all(&eph_path)?;
    mount(
        Some("tmpfs"),
        eph_path.as_str(),
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        Some("size=512M"),
    )?;

    // Step 6: Move mount points into new root
    let new_proc = format!("{}/proc", NEWROOT);
    let new_sys = format!("{}/sys", NEWROOT);
    let new_dev = format!("{}/dev", NEWROOT);
    std::fs::create_dir_all(&new_proc)?;
    std::fs::create_dir_all(&new_sys)?;
    std::fs::create_dir_all(&new_dev)?;

    mount(Some("/proc"), new_proc.as_str(), None::<&str>, MsFlags::MS_MOVE, None::<&str>)?;
    mount(Some("/sys"), new_sys.as_str(), None::<&str>, MsFlags::MS_MOVE, None::<&str>)?;
    mount(Some("/dev"), new_dev.as_str(), None::<&str>, MsFlags::MS_MOVE, None::<&str>)?;

    // Step 7: Switch root
    // pivot_root fails with EINVAL from initramfs (rootfs). The standard
    // approach is to chdir to the new root, mount --move it to /, chroot,
    // then exec init. This is what busybox switch_root does.
    eprintln!("[embra-init] Switching root to {}", NEWROOT);
    chdir(NEWROOT)?;
    mount(Some("."), "/", None::<&str>, MsFlags::MS_MOVE, None::<&str>)?;
    nix::unistd::chroot(".")?;
    chdir("/")?;

    // Step 8: exec embrad
    eprintln!("[embra-init] Executing /sbin/embrad");
    let embrad = CString::new("/sbin/embrad")?;
    let args = [embrad.clone()];
    execv(&embrad, &args)?;

    // execv doesn't return on success
    Err("execv returned unexpectedly".into())
}

/// The partition of the first of `names` that appears within five seconds,
/// else `legacy` when that node exists (a disk without GPT names, under a
/// printed warning), else an error naming what was looked for.
#[cfg(target_os = "linux")]
fn resolve_partition(names: &[&str], legacy: &str) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    const WAIT: std::time::Duration = std::time::Duration::from_secs(5);
    if let Some((name, path)) = embra_boot::partitions::wait_for_partition(names, WAIT) {
        eprintln!("[embra-init] partition '{name}' is {}", path.display());
        return Ok(path);
    }
    if std::path::Path::new(legacy).exists() {
        eprintln!("[embra-init] WARNING: no partition named {} within {}s; falling back to {legacy}", names.join(" or "), WAIT.as_secs());
        return Ok(std::path::PathBuf::from(legacy));
    }
    Err(format!("no partition named {} appeared within {}s, and {legacy} does not exist", names.join(" or "), WAIT.as_secs()).into())
}
