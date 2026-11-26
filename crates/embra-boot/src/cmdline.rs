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
/// The display console's video mode: `<width>x<height>`, or `native` for the
/// display's own. GRUB reads the same knob for the firmware's framebuffer
/// (`buildroot/board/embraos/grub.cfg.in`, `boot_slot`).
pub const KIOSK_MODE_KEY: &str = "embra.kioskmode";

/// What `embra.kioskmode=` asks of the display console's video mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KioskMode {
    /// The display's own: the firmware's mode, a driver's preferred one.
    Native,
    /// `<width>x<height>`, in pixels.
    Size(u32, u32),
}

impl KioskMode {
    /// `native`, or `<width>x<height>` in decimal digits alone and neither
    /// zero: the two forms GRUB's pattern takes, so both read a line alike.
    pub fn parse(value: &str) -> Option<Self> {
        if value == "native" {
            return Some(Self::Native);
        }
        let (w, h) = value.split_once('x')?;
        let digits = |s: &str| !s.is_empty() && s.len() <= 5 && s.chars().all(|c| c.is_ascii_digit());
        if !digits(w) || !digits(h) {
            return None;
        }
        match (w.parse::<u32>().ok()?, h.parse::<u32>().ok()?) {
            (0, _) | (_, 0) => None,
            (w, h) => Some(Self::Size(w, h)),
        }
    }
}
/// The installation medium, instead of the scan (`embra.media=/dev/sr0`).
pub const MEDIA_KEY: &str = "embra.media";
/// The time server embrad asks at boot (`embra.ntp=<host>[:<port>]`), or
/// `embra.ntp=off` for none.
pub const TIME_SERVER_KEY: &str = "embra.ntp";

/// What `embra.ntp=` asks of the time fetch at boot (`embrad/src/clock.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeServer<'a> {
    /// No knob on the line: embrad's own default server.
    Default,
    /// `embra.ntp=off`: no time server is asked.
    Off,
    /// `embra.ntp=<host>` or `<host>:<port>`: a name, an IPv4 address, or an
    /// IPv6 address (in brackets when a port follows).
    Named { host: &'a str, port: u16 },
}

impl<'a> TimeServer<'a> {
    /// NTP's port, used when the knob names none.
    pub const NTP_PORT: u16 = 123;

    /// `off`, or a host with an optional port. A host is letters, digits,
    /// dots and dashes, or an IPv6 address; anything else is no value, so a
    /// garbled knob changes nothing.
    pub fn parse(value: &'a str) -> Option<Self> {
        if value == "off" {
            return Some(TimeServer::Off);
        }
        let (host, port) = if let Some(rest) = value.strip_prefix('[') {
            let (host, tail) = rest.split_once(']')?;
            if !host.contains(':') {
                return None;
            }
            match tail {
                "" => (host, None),
                tail => (host, Some(tail.strip_prefix(':')?)),
            }
        } else if value.matches(':').count() == 1 {
            let (host, port) = value.split_once(':')?;
            (host, Some(port))
        } else {
            (value, None)
        };
        let port = match port {
            None => Self::NTP_PORT,
            Some(p) if !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()) => p.parse::<u16>().ok().filter(|p| *p != 0)?,
            Some(_) => return None,
        };
        let named = if host.contains(':') {
            host.chars().all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
        } else {
            host.len() <= 253
                && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
                && host.starts_with(|c: char| c.is_ascii_alphanumeric())
        };
        (named && !host.is_empty()).then_some(TimeServer::Named { host, port })
    }
}
/// The installer's target disk, unattended (`embra.install=vda`).
pub const INSTALL_KEY: &str = "embra.install";
/// The installer's target disk for an upgrade in place, unattended
/// (`embra.upgrade=vda`): the installation the disk holds keeps STATE and
/// DATA and takes the medium's OS into the slot that is not the default.
pub const UPGRADE_KEY: &str = "embra.upgrade";
/// `embra.downgrade=1`: an upgrade in place may write a release older than
/// the one installed.
pub const DOWNGRADE_KEY: &str = "embra.downgrade";
/// `embra.license=accept`: the operator who wrote the kernel line accepts
/// the licence the medium carries. An unattended install or upgrade
/// (`embra.install=<disk>`, `embra.upgrade=<disk>`) needs it, since it
/// asks nothing.
pub const LICENSE_KEY: &str = "embra.license";

/// What embrad runs: the services, or one maintenance job from the boot
/// menu (`embra.mode=update|backup|restore`), or the installer from the
/// installation medium (`embra.mode=install`: embra-init boots the rootfs
/// the medium carries), then a reboot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BootMode {
    #[default]
    Normal,
    Update,
    Backup,
    Restore,
    Install,
}

impl BootMode {
    pub fn parse(s: &str) -> Option<BootMode> {
        match s {
            "normal" => Some(BootMode::Normal),
            "update" => Some(BootMode::Update),
            "backup" => Some(BootMode::Backup),
            "restore" => Some(BootMode::Restore),
            "install" => Some(BootMode::Install),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            BootMode::Normal => "normal",
            BootMode::Update => "update",
            BootMode::Backup => "backup",
            BootMode::Restore => "restore",
            BootMode::Install => "install",
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

    /// `embra.wifi=0`: no wireless driver is loaded in this boot, the way
    /// out when one misbehaves on some card. The last `1` or `0` wins, any
    /// other value is ignored, and without one the drivers are loaded.
    pub fn wifi(&self) -> bool {
        let mut wifi = true;
        for t in &self.tokens {
            match t.as_str() {
                "embra.wifi=1" => wifi = true,
                "embra.wifi=0" => wifi = false,
                _ => {}
            }
        }
        wifi
    }

    /// `embra.native=0`: no model server is started in this boot (native
    /// models, experimental), the way out when a native model takes the
    /// machine's memory or stops on its processor. The last `1` or `0` wins,
    /// any other value is ignored, and without one the settings on STATE
    /// decide.
    pub fn native(&self) -> bool {
        let mut native = true;
        for t in &self.tokens {
            match t.as_str() {
                "embra.native=1" => native = true,
                "embra.native=0" => native = false,
                _ => {}
            }
        }
        native
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

    /// `embra.kioskmode=<width>x<height>|native`, the display console's
    /// video mode; the last value that is one of the two forms. Without one
    /// the display console asks for its default where the display offers it.
    pub fn kiosk_mode(&self) -> Option<KioskMode> {
        self.values(KIOSK_MODE_KEY).filter_map(KioskMode::parse).last()
    }
}

impl Cmdline {
    /// `embra.media=<device>`, the installation medium, instead of the scan
    /// of every block device; the last non-empty value.
    pub fn media(&self) -> Option<&str> {
        self.values(MEDIA_KEY).filter(|v| !v.is_empty()).last()
    }

    /// `embra.ntp=<host>[:<port>]|off`, the time server embrad asks at boot;
    /// the last value that is one of the forms, embrad's default without one.
    pub fn time_server(&self) -> TimeServer<'_> {
        self.values(TIME_SERVER_KEY).filter_map(TimeServer::parse).last().unwrap_or(TimeServer::Default)
    }

    /// `embra.install=<disk>`, the installer's target, unattended: a bare
    /// device name (`vda`, `sda`, `nvme0n1`); anything else is ignored.
    pub fn install_disk(&self) -> Option<&str> {
        self.values(INSTALL_KEY).filter(|v| bare_disk_name(v)).last()
    }

    /// `embra.upgrade=<disk>`, the installer's target for an upgrade in
    /// place, unattended: a bare device name, as `embra.install` takes one;
    /// anything else is ignored.
    pub fn upgrade_disk(&self) -> Option<&str> {
        self.values(UPGRADE_KEY).filter(|v| bare_disk_name(v)).last()
    }

    /// `embra.downgrade=1`: the installer's upgrade in place may write a
    /// release older than the installed one.
    pub fn allow_downgrade(&self) -> bool {
        self.last(DOWNGRADE_KEY) == Some("1")
    }

    /// `embra.license=accept`: the licence the medium carries is accepted
    /// on the kernel line. The word `accept` and nothing else, as its last
    /// occurrence says.
    pub fn license_accepted(&self) -> bool {
        self.last(LICENSE_KEY) == Some("accept")
    }
}

/// A bare device name (`vda`, `sda`, `nvme0n1`): lowercase letters and digits.
fn bare_disk_name(v: &str) -> bool {
    !v.is_empty() && v.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
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
    fn embra_wifi_0_loads_no_wireless_driver_and_the_last_word_wins() {
        assert!(Cmdline::parse("console=ttyS0 quiet").wifi(), "without the knob the drivers are loaded");
        assert!(!Cmdline::parse("embra.wifi=0").wifi());
        assert!(Cmdline::parse("embra.wifi=0 embra.wifi=1").wifi());
        assert!(!Cmdline::parse("embra.wifi=1 quiet embra.wifi=0").wifi());
        assert!(Cmdline::parse("embra.wifi=no embra.wifi=").wifi(), "any other value is ignored");
    }

    #[test]
    fn embra_native_0_starts_no_model_server_and_the_last_word_wins() {
        assert!(Cmdline::parse("console=ttyS0 quiet").native(), "without the knob the settings decide");
        assert!(!Cmdline::parse("embra.native=0").native());
        assert!(Cmdline::parse("embra.native=0 embra.native=1").native());
        assert!(!Cmdline::parse("embra.native=1 quiet embra.native=0").native());
        assert!(Cmdline::parse("embra.native=off embra.native=").native(), "any other value is ignored");
        // The boot menu's list of knobs names it.
        let menu = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../buildroot/board/embraos/grub.cfg.in")).unwrap();
        assert!(menu.contains("echo \"  embra.native=0 "), "the boot menu's list of knobs");
    }

    /// Both boot menus read the machine's SMBIOS OEM string for the
    /// launcher's knobs, and a machine's firmware has such a string of its
    /// own ("Default string", a board's name, an address). Neither menu
    /// puts the string on a kernel line: each takes the words of it that
    /// are `embra.*` knobs into `$extra`, and every kernel line ends in
    /// `$extra`. A word the kernel does not know is handed to the first
    /// process, and the kernel stops the boot at the 33rd.
    #[test]
    fn the_boot_menus_take_only_the_embra_knobs_of_the_firmwares_oem_string() {
        for name in ["grub.cfg.in", "grub-installer.cfg.in"] {
            let menu = std::fs::read_to_string(format!("{}/../../buildroot/board/embraos/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap();
            let code: Vec<&str> = menu.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("@@")).collect();
            // The table is read once, into a variable of its own.
            let reads: Vec<&str> = code.iter().copied().filter(|l| l.starts_with("smbios ")).collect();
            assert_eq!(reads, ["smbios --type 11 --get-string 4 --set=oem_string"], "{name}");
            // Its words that are knobs go into $extra, one at a time.
            let at = code
                .iter()
                .position(|l| *l == "for word in $oem_string; do")
                .unwrap_or_else(|| panic!("{name}: no loop over the string's words"));
            assert_eq!(
                code[at + 1..at + 5],
                [
                    r#"if regexp '^embra\.[a-z]' "$word"; then"#,
                    r#"if [ -z "$extra" ]; then set extra="$word"; else set extra="$extra $word"; fi"#,
                    "fi",
                    "done",
                ],
                "{name}"
            );
            let mut kernel_lines = 0;
            for (i, line) in code.iter().enumerate() {
                // Nothing but the loop writes $extra: it starts empty.
                if line.starts_with("set extra=") {
                    assert!(matches!(*line, "set extra=" | "set extra=\"\""), "{name}: {line}");
                } else if line.contains("set extra=") {
                    assert_eq!(i, at + 2, "{name}: {line}");
                }
                // The string itself reaches the loop, and nothing else.
                if line.contains("oem_string") && !line.starts_with("set oem_string=") {
                    assert!(line.starts_with("smbios ") || *line == "for word in $oem_string; do", "{name}: {line}");
                }
                if line.starts_with("linux ") {
                    kernel_lines += 1;
                    assert!(line.ends_with(" $extra"), "{name}: a kernel line that does not end in the knobs: {line}");
                }
            }
            assert!(kernel_lines >= 1, "{name}: no kernel line was read");
        }
    }

    /// The words the menus' pattern takes and the words it leaves, on the
    /// strings three machines' firmwares and the launcher have.
    #[test]
    fn the_menus_pattern_takes_a_knob_and_leaves_a_firmwares_word() {
        // `^embra\.[a-z]`, as the menus hand it to GRUB's regexp.
        let taken = |word: &str| word.strip_prefix("embra.").and_then(|rest| rest.chars().next()).is_some_and(|c| c.is_ascii_lowercase());
        let knobs = |string: &str| string.split_whitespace().filter(|w| taken(w)).collect::<Vec<_>>().join(" ");
        assert_eq!(knobs("embra.cols=120 embra.rows=40 embra.ntp=off"), "embra.cols=120 embra.rows=40 embra.ntp=off");
        assert_eq!(knobs("Default string"), "");
        assert_eq!(knobs("Supermicro motherboard-X11 Series"), "");
        assert_eq!(knobs("https://example.org/firmware"), "");
        assert_eq!(knobs("To be filled by O.E.M."), "");
        assert_eq!(knobs("ignore_loglevel embra.tui=1 video=efifb:off"), "embra.tui=1");
        assert_eq!(knobs("embra. embra.=1 EMBRA.TUI=1 xembra.tui=1 embra.Tui=1"), "");
        // Every knob the kernel line's reader knows is a word the pattern takes.
        let source = include_str!("cmdline.rs");
        let mut seen = 0;
        for word in source.split(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '_')).filter(|w| w.starts_with("embra.") && w.len() > 6) {
            if word.ends_with('.') || word == "embra.Tui" {
                continue;
            }
            seen += 1;
            assert!(taken(word), "{word} is named in this file as a knob and the menus would drop it");
        }
        assert!(seen > 20, "the knobs of this file were not read: {seen}");
    }

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

    #[test]
    fn the_video_mode_knob_takes_a_size_or_native_and_the_last_well_formed_one_wins() {
        let mode = |line: &str| Cmdline::parse(line).kiosk_mode();
        assert_eq!(mode("quiet"), None);
        assert_eq!(mode("embra.kioskmode=1280x800"), Some(KioskMode::Size(1280, 800)));
        assert_eq!(mode("embra.kioskmode=native"), Some(KioskMode::Native));
        assert_eq!(mode("embra.kioskmode=1280x800 embra.kioskmode=native"), Some(KioskMode::Native), "the last one wins");
        assert_eq!(mode("embra.kioskmode=native embra.kioskmode=1600x900"), Some(KioskMode::Size(1600, 900)));
        // a value that is neither form is skipped, as GRUB's pattern skips it
        assert_eq!(mode("embra.kioskmode=1600x900 embra.kioskmode=big"), Some(KioskMode::Size(1600, 900)));
        for bad in ["", "1920", "1920x", "x1080", "0x1080", "1920x0", "+1920x1080", "1920x1080-32", "1920X1080", "Native", "123456x1"] {
            assert_eq!(KioskMode::parse(bad), None, "{bad:?}");
        }
    }
}

#[cfg(test)]
mod install_tests {
    use super::*;

    #[test]
    fn install_mode_is_a_boot_mode_and_rescue_is_still_unknown() {
        assert_eq!(Cmdline::parse("embra.mode=install").mode(), Ok(BootMode::Install));
        assert_eq!(BootMode::Install.name(), "install");
        assert_eq!(BootMode::parse("install"), Some(BootMode::Install));
        assert!(matches!(Cmdline::parse("embra.mode=rescue").mode(), Err(CmdlineError::UnknownMode(_))));
    }

    #[test]
    fn the_medium_and_the_unattended_disk_come_from_their_knobs() {
        let c = Cmdline::parse("embra.mode=install embra.media=/dev/sr0 embra.install=sda embra.install=vda");
        assert_eq!(c.media(), Some("/dev/sr0"));
        assert_eq!(c.install_disk(), Some("vda"), "the last wins");
        assert_eq!(Cmdline::parse("embra.install=/dev/sda").install_disk(), None, "a bare name only");
        assert_eq!(Cmdline::parse("embra.install=sd;rm").install_disk(), None);
        assert_eq!(Cmdline::parse("quiet").media(), None);
    }

    #[test]
    fn the_upgrade_knob_names_a_disk_as_the_install_knob_does_and_the_downgrade_knob_is_a_switch() {
        let c = Cmdline::parse("embra.mode=install embra.upgrade=sda embra.upgrade=nvme0n1 embra.downgrade=1");
        assert_eq!(c.upgrade_disk(), Some("nvme0n1"), "the last wins");
        assert_eq!(c.install_disk(), None, "an upgrade names no clean install");
        assert!(c.allow_downgrade());
        assert_eq!(Cmdline::parse("embra.upgrade=/dev/sda").upgrade_disk(), None, "a bare name only");
        assert_eq!(Cmdline::parse("embra.upgrade=").upgrade_disk(), None);
        assert!(!Cmdline::parse("embra.upgrade=vda embra.downgrade=yes").allow_downgrade(), "1 and nothing else");
        assert!(!Cmdline::parse("embra.upgrade=vda").allow_downgrade());
        assert_eq!(Cmdline::parse("embra.install=vda").upgrade_disk(), None);
        // The licence's acceptance beside them: the word accept and nothing else, the last occurrence.
        assert!(Cmdline::parse("embra.mode=install embra.install=vda embra.license=accept").license_accepted());
        for line in ["embra.install=vda", "embra.license=1", "embra.license=yes", "embra.license=ACCEPT", "embra.license=", "embra.license=accept embra.license=no"] {
            assert!(!Cmdline::parse(line).license_accepted(), "{line}");
        }
        assert!(Cmdline::parse("embra.license=no embra.license=accept").license_accepted(), "the last occurrence");
    }
}

#[cfg(test)]
mod time_server_tests {
    use super::*;

    #[test]
    fn the_time_server_knob_names_a_host_a_port_or_off_and_the_last_well_formed_one_wins() {
        let check = |line: &str, want: TimeServer<'_>| assert_eq!(Cmdline::parse(line).time_server(), want, "{line}");
        check("console=ttyS0 quiet", TimeServer::Default);
        check("embra.ntp=off", TimeServer::Off);
        check("embra.ntp=ntp.example.org", TimeServer::Named { host: "ntp.example.org", port: 123 });
        check("embra.ntp=10.0.2.2:12300", TimeServer::Named { host: "10.0.2.2", port: 12300 });
        check("embra.ntp=2606:4700:f1::1", TimeServer::Named { host: "2606:4700:f1::1", port: 123 });
        check("embra.ntp=[2606:4700:f1::1]:1123", TimeServer::Named { host: "2606:4700:f1::1", port: 1123 });
        check("embra.ntp=[::1]", TimeServer::Named { host: "::1", port: 123 });
        // the last one wins
        check("embra.ntp=off embra.ntp=time.example.org", TimeServer::Named { host: "time.example.org", port: 123 });
        check("embra.ntp=time.example.org embra.ntp=off", TimeServer::Off);
        // a value that is none of the forms is skipped: the one before it stands
        check("embra.ntp=off embra.ntp=bad_host", TimeServer::Off);
        for bad in ["", "a b", "host:", "host:0", "host:99999", "host:12x", ":123", "-host", ".host", "ho$t", "[host]:123", "[::1]123", "[::1", "host/path", "http://host"] {
            assert_eq!(TimeServer::parse(bad), None, "{bad:?}");
        }
        assert_eq!(TimeServer::NTP_PORT, 123);
    }
}
