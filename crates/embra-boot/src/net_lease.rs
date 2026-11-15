//! The addresses of this boot, a record a card: `/embra/ephemeral/net/<card>.lease`,
//! one `key=value` per line. The DHCP client's lease script writes one when
//! a card gets or renews a lease (`/usr/share/embra/udhcpc.script`,
//! `source=dhcp`) and removes it when the lease is lost; embrad writes one
//! for a card the operator gave an address (`source=static`). A card without
//! a record has no address: nothing stands in for a lease that did not come.
//!
//! The readers: embrad for the resolver file, the banner and the clock;
//! embra-web for the display console's menu; the brain for what it reports.
//! Every value in a record came from a DHCP server or a file, so each is
//! checked as it is read and a record that does not hold an address reads as
//! no lease.

use std::fmt;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use crate::net::{card_name_ok, domain_ok, hostname_ok};

/// Where the records, embrad's status and the clients' other files are.
pub const NET_DIR: &str = "/embra/ephemeral/net";
/// The resolver file embrad writes from the records; the root filesystem's
/// `/etc/resolv.conf` is a link to it.
pub const RESOLV_PATH: &str = "/embra/ephemeral/resolv.conf";
/// What embrad says about each card, beside the records.
pub const STATUS_FILE: &str = "status";

/// The address QEMU's user network leases to its guest. It is unreachable
/// from outside the VM, so no console URL is printed for it: the forwarded
/// `localhost` one is the way in.
pub const QEMU_USER_IP: &str = "10.0.2.15";

/// The metric a record without one sorts by: after every card embrad set up.
const UNKNOWN_METRIC: u32 = 10_000;
/// musl's resolver reads three servers; a lease may name more.
const RESOLVERS_MAX: usize = 3;
/// How many of a lease's time servers are kept.
const TIME_SERVERS_MAX: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Dhcp,
    Static,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Source::Dhcp => "dhcp",
            Source::Static => "static",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub interface: String,
    pub ip: String,
    pub prefix: String,
    pub router: Option<String>,
    pub dns: Vec<String>,
    /// The name the DHCP server assigns this machine (option 12).
    pub hostname: Option<String>,
    /// The domain of the network (option 15).
    pub domain: Option<String>,
    /// The time servers the network announces (option 42).
    pub ntp: Vec<String>,
    /// The metric of this card's default route: the lowest carries the traffic.
    pub metric: u32,
    pub source: Source,
}

impl Lease {
    /// The file's text: the keys the lease script writes, in its order.
    pub fn to_text(&self) -> String {
        format!(
            "interface={}\nip={}\nmask={}\nrouter={}\ndns={}\nhostname={}\ndomain={}\nntp={}\nmetric={}\nsource={}\n",
            self.interface,
            self.ip,
            self.prefix,
            self.router.as_deref().unwrap_or(""),
            self.dns.join(" "),
            self.hostname.as_deref().unwrap_or(""),
            self.domain.as_deref().unwrap_or(""),
            self.ntp.join(" "),
            self.metric,
            self.source
        )
    }

    /// One line for the log: the address, the router, the resolvers.
    pub fn summary(&self) -> String {
        format!(
            "{}/{} on {} via {}, dns {} ({})",
            self.ip,
            self.prefix,
            self.interface,
            self.router.as_deref().unwrap_or("-"),
            if self.dns.is_empty() { "-".to_string() } else { self.dns.join(" ") },
            self.source
        )
    }
}

/// The addresses among `text`'s words, in their order, `max` at most. A word
/// that is not an IPv4 address is left out: it came from a server.
fn addresses(text: &str, max: usize) -> Vec<String> {
    text.split_whitespace().filter_map(|w| w.parse::<Ipv4Addr>().ok()).map(|a| a.to_string()).take(max).collect()
}

/// Parses a record. `None` when it names no card, no address or no prefix:
/// the file is the script's, and a half-written or foreign one reads as no
/// lease. Every other value is kept only when it is what its key says.
pub fn parse(text: &str) -> Option<Lease> {
    let mut interface = None;
    let mut ip = None;
    let mut prefix = None;
    let mut lease = Lease {
        interface: String::new(),
        ip: String::new(),
        prefix: String::new(),
        router: None,
        dns: Vec::new(),
        hostname: None,
        domain: None,
        ntp: Vec::new(),
        metric: UNKNOWN_METRIC,
        source: Source::Dhcp,
    };
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        match k.trim() {
            "interface" => interface = card_name_ok(v).then(|| v.to_string()),
            "ip" => ip = v.parse::<Ipv4Addr>().ok().filter(|a| !a.is_unspecified()).map(|a| a.to_string()),
            "mask" => prefix = v.parse::<u8>().ok().filter(|p| (1..=32).contains(p)).map(|p| p.to_string()),
            "router" => lease.router = addresses(v, 1).into_iter().next(),
            "dns" => lease.dns = addresses(v, RESOLVERS_MAX),
            "hostname" => lease.hostname = hostname_ok(v).then(|| v.to_string()),
            "domain" => lease.domain = domain_ok(v).then(|| v.to_string()),
            "ntp" => lease.ntp = addresses(v, TIME_SERVERS_MAX),
            "metric" => lease.metric = v.parse::<u32>().unwrap_or(UNKNOWN_METRIC),
            "source" => lease.source = if v == "static" { Source::Static } else { Source::Dhcp },
            _ => {}
        }
    }
    lease.interface = interface?;
    lease.ip = ip?;
    lease.prefix = prefix?;
    Some(lease)
}

/// Where a card's record is.
pub fn lease_path(dir: &Path, card: &str) -> PathBuf {
    dir.join(format!("{card}.lease"))
}

/// Every record under `dir`, the card with the best route first. A record
/// that names another card than its file does is not read: the file's name
/// is the card embrad set up.
pub fn read_all(dir: &Path) -> Vec<Lease> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut leases: Vec<Lease> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let card = name.strip_suffix(".lease")?;
            let lease = parse(&std::fs::read_to_string(entry.path()).ok()?)?;
            (lease.interface == card).then_some(lease)
        })
        .collect();
    leases.sort_by(|a, b| a.metric.cmp(&b.metric).then_with(|| a.interface.cmp(&b.interface)));
    leases
}

/// The records of this boot.
pub fn read() -> Vec<Lease> {
    read_all(Path::new(NET_DIR))
}

/// The resolver file's text for these records (best route first): the
/// resolvers of the first card that has any, and its domain as the search
/// list. One card's alone: musl asks every listed server at once and takes
/// the first answer, a "no such name" included, so a second network's
/// resolver beside the first would answer for names only the first knows.
/// Empty when no card has a resolver.
pub fn resolv_text(leases: &[Lease]) -> String {
    let Some(best) = leases.iter().find(|l| !l.dns.is_empty()) else { return String::new() };
    let mut text = String::new();
    if let Some(domain) = &best.domain {
        text.push_str(&format!("search {domain}\n"));
    }
    for server in best.dns.iter().take(RESOLVERS_MAX) {
        text.push_str(&format!("nameserver {server}\n"));
    }
    text
}

/// A console URL and the address it is on: `None` for the one through
/// QEMU's port forward.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsoleUrl<'a> {
    pub url: String,
    pub lease: Option<&'a Lease>,
}

/// The console URLs the banner prints: the one through QEMU's port forward
/// always, then one for every address this machine holds on a network (a
/// lease on a bridged network or a real host, an address the operator set).
/// QEMU's user-network address gets no line: it is unreachable from outside
/// the VM.
pub fn console_urls<'a>(token: &str, leases: &'a [Lease]) -> Vec<ConsoleUrl<'a>> {
    let mut urls = vec![ConsoleUrl { url: format!("https://localhost:3345/embraOS?token={token}"), lease: None }];
    for lease in leases.iter().filter(|l| l.ip != QEMU_USER_IP) {
        urls.push(ConsoleUrl { url: format!("https://{}:3345/embraOS?token={token}", lease.ip), lease: Some(lease) });
    }
    urls
}

/// How a card is configured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Dhcp,
    Static,
}

/// What embrad says about one card, beside its record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardStatus {
    pub name: String,
    pub kind: crate::net::Kind,
    pub mac: String,
    /// `Some(true)` with a link, `Some(false)` without, `None` when the
    /// kernel does not say.
    pub link: Option<bool>,
    pub mode: Mode,
    /// What went wrong the last time embrad set this card up, if anything.
    pub note: Option<String>,
}

/// The status file's text: a block a card, a blank line between blocks.
pub fn status_text(cards: &[CardStatus]) -> String {
    let mut text = String::new();
    for card in cards {
        text.push_str(&format!(
            "card={}\nkind={}\nmac={}\nlink={}\nmode={}\nnote={}\n\n",
            card.name,
            card.kind.as_str(),
            card.mac,
            match card.link {
                Some(true) => "up",
                Some(false) => "down",
                None => "unknown",
            },
            match card.mode {
                Mode::Dhcp => "dhcp",
                Mode::Static => "static",
            },
            card.note.as_deref().unwrap_or("").replace('\n', " ")
        ));
    }
    text
}

/// Parses the status file. A block without a card's name is skipped.
pub fn parse_status(text: &str) -> Vec<CardStatus> {
    let mut cards = Vec::new();
    for block in text.split("\n\n") {
        let mut card = CardStatus { name: String::new(), kind: crate::net::Kind::Wired, mac: String::new(), link: None, mode: Mode::Dhcp, note: None };
        for line in block.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k {
                "card" if card_name_ok(v) => card.name = v.to_string(),
                "kind" => card.kind = if v == "wifi" { crate::net::Kind::Wifi } else { crate::net::Kind::Wired },
                "mac" => card.mac = v.to_string(),
                "link" => {
                    card.link = match v {
                        "up" => Some(true),
                        "down" => Some(false),
                        _ => None,
                    }
                }
                "mode" => card.mode = if v == "static" { Mode::Static } else { Mode::Dhcp },
                "note" => card.note = (!v.is_empty()).then(|| v.to_string()),
                _ => {}
            }
        }
        if !card.name.is_empty() {
            cards.push(card);
        }
    }
    cards
}

/// embrad's status of this boot.
pub fn read_status() -> Vec<CardStatus> {
    std::fs::read_to_string(Path::new(NET_DIR).join(STATUS_FILE)).map(|t| parse_status(&t)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::Kind;

    const LEASED: &str = "interface=eth1\nip=192.168.1.42\nmask=24\nrouter=192.168.1.1 192.168.1.2\ndns=192.168.1.1 9.9.9.9\nhostname=axiom\ndomain=lan\nntp=192.168.1.1 192.168.1.5\nmetric=103\nsource=dhcp\n";

    #[test]
    fn a_record_carries_the_hostname_the_domain_and_the_time_servers() {
        let lease = parse(LEASED).unwrap();
        assert_eq!(lease.interface, "eth1");
        assert_eq!(lease.ip, "192.168.1.42");
        assert_eq!(lease.prefix, "24");
        assert_eq!(lease.router.as_deref(), Some("192.168.1.1"));
        assert_eq!(lease.dns, vec!["192.168.1.1", "9.9.9.9"]);
        assert_eq!(lease.hostname.as_deref(), Some("axiom"));
        assert_eq!(lease.domain.as_deref(), Some("lan"));
        assert_eq!(lease.ntp, vec!["192.168.1.1", "192.168.1.5"]);
        assert_eq!(lease.metric, 103);
        assert_eq!(lease.source, Source::Dhcp);
        assert_eq!(lease.summary(), "192.168.1.42/24 on eth1 via 192.168.1.1, dns 192.168.1.1 9.9.9.9 (dhcp)");
        // The record round-trips through its text.
        assert_eq!(parse(&lease.to_text()).unwrap(), lease);
        // A lease without a router, a resolver or a name is still a lease.
        let bare = parse("interface=eth0\nip=10.1.1.5\nmask=16\nrouter=\ndns=\nhostname=\nsource=dhcp\n").unwrap();
        assert_eq!((bare.router, bare.hostname, bare.domain), (None, None, None));
        assert!(bare.dns.is_empty() && bare.ntp.is_empty());
        // An address the operator set says so.
        assert_eq!(parse("interface=eth0\nip=10.1.1.5\nmask=16\nsource=static\n").unwrap().source, Source::Static);
    }

    #[test]
    fn a_missing_record_is_no_lease_and_no_static_address() {
        let dir = crate::net::tests::scratch("no-record");
        assert!(read_all(&dir).is_empty(), "nothing stands in for a lease that did not come");
        assert!(read_all(&dir.join("absent")).is_empty());
        // Nothing, a half-written file, a foreign file: no lease.
        assert_eq!(parse(""), None);
        assert_eq!(parse("interface=eth0\nsource=dhcp\n"), None);
        assert_eq!(parse("ip=192.168.1.42\nmask=24\n"), None, "a record names its card");
        assert_eq!(parse("interface=eth0\nip=192.168.1.42\n"), None, "and its prefix");
        assert_eq!(parse("hello"), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_hostile_record_reads_as_no_lease_and_never_panics() {
        // Every value came from a DHCP server through a shell script.
        let table = [
            "interface=../../etc\nip=192.168.1.42\nmask=24\n",
            "interface=eth0\nip=not-an-address\nmask=24\n",
            "interface=eth0\nip=0.0.0.0\nmask=24\n",
            "interface=eth0\nip=192.168.1.42\nmask=0\n",
            "interface=eth0\nip=192.168.1.42\nmask=33\n",
            "interface=eth0\nip=192.168.1.42\nmask=-1\n",
            "interface=eth0\nip=192.168.1.42/24\nmask=24\n",
            "=\n=\n===\n",
            "\u{0}\u{1}\u{2}",
        ];
        for text in table {
            assert_eq!(parse(text), None, "{text:?}");
        }
        // What is not what its key says is left out, and the lease stands.
        let odd = parse("interface=eth0\nip=192.168.1.42\nmask=24\nrouter=evil 192.168.1.1\ndns=6.6.6.6;rm 192.168.1.1 $(x)\nhostname=bad\ndomain=lan\\nnameserver 6.6.6.6\nntp=x\nmetric=many\n").unwrap();
        assert_eq!(odd.router.as_deref(), Some("192.168.1.1"));
        assert_eq!(odd.dns, vec!["192.168.1.1"]);
        assert_eq!(odd.hostname, None, "BusyBox's word for a name it rejected");
        assert_eq!(odd.domain, None);
        assert!(odd.ntp.is_empty());
        assert_eq!(odd.metric, UNKNOWN_METRIC);
        // A very long line, a line of every byte: read, never a panic.
        let _ = parse(&format!("interface=eth0\nip=192.168.1.42\nmask=24\ndns={}\n", "1.1.1.1 ".repeat(5000)));
        let bytes: String = (0u8..=255).map(|b| b as char).collect();
        let _ = parse(&bytes);
        let _ = parse_status(&bytes);
    }

    #[test]
    fn records_are_read_best_route_first_and_a_foreign_file_is_not_a_record() {
        let dir = crate::net::tests::scratch("records");
        std::fs::write(lease_path(&dir, "wlan0"), "interface=wlan0\nip=192.168.7.9\nmask=24\ndns=192.168.7.1\nmetric=603\nsource=dhcp\n").unwrap();
        std::fs::write(lease_path(&dir, "eth1"), LEASED).unwrap();
        // A record that names another card than its file is not read.
        std::fs::write(lease_path(&dir, "eth0"), LEASED).unwrap();
        std::fs::write(dir.join("status"), "card=eth1\n").unwrap();
        std::fs::write(dir.join("eth9.lease.tmp"), LEASED).unwrap();
        let leases = read_all(&dir);
        assert_eq!(leases.iter().map(|l| l.interface.as_str()).collect::<Vec<_>>(), ["eth1", "wlan0"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_resolver_file_holds_the_best_cards_resolvers_alone() {
        let wired = parse(LEASED).unwrap();
        let wifi = parse("interface=wlan0\nip=192.168.7.9\nmask=24\ndns=192.168.7.1\nmetric=603\nsource=dhcp\n").unwrap();
        assert_eq!(resolv_text(&[wired.clone(), wifi.clone()]), "search lan\nnameserver 192.168.1.1\nnameserver 9.9.9.9\n");
        // The best route's card has no resolver: the next card's are the file's.
        let mut silent = wired.clone();
        silent.dns.clear();
        assert_eq!(resolv_text(&[silent, wifi.clone()]), "nameserver 192.168.7.1\n");
        assert_eq!(resolv_text(&[]), "");
        // Three at most, whatever the lease named.
        let many = parse("interface=eth0\nip=10.0.0.9\nmask=8\ndns=10.0.0.1 10.0.0.2 10.0.0.3 10.0.0.4 10.0.0.5\n").unwrap();
        assert_eq!(resolv_text(&[many]).lines().count(), 3);
    }

    #[test]
    fn every_leased_address_gets_a_console_url_and_qemus_gets_none() {
        let token = "0123456789abcdef0123456789abcdef";
        let wired = parse(LEASED).unwrap();
        let fixed = parse("interface=eth0\nip=10.20.0.5\nmask=24\nmetric=102\nsource=static\n").unwrap();
        let leases = [fixed.clone(), wired.clone()];
        let urls = console_urls(token, &leases);
        assert_eq!(
            urls.iter().map(|u| u.url.as_str()).collect::<Vec<_>>(),
            [
                "https://localhost:3345/embraOS?token=0123456789abcdef0123456789abcdef",
                "https://10.20.0.5:3345/embraOS?token=0123456789abcdef0123456789abcdef",
                "https://192.168.1.42:3345/embraOS?token=0123456789abcdef0123456789abcdef",
            ]
        );
        assert_eq!(urls[0].lease, None);
        assert_eq!(urls[2].lease, Some(&wired));
        // QEMU's own DHCP hands out its user-network address: one URL.
        let qemu = [parse("interface=eth0\nip=10.0.2.15\nmask=24\nrouter=10.0.2.2\ndns=10.0.2.3\nmetric=102\nsource=dhcp\n").unwrap()];
        assert_eq!(console_urls(token, &qemu).len(), 1);
        // No address at all: the forwarded one, as before.
        assert_eq!(console_urls(token, &[]).len(), 1);
    }

    #[test]
    fn the_status_file_reads_back_card_by_card() {
        let cards = vec![
            CardStatus { name: "eth0".into(), kind: Kind::Wired, mac: "52:54:00:aa:00:01".into(), link: Some(false), mode: Mode::Dhcp, note: None },
            CardStatus { name: "wlan0".into(), kind: Kind::Wifi, mac: "02:00:00:00:01:00".into(), link: None, mode: Mode::Static, note: Some("10.0.0.9 is in use".into()) },
        ];
        assert_eq!(parse_status(&status_text(&cards)), cards);
        assert!(parse_status("").is_empty());
        assert!(parse_status("kind=wired\nlink=up\n").is_empty(), "a block without its card is skipped");
        // A note cannot open a block of its own.
        let tricky = vec![CardStatus { name: "eth0".into(), kind: Kind::Wired, mac: String::new(), link: Some(true), mode: Mode::Dhcp, note: Some("a\n\ncard=eth9".into()) }];
        assert_eq!(parse_status(&status_text(&tricky)).len(), 1);
    }
}
