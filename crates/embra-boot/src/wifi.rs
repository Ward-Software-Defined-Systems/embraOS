//! Wi-Fi: the networks the operator stored, the supplicant's configuration
//! made of them, and what the supplicant answers on its control socket.
//!
//! A Wi-Fi card is a network card like any other once it has joined a
//! network (`net.rs`, `net_lease.rs`): a DHCP client, a record, a route.
//! What comes before is here. The operator's networks are a file on STATE
//! (`/network wifi join`, written by the brain); embrad renders
//! `wpa_supplicant`'s configuration from it into a directory no tool can
//! read, keeps one supplicant a Wi-Fi card, and tells it to read the
//! configuration again when the file changes. The brain asks the
//! supplicant's control socket what is in range and what the card is on.
//!
//! The configuration's syntax and the control socket's protocol are
//! `wpa_supplicant` 2.11's, read from its source (`config.c`,
//! `src/utils/config.c`, `ctrl_iface.c`, `src/utils/common.c`).

use std::path::Path;

/// The networks the operator stored. Mode 0600: it holds passphrases.
pub const WIFI_PATH: &str = "/embra/state/network/wifi";
/// Where embrad renders the supplicant's configuration, a file a card. The
/// read jail refuses the directory: the files hold the passphrases.
pub const SECRET_DIR: &str = "/embra/ephemeral/net/secret";
/// The supplicant's control sockets, one a card, named after it.
pub const CTRL_DIR: &str = "/embra/ephemeral/net/wpa";
/// The supplicant's own lines, a file a card (`<card>.log`): what it joined,
/// what refused it. No key is in them at its default verbosity.
pub const LOG_DIR: &str = "/embra/ephemeral/net/wpa-log";
/// The modules embrad may load, with the device patterns that ask for each:
/// the lines of `modules.alias` whose module is a wireless driver
/// (`post_build.sh`).
pub const ALIAS_PATH: &str = "/usr/share/embra/wireless.alias";

/// How many networks are kept.
pub const NETWORKS_MAX: usize = 16;

/// One network the operator stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WifiNetwork {
    /// The network's name as it is on the air: 1 to 32 bytes, text or not.
    pub ssid: Vec<u8>,
    /// The passphrase; `None` for a network without one.
    pub key: Option<String>,
    /// The network does not announce its name and is asked for by it.
    pub hidden: bool,
}

/// What the operator stored about Wi-Fi.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Wifi {
    /// The regulatory domain, two letters: which channels may be used.
    pub country: Option<String>,
    /// In the order they were joined; the last one is preferred.
    pub networks: Vec<WifiNetwork>,
}

/// A network name: 1 to 32 bytes.
pub fn ssid_ok(ssid: &[u8]) -> bool {
    (1..=32).contains(&ssid.len())
}

/// A regulatory domain: two letters, written in capitals.
pub fn country_ok(code: &str) -> bool {
    code.len() == 2 && code.bytes().all(|b| b.is_ascii_uppercase())
}

/// Whether a passphrase can be stored: 8 to 63 bytes (WPA's own bounds,
/// which the supplicant checks too, printing what it refuses), no control
/// character, and written in a way the supplicant's configuration reads
/// back whole. The error is a sentence for the operator and never carries
/// the passphrase.
pub fn passphrase_ok(key: &str) -> Result<(), String> {
    if !(8..=63).contains(&key.len()) {
        return Err(format!("a passphrase is 8 to 63 characters, and this one has {}", key.len()));
    }
    if key.chars().any(|c| c.is_control()) {
        return Err("a passphrase holds no control character".to_string());
    }
    // The supplicant strips a `#` comment from a configuration line unless
    // the `#` stands inside double quotes, and it pairs the quotes as they
    // come: a `#` after an odd number of `"` inside the passphrase would end
    // the line there.
    let mut quotes = 0usize;
    for c in key.chars() {
        match c {
            '"' => quotes += 1,
            '#' if quotes % 2 == 1 => {
                return Err("a # after a single \" in a passphrase is read as a comment by the supplicant's configuration: this passphrase cannot be stored".to_string());
            }
            _ => {}
        }
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

impl Wifi {
    /// The file's text: `country=<CC>`, then a line a network,
    /// `network=<name in hex> open|key=<passphrase in hex> [hidden]`.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        if let Some(country) = &self.country {
            out.push_str(&format!("country={country}\n"));
        }
        for n in &self.networks {
            let key = match &n.key {
                Some(key) => format!("key={}", hex(key.as_bytes())),
                None => "open".to_string(),
            };
            out.push_str(&format!("network={} {key}{}\n", hex(&n.ssid), if n.hidden { " hidden" } else { "" }));
        }
        out
    }

    /// The settings and the lines that were not accepted, each as a reason
    /// that names the line by its number and never by its text (a line may
    /// hold a passphrase). A bad line is skipped; the rest stands.
    pub fn parse(text: &str) -> (Wifi, Vec<String>) {
        let mut wifi = Wifi::default();
        let mut refused = Vec::new();
        for (number, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let accepted = match line.split_once('=') {
                Some(("country", code)) if country_ok(code) => {
                    wifi.country = Some(code.to_string());
                    true
                }
                Some(("network", rest)) => match Self::network(rest) {
                    Some(network) if wifi.networks.len() < NETWORKS_MAX => {
                        wifi.networks.retain(|n| n.ssid != network.ssid);
                        wifi.networks.push(network);
                        true
                    }
                    _ => false,
                },
                _ => false,
            };
            if !accepted {
                refused.push(format!("line {} is not a Wi-Fi setting", number + 1));
            }
        }
        (wifi, refused)
    }

    fn network(rest: &str) -> Option<WifiNetwork> {
        let mut words = rest.split_whitespace();
        let ssid = unhex(words.next()?).filter(|s| ssid_ok(s))?;
        let key = match words.next()? {
            "open" => None,
            word => {
                let key = String::from_utf8(unhex(word.strip_prefix("key=")?)?).ok()?;
                passphrase_ok(&key).ok()?;
                Some(key)
            }
        };
        let hidden = match words.next() {
            None => false,
            Some("hidden") => true,
            Some(_) => return None,
        };
        words.next().is_none().then_some(WifiNetwork { ssid, key, hidden })
    }

    /// What is stored at `path`; nothing when there is no file.
    pub fn read(path: &Path) -> (Wifi, Vec<String>) {
        match std::fs::read_to_string(path) {
            Ok(text) => Wifi::parse(&text),
            Err(_) => (Wifi::default(), Vec::new()),
        }
    }

    /// The file written whole and private: a temporary file beside it,
    /// created 0600, synced and renamed over.
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        use std::io::Write;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(self.to_text().as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    }

    /// A network joined: one stored under the same name is replaced, and
    /// the new one is the preferred one. `false` when the list is full.
    pub fn join(&mut self, network: WifiNetwork) -> bool {
        let known = self.networks.iter().any(|n| n.ssid == network.ssid);
        if !known && self.networks.len() >= NETWORKS_MAX {
            return false;
        }
        self.networks.retain(|n| n.ssid != network.ssid);
        self.networks.push(network);
        true
    }

    /// A network taken away; `false` when none had that name.
    pub fn forget(&mut self, ssid: &[u8]) -> bool {
        let before = self.networks.len();
        self.networks.retain(|n| n.ssid != ssid);
        self.networks.len() != before
    }
}

/// `wpa_supplicant`'s configuration for one card: the control socket's
/// directory, the regulatory domain, and a block a network. A name is
/// written in hex, so no name can break out of its line. A passphrase is
/// written between double quotes, which is the one form the supplicant
/// takes a passphrase in ([`passphrase_ok`] has refused what that form
/// cannot hold); the same passphrase serves WPA2 and WPA3. A later network
/// has the higher priority.
pub fn supplicant_config(wifi: &Wifi) -> String {
    let mut out = format!("ctrl_interface=DIR={CTRL_DIR}\nupdate_config=0\nsae_pwe=2\n");
    if let Some(country) = wifi.country.as_deref().filter(|c| country_ok(c)) {
        out.push_str(&format!("country={country}\n"));
    }
    for (index, network) in wifi.networks.iter().enumerate() {
        if !ssid_ok(&network.ssid) || network.key.as_deref().is_some_and(|k| passphrase_ok(k).is_err()) {
            continue;
        }
        out.push_str(&format!("network={{\n\tssid={}\n", hex(&network.ssid)));
        if network.hidden {
            out.push_str("\tscan_ssid=1\n");
        }
        match &network.key {
            Some(key) => out.push_str(&format!("\tkey_mgmt=WPA-PSK WPA-PSK-SHA256 SAE\n\tieee80211w=1\n\tpsk=\"{key}\"\n")),
            None => out.push_str("\tkey_mgmt=NONE\n"),
        }
        out.push_str(&format!("\tpriority={}\n}}\n", index + 1));
    }
    out
}

/// A name as the supplicant prints one (`wpa_ssid_txt`: `\"`, `\\`, `\e`,
/// `\n`, `\r`, `\t`, and `\xNN` for every byte outside 32 to 126), back to
/// its bytes. An escape it does not write is kept as it stands.
pub fn ssid_from_txt(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b != b'\\' {
            out.push(b);
            i += 1;
            continue;
        }
        match bytes.get(i + 1) {
            Some(b'"') => out.push(b'"'),
            Some(b'\\') => out.push(b'\\'),
            Some(b'e') => out.push(0x1b),
            Some(b'n') => out.push(b'\n'),
            Some(b'r') => out.push(b'\r'),
            Some(b't') => out.push(b'\t'),
            Some(b'x') => {
                let value = text.get(i + 2..i + 4).and_then(|h| u8::from_str_radix(h, 16).ok());
                match value {
                    Some(v) => {
                        out.push(v);
                        i += 4;
                        continue;
                    }
                    None => out.push(b'\\'),
                }
                i += 1;
                continue;
            }
            _ => {
                out.push(b'\\');
                i += 1;
                continue;
            }
        }
        i += 2;
    }
    out
}

/// A name for a line an operator reads: the text when it is printable
/// UTF-8, else its bytes in hex after `hex:`. Never a control character.
pub fn ssid_shown(ssid: &[u8]) -> String {
    match std::str::from_utf8(ssid) {
        Ok(text) if !text.is_empty() && !text.chars().any(|c| c.is_control()) => text.to_string(),
        _ => format!("hex:{}", hex(ssid)),
    }
}

/// A name as an operator types one: the text itself, or `hex:<bytes>` as
/// [`ssid_shown`] writes a name that is no text.
pub fn ssid_typed(text: &str) -> Option<Vec<u8>> {
    let ssid = match text.strip_prefix("hex:") {
        Some(digits) => unhex(digits)?,
        None => text.as_bytes().to_vec(),
    };
    ssid_ok(&ssid).then_some(ssid)
}

/// An access point in range, as the supplicant's table has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub ssid: Vec<u8>,
    /// The signal in dBm: nearer zero is stronger.
    pub signal: i32,
    pub freq: u32,
    pub flags: String,
}

/// The fields `BSS RANGE=` is asked for, as its `MASK=`: an entry's id, its
/// address, frequency, signal, flags and name, each on a line of its own,
/// and the line that ends an entry. Bits 0, 1, 2, 7, 11, 12 and 17 of
/// wpa_supplicant's `WPA_BSS_MASK_*` (`wpa_ctrl.h`, 2.11).
pub const BSS_MASK: &str = "21887";

/// One answer to `BSS RANGE=<id>- MASK=`[`BSS_MASK`]: the access points it
/// holds, the highest id among its entries, and whether the supplicant's
/// table ends with them.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BssPage {
    pub seen: Vec<Seen>,
    pub last_id: Option<u32>,
    pub end: bool,
}

/// An answer of the supplicant holds 4096 bytes at most, some forty access
/// points, and `BSS RANGE=` stops at the last entry that fits whole: the
/// table is read a page at a time, each asked for from the id after the
/// last one read. (`SCAN_RESULTS` has the same limit and no way on from
/// there. On a site with fifty-eight access points its answer was at the
/// limit: a denser one would have lost the ones found last, without a
/// word.) An entry ends with `====`, the table's last with `####`. An entry
/// without a name, or with a name no network can have, counts for its id
/// and is left out; the caller keeps one entry a name.
pub fn parse_bss_page(text: &str) -> BssPage {
    /// The lines of one entry, as far as they have been read.
    #[derive(Default)]
    struct Entry<'a> {
        id: Option<u32>,
        freq: Option<u32>,
        signal: Option<i32>,
        flags: Option<&'a str>,
        name: Option<&'a str>,
    }
    let mut page = BssPage::default();
    let mut entry = Entry::default();
    for line in text.lines() {
        if line != "====" && line != "####" {
            match line.split_once('=') {
                Some(("id", v)) => entry.id = v.parse().ok(),
                Some(("freq", v)) => entry.freq = v.parse().ok(),
                Some(("level", v)) => entry.signal = v.parse().ok(),
                Some(("flags", v)) => entry.flags = Some(v),
                Some(("ssid", v)) => entry.name = Some(v),
                _ => {}
            }
            continue;
        }
        page.end |= line == "####";
        let Entry { id, freq, signal, flags, name } = std::mem::take(&mut entry);
        let Some(id) = id else { continue };
        page.last_id = Some(page.last_id.map_or(id, |last| last.max(id)));
        let (Some(freq), Some(signal), Some(name)) = (freq, signal, name) else { continue };
        let ssid = ssid_from_txt(name);
        if ssid_ok(&ssid) && !ssid.iter().all(|b| *b == 0) {
            page.seen.push(Seen { ssid, signal, freq, flags: flags.unwrap_or_default().to_string() });
        }
    }
    page
}

/// What a network asks of a machine that joins it, from its flags
/// (`[WPA2-PSK-CCMP][ESS]`), in an operator's words.
pub fn security_words(flags: &str) -> &'static str {
    let has = |word: &str| flags.contains(word);
    if has("SAE") && has("PSK") {
        "WPA2/WPA3"
    } else if has("SAE") {
        "WPA3"
    } else if has("WPA2-PSK") || has("RSN-PSK") || has("WPA2-FT/PSK") {
        "WPA2"
    } else if has("WPA-PSK") {
        "WPA"
    } else if has("EAP") {
        "802.1X (a company network: not supported)"
    } else if has("WEP") {
        "WEP (not supported)"
    } else if has("OWE") {
        "open, encrypted (OWE: not supported)"
    } else {
        "open"
    }
}

/// Whether [`security_words`] names something a stored network can join.
pub fn joinable(flags: &str) -> bool {
    !security_words(flags).contains("not supported")
}

/// What a card is on, from the supplicant's `STATUS`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Link {
    /// `COMPLETED` once joined; `SCANNING`, `ASSOCIATING`,
    /// `4WAY_HANDSHAKE`, `DISCONNECTED`, `INACTIVE` (no network stored),
    /// `INTERFACE_DISABLED` on the way or without one.
    pub state: String,
    pub ssid: Option<Vec<u8>>,
    pub freq: Option<u32>,
    pub key_mgmt: Option<String>,
}

impl Link {
    pub fn joined(&self) -> bool {
        self.state == "COMPLETED"
    }
}

/// `STATUS`: `key=value` lines. The name is printed as in the scan results.
pub fn parse_status(text: &str) -> Link {
    let field = |key: &str| text.lines().filter_map(|l| l.split_once('=')).find(|(k, _)| *k == key).map(|(_, v)| v);
    Link {
        state: field("wpa_state").unwrap_or("").to_string(),
        ssid: field("ssid").map(ssid_from_txt).filter(|s| ssid_ok(s)),
        freq: field("freq").and_then(|v| v.parse().ok()),
        key_mgmt: field("key_mgmt").map(str::to_string),
    }
}

/// `SIGNAL_POLL`'s `RSSI=` in dBm.
pub fn parse_signal(text: &str) -> Option<i32> {
    text.lines().find_map(|l| l.strip_prefix("RSSI=")).and_then(|v| v.parse().ok())
}

/// A signal in an operator's words beside its number.
pub fn signal_words(dbm: i32) -> String {
    let word = match dbm {
        d if d >= -55 => "strong",
        d if d >= -67 => "good",
        d if d >= -78 => "fair",
        _ => "weak",
    };
    format!("{word}, {dbm} dBm")
}

/// The last refusal in a supplicant's log: the network's name when its
/// passphrase was refused (`CTRL-EVENT-SSID-TEMP-DISABLED … reason=WRONG_KEY`)
/// and nothing since says it joined.
pub fn wrong_key_in_log(log: &str) -> Option<Vec<u8>> {
    let mut refused = None;
    for line in log.lines() {
        if line.contains("CTRL-EVENT-CONNECTED") {
            refused = None;
        } else if line.contains("CTRL-EVENT-SSID-TEMP-DISABLED") && line.contains("reason=WRONG_KEY") {
            let name = line.split_once("ssid=\"").and_then(|(_, rest)| rest.rsplit_once("\" auth_failures=")).map(|(name, _)| name);
            refused = name.map(ssid_from_txt);
        }
    }
    refused
}

/// The modules a device asks for: every line `alias <pattern> <module>` of
/// the list whose pattern matches the device's `modalias`, each module
/// once, in the list's order. `matches` is the shell-pattern match
/// (`fnmatch`), given by the caller.
pub fn modules_for(aliases: &str, modalias: &str, matches: impl Fn(&str, &str) -> bool) -> Vec<String> {
    let mut modules: Vec<String> = Vec::new();
    for line in aliases.lines() {
        let mut words = line.split_whitespace();
        let (Some("alias"), Some(pattern), Some(module), None) = (words.next(), words.next(), words.next(), words.next()) else {
            continue;
        };
        if matches(pattern, modalias) && !modules.iter().any(|m| m == module) {
            modules.push(module.to_string());
        }
    }
    modules
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(name: &str, key: Option<&str>, hidden: bool) -> WifiNetwork {
        WifiNetwork { ssid: name.as_bytes().to_vec(), key: key.map(str::to_string), hidden }
    }

    #[test]
    fn a_passphrase_is_eight_to_sixty_three_characters_without_control_ones() {
        assert_eq!(passphrase_ok("eight ch"), Ok(()));
        assert_eq!(passphrase_ok(&"x".repeat(63)), Ok(()));
        assert!(passphrase_ok("seven c").unwrap_err().contains("8 to 63 characters, and this one has 7"));
        assert!(passphrase_ok(&"x".repeat(64)).unwrap_err().contains("has 64"));
        assert!(passphrase_ok("with\ta tab in it").unwrap_err().contains("control character"));
        assert!(passphrase_ok("a line\nbreak here").is_err());
        // A reason never carries what was refused.
        let secret = "s3cret\twith a tab";
        assert!(!passphrase_ok(secret).unwrap_err().contains("s3cret"));
        // A # is a comment to the supplicant unless it stands inside quotes
        // as the supplicant pairs them: after one " it is outside.
        assert_eq!(passphrase_ok("has # and \" later"), Ok(()));
        assert_eq!(passphrase_ok("two \"quotes\" # then"), Ok(()));
        assert!(passphrase_ok("one \" then # hash").unwrap_err().contains("read as a comment"));
    }

    #[test]
    fn the_stored_networks_read_back_and_a_bad_line_is_skipped_without_its_text() {
        let mut wifi = Wifi { country: Some("DE".into()), networks: vec![] };
        assert!(wifi.join(net("Home Net", Some("correct horse battery"), false)));
        assert!(wifi.join(net("café ☕", None, true)));
        let text = wifi.to_text();
        assert_eq!(text, "country=DE\nnetwork=486f6d65204e6574 key=636f727265637420686f7273652062617474657279\nnetwork=636166c3a920e29895 open hidden\n");
        assert_eq!(Wifi::parse(&text), (wifi.clone(), vec![]));
        // No passphrase stands in the file as it is typed.
        assert!(!text.contains("correct horse"));

        let (parsed, refused) = Wifi::parse("country=de\nnetwork=zz open\nnetwork=486f6d65 key=7368\nnetwork=486f6d65 open extra\nnetwork=4f70656e open\nfrobnicate=1\n# a comment\n\n");
        assert_eq!(parsed, Wifi { country: None, networks: vec![net("Open", None, false)] });
        assert_eq!(refused, ["line 1 is not a Wi-Fi setting", "line 2 is not a Wi-Fi setting", "line 3 is not a Wi-Fi setting", "line 4 is not a Wi-Fi setting", "line 6 is not a Wi-Fi setting"]);
        // A second line for a name replaces the first.
        let (twice, _) = Wifi::parse("network=41 open\nnetwork=41 key=3132333435363738\n");
        assert_eq!(twice.networks, [net("A", Some("12345678"), false)]);
    }

    #[test]
    fn a_network_joined_again_replaces_its_entry_and_becomes_the_preferred_one() {
        let mut wifi = Wifi::default();
        wifi.join(net("first", Some("passphrase one"), false));
        wifi.join(net("second", None, false));
        wifi.join(net("first", Some("passphrase two"), false));
        assert_eq!(wifi.networks, [net("second", None, false), net("first", Some("passphrase two"), false)]);
        assert!(wifi.forget(b"second") && !wifi.forget(b"second"));
        for i in 0..NETWORKS_MAX {
            wifi.join(net(&format!("n{i}"), None, false));
        }
        assert_eq!(wifi.networks.len(), NETWORKS_MAX);
        assert!(!wifi.join(net("one too many", None, false)), "the list is full");
        assert!(wifi.join(net("n3", Some("a new passphrase"), false)), "a known name is replaced, full or not");
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_written_whole_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("embra-wifi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("network").join("wifi");
        assert_eq!(Wifi::read(&path), (Wifi::default(), vec![]));
        let wifi = Wifi { country: Some("US".into()), networks: vec![net("Home", Some("12345678"), false)] };
        wifi.write(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(Wifi::read(&path).0, wifi);
        // Written again over itself, and no temporary file stays.
        wifi.write(&path).unwrap();
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_configuration_carries_the_name_in_hex_and_no_line_break_from_a_passphrase() {
        let wifi = Wifi {
            country: Some("US".into()),
            networks: vec![net("guest", None, false), net("Home \"Net\"\n}", Some("pass # \"quoted\" phrase"), true)],
        };
        let config = supplicant_config(&wifi);
        assert_eq!(
            config,
            "ctrl_interface=DIR=/embra/ephemeral/net/wpa\nupdate_config=0\nsae_pwe=2\ncountry=US\n\
             network={\n\tssid=6775657374\n\tkey_mgmt=NONE\n\tpriority=1\n}\n\
             network={\n\tssid=486f6d6520224e6574220a7d\n\tscan_ssid=1\n\tkey_mgmt=WPA-PSK WPA-PSK-SHA256 SAE\n\tieee80211w=1\n\tpsk=\"pass # \"quoted\" phrase\"\n\tpriority=2\n}\n"
        );
        // A passphrase the quoted form cannot hold never reaches the file,
        // however it got into the list; neither does a name of no length.
        let bad = Wifi { country: Some("xx".into()), networks: vec![net("a", Some("one \" then # hash"), false), net("", None, false), net("b", Some("short"), false)] };
        assert_eq!(supplicant_config(&bad), "ctrl_interface=DIR=/embra/ephemeral/net/wpa\nupdate_config=0\nsae_pwe=2\n");
        // Every line of a configuration is one the renderer wrote.
        assert!(config.lines().all(|l| l.starts_with(['c', 'u', 's', 'n', '\t', '}'])));
    }

    #[test]
    fn a_name_the_supplicant_printed_reads_back_to_its_bytes() {
        assert_eq!(ssid_from_txt("Home Net"), b"Home Net");
        assert_eq!(ssid_from_txt(r#"a\"b\\c\td\n\r\e"#), b"a\"b\\c\td\n\r\x1b");
        assert_eq!(ssid_from_txt(r"caf\xc3\xa9"), "café".as_bytes());
        assert_eq!(ssid_from_txt(r"\x00\xff"), [0x00, 0xff]);
        // What the supplicant never writes is kept as it stands.
        assert_eq!(ssid_from_txt(r"odd\q \xzz end\"), br"odd\q \xzz end\");
        assert_eq!(ssid_shown("café".as_bytes()), "café");
        assert_eq!(ssid_shown(&[0xff, 0x00]), "hex:ff00");
        assert_eq!(ssid_shown(b"bell\x07"), "hex:62656c6c07");
        assert_eq!(ssid_typed("Home Net"), Some(b"Home Net".to_vec()));
        assert_eq!(ssid_typed("hex:ff00"), Some(vec![0xff, 0x00]));
        assert_eq!(ssid_typed("hex:zz"), None);
        assert_eq!(ssid_typed(""), None);
        assert_eq!(ssid_typed(&"x".repeat(33)), None);
    }

    #[test]
    fn a_page_of_the_supplicants_table_is_read_into_names_signals_and_security() {
        // Seven entries as `BSS RANGE=0- MASK=21887` prints them; the last
        // of the table ends with `####`.
        let entry = |id: u32, freq: u32, level: i32, flags: &str, name: &str, last: bool| format!("id={id}\nbssid=02:00:00:00:{id:02x}:00\nfreq={freq}\nlevel={level}\nflags={flags}\nssid={name}\n{}\n", if last { "####" } else { "====" });
        let reply = [
            entry(0, 2412, -41, "[WPA2-PSK-CCMP][ESS]", "Home Net", false),
            entry(1, 5180, -63, "[WPA2-PSK+SAE-CCMP][ESS]", "Home Net", false),
            entry(4, 2437, -70, "[WPA2-SAE-CCMP][ESS]", "caf\\xc3\\xa9", false),
            entry(5, 2462, -80, "[ESS]", "guest", false),
            entry(6, 2462, -82, "[WPA2-EAP-CCMP][ESS]", "a=b corp", false),
            entry(9, 2462, -50, "[WPA2-PSK-CCMP][ESS]", "", false),
            entry(12, 2462, -51, "[WPA2-PSK-CCMP][ESS]", "\\x00\\x00\\x00", true),
        ]
        .concat();
        let page = parse_bss_page(&reply);
        assert_eq!(page.seen.iter().map(|s| (ssid_shown(&s.ssid), s.signal, s.freq, security_words(&s.flags))).collect::<Vec<_>>(), [
            ("Home Net".to_string(), -41, 2412, "WPA2"),
            ("Home Net".to_string(), -63, 5180, "WPA2/WPA3"),
            ("café".to_string(), -70, 2437, "WPA3"),
            ("guest".to_string(), -80, 2462, "open"),
            ("a=b corp".to_string(), -82, 2462, "802.1X (a company network: not supported)"),
        ]);
        assert_eq!((page.last_id, page.end), (Some(12), true), "an access point that hides its name counts for its id, and is left out");
        // A page that is not the table's last: the next is asked for from the id after.
        let page = parse_bss_page(&[entry(0, 2412, -41, "[ESS]", "one", false), entry(7, 2412, -42, "[ESS]", "two", false)].concat());
        assert_eq!((page.seen.len(), page.last_id, page.end), (2, Some(7), false));
        // A page that ends with access points that hide their names, and
        // one that holds nothing else: their ids count all the same, or the
        // next page would be asked for from before them, or not at all.
        let page = parse_bss_page(&[entry(0, 2412, -41, "[ESS]", "one", false), entry(7, 2412, -42, "[ESS]", "", false), entry(8, 2412, -43, "[ESS]", "\\x00\\x00", false)].concat());
        assert_eq!((page.seen.len(), page.last_id, page.end), (1, Some(8), false));
        let page = parse_bss_page(&[entry(20, 2412, -41, "[ESS]", "", false), entry(21, 2412, -42, "[ESS]", "", false)].concat());
        assert_eq!((page.seen.len(), page.last_id, page.end), (0, Some(21), false));
        // Nothing at or after the id asked for: an empty answer.
        assert_eq!(parse_bss_page(""), BssPage::default());
        // An answer cut inside an entry never comes (the supplicant stops
        // at the last one that fits whole); half an entry is no entry, and
        // one without an id is not counted.
        let page = parse_bss_page(&format!("{}id=3\nbssid=02:00:00:00:03:00\nfreq=2412\n", entry(2, 2412, -41, "[ESS]", "whole", false)));
        assert_eq!((page.seen.len(), page.last_id, page.end), (1, Some(2), false));
        let page = parse_bss_page("bssid=02:00:00:00:03:00\nfreq=2412\nlevel=-40\nflags=[ESS]\nssid=no id\n====\nnot a line\nFAIL\n");
        assert_eq!(page, BssPage::default());
        assert_eq!(security_words("[WPA2-PSK+SAE-CCMP][ESS]"), "WPA2/WPA3");
        assert_eq!(security_words("[WPA-PSK-TKIP][ESS]"), "WPA");
        assert_eq!(security_words("[WEP][ESS]"), "WEP (not supported)");
        assert!(joinable("[WPA2-PSK-CCMP][ESS]") && joinable("[ESS]") && !joinable("[WPA2-EAP-CCMP][ESS]") && !joinable("[WEP]"));
    }

    /// The kernel is built with the patches of `buildroot/patches/linux`.
    /// Each is one upstream commit and says which: a line of the pinned
    /// kernel that has taken a commit since drops its patch, and nobody can
    /// tell which that is from a patch that does not name it.
    #[test]
    fn every_kernel_patch_is_an_upstream_commit_that_names_itself() {
        let tree = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../buildroot");
        let mut names: Vec<String> = std::fs::read_dir(tree.join("patches/linux")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert!(!names.is_empty());
        let mut driver_file_added = false;
        for (n, name) in names.iter().enumerate() {
            // Buildroot applies `*.patch` in the order of their names.
            assert!(name.starts_with(&format!("{:04}-", n + 1)) && name.ends_with(".patch"), "{name}: the patches are numbered without a gap, and nothing else is in the directory");
            let text = std::fs::read_to_string(tree.join("patches/linux").join(name)).unwrap();
            let commit = text.strip_prefix("From ").and_then(|rest| rest.get(..40)).unwrap_or_default();
            assert!(commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()), "{name}: its first line names the commit it is");
            assert!(
                text.contains(&format!("\nUpstream: https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/commit/?id={commit}\n")),
                "{name}: an Upstream line with the commit of its first line"
            );
            assert!(text.contains("\nSigned-off-by: "), "{name}: the upstream sign-offs are kept");
            driver_file_added |= text.contains("\n+++ b/drivers/net/wireless/realtek/rtw89/rtw8852au.c\n");
        }
        // The driver these patches bring is asked for in the fragment, and
        // the image build reads every line of the fragment back: a kernel
        // built without the patches has no such symbol and stops the build.
        let fragment = std::fs::read_to_string(tree.join("board/embraos/linux.fragment")).unwrap();
        assert_eq!(driver_file_added, fragment.lines().any(|l| l == "CONFIG_RTW89_8852AU=m"), "the driver's patch and its line in the fragment come and go together");
    }

    #[test]
    fn the_status_and_the_signal_are_read_from_the_supplicants_answers() {
        let joined = parse_status("bssid=02:00:00:00:01:00\nfreq=2412\nssid=caf\\xc3\\xa9\nid=0\nmode=station\nwifi_generation=4\npairwise_cipher=CCMP\ngroup_cipher=CCMP\nkey_mgmt=WPA2-PSK\nwpa_state=COMPLETED\nip_address=192.168.1.42\naddress=02:00:00:00:00:00\n");
        assert!(joined.joined());
        assert_eq!((joined.ssid.as_deref(), joined.freq, joined.key_mgmt.as_deref()), (Some("café".as_bytes()), Some(2412), Some("WPA2-PSK")));
        let idle = parse_status("wpa_state=INACTIVE\naddress=02:00:00:00:00:00\n");
        assert_eq!((idle.joined(), idle.state.as_str(), idle.ssid), (false, "INACTIVE", None));
        assert_eq!(parse_status("FAIL\n"), Link::default());
        assert_eq!(parse_signal("RSSI=-47\nLINKSPEED=65\nNOISE=-92\nFREQUENCY=2412\n"), Some(-47));
        assert_eq!(parse_signal("FAIL\n"), None);
        assert_eq!(signal_words(-47), "strong, -47 dBm");
        assert_eq!(signal_words(-60), "good, -60 dBm");
        assert_eq!(signal_words(-75), "fair, -75 dBm");
        assert_eq!(signal_words(-85), "weak, -85 dBm");
    }

    #[test]
    fn a_refused_passphrase_is_read_from_the_supplicants_log_until_a_join_follows() {
        let log = "wlan0: SME: Trying to authenticate with 02:00:00:00:01:00 (SSID='Home Net' freq=2412 MHz)\n\
                   wlan0: WPA: 4-Way Handshake failed - pre-shared key may be incorrect\n\
                   wlan0: CTRL-EVENT-SSID-TEMP-DISABLED id=0 ssid=\"Home Net\" auth_failures=1 duration=10 reason=WRONG_KEY\n";
        assert_eq!(wrong_key_in_log(log).as_deref(), Some(b"Home Net".as_slice()));
        let later = format!("{log}wlan0: CTRL-EVENT-CONNECTED - Connection to 02:00:00:00:01:00 completed [id=0 id_str=]\n");
        assert_eq!(wrong_key_in_log(&later), None);
        // Another reason for the pause is no refused passphrase.
        assert_eq!(wrong_key_in_log("wlan0: CTRL-EVENT-SSID-TEMP-DISABLED id=0 ssid=\"x\" auth_failures=2 duration=20 reason=CONN_FAILED\n"), None);
        // A name with a quote in it, as the supplicant escapes it.
        assert_eq!(wrong_key_in_log("wlan0: CTRL-EVENT-SSID-TEMP-DISABLED id=1 ssid=\"a\\\"b\" auth_failures=1 duration=10 reason=WRONG_KEY\n").as_deref(), Some(b"a\"b".as_slice()));
        assert_eq!(wrong_key_in_log(""), None);
    }

    /// The build's side (`buildroot/board/embraos/wireless.sh`), run on a
    /// stand-in module tree: the list embrad loads from names the wireless
    /// drivers' device patterns and no other module's; a module that is no
    /// wireless driver and nothing one depends on is found; a driver's
    /// firmware is completed from a linux-firmware tree with the tree's
    /// links, and a driver none of whose names is served fails the table.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_build_lists_the_wireless_drivers_and_completes_their_firmware() {
        use std::process::Command;
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../buildroot/board/embraos/wireless.sh");
        let root = std::env::temp_dir().join(format!("embra-wireless-sh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (modules, tree, fw) = (root.join("modules"), root.join("linux-firmware"), root.join("rootfs").join("lib").join("firmware"));
        let module = |path: &str, modinfo: &str| {
            let file = modules.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, format!("\u{7f}ELF\0{}\0", modinfo.replace(' ', "\0"))).unwrap();
        };
        module("kernel/drivers/net/wireless/intel/iwlwifi/iwlwifi.ko", "license=GPL firmware=iwlwifi-so-a0-gf-a0-99.ucode firmware=iwlwifi-cc-a0-77.ucode firmware=iwlwifi-ty-a0-gf-a0-99.ucode");
        module("kernel/drivers/net/wireless/realtek/rtw88/rtw88_8821cu.ko", "firmware=rtw88/rtw8821c_fw.bin");
        module("kernel/drivers/net/wireless/mediatek/mt76/mt76x2-common.ko", "firmware=mt7662.bin");
        module("kernel/drivers/net/wireless/ath/ath12k/ath12k.ko", "license=Dual");
        module("kernel/net/wireless/cfg80211.ko", "firmware=regulatory.db");
        module("kernel/net/mac80211/mac80211.ko", "");
        module("kernel/lib/crypto/libarc4.ko", "");
        module("kernel/drivers/net/ethernet/intel/e1000/e1000.ko", "");
        std::fs::write(
            modules.join("modules.dep"),
            "kernel/drivers/net/wireless/intel/iwlwifi/iwlwifi.ko: kernel/net/wireless/cfg80211.ko\n\
             kernel/drivers/net/wireless/realtek/rtw88/rtw88_8821cu.ko: kernel/net/mac80211/mac80211.ko kernel/net/wireless/cfg80211.ko\n\
             kernel/drivers/net/wireless/mediatek/mt76/mt76x2-common.ko: kernel/net/mac80211/mac80211.ko\n\
             kernel/drivers/net/wireless/ath/ath12k/ath12k.ko: kernel/net/mac80211/mac80211.ko\n\
             kernel/net/wireless/cfg80211.ko:\n\
             kernel/net/mac80211/mac80211.ko: kernel/lib/crypto/libarc4.ko kernel/net/wireless/cfg80211.ko\n\
             kernel/lib/crypto/libarc4.ko:\n\
             kernel/drivers/net/ethernet/intel/e1000/e1000.ko:\n",
        )
        .unwrap();
        std::fs::write(
            modules.join("modules.alias"),
            "# Aliases extracted from modules themselves.\n\
             alias pci:v00008086d00002723sv*sd*bc*sc*i* iwlwifi\n\
             alias usb:v0BDApC811d*dc*dsc*dp*icFFiscFFipFFin* rtw88_8821cu\n\
             alias pci:v00008086d0000100Esv*sd*bc*sc*i* e1000\n\
             alias net-pf-16-proto-16-family-nl80211 cfg80211\n",
        )
        .unwrap();
        let run = |args: &[&std::ffi::OsStr]| {
            let out = Command::new("bash").arg(&script).args(args).output().expect("bash runs the script");
            (out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
        };
        let os = std::ffi::OsStr::new;

        // The list: the two drivers' patterns; not the wired card's, and not
        // the stack's own alias (cfg80211 is no driver of a device).
        let list = root.join("wireless.alias");
        assert_eq!(run(&[os("embraos_wireless_alias"), modules.as_os_str(), list.as_os_str()]).0, Some(0));
        assert_eq!(
            std::fs::read_to_string(&list).unwrap(),
            "alias pci:v00008086d00002723sv*sd*bc*sc*i* iwlwifi\nalias usb:v0BDApC811d*dc*dsc*dp*icFFiscFFipFFin* rtw88_8821cu\n"
        );
        // What embrad makes of it is what the build wrote.
        assert_eq!(modules_for(&std::fs::read_to_string(&list).unwrap(), "usb:v0BDApC811d0200dc00dsc00dp00icFFiscFFipFFin00", |p, m| p.split('*').next().is_some_and(|head| m.starts_with(head))), ["rtw88_8821cu"]);

        // The module that is neither wireless nor a dependency of one;
        // libarc4 is one, two steps from a driver.
        let (code, outside, _) = run(&[os("embraos_modules_outside_wireless"), modules.as_os_str()]);
        assert_eq!((code, outside.as_str()), (Some(0), "kernel/drivers/net/ethernet/intel/e1000/e1000.ko\n"));

        // The firmware. Buildroot installed nothing of it; the tree has the
        // iwlwifi directory with an older version than the newest the driver
        // names, a link to it at the top, the Realtek directory, and the one
        // MediaTek file a module names. A file nobody names stays out.
        std::fs::create_dir_all(tree.join("intel/iwlwifi")).unwrap();
        std::fs::create_dir_all(tree.join("rtw88")).unwrap();
        std::fs::create_dir_all(tree.join("mediatek")).unwrap();
        std::fs::write(tree.join("intel/iwlwifi/iwlwifi-so-a0-gf-a0-89.ucode"), "fw").unwrap();
        std::fs::write(tree.join("intel/iwlwifi/iwlwifi-cc-a0-77.ucode"), "fw").unwrap();
        std::fs::write(tree.join("rtw88/rtw8821c_fw.bin"), "fw").unwrap();
        std::fs::write(tree.join("mt7662.bin"), "fw").unwrap();
        std::fs::write(tree.join("mediatek/mt8173.bin"), "fw").unwrap();
        std::fs::write(tree.join("WHENCE"), "File: intel/iwlwifi/iwlwifi-cc-a0-77.ucode\nLink: iwlwifi-cc-a0-77.ucode -> intel/iwlwifi/iwlwifi-cc-a0-77.ucode\nLink: missing.bin -> nowhere/missing.bin\n").unwrap();
        // What Buildroot's own package had installed: an older version of
        // one device's file, at the top, where older trees keep them.
        std::fs::create_dir_all(&fw).unwrap();
        std::fs::write(fw.join("iwlwifi-ty-a0-gf-a0-59.ucode"), "fw").unwrap();
        let (code, table, _) = run(&[os("embraos_wireless_firmware_table"), fw.as_os_str(), modules.as_os_str()]);
        assert_eq!(code, Some(1), "before the completion: {table}");
        assert!(table.starts_with("iwlwifi 1/3\n") && table.ends_with("no firmware for: mt76x2-common rtw88_8821cu\n"), "{table}");
        let (code, _, err) = run(&[os("embraos_wireless_firmware"), tree.as_os_str(), fw.as_os_str(), modules.as_os_str()]);
        assert_eq!(code, Some(0), "{err}");
        assert!(fw.join("intel/iwlwifi/iwlwifi-so-a0-gf-a0-89.ucode").is_file() && fw.join("rtw88/rtw8821c_fw.bin").is_file() && fw.join("mt7662.bin").is_file());
        assert_eq!(std::fs::read_link(fw.join("iwlwifi-cc-a0-77.ucode")).unwrap(), Path::new("intel/iwlwifi/iwlwifi-cc-a0-77.ucode"), "the tree's link, for the name the driver asks at the top");
        assert!(!fw.join("missing.bin").exists() && !fw.join("mediatek").exists(), "no link without its target, no file nobody names");
        let (code, table, _) = run(&[os("embraos_wireless_firmware_table"), fw.as_os_str(), modules.as_os_str()]);
        assert_eq!(
            (code, table.as_str()),
            (Some(0), "iwlwifi 3/3\nmt76x2-common 1/1\nrtw88_8821cu 1/1\n"),
            "the newest name of a device is served by an older version of that device's file, in the directory (so) or at the top (ty); ath12k names nothing and has no line"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_device_asks_for_the_modules_whose_patterns_match_it() {
        let aliases = "# Aliases extracted from modules themselves.\n\
                       alias pci:v00008086d00002723sv*sd*bc*sc*i* iwlwifi\n\
                       alias usb:v0BDAp8179d*dc*dsc*dp*ic*isc*ip*in* rtl8xxxu\n\
                       alias usb:v0BDAp*d*dc*dsc*dp*icFFiscFFipFFin* rtw88_8821cu\n\
                       alias usb:v0BDAp8179d*dc*dsc*dp*ic*isc*ip*in* rtl8xxxu\n\
                       alias with too many words here\n\
                       softdep iwlwifi pre: cfg80211\n";
        // A stand-in for fnmatch that is enough for these patterns: split on `*`.
        let glob = |pattern: &str, text: &str| {
            let mut rest = text;
            let mut parts = pattern.split('*');
            let first = parts.next().unwrap_or("");
            if !rest.starts_with(first) {
                return false;
            }
            rest = &rest[first.len()..];
            let parts: Vec<&str> = parts.collect();
            for (i, part) in parts.iter().enumerate() {
                if part.is_empty() {
                    continue;
                }
                match rest.find(part) {
                    Some(at) if i + 1 < parts.len() || rest.ends_with(part) => rest = &rest[at + part.len()..],
                    _ => return false,
                }
            }
            true
        };
        assert_eq!(modules_for(aliases, "pci:v00008086d00002723sv00008086sd00000084bc02sc80i00", glob), ["iwlwifi"]);
        assert_eq!(modules_for(aliases, "usb:v0BDAp8179d0000dc00dsc00dp00icFFiscFFipFFin00", glob), ["rtl8xxxu", "rtw88_8821cu"], "each module once, in the list's order");
        assert!(modules_for(aliases, "pci:v00008086d0000100Esv*", glob).is_empty(), "a wired card matches nothing in a list of wireless drivers");
        assert!(modules_for("", "usb:v0BDAp8179", glob).is_empty());
    }
}
