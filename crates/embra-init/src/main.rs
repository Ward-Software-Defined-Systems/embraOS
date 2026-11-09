//! embra-init — Initramfs early boot for embraOS.
//!
//! This is a minimal, synchronous Rust binary. No tokio, no async.
//! It must work before the full OS is available.
//!
//! Every line it prints goes to both boot consoles — the serial line, which
//! `/dev/console` is, and the display (`embra_boot::console::BootConsole`):
//! a VM with a display and no serial port sees why a boot stopped.

#[cfg(target_os = "linux")]
use embra_boot::console::BootConsole;

/// One line on both consoles, prefixed like every embra-init line.
#[cfg(target_os = "linux")]
macro_rules! say {
    ($console:expr, $($arg:tt)*) => {
        $console.line(&format!("[embra-init] {}", format_args!($($arg)*)))
    };
}

fn main() {
    #[cfg(target_os = "linux")]
    {
        // The pseudo-filesystems come first and silently: /dev/tty0 exists
        // once devtmpfs is mounted, and the console opens after that.
        let mut console = match early_mounts() {
            Ok(()) => BootConsole::open(),
            Err(e) => fatal(&mut BootConsole::open(), &e),
        };
        say!(console, "embraOS early boot starting");
        say!(console, "Mounted /proc, /sys and /dev");
        if let Err(e) = boot(&mut console) {
            fatal(&mut console, &e);
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
        eprintln!("[embra-init] With embra.mode=install: the installation medium is found by its manifest, the rootfs it");
        eprintln!("[embra-init]   carries is the root (a loop device), STATE and DATA are tmpfs, the medium stays at /run/embra/media");
        std::process::exit(1);
    }
}

/// The reason on both consoles, then the halt: an initramfs that cannot
/// mount its root has nothing to wait for, and a halt is what the operator
/// sees in QEMU, where a sleep forever looked like a hang (#29).
#[cfg(target_os = "linux")]
fn fatal(console: &mut BootConsole, e: &dyn std::fmt::Display) -> ! {
    say!(console, "FATAL: {e}");
    say!(console, "Halting: the boot cannot continue");
    std::thread::sleep(std::time::Duration::from_secs(2));
    let _ = nix::sys::reboot::reboot(nix::sys::reboot::RebootMode::RB_HALT_SYSTEM);
    // reboot(2) returned: stay here rather than exit, which would
    // panic the kernel ("Attempted to kill init!").
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

/// Step 1: the essential pseudo-filesystems. Before the console: the error
/// names the mount that failed.
#[cfg(target_os = "linux")]
fn early_mounts() -> Result<(), String> {
    use nix::mount::{mount, MsFlags};
    for (source, target, fstype) in [("proc", "/proc", "proc"), ("sysfs", "/sys", "sysfs"), ("devtmpfs", "/dev", "devtmpfs")] {
        std::fs::create_dir_all(target).map_err(|e| format!("mkdir {target}: {e}"))?;
        mount(Some(source), target, Some(fstype), MsFlags::empty(), None::<&str>).map_err(|e| format!("mount {target}: {e}"))?;
    }
    Ok(())
}

/// Where the root to be is assembled before the switch.
#[cfg(target_os = "linux")]
const NEWROOT: &str = "/mnt/root";

/// STATE and DATA hold files, never programs: `nosuid,nodev,noexec` (#6,
/// #29). The Guardian's build writes under DATA and executes nothing there
/// (rustc and rust-lld run from the rootfs), wasmtime runs a tool from
/// memory, and git runs no hook (the hooks path is pinned to an empty
/// directory under STATE).
#[cfg(target_os = "linux")]
const DATA_FLAGS: nix::mount::MsFlags = nix::mount::MsFlags::MS_NOSUID.union(nix::mount::MsFlags::MS_NODEV).union(nix::mount::MsFlags::MS_NOEXEC);

#[cfg(target_os = "linux")]
fn boot(console: &mut BootConsole) -> Result<(), Box<dyn std::error::Error>> {
    use embra_boot::{BootMode, Cmdline, Slot, slot};
    use nix::mount::{mount, MsFlags};

    let cmdline = Cmdline::read();
    // The installer's boot: the rootfs the medium carries, STATE and DATA
    // on tmpfs, the medium kept for the installer. An unknown mode value is
    // embrad's to report; a disk boot goes on below.
    if cmdline.mode().ok() == Some(BootMode::Install) {
        return install_boot(console, &cmdline);
    }

    // Step 2: Find and mount the SquashFS rootfs. Partitions are found by
    // their GPT names (embra-boot reads the kernel's uevents, no udev): the
    // slot GRUB booted is on the kernel line as embra.slot=a|b, and its
    // partition is rootfs_a or rootfs_b. A line without it (the direct-kernel
    // path of run-qemu.sh, a Phase 1 disk) tries slot A and then the Phase 1
    // name `rootfs`; /dev/vda2 is the last resort. A kernel from one slot
    // never gets the rootfs of another: an invalid slot value is fatal.
    let booted = cmdline.slot()?;
    let rootfs_names: Vec<&str> = match booted {
        Some(s) => vec![s.partition_name()],
        None => vec![Slot::A.partition_name(), slot::LEGACY_ROOTFS_PARTITION],
    };
    match booted {
        Some(s) => say!(console, "Waiting for block devices (slot {s})..."),
        None => say!(console, "Waiting for block devices (no slot on the kernel line)..."),
    }
    let rootfs_dev = resolve_partition(console, &rootfs_names, "/dev/vda2")?;

    std::fs::create_dir_all(NEWROOT)?;

    // Read-only, and it stays so: the root filesystem is immutable. What
    // is written at run time lives on STATE, DATA or tmpfs.
    say!(console, "Mounting SquashFS rootfs from {}", rootfs_dev.display());
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
    let data_flags = DATA_FLAGS;

    let state_dev = resolve_partition(console, &[slot::STATE_PARTITION], "/dev/vda3")?;
    say!(console, "Mounting STATE partition from {} (nosuid,nodev,noexec)", state_dev.display());
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

    let data_dev = resolve_partition(console, &[slot::DATA_PARTITION], "/dev/vda4")?;
    say!(console, "Mounting DATA partition from {} (nosuid,nodev,noexec)", data_dev.display());
    mount(
        Some(data_dev.as_path()),
        data_path.as_str(),
        Some("ext4"),
        data_flags,
        None::<&str>,
    )?;

    switch_and_exec(console)
}

/// The installer's boot (`embra.mode=install`): the installation medium is
/// found by trying every block device as ISO 9660 until one carries
/// /embraos/install.manifest (a USB stick enumerates seconds after the
/// kernel starts, so up to 30 s; `embra.media=<device>` names one), the
/// rootfs squashfs it carries is attached to a loop device and mounted as
/// the root, STATE and DATA are tmpfs (nothing of this boot persists), and
/// the medium moves under the new root at /run/embra/media, where embrad's
/// install mode hands it to the installer.
#[cfg(target_os = "linux")]
fn install_boot(console: &mut BootConsole, cmdline: &embra_boot::Cmdline) -> Result<(), Box<dyn std::error::Error>> {
    use embra_boot::{loopdev, media, partitions};
    use nix::mount::{mount, MsFlags};
    use std::path::Path;
    use std::time::Duration;

    const MEDIA: &str = "/media";
    const WAIT: Duration = Duration::from_secs(30);

    match cmdline.media() {
        Some(dev) => say!(console, "Installer boot: looking for the installation medium on {dev}..."),
        None => say!(console, "Installer boot: looking for the installation medium (every block device, up to {} s)...", WAIT.as_secs()),
    }
    let found = {
        let mut say_line = |l: &str| say!(console, "{l}");
        media::probe(Path::new(partitions::SYS_CLASS_BLOCK), Path::new(partitions::DEV), Path::new(MEDIA), WAIT, cmdline.media(), &mut say_line)
    }
    .ok_or_else(|| format!("no installation medium: no block device carries /{} within {} s", media::MANIFEST_RELATIVE, WAIT.as_secs()))?;
    let m = &found.manifest;
    say!(console, "Installation medium: {} (embraOS {} {}, {} {})", found.device.display(), m.version, m.build_id, m.arch, m.engine);

    // The medium's rootfs, on a loop device, as the root.
    let squash = Path::new(MEDIA).join(&m.rootfs);
    let loop_dev = loopdev::attach_read_only(&squash).map_err(|e| format!("loop device over {}: {e}", squash.display()))?;
    std::fs::create_dir_all(NEWROOT)?;
    say!(console, "Mounting the medium's rootfs ({}) from {}", m.rootfs, loop_dev.display());
    mount(Some(loop_dev.as_path()), NEWROOT, Some("squashfs"), MsFlags::MS_RDONLY, None::<&str>)?;

    // STATE and DATA: tmpfs, the flags of a disk boot.
    for (sub, size) in [("state", "size=64M"), ("data", "size=256M")] {
        let path = format!("{NEWROOT}/embra/{sub}");
        std::fs::create_dir_all(&path)?;
        mount(Some("tmpfs"), path.as_str(), Some("tmpfs"), DATA_FLAGS, Some(size))?;
    }
    say!(console, "STATE and DATA are tmpfs for this boot: nothing of it persists");

    // The medium moves under the new root; embrad's own /run mount is
    // skipped when /run is mounted already, so it survives.
    let run = format!("{NEWROOT}/run");
    std::fs::create_dir_all(&run)?;
    mount(Some("tmpfs"), run.as_str(), Some("tmpfs"), MsFlags::MS_NOSUID | MsFlags::MS_NODEV, None::<&str>)?;
    let media_in_root = format!("{NEWROOT}/run/embra/media");
    std::fs::create_dir_all(&media_in_root)?;
    mount(Some(MEDIA), media_in_root.as_str(), None::<&str>, MsFlags::MS_MOVE, None::<&str>)?;
    say!(console, "The medium stays mounted at /run/embra/media for the installer");

    switch_and_exec(console)
}

/// The tail every boot shares: the ephemeral tmpfs, the pseudo-filesystems
/// moved into the new root, the switch, embrad.
#[cfg(target_os = "linux")]
fn switch_and_exec(console: &mut BootConsole) -> Result<(), Box<dyn std::error::Error>> {
    use nix::mount::{mount, MsFlags};
    use nix::unistd::{chdir, execv};
    use std::ffi::CString;

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
    say!(console, "Switching root to {}", NEWROOT);
    chdir(NEWROOT)?;
    mount(Some("."), "/", None::<&str>, MsFlags::MS_MOVE, None::<&str>)?;
    nix::unistd::chroot(".")?;
    chdir("/")?;

    // Step 8: exec embrad
    say!(console, "Executing /sbin/embrad");
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
fn resolve_partition(console: &mut BootConsole, names: &[&str], legacy: &str) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    const WAIT: std::time::Duration = std::time::Duration::from_secs(5);
    if let Some((name, path)) = embra_boot::partitions::wait_for_partition(names, WAIT) {
        say!(console, "partition '{name}' is {}", path.display());
        return Ok(path);
    }
    if std::path::Path::new(legacy).exists() {
        say!(console, "WARNING: no partition named {} within {}s; falling back to {legacy}", names.join(" or "), WAIT.as_secs());
        return Ok(std::path::PathBuf::from(legacy));
    }
    Err(format!("no partition named {} appeared within {}s, and {legacy} does not exist", names.join(" or "), WAIT.as_secs()).into())
}
