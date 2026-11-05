//! The boot status embrad publishes at `/run/embra/boot-status` and every
//! other reader takes the A/B state from (embra-update, the brain's
//! `system_status`): the brain and the updater never mount the ESP to read.

use crate::slot::Slot;
use crate::state::SlotStatus;

pub const STATUS_PATH: &str = "/run/embra/boot-status";

/// The file's fields; the grubenv ones are `None` until embrad read the block.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BootStatus {
    /// The slot booted (`None` on a boot without `embra.slot=`).
    pub slot: Option<Slot>,
    pub trial: bool,
    pub mode: String,
    pub confirmed: String,
    pub version: Option<String>,
    pub build_id: Option<String>,
    pub engine: Option<String>,
    pub active: Option<Slot>,
    pub pending: Option<Slot>,
    pub tries_left: Option<u8>,
    pub slot_a_status: Option<SlotStatus>,
    pub slot_b_status: Option<SlotStatus>,
}

impl BootStatus {
    pub fn parse(text: &str) -> BootStatus {
        let mut s = BootStatus::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "slot" => s.slot = Slot::parse(v),
                "trial" => s.trial = v == "1",
                "mode" => s.mode = v.to_string(),
                "confirmed" => s.confirmed = v.to_string(),
                "version" => s.version = Some(v.to_string()),
                "build_id" => s.build_id = Some(v.to_string()),
                "engine" => s.engine = Some(v.to_string()),
                "active" => s.active = Slot::parse(v),
                "try" => s.pending = Slot::parse(v),
                "tries_left" => s.tries_left = v.parse().ok(),
                "slot_a_status" => s.slot_a_status = Some(SlotStatus::parse(v)),
                "slot_b_status" => s.slot_b_status = Some(SlotStatus::parse(v)),
                _ => {}
            }
        }
        s
    }

    /// `None` when the file is absent (a development host, or embrad
    /// before `prepare`).
    pub fn read() -> Option<BootStatus> {
        std::fs::read_to_string(STATUS_PATH).ok().map(|t| BootStatus::parse(&t))
    }

    pub fn status_of(&self, slot: Slot) -> Option<SlotStatus> {
        match slot {
            Slot::A => self.slot_a_status,
            Slot::B => self.slot_b_status,
        }
    }

    /// One line for a status surface: `a` / `a (trial)`, the active slot
    /// and a pending one with its tries.
    pub fn summary(&self) -> String {
        let booted = self.slot.map(|s| s.to_string()).unwrap_or_else(|| "-".into());
        let mut out = format!("booted {booted}");
        if self.trial {
            out.push_str(" (trial)");
        }
        if let Some(a) = self.active {
            out.push_str(&format!(", active {a}"));
        }
        if let Some(p) = self.pending {
            out.push_str(&format!(", {p} pending"));
            if let Some(t) = self.tries_left {
                out.push_str(&format!(" ({t} tries left)"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_file_reads_back_into_its_fields() {
        let text = "slot=b\ntrial=1\nmode=normal\nconfirmed=yes\nversion=0.24.0-phase2\nbuild_id=abc\narch=x86_64\nengine=fjall\nbuilt=now\nactive=b\ntry=-\ntries_left=0\nslot_a_status=\nslot_b_status=ok\n";
        let s = BootStatus::parse(text);
        assert_eq!(s.slot, Some(Slot::B));
        assert!(s.trial);
        assert_eq!(s.confirmed, "yes");
        assert_eq!(s.version.as_deref(), Some("0.24.0-phase2"));
        assert_eq!(s.engine.as_deref(), Some("fjall"));
        assert_eq!(s.active, Some(Slot::B));
        assert_eq!(s.pending, None);
        assert_eq!(s.tries_left, Some(0));
        assert_eq!(s.status_of(Slot::B), Some(SlotStatus::Ok));
        assert_eq!(s.status_of(Slot::A), Some(SlotStatus::Unknown));
        assert_eq!(s.summary(), "booted b (trial), active b");
        let s = BootStatus::parse("slot=a\ntrial=0\nactive=a\ntry=b\ntries_left=2\nslot_b_status=pending\n");
        assert_eq!(s.summary(), "booted a, active a, b pending (2 tries left)");
        assert_eq!(BootStatus::parse("").summary(), "booted -");
    }
}
