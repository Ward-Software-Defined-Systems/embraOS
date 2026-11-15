//! The network: every card of the machine, for as long as the machine runs.
//!
//! embrad used to ask for `eth0` once, wait ten seconds for a lease, kill
//! the client and put QEMU's static address on the card. On a machine with
//! two ports the cable was in the other one; a USB card that the kernel
//! names `usb0`, or that appeared a moment later, was never looked at; a
//! switch port that took longer than ten seconds was never asked again.
//!
//! Now a card is whatever the kernel says is one (`embra_boot::net::cards`),
//! and each gets a DHCP client of its own that embrad keeps: BusyBox
//! `udhcpc` in the foreground, which repeats its discovers for as long as
//! the card is there. The boot goes on at the first address on any card and
//! waits ten seconds for one at most. A keeper then looks every two seconds:
//! a card that appears gets its client, a card that goes loses it, a link
//! that returns is asked again, and the resolver file follows the card with
//! the best route. The keeper is a plain thread, not a task of the runtime:
//! a maintenance boot does its job and restarts before the runtime exists,
//! and needs a late lease and the resolver file all the same.
//!
//! The machine sits behind [`Host`], so the logic is tested without one. A
//! panic on any thread of PID 1 halts the machine, and this code reads what
//! a DHCP server sends for as long as the machine runs: nothing here indexes
//! or unwraps.

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use embra_boot::net::{self as cards, Card};
use embra_boot::net_lease::{self, CardStatus, Lease, Mode};
use tracing::{info, warn};

/// How long the boot waits for a first address. A DHCP server on the
/// segment answers within a second; a link takes a few to come up.
pub(crate) const BOOT_WAIT: Duration = Duration::from_secs(10);
/// How long the boot waits for a card when the kernel shows none yet (a USB
/// card is found a moment after the controller it hangs on).
pub(crate) const CARD_WAIT: Duration = Duration::from_secs(5);
/// How often the boot looks while it waits.
const BOOT_TICK: Duration = Duration::from_millis(100);
/// How often the keeper looks.
pub(crate) const KEEPER_TICK: Duration = Duration::from_secs(2);
/// A link that was down for less than this is renewed in place, so the
/// connections stay; after a longer break the network may be another one,
/// and the lease is given up and asked for again.
const SHORT_BREAK: Duration = Duration::from_secs(10);
/// A client that exited is not started again sooner than this.
const RESTART_PAUSE: Duration = Duration::from_secs(5);
/// The name every DHCP request carries.
pub(crate) const DEFAULT_HOSTNAME: &str = "embraos";

/// The helpers embrad starts beside its services: a DHCP client a card. The
/// reaper (`reconcile::reap_orphans`) takes every exited child that is not
/// on a list; these are on this one, so their exits stay for the owner to
/// collect. A helper is started with the lock held and the reaper sweeps
/// with it held, so no exit is taken between the two.
static HELPERS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

pub(crate) fn helpers() -> MutexGuard<'static, Vec<u32>> {
    // A poisoned lock means a thread panicked, and a panic halts PID 1; in
    // a test the list is still the list.
    HELPERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs `sweep` with the helpers' pids and the lock held.
pub(crate) fn with_helpers<R>(sweep: impl FnOnce(&[u32]) -> R) -> R {
    let list = helpers();
    sweep(&list)
}

/// The machine as this module needs it.
pub(crate) trait Host {
    /// The cards the kernel has now.
    fn cards(&mut self) -> Vec<Card>;
    /// Whether the card has a link.
    fn carrier(&mut self, card: &str) -> Option<bool>;
    /// The interface up (the DHCP client does not raise it).
    fn raise(&mut self, card: &str) -> bool;
    /// A DHCP client on the card; its pid.
    fn start_client(&mut self, card: &Card, hostname: &str) -> Option<u32>;
    /// Whether that client has exited (and collects it when it has).
    fn client_gone(&mut self, pid: u32) -> bool;
    /// The client renews its lease in place.
    fn renew(&mut self, pid: u32);
    /// The client gives its lease up and asks again.
    fn release_and_ask(&mut self, pid: u32);
    /// The client is stopped and collected.
    fn stop_client(&mut self, pid: u32);
    /// Every record, best route first.
    fn leases(&mut self) -> Vec<Lease>;
    /// A card's record and addresses are removed.
    fn forget(&mut self, card: &str);
    fn write_resolver(&mut self, text: &str);
    fn write_status(&mut self, text: &str);
}

/// Time as the boot's wait needs it.
pub(crate) trait Pace {
    fn now(&mut self) -> Instant;
    fn sleep(&mut self, d: Duration);
}

/// What the network looks like after a look.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Summary {
    /// How many cards the kernel has.
    pub cards: usize,
    /// The addresses this machine holds, best route first.
    pub leases: Vec<Lease>,
    /// Whether any card has a link.
    pub any_link: bool,
}

struct Slot {
    card: Card,
    client: Option<u32>,
    /// When a client was last started on this card.
    started: Option<Instant>,
    link: Option<bool>,
    down_since: Option<Instant>,
    lease: Option<Lease>,
    /// Since when this card has been asking (for the log's "after").
    asking_since: Instant,
}

/// The cards and what embrad last wrote about them.
pub(crate) struct Net {
    hostname: String,
    slots: Vec<Slot>,
    resolver: Option<String>,
    status: Option<String>,
}

fn same_card(a: &Card, b: &Card) -> bool {
    // A card that was pulled and put back has a new index: it is a new card.
    a.name == b.name && a.index == b.index
}

impl Net {
    pub(crate) fn new(hostname: &str) -> Self {
        Net { hostname: hostname.to_string(), slots: Vec::new(), resolver: None, status: None }
    }

    /// One look: the cards that went and came, each client and link, the
    /// records, the resolver file, the status.
    pub(crate) fn tick(&mut self, host: &mut dyn Host, now: Instant) -> Summary {
        let seen = host.cards();

        // The cards that went.
        let mut kept = Vec::with_capacity(self.slots.len());
        for slot in std::mem::take(&mut self.slots) {
            if seen.iter().any(|c| same_card(c, &slot.card)) {
                kept.push(slot);
            } else {
                info!("{}: the card is gone", slot.card.name);
                if let Some(pid) = slot.client {
                    host.stop_client(pid);
                }
                host.forget(&slot.card.name);
            }
        }
        self.slots = kept;

        // The cards that came.
        for card in &seen {
            if self.slots.iter().any(|s| same_card(card, &s.card)) {
                continue;
            }
            info!("{}: a {} card ({}); asking for a DHCP lease", card.name, card.kind.as_str(), card.mac);
            if !host.raise(&card.name) {
                warn!("{}: the interface could not be brought up", card.name);
            }
            let client = host.start_client(card, &self.hostname);
            self.slots.push(Slot { card: card.clone(), client, started: Some(now), link: None, down_since: None, lease: None, asking_since: now });
        }

        // Each card's client and link.
        for slot in self.slots.iter_mut() {
            if let Some(pid) = slot.client
                && host.client_gone(pid)
            {
                warn!("{}: the DHCP client exited; it is started again", slot.card.name);
                slot.client = None;
            }
            let rested = slot.started.is_none_or(|t| now.saturating_duration_since(t) >= RESTART_PAUSE);
            if slot.client.is_none() && rested {
                slot.client = host.start_client(&slot.card, &self.hostname);
                slot.started = Some(now);
            }
            let link = host.carrier(&slot.card.name);
            let was_up = slot.link == Some(true);
            let is_up = link == Some(true);
            if was_up && !is_up {
                slot.down_since = Some(now);
                info!("{}: the link is down", slot.card.name);
            } else if !was_up && is_up {
                if let (Some(pid), Some(_), Some(down)) = (slot.client, slot.lease.as_ref(), slot.down_since) {
                    if now.saturating_duration_since(down) < SHORT_BREAK {
                        info!("{}: the link is back; the lease is renewed in place", slot.card.name);
                        host.renew(pid);
                    } else {
                        info!("{}: the link is back after a long break; the lease is asked for again", slot.card.name);
                        host.release_and_ask(pid);
                    }
                }
                slot.down_since = None;
            }
            slot.link = link;
        }

        // The records of the cards that are there.
        let leases: Vec<Lease> = host.leases().into_iter().filter(|l| self.slots.iter().any(|s| s.card.name == l.interface)).collect();
        for slot in self.slots.iter_mut() {
            let current = leases.iter().find(|l| l.interface == slot.card.name).cloned();
            if current != slot.lease {
                match (&current, &slot.lease) {
                    (Some(l), _) => info!(
                        "{}: {} after {:.1} s",
                        slot.card.name,
                        l.summary(),
                        now.saturating_duration_since(slot.asking_since).as_secs_f32()
                    ),
                    (None, Some(_)) => {
                        info!("{}: its address is gone; DHCP is asked again", slot.card.name);
                        slot.asking_since = now;
                    }
                    (None, None) => {}
                }
                slot.lease = current;
            }
        }

        // The resolver file follows the card with the best route.
        let resolver = net_lease::resolv_text(&leases);
        if self.resolver.as_deref() != Some(resolver.as_str()) {
            let servers: Vec<&str> = resolver.lines().filter_map(|l| l.strip_prefix("nameserver ")).collect();
            info!("the resolver file: {}", if servers.is_empty() { "no resolver".to_string() } else { servers.join(" ") });
            host.write_resolver(&resolver);
            self.resolver = Some(resolver);
        }

        let status = net_lease::status_text(
            &self
                .slots
                .iter()
                .map(|s| CardStatus {
                    name: s.card.name.clone(),
                    kind: s.card.kind,
                    mac: s.card.mac.clone(),
                    link: s.link,
                    mode: Mode::Dhcp,
                    note: s.client.is_none().then(|| "the DHCP client is not running".to_string()),
                })
                .collect::<Vec<_>>(),
        );
        if self.status.as_deref() != Some(status.as_str()) {
            host.write_status(&status);
            self.status = Some(status);
        }

        Summary { cards: self.slots.len(), any_link: self.slots.iter().any(|s| s.link == Some(true)), leases }
    }

    /// The names of the cards that are still asking.
    fn asking(&self) -> Vec<&str> {
        self.slots.iter().filter(|s| s.lease.is_none()).map(|s| s.card.name.as_str()).collect()
    }
}

/// The boot's wait: it goes on at the first address on any card, after
/// [`BOOT_WAIT`] without one, or after [`CARD_WAIT`] when the kernel has no
/// card at all. Nothing is given up at the end of it: the clients go on
/// asking and the keeper takes up whatever comes.
pub(crate) fn boot(net: &mut Net, host: &mut dyn Host, pace: &mut dyn Pace) -> Summary {
    let started = pace.now();
    loop {
        let now = pace.now();
        let summary = net.tick(host, now);
        if !summary.leases.is_empty() {
            return summary;
        }
        let waited = now.saturating_duration_since(started);
        if summary.cards == 0 && waited >= CARD_WAIT {
            warn!("no network card was found; one that appears later is brought up then");
            return summary;
        }
        if waited >= BOOT_WAIT {
            if summary.cards == 0 {
                warn!("no network card was found; one that appears later is brought up then");
            } else {
                warn!("no address after {} s; DHCP is still being asked on {}", BOOT_WAIT.as_secs(), net.asking().join(", "));
            }
            return summary;
        }
        pace.sleep(BOOT_TICK);
    }
}

/// The keeper: one look every [`KEEPER_TICK`], for the life of the boot.
#[cfg(target_os = "linux")]
fn keep(mut net: Net, mut host: this_machine::ThisMachine) {
    let spawned = std::thread::Builder::new().name("net-keeper".into()).spawn(move || {
        loop {
            std::thread::sleep(KEEPER_TICK);
            net.tick(&mut host, Instant::now());
        }
    });
    if let Err(e) = spawned {
        warn!("no thread for the network's keeper ({e}): a card or a lease that comes later is not taken up this boot");
    }
}

/// The network at boot, for PID 1: every card asked, the boot's wait, then
/// the keeper. The environment is set by now (`main`), so a thread may run.
#[cfg(target_os = "linux")]
pub(crate) fn bring_up() -> Summary {
    let mut host = this_machine::ThisMachine::new();
    let mut net = Net::new(DEFAULT_HOSTNAME);
    let summary = boot(&mut net, &mut host, &mut this_machine::Clock);
    keep(net, host);
    summary
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn bring_up() -> Summary {
    warn!("network setup: not on Linux, skipping");
    Summary::default()
}

#[cfg(target_os = "linux")]
mod this_machine {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::process::{Child, Command, Stdio};

    /// BusyBox's DHCP client, the lease script it runs at every event, and
    /// `ip`. Absolute paths: PID 1 starts with no PATH in its environment.
    const UDHCPC: &str = "/sbin/udhcpc";
    const LEASE_SCRIPT: &str = "/usr/share/embra/udhcpc.script";
    const IP: &str = "/sbin/ip";

    pub(super) struct Clock;

    impl Pace for Clock {
        fn now(&mut self) -> Instant {
            Instant::now()
        }
        fn sleep(&mut self, d: Duration) {
            std::thread::sleep(d);
        }
    }

    pub(super) struct ThisMachine {
        children: BTreeMap<u32, Child>,
    }

    impl ThisMachine {
        pub(super) fn new() -> Self {
            if let Err(e) = std::fs::create_dir_all(net_lease::NET_DIR) {
                warn!("{} could not be made: {e}", net_lease::NET_DIR);
            }
            ThisMachine { children: BTreeMap::new() }
        }

        fn drop_child(&mut self, pid: u32) {
            self.children.remove(&pid);
            helpers().retain(|p| *p != pid);
        }

        fn signal(&self, pid: u32, signal: libc::c_int) {
            // Only a client this module still holds: its pid is not reused
            // while its exit has not been collected.
            if self.children.contains_key(&pid) {
                // SAFETY: kill(2) with a pid of a child of this process.
                unsafe { libc::kill(pid as libc::pid_t, signal) };
            }
        }
    }

    /// Writes `text` through a temporary file beside `path` and a rename,
    /// so a reader never sees half of it.
    fn write_whole(path: &Path, text: &str) {
        let tmp = path.with_extension("tmp");
        let done = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, path));
        if let Err(e) = done {
            warn!("{} could not be written: {e}", path.display());
        }
    }

    impl Host for ThisMachine {
        fn cards(&mut self) -> Vec<Card> {
            cards::cards(Path::new(cards::SYS_CLASS_NET))
        }

        fn carrier(&mut self, card: &str) -> Option<bool> {
            cards::carrier(Path::new(cards::SYS_CLASS_NET), card)
        }

        fn raise(&mut self, card: &str) -> bool {
            let name = card.as_bytes();
            if name.is_empty() || name.len() >= libc::IFNAMSIZ {
                return false;
            }
            // SAFETY: a zeroed ifreq with a name shorter than its field (so
            // it stays NUL-terminated), on a socket opened and closed here.
            unsafe {
                let sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
                if sock < 0 {
                    return false;
                }
                let mut ifr: libc::ifreq = std::mem::zeroed();
                std::ptr::copy_nonoverlapping(name.as_ptr(), ifr.ifr_name.as_mut_ptr() as *mut u8, name.len());
                let mut ok = false;
                if libc::ioctl(sock, libc::SIOCGIFFLAGS as _, &mut ifr) >= 0 {
                    ifr.ifr_ifru.ifru_flags |= libc::IFF_UP as i16;
                    ok = libc::ioctl(sock, libc::SIOCSIFFLAGS as _, &ifr) >= 0;
                }
                libc::close(sock);
                ok
            }
        }

        fn start_client(&mut self, card: &Card, hostname: &str) -> Option<u32> {
            // `-f`: in the foreground, so it stays this process's child.
            // Without `-n` or `-b` it repeats its three discovers and the
            // five-second pause for as long as the card is there. Its own
            // lines are discarded: a card without a cable would write one
            // every few seconds for ever, and embrad logs what changes.
            let mut list = helpers();
            let spawned = Command::new(UDHCPC)
                .args(["-f", "-i", &card.name, "-t", "3", "-T", "2", "-A", "5", "-x", &format!("hostname:{hostname}"), "-s", LEASE_SCRIPT])
                .env("EMBRA_METRIC", cards::route_metric(card).to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            match spawned {
                Ok(child) => {
                    let pid = child.id();
                    list.push(pid);
                    self.children.insert(pid, child);
                    Some(pid)
                }
                Err(e) => {
                    warn!("{}: {UDHCPC} could not be started: {e}", card.name);
                    None
                }
            }
        }

        fn client_gone(&mut self, pid: u32) -> bool {
            let Some(child) = self.children.get_mut(&pid) else { return true };
            match child.try_wait() {
                Ok(None) => false,
                // Exited, or no longer this process's to wait for.
                Ok(Some(_)) | Err(_) => {
                    self.drop_child(pid);
                    true
                }
            }
        }

        fn renew(&mut self, pid: u32) {
            self.signal(pid, libc::SIGUSR1);
        }

        fn release_and_ask(&mut self, pid: u32) {
            // The client takes its signals in order: the release runs the
            // script's `deconfig`, the second one starts the discovery.
            self.signal(pid, libc::SIGUSR2);
            self.signal(pid, libc::SIGUSR1);
        }

        fn stop_client(&mut self, pid: u32) {
            self.signal(pid, libc::SIGTERM);
            if let Some(child) = self.children.get_mut(&pid) {
                let mut exited = false;
                for _ in 0..10 {
                    if !matches!(child.try_wait(), Ok(None)) {
                        exited = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                if !exited {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            self.drop_child(pid);
        }

        fn leases(&mut self) -> Vec<Lease> {
            net_lease::read()
        }

        fn forget(&mut self, card: &str) {
            let _ = std::fs::remove_file(net_lease::lease_path(Path::new(net_lease::NET_DIR), card));
            // A stopped client runs no `deconfig`, so the card is cleared
            // here. With the helpers' lock held: this child's exit is this
            // function's to collect, not the reaper's.
            let _list = helpers();
            let _ = Command::new(IP).args(["-4", "addr", "flush", "dev", card]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }

        fn write_resolver(&mut self, text: &str) {
            write_whole(Path::new(net_lease::RESOLV_PATH), text);
        }

        fn write_status(&mut self, text: &str) {
            write_whole(&Path::new(net_lease::NET_DIR).join(net_lease::STATUS_FILE), text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embra_boot::net::Kind;

    fn card(name: &str, index: u32) -> Card {
        Card { name: name.into(), index, mac: format!("52:54:00:00:00:{index:02x}"), kind: Kind::Wired }
    }

    fn lease(card: &str, ip: &str, dns: &str, metric: u32) -> Lease {
        net_lease::parse(&format!("interface={card}\nip={ip}\nmask=24\nrouter=192.168.1.1\ndns={dns}\nmetric={metric}\nsource=dhcp\n")).unwrap()
    }

    /// A machine that does what a test tells it and writes down what it was
    /// asked.
    #[derive(Default)]
    struct Fake {
        cards: Vec<Card>,
        links: Vec<(String, Option<bool>)>,
        leases: Vec<Lease>,
        /// A lease that arrives once the records have been read this often.
        arrives: Option<(usize, Lease)>,
        reads: usize,
        next_pid: u32,
        started: Vec<(String, String, u32)>,
        exited: Vec<u32>,
        stopped: Vec<u32>,
        renewed: Vec<u32>,
        released: Vec<u32>,
        forgotten: Vec<String>,
        raised: Vec<String>,
        resolver: Vec<String>,
        status: Vec<String>,
    }

    impl Fake {
        fn link(&mut self, card: &str, up: Option<bool>) {
            self.links.retain(|(c, _)| c != card);
            self.links.push((card.to_string(), up));
        }
        fn client_of(&self, card: &str) -> Option<u32> {
            self.started.iter().rev().find(|(c, _, _)| c == card).map(|(_, _, pid)| *pid)
        }
    }

    impl Host for Fake {
        fn cards(&mut self) -> Vec<Card> {
            self.cards.clone()
        }
        fn carrier(&mut self, card: &str) -> Option<bool> {
            self.links.iter().find(|(c, _)| c == card).and_then(|(_, up)| *up)
        }
        fn raise(&mut self, card: &str) -> bool {
            self.raised.push(card.to_string());
            true
        }
        fn start_client(&mut self, card: &Card, hostname: &str) -> Option<u32> {
            self.next_pid += 1;
            let pid = 1000 + self.next_pid;
            self.started.push((card.name.clone(), hostname.to_string(), pid));
            Some(pid)
        }
        fn client_gone(&mut self, pid: u32) -> bool {
            self.exited.contains(&pid)
        }
        fn renew(&mut self, pid: u32) {
            self.renewed.push(pid);
        }
        fn release_and_ask(&mut self, pid: u32) {
            self.released.push(pid);
        }
        fn stop_client(&mut self, pid: u32) {
            self.stopped.push(pid);
        }
        fn leases(&mut self) -> Vec<Lease> {
            self.reads += 1;
            if let Some((after, lease)) = self.arrives.clone()
                && self.reads > after
            {
                self.leases.push(lease);
                self.arrives = None;
            }
            let mut all = self.leases.clone();
            all.sort_by_key(|a| a.metric);
            all
        }
        fn forget(&mut self, card: &str) {
            self.forgotten.push(card.to_string());
            self.leases.retain(|l| l.interface != card);
        }
        fn write_resolver(&mut self, text: &str) {
            self.resolver.push(text.to_string());
        }
        fn write_status(&mut self, text: &str) {
            self.status.push(text.to_string());
        }
    }

    /// A clock that moves only when it is slept on.
    struct Stepped {
        now: Instant,
        slept: Duration,
    }

    impl Stepped {
        fn new() -> Self {
            Stepped { now: Instant::now(), slept: Duration::ZERO }
        }
    }

    impl Pace for Stepped {
        fn now(&mut self) -> Instant {
            self.now
        }
        fn sleep(&mut self, d: Duration) {
            self.now += d;
            self.slept += d;
        }
    }

    #[test]
    fn the_boot_goes_on_at_the_first_lease_of_any_card() {
        // Two ports, the cable and its DHCP server on the second: the case
        // that left a machine on QEMU's static address on `eth0`.
        let mut host = Fake { cards: vec![card("eth0", 2), card("eth1", 3)], arrives: Some((3, lease("eth1", "192.168.1.42", "192.168.1.1", 103))), ..Fake::default() };
        let mut net = Net::new("embraos");
        let mut pace = Stepped::new();
        let summary = boot(&mut net, &mut host, &mut pace);
        assert_eq!(summary.leases.iter().map(|l| l.interface.as_str()).collect::<Vec<_>>(), ["eth1"]);
        assert_eq!(pace.slept, Duration::from_millis(300), "the boot went on when the lease came, not at its bound");
        // Both cards were brought up and asked, each once, under the name.
        assert_eq!(host.raised, ["eth0", "eth1"]);
        assert_eq!(host.started.iter().map(|(c, h, _)| (c.as_str(), h.as_str())).collect::<Vec<_>>(), [("eth0", "embraos"), ("eth1", "embraos")]);
        assert_eq!(host.resolver.last().map(String::as_str), Some("nameserver 192.168.1.1\n"));
        assert!(host.stopped.is_empty(), "the card without a lease goes on asking");
    }

    #[test]
    fn a_boot_without_a_lease_waits_its_ten_seconds_and_stops_no_client() {
        let mut host = Fake { cards: vec![card("eth0", 2)], ..Fake::default() };
        let mut net = Net::new("embraos");
        let mut pace = Stepped::new();
        let summary = boot(&mut net, &mut host, &mut pace);
        assert!(summary.leases.is_empty());
        assert_eq!(summary.cards, 1);
        assert_eq!(pace.slept, BOOT_WAIT);
        assert!(host.stopped.is_empty(), "the client is never killed for being slow");
        assert!(host.forgotten.is_empty(), "and no address is put in a lease's place");
        // Ten minutes later the same client is still the card's: a lease
        // that comes then is taken up.
        let asked = host.client_of("eth0");
        for _ in 0..300 {
            pace.sleep(KEEPER_TICK);
            net.tick(&mut host, pace.now);
        }
        assert_eq!(host.client_of("eth0"), asked);
        host.leases.push(lease("eth0", "192.168.1.42", "192.168.1.1", 102));
        assert_eq!(net.tick(&mut host, pace.now).leases.len(), 1);
    }

    #[test]
    fn a_machine_without_a_card_waits_five_seconds_for_one() {
        let mut host = Fake::default();
        let mut net = Net::new("embraos");
        let mut pace = Stepped::new();
        let summary = boot(&mut net, &mut host, &mut pace);
        assert_eq!((summary.cards, summary.leases.len()), (0, 0));
        assert_eq!(pace.slept, CARD_WAIT);
        // The resolver file is written once, empty: nothing stands in.
        assert_eq!(host.resolver, [""]);
    }

    #[test]
    fn a_card_that_appears_later_is_given_a_client() {
        let mut host = Fake::default();
        let mut net = Net::new("embraos");
        let t = Instant::now();
        assert_eq!(net.tick(&mut host, t).cards, 0);
        // A USB card is plugged in.
        host.cards.push(card("usb0", 7));
        assert_eq!(net.tick(&mut host, t + KEEPER_TICK).cards, 1);
        assert_eq!(host.raised, ["usb0"]);
        assert_eq!(host.started.len(), 1);
        // The next look starts nothing more.
        net.tick(&mut host, t + KEEPER_TICK * 2);
        assert_eq!(host.started.len(), 1);
    }

    #[test]
    fn a_card_that_goes_loses_its_client_and_its_record() {
        let mut host = Fake { cards: vec![card("eth0", 2), card("usb0", 7)], leases: vec![lease("usb0", "192.168.1.42", "192.168.1.1", 107)], ..Fake::default() };
        let mut net = Net::new("embraos");
        let t = Instant::now();
        assert_eq!(net.tick(&mut host, t).leases.len(), 1);
        let pid = host.client_of("usb0").unwrap();
        host.cards.retain(|c| c.name != "usb0");
        let summary = net.tick(&mut host, t + KEEPER_TICK);
        assert_eq!((summary.cards, summary.leases.len()), (1, 0));
        assert_eq!(host.stopped, [pid]);
        assert_eq!(host.forgotten, ["usb0"]);
        assert_eq!(host.resolver.last().map(String::as_str), Some(""), "its resolvers go with it");
        // Put back, it is a new card with a new index and is asked again.
        host.cards.push(card("usb0", 9));
        net.tick(&mut host, t + KEEPER_TICK * 2);
        assert_eq!(host.started.iter().filter(|(c, _, _)| c == "usb0").count(), 2);
    }

    #[test]
    fn a_short_break_renews_in_place_and_a_long_one_asks_again() {
        let mut host = Fake { cards: vec![card("eth0", 2)], leases: vec![lease("eth0", "192.168.1.42", "192.168.1.1", 102)], ..Fake::default() };
        host.link("eth0", Some(true));
        let mut net = Net::new("embraos");
        let t = Instant::now();
        net.tick(&mut host, t);
        let pid = host.client_of("eth0").unwrap();
        // A cable pulled for four seconds.
        host.link("eth0", Some(false));
        net.tick(&mut host, t + Duration::from_secs(2));
        host.link("eth0", Some(true));
        net.tick(&mut host, t + Duration::from_secs(6));
        assert_eq!((host.renewed.as_slice(), host.released.as_slice()), ([pid].as_slice(), [].as_slice()));
        // A cable pulled for a minute: the network may be another one.
        host.link("eth0", Some(false));
        net.tick(&mut host, t + Duration::from_secs(8));
        host.link("eth0", Some(true));
        net.tick(&mut host, t + Duration::from_secs(70));
        assert_eq!((host.renewed.as_slice(), host.released.as_slice()), ([pid].as_slice(), [pid].as_slice()));
        // A link that comes up on a card without a lease signals nobody:
        // its client is already asking every few seconds.
        let mut host = Fake { cards: vec![card("eth0", 2)], ..Fake::default() };
        host.link("eth0", Some(false));
        let mut net = Net::new("embraos");
        net.tick(&mut host, t);
        host.link("eth0", Some(true));
        net.tick(&mut host, t + Duration::from_secs(40));
        assert!(host.renewed.is_empty() && host.released.is_empty());
    }

    #[test]
    fn the_client_is_started_again_when_it_exits() {
        let mut host = Fake { cards: vec![card("eth0", 2)], ..Fake::default() };
        let mut net = Net::new("embraos");
        let t = Instant::now();
        net.tick(&mut host, t);
        let first = host.client_of("eth0").unwrap();
        host.exited.push(first);
        // Not at once: a client that cannot run is not started in a loop.
        net.tick(&mut host, t + Duration::from_secs(2));
        assert_eq!(host.started.len(), 1);
        assert!(host.status.last().unwrap().contains("note=the DHCP client is not running"));
        net.tick(&mut host, t + Duration::from_secs(6));
        assert_eq!(host.started.len(), 2);
        assert_ne!(host.client_of("eth0").unwrap(), first);
        assert!(host.status.last().unwrap().contains("note=\n"));
    }

    #[test]
    fn the_resolver_file_follows_the_card_with_the_best_route() {
        let mut host = Fake {
            cards: vec![card("eth0", 2), card("eth1", 3)],
            leases: vec![lease("eth1", "192.168.7.9", "192.168.7.1", 103)],
            ..Fake::default()
        };
        let mut net = Net::new("embraos");
        let t = Instant::now();
        net.tick(&mut host, t);
        assert_eq!(host.resolver, ["nameserver 192.168.7.1\n"]);
        // Nothing changed: nothing is written again.
        net.tick(&mut host, t + KEEPER_TICK);
        assert_eq!(host.resolver.len(), 1);
        // The first card gets its lease: its route is the better one, and
        // its resolver alone is in the file.
        host.leases.push(lease("eth0", "192.168.1.42", "192.168.1.1", 102));
        let summary = net.tick(&mut host, t + KEEPER_TICK * 2);
        assert_eq!(summary.leases.iter().map(|l| l.interface.as_str()).collect::<Vec<_>>(), ["eth0", "eth1"]);
        assert_eq!(host.resolver.last().map(String::as_str), Some("nameserver 192.168.1.1\n"));
        // A record of a card that is not there is nobody's.
        host.leases.push(lease("eth9", "10.9.9.9", "10.9.9.1", 1));
        assert_eq!(net.tick(&mut host, t + KEEPER_TICK * 3).leases.len(), 2);
        assert_eq!(host.resolver.last().map(String::as_str), Some("nameserver 192.168.1.1\n"));
    }

    #[test]
    fn the_status_says_each_cards_link() {
        let mut host = Fake { cards: vec![card("eth0", 2), card("eth1", 3)], ..Fake::default() };
        host.link("eth1", Some(true));
        let mut net = Net::new("embraos");
        let summary = net.tick(&mut host, Instant::now());
        assert!(summary.any_link);
        let status = net_lease::parse_status(host.status.last().unwrap());
        assert_eq!(status.iter().map(|c| (c.name.as_str(), c.link)).collect::<Vec<_>>(), [("eth0", None), ("eth1", Some(true))]);
    }
}
