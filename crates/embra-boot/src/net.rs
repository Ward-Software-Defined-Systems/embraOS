//! The network cards of this machine, read from sysfs: every interface of
//! Ethernet type that a device stands behind, whatever the kernel named it
//! (`eth0`, `eth1`, `usb0`, `wlan0`) and in the order the kernel made them.
//! embrad brings each one up; the brain and embra-web read the same list for
//! what they show. No name is assumed anywhere: a machine with two ports
//! has its cable in either, a USB card is `usb0` when its address is a local
//! one (`usbnet.c`), and a card can appear after the boot.
//!
//! The root is a path so a test builds a tree; on a machine it is
//! [`SYS_CLASS_NET`].

use std::path::Path;

/// Where the kernel lists its network interfaces.
pub const SYS_CLASS_NET: &str = "/sys/class/net";

/// `ARPHRD_ETHER`: what `/sys/class/net/<if>/type` reads for an Ethernet
/// card and for a Wi-Fi card alike.
const ETHERNET_TYPE: &str = "1";

/// The longest name the kernel gives an interface (`IFNAMSIZ` less the NUL).
const NAME_MAX: usize = 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Wired,
    Wifi,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Wired => "wired",
            Kind::Wifi => "wifi",
        }
    }
}

/// One card: its name as the kernel gave it, the kernel's index (the order
/// the cards were made in), its hardware address, and whether it is a Wi-Fi
/// card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Card {
    pub name: String,
    pub index: u32,
    pub mac: String,
    pub kind: Kind,
}

/// A name this code will put into a path and hand to `udhcpc -i`: what the
/// kernel accepts for an interface, less anything that could leave the
/// directory. The kernel refuses `/`, whitespace, `.` and `..` itself; this
/// is the same rule stated here, so a strange directory entry is skipped and
/// never used.
pub fn card_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name != "."
        && name != ".."
        && name.bytes().all(|b| b.is_ascii_graphic() && b != b'/' && b != b':')
}

/// The cards under `root`, in the kernel's order. A card is an interface
/// whose `type` is Ethernet and that has a `device` behind it: that leaves
/// out the loopback, bridges, tunnels, bonds and dummies, which have none.
/// One with a `wireless` directory (or a `phy80211` link) is a Wi-Fi card.
pub fn cards(root: &Path) -> Vec<Card> {
    let Ok(entries) = std::fs::read_dir(root) else { return Vec::new() };
    let mut cards: Vec<Card> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !card_name_ok(&name) {
                return None;
            }
            let dir = root.join(&name);
            if read_trimmed(&dir.join("type")).as_deref() != Some(ETHERNET_TYPE) {
                return None;
            }
            if !dir.join("device").exists() {
                return None;
            }
            let index = read_trimmed(&dir.join("ifindex"))?.parse::<u32>().ok()?;
            let mac = read_trimmed(&dir.join("address")).unwrap_or_default();
            let kind = if dir.join("wireless").exists() || dir.join("phy80211").exists() { Kind::Wifi } else { Kind::Wired };
            Some(Card { name, index, mac, kind })
        })
        .collect();
    cards.sort_by(|a, b| a.index.cmp(&b.index).then_with(|| a.name.cmp(&b.name)));
    cards
}

/// The flag of an interface that is up (`IFF_UP`), as `flags` shows it.
const IFF_UP: u64 = 0x1;

/// Whether the card has a link: `Some(true)` with one, `Some(false)`
/// without, `None` when the card is not there to say.
///
/// Read from `operstate` and `flags`, which the kernel hands out without its
/// network lock. The `carrier` file waits for that lock, and the lock is
/// held for as long as a card's driver takes to start: a USB Wi-Fi adapter
/// has its whole chip written over the bus then, which is seconds on a
/// machine and half a minute through a hypervisor's USB. A reader of
/// `carrier` stands still for that long, whichever card it asks about; PID 1
/// reads here at every look.
///
/// `up` is a link that carries traffic (on a Wi-Fi card: joined, and its
/// keys set). `unknown` is what a driver that never reports a link leaves:
/// such a card has a link once it is up, and none while it is still being
/// brought up. Every other word (`down`, `dormant`, `lowerlayerdown`,
/// `notpresent`, `testing`) is no link.
pub fn carrier(root: &Path, name: &str) -> Option<bool> {
    if !card_name_ok(name) {
        return None;
    }
    let dir = root.join(name);
    match read_trimmed(&dir.join("operstate")).as_deref() {
        None => None,
        Some("up") => Some(true),
        Some("unknown") => {
            let flags = read_trimmed(&dir.join("flags")).and_then(|f| u64::from_str_radix(f.trim_start_matches("0x"), 16).ok());
            Some(flags.is_some_and(|f| f & IFF_UP != 0))
        }
        Some(_) => Some(false),
    }
}

/// The metric of a card's default route: a wired card's is lower than any
/// Wi-Fi card's, so the cable carries the traffic when both have an address,
/// and among cards of one kind the one the kernel made first comes first.
/// (100 and 600 are NetworkManager's figures for the two kinds.)
pub fn route_metric(card: &Card) -> u32 {
    let base = match card.kind {
        Kind::Wired => 100,
        Kind::Wifi => 600,
    };
    base + card.index.min(499)
}

/// A hostname this OS will take: one label of letters, digits and hyphens,
/// 63 at most, no hyphen at either end. `bad` is what BusyBox's DHCP client
/// writes in place of a name it rejected (`dhcpc.c`), so it is no name.
pub fn hostname_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name != "bad"
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// A domain a lease may name for the resolver's search list: labels as in
/// [`hostname_ok`], joined by dots, 253 at most.
pub fn domain_ok(domain: &str) -> bool {
    !domain.is_empty() && domain.len() <= 253 && domain.split('.').all(hostname_ok)
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A directory of this test's own under the system's temporary directory.
    pub(crate) fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("embra-boot-net-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// One interface as sysfs shows it.
    pub(crate) fn interface(root: &Path, name: &str, kind: &str, index: u32, device: bool, wireless: bool) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("type"), format!("{kind}\n")).unwrap();
        std::fs::write(dir.join("ifindex"), format!("{index}\n")).unwrap();
        std::fs::write(dir.join("address"), format!("52:54:00:00:00:{index:02x}\n")).unwrap();
        if device {
            std::fs::create_dir_all(dir.join("device")).unwrap();
        }
        if wireless {
            std::fs::create_dir_all(dir.join("wireless")).unwrap();
        }
    }

    #[test]
    fn a_card_is_an_ethernet_interface_with_a_device_behind_it() {
        let root = scratch("card");
        interface(&root, "eth0", "1", 2, true, false);
        interface(&root, "usb0", "1", 5, true, false);
        let found = cards(&root);
        assert_eq!(found.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["eth0", "usb0"]);
        assert_eq!(found[0], Card { name: "eth0".into(), index: 2, mac: "52:54:00:00:00:02".into(), kind: Kind::Wired });
        // No directory at all: no cards, and no error to handle.
        assert!(cards(&root.join("absent")).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn loopback_bridges_and_tunnels_are_not_cards() {
        let root = scratch("virtual");
        interface(&root, "lo", "772", 1, false, false); // ARPHRD_LOOPBACK
        interface(&root, "br0", "1", 3, false, false); // Ethernet type, no device
        interface(&root, "sit0", "776", 4, false, false); // a tunnel
        interface(&root, "eth0", "1", 2, true, false);
        // An entry whose name could leave the directory is never used.
        interface(&root, "a:b", "1", 9, true, false);
        assert_eq!(cards(&root).iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["eth0"]);
        assert!(!card_name_ok(".."));
        assert!(!card_name_ok("a/b"));
        assert!(!card_name_ok("sixteen-chars-xx"));
        assert!(card_name_ok("enx0011223344ff"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_card_with_a_wireless_directory_is_a_wifi_card() {
        let root = scratch("wifi");
        interface(&root, "wlan0", "1", 3, true, true);
        interface(&root, "eth0", "1", 2, true, false);
        let found = cards(&root);
        assert_eq!(found[1].kind, Kind::Wifi);
        assert_eq!(found[0].kind, Kind::Wired);
        // The cable carries the traffic when both have an address.
        assert!(route_metric(&found[0]) < route_metric(&found[1]));
        assert_eq!((route_metric(&found[0]), route_metric(&found[1])), (102, 603));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn cards_come_in_the_order_the_kernel_made_them() {
        let root = scratch("order");
        // Neither the directory's order nor the names' is the kernel's: the
        // index is. (A USB card that was there first is the first card.)
        interface(&root, "eth1", "1", 3, true, false);
        interface(&root, "eth0", "1", 5, true, false);
        interface(&root, "usb0", "1", 2, true, false);
        assert_eq!(cards(&root).iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["usb0", "eth1", "eth0"]);
        // A wired card that the kernel made late still comes before Wi-Fi.
        let late = Card { name: "eth9".into(), index: 4000, mac: String::new(), kind: Kind::Wired };
        let wifi = Card { name: "wlan0".into(), index: 1, mac: String::new(), kind: Kind::Wifi };
        assert!(route_metric(&late) < route_metric(&wifi));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_link_is_read_from_the_operstate_and_never_from_the_file_that_waits_for_the_lock() {
        let root = scratch("carrier");
        interface(&root, "eth0", "1", 2, true, false);
        assert_eq!(carrier(&root, "eth0"), None, "no operstate: the card is not there to say");
        // The file that waits for the kernel's network lock is never the
        // answer: it says a link here, and every state below says its own.
        std::fs::write(root.join("eth0/carrier"), "1\n").unwrap();
        assert_eq!(carrier(&root, "eth0"), None);
        for (state, link) in [("up", true), ("down", false), ("dormant", false), ("lowerlayerdown", false), ("notpresent", false), ("testing", false)] {
            std::fs::write(root.join("eth0/operstate"), format!("{state}\n")).unwrap();
            assert_eq!(carrier(&root, "eth0"), Some(link), "{state}");
        }
        // A driver that never reports: a link once the card is up, none
        // while it is being brought up (its flags lack the one for up).
        std::fs::write(root.join("eth0/operstate"), "unknown\n").unwrap();
        assert_eq!(carrier(&root, "eth0"), Some(false), "no flags to read: not up");
        std::fs::write(root.join("eth0/flags"), "0x1002\n").unwrap();
        assert_eq!(carrier(&root, "eth0"), Some(false), "being brought up");
        std::fs::write(root.join("eth0/flags"), "0x1003\n").unwrap();
        assert_eq!(carrier(&root, "eth0"), Some(true));
        std::fs::write(root.join("eth0/flags"), "9\n").unwrap();
        assert_eq!(carrier(&root, "eth0"), Some(true), "a hexadecimal number with or without its prefix");
        std::fs::write(root.join("eth0/flags"), "what\n").unwrap();
        assert_eq!(carrier(&root, "eth0"), Some(false));
        assert_eq!(carrier(&root, "../eth0"), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_hostname_of_bad_is_no_name() {
        assert!(hostname_ok("embraos"));
        assert!(hostname_ok("axiom-2"));
        assert!(!hostname_ok("bad"), "what BusyBox writes for a name it rejected");
        assert!(!hostname_ok(""));
        assert!(!hostname_ok("-lead"));
        assert!(!hostname_ok("trail-"));
        assert!(!hostname_ok("two.labels"));
        assert!(!hostname_ok("under_score"));
        assert!(!hostname_ok("line\nbreak"));
        assert!(!hostname_ok(&"a".repeat(64)));
        assert!(hostname_ok(&"a".repeat(63)));
        assert!(domain_ok("lan"));
        assert!(domain_ok("home.arpa"));
        assert!(!domain_ok("home..arpa"));
        assert!(!domain_ok("evil.example\nnameserver 6.6.6.6"));
    }
}
