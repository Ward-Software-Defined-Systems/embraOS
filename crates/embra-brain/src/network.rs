//! `/network`: this machine's network cards, their addresses and its name,
//! shown and set.
//!
//! embrad owns the network (`crates/embrad/src/net.rs`): it brings every
//! card up, keeps a DHCP client on each and writes what it found under
//! `/embra/ephemeral/net`. This command reads that, and writes the
//! operator's settings to `/embra/state/network/settings`
//! (`embra_boot::net_settings`); embrad finds the file changed within a few
//! seconds and applies it. There is no channel between the brain and PID 1,
//! and none is needed.
//!
//! A machine on a network without DHCP is set up from its own display or
//! its serial line, before any session exists, so the command is answered
//! there too (`grpc_service/update.rs`, `SetupCommands`): it needs no
//! session, no database and no configuration.
//!
//! No tool reaches this: the intelligence sees the network in its status
//! (`tools/status.rs`), and changing it is the operator's.

use std::path::{Path, PathBuf};

use embra_boot::names::{self, CaSet};
use embra_boot::net::{self as cards, Card};
use embra_boot::net_lease::{self, CardStatus, Lease, Mode, Source};
use embra_boot::net_settings::{self, Settings, StaticCard, TimeSetting};
use serde::Serialize;

pub(crate) const USAGE: &str = "Usage: /network | /network static <card> <address>/<prefix> [<gateway>] [dns <server> ...] | /network dhcp <card> | /network hostname <name>|auto | /network time [<server> ...|default|off] | /network wifi ...\n\
  /network                 The cards, their links and addresses, the default route, the resolvers and the machine's name.\n\
  /network static ...      That card keeps this address; DHCP is no longer asked on it. Example: /network static eth0 192.168.1.50/24 192.168.1.1 dns 192.168.1.1\n\
  /network dhcp <card>     Back to DHCP.\n\
  /network hostname <name> The machine's name (letters, digits and hyphens); auto takes the name a DHCP server assigns, or embraos.\n\
  /network time ...        The time servers the clock is checked against, every hour and when the network returns: your own (four at most, host or host:port), default (the ones a DHCP lease names, then time.cloudflare.com), or off. Blank shows the clock.\n\
  /network wifi ...        Wi-Fi: blank shows the card, the network it is on and the ones in range; join <name> --key <passphrase> (or --open) stores one, forget <name> takes one away, country <CC> sets the regulatory domain.\n\
A setting is stored on STATE and applied within a few seconds.";

/// What a run of the command answers.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Reply {
    pub(crate) text: String,
    /// The command did not do what was asked.
    pub(crate) refused: bool,
}

impl Reply {
    pub(crate) fn said(text: impl Into<String>) -> Self {
        Self { text: text.into(), refused: false }
    }

    pub(crate) fn refusal(text: impl Into<String>) -> Self {
        Self { text: text.into(), refused: true }
    }
}

/// Where the network is read and the settings are written. The machine's is
/// [`NetEnv::in_force`]; a test names its own.
pub(crate) struct NetEnv {
    /// The kernel's interfaces (`/sys/class/net`).
    pub(crate) sys: PathBuf,
    /// embrad's records and status.
    pub(crate) net_dir: PathBuf,
    /// The operator's settings.
    pub(crate) settings: PathBuf,
    /// The resolver file.
    pub(crate) resolv: PathBuf,
    /// The kernel's hostname.
    pub(crate) hostname: PathBuf,
    /// embrad's record of the clock.
    pub(crate) clock: PathBuf,
    /// The kernel line: its `embra.ntp=` decides the time servers of its boot.
    pub(crate) cmdline: PathBuf,
    /// Where the trust service keeps the certificate authority, with the
    /// record of what it may sign.
    pub(crate) pki: PathBuf,
    /// The Wi-Fi networks the operator stored (`network_wifi.rs`).
    pub(crate) wifi: PathBuf,
    /// The supplicants' control sockets, one a card.
    pub(crate) wifi_ctrl: PathBuf,
    /// The supplicants' own lines, a file a card.
    pub(crate) wifi_logs: PathBuf,
    /// How long a scan's end is waited for before its results are read.
    pub(crate) scan_wait: std::time::Duration,
    /// How long a supplicant is given to answer one question.
    pub(crate) answer_wait: std::time::Duration,
    /// How long `system_status` waits for a card's supplicant to say what
    /// the card is on.
    pub(crate) status_wait: std::time::Duration,
}

impl NetEnv {
    pub(crate) fn in_force() -> Self {
        NetEnv {
            sys: PathBuf::from(cards::SYS_CLASS_NET),
            net_dir: PathBuf::from(net_lease::NET_DIR),
            settings: PathBuf::from(net_settings::SETTINGS_PATH),
            resolv: PathBuf::from(net_lease::RESOLV_PATH),
            hostname: PathBuf::from("/proc/sys/kernel/hostname"),
            clock: PathBuf::from(embra_boot::clock::STATUS_PATH),
            cmdline: PathBuf::from("/proc/cmdline"),
            pki: PathBuf::from(names::PKI_DIR),
            wifi: PathBuf::from(embra_boot::wifi::WIFI_PATH),
            wifi_ctrl: PathBuf::from(embra_boot::wifi::CTRL_DIR),
            wifi_logs: PathBuf::from(embra_boot::wifi::LOG_DIR),
            scan_wait: std::time::Duration::from_secs(12),
            answer_wait: std::time::Duration::from_secs(3),
            status_wait: crate::network_wifi::STATUS_WAIT,
        }
    }

    fn statuses(&self) -> Vec<CardStatus> {
        std::fs::read_to_string(self.net_dir.join(net_lease::STATUS_FILE)).map(|t| net_lease::parse_status(&t)).unwrap_or_default()
    }

    fn resolvers(&self) -> Vec<String> {
        std::fs::read_to_string(&self.resolv)
            .map(|t| t.lines().filter_map(|l| l.strip_prefix("nameserver ")).map(|s| s.trim().to_string()).collect())
            .unwrap_or_default()
    }

    fn kernel_hostname(&self) -> Option<String> {
        std::fs::read_to_string(&self.hostname).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty() && s != "(none)")
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Show,
    Static { card: String, address: String, gateway: Option<String>, dns: Vec<String> },
    Dhcp(String),
    /// `None`: `auto`.
    Hostname(Option<String>),
    Time(TimeAction),
    Usage,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TimeAction {
    Show,
    /// No setting of the operator's.
    Default,
    /// `off`, or the servers.
    Set(Vec<String>),
}

pub(crate) fn parse(args: &str) -> Action {
    let words: Vec<&str> = args.split_whitespace().collect();
    match words.as_slice() {
        [] | ["show"] => Action::Show,
        ["static", card, address, rest @ ..] => {
            let (gateway, rest) = match rest {
                [first, tail @ ..] if *first != "dns" => (Some(first.to_string()), tail),
                other => (None, other),
            };
            let dns = match rest {
                [] => Vec::new(),
                ["dns", servers @ ..] if !servers.is_empty() => servers.iter().map(|s| s.to_string()).collect(),
                _ => return Action::Usage,
            };
            Action::Static { card: card.to_string(), address: address.to_string(), gateway, dns }
        }
        ["dhcp", card] => Action::Dhcp(card.to_string()),
        ["hostname", "auto"] => Action::Hostname(None),
        ["hostname", name] => Action::Hostname(Some(name.to_string())),
        ["time"] => Action::Time(TimeAction::Show),
        ["time", "default"] => Action::Time(TimeAction::Default),
        ["time", servers @ ..] => Action::Time(TimeAction::Set(servers.iter().map(|s| s.to_string()).collect())),
        _ => Action::Usage,
    }
}

fn link_words(link: Option<bool>) -> &'static str {
    match link {
        Some(true) => "link up",
        Some(false) => "no link",
        None => "link unknown",
    }
}

fn address_words(lease: Option<&Lease>, mode: Mode) -> String {
    match (lease, mode) {
        (Some(l), _) if l.source == Source::Static => format!("{}/{} (set by you){}", l.ip, l.prefix, l.router.as_ref().map(|r| format!(", gateway {r}")).unwrap_or_default()),
        (Some(l), _) => format!("{}/{} (DHCP){}", l.ip, l.prefix, l.router.as_ref().map(|r| format!(", gateway {r}")).unwrap_or_default()),
        (None, Mode::Static) => "no address (the one you set is not on the card)".to_string(),
        (None, Mode::Dhcp) => "no address yet (DHCP is being asked)".to_string(),
    }
}

/// Where the machine's name came from, in words.
fn name_words(env: &NetEnv, settings: &Settings, leases: &[Lease]) -> String {
    let kernel = env.kernel_hostname();
    match (&settings.hostname, &kernel) {
        (Some(set), Some(k)) if set == k => format!("{k} (set by you)"),
        (Some(set), Some(k)) => format!("{k} (you set {set}; embrad applies it within a few seconds)"),
        (Some(set), None) => format!("{set} (set by you; not applied yet)"),
        // A DHCP server that hands the default name back assigned nothing:
        // it is the name the machine asked with.
        (None, Some(k)) if k != names::DEFAULT_HOSTNAME && leases.iter().any(|l| l.hostname.as_deref() == Some(k.as_str())) => format!("{k} (assigned by the DHCP server)"),
        (None, Some(k)) => format!("{k} (the default; /network hostname <name> sets another)"),
        (None, None) => "none yet".to_string(),
    }
}

/// The web console's certificate: the names it carries, by the rule the
/// trust service mints with (`names::leaf_names`), and where its CA is had.
/// A browser that imported the CA opens the console without a warning at
/// each of the names.
fn certificate_lines(env: &NetEnv, leases: &[Lease]) -> String {
    let hostname = env.kernel_hostname();
    let addresses: Vec<std::net::IpAddr> = leases.iter().filter_map(|l| l.ip.parse().ok()).collect();
    let mut out = String::new();
    match CaSet::read(&env.pki) {
        Some(set) => {
            let carried = names::leaf_names(&set, hostname.as_deref(), &addresses);
            out.push_str(&format!("  Certificate: the web console's names are {}\n", carried.listed().join(", ")));
            if let Some(name) = hostname.as_deref().filter(|n| cards::hostname_ok(n) && !set.admits_name(n)) {
                out.push_str(&format!(
                    "             {name} is not among them: a DHCP server assigned it. /network hostname {name} makes it this machine's own name and the certificate's.\n"
                ));
            }
        }
        None => out.push_str("  Certificate: the trust service has not recorded what its CA signs\n"),
    }
    out.push_str("  Its CA, imported into a browser once, ends the browser's warning at those names: /api/ca.crt (a download in the web console)\n");
    out
}

/// What a name the operator sets means for the certificate: it is on the
/// certificate within seconds, and a CA that does not admit it yet is
/// issued again, which a browser that imported it has to learn.
fn certificate_after_naming(env: &NetEnv, name: &str) -> &'static str {
    match CaSet::read(&env.pki) {
        Some(set) if !set.admits_name(name) => {
            " The web console's certificate names it within a few seconds; its CA is issued again to admit the name, so a browser that imported the CA imports it once more (/api/ca.crt)."
        }
        _ => " The web console's certificate names it within a few seconds.",
    }
}

/// What this boot's kernel line says about the time servers, as it is
/// written there: `off`, or a server. `None` without the knob.
fn kernel_time_knob(env: &NetEnv) -> Option<String> {
    let line = embra_boot::Cmdline::parse(&std::fs::read_to_string(&env.cmdline).unwrap_or_default());
    match line.time_server() {
        embra_boot::TimeServer::Default => None,
        embra_boot::TimeServer::Off => Some("off".to_string()),
        embra_boot::TimeServer::Named { host, port } if port == embra_boot::TimeServer::NTP_PORT => Some(host.to_string()),
        embra_boot::TimeServer::Named { host, port } => Some(format!("{host}:{port}")),
    }
}

/// The clock and whom it is checked against, two lines.
fn time_lines(env: &NetEnv, settings: &Settings, leases: &[Lease]) -> String {
    let clock = std::fs::read_to_string(&env.clock).ok().and_then(|t| embra_boot::ClockStatus::parse(&t));
    let said = clock.map(|c| c.summary()).unwrap_or_else(|| "embrad has written no record of the clock".to_string());
    let stored = match &settings.time {
        TimeSetting::Off => "none: /network time off (the clock is not checked)".to_string(),
        TimeSetting::Servers(list) => format!("{} (set by you)", list.join(", ")),
        TimeSetting::Default => {
            let mut named: Vec<String> = Vec::new();
            for server in leases.iter().flat_map(|l| l.ntp.iter()) {
                if named.len() < 2 && !named.contains(server) {
                    named.push(server.clone());
                }
            }
            match named.is_empty() {
                true => format!("{} (the default)", embra_boot::clock::DEFAULT_TIME_SERVER),
                false => format!("{} (named by the DHCP server), then {} (the default)", named.join(", "), embra_boot::clock::DEFAULT_TIME_SERVER),
            }
        }
    };
    // The kernel line decides for its boot; what is stored holds after it.
    let servers = match kernel_time_knob(env).as_deref() {
        None => stored,
        Some("off") => format!("none this boot: embra.ntp=off on the kernel line (the clock is not checked); without it: {stored}"),
        Some(server) => format!("{server} this boot (embra.ntp on the kernel line); without it: {stored}"),
    };
    format!("  Clock: {said}\n  Time servers: {servers}\n")
}

/// When embrad asks the servers just stored: within seconds, unless this
/// boot's kernel line decides.
fn asked_when(env: &NetEnv) -> String {
    match kernel_time_knob(env) {
        None => "embrad asks within a few seconds, then every hour, when the network returns and at every boot; /network time shows the answer.".to_string(),
        Some(knob) => format!("This boot's kernel line carries embra.ntp={knob}, which decides until a boot without it: the setting holds from then."),
    }
}

fn show(env: &NetEnv) -> Reply {
    let found = cards::cards(&env.sys);
    let leases = net_lease::read_all(&env.net_dir);
    let statuses = env.statuses();
    let (settings, refused) = net_settings::read(&env.settings);
    let mut text = String::from("Network\n");
    text.push_str(&format!("  Name: {}\n", name_words(env, &settings, &leases)));
    if found.is_empty() {
        text.push_str("  Cards: none. The kernel shows no network card; one that is plugged in is brought up when it appears.\n");
    } else {
        text.push_str("  Cards:\n");
        for card in &found {
            let status = statuses.iter().find(|s| s.name == card.name);
            let mode = match (status.map(|s| s.mode), settings.static_for(&card.mac)) {
                (Some(m), _) => m,
                (None, Some(_)) => Mode::Static,
                (None, None) => Mode::Dhcp,
            };
            let lease = leases.iter().find(|l| l.interface == card.name);
            let link = status.and_then(|s| s.link).or_else(|| cards::carrier(&env.sys, &card.name));
            text.push_str(&format!("    {:<8} {:<5} {}  {:<12} {}\n", card.name, card.kind.as_str(), card.mac, link_words(link), address_words(lease, mode)));
            if let Some(note) = status.and_then(|s| s.note.as_ref()) {
                text.push_str(&format!("             {note}\n"));
            }
        }
    }
    for stored in settings.statics.iter().filter(|s| !found.iter().any(|c| c.mac.eq_ignore_ascii_case(&s.mac))) {
        text.push_str(&format!("  Stored for a card that is not here: {} keeps {}/{}\n", stored.mac, stored.ip, stored.prefix));
    }
    match leases.iter().find(|l| l.router.is_some()) {
        Some(l) => text.push_str(&format!("  Default route: {} through {}\n", l.router.as_deref().unwrap_or("-"), l.interface)),
        None => text.push_str("  Default route: none\n"),
    }
    let resolvers = env.resolvers();
    text.push_str(&format!("  Resolvers: {}\n", if resolvers.is_empty() { "none".to_string() } else { resolvers.join(" ") }));
    text.push_str(&time_lines(env, &settings, &leases));
    text.push_str(&certificate_lines(env, &leases));
    for line in refused {
        text.push_str(&format!("  A line of the settings file was not accepted: {line}\n"));
    }
    if found.iter().any(|c| c.kind == cards::Kind::Wifi) || !embra_boot::wifi::Wifi::read(&env.wifi).0.networks.is_empty() {
        text.push_str("  Wi-Fi: /network wifi shows the network a card is on, the stored networks and the ones in range\n");
    }
    text.push_str("  /network static <card> <address>/<prefix> [<gateway>] [dns <server> ...] · /network dhcp <card> · /network hostname <name>|auto · /network time <server> ...|default|off · /network wifi");
    Reply::said(text)
}

fn card_named<'a>(found: &'a [Card], name: &str) -> Result<&'a Card, Reply> {
    found.iter().find(|c| c.name == name).ok_or_else(|| {
        let names: Vec<&str> = found.iter().map(|c| c.name.as_str()).collect();
        Reply::refusal(if names.is_empty() {
            format!("No card named {name}: the kernel shows no network card. Nothing was stored.")
        } else {
            format!("No card named {name}: the cards are {}. Nothing was stored.", names.join(", "))
        })
    })
}

fn store(env: &NetEnv, settings: &Settings, done: String) -> Reply {
    match net_settings::write(&env.settings, settings) {
        Ok(()) => Reply::said(done),
        Err(e) => Reply::refusal(format!("The settings could not be written to {}: {e}. Nothing changed.", env.settings.display())),
    }
}

/// Runs the command.
pub(crate) fn answer(args: &str, env: &NetEnv) -> Reply {
    // Wi-Fi has forms of its own: a network's name and its passphrase may
    // hold spaces, so its words are not split here.
    if let Some(rest) = args.trim_start().strip_prefix("wifi").filter(|r| r.is_empty() || r.starts_with(char::is_whitespace)) {
        return crate::network_wifi::answer(rest, env);
    }
    match parse(args) {
        Action::Show => show(env),
        Action::Usage => {
            if matches!(args.trim(), "help" | "--help" | "-h") {
                Reply::said(USAGE)
            } else {
                Reply::refusal(USAGE)
            }
        }
        Action::Static { card, address, gateway, dns } => {
            let found = cards::cards(&env.sys);
            let target = match card_named(&found, &card) {
                Ok(c) => c,
                Err(refusal) => return refusal,
            };
            let dns: Vec<&str> = dns.iter().map(String::as_str).collect();
            let setting = match StaticCard::new(&target.mac, &address, gateway.as_deref(), &dns) {
                Ok(s) => s,
                Err(reason) => return Reply::refusal(format!("Not stored: {reason}.\n{}", USAGE.lines().next().unwrap_or(USAGE))),
            };
            let (mut settings, _) = net_settings::read(&env.settings);
            let mut done = format!("Stored: {} ({}) keeps {}/{}", target.name, target.mac, setting.ip, setting.prefix);
            match setting.gateway {
                Some(gw) => done.push_str(&format!(", gateway {gw}")),
                None => done.push_str(", no gateway (its own network only)"),
            }
            if !setting.dns.is_empty() {
                done.push_str(&format!(", resolvers {}", setting.dns.iter().map(|d| d.to_string()).collect::<Vec<_>>().join(" ")));
            }
            done.push_str(&format!(
                ".\nembrad applies it within a few seconds and no longer asks DHCP on {}; /network shows the result.\nIf this console is open through {}, it is at {} from then on: the same link with that address.",
                target.name, target.name, setting.ip
            ));
            settings.set_static(setting);
            store(env, &settings, done)
        }
        Action::Dhcp(card) => {
            let found = cards::cards(&env.sys);
            let target = match card_named(&found, &card) {
                Ok(c) => c,
                Err(refusal) => return refusal,
            };
            let (mut settings, _) = net_settings::read(&env.settings);
            if !settings.remove_static(&target.mac) {
                return Reply::said(format!("{} is on DHCP already. Nothing changed.", target.name));
            }
            let done = format!(
                "Stored: {} goes back to DHCP.\nembrad takes the address off the card within a few seconds and asks for a lease; /network shows the result.\nIf this console is open through {}, it is at the leased address from then on.",
                target.name, target.name
            );
            store(env, &settings, done)
        }
        Action::Time(TimeAction::Show) => {
            let (settings, _) = net_settings::read(&env.settings);
            Reply::said(time_lines(env, &settings, &net_lease::read_all(&env.net_dir)).trim_end().to_string())
        }
        Action::Time(TimeAction::Default) => {
            let (mut settings, _) = net_settings::read(&env.settings);
            if settings.time == TimeSetting::Default {
                return Reply::said("The time servers are automatic already: the ones a DHCP lease names, then the default. Nothing changed.");
            }
            settings.time = TimeSetting::Default;
            store(env, &settings, format!("Stored: the time servers are automatic again, the ones a DHCP lease names and then {}.\n{}", embra_boot::clock::DEFAULT_TIME_SERVER, asked_when(env)))
        }
        Action::Time(TimeAction::Set(words)) => {
            let words: Vec<&str> = words.iter().map(String::as_str).collect();
            let setting = match TimeSetting::from_words(&words) {
                Ok(s) => s,
                Err(reason) => return Reply::refusal(format!("Not stored: {reason}. A time server is a host name or an address, with a port when it is not 123; four at most; or off, or default.")),
            };
            let done = match &setting {
                TimeSetting::Off => "Stored: no time server is asked, at boot or after. The clock keeps what the hardware clock gave it; /network time default asks again.".to_string(),
                TimeSetting::Servers(list) => format!("Stored: the clock is checked against {}, in that order, and no other.\n{}", list.join(", "), asked_when(env)),
                TimeSetting::Default => String::new(),
            };
            let (mut settings, _) = net_settings::read(&env.settings);
            settings.time = setting;
            store(env, &settings, done)
        }
        Action::Hostname(None) => {
            let (mut settings, _) = net_settings::read(&env.settings);
            if settings.hostname.take().is_none() {
                return Reply::said("The name is automatic already: the one a DHCP server assigns, or embraos. Nothing changed.");
            }
            store(env, &settings, "Stored: the name is automatic again, the one a DHCP server assigns, or embraos. embrad sets it within a few seconds.".to_string())
        }
        Action::Hostname(Some(name)) => {
            if !cards::hostname_ok(&name) {
                return Reply::refusal(format!("Not stored: {name} is not a hostname. Letters, digits and hyphens, 63 at most, no hyphen at either end."));
            }
            let (mut settings, _) = net_settings::read(&env.settings);
            settings.hostname = Some(name.clone());
            store(
                env,
                &settings,
                format!(
                    "Stored: the machine is named {name}.\nembrad sets it within a few seconds. A DHCP server is told the name when a card next starts asking: at the next boot at the latest.{}",
                    certificate_after_naming(env, &name)
                ),
            )
        }
    }
}

/// The network as the intelligence's status shows it: read-only, and never
/// more than the operator sees with `/network`.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct NetworkReport {
    /// The machine's name.
    pub hostname: Option<String>,
    pub cards: Vec<CardReport>,
    pub resolvers: Vec<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct CardReport {
    pub name: String,
    /// `wired` or `wifi`.
    pub kind: &'static str,
    /// Whether the card has a link; null when the kernel does not say.
    pub link: Option<bool>,
    /// `dhcp`, or `static` for an address the operator set.
    pub mode: &'static str,
    /// `address/prefix`, when the card holds one.
    pub address: Option<String>,
    pub gateway: Option<String>,
    /// The name of the Wi-Fi network a Wi-Fi card is on, as its supplicant
    /// says it now (`/network wifi` shows the same name); absent on a wired
    /// card and on a card that is on no network. Never a passphrase, and
    /// no network that is merely stored or in range.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wifi_network: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The report, on a machine whose embrad wrote its status; `None` elsewhere
/// (a development host, an embrad from before). It asks a Wi-Fi card's
/// supplicant which network the card is on, on a socket and for a second at
/// most: a caller on the runtime runs it on the blocking pool.
pub(crate) fn report(env: &NetEnv) -> Option<NetworkReport> {
    if !Path::new(&env.net_dir).join(net_lease::STATUS_FILE).exists() {
        return None;
    }
    let leases = net_lease::read_all(&env.net_dir);
    let cards = env
        .statuses()
        .into_iter()
        .map(|s| {
            let lease = leases.iter().find(|l| l.interface == s.name);
            CardReport {
                kind: s.kind.as_str(),
                link: s.link,
                mode: if s.mode == Mode::Static { "static" } else { "dhcp" },
                address: lease.map(|l| format!("{}/{}", l.ip, l.prefix)),
                gateway: lease.and_then(|l| l.router.clone()),
                wifi_network: (s.kind == cards::Kind::Wifi).then(|| crate::network_wifi::joined_network(env, &s.name)).flatten(),
                note: s.note,
                name: s.name,
            }
        })
        .collect();
    Some(NetworkReport { hostname: env.kernel_hostname(), cards, resolvers: env.resolvers() })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC0: &str = "52:54:00:aa:00:01";
    const MAC1: &str = "52:54:00:aa:00:02";

    /// A machine in a directory: two wired cards as sysfs shows them.
    fn machine(tag: &str) -> (PathBuf, NetEnv) {
        let root = std::env::temp_dir().join(format!("embra-brain-network-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (name, index, mac) in [("eth0", 2, MAC0), ("eth1", 3, MAC1)] {
            let dir = root.join("sys").join(name);
            std::fs::create_dir_all(dir.join("device")).unwrap();
            std::fs::write(dir.join("type"), "1\n").unwrap();
            std::fs::write(dir.join("ifindex"), format!("{index}\n")).unwrap();
            std::fs::write(dir.join("address"), format!("{mac}\n")).unwrap();
        }
        std::fs::create_dir_all(root.join("net")).unwrap();
        let env = NetEnv {
            sys: root.join("sys"),
            net_dir: root.join("net"),
            settings: root.join("state").join("network").join("settings"),
            resolv: root.join("resolv.conf"),
            hostname: root.join("hostname"),
            clock: root.join("clock-status"),
            cmdline: root.join("cmdline"),
            pki: root.join("state").join("pki"),
            wifi: root.join("state").join("network").join("wifi"),
            wifi_ctrl: root.join("net").join("wpa"),
            wifi_logs: root.join("net").join("wpa-log"),
            scan_wait: std::time::Duration::from_millis(20),
            answer_wait: std::time::Duration::from_secs(3),
            status_wait: crate::network_wifi::STATUS_WAIT,
        };
        (root, env)
    }

    #[test]
    fn the_command_is_read_into_its_forms() {
        assert_eq!(parse(""), Action::Show);
        assert_eq!(parse("  show "), Action::Show);
        assert_eq!(
            parse("static eth0 192.168.1.50/24 192.168.1.1 dns 192.168.1.1 9.9.9.9"),
            Action::Static { card: "eth0".into(), address: "192.168.1.50/24".into(), gateway: Some("192.168.1.1".into()), dns: vec!["192.168.1.1".into(), "9.9.9.9".into()] }
        );
        assert_eq!(parse("static eth0 10.0.0.9/8"), Action::Static { card: "eth0".into(), address: "10.0.0.9/8".into(), gateway: None, dns: vec![] });
        assert_eq!(parse("static eth0 10.0.0.9/8 dns 10.0.0.1"), Action::Static { card: "eth0".into(), address: "10.0.0.9/8".into(), gateway: None, dns: vec!["10.0.0.1".into()] });
        assert_eq!(parse("dhcp eth1"), Action::Dhcp("eth1".into()));
        assert_eq!(parse("hostname axiom"), Action::Hostname(Some("axiom".into())));
        assert_eq!(parse("hostname auto"), Action::Hostname(None));
        assert_eq!(parse("time"), Action::Time(TimeAction::Show));
        assert_eq!(parse("time default"), Action::Time(TimeAction::Default));
        assert_eq!(parse("time off"), Action::Time(TimeAction::Set(vec!["off".into()])));
        assert_eq!(parse("time 192.168.1.1 ntp.example.org"), Action::Time(TimeAction::Set(vec!["192.168.1.1".into(), "ntp.example.org".into()])));
        for odd in ["static", "static eth0", "static eth0 10.0.0.9/8 10.0.0.1 extra", "static eth0 10.0.0.9/8 dns", "dhcp", "dhcp eth0 eth1", "hostname", "hostname a b", "frobnicate", "help"] {
            assert_eq!(parse(odd), Action::Usage, "{odd}");
        }
    }

    #[test]
    fn an_address_is_stored_for_the_cards_hardware_address_and_taken_away_again() {
        let (root, env) = machine("static");
        let reply = answer("static eth1 192.168.1.50/24 192.168.1.1 dns 192.168.1.1", &env);
        assert!(!reply.refused, "{}", reply.text);
        assert!(reply.text.starts_with(&format!("Stored: eth1 ({MAC1}) keeps 192.168.1.50/24, gateway 192.168.1.1, resolvers 192.168.1.1.")), "{}", reply.text);
        assert!(reply.text.contains("it is at 192.168.1.50 from then on"), "the operator's own connection may be the one that changes");
        let (settings, refused) = net_settings::read(&env.settings);
        assert!(refused.is_empty());
        assert_eq!(settings.statics.len(), 1);
        assert_eq!(settings.static_for(MAC1).unwrap().ip.to_string(), "192.168.1.50");
        // The same card again: one entry, the new address.
        assert!(!answer("static eth1 192.168.1.60/24", &env).refused);
        assert_eq!(net_settings::read(&env.settings).0.statics.len(), 1);
        // Back to DHCP, and once more: nothing to change.
        let back = answer("dhcp eth1", &env);
        assert!(back.text.starts_with("Stored: eth1 goes back to DHCP."), "{}", back.text);
        assert!(net_settings::read(&env.settings).0.statics.is_empty());
        assert_eq!(answer("dhcp eth1", &env), Reply::said("eth1 is on DHCP already. Nothing changed."));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn what_does_not_hold_is_refused_and_nothing_is_stored() {
        let (root, env) = machine("refused");
        let table = [
            ("static eth9 192.168.1.50/24", "No card named eth9: the cards are eth0, eth1."),
            ("static eth0 192.168.1.50", "Not stored: 192.168.1.50 has no prefix"),
            ("static eth0 192.168.1.50/24 10.9.9.9", "Not stored: the gateway 10.9.9.9 is not in 192.168.1.0/24"),
            ("static eth0 192.168.1.50/24 dns one.example", "Not stored: one.example is not an IPv4 address"),
            ("dhcp eth9", "No card named eth9"),
            ("hostname not_a_name", "Not stored: not_a_name is not a hostname"),
            ("hostname bad", "Not stored: bad is not a hostname"),
            ("frobnicate", "Usage: /network"),
        ];
        for (args, said) in table {
            let reply = answer(args, &env);
            assert!(reply.refused && reply.text.starts_with(said), "{args}: {}", reply.text);
        }
        assert!(!env.settings.exists(), "nothing was written");
        // The usage, asked for, is no refusal.
        assert_eq!(answer("help", &env), Reply::said(USAGE));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_name_is_stored_and_auto_takes_it_away() {
        let (root, env) = machine("name");
        let reply = answer("hostname axiom", &env);
        assert!(reply.text.starts_with("Stored: the machine is named axiom."), "{}", reply.text);
        assert_eq!(net_settings::read(&env.settings).0.hostname.as_deref(), Some("axiom"));
        // A card's address beside it is kept.
        assert!(!answer("static eth0 10.0.0.9/8", &env).refused);
        assert!(answer("hostname auto", &env).text.starts_with("Stored: the name is automatic again"));
        let (settings, _) = net_settings::read(&env.settings);
        assert_eq!((settings.hostname, settings.statics.len()), (None, 1));
        assert!(answer("hostname auto", &env).text.starts_with("The name is automatic already"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_cards_are_shown_with_their_links_addresses_and_how_they_came() {
        let (root, env) = machine("show");
        std::fs::write(env.net_dir.join("status"), format!("card=eth0\nkind=wired\nmac={MAC0}\nlink=up\nmode=static\nnote=\n\ncard=eth1\nkind=wired\nmac={MAC1}\nlink=down\nmode=dhcp\nnote=\n\n")).unwrap();
        std::fs::write(net_lease::lease_path(&env.net_dir, "eth0"), "interface=eth0\nip=192.168.1.50\nmask=24\nrouter=192.168.1.1\ndns=192.168.1.1\nmetric=102\nsource=static\n").unwrap();
        std::fs::write(&env.resolv, "nameserver 192.168.1.1\n").unwrap();
        std::fs::write(&env.hostname, "axiom\n").unwrap();
        net_settings::write(&env.settings, &Settings { hostname: Some("axiom".into()), time: Default::default(), statics: vec![StaticCard::new(MAC0, "192.168.1.50/24", Some("192.168.1.1"), &["192.168.1.1"]).unwrap(), StaticCard::new("02:00:00:00:09:09", "10.9.9.9/8", None, &[]).unwrap()] }).unwrap();
        let text = answer("", &env).text;
        assert!(text.contains("  Name: axiom (set by you)\n"), "{text}");
        assert!(text.contains(&format!("    eth0     wired {MAC0}  link up      192.168.1.50/24 (set by you), gateway 192.168.1.1\n")), "{text}");
        assert!(text.contains(&format!("    eth1     wired {MAC1}  no link      no address yet (DHCP is being asked)\n")), "{text}");
        assert!(text.contains("  Stored for a card that is not here: 02:00:00:00:09:09 keeps 10.9.9.9/8\n"), "{text}");
        assert!(text.contains("  Default route: 192.168.1.1 through eth0\n") && text.contains("  Resolvers: 192.168.1.1\n"), "{text}");
        // The intelligence's view of the same: no more than the operator's.
        let report = report(&env).unwrap();
        assert_eq!(report.hostname.as_deref(), Some("axiom"));
        assert_eq!(report.cards.iter().map(|c| (c.name.as_str(), c.mode, c.address.as_deref(), c.link)).collect::<Vec<_>>(), [("eth0", "static", Some("192.168.1.50/24"), Some(true)), ("eth1", "dhcp", None, Some(false))]);
        assert_eq!(report.resolvers, ["192.168.1.1"]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_time_servers_are_stored_shown_and_taken_away() {
        let (root, env) = machine("time");
        // Nothing stored, no lease: the default, and no record of the clock.
        let shown = answer("time", &env).text;
        assert_eq!(shown, "  Clock: embrad has written no record of the clock\n  Time servers: time.cloudflare.com (the default)");
        // A lease names two: they are asked first.
        std::fs::write(net_lease::lease_path(&env.net_dir, "eth0"), "interface=eth0\nip=192.168.1.42\nmask=24\nntp=192.168.1.1 192.168.1.5 192.168.1.9\nmetric=102\nsource=dhcp\n").unwrap();
        assert!(answer("time", &env).text.ends_with("Time servers: 192.168.1.1, 192.168.1.5 (named by the DHCP server), then time.cloudflare.com (the default)"));
        // The operator's own replace both.
        let stored = answer("time ntp.example.org 10.0.2.2:11123", &env);
        assert_eq!(
            stored.text,
            "Stored: the clock is checked against ntp.example.org, 10.0.2.2:11123, in that order, and no other.\nembrad asks within a few seconds, then every hour, when the network returns and at every boot; /network time shows the answer."
        );
        assert_eq!(net_settings::read(&env.settings).0.time, TimeSetting::Servers(vec!["ntp.example.org".into(), "10.0.2.2:11123".into()]));
        assert!(answer("time", &env).text.ends_with("Time servers: ntp.example.org, 10.0.2.2:11123 (set by you)"));
        // Off, and back; a name stored beside it is kept.
        assert!(!answer("hostname axiom", &env).refused);
        assert!(answer("time off", &env).text.starts_with("Stored: no time server is asked"));
        assert!(answer("time default", &env).text.starts_with("Stored: the time servers are automatic again"));
        assert!(answer("time default", &env).text.starts_with("The time servers are automatic already"));
        let (settings, _) = net_settings::read(&env.settings);
        assert_eq!((settings.time, settings.hostname.as_deref()), (TimeSetting::Default, Some("axiom")));
        // What is not a server is refused, and the clock's record is shown as embrad wrote it.
        for bad in ["time not_a_name", "time a b c d e", "time host:0"] {
            assert!(answer(bad, &env).refused, "{bad}");
        }
        std::fs::write(&env.clock, "sync=synchronized\nserver=192.168.1.1\naddress=192.168.1.1:123\noffset=0.012\nstratum=2\nrtc=unchanged\n").unwrap();
        assert!(answer("", &env).text.contains("  Clock: set from 192.168.1.1 (192.168.1.1:123) at boot, corrected by +0.012 s\n"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn where_the_machines_name_came_from_is_said() {
        let (root, env) = machine("name-words");
        let name = |env: &NetEnv| answer("", env).text.lines().nth(1).unwrap_or("").to_string();
        assert_eq!(name(&env), "  Name: none yet");
        std::fs::write(&env.hostname, "embraos\n").unwrap();
        assert_eq!(name(&env), "  Name: embraos (the default; /network hostname <name> sets another)");
        // A DHCP server that hands the default name back in its lease
        // assigned nothing: it is the name the machine asked with.
        std::fs::write(net_lease::lease_path(&env.net_dir, "eth0"), "interface=eth0\nip=192.168.1.42\nmask=24\nhostname=embraos\nmetric=102\nsource=dhcp\n").unwrap();
        assert_eq!(name(&env), "  Name: embraos (the default; /network hostname <name> sets another)");
        // A name of the server's own is assigned.
        std::fs::write(net_lease::lease_path(&env.net_dir, "eth0"), "interface=eth0\nip=192.168.1.42\nmask=24\nhostname=node-7\nmetric=102\nsource=dhcp\n").unwrap();
        std::fs::write(&env.hostname, "node-7\n").unwrap();
        assert_eq!(name(&env), "  Name: node-7 (assigned by the DHCP server)");
        // The operator's own, applied and not yet applied.
        assert!(!answer("hostname axiom", &env).refused);
        assert_eq!(name(&env), "  Name: node-7 (you set axiom; embrad applies it within a few seconds)");
        std::fs::write(&env.hostname, "axiom\n").unwrap();
        assert_eq!(name(&env), "  Name: axiom (set by you)");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_certificates_names_are_shown_with_where_its_ca_is_had() {
        let (root, env) = machine("certificate");
        let ca_line = "  Its CA, imported into a browser once, ends the browser's warning at those names: /api/ca.crt (a download in the web console)\n";
        // Before the trust service has written its record.
        assert!(answer("", &env).text.contains(&format!("  Certificate: the trust service has not recorded what its CA signs\n{ca_line}")));
        // A CA made here, a lease, the default name.
        std::fs::create_dir_all(&env.pki).unwrap();
        std::fs::write(env.pki.join(names::CA_NAMES_FILE), "set=private.v1\n").unwrap();
        std::fs::write(&env.hostname, "embraos\n").unwrap();
        std::fs::write(net_lease::lease_path(&env.net_dir, "eth0"), "interface=eth0\nip=192.168.1.42\nmask=24\nmetric=102\nsource=dhcp\n").unwrap();
        assert!(answer("", &env).text.contains(&format!("  Certificate: the web console's names are localhost, embraos, 127.0.0.1, ::1, 192.168.1.42\n{ca_line}")));
        // A name a DHCP server assigned is the machine's and is not on the certificate; the way to put it there is said.
        std::fs::write(&env.hostname, "node-7\n").unwrap();
        let shown = answer("", &env).text;
        assert!(shown.contains("  Certificate: the web console's names are localhost, 127.0.0.1, ::1, 192.168.1.42\n             node-7 is not among them: a DHCP server assigned it. /network hostname node-7 makes it this machine's own name and the certificate's.\n"), "{shown}");
        // Setting it says what happens to the CA, when the CA has to change.
        let stored = answer("hostname node-7", &env).text;
        assert!(stored.ends_with("its CA is issued again to admit the name, so a browser that imported the CA imports it once more (/api/ca.crt)."), "{stored}");
        // The trust service issued it again: the name is on the certificate, and setting it again changes no CA.
        std::fs::write(env.pki.join(names::CA_NAMES_FILE), "set=private.v1\nhostname=node-7\n").unwrap();
        assert!(answer("", &env).text.contains("  Certificate: the web console's names are localhost, node-7, 127.0.0.1, ::1, 192.168.1.42\n  Its CA"));
        assert!(answer("hostname node-7", &env).text.ends_with("The web console's certificate names it within a few seconds."));
        // A CA from before the limits admits any name, and a public address is on no certificate.
        std::fs::write(env.pki.join(names::CA_NAMES_FILE), "set=none\n").unwrap();
        std::fs::write(net_lease::lease_path(&env.net_dir, "eth1"), "interface=eth1\nip=203.0.113.7\nmask=24\nmetric=103\nsource=dhcp\n").unwrap();
        assert!(answer("", &env).text.contains("  Certificate: the web console's names are localhost, node-7, 127.0.0.1, ::1, 192.168.1.42\n  Its CA"));
        assert!(answer("hostname axiom", &env).text.ends_with("The web console's certificate names it within a few seconds."));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_kernel_line_that_names_the_time_server_decides_for_its_boot_and_is_said() {
        let (root, env) = machine("time-knob");
        std::fs::write(&env.cmdline, "console=ttyS0 embra.slot=a embra.ntp=10.0.2.2:11123 quiet\n").unwrap();
        assert!(answer("time", &env).text.ends_with("Time servers: 10.0.2.2:11123 this boot (embra.ntp on the kernel line); without it: time.cloudflare.com (the default)"));
        // A setting is stored all the same, and the answer says when it holds.
        let stored = answer("time ntp.example.org", &env);
        assert_eq!(
            stored.text,
            "Stored: the clock is checked against ntp.example.org, in that order, and no other.\nThis boot's kernel line carries embra.ntp=10.0.2.2:11123, which decides until a boot without it: the setting holds from then."
        );
        assert!(answer("time", &env).text.ends_with("Time servers: 10.0.2.2:11123 this boot (embra.ntp on the kernel line); without it: ntp.example.org (set by you)"));
        // Off on the kernel line: nobody is asked this boot.
        std::fs::write(&env.cmdline, "embra.ntp=off\n").unwrap();
        assert!(answer("time", &env).text.ends_with("Time servers: none this boot: embra.ntp=off on the kernel line (the clock is not checked); without it: ntp.example.org (set by you)"));
        assert!(answer("time default", &env).text.ends_with("This boot's kernel line carries embra.ntp=off, which decides until a boot without it: the setting holds from then."));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_machine_without_embrads_status_has_no_report_and_still_shows_its_cards() {
        let (root, env) = machine("bare");
        assert_eq!(report(&env), None);
        let text = answer("", &env).text;
        assert!(text.contains("  Name: none yet\n") && text.contains("eth0") && text.contains("  Default route: none\n") && text.contains("  Resolvers: none\n"), "{text}");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
