//! `install`: embraOS onto a local disk from the installation medium. The
//! disk gets the layout's five partitions with DATA to its end (sgdisk), the
//! ESP formatted and filled from the medium's tree with a fresh grubenv that
//! makes slot A active, rootfs_a streamed from the medium's squashfs and
//! verified like a release (`slot::write_image`), STATE and DATA formatted.
//! The phases run in order and stop at the first failure, after which the
//! disk holds no bootable system and the installer says so. The operator is
//! asked twice, the disk's number and then its name, unless `--yes --disk`.

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use embra_boot::media::InstallManifest;
use embra_boot::{BootStatus, GrubEnv, Release, Slot, disks, layout, slot::PARTITION_NAMES};

use crate::Outcome;
use crate::console::Operator;

/// The phases, in the order they run; the real steps touch the disk, the
/// tests record them.
pub trait Steps {
    fn zap(&mut self, disk: &str) -> Result<(), String>;
    fn partition(&mut self, disk: &str) -> Result<(), String>;
    fn format_esp(&mut self, part: &str) -> Result<(), String>;
    fn write_rootfs(&mut self, part: &str, op: &mut Operator) -> Result<(), String>;
    fn format_ext4(&mut self, part: &str, label: &str) -> Result<(), String>;
    fn boot_files(&mut self, part: &str) -> Result<(), String>;
    fn verify(&mut self, disk: &str, rootfs_part: &str, esp_part: &str) -> Result<String, String>;
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
    steps.write_rootfs(&rootfs_a, op).map_err(|e| format!("rootfs_a: {e}"))?;
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
/// picks a number and confirms with the disk's name. `Err` is the refusal.
pub fn choose_disk(disks: &[disks::Disk], wanted: Option<&str>, yes: bool, op: &mut Operator) -> Result<disks::Disk, Outcome> {
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
    let chosen = numbered[idx - 1];
    let Some(second) = op.ask(&format!(
        "Everything on {} ({}, {}) is erased. Type its name ({}) to confirm: ",
        chosen.name, chosen.size_text(), chosen.model, chosen.name
    )) else {
        return Err(Outcome::Refused("install: nobody confirmed; nothing was changed".into()));
    };
    if second.trim() != chosen.name {
        return Err(Outcome::Refused("install: not confirmed; nothing was changed".into()));
    }
    Ok(chosen.clone())
}

/// The command: preconditions, the disk, the phases, the last prompt.
pub fn install_command(media: &Path, wanted: Option<&str>, yes: bool, force: bool, op: &mut Operator) -> Outcome {
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
    let disk = match choose_disk(&found, wanted, yes, op) {
        Ok(d) => d,
        Err(o) => return o,
    };
    let mut real = Real { media: media.to_path_buf(), manifest: manifest.clone(), disk_bytes: disk.bytes };
    match run_phases(&mut real, &disk.name, op) {
        Ok(_) => {}
        Err(e) => return Outcome::Failed(format!("install on {} stopped at {e}; the disk holds no bootable system yet", disk.name)),
    }
    let data_gib = (disk.bytes.saturating_sub(layout::planned_starts_mib()[4] * layout::MIB + layout::MIB)) as f64 / (1u64 << 30) as f64;
    op.line(&format!("done: embraOS {} is installed on {}; DATA is {:.1} GiB", manifest.version, disk.name, data_gib));
    if !yes {
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

    fn write_rootfs(&mut self, part: &str, op: &mut Operator) -> Result<(), String> {
        let path = self.media.join(&self.manifest.rootfs);
        let mut body = std::fs::File::open(&path).map_err(|e| format!("open {}: {e}", path.display()))?;
        let total = self.manifest.rootfs_size;
        let mut last_tenth = 0u64;
        crate::slot::write_image(&mut body, Path::new(part), total, &self.manifest.rootfs_sha256, |done| {
            let tenth = (done * 10).checked_div(total).unwrap_or(10);
            if tenth > last_tenth {
                last_tenth = tenth;
                op.line(&format!("rootfs_a: {}%", tenth * 10));
            }
        })?;
        op.line("rootfs_a: verified (sha256 as streamed and as read back)");
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
        fn write_rootfs(&mut self, _p: &str, _op: &mut Operator) -> Result<(), String> { self.step("rootfs") }
        fn format_ext4(&mut self, _p: &str, label: &str) -> Result<(), String> { self.step(&label.to_lowercase()) }
        fn boot_files(&mut self, _p: &str) -> Result<(), String> { self.step("boot") }
        fn verify(&mut self, d: &str, _r: &str, _e: &str) -> Result<String, String> { self.step("verify").map(|_| format!("{d} verified")) }
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
    fn the_installer_prompt_takes_a_number_then_the_device_name_and_anything_else_reboots_without_writing() {
        let disks = vec![disk("sda", 40, None), disk("sdb", 8, Some("holds the installation medium")), disk("vda", 20, None), disk("sdc", 4, Some("too small"))];
        let mut op = Operator::scripted(&["2", "vda"]);
        assert_eq!(choose_disk(&disks, None, false, &mut op).ok().expect("chosen").name, "vda");
        let text = op.transcript();
        assert!(text.contains("[1] sda") && text.contains("[2] vda") && text.contains("sdb") && text.contains("holds the installation medium"), "{text}");
        assert!(text.contains("Everything on vda (20.0 GiB, vda model) is erased. Type its name (vda) to confirm: "));
        for answers in [&["2", "sda"][..], &["q"], &["3"], &["2"], &[], &["0", "sda"]] {
            assert!(matches!(choose_disk(&disks, None, false, &mut Operator::scripted(answers)), Err(Outcome::Refused(_))), "{answers:?}");
        }
        assert!(matches!(choose_disk(&[disk("sdc", 4, Some("too small"))], None, false, &mut Operator::scripted(&["1"])), Err(Outcome::Refused(_))), "nothing offered");
    }

    #[test]
    fn a_yes_run_names_its_disk_and_asks_nothing_and_an_excluded_disk_is_refused() {
        let disks = vec![disk("sda", 40, None), disk("sdb", 8, Some("holds the installation medium"))];
        let mut op = Operator::scripted(&[]);
        assert_eq!(choose_disk(&disks, Some("sda"), true, &mut op).ok().expect("chosen").name, "sda");
        assert!(!op.transcript().contains('?'), "nothing asked: {}", op.transcript());
        match choose_disk(&disks, Some("sdb"), true, &mut Operator::scripted(&[])) {
            Err(Outcome::Refused(r)) => assert!(r.contains("holds the installation medium"), "{r}"),
            _ => panic!("an excluded disk is refused"),
        }
        assert!(matches!(choose_disk(&disks, Some("sdz"), true, &mut Operator::scripted(&[])), Err(Outcome::Refused(_))));
        assert!(matches!(choose_disk(&disks, None, true, &mut Operator::scripted(&[])), Err(Outcome::Refused(_))), "--yes without --disk");
    }

    #[test]
    fn the_medium_s_mount_source_is_read_from_proc_mounts() {
        let mounts = "tmpfs /run tmpfs rw 0 0\n/dev/sr0 /run/embra/media iso9660 ro 0 0\n/dev/vda2 / squashfs ro 0 0\n";
        assert_eq!(mount_source(mounts, "/run/embra/media").as_deref(), Some("/dev/sr0"));
        assert_eq!(mount_source(mounts, "/mnt"), None);
    }
}
