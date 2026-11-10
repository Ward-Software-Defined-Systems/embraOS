//! The network lease as embrad records it: `/embra/ephemeral/net-lease`,
//! one `key=value` per line, written by the udhcpc lease script
//! (`/usr/share/embra/udhcpc.script`, `source=dhcp`) or by embrad itself when
//! no lease arrived and the static QEMU user-network address was applied
//! (`source=static`). Two readers take the console's URLs from it
//! (`console_urls`): embrad for the boot banner, and embra-web for the
//! display console's menu, which shows them again. A missing or unreadable
//! file reads as the static configuration, which is what every boot before
//! DHCP ran.

use std::fmt;

/// Where the lease is recorded.
pub const LEASE_PATH: &str = "/embra/ephemeral/net-lease";
/// The resolver file the lease script and the static fallback write; the
/// root filesystem's `/etc/resolv.conf` is a link to it.
pub const RESOLV_PATH: &str = "/embra/ephemeral/resolv.conf";

/// The static QEMU user-network configuration: the address every boot ran
/// on before DHCP, the SLIRP gateway, its resolver.
pub const STATIC_IP: &str = "10.0.2.15";
pub const STATIC_PREFIX: &str = "24";
pub const STATIC_ROUTER: &str = "10.0.2.2";
pub const STATIC_DNS: &str = "10.0.2.3";

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
    pub source: Source,
}

impl Lease {
    /// The static configuration as a lease record.
    pub fn static_fallback() -> Self {
        Lease {
            interface: "eth0".into(),
            ip: STATIC_IP.into(),
            prefix: STATIC_PREFIX.into(),
            router: Some(STATIC_ROUTER.into()),
            dns: vec![STATIC_DNS.into()],
            source: Source::Static,
        }
    }

    /// The file's text: the keys the lease script writes, in its order.
    pub fn to_text(&self) -> String {
        format!(
            "interface={}\nip={}\nmask={}\nrouter={}\ndns={}\nsource={}\n",
            self.interface,
            self.ip,
            self.prefix,
            self.router.as_deref().unwrap_or(""),
            self.dns.join(" "),
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

/// Parses the lease file. `None` when there is no address in it: the file is
/// the script's and a half-written or foreign one reads as no lease.
pub fn parse(text: &str) -> Option<Lease> {
    let mut lease = Lease { interface: String::new(), ip: String::new(), prefix: String::new(), router: None, dns: Vec::new(), source: Source::Static };
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        match k.trim() {
            "interface" => lease.interface = v.to_string(),
            "ip" => lease.ip = v.to_string(),
            "mask" => lease.prefix = v.to_string(),
            "router" => lease.router = (!v.is_empty()).then(|| v.split_whitespace().next().unwrap_or(v).to_string()),
            "dns" => lease.dns = v.split_whitespace().map(str::to_string).collect(),
            "source" => lease.source = if v == "dhcp" { Source::Dhcp } else { Source::Static },
            _ => {}
        }
    }
    if lease.ip.is_empty() {
        return None;
    }
    if lease.interface.is_empty() {
        lease.interface = "eth0".into();
    }
    if lease.prefix.is_empty() {
        lease.prefix = STATIC_PREFIX.into();
    }
    Some(lease)
}

/// The lease of this boot; a missing or unreadable file is the static
/// configuration.
pub fn read() -> Lease {
    std::fs::read_to_string(LEASE_PATH).ok().and_then(|t| parse(&t)).unwrap_or_else(Lease::static_fallback)
}

/// The console URLs the banner prints: the one through QEMU's port forward
/// always, and the one on the leased address when a DHCP lease gave this
/// machine an address of its own (a bridged network, a real host). The
/// static address is QEMU's user network, unreachable from outside the VM,
/// and gets no line.
pub fn console_urls(token: &str, lease: &Lease) -> Vec<String> {
    let mut urls = vec![format!("https://localhost:3345/embraOS?token={token}")];
    if lease.source == Source::Dhcp && lease.ip != STATIC_IP {
        urls.push(format!("https://{}:3345/embraOS?token={token}", lease.ip));
    }
    urls
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lease_file_is_parsed_and_a_missing_one_reads_as_static() {
        let text = "interface=eth0\nip=192.168.1.42\nmask=24\nrouter=192.168.1.1 192.168.1.2\ndns=192.168.1.1 9.9.9.9\nsource=dhcp\n";
        let lease = parse(text).unwrap();
        assert_eq!(lease.ip, "192.168.1.42");
        assert_eq!(lease.prefix, "24");
        assert_eq!(lease.router.as_deref(), Some("192.168.1.1"));
        assert_eq!(lease.dns, vec!["192.168.1.1", "9.9.9.9"]);
        assert_eq!(lease.source, Source::Dhcp);
        assert_eq!(lease.summary(), "192.168.1.42/24 on eth0 via 192.168.1.1, dns 192.168.1.1 9.9.9.9 (dhcp)");
        // The record round-trips through its text.
        assert_eq!(parse(&lease.to_text()).unwrap(), lease);
        // A lease without a router or a resolver is still a lease.
        let bare = parse("ip=10.1.1.5\nmask=16\nrouter=\ndns=\nsource=dhcp\n").unwrap();
        assert_eq!(bare.router, None);
        assert!(bare.dns.is_empty());
        assert_eq!(bare.interface, "eth0");
        // Nothing, a half-written file, a foreign file: no lease.
        assert_eq!(parse(""), None);
        assert_eq!(parse("interface=eth0\nsource=dhcp\n"), None);
        assert_eq!(parse("hello"), None);
        // The static fallback is the address every boot before DHCP ran on.
        let s = Lease::static_fallback();
        assert_eq!((s.ip.as_str(), s.prefix.as_str(), s.router.as_deref(), s.source.clone()), ("10.0.2.15", "24", Some("10.0.2.2"), Source::Static));
        assert_eq!(parse(&s.to_text()).unwrap(), s);
    }

    #[test]
    fn the_banner_names_the_leased_address_beside_localhost() {
        let token = "0123456789abcdef0123456789abcdef";
        let leased = parse("ip=192.168.1.42\nmask=24\nrouter=192.168.1.1\ndns=192.168.1.1\nsource=dhcp\n").unwrap();
        assert_eq!(
            console_urls(token, &leased),
            vec![
                "https://localhost:3345/embraOS?token=0123456789abcdef0123456789abcdef".to_string(),
                "https://192.168.1.42:3345/embraOS?token=0123456789abcdef0123456789abcdef".to_string(),
            ]
        );
        // QEMU's own DHCP hands out the static address: one URL, as before.
        let qemu = parse("ip=10.0.2.15\nmask=24\nrouter=10.0.2.2\ndns=10.0.2.3\nsource=dhcp\n").unwrap();
        assert_eq!(console_urls(token, &qemu).len(), 1);
        // The static fallback: one URL.
        assert_eq!(console_urls(token, &Lease::static_fallback()).len(), 1);
    }
}
