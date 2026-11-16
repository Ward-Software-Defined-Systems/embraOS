//! Wi-Fi, before a card has joined a network: its driver, and a supplicant.
//!
//! Once a Wi-Fi card is on a network it is a card like any other and
//! `net.rs` keeps it: a DHCP client, a record, a route behind every wired
//! card's. What comes before that is here.
//!
//! - **The driver.** The wireless drivers are kernel modules, the only ones
//!   this image loads (`linux.fragment` says why). Every look, the devices
//!   on the buses a Wi-Fi card sits on (PCI, USB, SDIO) are matched against
//!   `/usr/share/embra/wireless.alias`, the list the build made of the
//!   wireless drivers' device patterns, and `modprobe` loads what a device
//!   asks for, once. The card then appears as `wlan0`. `embra.wifi=0` on
//!   the kernel line loads nothing for that boot.
//! - **A USB adapter that shows itself as a driver disk** is switched to
//!   its real mode first, when the mode-switch data names it.
//! - **A supplicant a card.** `wpa_supplicant` runs on every Wi-Fi card
//!   with a configuration rendered from the networks the operator stored
//!   (`embra_boot::wifi`), in a directory no tool can read. When the stored
//!   networks change the configuration is written again and the supplicant
//!   told to read it. The brain asks its control socket what is in range.
//! - **The radio block.** A soft block is lifted once a network is stored;
//!   a switch on the machine is said in the card's status.
//!
//! The machine is behind [`Radio`], so the logic runs in a test without one.
//! Nothing here unwraps or indexes: a panic on any thread of PID 1 halts
//! the machine.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use embra_boot::wifi::{self as stored, Wifi};
use tracing::{info, warn};

/// A supplicant that exited is not started again sooner than this.
const RESTART_PAUSE: Duration = Duration::from_secs(5);

/// The machine as the Wi-Fi keeper needs it. Every method has a default
/// that is a machine without any of it, so a test names what it needs.
pub(crate) trait Radio {
    /// The `modalias` of every device on a bus a Wi-Fi card sits on.
    fn modaliases(&mut self) -> Vec<String> {
        Vec::new()
    }
    /// The build's list of wireless drivers and the devices that ask for
    /// each (`wireless.alias`); empty when the image carries none.
    fn alias_list(&mut self) -> String {
        String::new()
    }
    /// Whether a pattern of that list matches a device (`fnmatch`).
    fn pattern_matches(&self, _pattern: &str, _modalias: &str) -> bool {
        false
    }
    /// `modprobe <module>`, with what it depends on; whether it loaded.
    fn load_module(&mut self, _module: &str) -> bool {
        false
    }
    /// The USB devices: where each is plugged, and its `vvvv:pppp`.
    fn usb_devices(&mut self) -> Vec<(String, String)> {
        Vec::new()
    }
    /// The mode-switch data for a device, when the data names it.
    fn switch_file(&mut self, _ids: &str) -> Option<PathBuf> {
        None
    }
    /// The device switched to the mode that file describes.
    fn switch_mode(&mut self, _ids: &str, _file: &Path) -> bool {
        false
    }
    /// The networks the operator stored, when they changed since this was
    /// last asked (the first time: as they stand).
    fn stored_wifi(&mut self) -> Option<Wifi> {
        None
    }
    /// The supplicant's configuration for a card, where no tool reads it.
    fn write_supplicant_config(&mut self, _card: &str, _text: &str) -> bool {
        false
    }
    /// A supplicant on the card; its pid.
    fn start_supplicant(&mut self, _card: &str) -> Option<u32> {
        None
    }
    /// Whether that supplicant has exited (and collects it when it has).
    fn supplicant_gone(&mut self, _pid: u32) -> bool {
        false
    }
    /// The supplicant reads its configuration again.
    fn reload_supplicant(&mut self, _pid: u32) {}
    /// The supplicant is stopped and collected, its configuration removed.
    fn stop_supplicant(&mut self, _pid: u32, _card: &str) {}
    /// Every soft block on a Wi-Fi radio is lifted; whether a hard block (a
    /// switch on the machine) stands on one.
    fn unblock_radios(&mut self) -> bool {
        false
    }
}

struct Supplicant {
    pid: Option<u32>,
    started: Option<Instant>,
}

/// The drivers that were asked for and the supplicants that run.
pub(crate) struct Keeper {
    /// `false` with `embra.wifi=0`: no driver is loaded this boot.
    enabled: bool,
    /// The alias list, read at the first look that needs it.
    aliases: Option<String>,
    /// The devices already matched against the list.
    looked_at: BTreeSet<String>,
    /// The modules already asked of `modprobe`, loaded or not.
    asked: BTreeSet<String>,
    /// The USB devices already looked up in the mode-switch data.
    switched: BTreeSet<(String, String)>,
    networks: Wifi,
    supplicants: BTreeMap<String, Supplicant>,
    /// A switch on the machine keeps a radio off.
    hard_block: bool,
    /// Whether a wireless driver was loaded this boot.
    loaded_any: bool,
}

impl Keeper {
    pub(crate) fn new(enabled: bool) -> Self {
        if !enabled {
            info!("embra.wifi=0: no wireless driver is loaded this boot");
        }
        Keeper {
            enabled,
            aliases: None,
            looked_at: BTreeSet::new(),
            asked: BTreeSet::new(),
            switched: BTreeSet::new(),
            networks: Wifi::default(),
            supplicants: BTreeMap::new(),
            hard_block: false,
            loaded_any: false,
        }
    }

    /// Whether the boot has a reason to wait for a Wi-Fi address: a network
    /// is stored, and a driver was loaded or a card is kept.
    pub(crate) fn expects_a_network(&self) -> bool {
        !self.networks.networks.is_empty() && (self.loaded_any || !self.supplicants.is_empty())
    }

    /// The drivers the devices ask for. A device is matched once, a module
    /// asked for once: a driver that does not load is not tried at every
    /// look.
    pub(crate) fn load_drivers(&mut self, radio: &mut dyn Radio) {
        if !self.enabled {
            return;
        }
        // A USB adapter that shows itself as a driver disk has no Wi-Fi
        // device to match until it is switched.
        for (place, ids) in radio.usb_devices() {
            if !self.switched.insert((place.clone(), ids.clone())) {
                continue;
            }
            if let Some(file) = radio.switch_file(&ids) {
                if radio.switch_mode(&ids, &file) {
                    info!("USB device {ids} at {place}: switched to its working mode");
                } else {
                    warn!("USB device {ids} at {place}: the mode switch failed; it stays what it shows itself as");
                }
            }
        }
        let fresh: Vec<String> = radio.modaliases().into_iter().filter(|m| self.looked_at.insert(m.clone())).collect();
        if fresh.is_empty() {
            return;
        }
        let aliases = self.aliases.get_or_insert_with(|| radio.alias_list()).clone();
        if aliases.is_empty() {
            return;
        }
        for modalias in fresh {
            for module in stored::modules_for(&aliases, &modalias, |pattern, device| radio.pattern_matches(pattern, device)) {
                if !self.asked.insert(module.clone()) {
                    continue;
                }
                if radio.load_module(&module) {
                    info!("the Wi-Fi driver {module} is loaded for {modalias}");
                    self.loaded_any = true;
                } else {
                    warn!("the Wi-Fi driver {module} could not be loaded for {modalias}");
                }
            }
        }
    }

    /// A supplicant on every Wi-Fi card that is there, with the stored
    /// networks as its configuration; none on a card that went.
    pub(crate) fn keep_supplicants(&mut self, radio: &mut dyn Radio, cards: &[&str], now: Instant) {
        let changed = match radio.stored_wifi() {
            Some(networks) => {
                self.networks = networks;
                true
            }
            None => false,
        };
        // The cards that went.
        let gone: Vec<String> = self.supplicants.keys().filter(|card| !cards.contains(&card.as_str())).cloned().collect();
        for card in gone {
            if let Some(Supplicant { pid: Some(pid), .. }) = self.supplicants.remove(&card) {
                radio.stop_supplicant(pid, &card);
            }
        }
        if cards.is_empty() {
            return;
        }
        // A network is stored: the radios are wanted.
        if !self.networks.networks.is_empty() {
            let hard = radio.unblock_radios();
            if hard && !self.hard_block {
                warn!("a switch on this machine keeps a Wi-Fi radio off");
            }
            self.hard_block = hard;
        }
        let config = stored::supplicant_config(&self.networks);
        for card in cards {
            let fresh = !self.supplicants.contains_key(*card);
            let entry = self.supplicants.entry((*card).to_string()).or_insert(Supplicant { pid: None, started: None });
            if let Some(pid) = entry.pid
                && radio.supplicant_gone(pid)
            {
                warn!("{card}: the supplicant exited; it is started again");
                entry.pid = None;
            }
            if fresh || changed {
                if !radio.write_supplicant_config(card, &config) {
                    warn!("{card}: the supplicant's configuration could not be written");
                    continue;
                }
                if let Some(pid) = entry.pid {
                    info!("{card}: the stored networks changed; the supplicant reads them again");
                    radio.reload_supplicant(pid);
                }
            }
            let rested = entry.started.is_none_or(|t| now.saturating_duration_since(t) >= RESTART_PAUSE);
            if entry.pid.is_none() && rested {
                if entry.started.is_none() {
                    info!("{card}: a Wi-Fi card; {}", if self.networks.networks.is_empty() { "no network is stored".to_string() } else { format!("{} stored network(s)", self.networks.networks.len()) });
                }
                entry.pid = radio.start_supplicant(card);
                entry.started = Some(now);
            }
        }
    }

    /// What the card's status line says about its radio, when there is
    /// something to say before it has joined anything.
    pub(crate) fn note(&self, card: &str) -> Option<String> {
        if !self.supplicants.contains_key(card) {
            return None;
        }
        if self.hard_block {
            return Some("a switch on this machine keeps the radio off".to_string());
        }
        self.networks.networks.is_empty().then(|| "no Wi-Fi network is stored: /network wifi lists the ones in range, /network wifi join <name> --key <passphrase> stores one".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embra_boot::wifi::WifiNetwork;

    #[derive(Default)]
    struct Fake {
        devices: Vec<String>,
        aliases: String,
        loads: Vec<String>,
        refuses: Vec<String>,
        usb: Vec<(String, String)>,
        switchable: Vec<String>,
        switches: Vec<String>,
        new_wifi: Option<Wifi>,
        configs: Vec<(String, String)>,
        started: Vec<(String, u32)>,
        exited: Vec<u32>,
        reloaded: Vec<u32>,
        stopped: Vec<(u32, String)>,
        unblocked: usize,
        hard: bool,
        alias_reads: usize,
    }

    impl Radio for Fake {
        fn modaliases(&mut self) -> Vec<String> {
            self.devices.clone()
        }
        fn alias_list(&mut self) -> String {
            self.alias_reads += 1;
            self.aliases.clone()
        }
        fn pattern_matches(&self, pattern: &str, modalias: &str) -> bool {
            // Enough for the test's patterns: a prefix, then `*`.
            pattern.strip_suffix('*').is_some_and(|prefix| modalias.starts_with(prefix))
        }
        fn load_module(&mut self, module: &str) -> bool {
            self.loads.push(module.to_string());
            !self.refuses.iter().any(|m| m == module)
        }
        fn usb_devices(&mut self) -> Vec<(String, String)> {
            self.usb.clone()
        }
        fn switch_file(&mut self, ids: &str) -> Option<PathBuf> {
            self.switchable.iter().any(|s| s == ids).then(|| PathBuf::from(format!("/usr/share/usb_modeswitch/{ids}")))
        }
        fn switch_mode(&mut self, ids: &str, _file: &Path) -> bool {
            self.switches.push(ids.to_string());
            true
        }
        fn stored_wifi(&mut self) -> Option<Wifi> {
            self.new_wifi.take()
        }
        fn write_supplicant_config(&mut self, card: &str, text: &str) -> bool {
            self.configs.push((card.to_string(), text.to_string()));
            true
        }
        fn start_supplicant(&mut self, card: &str) -> Option<u32> {
            let pid = 500 + self.started.len() as u32;
            self.started.push((card.to_string(), pid));
            Some(pid)
        }
        fn supplicant_gone(&mut self, pid: u32) -> bool {
            self.exited.contains(&pid)
        }
        fn reload_supplicant(&mut self, pid: u32) {
            self.reloaded.push(pid);
        }
        fn stop_supplicant(&mut self, pid: u32, card: &str) {
            self.stopped.push((pid, card.to_string()));
        }
        fn unblock_radios(&mut self) -> bool {
            self.unblocked += 1;
            self.hard
        }
    }

    const ALIASES: &str = "alias pci:v00008086d00002723* iwlwifi\nalias usb:v0BDAp8179* rtl8xxxu\nalias usb:v0BDApC811* rtw88_8821cu\n";

    fn home() -> Wifi {
        Wifi { country: Some("US".into()), networks: vec![WifiNetwork { ssid: b"Home".to_vec(), key: Some("correct horse".into()), hidden: false }] }
    }

    #[test]
    fn a_card_whose_modalias_matches_the_list_gets_its_module() {
        let mut radio = Fake { aliases: ALIASES.into(), devices: vec!["pci:v00008086d0000100Esv00".into(), "pci:v00008086d00002723sv00008086".into()], ..Fake::default() };
        let mut wifi = Keeper::new(true);
        wifi.load_drivers(&mut radio);
        assert_eq!(radio.loads, ["iwlwifi"], "the wired card asks for nothing in a list of wireless drivers");
        // The next looks find the same devices: nothing is matched or loaded twice.
        wifi.load_drivers(&mut radio);
        wifi.load_drivers(&mut radio);
        assert_eq!((radio.loads.len(), radio.alias_reads), (1, 1));
        // An adapter plugged in later is a new device.
        radio.devices.push("usb:v0BDAp8179d0000".into());
        wifi.load_drivers(&mut radio);
        assert_eq!(radio.loads, ["iwlwifi", "rtl8xxxu"]);
        // A second adapter of the same kind asks for a module already asked for.
        radio.devices.push("usb:v0BDAp8179d0200".into());
        wifi.load_drivers(&mut radio);
        assert_eq!(radio.loads.len(), 2);
    }

    #[test]
    fn a_device_that_matches_nothing_loads_nothing() {
        let mut radio = Fake { aliases: ALIASES.into(), devices: vec!["usb:v046DpC52Bd1211".into(), "pci:v00001AF4d00001000".into()], ..Fake::default() };
        let mut wifi = Keeper::new(true);
        wifi.load_drivers(&mut radio);
        assert!(radio.loads.is_empty());
        // An image without the list (no wireless drivers were built) loads nothing either.
        let mut bare = Fake { devices: vec!["pci:v00008086d00002723sv0".into()], ..Fake::default() };
        Keeper::new(true).load_drivers(&mut bare);
        assert!(bare.loads.is_empty());
    }

    #[test]
    fn a_driver_that_does_not_load_is_asked_for_once() {
        let mut radio = Fake { aliases: ALIASES.into(), devices: vec!["usb:v0BDAp8179d0000".into()], refuses: vec!["rtl8xxxu".into()], ..Fake::default() };
        let mut wifi = Keeper::new(true);
        for _ in 0..5 {
            wifi.load_drivers(&mut radio);
        }
        assert_eq!(radio.loads, ["rtl8xxxu"]);
        assert!(!wifi.expects_a_network());
    }

    #[test]
    fn embra_wifi_0_loads_no_driver_and_switches_no_device() {
        let mut radio = Fake { aliases: ALIASES.into(), devices: vec!["pci:v00008086d00002723sv0".into()], usb: vec![("1-2".into(), "0bda:1a2b".into())], switchable: vec!["0bda:1a2b".into()], ..Fake::default() };
        let mut wifi = Keeper::new(false);
        wifi.load_drivers(&mut radio);
        assert!(radio.loads.is_empty() && radio.switches.is_empty() && radio.alias_reads == 0);
    }

    #[test]
    fn a_usb_adapter_that_shows_itself_as_a_disk_is_switched_once() {
        let mut radio = Fake { usb: vec![("1-2".into(), "0bda:1a2b".into()), ("1-3".into(), "046d:c52b".into())], switchable: vec!["0bda:1a2b".into()], ..Fake::default() };
        let mut wifi = Keeper::new(true);
        wifi.load_drivers(&mut radio);
        wifi.load_drivers(&mut radio);
        assert_eq!(radio.switches, ["0bda:1a2b"], "the one device the data names, once");
        // The same adapter in another port is another plug.
        radio.usb.push(("1-4".into(), "0bda:1a2b".into()));
        wifi.load_drivers(&mut radio);
        assert_eq!(radio.switches.len(), 2);
    }

    #[test]
    fn a_wifi_card_gets_a_supplicant_with_the_stored_networks() {
        let mut radio = Fake { new_wifi: Some(home()), ..Fake::default() };
        let mut wifi = Keeper::new(true);
        let t = Instant::now();
        wifi.keep_supplicants(&mut radio, &["wlan0"], t);
        assert_eq!(radio.started, [("wlan0".to_string(), 500)]);
        assert_eq!(radio.configs.len(), 1);
        let (card, config) = &radio.configs[0];
        assert_eq!(card, "wlan0");
        assert!(config.contains("ssid=486f6d65") && config.contains("psk=\"correct horse\"") && config.contains("country=US"), "{config}");
        assert_eq!(radio.unblocked, 1, "a stored network lifts the soft block");
        assert!(wifi.expects_a_network());
        assert_eq!(wifi.note("wlan0"), None);
        // Nothing changed: nothing is written, reloaded or started again.
        wifi.keep_supplicants(&mut radio, &["wlan0"], t + Duration::from_secs(2));
        assert_eq!((radio.configs.len(), radio.started.len(), radio.reloaded.len()), (1, 1, 0));
        // No wired card is this module's business.
        assert_eq!(wifi.note("eth0"), None);
    }

    #[test]
    fn a_card_without_a_stored_network_still_has_a_supplicant_and_says_what_to_do() {
        let mut radio = Fake { new_wifi: Some(Wifi::default()), ..Fake::default() };
        let mut wifi = Keeper::new(true);
        wifi.keep_supplicants(&mut radio, &["wlan0"], Instant::now());
        assert_eq!(radio.started.len(), 1, "the supplicant is what scans for /network wifi");
        assert_eq!(radio.unblocked, 0, "no network is stored: the radio is left as it is");
        assert!(!wifi.expects_a_network());
        assert!(wifi.note("wlan0").unwrap().starts_with("no Wi-Fi network is stored"));
        assert!(radio.configs[0].1.starts_with("ctrl_interface=DIR=") && !radio.configs[0].1.contains("network="));
    }

    #[test]
    fn stored_networks_that_change_are_written_again_and_the_supplicant_reads_them() {
        let mut radio = Fake { new_wifi: Some(Wifi::default()), ..Fake::default() };
        let mut wifi = Keeper::new(true);
        let t = Instant::now();
        wifi.keep_supplicants(&mut radio, &["wlan0"], t);
        radio.new_wifi = Some(home());
        wifi.keep_supplicants(&mut radio, &["wlan0"], t + Duration::from_secs(2));
        assert_eq!(radio.configs.len(), 2);
        assert!(radio.configs[1].1.contains("ssid=486f6d65"));
        assert_eq!(radio.reloaded, [500], "the running supplicant, not a new one");
        assert_eq!(radio.started.len(), 1);
        assert_eq!(wifi.note("wlan0"), None);
    }

    #[test]
    fn a_supplicant_that_exits_is_started_again_after_a_pause_and_one_on_a_card_that_went_is_stopped() {
        let mut radio = Fake { new_wifi: Some(home()), ..Fake::default() };
        let mut wifi = Keeper::new(true);
        let t = Instant::now();
        wifi.keep_supplicants(&mut radio, &["wlan0", "wlan1"], t);
        assert_eq!(radio.started.len(), 2);
        radio.exited.push(500);
        wifi.keep_supplicants(&mut radio, &["wlan0", "wlan1"], t + Duration::from_secs(2));
        assert_eq!(radio.started.len(), 2, "not sooner than the pause");
        wifi.keep_supplicants(&mut radio, &["wlan0", "wlan1"], t + Duration::from_secs(6));
        assert_eq!(radio.started.last().unwrap(), &("wlan0".to_string(), 502));
        // The adapter is pulled.
        wifi.keep_supplicants(&mut radio, &["wlan0"], t + Duration::from_secs(8));
        assert_eq!(radio.stopped, [(501, "wlan1".to_string())]);
        wifi.keep_supplicants(&mut radio, &[], t + Duration::from_secs(10));
        assert_eq!(radio.stopped.len(), 2);
        assert!(!wifi.expects_a_network(), "no card, no driver loaded: nothing to wait for");
    }

    #[test]
    fn a_switch_on_the_machine_is_said_in_the_cards_status() {
        let mut radio = Fake { new_wifi: Some(home()), hard: true, ..Fake::default() };
        let mut wifi = Keeper::new(true);
        wifi.keep_supplicants(&mut radio, &["wlan0"], Instant::now());
        assert_eq!(wifi.note("wlan0").as_deref(), Some("a switch on this machine keeps the radio off"));
        radio.hard = false;
        wifi.keep_supplicants(&mut radio, &["wlan0"], Instant::now());
        assert_eq!(wifi.note("wlan0"), None);
    }
}
