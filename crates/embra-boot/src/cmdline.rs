//! The kernel command line: the slot GRUB booted, a maintenance mode, a
//! trial boot, and the `embra.*` knobs embrad reads. The last occurrence of
//! a knob wins, so an entry of the boot menu and the string run-qemu.sh
//! appends can both override GRUB's defaults.

use crate::slot::Slot;
use std::fmt;

/// Where the kernel publishes its command line.
pub const PATH: &str = "/proc/cmdline";

pub const SLOT_KEY: &str = "embra.slot";
pub const MODE_KEY: &str = "embra.mode";
pub const TRIAL_KEY: &str = "embra.trial";
pub const WEB_KEY: &str = "embra.web";
pub const TUI_KEY: &str = "embra.tui";
pub const DB_VERBOSE_KEY: &str = "embra.dbverbose";
pub const LOG_LEVEL_KEY: &str = "embra.loglevel";
pub const GRAPHICS_KEY: &str = "embra.graphics";
pub const COLS_KEY: &str = "embra.cols";
pub const ROWS_KEY: &str = "embra.rows";

/// What embrad runs: the services, or one maintenance job from the boot
/// menu (`embra.mode=update|backup|restore`), then a reboot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BootMode {
    #[default]
    Normal,
    Update,
    Backup,
    Restore,
}

impl BootMode {
    pub fn parse(s: &str) -> Option<BootMode> {
        match s {
            "normal" => Some(BootMode::Normal),
            "update" => Some(BootMode::Update),
            "backup" => Some(BootMode::Backup),
            "restore" => Some(BootMode::Restore),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            BootMode::Normal => "normal",
            BootMode::Update => "update",
            BootMode::Backup => "backup",
            BootMode::Restore => "restore",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CmdlineError {
    /// `embra.slot=` with a value that is neither `a` nor `b`.
    InvalidSlot(String),
    /// `embra.mode=` with a value no boot mode has.
    UnknownMode(String),
}

impl fmt::Display for CmdlineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CmdlineError::InvalidSlot(v) => write!(f, "{SLOT_KEY}={v} on the kernel line is not a slot (a or b)"),
            CmdlineError::UnknownMode(v) => {
                write!(f, "{MODE_KEY}={v} on the kernel line is not a boot mode (update, backup or restore)")
            }
        }
    }
}

impl std::error::Error for CmdlineError {}

/// The command line, split on whitespace as the kernel splits it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cmdline {
    tokens: Vec<String>,
}

impl Cmdline {
    /// `/proc/cmdline`; empty when it cannot be read (a host test, not the OS).
    pub fn read() -> Cmdline {
        Cmdline::parse(&std::fs::read_to_string(PATH).unwrap_or_default())
    }

    pub fn parse(text: &str) -> Cmdline {
        Cmdline { tokens: text.split_whitespace().map(str::to_string).collect() }
    }

    pub fn tokens(&self) -> &[String] {
        &self.tokens
    }

    fn values<'a>(&'a self, key: &str) -> impl Iterator<Item = &'a str> + use<'a> {
        let prefix = format!("{key}=");
        self.tokens.iter().filter_map(move |t| t.strip_prefix(prefix.as_str()))
    }

    /// The last `key=value` of the line, its value.
    pub fn last(&self, key: &str) -> Option<&str> {
        self.values(key).last()
    }

    /// The slot GRUB booted. `None` on a line without `embra.slot=` (the
    /// direct-kernel path, a Phase 1 disk); an error on a value that is not
    /// a slot, because the kernel came from one slot and the rootfs would
    /// come from another.
    pub fn slot(&self) -> Result<Option<Slot>, CmdlineError> {
        match self.last(SLOT_KEY) {
            None => Ok(None),
            Some(v) => Slot::parse(v).map(Some).ok_or_else(|| CmdlineError::InvalidSlot(v.to_string())),
        }
    }

    /// The boot mode; `Normal` without `embra.mode=`.
    pub fn mode(&self) -> Result<BootMode, CmdlineError> {
        match self.last(MODE_KEY) {
            None => Ok(BootMode::Normal),
            Some(v) => BootMode::parse(v).ok_or_else(|| CmdlineError::UnknownMode(v.to_string())),
        }
    }

    /// GRUB booted a staged slot (`embra.trial=1`): embrad confirms it once
    /// every service is healthy.
    pub fn trial(&self) -> bool {
        self.last(TRIAL_KEY) == Some("1")
    }

    /// The web console or the serial TUI: the last word of `embra.web` and
    /// `embra.tui` on the line (`embra.web=1`, `embra.tui=0` say web;
    /// `embra.web=0`, `embra.tui=1` say TUI); the serial TUI without either.
    pub fn web_console(&self) -> bool {
        let mut web = false;
        for t in &self.tokens {
            match t.as_str() {
                "embra.web=1" | "embra.tui=0" => web = true,
                "embra.web=0" | "embra.tui=1" => web = false,
                _ => {}
            }
        }
        web
    }

    /// `embra.dbverbose=1`: WardSONDB's per-request log lines.
    pub fn db_verbose(&self) -> bool {
        self.last(DB_VERBOSE_KEY) == Some("1")
    }

    /// `embra.loglevel=<spec>`, the brain's log filter; the last non-empty value.
    pub fn log_level(&self) -> Option<&str> {
        self.values(LOG_LEVEL_KEY).filter(|v| !v.is_empty()).last()
    }

    /// `embra.graphics=<mode>`, the serial console's image protocol; the last non-empty value.
    pub fn graphics(&self) -> Option<&str> {
        self.values(GRAPHICS_KEY).filter(|v| !v.is_empty()).last()
    }

    /// `embra.cols=` and `embra.rows=` for the serial TUI, 80×24 without them.
    pub fn terminal_size(&self) -> (u16, u16) {
        let cols = self.last(COLS_KEY).and_then(|v| v.parse().ok()).unwrap_or(80);
        let rows = self.last(ROWS_KEY).and_then(|v| v.parse().ok()).unwrap_or(24);
        (cols, rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embra_slot_and_embra_mode_take_the_last_occurrence_and_refuse_an_unknown_value() {
        let c = Cmdline::parse("console=ttyS0 embra.slot=a quiet embra.slot=b embra.mode=update");
        assert_eq!(c.slot(), Ok(Some(Slot::B)));
        assert_eq!(c.mode(), Ok(BootMode::Update));
        assert!(!c.trial());

        let c = Cmdline::parse("console=ttyS0 root=/dev/vda2 ro quiet");
        assert_eq!(c.slot(), Ok(None));
        assert_eq!(c.mode(), Ok(BootMode::Normal));

        let c = Cmdline::parse("embra.slot=c");
        assert_eq!(c.slot(), Err(CmdlineError::InvalidSlot("c".into())));
        let c = Cmdline::parse("embra.mode=rescue");
        assert_eq!(c.mode(), Err(CmdlineError::UnknownMode("rescue".into())));
        assert!(c.mode().unwrap_err().to_string().contains("rescue"));

        let c = Cmdline::parse("embra.slot=b embra.trial=1");
        assert!(c.trial());
        assert_eq!(c.slot(), Ok(Some(Slot::B)));
        // A prefix is not a key: `embra.slots=` says nothing about the slot.
        let c = Cmdline::parse("embra.slots=zz");
        assert_eq!(c.slot(), Ok(None));
    }

    #[test]
    fn web_mode_is_the_last_word_of_embra_web_and_embra_tui() {
        assert!(!Cmdline::parse("console=ttyS0 quiet").web_console());
        assert!(Cmdline::parse("embra.web=1").web_console());
        assert!(!Cmdline::parse("embra.web=1 embra.web=0").web_console());
        assert!(!Cmdline::parse("embra.web=1 embra.tui=1").web_console());
        assert!(Cmdline::parse("embra.tui=1 embra.web=1").web_console());
        assert!(Cmdline::parse("embra.web=0 embra.tui=0").web_console());
        // An unknown value changes nothing.
        assert!(Cmdline::parse("embra.web=1 embra.web=yes").web_console());
    }

    #[test]
    fn the_knobs_keep_their_phase_1_meaning_and_the_last_value_wins() {
        let c = Cmdline::parse("embra.cols=120 embra.rows=40 embra.dbverbose=1 embra.loglevel=info,kg::traversal=debug embra.graphics=sixel");
        assert_eq!(c.terminal_size(), (120, 40));
        assert!(c.db_verbose());
        assert_eq!(c.log_level(), Some("info,kg::traversal=debug"));
        assert_eq!(c.graphics(), Some("sixel"));

        let c = Cmdline::parse("embra.cols=zz embra.dbverbose=0 embra.loglevel= embra.graphics=auto embra.graphics=");
        assert_eq!(c.terminal_size(), (80, 24));
        assert!(!c.db_verbose());
        assert_eq!(c.log_level(), None, "an empty value is no value");
        assert_eq!(c.graphics(), Some("auto"), "an empty value does not clear an earlier one");
        assert_eq!(Cmdline::parse("embra.loglevel=debug embra.loglevel=trace").log_level(), Some("trace"));
        assert!(Cmdline::read().tokens().iter().all(|t| !t.is_empty()));
    }
}
