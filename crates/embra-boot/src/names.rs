//! The names this machine answers to, for the web console's certificate.
//!
//! One function decides what the certificate names ([`leaf_names`]), and
//! three processes call it: embra-trustd, which mints the certificate and
//! holds the certificate authority; embra-web, which asks for a new one
//! when the answer changes; and the brain, which shows the names in
//! `/network`. A name outside what the authority may sign makes a browser
//! reject the whole certificate, `localhost` included, so the list is cut
//! to the authority's limits here and nowhere else.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

/// The machine's name when nobody gave it one.
pub const DEFAULT_HOSTNAME: &str = "embraos";

/// Where the trust service keeps the authority's certificate and key.
pub const PKI_DIR: &str = "/embra/state/pki";
/// The file beside the authority's certificate that says what it may sign.
pub const CA_NAMES_FILE: &str = "ca.names";

/// The IPv4 ranges a certificate of this machine may name and its authority
/// may sign: loopback, the private ranges and the link-local range. A public
/// address is never among them: the authority's key could sign for it.
pub const PRIVATE_V4: &[(Ipv4Addr, u8)] = &[
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
];

/// The IPv6 ranges, the same way: loopback, the unique local addresses and
/// the link-local range.
pub const PRIVATE_V6: &[(Ipv6Addr, u8)] = &[
    (Ipv6Addr::LOCALHOST, 128),
    (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
    (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
];

/// Whether an address lies in [`PRIVATE_V4`] or [`PRIVATE_V6`].
pub fn is_private(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(a) => PRIVATE_V4.iter().any(|(net, bits)| {
            let mask = u32::MAX.checked_shl(32 - u32::from(*bits)).unwrap_or(0);
            u32::from(a) & mask == u32::from(*net) & mask
        }),
        IpAddr::V6(a) => PRIVATE_V6.iter().any(|(net, bits)| {
            let mask = u128::MAX.checked_shl(128 - u32::from(*bits)).unwrap_or(0);
            u128::from(a) & mask == u128::from(*net) & mask
        }),
    }
}

/// An IPv6 link-local address: every card has one, and no browser can be
/// pointed at it (a URL carries no interface).
fn link_local_v6(address: &IpAddr) -> bool {
    matches!(address, IpAddr::V6(a) if a.segments()[0] & 0xffc0 == 0xfe80)
}

/// What the certificate authority of this machine may sign, as the file
/// beside its certificate records it ([`CA_NAMES_FILE`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaSet {
    /// An authority from before the name constraints: it signs any name.
    Unconstrained,
    /// `localhost`, the default name, the operator's name when the
    /// certificate was made with one, and the addresses of [`PRIVATE_V4`]
    /// and [`PRIVATE_V6`].
    Private { hostname: Option<String> },
}

/// The word for [`CaSet::Private`] in the file. A later set with other
/// limits takes another word, and an image that does not know a word reads
/// the file as absent.
const PRIVATE_SET: &str = "private.v1";

impl CaSet {
    pub fn to_text(&self) -> String {
        match self {
            CaSet::Unconstrained => "set=none\n".to_string(),
            CaSet::Private { hostname: None } => format!("set={PRIVATE_SET}\n"),
            CaSet::Private { hostname: Some(name) } => format!("set={PRIVATE_SET}\nhostname={name}\n"),
        }
    }

    /// `None` for a text that names no set this image knows.
    pub fn parse(text: &str) -> Option<CaSet> {
        let field = |key: &str| text.lines().filter_map(|l| l.split_once('=')).filter(|(k, _)| k.trim() == key).map(|(_, v)| v.trim()).next_back();
        match field("set")? {
            "none" => Some(CaSet::Unconstrained),
            PRIVATE_SET => Some(CaSet::Private { hostname: field("hostname").filter(|n| crate::net::hostname_ok(n)).map(str::to_string) }),
            _ => None,
        }
    }

    /// The set recorded in `dir`, `None` without a readable record.
    pub fn read(dir: &Path) -> Option<CaSet> {
        CaSet::parse(&std::fs::read_to_string(dir.join(CA_NAMES_FILE)).ok()?)
    }

    /// The DNS names the authority's certificate permits; empty for an
    /// authority without limits.
    pub fn permitted_names(&self) -> Vec<String> {
        match self {
            CaSet::Unconstrained => Vec::new(),
            CaSet::Private { hostname } => {
                let mut names = vec!["localhost".to_string(), DEFAULT_HOSTNAME.to_string()];
                if let Some(name) = hostname.as_ref().filter(|n| !names.iter().any(|have| have.eq_ignore_ascii_case(n))) {
                    names.push(name.clone());
                }
                names
            }
        }
    }

    /// Whether a certificate under this authority may carry `name`.
    pub fn admits_name(&self, name: &str) -> bool {
        match self {
            CaSet::Unconstrained => true,
            CaSet::Private { .. } => self.permitted_names().iter().any(|have| have.eq_ignore_ascii_case(name)),
        }
    }
}

/// The names a certificate of this machine carries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LeafNames {
    pub dns: Vec<String>,
    pub ips: Vec<IpAddr>,
}

impl LeafNames {
    /// Every name, the way an operator types it into a browser.
    pub fn listed(&self) -> Vec<String> {
        self.dns.iter().cloned().chain(self.ips.iter().map(|ip| ip.to_string())).collect()
    }
}

/// What the certificate names: `localhost`, the machine's name when it is a
/// plain host name the authority admits, the two loopback addresses, and
/// every private address a card holds. Left out: a public address (the
/// authority does not sign for one), an IPv6 link-local address, and a name
/// with a domain (`host.lan`: a permitted `host` does not cover it, and one
/// name the authority refuses fails the whole certificate).
pub fn leaf_names(set: &CaSet, hostname: Option<&str>, addresses: &[IpAddr]) -> LeafNames {
    let mut dns = vec!["localhost".to_string()];
    if let Some(name) = hostname.filter(|n| crate::net::hostname_ok(n) && set.admits_name(n) && !n.eq_ignore_ascii_case("localhost")) {
        dns.push(name.to_string());
    }
    let mut held: Vec<IpAddr> = addresses.iter().copied().filter(|a| is_private(*a) && !a.is_loopback() && !link_local_v6(a)).collect();
    held.sort();
    held.dedup();
    let mut ips = vec![IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)];
    ips.extend(held);
    LeafNames { dns, ips }
}

/// The kernel's hostname; `None` when it has none (`(none)` is the kernel's
/// own word for that).
#[cfg(target_os = "linux")]
pub fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `len` bytes into the buffer it is
    // handed, and the buffer lives across the call.
    let failed = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0;
    if failed {
        return None;
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    let name = std::str::from_utf8(buf.get(..end)?).ok()?;
    (!name.is_empty() && name != "(none)").then(|| name.to_string())
}

#[cfg(not(target_os = "linux"))]
pub fn hostname() -> Option<String> {
    None
}

/// Every address an interface of this machine holds, as the kernel lists
/// them. Empty when the list cannot be had.
///
/// This can stand still for seconds: the list of interfaces is read under
/// the kernel's network lock (`RTM_GETLINK`'s dump, kernel 6.18), which is
/// held for as long as a card's driver takes to start, and a USB Wi-Fi
/// adapter takes seconds. A caller on an async runtime asks on the
/// blocking pool.
#[cfg(target_os = "linux")]
pub fn addresses() -> Vec<IpAddr> {
    let mut out = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs stores a list it allocates in `list`; it is walked
    // and freed below, once.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return out;
    }
    let mut cursor = list;
    while !cursor.is_null() {
        // SAFETY: a non-null node of the list getifaddrs returned.
        let entry = unsafe { &*cursor };
        if !entry.ifa_addr.is_null() {
            // SAFETY: ifa_addr points at a socket address whose family
            // names its real type, which is read as that type alone.
            match i32::from(unsafe { (*entry.ifa_addr).sa_family }) {
                libc::AF_INET => {
                    let sin = unsafe { std::ptr::read_unaligned(entry.ifa_addr.cast::<libc::sockaddr_in>()) };
                    out.push(IpAddr::V4(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr))));
                }
                libc::AF_INET6 => {
                    let sin6 = unsafe { std::ptr::read_unaligned(entry.ifa_addr.cast::<libc::sockaddr_in6>()) };
                    out.push(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)));
                }
                _ => {}
            }
        }
        cursor = entry.ifa_next;
    }
    // SAFETY: the list getifaddrs allocated, freed once and not used after.
    unsafe { libc::freeifaddrs(list) };
    out
}

#[cfg(not(target_os = "linux"))]
pub fn addresses() -> Vec<IpAddr> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn the_private_ranges_are_loopback_the_rfc1918_blocks_and_the_local_ones() {
        for inside in ["127.0.0.1", "127.255.255.254", "10.0.2.15", "10.255.0.1", "172.16.0.1", "172.31.255.254", "192.168.1.42", "169.254.10.1", "::1", "fd12:3456:789a::1", "fc00::1", "fe80::5054:ff:fe12:3456"] {
            assert!(is_private(ip(inside)), "{inside}");
        }
        for outside in ["8.8.8.8", "172.15.255.255", "172.32.0.1", "192.167.1.1", "192.169.1.1", "11.0.0.1", "169.253.1.1", "100.64.0.1", "2001:db8::1", "2606:4700::1111", "::", "::ffff:10.0.2.15", "fec0::1"] {
            assert!(!is_private(ip(outside)), "{outside}");
        }
    }

    #[test]
    fn a_certificate_names_localhost_the_machines_name_and_its_private_addresses() {
        let set = CaSet::Private { hostname: Some("axiom".into()) };
        let names = leaf_names(&set, Some("axiom"), &[ip("127.0.0.1"), ip("192.168.1.42"), ip("10.0.2.15"), ip("fe80::1"), ip("::1"), ip("192.168.1.42")]);
        assert_eq!(names.dns, ["localhost", "axiom"]);
        assert_eq!(names.ips, [ip("127.0.0.1"), ip("::1"), ip("10.0.2.15"), ip("192.168.1.42")], "loopback first, then each held address once; no link-local IPv6");
        assert_eq!(names.listed(), ["localhost", "axiom", "127.0.0.1", "::1", "10.0.2.15", "192.168.1.42"]);
        // The default name is admitted by every set.
        assert_eq!(leaf_names(&CaSet::Private { hostname: None }, Some("embraos"), &[]).dns, ["localhost", "embraos"]);
        // No card, no name: the machine still answers on its loopback.
        assert_eq!(leaf_names(&set, None, &[]), LeafNames { dns: vec!["localhost".into()], ips: vec![ip("127.0.0.1"), ip("::1")] });
    }

    #[test]
    fn a_public_address_and_a_name_with_a_domain_are_left_out() {
        let set = CaSet::Private { hostname: Some("axiom".into()) };
        let names = leaf_names(&set, Some("axiom.lan"), &[ip("203.0.113.7"), ip("2001:db8::7"), ip("192.168.1.42")]);
        assert_eq!(names.dns, ["localhost"], "a name with a domain is no plain host name");
        assert_eq!(names.ips, [ip("127.0.0.1"), ip("::1"), ip("192.168.1.42")]);
        // An authority without limits changes nothing about either rule.
        let names = leaf_names(&CaSet::Unconstrained, Some("axiom.lan"), &[ip("203.0.113.7")]);
        assert_eq!((names.dns.len(), names.ips.len()), (1, 2));
    }

    #[test]
    fn a_name_the_authority_does_not_admit_is_left_out() {
        // A name a DHCP server assigned: the machine carries it, the
        // authority was not made with it.
        let made_without = CaSet::Private { hostname: None };
        assert_eq!(leaf_names(&made_without, Some("node-7"), &[]).dns, ["localhost"]);
        let made_for_another = CaSet::Private { hostname: Some("axiom".into()) };
        assert_eq!(leaf_names(&made_for_another, Some("node-7"), &[]).dns, ["localhost"]);
        assert_eq!(leaf_names(&made_for_another, Some("AXIOM"), &[]).dns, ["localhost", "AXIOM"], "names compare without case");
        // An authority from before the constraints admits any plain name.
        assert_eq!(leaf_names(&CaSet::Unconstrained, Some("node-7"), &[]).dns, ["localhost", "node-7"]);
        // `localhost` is named once.
        assert_eq!(leaf_names(&CaSet::Unconstrained, Some("localhost"), &[]).dns, ["localhost"]);
    }

    #[test]
    fn the_record_of_what_the_authority_signs_reads_back() {
        for set in [CaSet::Unconstrained, CaSet::Private { hostname: None }, CaSet::Private { hostname: Some("axiom".into()) }] {
            assert_eq!(CaSet::parse(&set.to_text()), Some(set.clone()), "{set:?}");
        }
        assert_eq!(CaSet::Private { hostname: Some("axiom".into()) }.to_text(), "set=private.v1\nhostname=axiom\n");
        // A set this image does not know, an empty file and a foreign one say nothing.
        for text in ["", "set=private.v9\n", "hostname=axiom\n", "not a record"] {
            assert_eq!(CaSet::parse(text), None, "{text:?}");
        }
        // A name that is no host name is not taken from the file.
        assert_eq!(CaSet::parse("set=private.v1\nhostname=bad name\n"), Some(CaSet::Private { hostname: None }));
        assert_eq!(CaSet::Private { hostname: Some("axiom".into()) }.permitted_names(), ["localhost", "embraos", "axiom"]);
        assert_eq!(CaSet::Private { hostname: Some("EmbraOS".into()) }.permitted_names(), ["localhost", "embraos"]);
        assert!(CaSet::Unconstrained.permitted_names().is_empty());
        let dir = std::env::temp_dir().join(format!("embra-ca-names-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(CaSet::read(&dir), None);
        std::fs::write(dir.join(CA_NAMES_FILE), "set=none\n").unwrap();
        assert_eq!(CaSet::read(&dir), Some(CaSet::Unconstrained));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn this_machine_has_a_loopback_address_and_a_name_or_none() {
        // The kernel's own list: whatever the host has, the loopback is in it.
        assert!(addresses().contains(&ip("127.0.0.1")), "{:?}", addresses());
        if let Some(name) = hostname() {
            assert!(!name.is_empty() && name != "(none)");
        }
    }
}
