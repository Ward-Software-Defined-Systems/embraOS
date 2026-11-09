//! `embra-update grow-data`: DATA takes the rest of its disk.
//!
//! An installed disk, or a VM disk enlarged after the fact, has free space
//! after DATA, the last partition. At every normal boot, before the
//! services (embrad runs this with the database stopped), the partition is
//! grown in place when at least a gibibyte follows it: sgdisk moves the
//! backup GPT to the disk's end and recreates the entry from the same
//! start to the last usable sector, the kernel is told the new length
//! (`BLKPG_RESIZE_PARTITION`, which works on a mounted partition), and
//! resize2fs grows the filesystem online. One line says what happened;
//! nothing is said when there is nothing to do, and a failure is a line
//! on the consoles and a normal boot: the filesystem that was there is
//! still there.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use embra_boot::{disks, partitions};

use crate::console::Operator;
use crate::Outcome;

/// The whole disks, each partition a directory under its disk.
const SYS_BLOCK: &str = "/sys/block";

/// The least free run after DATA worth a growth: smaller tails are the
/// rounding of a disk's size, not an enlarged disk.
pub const MIN_GAIN_BYTES: u64 = 1 << 30;

/// The growth to make, from a partition's geometry; `None` when DATA is
/// not the last partition or less than `min_gain` follows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub disk: String,
    pub partition: String,
    pub number: u32,
    pub start_sectors: u64,
    pub old_len_sectors: u64,
    pub gain_sectors: u64,
}

pub fn plan(g: &disks::Geometry, min_gain: u64) -> Option<Plan> {
    if !g.last {
        return None;
    }
    let gain = g.free_after_sectors();
    if gain * 512 < min_gain {
        return None;
    }
    Some(Plan {
        disk: g.disk.clone(),
        partition: g.partition.clone(),
        number: g.number,
        start_sectors: g.start_sectors,
        old_len_sectors: g.len_sectors,
        gain_sectors: gain,
    })
}

/// The last sector of a partition as `sgdisk -i <n>` prints it
/// ("Last sector: 125829086 (at 60.0 GiB)"), the figure the kernel is told.
pub fn last_sector_from_info(info: &str) -> Option<u64> {
    info.lines()
        .find_map(|l| l.trim().strip_prefix("Last sector:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
}

fn gib(sectors: u64) -> String {
    format!("{:.1} GiB", sectors as f64 * 512.0 / (1u64 << 30) as f64)
}

/// The command. `Done` and silent when there is nothing to grow (every boot
/// but the one after a disk grew); a line and `Nothing` when DATA or its
/// geometry cannot be found, a line and `Failed` when a step failed.
pub fn grow_data_command(op: &mut Operator) -> Outcome {
    let Some(data) = partitions::find_partition(embra_boot::slot::DATA_PARTITION) else {
        return Outcome::Nothing("no DATA partition by name; nothing to grow".into());
    };
    let node = data.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let geometry = match disks::geometry_in(Path::new(SYS_BLOCK), &node) {
        Ok(g) => g,
        Err(e) => return Outcome::Nothing(format!("DATA's geometry could not be read: {e}")),
    };
    let Some(p) = plan(&geometry, MIN_GAIN_BYTES) else {
        // The usual boot: nothing to say, nothing for embrad to log.
        return Outcome::Done;
    };
    match grow(&p, op) {
        Ok(new_len) => {
            op.line(&format!(
                "DATA grew from {} to {} on {}: the disk's end",
                gib(p.old_len_sectors),
                gib(new_len),
                p.disk
            ));
            Outcome::Done
        }
        Err(e) => {
            op.line(&format!("DATA was not grown ({e}); the partition is as it was"));
            Outcome::Failed(e)
        }
    }
}

fn tool(candidates: &[&str]) -> Result<PathBuf, String> {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .ok_or_else(|| format!("{} is not in this image", candidates[0]))
}

fn run(tool_path: &Path, args: &[String]) -> Result<String, String> {
    let out = Command::new(tool_path)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{} could not be run: {e}", tool_path.display()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let last = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
        return Err(format!("{} {} failed ({}): {last}", tool_path.display(), args.join(" "), out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The three steps; returns the new length in sectors.
fn grow(p: &Plan, op: &mut Operator) -> Result<u64, String> {
    let sgdisk = tool(&["/usr/sbin/sgdisk", "/sbin/sgdisk", "/usr/bin/sgdisk"])?;
    let resize2fs = tool(&["/sbin/resize2fs", "/usr/sbin/resize2fs"])?;
    let disk_dev = format!("/dev/{}", p.disk);
    let part_dev = format!("/dev/{}", p.partition);
    op.line(&format!("DATA: {} follow it on {}; growing the partition to the disk's end", gib(p.gain_sectors), p.disk));
    // 1. The table: the backup GPT to the disk's end, the entry recreated
    //    from the same start to the last usable sector, name and type kept.
    let n = p.number;
    run(&sgdisk, &["-e".to_string(), disk_dev.clone()])?;
    run(
        &sgdisk,
        &[
            "-d".to_string(),
            n.to_string(),
            "-n".to_string(),
            format!("{n}:{}:0", p.start_sectors),
            "-c".to_string(),
            format!("{n}:{}", embra_boot::slot::DATA_PARTITION),
            "-t".to_string(),
            format!("{n}:8300"),
            disk_dev.clone(),
        ],
    )?;
    let info = run(&sgdisk, &["-i".to_string(), n.to_string(), disk_dev.clone()])?;
    let last = last_sector_from_info(&info).ok_or_else(|| format!("sgdisk -i {n} named no last sector"))?;
    let new_len = last + 1 - p.start_sectors;
    // 2. The kernel's view of the partition, while it is mounted.
    blkpg_resize(&disk_dev, n, p.start_sectors * 512, new_len * 512)?;
    // 3. The filesystem, online.
    run(&resize2fs, &[part_dev])?;
    Ok(new_len)
}

/// `BLKPG` with `BLKPG_RESIZE_PARTITION` on the whole disk: the in-core
/// partition takes the new length. `<linux/blkpg.h>`; libc has neither the
/// request nor the structures.
#[cfg(target_os = "linux")]
fn blkpg_resize(disk_dev: &str, number: u32, start_bytes: u64, len_bytes: u64) -> Result<(), String> {
    use std::os::unix::io::AsRawFd;
    const BLKPG: libc::c_ulong = 0x1269;
    const BLKPG_RESIZE_PARTITION: libc::c_int = 3;
    #[repr(C)]
    struct BlkpgPartition {
        start: libc::c_longlong,
        length: libc::c_longlong,
        pno: libc::c_int,
        devname: [u8; 64],
        volname: [u8; 64],
    }
    #[repr(C)]
    struct BlkpgIoctlArg {
        op: libc::c_int,
        flags: libc::c_int,
        datalen: libc::c_int,
        data: *mut libc::c_void,
    }
    let f = std::fs::OpenOptions::new().read(true).write(true).open(disk_dev).map_err(|e| format!("{disk_dev}: {e}"))?;
    let mut part = BlkpgPartition {
        start: start_bytes as libc::c_longlong,
        length: len_bytes as libc::c_longlong,
        pno: number as libc::c_int,
        devname: [0; 64],
        volname: [0; 64],
    };
    let mut arg = BlkpgIoctlArg {
        op: BLKPG_RESIZE_PARTITION,
        flags: 0,
        datalen: std::mem::size_of::<BlkpgPartition>() as libc::c_int,
        data: (&mut part as *mut BlkpgPartition).cast(),
    };
    // SAFETY: the two structures are the kernel's, laid out as repr(C), and
    // live for the call; the descriptor is the open disk's.
    let r = unsafe { libc::ioctl(f.as_raw_fd(), BLKPG as _, &mut arg as *mut BlkpgIoctlArg) };
    if r == -1 {
        return Err(format!("BLKPG resize of {disk_dev} partition {number}: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn blkpg_resize(_disk_dev: &str, _number: u32, _start_bytes: u64, _len_bytes: u64) -> Result<(), String> {
    Err("BLKPG is Linux's".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(len: u64, disk: u64, last: bool) -> disks::Geometry {
        disks::Geometry { disk: "vda".into(), partition: "vda5".into(), number: 5, start_sectors: 4_851_712, len_sectors: len, disk_sectors: disk, last }
    }

    /// The disk as installed grows nothing; a gibibyte after DATA does; a
    /// partition after DATA stops it.
    #[test]
    fn grow_data_does_nothing_without_a_gigabyte_after_data() {
        let start = 4_851_712u64;
        let disk40 = 83_886_080u64;
        assert_eq!(plan(&geometry(disk40 - 33 - start, disk40, true), MIN_GAIN_BYTES), None, "as installed");
        let half = (1u64 << 29) / 512;
        assert_eq!(plan(&geometry(disk40 - 33 - start, disk40 + half, true), MIN_GAIN_BYTES), None, "half a gibibyte is rounding");
        let disk60 = 125_829_120u64;
        let p = plan(&geometry(disk40 - 33 - start, disk60, true), MIN_GAIN_BYTES).expect("20 GiB after DATA");
        assert_eq!((p.number, p.start_sectors, p.gain_sectors), (5, start, disk60 - disk40));
        assert_eq!(plan(&geometry(disk40 - 33 - start, disk60, false), MIN_GAIN_BYTES), None, "something after DATA");
    }

    #[test]
    fn the_new_length_is_read_from_sgdisk_info() {
        let info = "Partition GUID code: 0FC63DAF-8483-4772-8E79-3D69D8477DE4 (Linux filesystem)\nPartition unique GUID: ...\nFirst sector: 4851712 (at 2.3 GiB)\nLast sector: 125829086 (at 60.0 GiB)\nPartition size: 120977375 sectors (57.7 GiB)\nAttribute flags: 0000000000000000\nPartition name: 'data'\n";
        assert_eq!(last_sector_from_info(info), Some(125_829_086));
        assert_eq!(last_sector_from_info("no such partition"), None);
    }
}
