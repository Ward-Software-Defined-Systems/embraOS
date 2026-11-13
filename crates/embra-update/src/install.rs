//! `install`: embraOS onto a local disk from the installation medium, or
//! into the installation the disk already holds.
//!
//! A clean install gives the disk the layout's five partitions with DATA to
//! its end (sgdisk), the ESP formatted and filled from the medium's tree with
//! a fresh grubenv that makes slot A active, rootfs_a streamed from the
//! medium's squashfs and verified like a release (`slot::write_image`), STATE
//! and DATA formatted. The phases run in order and stop at the first failure,
//! after which the disk holds no bootable system and the installer says so.
//! The operator is asked twice, the disk's number and then its name, unless
//! `--yes --disk`.
//!
//! An upgrade in place leaves the partitions, STATE and DATA as they are: the
//! medium's squashfs goes into the slot that is not the default, the ESP's
//! loader and `grub.cfg` are replaced with the medium's while `grubenv`
//! stays, and the slot is staged there as an OS update stages one, so the
//! next boot is a trial with the snapshot and the fallback an update has.
//! The installer reads what the chosen disk holds before it asks anything
//! (`probe`), offers the upgrade beside the clean install when the
//! installation can take the medium's release (`offer`), and writes nothing
//! before an answer. `--upgrade` asks for the upgrade outright and never
//! becomes a clean install.

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use embra_boot::media::InstallManifest;
use embra_boot::{BootState, BootStatus, GrubEnv, Release, Slot, SlotStatus, disks, esp, layout, partitions, slot::PARTITION_NAMES};

use crate::Outcome;
use crate::console::Operator;
use crate::manifest::{self, Verdict};

/// The phases, in the order they run; the real steps touch the disk, the
/// tests record them.
pub trait Steps {
    fn zap(&mut self, disk: &str) -> Result<(), String>;
    fn partition(&mut self, disk: &str) -> Result<(), String>;
    fn format_esp(&mut self, part: &str) -> Result<(), String>;
    /// The medium's rootfs into `part`, verified; `name` is the partition's
    /// GPT name, for the lines that say how far it is.
    fn write_rootfs(&mut self, part: &str, name: &str, op: &mut Operator) -> Result<(), String>;
    fn format_ext4(&mut self, part: &str, label: &str) -> Result<(), String>;
    fn boot_files(&mut self, part: &str) -> Result<(), String>;
    fn verify(&mut self, disk: &str, rootfs_part: &str, esp_part: &str) -> Result<String, String>;
    /// An upgrade: the ESP's loader and `grub.cfg` replaced with the
    /// medium's; `grubenv` stays.
    fn refresh_boot_files(&mut self, part: &str) -> Result<(), String>;
    /// An upgrade: `slot` staged in the ESP's `grubenv`; the line that says so.
    fn stage(&mut self, esp_part: &str, slot: Slot) -> Result<String, String>;
    /// An upgrade: the slot, the boot state and the boot files, read back.
    fn verify_upgrade(&mut self, slot: Slot, rootfs_part: &str, esp_part: &str) -> Result<String, String>;
}

/// The phases on `disk`, each announced; `Err` names the phase and its reason.
pub fn run_phases(steps: &mut dyn Steps, disk: &str, op: &mut Operator) -> Result<String, String> {
    let dev = |n: u32| format!("/dev/{}", layout::partition_dev(disk, n));
    let [esp, rootfs_a, _rootfs_b, state, data] = [dev(1), dev(2), dev(3), dev(4), dev(5)];
    let starts = layout::planned_starts_mib();
    op.line(&format!(
        "partition table: esp {} MiB, rootfs_a {} MiB, rootfs_b {} MiB, state {} MiB, data the rest (from {} MiB)",
        layout::ESP_MIB, layout::SLOT_MIB, layout::SLOT_MIB, layout::STATE_MIB, starts[4]
    ));
    steps.zap(disk).map_err(|e| format!("zap: {e}"))?;
    steps.partition(disk).map_err(|e| format!("partition table: {e}"))?;
    op.line(&format!("esp: FAT16, label ESP ({esp})"));
    steps.format_esp(&esp).map_err(|e| format!("esp: {e}"))?;
    op.line(&format!("rootfs_a: the medium's rootfs → {rootfs_a}"));
    steps.write_rootfs(&rootfs_a, "rootfs_a", op).map_err(|e| format!("rootfs_a: {e}"))?;
    op.line(&format!("state: ext4, label STATE ({state})"));
    steps.format_ext4(&state, "STATE").map_err(|e| format!("state: {e}"))?;
    op.line(&format!("data: ext4, label DATA ({data})"));
    steps.format_ext4(&data, "DATA").map_err(|e| format!("data: {e}"))?;
    op.line(&format!("boot files: GRUB's loader, grub.cfg, grubenv (slot A active) in EFI/BOOT → {esp}"));
    steps.boot_files(&esp).map_err(|e| format!("boot files: {e}"))?;
    let summary = steps.verify(disk, &rootfs_a, &esp).map_err(|e| format!("verify: {e}"))?;
    op.line(&format!("verify: {summary}"));
    Ok(summary)
}

/// A slot's letter as the installer says it (`A`, `B`).
fn up(slot: Slot) -> char {
    slot.letter().to_ascii_uppercase()
}

/// A slot's place in a pair held as `[slot A, slot B]`.
fn index(slot: Slot) -> usize {
    match slot {
        Slot::A => 0,
        Slot::B => 1,
    }
}

/// A slot's partition number in the layout: rootfs_a is the second
/// partition, rootfs_b the third.
fn slot_partition_number(slot: Slot) -> u32 {
    index(slot) as u32 + 2
}

/// The upgrade's phases on `disk`: the medium's rootfs into `target`'s
/// partition, the boot files, the staging, the read-back. Nothing touches
/// the partition table, STATE or DATA. `Err` names the phase, its reason and
/// what the disk boots now.
pub fn run_upgrade_phases(steps: &mut dyn Steps, disk: &str, target: Slot, op: &mut Operator) -> Result<String, String> {
    let active = target.other();
    let dev = |n: u32| format!("/dev/{}", layout::partition_dev(disk, n));
    let esp = dev(1);
    let rootfs = dev(slot_partition_number(target));
    let name = target.partition_name();
    let (t, a) = (up(target), up(active));
    op.line(&format!("{name}: the medium's rootfs → {rootfs}"));
    steps
        .write_rootfs(&rootfs, name, op)
        .map_err(|e| format!("{name}: {e}; slot {t} is not staged and the disk boots slot {a} as before"))?;
    op.line(&format!("boot files: GRUB's loader and grub.cfg from the medium → {esp}; grubenv stays"));
    steps
        .refresh_boot_files(&esp)
        .map_err(|e| format!("boot files: {e}; slot {t} is written and verified but not staged"))?;
    let staged = steps
        .stage(&esp, target)
        .map_err(|e| format!("grubenv: {e}; slot {t} is written and verified but not staged, and the disk boots slot {a} as before"))?;
    op.line(&format!("staged: {staged}"));
    let summary = steps.verify_upgrade(target, &rootfs, &esp).map_err(|e| format!("verify: {e}"))?;
    op.line(&format!("verify: {summary}"));
    Ok(summary)
}

/// The mount source of `mountpoint` in `/proc/mounts` text (`/dev/sr0`).
pub fn mount_source(mounts: &str, mountpoint: &str) -> Option<String> {
    mounts
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let src = f.next()?;
            let mnt = f.next()?;
            (mnt == mountpoint).then(|| src.to_string())
        })
        .next_back()
}

/// The disk chosen: `wanted` (`--disk`) must be offered, else the operator
/// picks a number from the list. `Err` is the refusal. Nothing is confirmed
/// here: what the disk holds decides what is asked next (`plan`).
pub fn pick_disk(disks: &[disks::Disk], wanted: Option<&str>, yes: bool, op: &mut Operator) -> Result<disks::Disk, Outcome> {
    let offered: Vec<&disks::Disk> = disks.iter().filter(|d| d.offered()).collect();
    if let Some(name) = wanted {
        return match disks.iter().find(|d| d.name == name) {
            Some(d) if d.offered() => Ok(d.clone()),
            Some(d) => Err(Outcome::Refused(format!("refused: {name} is not a target: {}", d.excluded.clone().unwrap_or_default()))),
            None => Err(Outcome::Refused(format!("refused: no disk named {name}"))),
        };
    }
    if yes {
        return Err(Outcome::Refused("refused: --yes needs --disk <name>; without it the installer asks".into()));
    }
    op.line("Disks:");
    let mut numbered: Vec<&disks::Disk> = Vec::new();
    for d in disks {
        match &d.excluded {
            None => {
                numbered.push(d);
                op.line(&format!("  [{}] {:<10} {:>10}  {}  {}", numbered.len(), d.name, d.size_text(), d.model, d.bus));
            }
            Some(why) => op.line(&format!("      {:<10} {:>10}  {}", d.name, d.size_text(), why)),
        }
    }
    if offered.is_empty() {
        return Err(Outcome::Refused("refused: no disk to install on".into()));
    }
    let Some(first) = op.ask(&format!("Install to which disk? [1-{}, q = reboot without installing]: ", numbered.len())) else {
        return Err(Outcome::Refused("install: nobody answered; nothing was changed".into()));
    };
    let Some(idx) = first.trim().parse::<usize>().ok().filter(|n| (1..=numbered.len()).contains(n)) else {
        return Err(Outcome::Refused("install: nothing chosen; nothing was changed".into()));
    };
    Ok(numbered[idx - 1].clone())
}

/// The clean install's confirmation: the disk's name, typed.
pub fn confirm_erase(disk: &disks::Disk, op: &mut Operator) -> Result<(), Outcome> {
    let Some(second) = op.ask(&format!(
        "Everything on {} ({}, {}) is erased. Type its name ({}) to confirm: ",
        disk.name, disk.size_text(), disk.model, disk.name
    )) else {
        return Err(Outcome::Refused("install: nobody confirmed; nothing was changed".into()));
    };
    if second.trim() != disk.name {
        return Err(Outcome::Refused("install: not confirmed; nothing was changed".into()));
    }
    Ok(())
}

/// What the command line asks for.
#[derive(Clone, Copy, Debug, Default)]
pub struct Wanted<'a> {
    /// `--disk <name>`.
    pub disk: Option<&'a str>,
    /// `--yes`: nothing is asked.
    pub yes: bool,
    /// `--upgrade`: the installation the disk holds is upgraded in place, or
    /// the run is refused; never a clean install.
    pub upgrade: bool,
    /// `--allow-downgrade`: an upgrade in place may write an older release.
    pub allow_downgrade: bool,
}

/// What the chosen disk holds, read before anything is asked or written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// Not the layout's five partitions by name: no installation.
    Nothing,
    /// The layout is there and the boot state on its ESP cannot be read.
    Unreadable(String),
    Installed(Box<Installed>),
}

/// An installation: the boot state, what each slot holds and how large its
/// partition is, the database's engine, and what the default slot lacks of
/// the files the medium's boot menu reads from a slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub state: BootState,
    /// What slot A and slot B hold.
    pub releases: [Option<Release>; 2],
    /// The size of slot A's and of slot B's partition.
    pub slot_bytes: [u64; 2],
    /// DATA's engine marker, when DATA has one.
    pub data_engine: Option<String>,
    /// The files the medium's boot menu reads from a slot that the default
    /// slot does not hold; `Err` when the menu could not be read for them.
    pub fallback_lacks: Result<Vec<String>, String>,
}

impl Installed {
    /// The slot GRUB boots by default; the fallback of an upgrade.
    pub fn active(&self) -> Slot {
        self.state.slot_active
    }

    /// The slot an upgrade writes: the one that is not the default.
    pub fn target(&self) -> Slot {
        self.active().other()
    }

    fn release(&self, slot: Slot) -> Option<&Release> {
        self.releases[index(slot)].as_ref()
    }

    /// What the disk holds, in one line.
    pub fn describe(&self, disk: &str) -> String {
        let slot_text = |slot: Slot| {
            let mut text = match self.release(slot) {
                Some(r) => format!("slot {} holds {} (build {}, {})", up(slot), r.version, r.build_id, r.engine),
                None => format!("slot {} holds no release", up(slot)),
            };
            if slot == self.active() {
                text.push_str(", the default");
            }
            if self.state.slot_try == Some(slot) {
                text.push_str(&format!(", staged with {} tries left", self.state.tries_left));
            } else if self.state.status_of(slot) == SlotStatus::Failed {
                text.push_str(", its trial failed");
            }
            text
        };
        format!("{disk} holds an embraOS installation: {}; {}.", slot_text(Slot::A), slot_text(Slot::B))
    }
}

/// What an upgrade in place would be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The medium's version is above the installed one.
    Newer,
    /// The same version, another build.
    OtherBuild,
    /// An older version, allowed by `--allow-downgrade`.
    Downgrade,
}

/// Whether the installation can take the medium's release in place, and as
/// what. `Err` is the reason it cannot, a clause that completes "an upgrade
/// in place is not offered: ". The checks are the OS update's (`update.rs`):
/// the architecture, the engine of the data over the release's, the slot's
/// size, the version; and one of the installer's own, since it replaces the
/// boot menu: the default slot, the fallback, holds every file the medium's
/// menu reads from a slot.
pub fn offer(inst: &Installed, m: &InstallManifest, allow_downgrade: bool) -> Result<Kind, String> {
    let (active, target) = (inst.active(), inst.target());
    let Some(installed) = inst.release(active) else {
        return Err(format!("slot {}, the default, holds no release to upgrade from", up(active)));
    };
    if let Some(staged) = inst.release(target)
        && inst.state.slot_try == Some(target)
        && staged.version == m.version
        && staged.build_id == m.build_id
    {
        return Err(format!(
            "slot {} already holds this build ({}, {}), staged and not booted yet",
            up(target),
            staged.version,
            staged.build_id
        ));
    }
    if m.arch != installed.arch {
        return Err(format!("the medium is {} and the installed OS {}", m.arch, installed.arch));
    }
    let engine = inst.data_engine.as_deref().unwrap_or(installed.engine.as_str());
    if m.engine != engine {
        return Err(format!("the medium is built for the {} engine and the disk's database is {engine}", m.engine));
    }
    let room = inst.slot_bytes[index(target)];
    if m.rootfs_size > room {
        return Err(format!(
            "the medium's image is {} bytes and slot {} is {room}: this disk's slots are too small for it",
            m.rootfs_size,
            up(target)
        ));
    }
    match &inst.fallback_lacks {
        Err(why) => return Err(format!("the medium's boot menu cannot be read for the files it takes from a slot ({why})")),
        Ok(lacks) if !lacks.is_empty() => {
            return Err(format!(
                "slot {} has no {}, which the boot menu this medium installs reads from a slot: the menu could not boot it as the fallback",
                up(active),
                lacks.join(", ")
            ));
        }
        Ok(_) => {}
    }
    match manifest::version_verdict(&m.version, &m.build_id, &installed.version, &installed.build_id) {
        Verdict::Newer => Ok(Kind::Newer),
        Verdict::SameVersionOtherBuild => Ok(Kind::OtherBuild),
        Verdict::Older if allow_downgrade => Ok(Kind::Downgrade),
        Verdict::Older => Err(format!(
            "the medium's {} is older than slot {}'s {}, and an older OS halts on a newer knowledge-graph schema (embra.downgrade=1 on the installer's kernel line allows it)",
            m.version,
            up(active),
            installed.version
        )),
        Verdict::SameBuild => Err(format!("slot {} already holds this build ({}, {})", up(active), installed.version, installed.build_id)),
        Verdict::Mismatch(why) => Err(why),
    }
}

/// What the installer does with the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// Everything on the disk is replaced.
    Clean,
    /// The medium's OS into `target`, staged; STATE and DATA stay.
    Upgrade { target: Slot },
}

/// The medium's release against the default slot's, as the menu and the
/// confirmation say it.
fn kind_sentence(kind: Kind, installed: &Release, active: Slot) -> String {
    match kind {
        Kind::Newer => format!("newer than slot {}'s {}", up(active), installed.version),
        Kind::OtherBuild => format!("the same version as slot {}'s, another build (slot {} holds {})", up(active), up(active), installed.build_id),
        Kind::Downgrade => format!("older than slot {}'s {} (embra.downgrade=1 allows it)", up(active), installed.version),
    }
}

/// The upgrade's confirmation: what is written where, and what goes.
fn confirm_upgrade(disk: &disks::Disk, inst: &Installed, m: &InstallManifest, kind: Kind, op: &mut Operator) -> Result<(), Outcome> {
    let (active, target) = (inst.active(), inst.target());
    let note = match kind {
        Kind::Newer => String::new(),
        Kind::OtherBuild => format!("It is the same version as slot {}'s, another build. ", up(active)),
        Kind::Downgrade => format!(
            "It is older than slot {}'s {}: an older OS halts on a newer knowledge-graph schema, and the snapshot the trial boot takes is the way back. ",
            up(active),
            inst.release(active).map(|r| r.version.as_str()).unwrap_or("release")
        ),
    };
    let goes = match inst.release(target) {
        Some(r) => format!("{}, build {}", r.version, r.build_id),
        None => "no release".to_string(),
    };
    let answer = op.ask(&format!(
        "Write {} (build {}) into slot {} of {}? {note}Slot {}'s content ({goes}) is gone from the first byte; slot {} stays as the fallback. [y/N] ",
        m.version,
        m.build_id,
        up(target),
        disk.name,
        up(target),
        up(active)
    ));
    if crate::console::confirmed(answer.as_deref()) {
        Ok(())
    } else {
        Err(Outcome::Refused("install: the upgrade was not confirmed; nothing was changed".into()))
    }
}

/// What to do with `disk`, from what it holds and what was asked: the
/// questions are put here and nothing is written. A disk without an
/// installation gets the clean install and its confirmation. One with an
/// installation is described; when it can take the medium's release in place
/// the operator chooses between the upgrade and the clean install, with no
/// default, and confirms the one chosen; when it cannot, the reason is said
/// and the clean install's confirmation follows. `--upgrade` is the upgrade
/// or a refusal; `--yes` without it is the clean install it always was.
pub fn plan(disk: &disks::Disk, found: &Found, m: &InstallManifest, wanted: Wanted<'_>, op: &mut Operator) -> Result<Plan, Outcome> {
    let inst = match found {
        Found::Installed(inst) => inst,
        other => {
            if let Found::Unreadable(why) = other {
                op.line(&format!("{} carries the embraOS partitions, but its boot state cannot be read: {why}", disk.name));
            }
            if wanted.upgrade {
                let why = match other {
                    Found::Unreadable(_) => "its boot state cannot be read",
                    _ => "it holds no embraOS installation",
                };
                return Err(Outcome::Refused(format!("refused: {} is not upgraded in place: {why}; nothing was changed", disk.name)));
            }
            if !wanted.yes {
                confirm_erase(disk, op)?;
            }
            return Ok(Plan::Clean);
        }
    };
    op.line(&inst.describe(&disk.name));
    let (active, target) = (inst.active(), inst.target());
    let offered = offer(inst, m, wanted.allow_downgrade);
    let medium_line = |kind: Kind| match inst.release(active) {
        Some(installed) => format!("The medium carries {} (build {}): {}.", m.version, m.build_id, kind_sentence(kind, installed, active)),
        None => format!("The medium carries {} (build {}).", m.version, m.build_id),
    };
    if wanted.upgrade {
        let kind = offered.map_err(|why| Outcome::Refused(format!("refused: {} is not upgraded in place: {why}; nothing was changed", disk.name)))?;
        op.line(&medium_line(kind));
        if !wanted.yes {
            confirm_upgrade(disk, inst, m, kind, op)?;
        }
        return Ok(Plan::Upgrade { target });
    }
    if wanted.yes {
        op.line(&format!("--yes without --upgrade: the installation on {} is erased", disk.name));
        return Ok(Plan::Clean);
    }
    let kind = match offered {
        Ok(kind) => kind,
        Err(why) => {
            op.line(&format!("An upgrade in place is not offered: {why}."));
            confirm_erase(disk, op)?;
            return Ok(Plan::Clean);
        }
    };
    op.line(&medium_line(kind));
    op.line(&format!(
        "  [u] upgrade in place: the medium's OS goes into slot {} and is tried at the next boot, slot {} stays as the fallback; STATE and DATA (the identity, the memories, the knowledge graph, the workspace) are not touched",
        up(target),
        up(active)
    ));
    op.line(&format!("  [c] clean install: everything on {} is erased", disk.name));
    let Some(choice) = op.ask("Upgrade in place or clean install? [u/c, q = reboot without installing]: ") else {
        return Err(Outcome::Refused("install: nobody answered; nothing was changed".into()));
    };
    match choice.trim() {
        "u" | "U" => {
            confirm_upgrade(disk, inst, m, kind, op)?;
            Ok(Plan::Upgrade { target })
        }
        "c" | "C" => {
            confirm_erase(disk, op)?;
            Ok(Plan::Clean)
        }
        _ => Err(Outcome::Refused("install: nothing chosen; nothing was changed".into())),
    }
}

/// The files a boot menu reads from a slot: every `$2/boot/<name>` on a
/// line that is no comment, which is how `boot_slot` in `grub.cfg.in` names
/// the kernel and the initrds of the slot it is handed. Read from the
/// medium's own `grub.cfg`, the menu an upgrade installs, so the list is
/// that menu's and no second copy of it.
pub fn slot_files_of_menu(grub_cfg: &str) -> Vec<String> {
    const MARK: &str = "$2/boot/";
    let mut files: Vec<String> = Vec::new();
    for line in grub_cfg.lines().filter(|l| !l.trim_start().starts_with('#')) {
        for (at, _) in line.match_indices(MARK) {
            let name: String = line[at + MARK.len()..].chars().take_while(|c| !c.is_whitespace() && !matches!(c, '"' | '\'' | ';')).collect();
            let file = format!("boot/{name}");
            if !name.is_empty() && !files.contains(&file) {
                files.push(file);
            }
        }
    }
    files
}

/// What `disk` holds: the five partitions by their GPT names, the boot state
/// on the ESP, each slot's release and size, DATA's engine marker, and what
/// the default slot lacks of the files the medium's boot menu reads from a
/// slot. Every partition is mounted read-only for its read, DATA without a
/// replay of its journal, and unmounted again: nothing on the disk changes.
fn probe(disk: &str, media: &Path) -> Found {
    let named = PARTITION_NAMES.iter().enumerate().all(|(i, name)| {
        let part = layout::partition_dev(disk, i as u32 + 1);
        std::fs::read_to_string(format!("/sys/class/block/{disk}/{part}/uevent"))
            .map(|u| u.lines().any(|l| l == format!("PARTNAME={name}")))
            .unwrap_or(false)
    });
    if !named {
        return Found::Nothing;
    }
    let dev = |n: u32| PathBuf::from(format!("/dev/{}", layout::partition_dev(disk, n)));
    let state = match esp::with_env(&dev(1), BootState::from_env) {
        Ok(state) => state,
        Err(e) => return Found::Unreadable(format!("grubenv on {}: {e}", dev(1).display())),
    };
    let mut releases = [None, None];
    let mut slot_bytes = [0u64; 2];
    for slot in [Slot::A, Slot::B] {
        let part = dev(slot_partition_number(slot));
        releases[index(slot)] = crate::slot::release_of(slot, &part).ok().flatten();
        slot_bytes[index(slot)] = partitions::size_bytes(&part).unwrap_or(0);
    }
    let active = state.slot_active;
    let menu_path = media.join("embraos/esp/EFI/BOOT/grub.cfg");
    let fallback_lacks = match std::fs::read_to_string(&menu_path) {
        Ok(menu) => {
            let wanted = slot_files_of_menu(&menu);
            if wanted.is_empty() {
                Err(format!("{} names none as $2/boot/<file>", menu_path.display()))
            } else if releases[index(active)].is_some() {
                Ok(crate::slot::missing_files(active, &dev(slot_partition_number(active)), &wanted).unwrap_or(wanted))
            } else {
                Ok(Vec::new())
            }
        }
        Err(e) => Err(format!("{}: {e}", menu_path.display())),
    };
    let data_engine = data_engine_of(&format!("/dev/{}", layout::partition_dev(disk, 5)));
    Found::Installed(Box::new(Installed { state, releases, slot_bytes, data_engine, fallback_lacks }))
}

/// DATA's engine marker (`wardsondb/.engine`), through a read-only mount
/// that replays no journal (`noload`): the marker is written once, at the
/// first boot, and nothing on DATA is to change before the operator answers.
fn data_engine_of(part: &str) -> Option<String> {
    use std::ffi::CString;
    let mnt = Path::new("/run/embra/data-probe");
    std::fs::create_dir_all(mnt).ok()?;
    let src = CString::new(part).ok()?;
    let dst = CString::new(mnt.to_string_lossy().as_bytes()).ok()?;
    let fstype = CString::new("ext4").expect("a literal");
    let options = CString::new("noload").expect("a literal");
    let flags = libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    // SAFETY: the strings live for the call; the data pointer is ext4's option text, NUL-terminated.
    let rc = unsafe { libc::mount(src.as_ptr(), dst.as_ptr(), fstype.as_ptr(), flags, options.as_ptr().cast()) };
    if rc != 0 {
        return None;
    }
    let engine = std::fs::read_to_string(mnt.join("wardsondb/.engine")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    unmount(mnt);
    engine
}

/// The command: preconditions, the disk, what it holds, the plan, the
/// phases, the last prompt.
pub fn install_command(media: &Path, wanted: Wanted<'_>, force: bool, op: &mut Operator) -> Outcome {
    if !force && BootStatus::read().map(|s| s.mode) != Some("install".to_string()) {
        return Outcome::Refused("refused: not an install boot (boot the installation medium; --force overrides)".into());
    }
    let manifest = match InstallManifest::read(media) {
        Ok(m) => m,
        Err(e) => return Outcome::Refused(format!("refused: {} is not the installation medium: {e}", media.display())),
    };
    let release = Release::read().ok();
    if let Some(r) = &release
        && r.arch != manifest.arch
    {
        return Outcome::Refused(format!("refused: the medium is {} and this machine runs {}", manifest.arch, r.arch));
    }
    let rootfs = media.join(&manifest.rootfs);
    match std::fs::metadata(&rootfs) {
        Ok(m) if m.len() == manifest.rootfs_size => {}
        Ok(m) => return Outcome::Refused(format!("refused: {} is {} bytes, the manifest says {}", rootfs.display(), m.len(), manifest.rootfs_size)),
        Err(e) => return Outcome::Refused(format!("refused: {}: {e}", rootfs.display())),
    }
    op.line(&format!("embraOS installer {} (build {}, {}, {})", manifest.version, manifest.build_id, manifest.arch, manifest.engine));
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let source = mount_source(&mounts, &media.to_string_lossy());
    op.line(&format!("Installation medium: {} at {}", source.as_deref().unwrap_or("?"), media.display()));
    let found = disks::list(Path::new("/sys/block"), &mounts, source.as_deref(), layout::MIN_DISK_MIB * layout::MIB);
    let disk = match pick_disk(&found, wanted.disk, wanted.yes, op) {
        Ok(d) => d,
        Err(o) => return o,
    };
    let on_disk = probe(&disk.name, media);
    let plan = match plan(&disk, &on_disk, &manifest, wanted, op) {
        Ok(p) => p,
        Err(o) => return o,
    };
    let mut real = Real { media: media.to_path_buf(), manifest: manifest.clone(), disk_bytes: disk.bytes };
    match plan {
        Plan::Clean => {
            match run_phases(&mut real, &disk.name, op) {
                Ok(_) => {}
                Err(e) => return Outcome::Failed(format!("install on {} stopped at {e}; the disk holds no bootable system yet", disk.name)),
            }
            let data_gib = (disk.bytes.saturating_sub(layout::planned_starts_mib()[4] * layout::MIB + layout::MIB)) as f64 / (1u64 << 30) as f64;
            op.line(&format!("done: embraOS {} is installed on {}; DATA is {:.1} GiB", manifest.version, disk.name, data_gib));
        }
        Plan::Upgrade { target } => {
            if let Err(e) = run_upgrade_phases(&mut real, &disk.name, target, op) {
                return Outcome::Failed(format!("upgrade of {} stopped at {e}", disk.name));
            }
            op.line(&format!(
                "done: embraOS {} (build {}) is in slot {} of {} and staged. The next boot tries it: a snapshot of STATE and DATA is taken first, before the knowledge graph migrates, and GRUB falls back to slot {} if the new release does not come up.",
                manifest.version,
                manifest.build_id,
                up(target),
                disk.name,
                up(target.other())
            ));
        }
    }
    if !wanted.yes {
        let _ = op.ask_timeout("Remove the installation medium and press Enter to reboot (60 s): ", Duration::from_secs(60));
    }
    Outcome::Done
}

/// The steps on the machine.
struct Real {
    media: PathBuf,
    manifest: InstallManifest,
    disk_bytes: u64,
}

fn tool(candidates: &[&str]) -> Result<PathBuf, String> {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .ok_or_else(|| format!("{} is not in this image", candidates[0]))
}

fn run(tool_path: &Path, args: &[String]) -> Result<(), String> {
    let out = Command::new(tool_path)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{}: {e}", tool_path.display()))?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(format!("{} {}: {} ({})", tool_path.display(), args.join(" "), err.lines().last().unwrap_or("").trim(), out.status))
    }
}

const BLKRRPART: libc::c_ulong = 0x125F;

impl Steps for Real {
    /// The old tables, and the first 4 MiB at each planned start: a stale
    /// slot B, STATE or DATA signature would otherwise show through.
    fn zap(&mut self, disk: &str) -> Result<(), String> {
        let sgdisk = tool(&["/usr/sbin/sgdisk", "/sbin/sgdisk", "/usr/bin/sgdisk"])?;
        let dev = format!("/dev/{disk}");
        run(&sgdisk, &["--zap-all".to_string(), dev.clone()])?;
        let mut f = std::fs::OpenOptions::new().write(true).open(&dev).map_err(|e| format!("open {dev}: {e}"))?;
        let zeros = vec![0u8; 4 * 1024 * 1024];
        for start in layout::planned_starts_mib() {
            let offset = start * layout::MIB;
            if offset + zeros.len() as u64 > self.disk_bytes {
                break;
            }
            f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
            f.write_all(&zeros).map_err(|e| format!("wiping {dev} at {start} MiB: {e}"))?;
        }
        f.sync_all().map_err(|e| e.to_string())?;
        Ok(())
    }

    fn partition(&mut self, disk: &str) -> Result<(), String> {
        let sgdisk = tool(&["/usr/sbin/sgdisk", "/sbin/sgdisk", "/usr/bin/sgdisk"])?;
        let dev = format!("/dev/{disk}");
        run(&sgdisk, &layout::sgdisk_create_args(&dev))?;
        {
            let f = std::fs::File::open(&dev).map_err(|e| format!("open {dev}: {e}"))?;
            use std::os::unix::io::AsRawFd;
            // SAFETY: BLKRRPART takes no argument; the kernel re-reads the table.
            let rc = unsafe { libc::ioctl(f.as_raw_fd(), BLKRRPART as _) };
            if rc < 0 {
                return Err(format!("the kernel did not re-read {dev}'s partition table: {}", std::io::Error::last_os_error()));
            }
        }
        // The five children, by name, within five seconds.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let present = PARTITION_NAMES.iter().enumerate().all(|(i, name)| {
                let part = layout::partition_dev(disk, i as u32 + 1);
                std::fs::read_to_string(format!("/sys/class/block/{disk}/{part}/uevent"))
                    .map(|u| u.lines().any(|l| l == format!("PARTNAME={name}")))
                    .unwrap_or(false)
                    && Path::new(&format!("/dev/{part}")).exists()
            });
            if present {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!("the five partitions of {disk} did not appear within 5 s"));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn format_esp(&mut self, part: &str) -> Result<(), String> {
        let mkfs = tool(&["/sbin/mkfs.fat", "/usr/sbin/mkfs.fat", "/sbin/mkfs.vfat", "/usr/sbin/mkfs.vfat"])?;
        run(&mkfs, &["-F".into(), "16".into(), "-n".into(), "ESP".into(), part.to_string()])
    }

    fn write_rootfs(&mut self, part: &str, name: &str, op: &mut Operator) -> Result<(), String> {
        let path = self.media.join(&self.manifest.rootfs);
        let mut body = std::fs::File::open(&path).map_err(|e| format!("open {}: {e}", path.display()))?;
        let total = self.manifest.rootfs_size;
        let mut last_tenth = 0u64;
        crate::slot::write_image(&mut body, Path::new(part), total, &self.manifest.rootfs_sha256, |done| {
            let tenth = (done * 10).checked_div(total).unwrap_or(10);
            if tenth > last_tenth {
                last_tenth = tenth;
                op.line(&format!("{name}: {}%", tenth * 10));
            }
        })?;
        op.line(&format!("{name}: verified (sha256 as streamed and as read back)"));
        Ok(())
    }

    /// e2fsprogs' mke2fs with the feature set of the image's STATE and DATA
    /// (the same mke2fs.conf); the lazy-init flags genimage sets for an
    /// image file are not wanted on a live disk, where the kernel finishes
    /// the tables in the background.
    fn format_ext4(&mut self, part: &str, label: &str) -> Result<(), String> {
        let mke2fs = tool(&["/sbin/mke2fs", "/usr/sbin/mke2fs", "/sbin/mkfs.ext4", "/usr/sbin/mkfs.ext4"])?;
        run(&mke2fs, &["-q".into(), "-F".into(), "-t".into(), "ext4".into(), "-I".into(), "256".into(), "-E".into(), "root_owner=0:0".into(), "-L".into(), label.into(), part.to_string()])
    }

    /// The disk's ESP tree from the medium (GRUB's loader under the
    /// architecture's name, bootx64.efi or bootaa64.efi, and grub.cfg), and
    /// a fresh grubenv: slot A active, as post_image.sh writes it.
    fn boot_files(&mut self, part: &str) -> Result<(), String> {
        let mnt = Path::new("/run/embra/esp-install");
        std::fs::create_dir_all(mnt).map_err(|e| e.to_string())?;
        mount_vfat(part, mnt)?;
        let result = (|| -> Result<(), String> {
            let from = self.media.join("embraos/esp/EFI/BOOT");
            let to = mnt.join("EFI/BOOT");
            std::fs::create_dir_all(&to).map_err(|e| e.to_string())?;
            for entry in std::fs::read_dir(&from).map_err(|e| format!("{}: {e}", from.display()))?.flatten() {
                let name = entry.file_name();
                if name == "grubenv" {
                    continue;
                }
                let dst = to.join(&name);
                std::fs::copy(entry.path(), &dst).map_err(|e| format!("copying {}: {e}", entry.path().display()))?;
                std::fs::File::open(&dst).and_then(|f| f.sync_all()).map_err(|e| e.to_string())?;
            }
            let mut env = GrubEnv::new();
            for (k, v) in [("slot_active", "a"), ("slot_try", ""), ("tries_left", "0"), ("slot_a_status", "ok"), ("slot_b_status", ""), ("menu_timeout", "3")] {
                env.set(k, v).map_err(|e| e.to_string())?;
            }
            env.write_new(&to.join("grubenv")).map_err(|e| format!("grubenv: {e}"))?;
            std::fs::File::open(&to).and_then(|d| d.sync_all()).map_err(|e| e.to_string())?;
            Ok(())
        })();
        unmount(mnt);
        result
    }

    fn verify(&mut self, disk: &str, rootfs_part: &str, esp_part: &str) -> Result<String, String> {
        let sgdisk = tool(&["/usr/sbin/sgdisk", "/sbin/sgdisk", "/usr/bin/sgdisk"])?;
        run(&sgdisk, &["-v".into(), format!("/dev/{disk}")])?;
        let release = crate::slot::release_of(Slot::A, Path::new(rootfs_part))?
            .ok_or_else(|| format!("{rootfs_part} holds no release stamp"))?;
        if release.version != self.manifest.version || release.build_id != self.manifest.build_id {
            return Err(format!("{rootfs_part} holds {} ({}), the medium {} ({})", release.version, release.build_id, self.manifest.version, self.manifest.build_id));
        }
        let mnt = Path::new("/run/embra/esp-install");
        mount_vfat(esp_part, mnt)?;
        let env = GrubEnv::read_file(&mnt.join("EFI/BOOT/grubenv")).map_err(|e| format!("grubenv: {e}"));
        unmount(mnt);
        let env = env?;
        if env.get("slot_active") != Some("a") {
            return Err("grubenv does not say slot_active=a".into());
        }
        Ok(format!("{}: both GPT headers, slot A holds {} ({}), grubenv says slot A", disk, release.version, release.build_id))
    }

    /// The medium's loader and `grub.cfg` over the disk's. Every file is
    /// first written beside its destination under a name of its own and
    /// synced, so a medium that cannot be read or an ESP without room
    /// replaces nothing; then the names are switched, the loader before
    /// `grub.cfg`: a loader with the menu of the release before it boots,
    /// and a menu ahead of its loader may name a command the loader lacks.
    /// `grubenv` is never touched here.
    fn refresh_boot_files(&mut self, part: &str) -> Result<(), String> {
        let mnt = Path::new("/run/embra/esp-install");
        std::fs::create_dir_all(mnt).map_err(|e| e.to_string())?;
        mount_vfat(part, mnt)?;
        let result = (|| -> Result<(), String> {
            let from = self.media.join("embraos/esp/EFI/BOOT");
            let to = mnt.join("EFI/BOOT");
            if !to.join("grubenv").is_file() {
                return Err(format!("{part} has no EFI/BOOT/grubenv; nothing on the ESP was replaced"));
            }
            let mut names: Vec<std::ffi::OsString> = std::fs::read_dir(&from)
                .map_err(|e| format!("{}: {e}; nothing on the ESP was replaced", from.display()))?
                .flatten()
                .map(|entry| entry.file_name())
                .filter(|name| name != "grubenv")
                .collect();
            // grub.cfg last.
            names.sort_by_key(|name| name == "grub.cfg");
            let staged = |name: &std::ffi::OsStr| {
                let mut new = name.to_os_string();
                new.push(".new");
                to.join(new)
            };
            let written = names.iter().try_for_each(|name| -> Result<(), String> {
                let tmp = staged(name);
                std::fs::copy(from.join(name), &tmp).map_err(|e| format!("copying {}: {e}", from.join(name).display()))?;
                std::fs::File::open(&tmp).and_then(|f| f.sync_all()).map_err(|e| format!("syncing {}: {e}", tmp.display()))
            });
            if let Err(e) = written {
                for name in &names {
                    let _ = std::fs::remove_file(staged(name));
                }
                return Err(format!("{e}; nothing on the ESP was replaced"));
            }
            for (done, name) in names.iter().enumerate() {
                std::fs::rename(staged(name), to.join(name)).map_err(|e| {
                    format!("replacing {}: {e}; {done} of the ESP's {} boot files are the medium's, the rest the disk's own", to.join(name).display(), names.len())
                })?;
            }
            std::fs::File::open(&to).and_then(|d| d.sync_all()).map_err(|e| e.to_string())?;
            Ok(())
        })();
        unmount(mnt);
        result
    }

    /// The slot staged as an OS update stages one (`BootState::stage`, the
    /// block rewritten in place): GRUB tries it at the next boots and keeps
    /// the default until embrad confirms it.
    fn stage(&mut self, esp_part: &str, slot: Slot) -> Result<String, String> {
        let staged = esp::with_env_mut(Path::new(esp_part), |env| {
            let mut state = BootState::from_env(env);
            state.stage(slot);
            state.apply(env).map(|()| state)
        });
        match staged {
            Ok(Ok(state)) => Ok(format!(
                "slot {} is pending with {} tries; slot {} stays the default until the new release confirms",
                up(slot),
                state.tries_left,
                up(state.slot_active)
            )),
            Ok(Err(e)) => Err(e.to_string()),
            Err(e) => Err(format!("{esp_part}: {e}")),
        }
    }

    fn verify_upgrade(&mut self, slot: Slot, rootfs_part: &str, esp_part: &str) -> Result<String, String> {
        let release = crate::slot::release_of(slot, Path::new(rootfs_part))?.ok_or_else(|| format!("{rootfs_part} holds no release stamp"))?;
        if release.version != self.manifest.version || release.build_id != self.manifest.build_id {
            return Err(format!("{rootfs_part} holds {} ({}), the medium {} ({})", release.version, release.build_id, self.manifest.version, self.manifest.build_id));
        }
        let state = esp::with_env(Path::new(esp_part), BootState::from_env).map_err(|e| format!("grubenv: {e}"))?;
        if state.slot_try != Some(slot) || state.status_of(slot) != SlotStatus::Pending || state.slot_active != slot.other() {
            return Err(format!("grubenv does not stage slot {} beside the default slot {}", up(slot), up(slot.other())));
        }
        let mnt = Path::new("/run/embra/esp-install");
        mount_vfat(esp_part, mnt)?;
        let same = (|| -> Result<(), String> {
            let from = self.media.join("embraos/esp/EFI/BOOT");
            for entry in std::fs::read_dir(&from).map_err(|e| format!("{}: {e}", from.display()))?.flatten() {
                let name = entry.file_name();
                if name == "grubenv" {
                    continue;
                }
                let on_disk = mnt.join("EFI/BOOT").join(&name);
                let theirs = std::fs::read(entry.path()).map_err(|e| format!("{}: {e}", entry.path().display()))?;
                let ours = std::fs::read(&on_disk).map_err(|e| format!("{}: {e}", on_disk.display()))?;
                if theirs != ours {
                    return Err(format!("{} on the ESP is not the medium's", name.to_string_lossy()));
                }
            }
            Ok(())
        })();
        unmount(mnt);
        same?;
        Ok(format!(
            "slot {} holds {} ({}), grubenv tries it next and keeps slot {} as the default, the loader and grub.cfg are the medium's",
            up(slot),
            release.version,
            release.build_id,
            up(slot.other())
        ))
    }
}

fn mount_vfat(part: &str, mnt: &Path) -> Result<(), String> {
    use std::ffi::CString;
    let src = CString::new(part).map_err(|e| e.to_string())?;
    let dst = CString::new(mnt.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
    let fstype = CString::new("vfat").expect("a literal");
    // SAFETY: the strings live for the call; no data string is passed.
    let rc = unsafe { libc::mount(src.as_ptr(), dst.as_ptr(), fstype.as_ptr(), libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, std::ptr::null()) };
    if rc == 0 { Ok(()) } else { Err(format!("mount {part} on {}: {}", mnt.display(), std::io::Error::last_os_error())) }
}

fn unmount(mnt: &Path) {
    use std::ffi::CString;
    if let Ok(dst) = CString::new(mnt.to_string_lossy().as_bytes()) {
        // SAFETY: the string lives for the call.
        unsafe {
            libc::umount(dst.as_ptr());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(name: &str, gib: u64, excluded: Option<&str>) -> disks::Disk {
        disks::Disk { name: name.into(), bytes: gib << 30, model: format!("{name} model"), bus: "ata".into(), removable: false, excluded: excluded.map(str::to_string) }
    }

    /// Records the phases; fails the one it is told to.
    struct Recording {
        phases: Vec<String>,
        fail_at: Option<&'static str>,
    }

    impl Recording {
        fn step(&mut self, name: &str) -> Result<(), String> {
            self.phases.push(name.to_string());
            if self.fail_at == Some(name) { Err(format!("{name} refused")) } else { Ok(()) }
        }
    }

    impl Steps for Recording {
        fn zap(&mut self, _d: &str) -> Result<(), String> { self.step("zap") }
        fn partition(&mut self, _d: &str) -> Result<(), String> { self.step("partition") }
        fn format_esp(&mut self, _p: &str) -> Result<(), String> { self.step("esp") }
        fn write_rootfs(&mut self, _p: &str, _name: &str, _op: &mut Operator) -> Result<(), String> { self.step("rootfs") }
        fn format_ext4(&mut self, _p: &str, label: &str) -> Result<(), String> { self.step(&label.to_lowercase()) }
        fn boot_files(&mut self, _p: &str) -> Result<(), String> { self.step("boot") }
        fn verify(&mut self, d: &str, _r: &str, _e: &str) -> Result<String, String> { self.step("verify").map(|_| format!("{d} verified")) }
        fn refresh_boot_files(&mut self, _p: &str) -> Result<(), String> { self.step("refresh") }
        fn stage(&mut self, _e: &str, slot: Slot) -> Result<String, String> { self.step("stage").map(|_| format!("slot {} is pending", up(slot))) }
        fn verify_upgrade(&mut self, slot: Slot, _r: &str, _e: &str) -> Result<String, String> { self.step("verify-upgrade").map(|_| format!("slot {} verified", up(slot))) }
    }

    fn release(version: &str, build: &str) -> Release {
        Release { version: version.into(), build_id: build.into(), arch: "x86_64".into(), engine: "fjall".into(), built: "2026-11-13T16:46:39Z".into() }
    }

    /// The medium: 0.23.1, build `new0000000`, x86_64, fjall, an 800 MiB image.
    fn medium() -> InstallManifest {
        InstallManifest {
            version: "0.23.1".into(),
            build_id: "new0000000".into(),
            arch: "x86_64".into(),
            engine: "fjall".into(),
            built: "2026-11-13T16:46:39Z".into(),
            kernel: "boot/bzImage".into(),
            rootfs: "embraos/rootfs.squashfs".into(),
            rootfs_size: 800 << 20,
            rootfs_sha256: "00".repeat(32),
        }
    }

    /// An installation as a first boot leaves it: slot A holds `in_a` and is
    /// the default, slot B is empty, 2048 MiB slots.
    fn installed(in_a: Release) -> Installed {
        let mut state = BootState::from_env(&GrubEnv::new());
        state.set_status(Slot::A, SlotStatus::Ok);
        Installed { state, releases: [Some(in_a), None], slot_bytes: [2048 << 20; 2], data_engine: None, fallback_lacks: Ok(Vec::new()) }
    }

    fn found(inst: Installed) -> Found {
        Found::Installed(Box::new(inst))
    }

    fn interactive() -> Wanted<'static> {
        Wanted::default()
    }

    /// `plan` with the answers given; what it came to and what was said.
    fn planned(found: &Found, wanted: Wanted<'_>, answers: &[&str]) -> (Result<Plan, String>, String) {
        let mut op = Operator::scripted(answers);
        let out = plan(&disk("vda", 20, None), found, &medium(), wanted, &mut op).map_err(|o| match o {
            Outcome::Refused(why) => why,
            _ => "an outcome other than a refusal".to_string(),
        });
        (out, op.transcript())
    }

    #[test]
    fn the_installer_phases_run_in_order_and_stop_at_the_first_failure() {
        let mut rec = Recording { phases: vec![], fail_at: None };
        let mut op = Operator::scripted(&[]);
        assert_eq!(run_phases(&mut rec, "vda", &mut op).unwrap(), "vda verified");
        assert_eq!(rec.phases, ["zap", "partition", "esp", "rootfs", "state", "data", "boot", "verify"]);
        let text = op.transcript();
        assert!(text.contains("esp: FAT16, label ESP (/dev/vda1)") && text.contains("data: ext4, label DATA (/dev/vda5)"), "{text}");

        let mut rec = Recording { phases: vec![], fail_at: Some("state") };
        let err = run_phases(&mut rec, "nvme0n1", &mut Operator::scripted(&[])).unwrap_err();
        assert_eq!(err, "state: state refused");
        assert_eq!(rec.phases, ["zap", "partition", "esp", "rootfs", "state"], "nothing after the failure");
    }

    #[test]
    fn an_upgrade_writes_the_other_slot_replaces_the_boot_files_and_stages_it_and_stops_at_the_first_failure() {
        let mut rec = Recording { phases: vec![], fail_at: None };
        let mut op = Operator::scripted(&[]);
        assert_eq!(run_upgrade_phases(&mut rec, "vda", Slot::B, &mut op).unwrap(), "slot B verified");
        assert_eq!(rec.phases, ["rootfs", "refresh", "stage", "verify-upgrade"], "no table, no format: the partitions, STATE and DATA stay");
        let text = op.transcript();
        assert!(text.contains("rootfs_b: the medium's rootfs → /dev/vda3"), "{text}");
        assert!(text.contains("→ /dev/vda1; grubenv stays") && text.contains("staged: slot B is pending"), "{text}");

        // Slot A is the second partition, on a disk whose partitions take a `p`.
        let mut op = Operator::scripted(&[]);
        run_upgrade_phases(&mut Recording { phases: vec![], fail_at: None }, "nvme0n1", Slot::A, &mut op).unwrap();
        assert!(op.transcript().contains("rootfs_a: the medium's rootfs → /dev/nvme0n1p2"), "{}", op.transcript());

        // Each failure stops the run and says what the disk boots now.
        for (fail_at, ran, says) in [
            ("rootfs", &["rootfs"][..], "slot B is not staged and the disk boots slot A as before"),
            ("refresh", &["rootfs", "refresh"], "slot B is written and verified but not staged"),
            ("stage", &["rootfs", "refresh", "stage"], "the disk boots slot A as before"),
        ] {
            let mut rec = Recording { phases: vec![], fail_at: Some(fail_at) };
            let err = run_upgrade_phases(&mut rec, "vda", Slot::B, &mut Operator::scripted(&[])).unwrap_err();
            assert!(err.contains(&format!("{fail_at} refused")) && err.contains(says), "{fail_at}: {err}");
            assert_eq!(rec.phases, ran, "nothing after {fail_at}");
        }
    }

    #[test]
    fn the_installer_prompt_takes_a_number_then_the_device_name_and_anything_else_reboots_without_writing() {
        // A disk without an installation: the number, then the clean
        // install's confirmation, as before the upgrade existed.
        let choose = |disks: &[disks::Disk], answers: &[&str]| {
            let mut op = Operator::scripted(answers);
            let chosen = pick_disk(disks, None, false, &mut op).and_then(|d| plan(&d, &Found::Nothing, &medium(), interactive(), &mut op).map(|p| (d, p)));
            (chosen, op.transcript())
        };
        let disks = vec![disk("sda", 40, None), disk("sdb", 8, Some("holds the installation medium")), disk("vda", 20, None), disk("sdc", 4, Some("too small"))];
        let (chosen, text) = choose(&disks, &["2", "vda"]);
        let (chosen, what) = chosen.ok().expect("chosen");
        assert_eq!((chosen.name.as_str(), what), ("vda", Plan::Clean));
        assert!(text.contains("[1] sda") && text.contains("[2] vda") && text.contains("sdb") && text.contains("holds the installation medium"), "{text}");
        assert!(text.contains("Everything on vda (20.0 GiB, vda model) is erased. Type its name (vda) to confirm: "));
        assert!(!text.contains("upgrade"), "a blank disk is asked nothing about an upgrade: {text}");
        for answers in [&["2", "sda"][..], &["q"], &["3"], &["2"], &[], &["0", "sda"]] {
            assert!(matches!(choose(&disks, answers).0, Err(Outcome::Refused(_))), "{answers:?}");
        }
        assert!(matches!(choose(&[disk("sdc", 4, Some("too small"))], &["1"]).0, Err(Outcome::Refused(_))), "nothing offered");
    }

    #[test]
    fn a_yes_run_names_its_disk_and_asks_nothing_and_an_excluded_disk_is_refused() {
        let disks = vec![disk("sda", 40, None), disk("sdb", 8, Some("holds the installation medium"))];
        let mut op = Operator::scripted(&[]);
        assert_eq!(pick_disk(&disks, Some("sda"), true, &mut op).ok().expect("chosen").name, "sda");
        assert!(!op.transcript().contains('?'), "nothing asked: {}", op.transcript());
        match pick_disk(&disks, Some("sdb"), true, &mut Operator::scripted(&[])) {
            Err(Outcome::Refused(r)) => assert!(r.contains("holds the installation medium"), "{r}"),
            _ => panic!("an excluded disk is refused"),
        }
        assert!(matches!(pick_disk(&disks, Some("sdz"), true, &mut Operator::scripted(&[])), Err(Outcome::Refused(_))));
        assert!(matches!(pick_disk(&disks, None, true, &mut Operator::scripted(&[])), Err(Outcome::Refused(_))), "--yes without --disk");
        // On a blank disk `--yes` is the clean install, with nothing asked.
        let (out, text) = planned(&Found::Nothing, Wanted { disk: Some("vda"), yes: true, ..Default::default() }, &[]);
        assert_eq!(out, Ok(Plan::Clean));
        assert!(!text.contains('?'), "{text}");
    }

    #[test]
    fn a_disk_that_holds_an_installation_is_asked_upgrade_or_clean_install_and_anything_else_changes_nothing() {
        let found = found(installed(release("0.23.0", "old0000000")));
        // The upgrade: `u`, then its own yes.
        let (out, text) = planned(&found, interactive(), &["u", "y"]);
        assert_eq!(out, Ok(Plan::Upgrade { target: Slot::B }));
        assert!(
            text.contains("vda holds an embraOS installation: slot A holds 0.23.0 (build old0000000, fjall), the default; slot B holds no release."),
            "{text}"
        );
        assert!(text.contains("The medium carries 0.23.1 (build new0000000): newer than slot A's 0.23.0."), "{text}");
        assert!(text.contains("[u] upgrade in place: the medium's OS goes into slot B") && text.contains("STATE and DATA"), "{text}");
        assert!(text.contains("[c] clean install: everything on vda is erased"), "{text}");
        assert!(text.contains("Upgrade in place or clean install? [u/c, q = reboot without installing]: "), "{text}");
        assert!(
            text.contains("Write 0.23.1 (build new0000000) into slot B of vda? Slot B's content (no release) is gone from the first byte; slot A stays as the fallback. [y/N] "),
            "{text}"
        );
        assert!(!text.contains("is erased. Type its name"), "an upgrade asks for no erasure: {text}");

        // The clean install: `c`, then the disk's name, as on a blank disk.
        let (out, text) = planned(&found, interactive(), &["c", "vda"]);
        assert_eq!(out, Ok(Plan::Clean));
        assert!(text.contains("Everything on vda (20.0 GiB, vda model) is erased. Type its name (vda) to confirm: "), "{text}");

        // No default, and every other answer writes nothing.
        for answers in [&[][..], &[""], &["q"], &["y"], &["upgrade"], &["u"], &["u", "n"], &["u", ""], &["c"], &["c", "sda"], &["c", "y"]] {
            let (out, _) = planned(&found, interactive(), answers);
            assert!(matches!(&out, Err(why) if why.contains("nothing was changed")), "{answers:?}: {out:?}");
        }
    }

    #[test]
    fn the_same_version_in_another_build_is_offered_and_its_confirmation_says_so() {
        let found = found(installed(release("0.23.1", "old0000000")));
        let (out, text) = planned(&found, interactive(), &["u", "y"]);
        assert_eq!(out, Ok(Plan::Upgrade { target: Slot::B }));
        assert!(text.contains("The medium carries 0.23.1 (build new0000000): the same version as slot A's, another build (slot A holds old0000000)."), "{text}");
        assert!(
            text.contains("Write 0.23.1 (build new0000000) into slot B of vda? It is the same version as slot A's, another build. Slot B's content"),
            "{text}"
        );
        // Unconfirmed, it is not written.
        assert!(planned(&found, interactive(), &["u", "n"]).0.is_err());
    }

    #[test]
    fn an_upgrade_goes_into_the_slot_that_is_not_the_default_whatever_that_slot_holds() {
        // Slot B is the default (an update confirmed there), slot A holds the
        // release from before it: the upgrade writes slot A.
        let mut inst = installed(release("0.22.0", "older00000"));
        inst.releases[1] = Some(release("0.23.0", "old0000000"));
        inst.state.confirm(Slot::B);
        let (out, text) = planned(&found(inst.clone()), interactive(), &["u", "y"]);
        assert_eq!(out, Ok(Plan::Upgrade { target: Slot::A }));
        assert!(text.contains("slot A holds 0.22.0 (build older00000, fjall); slot B holds 0.23.0 (build old0000000, fjall), the default."), "{text}");
        assert!(text.contains("into slot A of vda? Slot A's content (0.22.0, build older00000) is gone from the first byte; slot B stays as the fallback."), "{text}");

        // A release staged and not yet tried is in the slot the upgrade
        // writes: it is replaced and staged again, and the line says it.
        let mut inst = installed(release("0.23.0", "old0000000"));
        inst.releases[1] = Some(release("0.23.0", "staged0000"));
        inst.state.stage(Slot::B);
        let (out, text) = planned(&found(inst), interactive(), &["u", "y"]);
        assert_eq!(out, Ok(Plan::Upgrade { target: Slot::B }));
        assert!(text.contains("slot B holds 0.23.0 (build staged0000, fjall), staged with 3 tries left."), "{text}");
    }

    #[test]
    fn an_upgrade_in_place_is_refused_with_its_reason_and_the_clean_install_stays() {
        let base = || installed(release("0.23.0", "old0000000"));
        let refused = |inst: Installed, m: &InstallManifest, allow_downgrade: bool| offer(&inst, m, allow_downgrade).unwrap_err();

        // An older medium, unless the downgrade is allowed.
        let newer_installed = installed(release("0.24.0", "next000000"));
        let why = refused(newer_installed.clone(), &medium(), false);
        assert!(why.contains("the medium's 0.23.1 is older than slot A's 0.24.0") && why.contains("embra.downgrade=1"), "{why}");
        assert_eq!(offer(&newer_installed, &medium(), true), Ok(Kind::Downgrade));

        // The build that is installed already.
        let why = refused(installed(release("0.23.1", "new0000000")), &medium(), true);
        assert!(why.contains("slot A already holds this build (0.23.1, new0000000)"), "{why}");

        // Another engine: DATA's marker says what the database is, over the release's.
        let mut rocks = base();
        rocks.data_engine = Some("rocksdb".into());
        let why = refused(rocks, &medium(), false);
        assert!(why.contains("the medium is built for the fjall engine and the disk's database is rocksdb"), "{why}");
        let mut marker_agrees = installed(Release { engine: "rocksdb".into(), ..release("0.23.0", "old0000000") });
        marker_agrees.data_engine = Some("fjall".into());
        assert_eq!(offer(&marker_agrees, &medium(), false), Ok(Kind::Newer), "the marker wins over the release's engine");
        let why = refused(installed(Release { engine: "rocksdb".into(), ..release("0.23.0", "old0000000") }), &medium(), false);
        assert!(why.contains("the disk's database is rocksdb"), "without a marker the release's engine: {why}");

        // Another architecture.
        let why = refused(installed(Release { arch: "aarch64".into(), ..release("0.23.0", "old0000000") }), &medium(), false);
        assert!(why.contains("the medium is x86_64 and the installed OS aarch64"), "{why}");

        // Slots too small for the medium's image: the slot that would be written.
        let mut small = base();
        small.slot_bytes = [2048 << 20, 512 << 20];
        let why = refused(small, &medium(), false);
        assert!(why.contains("slot B is 536870912") && why.contains("too small"), "{why}");
        let mut exact = base();
        exact.slot_bytes = [0, 800 << 20];
        assert_eq!(offer(&exact, &medium(), false), Ok(Kind::Newer), "an image the size of the slot fits");

        // The default slot could not be booted by the menu the medium installs.
        let mut lacks = base();
        lacks.fallback_lacks = Ok(vec!["boot/ucode.cpio".into(), "boot/firmware.cpio.gz".into()]);
        let why = refused(lacks, &medium(), false);
        assert!(why.contains("slot A has no boot/ucode.cpio, boot/firmware.cpio.gz") && why.contains("fallback"), "{why}");
        // A menu on the medium that cannot be read for that list refuses too: the fallback is not taken on trust.
        let mut unread = base();
        unread.fallback_lacks = Err("/run/embra/media/embraos/esp/EFI/BOOT/grub.cfg: No such file or directory".into());
        let why = refused(unread, &medium(), false);
        assert!(why.contains("the medium's boot menu cannot be read") && why.contains("No such file"), "{why}");

        // The medium's build is in the other slot already, staged and not booted: nothing to write again.
        let mut again = base();
        again.releases[1] = Some(release("0.23.1", "new0000000"));
        again.state.stage(Slot::B);
        let why = refused(again.clone(), &medium(), false);
        assert!(why.contains("slot B already holds this build (0.23.1, new0000000), staged and not booted yet"), "{why}");
        // Once that trial has failed, or after a rollback, the slot is no longer staged and the upgrade is offered again.
        again.state.slot_try = None;
        again.state.set_status(Slot::B, SlotStatus::Failed);
        assert_eq!(offer(&again, &medium(), false), Ok(Kind::Newer));

        // The default slot holds no release.
        let mut empty = base();
        empty.releases = [None, Some(release("0.23.0", "old0000000"))];
        let why = refused(empty, &medium(), false);
        assert!(why.contains("slot A, the default, holds no release"), "{why}");

        // A version that does not compare.
        let why = refused(installed(release("nightly", "old0000000")), &medium(), true);
        assert!(why.contains("does not compare"), "{why}");

        // At the prompt the reason is said and the clean install's confirmation follows: no menu.
        let found = found(newer_installed);
        let (out, text) = planned(&found, interactive(), &["vda"]);
        assert_eq!(out, Ok(Plan::Clean));
        assert!(text.contains("An upgrade in place is not offered: the medium's 0.23.1 is older than slot A's 0.24.0"), "{text}");
        assert!(!text.contains("[u]") && text.contains("Everything on vda (20.0 GiB, vda model) is erased. Type its name (vda) to confirm: "), "{text}");
        assert!(planned(&found, interactive(), &["u"]).0.is_err(), "`u` is not the disk's name");
        // With the downgrade allowed it is offered, and both lines say what it is.
        let (out, text) = planned(&found, Wanted { allow_downgrade: true, ..Default::default() }, &["u", "y"]);
        assert_eq!(out, Ok(Plan::Upgrade { target: Slot::B }));
        assert!(text.contains("The medium carries 0.23.1 (build new0000000): older than slot A's 0.24.0 (embra.downgrade=1 allows it)."), "{text}");
        assert!(text.contains("It is older than slot A's 0.24.0: an older OS halts on a newer knowledge-graph schema"), "{text}");
    }

    #[test]
    fn an_unattended_upgrade_asks_nothing_and_never_becomes_a_clean_install() {
        let unattended = Wanted { disk: Some("vda"), yes: true, upgrade: true, allow_downgrade: false };
        let (out, text) = planned(&found(installed(release("0.23.0", "old0000000"))), unattended, &[]);
        assert_eq!(out, Ok(Plan::Upgrade { target: Slot::B }));
        assert!(!text.contains('?'), "nothing asked: {text}");
        assert!(text.contains("vda holds an embraOS installation") && text.contains("The medium carries 0.23.1"), "{text}");
        // Another build of the same version is the upgrade a rebuild is: `--yes` covers it.
        let (out, _) = planned(&found(installed(release("0.23.1", "old0000000"))), unattended, &[]);
        assert_eq!(out, Ok(Plan::Upgrade { target: Slot::B }));

        // No installation, an unreadable one, an upgrade that is refused: a refusal each, never the clean install.
        for (found, says) in [
            (Found::Nothing, "it holds no embraOS installation"),
            (Found::Unreadable("grubenv on /dev/vda1: No such file or directory".into()), "its boot state cannot be read"),
            (found(installed(release("0.24.0", "next000000"))), "older than slot A's 0.24.0"),
            (found(installed(release("0.23.1", "new0000000"))), "already holds this build"),
        ] {
            let (out, text) = planned(&found, unattended, &[]);
            let why = out.expect_err("refused");
            assert!(why.starts_with("refused: vda is not upgraded in place: ") && why.contains(says) && why.ends_with("nothing was changed"), "{why}");
            assert!(!text.contains('?'), "{text}");
        }

        // `--upgrade` without `--yes` goes straight to the upgrade's own confirmation.
        let asked = Wanted { disk: Some("vda"), yes: false, upgrade: true, allow_downgrade: false };
        let found = found(installed(release("0.23.0", "old0000000")));
        let (out, text) = planned(&found, asked, &["y"]);
        assert_eq!(out, Ok(Plan::Upgrade { target: Slot::B }));
        assert!(!text.contains("[u]") && text.contains("into slot B of vda?"), "{text}");
        assert!(planned(&found, asked, &["n"]).0.is_err() && planned(&found, asked, &[]).0.is_err());
    }

    #[test]
    fn an_unattended_clean_install_stays_one_on_a_disk_that_holds_an_installation() {
        // `embra.install=<disk>` erased the disk before the upgrade existed, and still does.
        let unattended = Wanted { disk: Some("vda"), yes: true, upgrade: false, allow_downgrade: false };
        let (out, text) = planned(&found(installed(release("0.23.0", "old0000000"))), unattended, &[]);
        assert_eq!(out, Ok(Plan::Clean));
        assert!(!text.contains('?'), "nothing asked: {text}");
        assert!(text.contains("vda holds an embraOS installation") && text.contains("--yes without --upgrade: the installation on vda is erased"), "{text}");
        // An installation whose boot state cannot be read is said, and the clean install's confirmation follows.
        let (out, text) = planned(&Found::Unreadable("grubenv on /dev/vda1: Input/output error".into()), interactive(), &["vda"]);
        assert_eq!(out, Ok(Plan::Clean));
        assert!(text.contains("vda carries the embraOS partitions, but its boot state cannot be read: grubenv on /dev/vda1: Input/output error"), "{text}");
    }

    #[test]
    fn the_files_the_boot_menu_reads_from_a_slot_are_the_ones_its_boot_slot_names() {
        // The template the medium's grub.cfg is rendered from: the kernel
        // (its name a placeholder there) and the three initrds, in the
        // order boot_slot loads them.
        let template = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../buildroot/board/embraos/grub.cfg.in")).expect("grub.cfg.in");
        assert_eq!(
            slot_files_of_menu(&template),
            ["boot/@KERNEL_IMAGE@", "boot/ucode.cpio", "boot/firmware.cpio.gz", "boot/initramfs.cpio.gz"],
            "boot_slot names what it reads from a slot as \"$2/boot/<file>\"; the installer reads the medium's menu for that list"
        );
        // Rendered, quoted or not, named twice, and never from a comment.
        let menu = "# linux \"$2/boot/old\"\n    linux \"$2/boot/bzImage\" quiet embra.slot=$1\n    initrd $2/boot/ucode.cpio \"$2/boot/initramfs.cpio.gz\"\n    initrd \"$2/boot/ucode.cpio\"\n";
        assert_eq!(slot_files_of_menu(menu), ["boot/bzImage", "boot/ucode.cpio", "boot/initramfs.cpio.gz"]);
        assert!(slot_files_of_menu("menuentry \"x\" { linux /boot/bzImage }\n").is_empty(), "a menu that names no slot file");
    }

    #[test]
    fn the_medium_s_mount_source_is_read_from_proc_mounts() {
        let mounts = "tmpfs /run tmpfs rw 0 0\n/dev/sr0 /run/embra/media iso9660 ro 0 0\n/dev/vda2 / squashfs ro 0 0\n";
        assert_eq!(mount_source(mounts, "/run/embra/media").as_deref(), Some("/dev/sr0"));
        assert_eq!(mount_source(mounts, "/mnt"), None);
    }
}
