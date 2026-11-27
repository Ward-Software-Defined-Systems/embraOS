//! The boot menu on the ESP follows the default slot.
//!
//! A release is a slot: an OS update writes the inactive slot and stages it,
//! and writes nothing else of the ESP. The menu GRUB reads, `grub.cfg`, would
//! so stay the one the installer wrote, whatever release the machine has
//! come to since. Every slot therefore carries its release's menu
//! (`embra_boot::esp::SLOT_MENU_PATH`, the very file the image's ESP is
//! given), and `boot-menu` puts the running slot's on the ESP when the boot
//! is of the default slot and came up: embrad runs it once every service is
//! healthy, after a trial is confirmed. The update that brings a release
//! brings its menu with it, at the first boot that confirms it; a rollback
//! brings back the menu of the release it returns to.
//!
//! The menu alone, never GRUB's loader: the loader is the installer's, and a
//! loader that was replaced while nobody is at the machine could not be
//! taken back. Every loader an installer has written has the same built-in
//! modules (`the_loader_every_installed_disk_has_is_the_one_the_menu_is_written_for`),
//! so the menu of any release runs on it.
//!
//! One rule, the installer's own for an upgrade in place: a menu is put on
//! the ESP only when both slots hold every file it reads from a slot. A menu
//! that names a file the other slot lacks could not start that slot, and the
//! other slot is where a rollback goes.

use std::io::Write as _;
use std::path::Path;

use embra_boot::{BootStatus, Slot, esp, partitions, slot};

use crate::Outcome;
use crate::console::Operator;

/// Whether this boot's slot is the one whose menu the ESP is to carry: the
/// slot GRUB starts by default, in a boot that needed no confirmation or got
/// it. A slot started once from the menu's Diagnostics, and a trial that was
/// not confirmed, leave the ESP as it is.
pub fn due(status: &BootStatus) -> Option<Slot> {
    let booted = status.slot?;
    let settled = matches!(status.confirmed.as_str(), "yes" | "not-needed");
    (settled && status.active == Some(booted)).then_some(booted)
}

/// What the ESP's menu is to the running release's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// The ESP's menu is this release's, byte for byte.
    Same,
    /// It is another release's.
    Other,
    /// This release carries no menu of its own.
    NoneInSlot,
    /// No ESP beside the root filesystem.
    NoEsp,
    /// One of the two could not be read.
    Unreadable(String),
}

/// What `status` says of it.
pub fn standing_words(standing: &Standing) -> String {
    match standing {
        Standing::Same => "the ESP's is this release's".to_string(),
        Standing::Other => {
            "the ESP's is another release's; a boot of the default slot that comes up puts this release's there, when both slots hold the files it reads".to_string()
        }
        Standing::NoneInSlot => "this release carries none of its own; the ESP's is as the installer wrote it".to_string(),
        Standing::NoEsp => "no ESP beside the root filesystem".to_string(),
        Standing::Unreadable(why) => format!("could not be read ({why})"),
    }
}

/// The ESP's menu against the running release's, the partition mounted
/// read-only for the read.
pub fn standing() -> Standing {
    let ours = match std::fs::read(esp::SLOT_MENU_PATH) {
        Ok(menu) => menu,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Standing::NoneInSlot,
        Err(e) => return Standing::Unreadable(format!("{}: {e}", esp::SLOT_MENU_PATH)),
    };
    let Some(dev) = partitions::find_partition_beside_root(slot::ESP_PARTITION) else {
        return Standing::NoEsp;
    };
    match esp::with_mounted(&dev, true, |root| std::fs::read(root.join(esp::MENU_RELATIVE))) {
        Ok(Ok(theirs)) if theirs == ours => Standing::Same,
        Ok(Ok(_)) => Standing::Other,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Standing::Other,
        Ok(Err(e)) => Standing::Unreadable(format!("{} on {}: {e}", esp::MENU_RELATIVE, dev.display())),
        Err(e) => Standing::Unreadable(format!("{}: {e}", dev.display())),
    }
}

/// Why a menu is not put on the ESP: a slot lacks a file the menu reads from
/// a slot. `None` when both hold them all. `here` is what the running slot
/// lacks; `other` the other slot with what it lacks, when it holds a release
/// at all (an empty slot has nothing to start).
pub fn refusal(wanted: &[String], booted: Slot, here: &[String], other: Option<(Slot, &[String])>) -> Option<String> {
    const KEEPS: &str = "the ESP keeps the menu it has";
    if wanted.is_empty() {
        return Some(format!("refused: this release's boot menu names no file of a slot ($2/boot/<file>); {KEEPS}"));
    }
    if !here.is_empty() {
        return Some(format!(
            "refused: this release's boot menu reads {} from a slot, and slot {booted}, which runs, lacks {}; {KEEPS}",
            wanted.join(", "),
            here.join(", ")
        ));
    }
    match other {
        Some((slot, lacks)) if !lacks.is_empty() => Some(format!(
            "refused: this release's boot menu reads {} from a slot, and slot {slot} lacks {}: {KEEPS}, which starts both slots",
            wanted.join(", "),
            lacks.join(", ")
        )),
        _ => None,
    }
}

/// `menu` over `<root>/EFI/BOOT/grub.cfg`, where `root` is the mounted ESP:
/// written beside its destination under a name of its own and synced, so a
/// write that fails replaces nothing; then renamed over it, the directory
/// synced, and read back. The loader and `grubenv` are never touched. An
/// ESP without `grubenv` is no installed disk's, and is left alone.
pub fn replace_menu(root: &Path, menu: &[u8]) -> Result<(), String> {
    let to = root.join(esp::MENU_RELATIVE);
    let dir = to.parent().ok_or_else(|| "the menu's path has no directory".to_string())?;
    if !root.join(esp::GRUBENV_RELATIVE).is_file() {
        return Err(format!("no {} on the ESP; nothing on it was replaced", esp::GRUBENV_RELATIVE));
    }
    let staged = dir.join("grub.cfg.new");
    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&staged)?;
        file.write_all(menu)?;
        file.sync_all()
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&staged);
        return Err(format!("writing {}: {e}; nothing on the ESP was replaced", staged.display()));
    }
    if let Err(e) = std::fs::rename(&staged, &to) {
        let _ = std::fs::remove_file(&staged);
        return Err(format!("replacing {}: {e}; the ESP's menu is as it was", to.display()));
    }
    std::fs::File::open(dir).and_then(|d| d.sync_all()).map_err(|e| format!("syncing {}: {e}", dir.display()))?;
    match std::fs::read(&to) {
        Ok(back) if back == menu => Ok(()),
        Ok(_) => Err(format!("{} read back is not the menu that was written", to.display())),
        Err(e) => Err(format!("reading {} back: {e}", to.display())),
    }
}

/// The command. Silent, and `Nothing`, on every boot that leaves the ESP as
/// it is: the menu there is this release's, the boot is not of the default
/// slot, the release carries no menu, there is no ESP. A line and `Done`
/// when the menu was replaced; `Refused` with the reason when a slot lacks a
/// file the menu reads; `Failed` when a read or the write failed.
pub fn boot_menu_command(op: &mut Operator) -> Outcome {
    let Some(status) = BootStatus::read() else {
        return Outcome::Nothing("no boot status (/run/embra/boot-status); is this embraOS, booted through GRUB?".into());
    };
    let Some(booted) = due(&status) else {
        return Outcome::Nothing(String::new());
    };
    let menu = match std::fs::read(esp::SLOT_MENU_PATH) {
        Ok(menu) if !menu.is_empty() => menu,
        Ok(_) => return Outcome::Failed(format!("boot-menu: {} is empty; the ESP keeps the menu it has", esp::SLOT_MENU_PATH)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Outcome::Nothing(String::new()),
        Err(e) => return Outcome::Failed(format!("boot-menu: {}: {e}; the ESP keeps the menu it has", esp::SLOT_MENU_PATH)),
    };
    let Some(esp_dev) = partitions::find_partition_beside_root(slot::ESP_PARTITION) else {
        return Outcome::Nothing(String::new());
    };
    match esp::with_mounted(&esp_dev, true, |root| std::fs::read(root.join(esp::MENU_RELATIVE))) {
        Ok(Ok(on_esp)) if on_esp == menu => return Outcome::Nothing(String::new()),
        Ok(Ok(_)) => {}
        // An ESP without a menu is given one.
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
        Ok(Err(e)) => return Outcome::Failed(format!("boot-menu: {} on {}: {e}; nothing was changed", esp::MENU_RELATIVE, esp_dev.display())),
        Err(e) => return Outcome::Failed(format!("boot-menu: {} could not be mounted: {e}; nothing was changed", esp_dev.display())),
    }

    // The rule of the two slots. The running slot is the root filesystem;
    // the other is read through a mount of its own, when it holds a release.
    let wanted = crate::install::slot_files_of_menu(&String::from_utf8_lossy(&menu));
    let here: Vec<String> = wanted.iter().filter(|file| !Path::new("/").join(file).is_file()).cloned().collect();
    let other = booted.other();
    let other_lacks = match partitions::find_partition_beside_root(other.partition_name()) {
        None => None,
        Some(dev) => match crate::slot::release_of(other, &dev) {
            Ok(None) => None,
            // A slot that cannot be read is taken to lack them all, as the
            // installer takes it.
            Ok(Some(_)) => Some(crate::slot::missing_files(other, &dev, &wanted).unwrap_or_else(|_| wanted.clone())),
            Err(_) => Some(wanted.clone()),
        },
    };
    if let Some(why) = refusal(&wanted, booted, &here, other_lacks.as_deref().map(|lacks| (other, lacks))) {
        return Outcome::Refused(why);
    }

    match esp::with_mounted(&esp_dev, false, |root| replace_menu(root, &menu)) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Outcome::Failed(format!("boot-menu: {e}")),
        Err(e) => return Outcome::Failed(format!("boot-menu: {} could not be mounted for writing: {e}; its menu is as it was", esp_dev.display())),
    }
    let release = match (&status.version, &status.build_id) {
        (Some(version), Some(build)) => format!("{version} ({build})"),
        _ => "this release".to_string(),
    };
    op.line(&format!(
        "boot menu: the ESP on {} has the menu of {release} now, the default slot's; GRUB's loader and grubenv are as they were",
        esp_dev.display()
    ));
    Outcome::Done
}

#[cfg(test)]
mod tests {
    use super::*;
    use embra_boot::SlotStatus;

    fn status(slot: Option<Slot>, trial: bool, confirmed: &str, active: Option<Slot>) -> BootStatus {
        BootStatus { slot, trial, confirmed: confirmed.to_string(), active, slot_a_status: Some(SlotStatus::Ok), ..BootStatus::default() }
    }

    /// The ESP takes the menu of the slot GRUB starts by default, at a boot
    /// that came up: a plain boot of the default slot, and a trial once it is
    /// confirmed, which makes its slot the default.
    #[test]
    fn the_menu_is_the_default_slots_at_a_boot_that_came_up() {
        // A plain boot of the default slot.
        assert_eq!(due(&status(Some(Slot::A), false, "not-needed", Some(Slot::A))), Some(Slot::A));
        // A trial of slot B, confirmed: B is the default now.
        assert_eq!(due(&status(Some(Slot::B), true, "yes", Some(Slot::B))), Some(Slot::B));
        // A trial whose confirmation was not written: GRUB still starts the slot before it by default.
        assert_eq!(due(&status(Some(Slot::B), true, "failed: grubenv", Some(Slot::A))), None);
        assert_eq!(due(&status(Some(Slot::B), true, "pending", Some(Slot::A))), None);
        // The confirmation counts by itself: a block that already names the slot as the default
        // does not stand for a boot that came up.
        assert_eq!(due(&status(Some(Slot::B), true, "failed: grubenv", Some(Slot::B))), None);
        assert_eq!(due(&status(Some(Slot::B), true, "pending", Some(Slot::B))), None);
        // A slot started once from Diagnostics while the other is the default.
        assert_eq!(due(&status(Some(Slot::B), false, "not-needed", Some(Slot::A))), None);
        // No slot on the kernel line, and a status whose block was not read.
        assert_eq!(due(&status(None, false, "not-needed", None)), None);
        assert_eq!(due(&status(Some(Slot::A), false, "not-needed", None)), None);
    }

    /// A menu goes on the ESP only when both slots hold every file it reads
    /// from a slot; the refusal names the files and the slot, and says that
    /// the ESP keeps its menu.
    #[test]
    fn a_menu_that_a_slot_could_not_be_started_with_is_refused() {
        let wanted: Vec<String> = ["boot/bzImage", "boot/ucode.cpio", "boot/firmware.cpio.gz", "boot/initramfs.cpio.gz"].iter().map(|s| s.to_string()).collect();
        let none: Vec<String> = Vec::new();
        // Both slots hold them all; an empty other slot has nothing to start.
        assert_eq!(refusal(&wanted, Slot::B, &none, Some((Slot::A, &none))), None);
        assert_eq!(refusal(&wanted, Slot::A, &none, None), None);
        // The slot a rollback returns to lacks one.
        let lacks = vec!["boot/ucode.cpio".to_string()];
        assert_eq!(
            refusal(&wanted, Slot::B, &none, Some((Slot::A, &lacks))).as_deref(),
            Some(
                "refused: this release's boot menu reads boot/bzImage, boot/ucode.cpio, boot/firmware.cpio.gz, boot/initramfs.cpio.gz from a slot, \
                 and slot a lacks boot/ucode.cpio: the ESP keeps the menu it has, which starts both slots"
            )
        );
        // The running slot lacks one of its own menu's files.
        let said = refusal(&wanted, Slot::B, &lacks, Some((Slot::A, &none))).unwrap();
        assert!(said.contains("slot b, which runs, lacks boot/ucode.cpio; the ESP keeps the menu it has"), "{said}");
        // A menu that names no file of a slot is no menu of ours.
        let said = refusal(&[], Slot::A, &none, None).unwrap();
        assert!(said.starts_with("refused: this release's boot menu names no file of a slot"), "{said}");
        assert!(said.ends_with("the ESP keeps the menu it has"), "{said}");
    }

    /// A stand-in for a mounted ESP: the loader, the menu and the block.
    fn esp_tree(tag: &str, menu: Option<&[u8]>, block: bool) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("embra-update-boot-menu-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("EFI/BOOT");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bootx64.efi"), b"the loader the installer wrote").unwrap();
        if let Some(menu) = menu {
            std::fs::write(dir.join("grub.cfg"), menu).unwrap();
        }
        if block {
            std::fs::write(dir.join("grubenv"), vec![b'#'; 1024]).unwrap();
        }
        root
    }

    fn files(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(root.join("EFI/BOOT")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    /// The menu is replaced whole and nothing else is touched: the loader
    /// and the block are byte for byte what they were, and no staged file is
    /// left beside them.
    #[test]
    fn the_menu_is_replaced_and_the_loader_and_the_block_are_left() {
        let root = esp_tree("replace", Some(b"the menu of the release before\n"), true);
        let new = b"set timeout_style=menu\n# the menu of this release\n".to_vec();
        replace_menu(&root, &new).unwrap();
        assert_eq!(std::fs::read(root.join(esp::MENU_RELATIVE)).unwrap(), new);
        assert_eq!(std::fs::read(root.join("EFI/BOOT/bootx64.efi")).unwrap(), b"the loader the installer wrote");
        assert_eq!(std::fs::read(root.join(esp::GRUBENV_RELATIVE)).unwrap(), vec![b'#'; 1024]);
        assert_eq!(files(&root), ["bootx64.efi", "grub.cfg", "grubenv"]);
        // The same menu again is written again and is the same: the command decides before, not this.
        replace_menu(&root, &new).unwrap();
        assert_eq!(std::fs::read(root.join(esp::MENU_RELATIVE)).unwrap(), new);
        // An ESP without a menu is given one.
        std::fs::remove_file(root.join(esp::MENU_RELATIVE)).unwrap();
        replace_menu(&root, &new).unwrap();
        assert_eq!(files(&root), ["bootx64.efi", "grub.cfg", "grubenv"]);
        // A file staged by a run that was cut is written over.
        std::fs::write(root.join("EFI/BOOT/grub.cfg.new"), b"half a menu").unwrap();
        replace_menu(&root, &new).unwrap();
        assert_eq!(files(&root), ["bootx64.efi", "grub.cfg", "grubenv"]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A partition that is no installed disk's ESP, and a write that fails,
    /// leave what is there.
    #[test]
    fn nothing_is_replaced_on_a_partition_without_the_block_or_when_the_write_fails() {
        let old = b"the menu that is there\n";
        // No grubenv: not an installed disk's ESP.
        let root = esp_tree("no-block", Some(old), false);
        let said = replace_menu(&root, b"another menu\n").unwrap_err();
        assert!(said.ends_with("nothing on it was replaced"), "{said}");
        assert_eq!(std::fs::read(root.join(esp::MENU_RELATIVE)).unwrap(), old);
        assert_eq!(files(&root), ["bootx64.efi", "grub.cfg"]);
        std::fs::remove_dir_all(&root).unwrap();
        // The staged name is taken by a directory: the write fails, and the menu stays.
        let root = esp_tree("no-write", Some(old), true);
        std::fs::create_dir(root.join("EFI/BOOT/grub.cfg.new")).unwrap();
        let said = replace_menu(&root, b"another menu\n").unwrap_err();
        assert!(said.ends_with("nothing on the ESP was replaced"), "{said}");
        assert_eq!(std::fs::read(root.join(esp::MENU_RELATIVE)).unwrap(), old);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// What `status` says of the menu, each case in a sentence of its own.
    #[test]
    fn the_status_says_whose_menu_the_esp_has() {
        assert_eq!(standing_words(&Standing::Same), "the ESP's is this release's");
        assert!(standing_words(&Standing::Other).starts_with("the ESP's is another release's; a boot of the default slot that comes up puts this release's there"));
        assert!(standing_words(&Standing::NoneInSlot).starts_with("this release carries none of its own"));
        assert_eq!(standing_words(&Standing::NoEsp), "no ESP beside the root filesystem");
        assert_eq!(standing_words(&Standing::Unreadable("EIO".into())), "could not be read (EIO)");
    }

    /// The menu every slot carries is the one its own files answer: the
    /// template reads four files from a slot, and they are the ones the
    /// build puts under /boot.
    #[test]
    fn the_menu_of_the_tree_reads_the_files_the_build_puts_in_a_slot() {
        let template = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../buildroot/board/embraos/grub.cfg.in")).unwrap();
        let wanted = crate::install::slot_files_of_menu(&template);
        assert_eq!(wanted, ["boot/@KERNEL_IMAGE@", "boot/ucode.cpio", "boot/firmware.cpio.gz", "boot/initramfs.cpio.gz"]);
    }
}
