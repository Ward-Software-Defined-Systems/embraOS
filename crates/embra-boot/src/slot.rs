//! The two OS slots and the GPT partition names of the image.

use std::fmt;
use std::str::FromStr;

/// The EFI System Partition: GRUB, `grub.cfg` and `grubenv`.
pub const ESP_PARTITION: &str = "esp";
/// STATE: the sealed hash, the CA, the operator's settings.
pub const STATE_PARTITION: &str = "state";
/// DATA: the database, the workspace, the backups.
pub const DATA_PARTITION: &str = "data";
/// The one rootfs of a Phase 1 disk, kept as the fallback name a boot
/// without `embra.slot=` tries after slot A.
pub const LEGACY_ROOTFS_PARTITION: &str = "rootfs";
/// Every partition of the image in GPT order; the same list as
/// `buildroot/board/embraos/layout.sh` (`the_partition_names_are_the_ones_genimage_writes`).
pub const PARTITION_NAMES: [&str; 5] =
    [ESP_PARTITION, "rootfs_a", "rootfs_b", STATE_PARTITION, DATA_PARTITION];

/// An OS slot: one squashfs partition named `rootfs_<letter>` that carries
/// its own kernel and initramfs under `/boot`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Slot {
    A,
    B,
}

impl Slot {
    /// `a` or `b`, nothing else.
    pub fn parse(s: &str) -> Option<Slot> {
        match s {
            "a" => Some(Slot::A),
            "b" => Some(Slot::B),
            _ => None,
        }
    }

    pub fn other(self) -> Slot {
        match self {
            Slot::A => Slot::B,
            Slot::B => Slot::A,
        }
    }

    pub fn letter(self) -> char {
        match self {
            Slot::A => 'a',
            Slot::B => 'b',
        }
    }

    /// The GPT name of the slot's partition.
    pub fn partition_name(self) -> &'static str {
        match self {
            Slot::A => "rootfs_a",
            Slot::B => "rootfs_b",
        }
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.letter())
    }
}

impl FromStr for Slot {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Slot::parse(s).ok_or_else(|| format!("not a slot: '{s}' (a or b)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn a_slot_names_its_partition_and_its_other() {
        assert_eq!(Slot::A.partition_name(), "rootfs_a");
        assert_eq!(Slot::B.partition_name(), "rootfs_b");
        assert_eq!(Slot::A.other(), Slot::B);
        assert_eq!(Slot::B.other(), Slot::A);
        assert_eq!(Slot::parse("a"), Some(Slot::A));
        assert_eq!(Slot::parse("b"), Some(Slot::B));
        assert_eq!(Slot::parse("A"), None);
        assert_eq!(Slot::parse("c"), None);
        assert_eq!("b".parse::<Slot>(), Ok(Slot::B));
        assert_eq!(Slot::A.to_string(), "a");
        assert!(PARTITION_NAMES.contains(&Slot::A.partition_name()));
        assert!(PARTITION_NAMES.contains(&Slot::B.partition_name()));
    }

    /// The names the OS looks partitions up by are the names genimage
    /// writes: `layout.sh` lists them in order and `genimage.cfg.in`
    /// declares a `partition <name>` block for each, in the same order.
    #[test]
    fn the_partition_names_are_the_ones_genimage_writes() {
        let board = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../buildroot/board/embraos");
        let layout = std::fs::read_to_string(board.join("layout.sh")).expect("layout.sh");
        let names = layout
            .lines()
            .find_map(|l| l.strip_prefix("EMBRAOS_PARTITION_NAMES="))
            .expect("EMBRAOS_PARTITION_NAMES in layout.sh")
            .trim()
            .trim_matches('"')
            .to_string();
        assert_eq!(names, PARTITION_NAMES.join(" "));

        let cfg = std::fs::read_to_string(board.join("genimage.cfg.in")).expect("genimage.cfg.in");
        let declared: Vec<&str> = cfg
            .lines()
            .filter_map(|l| l.trim().strip_prefix("partition "))
            .map(|rest| rest.trim_end_matches('{').trim())
            .collect();
        assert_eq!(declared, PARTITION_NAMES.to_vec());
    }
}
