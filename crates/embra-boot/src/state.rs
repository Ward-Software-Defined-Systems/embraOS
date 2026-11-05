//! The A/B variables of `grubenv`, as GRUB's `grub.cfg` reads them: the
//! slot booted by default, a staged slot and the tries it has left (a
//! 3-2-1-0 ladder; GRUB has no arithmetic), and what each slot's last boot
//! came to.

use crate::grubenv::{GrubEnv, GrubEnvError};
use crate::slot::Slot;

pub const SLOT_ACTIVE: &str = "slot_active";
pub const SLOT_TRY: &str = "slot_try";
pub const TRIES_LEFT: &str = "tries_left";
pub const SLOT_A_STATUS: &str = "slot_a_status";
pub const SLOT_B_STATUS: &str = "slot_b_status";
pub const MENU_TIMEOUT: &str = "menu_timeout";
/// The most tries GRUB counts for a staged slot.
pub const MAX_TRIES: u8 = 3;

/// What `grubenv` records about a slot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SlotStatus {
    /// Nothing recorded (the empty string).
    #[default]
    Unknown,
    /// embrad confirmed a boot of it.
    Ok,
    /// embra-update staged it; GRUB tries it next.
    Pending,
    /// GRUB ran out of tries before embrad confirmed it.
    Failed,
}

impl SlotStatus {
    pub fn parse(s: &str) -> SlotStatus {
        match s {
            "ok" => SlotStatus::Ok,
            "pending" => SlotStatus::Pending,
            "failed" => SlotStatus::Failed,
            _ => SlotStatus::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SlotStatus::Unknown => "",
            SlotStatus::Ok => "ok",
            SlotStatus::Pending => "pending",
            SlotStatus::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootState {
    pub slot_active: Slot,
    pub slot_try: Option<Slot>,
    pub tries_left: u8,
    pub slot_a_status: SlotStatus,
    pub slot_b_status: SlotStatus,
}

impl Default for BootState {
    /// GRUB's own defaults for a missing or garbled block: slot A, no trial.
    fn default() -> Self {
        BootState {
            slot_active: Slot::A,
            slot_try: None,
            tries_left: 0,
            slot_a_status: SlotStatus::Unknown,
            slot_b_status: SlotStatus::Unknown,
        }
    }
}

impl BootState {
    /// The state a block holds; a missing or garbled variable reads as GRUB
    /// reads it, the default.
    pub fn from_env(env: &GrubEnv) -> BootState {
        let d = BootState::default();
        BootState {
            slot_active: env.get(SLOT_ACTIVE).and_then(Slot::parse).unwrap_or(d.slot_active),
            slot_try: env.get(SLOT_TRY).and_then(Slot::parse),
            tries_left: env.get(TRIES_LEFT).and_then(|v| v.parse().ok()).unwrap_or(d.tries_left).min(MAX_TRIES),
            slot_a_status: env.get(SLOT_A_STATUS).map(SlotStatus::parse).unwrap_or_default(),
            slot_b_status: env.get(SLOT_B_STATUS).map(SlotStatus::parse).unwrap_or_default(),
        }
    }

    /// Write the five variables into a block; anything else in it stays.
    pub fn apply(&self, env: &mut GrubEnv) -> Result<(), GrubEnvError> {
        env.set(SLOT_ACTIVE, &self.slot_active.to_string())?;
        env.set(SLOT_TRY, &self.slot_try.map(|s| s.to_string()).unwrap_or_default())?;
        env.set(TRIES_LEFT, &self.tries_left.to_string())?;
        env.set(SLOT_A_STATUS, self.slot_a_status.as_str())?;
        env.set(SLOT_B_STATUS, self.slot_b_status.as_str())
    }

    pub fn status_of(&self, slot: Slot) -> SlotStatus {
        match slot {
            Slot::A => self.slot_a_status,
            Slot::B => self.slot_b_status,
        }
    }

    pub fn set_status(&mut self, slot: Slot, status: SlotStatus) {
        match slot {
            Slot::A => self.slot_a_status = status,
            Slot::B => self.slot_b_status = status,
        }
    }

    /// GRUB booted `booted` as the staged slot.
    pub fn is_trial(&self, booted: Slot) -> bool {
        self.slot_try == Some(booted)
    }

    /// The booted slot came up whole: it is the default from now on, the
    /// trial is over.
    pub fn confirm(&mut self, booted: Slot) {
        self.slot_active = booted;
        self.slot_try = None;
        self.tries_left = 0;
        self.set_status(booted, SlotStatus::Ok);
    }

    /// A slot was written and verified: GRUB tries it at the next boots.
    pub fn stage(&mut self, slot: Slot) {
        self.slot_try = Some(slot);
        self.tries_left = MAX_TRIES;
        self.set_status(slot, SlotStatus::Pending);
    }

    /// The other slot becomes the default again (a rollback by hand).
    pub fn activate(&mut self, slot: Slot) {
        self.slot_active = slot;
        self.slot_try = None;
        self.tries_left = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_booted_slot_is_confirmed_by_making_it_active_and_clearing_the_trial() {
        let mut env = GrubEnv::new();
        env.set(SLOT_ACTIVE, "a").unwrap();
        env.set(SLOT_TRY, "b").unwrap();
        env.set(TRIES_LEFT, "2").unwrap();
        env.set(SLOT_B_STATUS, "pending").unwrap();
        env.set(MENU_TIMEOUT, "5").unwrap();
        let mut state = BootState::from_env(&env);
        assert!(state.is_trial(Slot::B));
        assert!(!state.is_trial(Slot::A));
        assert_eq!(state.tries_left, 2);
        assert_eq!(state.status_of(Slot::B), SlotStatus::Pending);
        state.confirm(Slot::B);
        assert_eq!(state.slot_active, Slot::B);
        assert_eq!(state.slot_try, None);
        assert_eq!(state.tries_left, 0);
        assert_eq!(state.status_of(Slot::B), SlotStatus::Ok);
        state.apply(&mut env).unwrap();
        assert_eq!(env.get(SLOT_ACTIVE), Some("b"));
        assert_eq!(env.get(SLOT_TRY), Some(""));
        assert_eq!(env.get(TRIES_LEFT), Some("0"));
        assert_eq!(env.get(SLOT_B_STATUS), Some("ok"));
        assert_eq!(env.get(SLOT_A_STATUS), Some(""));
        assert_eq!(env.get(MENU_TIMEOUT), Some("5"), "what is not the A/B state stays");
        assert_eq!(BootState::from_env(&env), state);
    }

    #[test]
    fn a_missing_or_garbled_block_reads_as_slot_a_with_no_trial() {
        assert_eq!(BootState::from_env(&GrubEnv::new()), BootState::default());
        let mut env = GrubEnv::new();
        env.set(SLOT_ACTIVE, "z").unwrap();
        env.set(SLOT_TRY, "q").unwrap();
        env.set(TRIES_LEFT, "many").unwrap();
        env.set(SLOT_A_STATUS, "fine").unwrap();
        assert_eq!(BootState::from_env(&env), BootState::default());
        env.set(TRIES_LEFT, "9").unwrap();
        assert_eq!(BootState::from_env(&env).tries_left, MAX_TRIES, "capped at the ladder's top");
    }

    #[test]
    fn staging_marks_a_slot_pending_with_the_full_ladder_and_a_rollback_clears_it() {
        let mut state = BootState::default();
        state.stage(Slot::B);
        assert_eq!(state.slot_try, Some(Slot::B));
        assert_eq!(state.tries_left, MAX_TRIES);
        assert_eq!(state.status_of(Slot::B), SlotStatus::Pending);
        assert_eq!(state.slot_active, Slot::A);
        state.activate(Slot::B);
        assert_eq!(state.slot_active, Slot::B);
        assert_eq!(state.slot_try, None);
        assert_eq!(state.tries_left, 0);
        assert_eq!(SlotStatus::parse("failed"), SlotStatus::Failed);
        assert_eq!(SlotStatus::parse(""), SlotStatus::Unknown);
        assert_eq!(SlotStatus::Failed.as_str(), "failed");
    }
}
