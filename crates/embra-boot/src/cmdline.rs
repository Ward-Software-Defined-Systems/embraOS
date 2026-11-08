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
/// The display console: the web console on the machine's display.
pub const KIOSK_KEY: &str = "embra.kiosk";
/// The display console's keyboard layout (an XKB layout name).
pub const KEYBOARD_KEY: &str = "embra.kbd";
/// The display console's device scale, 1 or 2.
pub const KIOSK_SCALE_KEY: &str = "embra.kioskscale";

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

    /// `embra.kiosk=1|0`, the display console: the last `1` or `0` wins,
    /// any other value is ignored, and without one `display_console`
    /// decides from the machine.
    pub fn kiosk(&self) -> Option<bool> {
        let mut kiosk = None;
        for t in &self.tokens {
            match t.as_str() {
                "embra.kiosk=1" => kiosk = Some(true),
                "embra.kiosk=0" => kiosk = Some(false),
                _ => {}
            }
        }
        kiosk
    }

    /// `embra.kbd=<layout>`, the display console's keyboard layout; the
    /// last non-empty value.
    pub fn keyboard_layout(&self) -> Option<&str> {
        self.values(KEYBOARD_KEY).filter(|v| !v.is_empty()).last()
    }

    /// `embra.kioskscale=1|2`, the display console's device scale; the
    /// last value that is one of the two.
    pub fn kiosk_scale(&self) -> Option<u8> {
        self.values(KIOSK_SCALE_KEY)
            .filter_map(|v| match v {
                "1" => Some(1),
                "2" => Some(2),
                _ => None,
            })
            .last()
    }
}

/// Whether the display console runs: under the web console, on a machine
/// with a display (`/dev/dri/card*`), unless `embra.kiosk=0`. `embra.kiosk=1`
/// conjures no display, and the serial TUI (`embra.tui=1`) never has it.
pub fn display_console(web: bool, kiosk: Option<bool>, display_present: bool) -> bool {
    web && display_present && kiosk.unwrap_or(true)
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

#[cfg(test)]
mod display_console_tests {
    use super::*;

    #[test]
    fn the_display_console_follows_embra_kiosk_and_is_on_by_default_where_a_display_exists() {
        assert!(!display_console(false, None, true), "the serial TUI never has it");
        assert!(!display_console(true, Some(true), false), "no display, no console, whatever the knob says");
        assert!(display_console(true, None, true), "on by default under the web console with a display");
        assert!(!display_console(true, Some(false), true), "embra.kiosk=0 turns it off");
        assert_eq!(Cmdline::parse("embra.web=1 embra.kiosk=0 embra.kiosk=1").kiosk(), Some(true), "the last word wins");
        assert_eq!(Cmdline::parse("embra.kiosk=maybe").kiosk(), None, "an unknown value changes nothing");
        assert_eq!(Cmdline::parse("quiet").kiosk(), None);
    }

    #[test]
    fn the_keyboard_layout_and_scale_knobs_take_the_last_usable_value() {
        let c = Cmdline::parse("embra.kbd=de embra.kbd= embra.kioskscale=3 embra.kioskscale=2");
        assert_eq!(c.keyboard_layout(), Some("de"));
        assert_eq!(c.kiosk_scale(), Some(2));
        assert_eq!(Cmdline::parse("quiet").keyboard_layout(), None);
        assert_eq!(Cmdline::parse("embra.kioskscale=4").kiosk_scale(), None);
    }
}
