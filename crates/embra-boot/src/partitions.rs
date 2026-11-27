//! Partitions by their GPT name, from sysfs and without udev: the kernel
//! puts `PARTNAME=` into every GPT partition's uevent
//! (`/sys/class/block/<dev>/uevent`), and devtmpfs carries the node under
//! `/dev/<DEVNAME>`.
//!
//! Every installed disk carries the same five names, so a name alone does
//! not say which disk. Two things do. The boot menu names the partition it
//! was read from by its GPT identifier (`embra.esp=`, which the kernel has
//! as `PARTUUID=` in the same uevent), and the initramfs takes the slot,
//! STATE and DATA from the disk that holds it ([`boot_disk_in`],
//! [`find_partition_on`]). Once the root filesystem is mounted, the disk is
//! the root's own ([`root_disk`], [`find_partition_beside_root`]). A name
//! over every disk ([`find_partition`]) is what is left for a boot whose
//! menu named nothing.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::disks;

pub const SYS_CLASS_BLOCK: &str = "/sys/class/block";
/// The whole disks, each partition a directory under its disk.
pub const SYS_BLOCK: &str = "/sys/block";
/// Every block device by its number (`<major>:<minor>`), a link to its entry.
pub const SYS_DEV_BLOCK: &str = "/sys/dev/block";
pub const DEV: &str = "/dev";
const POLL: Duration = Duration::from_millis(100);

/// `/dev/<node>` of the partition named `name`, the first one in sysfs
/// order when two disks carry the name.
pub fn find_partition(name: &str) -> Option<PathBuf> {
    find_partition_in(Path::new(SYS_CLASS_BLOCK), Path::new(DEV), name)
}

/// [`find_partition`] over another sysfs tree and device directory (tests).
pub fn find_partition_in(sys_class_block: &Path, dev: &Path, name: &str) -> Option<PathBuf> {
    partitions_named_in(sys_class_block, dev, name).into_iter().next()
}

/// Every partition named `name`, in sysfs order: [`find_partition`] gives
/// the first of them.
pub fn partitions_named_in(sys_class_block: &Path, dev: &Path, name: &str) -> Vec<PathBuf> {
    uevents_in(sys_class_block)
        .iter()
        .filter_map(|text| match parse_uevent(text) {
            (Some(d), Some(n)) if n == name => Some(dev.join(d)),
            _ => None,
        })
        .collect()
}

/// The uevent of every entry of a sysfs block directory, in the order of
/// the entries' names.
fn uevents_in(dir: &Path) -> Vec<String> {
    let Ok(read) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut entries: Vec<PathBuf> = read.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    entries.iter().filter_map(|entry| std::fs::read_to_string(entry.join("uevent")).ok()).collect()
}

/// The disks that carry a partition named `name`, each once, in the order
/// [`find_partition`] meets them: the first is the one it takes.
pub fn disks_with(name: &str) -> Vec<String> {
    disks_with_in(Path::new(SYS_CLASS_BLOCK), Path::new(SYS_BLOCK), name)
}

/// [`disks_with`] over other sysfs trees (tests).
pub fn disks_with_in(sys_class_block: &Path, sys_block: &Path, name: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for node in partitions_named_in(sys_class_block, Path::new(""), name) {
        let disk = disks::parent_disk_in(sys_block, &node.to_string_lossy());
        if !found.contains(&disk) {
            found.push(disk);
        }
    }
    found
}

/// What an operator does about a boot menu that does not name its disk: the
/// end of every warning of a second disk with the layout.
pub const MENU_REMEDY: &str =
    "The boot menu of a current release names its disk; an upgrade in place from the installation medium writes that menu";

/// What the initramfs says when it took the partitions by name and more
/// than one disk carries the slot's (`disks`, the first being the one
/// taken): nothing for one disk or none.
pub fn several_disks_warning(disks: &[String]) -> Option<String> {
    let [first, _, ..] = disks else { return None };
    Some(format!(
        "{} disks here carry the embraOS layout ({}), and this boot could not tell which one it was started from: it takes {first}, the first. {MENU_REMEDY}",
        disks.len(),
        disks.join(", ")
    ))
}

/// The partition named `name` on the disk `disk`
/// (`/sys/block/<disk>/<partition>/uevent`), the first by its node's name
/// should the disk carry the name twice.
pub fn find_partition_on(disk: &str, name: &str) -> Option<PathBuf> {
    find_partition_on_in(Path::new(SYS_BLOCK), Path::new(DEV), disk, name)
}

/// [`find_partition_on`] over another sysfs tree and device directory (tests).
pub fn find_partition_on_in(sys_block: &Path, dev: &Path, disk: &str, name: &str) -> Option<PathBuf> {
    // A disk's name is a kernel name: one path component, never a path.
    if disk.is_empty() || disk.contains('/') || disk == "." || disk == ".." {
        return None;
    }
    uevents_in(&sys_block.join(disk)).iter().find_map(|text| match parse_uevent(text) {
        (Some(d), Some(n)) if n == name => Some(dev.join(d)),
        _ => None,
    })
}

/// The disk a boot takes its slot, STATE and DATA from, by the partition
/// the boot menu named ([`crate::Cmdline::boot_partition`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootDisk {
    /// The one disk that holds a partition of that identifier.
    Named(String),
    /// Several disks hold one (a disk copied whole keeps its identifiers):
    /// the first in sysfs order is taken.
    NamedOnSeveral { taken: String, disks: Vec<String> },
    /// No disk here holds one.
    NotFound,
}

/// The disk that holds the partition whose GPT identifier is `identifier`,
/// letters compared without regard to case.
pub fn boot_disk_in(sys_class_block: &Path, sys_block: &Path, identifier: &str) -> BootDisk {
    let mut found: Vec<String> = Vec::new();
    for text in uevents_in(sys_class_block) {
        let (Some(node), _) = parse_uevent(&text) else { continue };
        if partuuid_of(&text).is_some_and(|uuid| uuid.eq_ignore_ascii_case(identifier)) {
            let disk = disks::parent_disk_in(sys_block, node);
            if !found.contains(&disk) {
                found.push(disk);
            }
        }
    }
    match found.len() {
        0 => BootDisk::NotFound,
        1 => BootDisk::Named(found.remove(0)),
        _ => BootDisk::NamedOnSeveral { taken: found[0].clone(), disks: found },
    }
}

/// [`boot_disk_in`] on this machine, asked every 100 ms until a disk holds
/// the partition or `timeout` has passed: a disk on USB appears seconds
/// after the kernel starts.
pub fn wait_for_boot_disk(identifier: &str, timeout: Duration) -> BootDisk {
    let deadline = Instant::now() + timeout;
    loop {
        let found = boot_disk_in(Path::new(SYS_CLASS_BLOCK), Path::new(SYS_BLOCK), identifier);
        if found != BootDisk::NotFound || Instant::now() >= deadline {
            return found;
        }
        std::thread::sleep(POLL);
    }
}

/// The first of `names` whose partition appears on `disk` (its name and
/// node), polling every 100 ms until `timeout`; `None` when none of them did.
pub fn wait_for_partition_on(disk: &str, names: &[&str], timeout: Duration) -> Option<(String, PathBuf)> {
    let deadline = Instant::now() + timeout;
    loop {
        for name in names {
            if let Some(path) = find_partition_on(disk, name)
                && path.exists()
            {
                return Some((name.to_string(), path));
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(POLL);
    }
}

/// The disk the root filesystem is on, when the root is a partition of a
/// disk: an installed system's slot. `None` for a root on a loop device
/// (the installer's boot), on a whole disk, or with no block device behind
/// it (a development run).
pub fn root_disk() -> Option<String> {
    root_disk_in(Path::new("/"), Path::new(SYS_DEV_BLOCK), Path::new(SYS_BLOCK))
}

/// [`root_disk`] for another root and other sysfs trees (tests).
pub fn root_disk_in(root: &Path, sys_dev_block: &Path, sys_block: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let device = std::fs::metadata(root).ok()?.dev() as libc::dev_t;
    let entry = sys_dev_block.join(format!("{}:{}", libc::major(device), libc::minor(device)));
    // A partition's entry has a `partition` file (its number); a whole
    // device's has none.
    if !entry.join("partition").exists() {
        return None;
    }
    let node = std::fs::read_link(&entry).ok()?.file_name()?.to_string_lossy().into_owned();
    let disk = disks::parent_disk_in(sys_block, &node);
    (disk != node).then_some(disk)
}

/// The partition named `name` of the running system: on the root
/// filesystem's own disk, and on no other, when the root is a partition of
/// a disk; over every disk, the first in sysfs order, when it is not. The
/// ESP whose `grubenv` a boot confirms, the slot an update writes and the
/// DATA partition that grows are found this way: a second disk with the
/// same layout is another installation's.
pub fn find_partition_beside_root(name: &str) -> Option<PathBuf> {
    find_partition_beside_root_in(Path::new("/"), Path::new(SYS_DEV_BLOCK), Path::new(SYS_BLOCK), Path::new(SYS_CLASS_BLOCK), Path::new(DEV), name)
}

/// [`find_partition_beside_root`] for another root and other trees (tests).
pub fn find_partition_beside_root_in(
    root: &Path,
    sys_dev_block: &Path,
    sys_block: &Path,
    sys_class_block: &Path,
    dev: &Path,
    name: &str,
) -> Option<PathBuf> {
    match root_disk_in(root, sys_dev_block, sys_block) {
        Some(disk) => find_partition_on_in(sys_block, dev, &disk, name),
        None => find_partition_in(sys_class_block, dev, name),
    }
}

/// `DEVNAME=` and `PARTNAME=` of a uevent file.
pub fn parse_uevent(text: &str) -> (Option<&str>, Option<&str>) {
    let mut devname = None;
    let mut partname = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("DEVNAME=") {
            devname = Some(v);
        } else if let Some(v) = line.strip_prefix("PARTNAME=") {
            partname = Some(v);
        }
    }
    (devname, partname)
}

/// `PARTUUID=` of a uevent file: a GPT partition's identifier, as the
/// kernel writes it (lowercase).
pub fn partuuid_of(text: &str) -> Option<&str> {
    text.lines().find_map(|line| line.strip_prefix("PARTUUID="))
}

/// The first of `names` whose partition appears (its name and node), polling
/// every 100 ms until `timeout`; `None` when none of them did.
pub fn wait_for_partition(names: &[&str], timeout: Duration) -> Option<(String, PathBuf)> {
    let deadline = Instant::now() + timeout;
    loop {
        for name in names {
            if let Some(path) = find_partition(name)
                && path.exists()
            {
                return Some((name.to_string(), path));
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(POLL);
    }
}

/// The size of a block device in bytes, from sysfs (`size` counts 512-byte sectors).
pub fn size_bytes(dev: &Path) -> io::Result<u64> {
    size_bytes_in(Path::new(SYS_CLASS_BLOCK), dev)
}

pub fn size_bytes_in(sys_class_block: &Path, dev: &Path) -> io::Result<u64> {
    let node = dev
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("{} names no device", dev.display())))?;
    let text = std::fs::read_to_string(sys_class_block.join(node).join("size"))?;
    let sectors: u64 = text
        .trim()
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("sysfs size of {}: {text:?}", dev.display())))?;
    Ok(sectors * 512)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_sysfs(dir: &Path) {
        for (node, uevent) in [
            ("vda", "MAJOR=254\nMINOR=0\nDEVNAME=vda\nDEVTYPE=disk\n"),
            ("vda1", "MAJOR=254\nMINOR=1\nDEVNAME=vda1\nDEVTYPE=partition\nPARTN=1\nPARTNAME=esp\n"),
            ("vda2", "MAJOR=254\nMINOR=2\nDEVNAME=vda2\nDEVTYPE=partition\nPARTN=2\nPARTNAME=rootfs_a\n"),
            ("vda4", "MAJOR=254\nMINOR=4\nDEVNAME=vda4\nDEVTYPE=partition\nPARTN=4\nPARTNAME=state\n"),
            ("loop0", "MAJOR=7\nMINOR=0\nDEVNAME=loop0\nDEVTYPE=disk\n"),
        ] {
            let d = dir.join(node);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("uevent"), uevent).unwrap();
        }
        std::fs::write(dir.join("vda2").join("size"), "2097152\n").unwrap();
    }

    #[test]
    fn a_partition_is_found_by_its_gpt_name_and_an_unnamed_disk_is_skipped() {
        let tmp = std::env::temp_dir().join(format!("embra-boot-sysfs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        fake_sysfs(&tmp);
        let dev = Path::new("/dev");
        assert_eq!(find_partition_in(&tmp, dev, "rootfs_a"), Some(PathBuf::from("/dev/vda2")));
        assert_eq!(find_partition_in(&tmp, dev, "state"), Some(PathBuf::from("/dev/vda4")));
        assert_eq!(find_partition_in(&tmp, dev, "esp"), Some(PathBuf::from("/dev/vda1")));
        assert_eq!(find_partition_in(&tmp, dev, "rootfs_b"), None, "a hole has no node yet");
        assert_eq!(find_partition_in(&tmp, dev, "vda"), None, "a disk has no PARTNAME");
        assert_eq!(find_partition_in(Path::new("/nonexistent"), dev, "state"), None);
        assert_eq!(size_bytes_in(&tmp, Path::new("/dev/vda2")).unwrap(), 2097152 * 512);
        assert!(size_bytes_in(&tmp, Path::new("/dev/vda9")).is_err());
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// A partition of the stand-in tree: its number, its GPT name, its identifier.
    type Part = (u32, &'static str, &'static str);

    /// A stand-in for sysfs with the disks of `layout`, as the kernel lays
    /// them out: `block/<disk>/<partition>/{uevent,partition}`, and every
    /// disk and partition once more under `class/` as a link to its entry.
    fn disks_tree(dir: &Path, layout: &[(&str, &[Part])]) -> (PathBuf, PathBuf) {
        let _ = std::fs::remove_dir_all(dir);
        let (block, class) = (dir.join("block"), dir.join("class"));
        std::fs::create_dir_all(&class).unwrap();
        for (disk, parts) in layout {
            let d = block.join(disk);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("uevent"), format!("MAJOR=254\nMINOR=0\nDEVNAME={disk}\nDEVTYPE=disk\n")).unwrap();
            std::os::unix::fs::symlink(&d, class.join(disk)).unwrap();
            for (number, name, id) in parts.iter() {
                let node = format!("{disk}{number}");
                let pd = d.join(&node);
                std::fs::create_dir_all(&pd).unwrap();
                std::fs::write(pd.join("partition"), format!("{number}\n")).unwrap();
                std::fs::write(pd.join("uevent"), format!("MAJOR=254\nMINOR={number}\nDEVNAME={node}\nDEVTYPE=partition\nPARTN={number}\nPARTNAME={name}\nPARTUUID={id}\n")).unwrap();
                std::os::unix::fs::symlink(&pd, class.join(&node)).unwrap();
            }
        }
        (block, class)
    }

    /// Two installed disks: the same five names, identifiers of their own.
    const FIRST: &[Part] = &[
        (1, "esp", "11111111-0000-4000-8000-000000000001"),
        (2, "rootfs_a", "11111111-0000-4000-8000-000000000002"),
        (3, "rootfs_b", "11111111-0000-4000-8000-000000000003"),
        (4, "state", "11111111-0000-4000-8000-000000000004"),
        (5, "data", "11111111-0000-4000-8000-000000000005"),
    ];
    const SECOND: &[Part] = &[
        (1, "esp", "2222aaaa-0000-4000-8000-000000000001"),
        (2, "rootfs_a", "2222aaaa-0000-4000-8000-000000000002"),
        (3, "rootfs_b", "2222aaaa-0000-4000-8000-000000000003"),
        (4, "state", "2222aaaa-0000-4000-8000-000000000004"),
        (5, "data", "2222aaaa-0000-4000-8000-000000000005"),
    ];

    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("embra-boot-disks-{tag}-{}", std::process::id()))
    }

    /// A name alone says nothing of the disk: the lookup over every disk
    /// takes the first, as it always did, and the disks that carry the name
    /// can be listed, the one it takes first.
    #[test]
    fn a_name_over_every_disk_takes_the_first_and_the_disks_that_carry_it_are_listed() {
        let dir = scratch("names");
        let (block, class) = disks_tree(&dir, &[("vda", FIRST), ("vdb", SECOND), ("vdc", &[(1, "other", "33333333-0000-4000-8000-000000000001")])]);
        let dev = Path::new("/dev");
        for name in ["esp", "rootfs_a", "rootfs_b", "state", "data"] {
            let first = find_partition_in(&class, dev, name).unwrap();
            assert!(first.to_string_lossy().starts_with("/dev/vda"), "{name}: {first:?}");
            let all = partitions_named_in(&class, dev, name);
            assert_eq!(all.len(), 2, "{name}: {all:?}");
            assert_eq!(all[0], first);
            assert_eq!(disks_with_in(&class, &block, name), ["vda", "vdb"], "{name}");
        }
        assert_eq!(disks_with_in(&class, &block, "other"), ["vdc"]);
        assert!(disks_with_in(&class, &block, "rootfs").is_empty());
        // A disk that carries a name twice is one disk, not two.
        let twice: &[Part] = &[(1, "esp", "44444444-0000-4000-8000-000000000001"), (2, "esp", "44444444-0000-4000-8000-000000000002")];
        let (block, class) = disks_tree(&dir, &[("vda", twice), ("vdb", SECOND)]);
        assert_eq!(partitions_named_in(&class, dev, "esp").len(), 3);
        assert_eq!(disks_with_in(&class, &block, "esp"), ["vda", "vdb"]);
        assert_eq!(disks_with_in(&class, &block, "state"), ["vdb"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The warning of a second disk is one sentence after another, with the
    /// disks, the one taken and what to do; one disk is no warning.
    #[test]
    fn a_second_disk_with_the_layout_is_a_warning_that_names_the_disks_and_the_way_out() {
        assert_eq!(several_disks_warning(&[]), None);
        assert_eq!(several_disks_warning(&["vda".to_string()]), None);
        assert_eq!(
            several_disks_warning(&["nvme0n1".to_string(), "nvme1n1".to_string()]).as_deref(),
            Some(
                "2 disks here carry the embraOS layout (nvme0n1, nvme1n1), and this boot could not tell which one it was started from: \
                 it takes nvme0n1, the first. The boot menu of a current release names its disk; an upgrade in place from the installation medium writes that menu"
            )
        );
        let three = several_disks_warning(&["sda".to_string(), "sdb".to_string(), "sdc".to_string()]).unwrap();
        assert!(three.starts_with("3 disks here carry the embraOS layout (sda, sdb, sdc), "), "{three}");
        assert!(!three.contains("  "), "two spaces in a row: {three:?}");
    }

    /// The disk of a boot is the one that holds the partition its menu
    /// named, wherever that disk stands among the others.
    #[test]
    fn the_boot_disk_is_the_one_that_holds_the_partition_the_menu_named() {
        let dir = scratch("named");
        let (block, class) = disks_tree(&dir, &[("vda", FIRST), ("vdb", SECOND)]);
        assert_eq!(boot_disk_in(&class, &block, "2222aaaa-0000-4000-8000-000000000001"), BootDisk::Named("vdb".into()));
        assert_eq!(boot_disk_in(&class, &block, "11111111-0000-4000-8000-000000000001"), BootDisk::Named("vda".into()));
        // GRUB and the kernel both write lowercase; a line typed by hand may not.
        assert_eq!(boot_disk_in(&class, &block, "2222AAAA-0000-4000-8000-000000000001"), BootDisk::Named("vdb".into()));
        // Any partition of the disk names the disk: the menu names its own, the first.
        assert_eq!(boot_disk_in(&class, &block, "2222aaaa-0000-4000-8000-000000000004"), BootDisk::Named("vdb".into()));
        // A partition no disk here holds.
        assert_eq!(boot_disk_in(&class, &block, "99999999-0000-4000-8000-000000000001"), BootDisk::NotFound);
        assert_eq!(boot_disk_in(Path::new("/nonexistent"), &block, "2222aaaa-0000-4000-8000-000000000001"), BootDisk::NotFound);
        // A disk copied whole keeps its identifiers: both are named, the first is taken.
        let (block, class) = disks_tree(&dir, &[("sda", FIRST), ("sdb", SECOND), ("sdc", FIRST)]);
        assert_eq!(
            boot_disk_in(&class, &block, "11111111-0000-4000-8000-000000000001"),
            BootDisk::NamedOnSeveral { taken: "sda".into(), disks: vec!["sda".into(), "sdc".into()] }
        );
        assert_eq!(boot_disk_in(&class, &block, "2222aaaa-0000-4000-8000-000000000001"), BootDisk::Named("sdb".into()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// With the disk known, a partition is that disk's or it is none: the
    /// other disk's partition of the same name is never taken in its place.
    #[test]
    fn a_partition_is_taken_from_the_boots_own_disk_and_from_no_other() {
        let dir = scratch("on");
        // The second disk lacks slot B, as a disk does that was installed and never updated... and here STATE too.
        let partial: &[Part] = &[(1, "esp", "2222aaaa-0000-4000-8000-000000000001"), (2, "rootfs_a", "2222aaaa-0000-4000-8000-000000000002"), (5, "data", "2222aaaa-0000-4000-8000-000000000005")];
        let (block, _class) = disks_tree(&dir, &[("nvme0n", FIRST), ("nvme1n", partial)]);
        let dev = Path::new("/dev");
        assert_eq!(find_partition_on_in(&block, dev, "nvme1n", "rootfs_a"), Some(PathBuf::from("/dev/nvme1n2")));
        assert_eq!(find_partition_on_in(&block, dev, "nvme1n", "data"), Some(PathBuf::from("/dev/nvme1n5")));
        assert_eq!(find_partition_on_in(&block, dev, "nvme0n", "state"), Some(PathBuf::from("/dev/nvme0n4")));
        // The first disk has both; the second disk's lookup does not see them.
        assert_eq!(find_partition_on_in(&block, dev, "nvme1n", "rootfs_b"), None);
        assert_eq!(find_partition_on_in(&block, dev, "nvme1n", "state"), None);
        // A disk that is not there, and a name that is no kernel name.
        assert_eq!(find_partition_on_in(&block, dev, "nvme9n", "esp"), None);
        for no_disk in ["", ".", "..", "nvme0n/nvme0n1", "../block/nvme0n"] {
            assert_eq!(find_partition_on_in(&block, dev, no_disk, "esp"), None, "{no_disk:?}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The running system's partitions are the ones beside its root: the
    /// second disk's when the root is the second disk's slot. A root that is
    /// no partition of a disk leaves the lookup over every disk.
    #[test]
    fn the_running_system_finds_its_partitions_on_the_roots_own_disk() {
        use std::os::unix::fs::MetadataExt;
        let dir = scratch("root");
        let (block, class) = disks_tree(&dir, &[("vda", FIRST), ("vdb", SECOND)]);
        let dev = Path::new("/dev");
        // The stand-in root is this directory; its device number is the number the kernel's table is asked for.
        let root = dir.join("root");
        std::fs::create_dir_all(&root).unwrap();
        let device = std::fs::metadata(&root).unwrap().dev() as libc::dev_t;
        let numbers = dir.join("dev-block");
        std::fs::create_dir_all(&numbers).unwrap();
        let entry = numbers.join(format!("{}:{}", libc::major(device), libc::minor(device)));

        // Nothing behind the root: a development run.
        assert_eq!(root_disk_in(&root, &numbers, &block), None);
        assert_eq!(find_partition_beside_root_in(&root, &numbers, &block, &class, dev, "esp"), Some(PathBuf::from("/dev/vda1")));

        // The root is slot B of the second disk.
        std::os::unix::fs::symlink(block.join("vdb").join("vdb3"), &entry).unwrap();
        assert_eq!(root_disk_in(&root, &numbers, &block), Some("vdb".into()));
        for (name, node) in [("esp", "/dev/vdb1"), ("rootfs_a", "/dev/vdb2"), ("rootfs_b", "/dev/vdb3"), ("state", "/dev/vdb4"), ("data", "/dev/vdb5")] {
            assert_eq!(find_partition_beside_root_in(&root, &numbers, &block, &class, dev, name), Some(PathBuf::from(node)), "{name}");
        }
        // A name the root's disk lacks is none: the first disk has one, and it is another installation's.
        std::fs::remove_dir_all(block.join("vdb").join("vdb1")).unwrap();
        assert_eq!(find_partition_beside_root_in(&root, &numbers, &block, &class, dev, "esp"), None);

        // A root on a whole device (the installer's loop device) is on no disk's partition.
        std::fs::remove_file(&entry).unwrap();
        std::os::unix::fs::symlink(block.join("vdb"), &entry).unwrap();
        assert_eq!(root_disk_in(&root, &numbers, &block), None);
        assert_eq!(find_partition_beside_root_in(&root, &numbers, &block, &class, dev, "state"), Some(PathBuf::from("/dev/vda4")));

        // So is a root on a device the table of disks does not list and whose name ends in a digit:
        // the kernel says what is a partition (its `partition` file), the name does not.
        std::fs::remove_file(&entry).unwrap();
        let unlisted = dir.join("devices").join("virtual").join("zzz9");
        std::fs::create_dir_all(&unlisted).unwrap();
        std::os::unix::fs::symlink(&unlisted, &entry).unwrap();
        assert_eq!(root_disk_in(&root, &numbers, &block), None);
        assert_eq!(find_partition_beside_root_in(&root, &numbers, &block, &class, dev, "data"), Some(PathBuf::from("/dev/vda5")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_uevent_yields_its_device_and_partition_names() {
        assert_eq!(partuuid_of("DEVNAME=vda2\nPARTNAME=rootfs_a\nPARTUUID=0fc63daf-8483-4772-8e79-3d69d8477de4\n"), Some("0fc63daf-8483-4772-8e79-3d69d8477de4"));
        assert_eq!(partuuid_of("DEVNAME=vda\nDEVTYPE=disk\n"), None);
        assert_eq!(parse_uevent("DEVNAME=vda2\nPARTNAME=rootfs_a\n"), (Some("vda2"), Some("rootfs_a")));
        assert_eq!(parse_uevent("DEVNAME=vda\n"), (Some("vda"), None));
        assert_eq!(parse_uevent(""), (None, None));
    }

    #[test]
    fn waiting_for_a_name_no_disk_carries_times_out_empty() {
        let started = Instant::now();
        assert_eq!(wait_for_partition(&["no-such-embraos-partition"], Duration::from_millis(250)), None);
        assert!(started.elapsed() >= Duration::from_millis(250));
    }
}
