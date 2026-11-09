//! The disk layout the image is built with (`buildroot/board/embraos/layout.sh`):
//! the sizes the installer lays a disk out with, in order, and the smallest
//! disk that holds them. The constants here equal layout.sh's; a test reads
//! the file. An installed disk gives DATA the rest of the disk, so only the
//! image's DATA size bounds the smallest disk.

use crate::slot::PARTITION_NAMES;

pub const ESP_MIB: u64 = 64;
pub const SLOT_MIB: u64 = 2048;
pub const STATE_MIB: u64 = 256;
/// The image's DATA; an installed disk's DATA is the rest of the disk.
pub const DATA_MIB: u64 = 4096;
/// The image: 1 MiB in front, the partitions, 1 MiB holding the backup GPT.
pub const IMAGE_MIB: u64 = 1 + ESP_MIB + 2 * SLOT_MIB + STATE_MIB + DATA_MIB + 1;
/// The smallest disk the installer writes: the image's size.
pub const MIN_DISK_MIB: u64 = IMAGE_MIB;

pub const MIB: u64 = 1024 * 1024;

/// Where each partition starts, in MiB from the disk's start: 1 MiB in
/// front, then the partitions in order (DATA last, to the end of the disk).
pub fn planned_starts_mib() -> [u64; 5] {
    let esp = 1;
    let a = esp + ESP_MIB;
    let b = a + SLOT_MIB;
    let state = b + SLOT_MIB;
    let data = state + STATE_MIB;
    [esp, a, b, state, data]
}

/// The device of partition `n` of `disk`: `sda` → `sda1`, `nvme0n1` →
/// `nvme0n1p1` (a `p` when the disk's name ends in a digit).
pub fn partition_dev(disk: &str, n: u32) -> String {
    if disk.ends_with(|c: char| c.is_ascii_digit()) { format!("{disk}p{n}") } else { format!("{disk}{n}") }
}

/// sgdisk's type codes: the EFI System Partition and Linux filesystem data.
const ESP_TYPE: &str = "ef00";
const LINUX_TYPE: &str = "8300";

/// The arguments that lay the five partitions out on `disk` with sgdisk, 1 MiB
/// aligned (`-a 2048`), in layout.sh's order and sizes, DATA to the end
/// (`-n 5:0:0`). `sgdisk --zap-all <disk>` goes first, separately.
pub fn sgdisk_create_args(disk: &str) -> Vec<String> {
    let sizes: [Option<u64>; 5] = [Some(ESP_MIB), Some(SLOT_MIB), Some(SLOT_MIB), Some(STATE_MIB), None];
    let mut args = vec!["-a".to_string(), "2048".to_string()];
    for (i, (name, size)) in PARTITION_NAMES.iter().zip(sizes).enumerate() {
        let n = i + 1;
        let end = size.map(|m| format!("+{m}M")).unwrap_or_else(|| "0".to_string());
        args.push("-n".to_string());
        args.push(format!("{n}:0:{end}"));
        args.push("-t".to_string());
        args.push(format!("{n}:{}", if n == 1 { ESP_TYPE } else { LINUX_TYPE }));
        args.push("-c".to_string());
        args.push(format!("{n}:{name}"));
    }
    args.push(disk.to_string());
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// layout.sh's sizes in MiB, by its own rule (`embraos_mib`).
    fn mib(size: &str) -> u64 {
        let (n, unit) = size.split_at(size.len() - 1);
        let n: u64 = n.parse().expect("a number");
        match unit {
            "G" => n * 1024,
            "M" => n,
            "K" => n.div_ceil(1024),
            other => panic!("not a size unit: {other}"),
        }
    }

    fn setting(layout: &str, key: &str) -> String {
        layout
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| panic!("{key} in layout.sh"))
            .split('#')
            .next()
            .unwrap()
            .trim()
            .trim_matches('"')
            .to_string()
    }

    #[test]
    fn the_layout_constants_are_the_ones_layout_sh_declares() {
        let board = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../buildroot/board/embraos");
        let layout = std::fs::read_to_string(board.join("layout.sh")).expect("layout.sh");
        assert_eq!(mib(&setting(&layout, "EMBRAOS_ESP_SIZE")), ESP_MIB);
        assert_eq!(mib(&setting(&layout, "EMBRAOS_SLOT_SIZE")), SLOT_MIB);
        assert_eq!(mib(&setting(&layout, "EMBRAOS_STATE_SIZE")), STATE_MIB);
        assert_eq!(mib(&setting(&layout, "EMBRAOS_DATA_SIZE")), DATA_MIB);
        assert_eq!(IMAGE_MIB, 1 + ESP_MIB + 2 * SLOT_MIB + STATE_MIB + DATA_MIB + 1, "layout.sh's own formula");
    }

    #[test]
    fn the_planned_starts_follow_the_layout_and_a_partition_is_named_after_its_disk() {
        assert_eq!(planned_starts_mib(), [1, 65, 2113, 4161, 4417]);
        assert_eq!(partition_dev("sda", 1), "sda1");
        assert_eq!(partition_dev("vda", 5), "vda5");
        assert_eq!(partition_dev("nvme0n1", 2), "nvme0n1p2");
        assert_eq!(partition_dev("mmcblk0", 5), "mmcblk0p5");
    }

    #[test]
    fn the_sgdisk_arguments_name_the_five_partitions_in_order_with_data_to_the_end() {
        let args = sgdisk_create_args("/dev/sda");
        let names: Vec<&str> = args.iter().filter_map(|a| a.split_once(':').filter(|(n, _)| n.len() == 1).map(|(_, v)| v)).filter(|v| v.chars().all(|c| c.is_ascii_alphabetic() || c == '_')).collect();
        assert_eq!(names, PARTITION_NAMES.to_vec());
        assert_eq!(args[..2], ["-a", "2048"]);
        assert!(args.windows(2).any(|w| w == ["-n", "1:0:+64M"]));
        assert!(args.windows(2).any(|w| w == ["-t", "1:ef00"]));
        assert!(args.windows(2).any(|w| w == ["-n".to_string(), format!("2:0:+{SLOT_MIB}M")]));
        assert!(args.windows(2).any(|w| w == ["-n", "5:0:0"]), "DATA takes the rest");
        assert_eq!(args.last().map(String::as_str), Some("/dev/sda"));
    }
}
