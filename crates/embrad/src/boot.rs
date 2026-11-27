//! The A/B boot as embrad sees it: the status file every reader takes the
//! state from (`/run/embra/boot-status`: the brain's `system_status`,
//! embra-update), and the one write embrad makes to the EFI System
//! Partition — confirming a trial boot once every service is healthy.
//!
//! GRUB boots a staged slot with `embra.trial=1` on the kernel line while
//! the slot has tries left (`grub.cfg.in`). A boot that halts before the
//! confirmation consumes a try, which is the point; a boot that reaches it
//! makes the slot the default from then on. A normal boot reads the block
//! for the status and writes nothing.

use embra_boot::{BootMode, BootState, Cmdline, Release, esp, partitions, slot};
use std::fmt;
use std::io;
use std::path::Path;
use tracing::{error, info, warn};

pub const STATUS_PATH: &str = "/run/embra/boot-status";

/// The database's engine marker inside its data directory.
pub const ENGINE_MARKER: &str = ".engine";

/// Removes an empty engine marker, and says whether it did.
///
/// WardSONDB writes `<data dir>/.engine` on the first boot of a DATA
/// partition with one `fs::write` and no fsync, so a stop before the page
/// cache reached the disk (the VM killed within half a minute of its first
/// boot; the first headless boot test of 2026-11-05 did exactly that) leaves
/// the file created and empty, and the database refuses to open: "created
/// with engine '' but 'fjall' was requested". An empty marker guards
/// nothing, so it is removed and the database writes it again. A marker
/// with content, a missing one and a missing directory are left alone.
pub fn remove_empty_engine_marker(data_dir: &Path) -> io::Result<bool> {
    let marker = data_dir.join(ENGINE_MARKER);
    let meta = match std::fs::metadata(&marker) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if !meta.is_file() || meta.len() != 0 {
        return Ok(false);
    }
    std::fs::remove_file(&marker)?;
    Ok(true)
}

/// Where the confirmation stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Confirmed {
    /// The services are not up yet.
    Pending,
    /// A trial boot, confirmed: the slot is the default now.
    Yes,
    /// Not a trial boot (or a boot without a slot): nothing to confirm.
    NotNeeded,
    /// A trial boot whose confirmation could not be written; GRUB retries
    /// while tries last, then falls back.
    Failed(String),
}

impl fmt::Display for Confirmed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Confirmed::Pending => write!(f, "pending"),
            Confirmed::Yes => write!(f, "yes"),
            Confirmed::NotNeeded => write!(f, "not-needed"),
            Confirmed::Failed(reason) => write!(f, "failed: {reason}"),
        }
    }
}

/// The status file's text: `key=value` lines. The slot is `-` on a boot
/// without one; the grubenv fields appear once the block was read.
pub fn status_text(
    cmdline: &Cmdline,
    mode: BootMode,
    confirmed: &Confirmed,
    state: Option<&BootState>,
    release: Option<&Release>,
) -> String {
    let mut out = String::new();
    let slot = cmdline.slot().ok().flatten().map(|s| s.to_string()).unwrap_or_else(|| "-".into());
    out.push_str(&format!("slot={slot}\n"));
    out.push_str(&format!("trial={}\n", u8::from(cmdline.trial())));
    out.push_str(&format!("mode={}\n", mode.name()));
    out.push_str(&format!("confirmed={confirmed}\n"));
    if let Some(r) = release {
        out.push_str(&format!("version={}\nbuild_id={}\narch={}\nengine={}\nbuilt={}\n", r.version, r.build_id, r.arch, r.engine, r.built));
    }
    if let Some(st) = state {
        out.push_str(&format!("active={}\n", st.slot_active));
        out.push_str(&format!("try={}\n", st.slot_try.map(|s| s.to_string()).unwrap_or_else(|| "-".into())));
        out.push_str(&format!("tries_left={}\n", st.tries_left));
        out.push_str(&format!("slot_a_status={}\n", st.slot_a_status.as_str()));
        out.push_str(&format!("slot_b_status={}\n", st.slot_b_status.as_str()));
    }
    out
}

/// Write the status file; a failure is logged, never fatal.
pub fn write_status(cmdline: &Cmdline, mode: BootMode, confirmed: &Confirmed, state: Option<&BootState>, release: Option<&Release>) {
    let text = status_text(cmdline, mode, confirmed, state, release);
    let dir = std::path::Path::new(STATUS_PATH).parent().expect("a path with a parent");
    if let Err(e) = std::fs::create_dir_all(dir).and_then(|()| std::fs::write(STATUS_PATH, text)) {
        warn!("boot status could not be written to {STATUS_PATH}: {e}");
    }
}

/// What a boot says of its disk on a machine that holds more than one with
/// the layout; nothing on a machine with one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiskNote {
    /// The boot menu named its disk, and the partitions in use are that disk's.
    Told(String),
    /// The partitions in use were taken by name: they are the first such
    /// disk's, which may not be the one that was started.
    Untold(String),
}

/// `menu_disk` is the disk that holds the partition the boot menu named
/// (`embra.esp=`), when the line names one and one disk holds it; `root` the
/// disk the root filesystem is on; `carrying` every disk with a partition
/// named like the ESP.
pub fn disk_note(menu_disk: Option<&str>, root: Option<&str>, carrying: &[String]) -> Option<DiskNote> {
    let root = root?;
    if carrying.len() < 2 {
        return None;
    }
    let all = carrying.join(", ");
    Some(if menu_disk == Some(root) {
        DiskNote::Told(format!(
            "{} disks here carry the embraOS layout ({all}); this boot's partitions are {root}'s, the disk its boot menu was read from",
            carrying.len()
        ))
    } else {
        DiskNote::Untold(format!(
            "{} disks here carry the embraOS layout ({all}), and this boot could not tell which one it was started from: its partitions are {root}'s. {}",
            carrying.len(),
            partitions::MENU_REMEDY
        ))
    })
}

/// [`disk_note`] of this machine, into the log.
fn say_of_the_disk(cmdline: &Cmdline) {
    use partitions::BootDisk;
    let menu_disk = cmdline.boot_partition().and_then(|identifier| {
        match partitions::boot_disk_in(Path::new(partitions::SYS_CLASS_BLOCK), Path::new(partitions::SYS_BLOCK), identifier) {
            BootDisk::Named(disk) => Some(disk),
            // A copy of the disk beside it, or no such partition: the menu's word names no one disk.
            BootDisk::NamedOnSeveral { .. } | BootDisk::NotFound => None,
        }
    });
    match disk_note(menu_disk.as_deref(), partitions::root_disk().as_deref(), &partitions::disks_with(slot::ESP_PARTITION)) {
        Some(DiskNote::Told(line)) => info!("{line}"),
        Some(DiskNote::Untold(line)) => warn!("{line}"),
        None => {}
    }
}

/// After every service is healthy. On a trial boot the block on the ESP is
/// rewritten with the booted slot active and the trial cleared; on any
/// other boot with a slot the block is read for the status; a boot without
/// a slot has nothing to read. The outcome goes to the status file.
pub fn after_services_healthy(cmdline: &Cmdline, mode: BootMode) -> Confirmed {
    // What the services wrote while they came up — the first boot's engine
    // marker, the sealed soul, the configuration — is on disk before the
    // slot that produced it is confirmed, and before an operator's stop of
    // the machine can lose it (the page cache holds a write for up to
    // 30 s). SAFETY: sync(2) takes no arguments and cannot fail.
    unsafe { libc::sync() };
    let release = Release::read().ok();
    let Some(booted) = cmdline.slot().ok().flatten() else {
        let c = Confirmed::NotNeeded;
        write_status(cmdline, mode, &c, None, release.as_ref());
        return c;
    };
    say_of_the_disk(cmdline);
    // The ESP of the disk this boot's root filesystem is on: a second disk
    // with the layout has an ESP of the same name, and its block is another
    // installation's.
    let Some(dev) = partitions::find_partition_beside_root(slot::ESP_PARTITION) else {
        let c = if cmdline.trial() {
            error!("slot {booted} cannot be confirmed: no partition named {}", slot::ESP_PARTITION);
            Confirmed::Failed(format!("no partition named {}", slot::ESP_PARTITION))
        } else {
            warn!("boot status without the ESP: no partition named {}", slot::ESP_PARTITION);
            Confirmed::NotNeeded
        };
        write_status(cmdline, mode, &c, None, release.as_ref());
        return c;
    };
    if cmdline.trial() {
        let written = esp::with_env_mut(&dev, |env| {
            let mut st = BootState::from_env(env);
            st.confirm(booted);
            st.apply(env).map(|()| st)
        });
        let (c, state) = match written {
            Ok(Ok(st)) => {
                info!("slot {booted} confirmed: the default from now on (grubenv on {})", dev.display());
                (Confirmed::Yes, Some(st))
            }
            Ok(Err(e)) => (Confirmed::Failed(e.to_string()), None),
            Err(e) => (Confirmed::Failed(e.to_string()), None),
        };
        if let Confirmed::Failed(reason) = &c {
            error!("slot {booted} could not be confirmed: {reason}; GRUB retries while tries last, then falls back to the previous slot");
        }
        write_status(cmdline, mode, &c, state.as_ref(), release.as_ref());
        c
    } else {
        let state = match esp::with_env(&dev, BootState::from_env) {
            Ok(st) => Some(st),
            Err(e) => {
                warn!("grubenv on {} could not be read for the boot status: {e}", dev.display());
                None
            }
        };
        let c = Confirmed::NotNeeded;
        write_status(cmdline, mode, &c, state.as_ref(), release.as_ref());
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embra_boot::{Slot, SlotStatus};

    #[test]
    fn the_boot_status_file_names_the_slot_the_trial_and_the_mode() {
        let cmdline = Cmdline::parse("console=ttyS0 embra.slot=b embra.trial=1 embra.web=1");
        let mut state = BootState::default();
        state.stage(Slot::B);
        state.confirm(Slot::B);
        let release = Release::parse("VERSION=0.24.0-phase2\nBUILD_ID=abc\nARCH=x86_64\nENGINE=fjall\nBUILT=2026-11-05T00:00:00Z\n").unwrap();
        let text = status_text(&cmdline, BootMode::Normal, &Confirmed::Yes, Some(&state), Some(&release));
        let want = "slot=b\ntrial=1\nmode=normal\nconfirmed=yes\nversion=0.24.0-phase2\nbuild_id=abc\narch=x86_64\nengine=fjall\nbuilt=2026-11-05T00:00:00Z\nactive=b\ntry=-\ntries_left=0\nslot_a_status=\nslot_b_status=ok\n";
        assert_eq!(text, want);
        assert_eq!(state.status_of(Slot::B), SlotStatus::Ok);

        let slotless = Cmdline::parse("console=ttyS0 root=/dev/vda2 ro quiet");
        let text = status_text(&slotless, BootMode::Update, &Confirmed::Failed("no esp".into()), None, None);
        assert_eq!(text, "slot=-\ntrial=0\nmode=update\nconfirmed=failed: no esp\n");
        assert_eq!(Confirmed::Pending.to_string(), "pending");
        assert_eq!(Confirmed::NotNeeded.to_string(), "not-needed");
    }

    #[test]
    fn an_empty_engine_marker_is_removed_and_a_written_one_is_kept() {
        let dir = std::env::temp_dir().join(format!("embrad-engine-marker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // No directory, no marker: nothing to do, no error.
        assert!(!remove_empty_engine_marker(&dir.join("absent")).unwrap());
        assert!(!remove_empty_engine_marker(&dir).unwrap());
        // The trace of an unclean first boot: created, empty.
        let marker = dir.join(ENGINE_MARKER);
        std::fs::write(&marker, "").unwrap();
        assert!(remove_empty_engine_marker(&dir).unwrap());
        assert!(!marker.exists());
        // A marker the database finished writing stays.
        std::fs::write(&marker, "fjall").unwrap();
        assert!(!remove_empty_engine_marker(&dir).unwrap());
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "fjall");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// One disk with the layout: nothing is said. Two: which disk this
    /// boot's partitions are on, as information when the boot menu named
    /// that disk and as a warning when the partitions were taken by name.
    #[test]
    fn a_second_disk_with_the_layout_is_said_and_a_boot_that_could_not_tell_them_apart_is_a_warning() {
        let one = ["nvme0n1".to_string()];
        let two = ["nvme0n1".to_string(), "nvme1n1".to_string()];
        assert_eq!(disk_note(Some("nvme0n1"), Some("nvme0n1"), &one), None);
        assert_eq!(disk_note(None, Some("nvme0n1"), &one), None);
        assert_eq!(disk_note(None, Some("nvme0n1"), &[]), None, "a Phase 1 disk has no partition of the name");
        assert_eq!(disk_note(Some("nvme1n1"), None, &two), None, "a development run has no disk under its root");
        // The menu named the disk the root is on: the second disk's install, started and running.
        match disk_note(Some("nvme1n1"), Some("nvme1n1"), &two) {
            Some(DiskNote::Told(line)) => assert_eq!(
                line,
                "2 disks here carry the embraOS layout (nvme0n1, nvme1n1); this boot's partitions are nvme1n1's, the disk its boot menu was read from"
            ),
            other => panic!("{other:?}"),
        }
        // No word from the menu, or a word that names another disk than the one in use: a warning that says what to do.
        for menu_disk in [None, Some("nvme1n1")] {
            match disk_note(menu_disk, Some("nvme0n1"), &two) {
                Some(DiskNote::Untold(line)) => assert_eq!(
                    line,
                    "2 disks here carry the embraOS layout (nvme0n1, nvme1n1), and this boot could not tell which one it was started from: \
                     its partitions are nvme0n1's. The boot menu of a current release names its disk; an upgrade in place from the installation medium writes that menu"
                ),
                other => panic!("{menu_disk:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn a_boot_without_a_slot_runs_no_slot_logic() {
        // No slot on the line: nothing is confirmed and no device is looked for,
        // on a host without an ESP as much as in the OS.
        let cmdline = Cmdline::parse("console=ttyS0 embra.web=1");
        assert_eq!(after_services_healthy(&cmdline, BootMode::Normal), Confirmed::NotNeeded);
    }
}
