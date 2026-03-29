//! embra-init — Initramfs early boot for embraOS.
//!
//! This is a minimal, synchronous Rust binary. No tokio, no async.
//! It must work before the full OS is available.

fn main() {
    eprintln!("[embra-init] embraOS early boot starting");

    #[cfg(target_os = "linux")]
    {
        if let Err(e) = boot() {
            eprintln!("[embra-init] FATAL: {}", e);
            eprintln!("[embra-init] Dropping to emergency halt");
            loop { std::thread::sleep(std::time::Duration::from_secs(3600)); }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("[embra-init] Not running on Linux — this binary is for initramfs only");
        eprintln!("[embra-init] Boot sequence would be:");
        eprintln!("[embra-init]   1. Mount /proc, /sys, /dev");
        eprintln!("[embra-init]   2. Mount SquashFS rootfs from /dev/vda2 → /mnt/root");
        eprintln!("[embra-init]   3. Mount STATE from /dev/vda3 → /mnt/root/embra/state");
        eprintln!("[embra-init]   4. Mount DATA from /dev/vda4 → /mnt/root/embra/data");
        eprintln!("[embra-init]   5. Mount ephemeral tmpfs → /mnt/root/embra/ephemeral");
        eprintln!("[embra-init]   6. pivot_root /mnt/root");
        eprintln!("[embra-init]   7. exec /sbin/embrad");
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
fn boot() -> Result<(), Box<dyn std::error::Error>> {
    use nix::mount::{mount, MsFlags};
    use nix::unistd::{chdir, execv};
    use std::ffi::CString;
    use std::path::Path;

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

    // Step 2: Find and mount the SquashFS rootfs
    // Disk layout: /dev/vda1 = boot, /dev/vda2 = SquashFS rootfs,
    //              /dev/vda3 = STATE (ext4), /dev/vda4 = DATA (ext4)
    eprintln!("[embra-init] Waiting for block devices...");
    wait_for_device("/dev/vda2", 5)?;

    std::fs::create_dir_all(NEWROOT)?;

    eprintln!("[embra-init] Mounting SquashFS rootfs from /dev/vda2");
    mount(
        Some("/dev/vda2"),
        NEWROOT,
        Some("squashfs"),
        MsFlags::MS_RDONLY,
        None::<&str>,
    )?;

    // Step 3: Mount STATE partition
    let state_path = format!("{}/embra/state", NEWROOT);
    std::fs::create_dir_all(&state_path)?;

    eprintln!("[embra-init] Mounting STATE partition from /dev/vda3");
    wait_for_device("/dev/vda3", 5)?;
    mount(
        Some("/dev/vda3"),
        state_path.as_str(),
        Some("ext4"),
        MsFlags::empty(),
        None::<&str>,
    )?;

    // Step 4: Mount DATA partition
    let data_path = format!("{}/embra/data", NEWROOT);
    std::fs::create_dir_all(&data_path)?;

    eprintln!("[embra-init] Mounting DATA partition from /dev/vda4");
    wait_for_device("/dev/vda4", 5)?;
    mount(
        Some("/dev/vda4"),
        data_path.as_str(),
        Some("ext4"),
        MsFlags::empty(),
        None::<&str>,
    )?;

    // Step 5: Mount EPHEMERAL (tmpfs)
    let eph_path = format!("{}/embra/ephemeral", NEWROOT);
    std::fs::create_dir_all(&eph_path)?;
    mount(
        Some("tmpfs"),
        eph_path.as_str(),
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some("size=256M"),
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

#[cfg(target_os = "linux")]
fn wait_for_device(path: &str, timeout_secs: u64) -> Result<(), Box<dyn std::error::Error>> {
    use std::path::Path;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    while std::time::Instant::now() < deadline {
        if Path::new(path).exists() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(format!("Device {} did not appear within {}s", path, timeout_secs).into())
}
