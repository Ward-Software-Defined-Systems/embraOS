//! Partitions by their GPT name, from sysfs and without udev: the kernel
//! puts `PARTNAME=` into every GPT partition's uevent
//! (`/sys/class/block/<dev>/uevent`), and devtmpfs carries the node under
//! `/dev/<DEVNAME>`.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const SYS_CLASS_BLOCK: &str = "/sys/class/block";
pub const DEV: &str = "/dev";
const POLL: Duration = Duration::from_millis(100);

/// `/dev/<node>` of the partition named `name`, the first one in sysfs
/// order when two disks carry the name.
pub fn find_partition(name: &str) -> Option<PathBuf> {
    find_partition_in(Path::new(SYS_CLASS_BLOCK), Path::new(DEV), name)
}

/// [`find_partition`] over another sysfs tree and device directory (tests).
pub fn find_partition_in(sys_class_block: &Path, dev: &Path, name: &str) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> =
        std::fs::read_dir(sys_class_block).ok()?.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for entry in entries {
        let Ok(text) = std::fs::read_to_string(entry.join("uevent")) else { continue };
        let (devname, partname) = parse_uevent(&text);
        if partname == Some(name)
            && let Some(d) = devname
        {
            return Some(dev.join(d));
        }
    }
    None
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

    #[test]
    fn a_uevent_yields_its_device_and_partition_names() {
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
