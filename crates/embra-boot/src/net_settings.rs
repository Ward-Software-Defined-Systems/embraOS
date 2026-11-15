//! The operator's network settings: `/embra/state/network/settings`, one
//! `key=value` a line. The brain writes the file when the operator runs
//! `/network`; embrad reads it at boot and whenever it changes, and applies
//! it. Both use this parser, so what the brain accepts is what embrad
//! applies, and a line embrad does not accept is skipped and named, never a
//! reason to stop a boot.
//!
//! ```text
//! hostname=axiom
//! static=52:54:00:aa:00:01 192.168.1.50/24 gateway 192.168.1.1 dns 192.168.1.1 9.9.9.9
//! ```
//!
//! A card is stored by its hardware address: a USB card's name can change
//! from one boot to the next, and a setting restored onto another machine
//! must not apply itself to that machine's card.

use std::net::Ipv4Addr;
use std::path::Path;

use crate::net::hostname_ok;

/// The directory of the settings on STATE (the Wi-Fi networks join it).
pub const SETTINGS_DIR: &str = "/embra/state/network";
/// The settings file.
pub const SETTINGS_PATH: &str = "/embra/state/network/settings";

/// How many resolvers a card's setting names at most (musl reads three).
const RESOLVERS_MAX: usize = 3;

/// An address the operator gave a card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaticCard {
    /// The card's hardware address, lower case.
    pub mac: String,
    pub ip: Ipv4Addr,
    pub prefix: u8,
    pub gateway: Option<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
}

/// A hardware address as the kernel writes it, lower case; `None` when the
/// text is not six pairs of hex digits.
pub fn mac_ok(text: &str) -> Option<String> {
    let parts: Vec<&str> = text.split(':').collect();
    if parts.len() != 6 || !parts.iter().all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_hexdigit())) {
        return None;
    }
    Some(text.to_ascii_lowercase())
}

fn mask(prefix: u8) -> u32 {
    if prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(prefix.min(32))) }
}

impl StaticCard {
    /// A card's setting from its parts, each checked: the address with its
    /// prefix (`192.168.1.50/24`), the gateway and the resolvers. The error
    /// is a sentence for the operator.
    pub fn new(mac: &str, address: &str, gateway: Option<&str>, dns: &[&str]) -> Result<Self, String> {
        let mac = mac_ok(mac).ok_or_else(|| format!("{mac} is not a hardware address"))?;
        let (ip, prefix) = address.split_once('/').ok_or_else(|| format!("{address} has no prefix: write the address as 192.168.1.50/24"))?;
        let ip: Ipv4Addr = ip.parse().map_err(|_| format!("{ip} is not an IPv4 address"))?;
        let prefix: u8 = prefix.parse().ok().filter(|p| (1..=32).contains(p)).ok_or_else(|| format!("/{prefix} is not a prefix: 1 to 32"))?;
        if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() || ip.is_broadcast() {
            return Err(format!("{ip} is not an address a card can hold"));
        }
        let net = u32::from(ip) & mask(prefix);
        let last = net | !mask(prefix);
        if prefix <= 30 && (u32::from(ip) == net || u32::from(ip) == last) {
            return Err(format!("{ip} is the network's own or its broadcast address in a /{prefix}"));
        }
        let gateway = match gateway {
            None => None,
            Some(g) => {
                let gw: Ipv4Addr = g.parse().map_err(|_| format!("{g} is not an IPv4 address"))?;
                if gw == ip {
                    return Err(format!("the gateway {gw} is the card's own address"));
                }
                if u32::from(gw) & mask(prefix) != net {
                    return Err(format!("the gateway {gw} is not in {}/{prefix}", Ipv4Addr::from(net)));
                }
                Some(gw)
            }
        };
        if dns.len() > RESOLVERS_MAX {
            return Err(format!("{} resolvers: three at most", dns.len()));
        }
        let mut servers = Vec::with_capacity(dns.len());
        for d in dns {
            let server: Ipv4Addr = d.parse().map_err(|_| format!("{d} is not an IPv4 address"))?;
            if server.is_unspecified() {
                return Err(format!("{server} is not a resolver"));
            }
            servers.push(server);
        }
        Ok(StaticCard { mac, ip, prefix, gateway, dns: servers })
    }

    /// The value of a `static=` line.
    fn parse(value: &str) -> Result<Self, String> {
        let mut words = value.split_whitespace();
        let mac = words.next().ok_or("no hardware address")?;
        let address = words.next().ok_or("no address")?;
        let mut gateway = None;
        let mut dns = Vec::new();
        let mut in_dns = false;
        while let Some(word) = words.next() {
            match word {
                "gateway" if !in_dns => gateway = Some(words.next().ok_or("gateway without an address")?),
                "dns" if !in_dns => in_dns = true,
                other if in_dns => dns.push(other),
                other => return Err(format!("{other}: not a part of a card's setting")),
            }
        }
        StaticCard::new(mac, address, gateway, &dns)
    }

    fn to_value(&self) -> String {
        let mut value = format!("{} {}/{}", self.mac, self.ip, self.prefix);
        if let Some(gw) = self.gateway {
            value.push_str(&format!(" gateway {gw}"));
        }
        if !self.dns.is_empty() {
            value.push_str(" dns");
            for d in &self.dns {
                value.push_str(&format!(" {d}"));
            }
        }
        value
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// The machine's name, when the operator set one.
    pub hostname: Option<String>,
    /// The cards the operator gave an address, one entry a card.
    pub statics: Vec<StaticCard>,
}

impl Settings {
    /// The setting of the card with this hardware address.
    pub fn static_for(&self, mac: &str) -> Option<&StaticCard> {
        let mac = mac.to_ascii_lowercase();
        self.statics.iter().find(|s| s.mac == mac)
    }

    /// Gives a card its address, in place of an earlier one.
    pub fn set_static(&mut self, card: StaticCard) {
        self.statics.retain(|s| s.mac != card.mac);
        self.statics.push(card);
    }

    /// Takes a card's address away; whether it had one.
    pub fn remove_static(&mut self, mac: &str) -> bool {
        let mac = mac.to_ascii_lowercase();
        let before = self.statics.len();
        self.statics.retain(|s| s.mac != mac);
        self.statics.len() != before
    }

    /// The file's text.
    pub fn to_text(&self) -> String {
        let mut text = String::from("# embraOS network settings, written by /network. embrad applies a change within seconds.\n");
        if let Some(name) = &self.hostname {
            text.push_str(&format!("hostname={name}\n"));
        }
        for card in &self.statics {
            text.push_str(&format!("static={}\n", card.to_value()));
        }
        text
    }
}

/// Parses the file: the settings, and every line that was not accepted with
/// the reason. A second setting for one card replaces the first.
pub fn parse(text: &str) -> (Settings, Vec<String>) {
    let mut settings = Settings::default();
    let mut refused = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let shown: String = line.chars().take(120).collect();
        let Some((key, value)) = line.split_once('=') else {
            refused.push(format!("{shown}: not a setting"));
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "hostname" if hostname_ok(value) => settings.hostname = Some(value.to_string()),
            "hostname" => refused.push(format!("{shown}: not a hostname (letters, digits and hyphens, 63 at most)")),
            "static" => match StaticCard::parse(value) {
                Ok(card) => settings.set_static(card),
                Err(reason) => refused.push(format!("{shown}: {reason}")),
            },
            _ => refused.push(format!("{shown}: not a setting this release knows")),
        }
    }
    (settings, refused)
}

/// The settings on this machine; none when there is no file.
pub fn read(path: &Path) -> (Settings, Vec<String>) {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(_) => (Settings::default(), Vec::new()),
    }
}

/// Writes the settings whole: a temporary file beside the target, synced,
/// then renamed over it, so embrad never reads half a file.
pub fn write(path: &Path, settings: &Settings) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(settings.to_text().as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: &str = "52:54:00:aa:00:01";

    #[test]
    fn a_cards_address_is_checked_part_by_part() {
        let card = StaticCard::new("52:54:00:AA:00:01", "192.168.1.50/24", Some("192.168.1.1"), &["192.168.1.1", "9.9.9.9"]).unwrap();
        assert_eq!(card.mac, MAC, "stored lower case, as the kernel writes it");
        assert_eq!((card.ip, card.prefix, card.gateway), ("192.168.1.50".parse().unwrap(), 24, Some("192.168.1.1".parse().unwrap())));
        assert_eq!(card.dns.len(), 2);
        // No gateway and no resolver: an address on a network of its own.
        assert!(StaticCard::new(MAC, "10.0.0.9/8", None, &[]).is_ok());
        // A /31 and a /32 have no network or broadcast address to refuse.
        assert!(StaticCard::new(MAC, "10.0.0.0/31", None, &[]).is_ok());
        assert!(StaticCard::new(MAC, "10.0.0.7/32", None, &[]).is_ok());
        let refused = [
            (("52:54:00:aa:00", "192.168.1.50/24", None, vec![]), "not a hardware address"),
            ((MAC, "192.168.1.50", None, vec![]), "has no prefix"),
            ((MAC, "192.168.1.500/24", None, vec![]), "not an IPv4 address"),
            ((MAC, "192.168.1.50/0", None, vec![]), "not a prefix"),
            ((MAC, "192.168.1.50/33", None, vec![]), "not a prefix"),
            ((MAC, "127.0.0.1/8", None, vec![]), "not an address a card can hold"),
            ((MAC, "224.0.0.1/24", None, vec![]), "not an address a card can hold"),
            ((MAC, "192.168.1.0/24", None, vec![]), "the network's own or its broadcast"),
            ((MAC, "192.168.1.255/24", None, vec![]), "the network's own or its broadcast"),
            ((MAC, "192.168.1.50/24", Some("192.168.2.1"), vec![]), "is not in 192.168.1.0/24"),
            ((MAC, "192.168.1.50/24", Some("192.168.1.50"), vec![]), "the card's own address"),
            ((MAC, "192.168.1.50/24", Some("gateway"), vec![]), "not an IPv4 address"),
            ((MAC, "192.168.1.50/24", None, vec!["1.1.1.1", "8.8.8.8", "9.9.9.9", "4.4.4.4"]), "three at most"),
            ((MAC, "192.168.1.50/24", None, vec!["resolver.example"]), "not an IPv4 address"),
        ];
        for ((mac, address, gateway, dns), reason) in refused {
            let got = StaticCard::new(mac, address, gateway, &dns).unwrap_err();
            assert!(got.contains(reason), "{address} {gateway:?} {dns:?}: {got}");
        }
    }

    #[test]
    fn the_settings_read_back_and_a_second_setting_for_a_card_replaces_the_first() {
        let mut settings = Settings { hostname: Some("axiom".into()), statics: Vec::new() };
        settings.set_static(StaticCard::new(MAC, "192.168.1.50/24", Some("192.168.1.1"), &["192.168.1.1"]).unwrap());
        settings.set_static(StaticCard::new("02:00:00:00:01:00", "10.0.0.9/8", None, &[]).unwrap());
        let (back, refused) = parse(&settings.to_text());
        assert_eq!(back, settings);
        assert!(refused.is_empty());
        // The same card again: one entry, the new address.
        settings.set_static(StaticCard::new(MAC, "192.168.1.60/24", None, &[]).unwrap());
        assert_eq!(settings.statics.len(), 2);
        assert_eq!(settings.static_for("52:54:00:AA:00:01").unwrap().ip, "192.168.1.60".parse::<Ipv4Addr>().unwrap());
        assert!(settings.remove_static(MAC));
        assert!(!settings.remove_static(MAC), "it had none left");
        assert!(settings.static_for(MAC).is_none());
        // Two lines for one card in a file: the later one stands.
        let (twice, _) = parse(&format!("static={MAC} 10.0.0.9/8\nstatic={MAC} 10.0.0.10/8\n"));
        assert_eq!(twice.statics.len(), 1);
        assert_eq!(twice.statics[0].ip, "10.0.0.10".parse::<Ipv4Addr>().unwrap());
    }

    #[test]
    fn a_line_that_is_not_accepted_is_skipped_and_named_and_the_rest_stands() {
        let text = format!(
            "# a comment\n\nhostname=axiom\nhostname=not a name\nstatic={MAC} 192.168.1.50/24 gateway 10.9.9.9\nstatic={MAC} 192.168.1.50/24 dns 192.168.1.1\nstatic={MAC} 192.168.1.50/24 router 192.168.1.1\nfuture=setting\nno equals sign\n"
        );
        let (settings, refused) = parse(&text);
        assert_eq!(settings.hostname.as_deref(), Some("axiom"), "a bad line does not undo a good one");
        assert_eq!(settings.statics.len(), 1);
        assert_eq!(settings.statics[0].gateway, None);
        assert_eq!(refused.len(), 5);
        assert!(refused[0].contains("not a hostname"));
        assert!(refused[1].contains("is not in 192.168.1.0/24"));
        assert!(refused[2].contains("router: not a part of a card's setting"));
        assert!(refused[3].contains("not a setting this release knows"));
        assert!(refused[4].contains("not a setting"));
        // Every byte, a very long line: read, never a panic.
        let bytes: String = (0u8..=255).map(|b| b as char).collect();
        let _ = parse(&bytes);
        let _ = parse(&format!("static={}", "x ".repeat(100_000)));
        assert_eq!(parse("").0, Settings::default());
    }

    #[test]
    fn the_file_is_written_whole_and_a_missing_one_is_no_setting() {
        let dir = crate::net::tests::scratch("settings");
        let path = dir.join("network").join("settings");
        assert_eq!(read(&path).0, Settings::default());
        let mut settings = Settings::default();
        settings.set_static(StaticCard::new(MAC, "192.168.1.50/24", Some("192.168.1.1"), &["192.168.1.1"]).unwrap());
        write(&path, &settings).unwrap();
        assert_eq!(read(&path).0, settings);
        assert!(!path.with_extension("tmp").exists(), "the temporary file is gone");
        assert!(std::fs::read_to_string(&path).unwrap().starts_with("# embraOS network settings"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
