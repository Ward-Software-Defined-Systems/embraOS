//! The whole disks an installer may write, from sysfs, each with the reason
//! it is not offered when there is one: it holds the installation medium,
//! something on it is mounted, or it is smaller than the layout.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disk {
    /// The kernel's name (`sda`, `nvme0n1`, `vda`).
    pub name: String,
    pub bytes: u64,
    /// Vendor and model as sysfs has them, or the bus's generic name.
    pub model: String,
    /// `usb`, `nvme`, `virtio`, `ata`, `scsi`, `mmc` or `disk`.
    pub bus: String,
    pub removable: bool,
    /// Why this disk is not offered; `None` offers it.
    pub excluded: Option<String>,
}

impl Disk {
    pub fn offered(&self) -> bool {
        self.excluded.is_none()
    }

    /// `40.0 GiB`.
    pub fn size_text(&self) -> String {
        format!("{:.1} GiB", self.bytes as f64 / (1u64 << 30) as f64)
    }
}

/// The whole disks under `/sys/block` (`sys_block`): the entries with a
/// `device` link, none of the optical, loop, RAM, zram, device-mapper, MD,
/// NBD and floppy names. `mounts` is `/proc/mounts`; `media_source` the
/// device the medium is mounted from (`/dev/sdb1`), whose disk is never
/// offered; `min_bytes` the smallest disk the layout fits.
pub fn list(sys_block: &Path, mounts: &str, media_source: Option<&str>, min_bytes: u64) -> Vec<Disk> {
    let Ok(dir) = std::fs::read_dir(sys_block) else { return Vec::new() };
    let mut names: Vec<String> = dir
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !["sr", "loop", "ram", "zram", "dm-", "md", "nbd", "fd"].iter().any(|p| n.starts_with(p)))
        .filter(|n| std::fs::symlink_metadata(sys_block.join(n).join("device")).is_ok())
        .collect();
    names.sort();
    let mounted: Vec<String> = mounts
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter_map(|dev| dev.strip_prefix("/dev/"))
        .map(str::to_string)
        .collect();
    let media_disk = media_source.and_then(|s| s.strip_prefix("/dev/")).map(|n| parent_disk_in(sys_block, n));
    names
        .into_iter()
        .map(|name| {
            let entry = sys_block.join(&name);
            let sectors = std::fs::read_to_string(entry.join("size")).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0);
            let bytes = sectors * 512;
            let read = |p: &str| std::fs::read_to_string(entry.join(p)).map(|s| s.trim().to_string()).unwrap_or_default();
            let bus = bus_of(&std::fs::read_link(entry.join("device")).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default());
            let vendor = read("device/vendor");
            let model = read("device/model");
            let model = match (vendor.is_empty(), model.is_empty()) {
                (true, true) => format!("{bus} disk"),
                (false, true) => vendor,
                (true, false) => model,
                (false, false) => format!("{vendor} {model}"),
            };
            let removable = read("removable") == "1";
            let excluded = if media_disk.as_deref() == Some(name.as_str()) {
                Some("holds the installation medium".to_string())
            } else if mounted.iter().any(|m| parent_disk_in(sys_block, m) == name) {
                Some("mounted".to_string())
            } else if bytes < min_bytes {
                Some(format!("too small: embraOS needs at least {} MiB", min_bytes / (1024 * 1024)))
            } else {
                None
            };
            Disk { name, bytes, model, bus, removable, excluded }
        })
        .collect()
}

/// The disk a block device name belongs to, by sysfs: a whole disk is an
/// entry of `/sys/block`, a partition a directory under its disk's entry
/// (`/sys/block/sdb/sdb1`, `/sys/block/nvme0n1/nvme0n1p2`). A name sysfs
/// does not have falls back to the spelling rule (`sda1` → `sda`,
/// `nvme0n1p2` → `nvme0n1`); a disk whose own name ends in a digit cannot be
/// told from a partition by spelling, which is why sysfs is asked first.
pub fn parent_disk_in(sys_block: &Path, name: &str) -> String {
    if sys_block.join(name).exists() {
        return name.to_string();
    }
    if let Ok(dir) = std::fs::read_dir(sys_block) {
        for entry in dir.flatten() {
            if entry.path().join(name).is_dir() {
                return entry.file_name().to_string_lossy().into_owned();
            }
        }
    }
    let trimmed = name.trim_end_matches(|c: char| c.is_ascii_digit());
    if trimmed.len() == name.len() || trimmed.is_empty() {
        return name.to_string();
    }
    if let Some(stem) = trimmed.strip_suffix('p')
        && stem.ends_with(|c: char| c.is_ascii_digit())
    {
        return stem.to_string();
    }
    trimmed.to_string()
}

/// The bus from the `device` link's path.
fn bus_of(link: &str) -> String {
    for (needle, bus) in [("/usb", "usb"), ("/nvme", "nvme"), ("/virtio", "virtio"), ("/ata", "ata"), ("/mmc", "mmc"), ("/host", "scsi")] {
        if link.contains(needle) {
            return bus.to_string();
        }
    }
    "disk".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A disk of the fake tree: `/sys/block/<name>` with its `device` link
    /// into a devices path that names the bus, as sysfs does, and resolves.
    fn disk(dir: &Path, name: &str, sectors: u64, bus_path: &str, (vendor, model): (&str, &str), removable: bool, partitions: &[&str]) {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("size"), format!("{sectors}\n")).unwrap();
        std::fs::write(d.join("removable"), if removable { "1\n" } else { "0\n" }).unwrap();
        let target = dir.join("devices").join(bus_path);
        std::fs::create_dir_all(&target).unwrap();
        if !vendor.is_empty() {
            std::fs::write(target.join("vendor"), format!("{vendor}\n")).unwrap();
        }
        if !model.is_empty() {
            std::fs::write(target.join("model"), format!("{model}\n")).unwrap();
        }
        symlink(format!("../devices/{bus_path}"), d.join("device")).unwrap();
        for part in partitions {
            std::fs::create_dir_all(d.join(part)).unwrap();
            std::fs::write(d.join(part).join("partition"), "1\n").unwrap();
        }
    }

    fn fake_tree() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("embra-boot-disks-{}-{}", std::process::id(), std::thread::current().name().unwrap_or("t").replace("::", "-")));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        disk(&dir, "sda", 83_886_080, "pci0000:00/0000:00:1f.2/ata1/host0/target0:0:0/0:0:0:0", ("ATA", "QEMU HARDDISK"), false, &["sda1", "sda2"]);
        disk(&dir, "nvme0n1", 1_000_215_216, "pci0000:00/0000:00:1d.0/nvme/nvme0", ("", "Samsung SSD 970"), false, &["nvme0n1p1", "nvme0n1p2"]);
        disk(&dir, "sdb", 15_630_336, "pci0000:00/0000:00:14.0/usb1/1-1/1-1:1.0/host2/target2:0:0/2:0:0:0", ("SanDisk", "Ultra"), true, &["sdb1"]);
        disk(&dir, "sdc", 7_815_168, "pci0000:00/0000:00:1f.2/ata2/host1/target1:0:0/1:0:0:0", ("", ""), false, &[]);
        disk(&dir, "vda", 41_943_040, "pci0000:00/0000:00:04.0/virtio1", ("", ""), false, &[]);
        disk(&dir, "mmcblk0", 61_071_360, "pci0000:00/0000:00:1a.0/mmc_host/mmc0/mmc0:0001", ("", ""), false, &["mmcblk0p1"]);
        for n in ["sr0", "loop0", "ram0", "zram0", "dm-0", "md127"] {
            std::fs::create_dir_all(dir.join(n)).unwrap();
            std::fs::write(dir.join(n).join("size"), "4096\n").unwrap();
            symlink("../devices/x", dir.join(n).join("device")).unwrap();
        }
        dir
    }

    #[test]
    fn disks_are_listed_from_sysfs_with_size_model_and_bus_and_the_media_s_disk_is_left_out() {
        let dir = fake_tree();
        let mounts = "/dev/sdb1 /run/embra/media iso9660 ro 0 0\n/dev/nvme0n1p2 /mnt ext4 rw 0 0\ntmpfs /run tmpfs rw 0 0\n";
        let min = 6466 * 1024 * 1024;
        let disks = list(&dir, mounts, Some("/dev/sdb1"), min);
        let names: Vec<&str> = disks.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["mmcblk0", "nvme0n1", "sda", "sdb", "sdc", "vda"], "sorted, the optical and virtual ones left out");
        let by = |n: &str| disks.iter().find(|d| d.name == n).unwrap();
        assert!(by("sda").offered());
        assert_eq!(by("sda").bus, "ata");
        assert_eq!(by("sda").model, "ATA QEMU HARDDISK");
        assert_eq!(by("sda").size_text(), "40.0 GiB");
        assert_eq!(by("nvme0n1").excluded.as_deref(), Some("mounted"));
        assert_eq!(by("nvme0n1").bus, "nvme");
        assert_eq!(by("nvme0n1").model, "Samsung SSD 970");
        assert_eq!(by("sdb").excluded.as_deref(), Some("holds the installation medium"));
        assert!(by("sdb").removable && by("sdb").bus == "usb");
        assert_eq!(by("sdc").excluded.as_deref(), Some("too small: embraOS needs at least 6466 MiB"));
        assert!(by("vda").offered());
        assert_eq!(by("vda").model, "virtio disk");
        assert_eq!(by("mmcblk0").bus, "mmc");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_partition_names_its_disk() {
        let dir = fake_tree();
        assert_eq!(parent_disk_in(&dir, "sda1"), "sda");
        assert_eq!(parent_disk_in(&dir, "sda"), "sda");
        assert_eq!(parent_disk_in(&dir, "nvme0n1p2"), "nvme0n1");
        assert_eq!(parent_disk_in(&dir, "nvme0n1"), "nvme0n1", "a disk whose name ends in a digit");
        assert_eq!(parent_disk_in(&dir, "mmcblk0p1"), "mmcblk0");
        assert_eq!(parent_disk_in(&dir, "mmcblk0"), "mmcblk0");
        // names sysfs does not have: the spelling rule
        assert_eq!(parent_disk_in(&dir, "vdb3"), "vdb");
        assert_eq!(parent_disk_in(&dir, "nvme1n1p3"), "nvme1n1");
        assert_eq!(parent_disk_in(&dir, "sdz"), "sdz");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Where a partition lies on its disk, from sysfs (`sys_block` is
/// `/sys/block`: the whole disks, each partition a directory under its
/// disk): sectors are the kernel's 512-byte ones. `last` says no other
/// partition of the disk starts after this one, the case a partition can
/// grow in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Geometry {
    pub disk: String,
    pub partition: String,
    pub number: u32,
    pub start_sectors: u64,
    pub len_sectors: u64,
    pub disk_sectors: u64,
    pub last: bool,
}

impl Geometry {
    /// The sectors between this partition's end and the disk's: what a
    /// growth to the disk's end would add (the backup GPT's 33 sectors
    /// stay out of it).
    pub fn free_after_sectors(&self) -> u64 {
        self.disk_sectors.saturating_sub(self.start_sectors + self.len_sectors + 33)
    }
}

pub fn geometry_in(sys_block: &Path, partition: &str) -> std::io::Result<Geometry> {
    let read = |path: std::path::PathBuf| -> std::io::Result<u64> {
        let text = std::fs::read_to_string(&path)?;
        text.trim()
            .parse::<u64>()
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: {text:?}", path.display())))
    };
    let disk = parent_disk_in(sys_block, partition);
    if disk == partition {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{partition} is a whole disk, not a partition")));
    }
    let disk_dir = sys_block.join(&disk);
    let part_dir = disk_dir.join(partition);
    let start_sectors = read(part_dir.join("start"))?;
    let len_sectors = read(part_dir.join("size"))?;
    let number = u32::try_from(read(part_dir.join("partition"))?).unwrap_or(0);
    let disk_sectors = read(disk_dir.join("size"))?;
    // The siblings: the disk's other partitions, by their `partition` file.
    let mut last = true;
    for entry in std::fs::read_dir(&disk_dir)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == partition || !entry.path().join("partition").exists() {
            continue;
        }
        if let Ok(other_start) = read(entry.path().join("start"))
            && other_start > start_sectors
        {
            last = false;
        }
    }
    Ok(Geometry { disk, partition: partition.to_string(), number, start_sectors, len_sectors, disk_sectors, last })
}

#[cfg(test)]
mod geometry_tests {
    use super::*;

    fn fake(dir: &Path, disk_sectors: u64, parts: &[(&str, u32, u64, u64)]) {
        // /sys/block/<disk>/<partition>/{start,size,partition}, as the
        // kernel lays it out; parent_disk_in walks it.
        let d = dir.join("vda");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("size"), disk_sectors.to_string()).unwrap();
        for (name, number, start, len) in parts {
            let pd = d.join(name);
            std::fs::create_dir_all(&pd).unwrap();
            std::fs::write(pd.join("partition"), number.to_string()).unwrap();
            std::fs::write(pd.join("start"), start.to_string()).unwrap();
            std::fs::write(pd.join("size"), len.to_string()).unwrap();
        }
    }

    /// DATA last on a 40 GiB disk the installer wrote, and the same disk
    /// enlarged to 60 GiB: the free run after it is what sysfs says.
    #[test]
    fn a_partitions_geometry_comes_from_sysfs_and_says_whether_it_is_last() {
        let dir = std::env::temp_dir().join(format!("embra-geom-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let data_start = 4_851_712u64;
        let disk40 = 83_886_080u64; // 40 GiB
        fake(&dir, disk40, &[("vda1", 1, 2048, 131_072), ("vda4", 4, 4_327_424, 524_288), ("vda5", 5, data_start, disk40 - 33 - data_start)]);
        let g = geometry_in(&dir, "vda5").unwrap();
        assert_eq!((g.disk.as_str(), g.number, g.start_sectors, g.last), ("vda", 5, data_start, true));
        assert_eq!(g.free_after_sectors(), 0, "nothing after DATA on the disk as installed");
        std::fs::write(dir.join("vda/size"), (125_829_120u64).to_string()).unwrap(); // 60 GiB now
        let g = geometry_in(&dir, "vda5").unwrap();
        assert_eq!(g.free_after_sectors(), 125_829_120 - 83_886_080, "the enlarged disk's tail");
        let g4 = geometry_in(&dir, "vda4").unwrap();
        assert!(!g4.last, "STATE has DATA after it");
        assert!(geometry_in(&dir, "vda").is_err(), "a whole disk has no geometry of its own");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
