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
//!   The supplicant is also what brings the card up.
//! - **Nothing here waits for a radio.** A driver's load runs its card's
//!   probe, and bringing a card up runs the driver's start: on a USB adapter
//!   each writes the chip over the bus, seconds on a machine and half a
//!   minute through a hypervisor's USB (seen with an RTL8852AU, 2026-11-15:
//!   nine seconds to load, thirty-one to come up). So `modprobe` and the
//!   mode switch are helpers that are started and looked at again at a later
//!   look, the card is raised by its supplicant in the supplicant's own
//!   process, and a supplicant is not started while a wired card is in its
//!   first seconds of asking: the driver's start holds the kernel's network
//!   lock, and no address can be set on any card meanwhile.
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
/// A driver's load or a mode switch that still runs after this long is
/// said once. Nothing waits for it either way.
const SLOW_HELPER: Duration = Duration::from_secs(20);

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
    /// `modprobe <module>`, with what it depends on, started and not
    /// waited for; the helper's pid.
    fn start_load(&mut self, _module: &str) -> Option<u32> {
        None
    }
    /// The USB devices: where each is plugged, and its `vvvv:pppp`.
    fn usb_devices(&mut self) -> Vec<(String, String)> {
        Vec::new()
    }
    /// The mode-switch data for a device, when the data names it.
    fn switch_file(&mut self, _ids: &str) -> Option<PathBuf> {
        None
    }
    /// The switch of the device to the mode that file describes, started
    /// and not waited for; the helper's pid.
    fn start_switch(&mut self, _ids: &str, _file: &Path) -> Option<u32> {
        None
    }
    /// Whether a helper one of the two above started has ended: `None`
    /// while it runs, else whether it succeeded (and it is collected).
    fn helper_done(&mut self, _pid: u32) -> Option<bool> {
        None
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

/// What a helper that was started and is not waited for is doing.
enum Errand {
    Load { module: String, modalias: String },
    Switch { ids: String, place: String },
}

struct Pending {
    pid: u32,
    errand: Errand,
    since: Instant,
    slow_said: bool,
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
    /// The driver loads and mode switches that run now.
    pending: Vec<Pending>,
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
            pending: Vec::new(),
        }
    }

    /// Whether the boot has a reason to wait for a Wi-Fi address: a network
    /// is stored, and a driver is loading or was loaded, a device is being
    /// switched, or a card is kept.
    pub(crate) fn expects_a_network(&self) -> bool {
        !self.networks.networks.is_empty() && (self.loaded_any || !self.pending.is_empty() || !self.supplicants.is_empty())
    }

    /// The drivers the devices ask for. A device is matched once, a module
    /// asked for once: a driver that does not load is not tried at every
    /// look. A load and a mode switch are started here and their end is
    /// read at a later look: no look waits for one.
    pub(crate) fn load_drivers(&mut self, radio: &mut dyn Radio, now: Instant) {
        if !self.enabled {
            return;
        }
        // What an earlier look started and has ended since.
        for mut pending in std::mem::take(&mut self.pending) {
            let Some(succeeded) = radio.helper_done(pending.pid) else {
                let waited = now.saturating_duration_since(pending.since);
                if !pending.slow_said && waited >= SLOW_HELPER {
                    pending.slow_said = true;
                    match &pending.errand {
                        Errand::Load { module, modalias } => warn!("the Wi-Fi driver {module} is still loading for {modalias} after {} s; nothing waits for it", waited.as_secs()),
                        Errand::Switch { ids, place } => warn!("USB device {ids} at {place}: the mode switch still runs after {} s; nothing waits for it", waited.as_secs()),
                    }
                }
                self.pending.push(pending);
                continue;
            };
            match (pending.errand, succeeded) {
                (Errand::Load { module, modalias }, true) => {
                    info!("the Wi-Fi driver {module} is loaded for {modalias}");
                    self.loaded_any = true;
                }
                (Errand::Load { module, modalias }, false) => warn!("the Wi-Fi driver {module} could not be loaded for {modalias}"),
                (Errand::Switch { ids, place }, true) => info!("USB device {ids} at {place}: switched to its working mode"),
                (Errand::Switch { ids, place }, false) => warn!("USB device {ids} at {place}: the mode switch failed; it stays what it shows itself as"),
            }
        }
        // A USB adapter that shows itself as a driver disk has no Wi-Fi
        // device to match until it is switched.
        for (place, ids) in radio.usb_devices() {
            if !self.switched.insert((place.clone(), ids.clone())) {
                continue;
            }
            if let Some(file) = radio.switch_file(&ids) {
                match radio.start_switch(&ids, &file) {
                    Some(pid) => self.pending.push(Pending { pid, errand: Errand::Switch { ids, place }, since: now, slow_said: false }),
                    None => warn!("USB device {ids} at {place}: the mode-switch tool could not be started; it stays what it shows itself as"),
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
                match radio.start_load(&module) {
                    Some(pid) => self.pending.push(Pending { pid, errand: Errand::Load { module, modalias: modalias.clone() }, since: now, slow_said: false }),
                    None => warn!("the Wi-Fi driver {module} could not be loaded for {modalias}: modprobe could not be started"),
                }
            }
        }
    }

    /// A supplicant on every Wi-Fi card that is there, with the stored
    /// networks as its configuration; none on a card that went. The
    /// supplicant brings its card up, and with `wired_first` none is
    /// started at this look: a wired card is in its first seconds of
    /// asking, and a radio's start would hold its address back.
    pub(crate) fn keep_supplicants(&mut self, radio: &mut dyn Radio, cards: &[&str], now: Instant, wired_first: bool) {
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
            if entry.pid.is_none() && rested && !wired_first {
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
        /// The modules a load was started for, in order.
        loads: Vec<String>,
        /// The modules whose load fails.
        refuses: Vec<String>,
        /// The modules whose load still runs.
        loading: Vec<String>,
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
        fn start_load(&mut self, module: &str) -> Option<u32> {
            self.loads.push(module.to_string());
            Some(700 + self.loads.len() as u32 - 1)
        }
        fn helper_done(&mut self, pid: u32) -> Option<bool> {
            // A mode switch (800 and up) ends at once and well.
            if pid >= 800 {
                return Some(true);
            }
            let module = self.loads.get(pid.checked_sub(700)? as usize)?;
            if self.loading.iter().any(|m| m == module) {
                return None;
            }
            Some(!self.refuses.iter().any(|m| m == module))
        }
        fn usb_devices(&mut self) -> Vec<(String, String)> {
            self.usb.clone()
        }
        fn switch_file(&mut self, ids: &str) -> Option<PathBuf> {
            self.switchable.iter().any(|s| s == ids).then(|| PathBuf::from(format!("/usr/share/usb_modeswitch/{ids}")))
        }
        fn start_switch(&mut self, ids: &str, _file: &Path) -> Option<u32> {
            self.switches.push(ids.to_string());
            Some(800 + self.switches.len() as u32 - 1)
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
        wifi.load_drivers(&mut radio, Instant::now());
        assert_eq!(radio.loads, ["iwlwifi"], "the wired card asks for nothing in a list of wireless drivers");
        // The next looks find the same devices: nothing is matched or loaded twice.
        wifi.load_drivers(&mut radio, Instant::now());
        wifi.load_drivers(&mut radio, Instant::now());
        assert_eq!((radio.loads.len(), radio.alias_reads), (1, 1));
        // An adapter plugged in later is a new device.
        radio.devices.push("usb:v0BDAp8179d0000".into());
        wifi.load_drivers(&mut radio, Instant::now());
        assert_eq!(radio.loads, ["iwlwifi", "rtl8xxxu"]);
        // A second adapter of the same kind asks for a module already asked for.
        radio.devices.push("usb:v0BDAp8179d0200".into());
        wifi.load_drivers(&mut radio, Instant::now());
        assert_eq!(radio.loads.len(), 2);
    }

    #[test]
    fn a_device_that_matches_nothing_loads_nothing() {
        let mut radio = Fake { aliases: ALIASES.into(), devices: vec!["usb:v046DpC52Bd1211".into(), "pci:v00001AF4d00001000".into()], ..Fake::default() };
        let mut wifi = Keeper::new(true);
        wifi.load_drivers(&mut radio, Instant::now());
        assert!(radio.loads.is_empty());
        // An image without the list (no wireless drivers were built) loads nothing either.
        let mut bare = Fake { devices: vec!["pci:v00008086d00002723sv0".into()], ..Fake::default() };
        Keeper::new(true).load_drivers(&mut bare, Instant::now());
        assert!(bare.loads.is_empty());
    }

    #[test]
    fn a_driver_that_does_not_load_is_asked_for_once() {
        let mut radio = Fake { aliases: ALIASES.into(), devices: vec!["usb:v0BDAp8179d0000".into()], refuses: vec!["rtl8xxxu".into()], new_wifi: Some(home()), ..Fake::default() };
        let mut wifi = Keeper::new(true);
        // The stored networks are read; there is no card.
        wifi.keep_supplicants(&mut radio, &[], Instant::now(), false);
        for _ in 0..5 {
            wifi.load_drivers(&mut radio, Instant::now());
        }
        assert_eq!(radio.loads, ["rtl8xxxu"]);
        assert!(!wifi.expects_a_network(), "a network is stored, and no card is on its way: the boot does not wait for one");
    }

    #[test]
    fn no_look_waits_for_a_drivers_load_and_the_boot_waits_for_the_card_it_brings() {
        let mut radio = Fake { aliases: ALIASES.into(), devices: vec!["usb:v0BDApC811d0200".into()], loading: vec!["rtw88_8821cu".into()], new_wifi: Some(home()), ..Fake::default() };
        let mut wifi = Keeper::new(true);
        let t = Instant::now();
        wifi.keep_supplicants(&mut radio, &[], t, false);
        assert!(!wifi.expects_a_network(), "a stored network and nothing to join it with");
        // The look that finds the device starts the load and is over: the
        // driver's probe of its card runs on, seconds on a USB adapter.
        wifi.load_drivers(&mut radio, t);
        assert_eq!(radio.loads, ["rtw88_8821cu"]);
        assert!(wifi.expects_a_network(), "a driver is loading: its card is on the way");
        for look in 1..=14 {
            wifi.load_drivers(&mut radio, t + Duration::from_secs(2 * look));
        }
        assert_eq!(radio.loads.len(), 1, "asked for once, however long it takes");
        assert!(wifi.expects_a_network());
        // It has loaded: the card appears at this look or the next.
        radio.loading.clear();
        wifi.load_drivers(&mut radio, t + Duration::from_secs(30));
        assert!(wifi.expects_a_network());
        assert_eq!(radio.loads.len(), 1);
        // A load that fails after a while leaves nothing to wait for.
        let mut radio = Fake { aliases: ALIASES.into(), devices: vec!["usb:v0BDApC811d0200".into()], loading: vec!["rtw88_8821cu".into()], refuses: vec!["rtw88_8821cu".into()], new_wifi: Some(home()), ..Fake::default() };
        let mut wifi = Keeper::new(true);
        wifi.keep_supplicants(&mut radio, &[], t, false);
        wifi.load_drivers(&mut radio, t);
        assert!(wifi.expects_a_network());
        radio.loading.clear();
        wifi.load_drivers(&mut radio, t + Duration::from_secs(4));
        assert!(!wifi.expects_a_network());
    }

    #[test]
    fn no_supplicant_is_started_while_the_wired_cards_go_first() {
        let mut radio = Fake { new_wifi: Some(home()), ..Fake::default() };
        let mut wifi = Keeper::new(true);
        let t = Instant::now();
        // The supplicant brings the card up, and a radio's start holds
        // every card's address back: not while a wired card is asking.
        wifi.keep_supplicants(&mut radio, &["wlan0"], t, true);
        assert!(radio.started.is_empty());
        assert_eq!(radio.configs.len(), 1, "its configuration is ready");
        assert!(wifi.expects_a_network(), "the card is kept: the boot waits for it when Wi-Fi is the way");
        wifi.keep_supplicants(&mut radio, &["wlan0"], t + Duration::from_millis(300), false);
        assert_eq!(radio.started, [("wlan0".to_string(), 500)]);
        assert_eq!(radio.configs.len(), 1);
        // A supplicant that runs is left alone by a wired card that comes later.
        wifi.keep_supplicants(&mut radio, &["wlan0"], t + Duration::from_secs(4), true);
        assert!(radio.stopped.is_empty() && radio.started.len() == 1 && radio.reloaded.is_empty());
        // One that exited is started again once the wired cards are through.
        radio.exited.push(500);
        wifi.keep_supplicants(&mut radio, &["wlan0"], t + Duration::from_secs(20), true);
        assert_eq!(radio.started.len(), 1);
        wifi.keep_supplicants(&mut radio, &["wlan0"], t + Duration::from_secs(22), false);
        assert_eq!(radio.started.len(), 2);
    }

    #[test]
    fn embra_wifi_0_loads_no_driver_and_switches_no_device() {
        let mut radio = Fake { aliases: ALIASES.into(), devices: vec!["pci:v00008086d00002723sv0".into()], usb: vec![("1-2".into(), "0bda:1a2b".into())], switchable: vec!["0bda:1a2b".into()], ..Fake::default() };
        let mut wifi = Keeper::new(false);
        wifi.load_drivers(&mut radio, Instant::now());
        assert!(radio.loads.is_empty() && radio.switches.is_empty() && radio.alias_reads == 0);
    }

    #[test]
    fn a_usb_adapter_that_shows_itself_as_a_disk_is_switched_once() {
        let mut radio = Fake { usb: vec![("1-2".into(), "0bda:1a2b".into()), ("1-3".into(), "046d:c52b".into())], switchable: vec!["0bda:1a2b".into()], ..Fake::default() };
        let mut wifi = Keeper::new(true);
        wifi.load_drivers(&mut radio, Instant::now());
        wifi.load_drivers(&mut radio, Instant::now());
        assert_eq!(radio.switches, ["0bda:1a2b"], "the one device the data names, once");
        // The same adapter in another port is another plug.
        radio.usb.push(("1-4".into(), "0bda:1a2b".into()));
        wifi.load_drivers(&mut radio, Instant::now());
        assert_eq!(radio.switches.len(), 2);
    }

    #[test]
    fn a_wifi_card_gets_a_supplicant_with_the_stored_networks() {
        let mut radio = Fake { new_wifi: Some(home()), ..Fake::default() };
        let mut wifi = Keeper::new(true);
        let t = Instant::now();
        wifi.keep_supplicants(&mut radio, &["wlan0"], t, false);
        assert_eq!(radio.started, [("wlan0".to_string(), 500)]);
        assert_eq!(radio.configs.len(), 1);
        let (card, config) = &radio.configs[0];
        assert_eq!(card, "wlan0");
        assert!(config.contains("ssid=486f6d65") && config.contains("psk=\"correct horse\"") && config.contains("country=US"), "{config}");
        assert_eq!(radio.unblocked, 1, "a stored network lifts the soft block");
        assert!(wifi.expects_a_network());
        assert_eq!(wifi.note("wlan0"), None);
        // Nothing changed: nothing is written, reloaded or started again.
        wifi.keep_supplicants(&mut radio, &["wlan0"], t + Duration::from_secs(2), false);
        assert_eq!((radio.configs.len(), radio.started.len(), radio.reloaded.len()), (1, 1, 0));
        // No wired card is this module's business.
        assert_eq!(wifi.note("eth0"), None);
    }

    #[test]
    fn a_card_without_a_stored_network_still_has_a_supplicant_and_says_what_to_do() {
        let mut radio = Fake { new_wifi: Some(Wifi::default()), ..Fake::default() };
        let mut wifi = Keeper::new(true);
        wifi.keep_supplicants(&mut radio, &["wlan0"], Instant::now(), false);
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
        wifi.keep_supplicants(&mut radio, &["wlan0"], t, false);
        radio.new_wifi = Some(home());
        wifi.keep_supplicants(&mut radio, &["wlan0"], t + Duration::from_secs(2), false);
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
        wifi.keep_supplicants(&mut radio, &["wlan0", "wlan1"], t, false);
        assert_eq!(radio.started.len(), 2);
        radio.exited.push(500);
        wifi.keep_supplicants(&mut radio, &["wlan0", "wlan1"], t + Duration::from_secs(2), false);
        assert_eq!(radio.started.len(), 2, "not sooner than the pause");
        wifi.keep_supplicants(&mut radio, &["wlan0", "wlan1"], t + Duration::from_secs(6), false);
        assert_eq!(radio.started.last().unwrap(), &("wlan0".to_string(), 502));
        // The adapter is pulled.
        wifi.keep_supplicants(&mut radio, &["wlan0"], t + Duration::from_secs(8), false);
        assert_eq!(radio.stopped, [(501, "wlan1".to_string())]);
        wifi.keep_supplicants(&mut radio, &[], t + Duration::from_secs(10), false);
        assert_eq!(radio.stopped.len(), 2);
        assert!(!wifi.expects_a_network(), "no card, no driver loaded: nothing to wait for");
    }

    #[test]
    fn a_switch_on_the_machine_is_said_in_the_cards_status() {
        let mut radio = Fake { new_wifi: Some(home()), hard: true, ..Fake::default() };
        let mut wifi = Keeper::new(true);
        wifi.keep_supplicants(&mut radio, &["wlan0"], Instant::now(), false);
        assert_eq!(wifi.note("wlan0").as_deref(), Some("a switch on this machine keeps the radio off"));
        radio.hard = false;
        wifi.keep_supplicants(&mut radio, &["wlan0"], Instant::now(), false);
        assert_eq!(wifi.note("wlan0"), None);
    }
}
