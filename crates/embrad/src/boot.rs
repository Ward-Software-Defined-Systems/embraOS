//! The A/B boot as embrad sees it: the status file every reader takes the
//! state from (`/run/embra/boot-status`: the brain's `system_status`,
//! embra-update), and the one write embrad makes to the EFI System
//! Partition — confirming a trial boot once every service is healthy.
//!
//! GRUB boots a staged slot with `embra.trial=1` on the kernel line while
//! the slot has tries left (`grub.cfg.in`). A boot that halts before the
//! confirmation consumes a try, which is the point; a boot that reaches it
//! makes the slot the default from then on. A normal boot reads the block
//! for the status and writes nothing.

use embra_boot::{BootMode, BootState, Cmdline, Release, esp, partitions, slot};
use std::fmt;
use tracing::{error, info, warn};

pub const STATUS_PATH: &str = "/run/embra/boot-status";

/// Where the confirmation stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Confirmed {
    /// The services are not up yet.
    Pending,
    /// A trial boot, confirmed: the slot is the default now.
    Yes,
    /// Not a trial boot (or a boot without a slot): nothing to confirm.
    NotNeeded,
    /// A trial boot whose confirmation could not be written; GRUB retries
    /// while tries last, then falls back.
    Failed(String),
}

impl fmt::Display for Confirmed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Confirmed::Pending => write!(f, "pending"),
            Confirmed::Yes => write!(f, "yes"),
            Confirmed::NotNeeded => write!(f, "not-needed"),
            Confirmed::Failed(reason) => write!(f, "failed: {reason}"),
        }
    }
}

/// The status file's text: `key=value` lines. The slot is `-` on a boot
/// without one; the grubenv fields appear once the block was read.
pub fn status_text(
    cmdline: &Cmdline,
    mode: BootMode,
    confirmed: &Confirmed,
    state: Option<&BootState>,
    release: Option<&Release>,
) -> String {
    let mut out = String::new();
    let slot = cmdline.slot().ok().flatten().map(|s| s.to_string()).unwrap_or_else(|| "-".into());
    out.push_str(&format!("slot={slot}\n"));
    out.push_str(&format!("trial={}\n", u8::from(cmdline.trial())));
    out.push_str(&format!("mode={}\n", mode.name()));
    out.push_str(&format!("confirmed={confirmed}\n"));
    if let Some(r) = release {
        out.push_str(&format!("version={}\nbuild_id={}\narch={}\nengine={}\nbuilt={}\n", r.version, r.build_id, r.arch, r.engine, r.built));
    }
    if let Some(st) = state {
        out.push_str(&format!("active={}\n", st.slot_active));
        out.push_str(&format!("try={}\n", st.slot_try.map(|s| s.to_string()).unwrap_or_else(|| "-".into())));
        out.push_str(&format!("tries_left={}\n", st.tries_left));
        out.push_str(&format!("slot_a_status={}\n", st.slot_a_status.as_str()));
        out.push_str(&format!("slot_b_status={}\n", st.slot_b_status.as_str()));
    }
    out
}

/// Write the status file; a failure is logged, never fatal.
pub fn write_status(cmdline: &Cmdline, mode: BootMode, confirmed: &Confirmed, state: Option<&BootState>, release: Option<&Release>) {
    let text = status_text(cmdline, mode, confirmed, state, release);
    let dir = std::path::Path::new(STATUS_PATH).parent().expect("a path with a parent");
    if let Err(e) = std::fs::create_dir_all(dir).and_then(|()| std::fs::write(STATUS_PATH, text)) {
        warn!("boot status could not be written to {STATUS_PATH}: {e}");
    }
}

/// After every service is healthy. On a trial boot the block on the ESP is
/// rewritten with the booted slot active and the trial cleared; on any
/// other boot with a slot the block is read for the status; a boot without
/// a slot has nothing to read. The outcome goes to the status file.
pub fn after_services_healthy(cmdline: &Cmdline, mode: BootMode) -> Confirmed {
    let release = Release::read().ok();
    let Some(booted) = cmdline.slot().ok().flatten() else {
        let c = Confirmed::NotNeeded;
        write_status(cmdline, mode, &c, None, release.as_ref());
        return c;
    };
    let Some(dev) = partitions::find_partition(slot::ESP_PARTITION) else {
        let c = if cmdline.trial() {
            error!("slot {booted} cannot be confirmed: no partition named {}", slot::ESP_PARTITION);
            Confirmed::Failed(format!("no partition named {}", slot::ESP_PARTITION))
        } else {
            warn!("boot status without the ESP: no partition named {}", slot::ESP_PARTITION);
            Confirmed::NotNeeded
        };
        write_status(cmdline, mode, &c, None, release.as_ref());
        return c;
    };
    if cmdline.trial() {
        let written = esp::with_env_mut(&dev, |env| {
            let mut st = BootState::from_env(env);
            st.confirm(booted);
            st.apply(env).map(|()| st)
        });
        let (c, state) = match written {
            Ok(Ok(st)) => {
                info!("slot {booted} confirmed: the default from now on (grubenv on {})", dev.display());
                (Confirmed::Yes, Some(st))
            }
            Ok(Err(e)) => (Confirmed::Failed(e.to_string()), None),
            Err(e) => (Confirmed::Failed(e.to_string()), None),
        };
        if let Confirmed::Failed(reason) = &c {
            error!("slot {booted} could not be confirmed: {reason}; GRUB retries while tries last, then falls back to the previous slot");
        }
        write_status(cmdline, mode, &c, state.as_ref(), release.as_ref());
        c
    } else {
        let state = match esp::with_env(&dev, BootState::from_env) {
            Ok(st) => Some(st),
            Err(e) => {
                warn!("grubenv on {} could not be read for the boot status: {e}", dev.display());
                None
            }
        };
        let c = Confirmed::NotNeeded;
        write_status(cmdline, mode, &c, state.as_ref(), release.as_ref());
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embra_boot::{Slot, SlotStatus};

    #[test]
    fn the_boot_status_file_names_the_slot_the_trial_and_the_mode() {
        let cmdline = Cmdline::parse("console=ttyS0 embra.slot=b embra.trial=1 embra.web=1");
        let mut state = BootState::default();
        state.stage(Slot::B);
        state.confirm(Slot::B);
        let release = Release::parse("VERSION=0.24.0-phase2\nBUILD_ID=abc\nARCH=x86_64\nENGINE=fjall\nBUILT=2026-11-05T00:00:00Z\n").unwrap();
        let text = status_text(&cmdline, BootMode::Normal, &Confirmed::Yes, Some(&state), Some(&release));
        let want = "slot=b\ntrial=1\nmode=normal\nconfirmed=yes\nversion=0.24.0-phase2\nbuild_id=abc\narch=x86_64\nengine=fjall\nbuilt=2026-11-05T00:00:00Z\nactive=b\ntry=-\ntries_left=0\nslot_a_status=\nslot_b_status=ok\n";
        assert_eq!(text, want);
        assert_eq!(state.status_of(Slot::B), SlotStatus::Ok);

        let slotless = Cmdline::parse("console=ttyS0 root=/dev/vda2 ro quiet");
        let text = status_text(&slotless, BootMode::Update, &Confirmed::Failed("no esp".into()), None, None);
        assert_eq!(text, "slot=-\ntrial=0\nmode=update\nconfirmed=failed: no esp\n");
        assert_eq!(Confirmed::Pending.to_string(), "pending");
        assert_eq!(Confirmed::NotNeeded.to_string(), "not-needed");
    }

    #[test]
    fn a_boot_without_a_slot_runs_no_slot_logic() {
        // No slot on the line: nothing is confirmed and no device is looked for,
        // on a host without an ESP as much as in the OS.
        let cmdline = Cmdline::parse("console=ttyS0 embra.web=1");
        assert_eq!(after_services_healthy(&cmdline, BootMode::Normal), Confirmed::NotNeeded);
    }
}
