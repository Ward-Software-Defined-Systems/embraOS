//! The clock at boot.
//!
//! The kernel takes the time from the machine's hardware clock (the CMOS
//! clock on x86, read as UTC; a hypervisor's own on a VM), and until this
//! module nothing in the OS corrected it. A board whose clock battery ran
//! down boots at its firmware's default date — 2018-01-01 on the bare-metal
//! rig of 2026-11-11 — every certificate is then "not valid yet", and a
//! first boot dates the OS's own certificate authority from that clock.
//!
//! Two steps, both before any service:
//!
//! 1. The floor ([`apply_floor`], first thing in `main`). A clock earlier
//!    than the image's build time (`BUILT` in `/etc/embraos-release`) is
//!    certainly wrong and is set to it. It needs no network and is said on
//!    both consoles; it makes the clock sane, not right.
//! 2. The fetch ([`sync_at_boot`], after the network is up and before a
//!    maintenance job, the database and the trust service, which makes the
//!    CA on a first boot). One SNTP exchange (RFC 4330) with
//!    [`DEFAULT_TIME_SERVER`], or with `embra.ntp=<host>[:<port>]`;
//!    `embra.ntp=off` asks nobody. The boot waits [`FETCH_WAIT`] at most. An
//!    answer sets the clock, and the hardware clock when that was a second
//!    or more off, so the next boot starts right without a network.
//!
//! The outcome is `/run/embra/clock-status` (`embra_boot::clock`): the
//! banner names a clock that is still the build time ([`banner_lines`]),
//! and the brain reads the record for a certificate date error's hint.
//!
//! The clock is set at boot and at no other time: nothing here runs while
//! the services do, so no timestamp they write ever steps backwards.
//!
//! PID 1 only. In development embrad never touches the host's clock: the
//! machine is behind [`Machine`], and the tests pass a stand-in.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use embra_boot::clock::{format_utc, CivilTime, ClockStatus, Rtc, Sync};
use embra_boot::TimeServer;
use tracing::{info, warn};

/// The time server asked when the kernel line names none: Cloudflare's
/// public time service, one anycast name, free to use by its own
/// documentation (developers.cloudflare.com/time-services/ntp/usage/).
///
/// Not a `pool.ntp.org` name: the NTP Pool's rules for vendors
/// (ntppool.org/vendors.html) forbid its zone names as the default of a
/// distributed system or appliance without a vendor zone of its own, and
/// embraOS has none. With one, this constant becomes that zone's name
/// (`0.<vendor>.pool.ntp.org`) and nothing else changes.
pub const DEFAULT_TIME_SERVER: &str = "time.cloudflare.com";

/// How long the boot waits for the time, the name lookup included. An
/// answer takes a fraction of a second; the wait is spent only when nothing
/// answers.
pub const FETCH_WAIT: Duration = Duration::from_secs(5);

/// How long one address has to answer before the next is asked.
const REPLY_WAIT: Duration = Duration::from_millis(1500);

/// How many of a name's addresses are asked, one request each: the boot
/// sends three packets at most.
const MAX_ADDRESSES: usize = 3;

/// The hardware clock keeps whole seconds: a clock that was less than this
/// far off is not worth a write.
const RTC_WRITE_FROM_SECONDS: f64 = 1.0;

/// The hardware clock, as devtmpfs names the first one.
const RTC_DEVICE: &str = "/dev/rtc0";

/// A time: seconds after the epoch, and nanoseconds into the second.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp {
    pub secs: i64,
    pub nanos: u32,
}

impl Timestamp {
    pub fn from_system(t: SystemTime) -> Timestamp {
        match t.duration_since(UNIX_EPOCH) {
            Ok(d) => Timestamp { secs: d.as_secs() as i64, nanos: d.subsec_nanos() },
            // Before 1970: the seconds count down, the nanoseconds still up.
            Err(e) => {
                let d = e.duration();
                match d.subsec_nanos() {
                    0 => Timestamp { secs: -(d.as_secs() as i64), nanos: 0 },
                    n => Timestamp { secs: -(d.as_secs() as i64) - 1, nanos: 1_000_000_000 - n },
                }
            }
        }
    }

    pub fn plus(self, d: Duration) -> Timestamp {
        let nanos = u64::from(self.nanos) + u64::from(d.subsec_nanos());
        Timestamp { secs: self.secs + d.as_secs() as i64 + (nanos / 1_000_000_000) as i64, nanos: (nanos % 1_000_000_000) as u32 }
    }

    /// `self` minus `earlier`, in seconds.
    pub fn seconds_since(self, earlier: Timestamp) -> f64 {
        (self.secs - earlier.secs) as f64 + (f64::from(self.nanos) - f64::from(earlier.nanos)) / 1e9
    }
}

/// What this module asks of the machine: its clock and its hardware clock.
pub trait Machine {
    fn now(&self) -> Timestamp;
    fn set(&self, to: Timestamp) -> io::Result<()>;
    /// Set the hardware clock to `unix_secs`, in UTC.
    fn write_rtc(&self, unix_secs: i64) -> Rtc;
}

/// The machine embrad is PID 1 of.
pub struct ThisMachine;

impl Machine for ThisMachine {
    fn now(&self) -> Timestamp {
        Timestamp::from_system(SystemTime::now())
    }

    #[cfg(target_os = "linux")]
    fn set(&self, to: Timestamp) -> io::Result<()> {
        // SAFETY: a timespec is plain integers, and all zeroes is one (a
        // literal would not build where libc pads the struct).
        let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
        // The fields' own types, unnamed: libc marks `time_t` deprecated
        // under musl, which the image is built for.
        ts.tv_sec = to.secs as _;
        ts.tv_nsec = to.nanos as _;
        // SAFETY: clock_settime(2) reads the timespec it is handed and
        // nothing else; a refusal is its return value.
        if unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    }

    #[cfg(not(target_os = "linux"))]
    fn set(&self, _to: Timestamp) -> io::Result<()> {
        Err(io::Error::other("the clock is set on Linux only"))
    }

    #[cfg(target_os = "linux")]
    fn write_rtc(&self, unix_secs: i64) -> Rtc {
        use std::os::unix::io::AsRawFd;
        let device = match std::fs::File::open(RTC_DEVICE) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Rtc::Absent,
            Err(e) => return Rtc::Failed(format!("{RTC_DEVICE}: {e}")),
        };
        let fields = rtc_fields(unix_secs);
        // SAFETY: RTC_SET_TIME reads one `struct rtc_time` from the pointer,
        // which `RtcTime` is, field for field; the descriptor is open.
        if unsafe { libc::ioctl(device.as_raw_fd(), RTC_SET_TIME as _, &fields) } == 0 {
            Rtc::Written
        } else {
            Rtc::Failed(format!("{RTC_DEVICE}: {}", io::Error::last_os_error()))
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn write_rtc(&self, _unix_secs: i64) -> Rtc {
        Rtc::Absent
    }
}

/// `struct rtc_time` of `<linux/rtc.h>`: nine `int`s, the first six read by
/// every driver.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RtcTime {
    tm_sec: libc::c_int,
    tm_min: libc::c_int,
    tm_hour: libc::c_int,
    tm_mday: libc::c_int,
    /// 0 to 11.
    tm_mon: libc::c_int,
    /// Years since 1900.
    tm_year: libc::c_int,
    tm_wday: libc::c_int,
    tm_yday: libc::c_int,
    tm_isdst: libc::c_int,
}

/// `RTC_SET_TIME`, `_IOW('p', 0x0a, struct rtc_time)` as x86_64 and aarch64
/// encode an ioctl; `libc` carries none of the RTC ioctls.
const RTC_SET_TIME: libc::c_ulong = 0x4024_700a;

/// The hardware clock's fields for a time, in UTC: the kernel reads the
/// clock back as UTC at the next boot.
fn rtc_fields(unix_secs: i64) -> RtcTime {
    let t = CivilTime::from_unix(unix_secs);
    RtcTime {
        tm_sec: t.second as libc::c_int,
        tm_min: t.minute as libc::c_int,
        tm_hour: t.hour as libc::c_int,
        tm_mday: t.day as libc::c_int,
        tm_mon: t.month as libc::c_int - 1,
        tm_year: (t.year - 1900) as libc::c_int,
        tm_wday: t.weekday() as libc::c_int,
        tm_yday: t.day_of_year() as libc::c_int,
        tm_isdst: 0,
    }
}

/// Step 1. A clock earlier than the image's build time cannot be right: it
/// is set to the build time, and what it read is returned. `None` when the
/// clock is not earlier, when the release stamp gave no build time, or when
/// the clock could not be set (said in the log).
pub fn apply_floor(machine: &impl Machine, built: Option<i64>) -> Option<i64> {
    let built = built?;
    let was = machine.now().secs;
    if was >= built {
        return None;
    }
    match machine.set(Timestamp { secs: built, nanos: 0 }) {
        Ok(()) => {
            warn!(
                "clock: the hardware clock read {}, earlier than this image was built: the clock is set to the build time, {}, until a time server answers",
                format_utc(was),
                format_utc(built)
            );
            Some(was)
        }
        Err(e) => {
            warn!("clock: the hardware clock read {}, earlier than this image was built ({}), and the clock could not be set: {e}", format_utc(was), format_utc(built));
            None
        }
    }
}

/// SNTP (RFC 4330): the request, and what is made of an answer.
pub(crate) mod sntp {
    use super::Timestamp;
    use std::fmt;

    /// An NTP packet without extensions.
    pub const PACKET: usize = 48;
    /// Leap indicator 0, version 4, mode 3 (client).
    const CLIENT_V4: u8 = 0x23;
    const MODE_SERVER: u8 = 4;
    /// Seconds from 1900-01-01, where NTP counts from, to the epoch.
    const NTP_TO_UNIX: i64 = 2_208_988_800;

    /// The request: the header byte and a nonce where the transmit time
    /// goes. Nothing of this machine is in it, its clock included (RFC 4330
    /// lets a client send any value there; the server returns it as the
    /// answer's origin, which is how an answer is told from a stray packet).
    pub fn request(nonce: u64) -> [u8; PACKET] {
        let mut packet = [0u8; PACKET];
        packet[0] = CLIENT_V4;
        packet[40..48].copy_from_slice(&nonce.to_be_bytes());
        packet
    }

    /// Why a packet set no clock.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum Refusal {
        /// Not an answer to this request: left alone, the wait goes on.
        NotOurs(&'static str),
        /// Stratum 0: the server declines, with its four-letter code
        /// (`RATE`, `DENY`, `RSTR`). No other address is asked this boot.
        Kiss(String),
        /// Leap indicator 3: the server's own clock is not synchronized.
        Unsynchronized,
        Stratum(u8),
        NoTime,
    }

    impl fmt::Display for Refusal {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Refusal::NotOurs(why) => write!(f, "a packet that is {why}"),
                Refusal::Kiss(code) => write!(f, "the server declined the request (kiss code {code})"),
                Refusal::Unsynchronized => f.write_str("the server says its own clock is not synchronized"),
                Refusal::Stratum(n) => write!(f, "the server's stratum is {n}, which no synchronized server has"),
                Refusal::NoTime => f.write_str("the server's answer carries no time"),
            }
        }
    }

    /// The two times of an answer that matter, by the server's clock.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Reply {
        pub stratum: u8,
        /// When the request arrived.
        pub receive: Timestamp,
        /// When the answer left.
        pub transmit: Timestamp,
    }

    /// NTP seconds and fraction as a time. NTP's 32-bit seconds wrap in
    /// 2036; RFC 4330 §3 tells the eras apart by the top bit: set, the time
    /// is before the wrap (1968 to 2036), clear, after it (2036 to 2104).
    pub fn unix_from_ntp(seconds: u32, fraction: u32) -> Timestamp {
        let since_1900 = if seconds & 0x8000_0000 != 0 { i64::from(seconds) } else { i64::from(seconds) + (1 << 32) };
        Timestamp { secs: since_1900 - NTP_TO_UNIX, nanos: ((u64::from(fraction) * 1_000_000_000) >> 32) as u32 }
    }

    pub fn parse(packet: &[u8], nonce: u64) -> Result<Reply, Refusal> {
        if packet.len() < PACKET {
            return Err(Refusal::NotOurs("shorter than an NTP packet"));
        }
        let (leap, version, mode) = (packet[0] >> 6, (packet[0] >> 3) & 7, packet[0] & 7);
        if mode != MODE_SERVER || !(3..=4).contains(&version) {
            return Err(Refusal::NotOurs("not a server's answer"));
        }
        if packet[24..32] != nonce.to_be_bytes() {
            return Err(Refusal::NotOurs("not an answer to this request"));
        }
        let word = |at: usize| u32::from_be_bytes([packet[at], packet[at + 1], packet[at + 2], packet[at + 3]]);
        let stratum = packet[1];
        if stratum == 0 {
            let code = packet[12..16].iter().map(|&b| if b.is_ascii_graphic() { char::from(b) } else { '?' }).collect();
            return Err(Refusal::Kiss(code));
        }
        if leap == 3 {
            return Err(Refusal::Unsynchronized);
        }
        if stratum > 15 {
            return Err(Refusal::Stratum(stratum));
        }
        if (word(40), word(44)) == (0, 0) {
            return Err(Refusal::NoTime);
        }
        Ok(Reply { stratum, receive: unix_from_ntp(word(32), word(36)), transmit: unix_from_ntp(word(40), word(44)) })
    }
}

/// A server's time, and when on this machine's own steady clock it held.
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    /// The server's time at `at`.
    pub time: Timestamp,
    pub at: Instant,
    pub address: SocketAddr,
    pub stratum: u8,
    pub round_trip: Duration,
}

/// The server's time at the moment its answer arrived: its transmit time
/// plus the way back, taken as half of the round trip that the server did
/// not spend holding the request. The round trip is measured on the steady
/// clock, so the clock being corrected plays no part in it.
fn sample(reply: sntp::Reply, sent: Instant, at: Instant, address: SocketAddr) -> Sample {
    let round_trip = at.saturating_duration_since(sent);
    let held = reply.transmit.seconds_since(reply.receive).clamp(0.0, round_trip.as_secs_f64());
    let way_back = Duration::from_secs_f64((round_trip.as_secs_f64() - held) / 2.0);
    Sample { time: reply.transmit.plus(way_back), at, address, stratum: reply.stratum, round_trip }
}

/// A wait as a line names it: `5 s`, `1.5 s`.
fn seconds(wait: Duration) -> String {
    format!("{} s", wait.as_secs_f32())
}

/// An exchange that set no clock: why, and whether another address may be asked.
#[derive(Debug, PartialEq, Eq)]
struct Miss {
    reason: String,
    stop: bool,
}

impl Miss {
    fn next(reason: impl Into<String>) -> Miss {
        Miss { reason: reason.into(), stop: false }
    }
}

/// A nonce nobody off the path can guess. std seeds every `RandomState`
/// from the kernel's random source; no file is read.
fn nonce() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new().build_hasher().finish()
}

/// One request to one address, and its answer within `wait`. The socket is
/// connected, so the kernel passes up packets from that address alone, and
/// a packet that does not carry the nonce is left alone.
fn exchange(address: SocketAddr, wait: Duration, nonce: u64) -> Result<Sample, Miss> {
    let local: SocketAddr = if address.is_ipv4() { (Ipv4Addr::UNSPECIFIED, 0).into() } else { (Ipv6Addr::UNSPECIFIED, 0).into() };
    let socket = UdpSocket::bind(local).map_err(|e| Miss::next(format!("no socket: {e}")))?;
    socket.connect(address).map_err(|e| Miss::next(e.to_string()))?;
    let sent = Instant::now();
    socket.send(&sntp::request(nonce)).map_err(|e| Miss::next(e.to_string()))?;
    let deadline = sent + wait;
    let no_answer = || Miss::next(format!("no answer within {}", seconds(wait)));
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(no_answer());
        }
        socket.set_read_timeout(Some(left)).map_err(|e| Miss::next(e.to_string()))?;
        let mut buf = [0u8; 512];
        match socket.recv(&mut buf) {
            Ok(n) => {
                let at = Instant::now();
                match sntp::parse(&buf[..n], nonce) {
                    Ok(reply) => return Ok(sample(reply, sent, at, address)),
                    Err(sntp::Refusal::NotOurs(_)) => continue,
                    Err(refusal @ sntp::Refusal::Kiss(_)) => return Err(Miss { reason: refusal.to_string(), stop: true }),
                    Err(refusal) => return Err(Miss::next(refusal.to_string())),
                }
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => return Err(no_answer()),
            // A port nobody listens on answers with an ICMP error.
            Err(e) => return Err(Miss::next(e.to_string())),
        }
    }
}

/// The name's addresses, IPv4 first (the lease is IPv4), asked one after
/// the other until one answers.
fn fetch(host: &str, port: u16, reply_wait: Duration) -> Result<Sample, String> {
    let mut addresses: Vec<SocketAddr> = (host, port).to_socket_addrs().map_err(|e| format!("the name could not be resolved: {e}"))?.collect();
    addresses.sort_by_key(|a| !a.is_ipv4());
    addresses.dedup();
    addresses.truncate(MAX_ADDRESSES);
    let mut last = "the name resolved to no address".to_string();
    for address in addresses {
        match exchange(address, reply_wait, nonce()) {
            Ok(sample) => return Ok(sample),
            Err(miss) => {
                last = format!("{address}: {}", miss.reason);
                if miss.stop {
                    break;
                }
            }
        }
    }
    Err(last)
}

/// [`fetch`] with the boot's patience. The lookup has the resolver's own
/// timeouts, longer than `wait`, so it runs on a thread of its own: past
/// `wait` the boot goes on, and a late answer sets nothing.
fn fetch_within(host: &str, port: u16, wait: Duration) -> Result<Sample, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let name = host.to_string();
    let spawned = std::thread::Builder::new().name("clock-fetch".into()).spawn(move || {
        let _ = tx.send(fetch(&name, port, REPLY_WAIT));
    });
    if let Err(e) = spawned {
        return Err(format!("no thread for the request: {e}"));
    }
    match rx.recv_timeout(wait) {
        Ok(result) => result,
        Err(_) => Err(format!("no answer within {}", seconds(wait))),
    }
}

/// Why the network cannot carry the request, before it is tried: no
/// interface came up (no lease record exists), or the interface has no
/// link. `None` when the request is worth its wait.
pub fn network_missing(lease_recorded: bool, carrier: Option<&str>) -> Option<&'static str> {
    if !lease_recorded {
        return Some("no network interface came up");
    }
    match carrier.map(str::trim) {
        Some("0") => Some("the network interface has no link"),
        _ => None,
    }
}

/// How the kernel line's choice is named in a line for the operator, and
/// what is asked: `None` for `embra.ntp=off`.
fn target(server: TimeServer<'_>) -> Option<(String, String, u16)> {
    match server {
        TimeServer::Off => None,
        TimeServer::Default => Some((DEFAULT_TIME_SERVER.to_string(), DEFAULT_TIME_SERVER.to_string(), TimeServer::NTP_PORT)),
        TimeServer::Named { host, port } if port == TimeServer::NTP_PORT => Some((host.to_string(), host.to_string(), port)),
        TimeServer::Named { host, port } => Some((format!("{host}:{port}"), host.to_string(), port)),
    }
}

/// Step 2. The time from the server the kernel line names (or the default),
/// applied to the clock and, when that was a second or more off, to the
/// hardware clock. `network_missing` is why no request can leave, when that
/// is already known. Never an error: a boot without the time goes on, with
/// the reason in the record this returns.
pub fn sync_at_boot(machine: &impl Machine, server: TimeServer<'_>, floored_from: Option<i64>, built: Option<i64>, network_missing: Option<&str>) -> ClockStatus {
    let Some((named, host, port)) = target(server) else {
        info!("clock: embra.ntp=off, no time server is asked; the clock reads {}", format_utc(machine.now().secs));
        return ClockStatus { floored_from, built, sync: Sync::Off, rtc: Rtc::Unchanged };
    };
    let fetched = match network_missing {
        Some(why) => Err(why.to_string()),
        None => fetch_within(&host, port, FETCH_WAIT),
    };
    apply(machine, &named, fetched, floored_from, built)
}

/// What a fetch came to, applied: the clock set from a sample that is not
/// earlier than the image's build time, the hardware clock after it.
fn apply(machine: &impl Machine, server: &str, fetched: Result<Sample, String>, floored_from: Option<i64>, built: Option<i64>) -> ClockStatus {
    let failed = |reason: String| {
        let now = machine.now().secs;
        let guess = if floored_from.is_some() { " (the image's build time, not the real time)" } else { "" };
        warn!("clock: no time from {server}: {reason}; the clock stays at {}{guess}", format_utc(now));
        ClockStatus { floored_from, built, sync: Sync::Failed { server: server.to_string(), reason }, rtc: Rtc::Unchanged }
    };
    let sample = match fetched {
        Ok(sample) => sample,
        Err(reason) => return failed(reason),
    };
    let time = sample.time.plus(sample.at.elapsed());
    if let Some(built) = built
        && time.secs < built
    {
        return failed(format!("{} answered {}, earlier than this image was built", sample.address, format_utc(time.secs)));
    }
    let before = machine.now();
    if let Err(e) = machine.set(time) {
        return failed(format!("{} answered and the clock could not be set: {e}", sample.address));
    }
    let offset = time.seconds_since(before);
    let rtc = if floored_from.is_some() || offset.abs() >= RTC_WRITE_FROM_SECONDS { machine.write_rtc(time.secs) } else { Rtc::Unchanged };
    let hardware = match &rtc {
        Rtc::Written => "; the hardware clock is set from it".to_string(),
        Rtc::Unchanged => String::new(),
        Rtc::Absent => "; this machine has no hardware clock to set".to_string(),
        Rtc::Failed(e) => format!("; the hardware clock could not be set ({e})"),
    };
    info!(
        "clock: {} from {server} ({}, stratum {}, {:.0} ms away), corrected by {offset:+.3} s{hardware}",
        format_utc(time.secs),
        sample.address,
        sample.stratum,
        sample.round_trip.as_secs_f64() * 1000.0
    );
    ClockStatus { floored_from, built, sync: Sync::Synchronized { server: server.to_string(), address: sample.address.to_string(), offset, stratum: sample.stratum }, rtc }
}

/// The banner's lines about the clock: none on a clock a time server set
/// or nothing called into question, two when it is the image's build time
/// and nothing confirmed it.
pub fn banner_lines(status: Option<&ClockStatus>) -> Vec<String> {
    let Some(status) = status.filter(|s| s.is_a_guess()) else {
        return Vec::new();
    };
    let was = status.floored_from.map(format_utc).unwrap_or_default();
    let built = status.built.map(format_utc).unwrap_or_else(|| "its build time".into());
    let why = match &status.sync {
        Sync::Failed { server, reason } => format!("no time came from {server} ({reason})"),
        Sync::Off => "embra.ntp=off asks no time server".to_string(),
        Sync::Synchronized { .. } => return Vec::new(),
    };
    vec![
        format!("The clock is this image's build time, {built}, not the real time: the hardware clock read {was}, and {why}."),
        "Certificates are checked against it and will be refused on their dates. Set the date in the firmware setup (in UTC), or boot on a network where a time server answers (embra.ntp=<host>).".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// A machine whose clock and hardware clock record what is asked of them.
    struct Fake {
        now: Cell<Timestamp>,
        sets: RefCell<Vec<Timestamp>>,
        rtc_writes: RefCell<Vec<i64>>,
        rtc: Rtc,
        refuses_set: bool,
    }

    impl Fake {
        fn at(secs: i64) -> Fake {
            Fake { now: Cell::new(Timestamp { secs, nanos: 0 }), sets: RefCell::new(Vec::new()), rtc_writes: RefCell::new(Vec::new()), rtc: Rtc::Written, refuses_set: false }
        }
    }

    impl Machine for Fake {
        fn now(&self) -> Timestamp {
            self.now.get()
        }
        fn set(&self, to: Timestamp) -> io::Result<()> {
            if self.refuses_set {
                return Err(io::Error::from_raw_os_error(libc::EPERM));
            }
            self.sets.borrow_mut().push(to);
            self.now.set(to);
            Ok(())
        }
        fn write_rtc(&self, unix_secs: i64) -> Rtc {
            self.rtc_writes.borrow_mut().push(unix_secs);
            self.rtc.clone()
        }
    }

    /// The bare-metal run of 2026-11-11: the board woke in 2018.
    const WOKE: i64 = 1_514_764_803;
    /// 2026-11-12T02:58:42Z, a build time.
    const BUILT: i64 = 1_794_452_322;
    /// A real time after the build.
    const REAL: i64 = BUILT + 86_400;

    fn sample_at(secs: i64) -> Sample {
        Sample { time: Timestamp { secs, nanos: 250_000_000 }, at: Instant::now(), address: "162.159.200.1:123".parse().unwrap(), stratum: 3, round_trip: Duration::from_millis(18) }
    }

    #[test]
    fn a_clock_earlier_than_the_images_build_time_is_set_to_it_and_no_other_clock_is_touched() {
        let behind = Fake::at(WOKE);
        assert_eq!(apply_floor(&behind, Some(BUILT)), Some(WOKE), "what the clock read comes back");
        assert_eq!(*behind.sets.borrow(), [Timestamp { secs: BUILT, nanos: 0 }]);
        assert!(behind.rtc_writes.borrow().is_empty(), "a guess never reaches the hardware clock");

        for later in [BUILT, BUILT + 1, REAL] {
            let fine = Fake::at(later);
            assert_eq!(apply_floor(&fine, Some(BUILT)), None);
            assert!(fine.sets.borrow().is_empty(), "a clock at or after the build time is left alone");
        }
        let undated = Fake::at(WOKE);
        assert_eq!(apply_floor(&undated, None), None, "no build time, no floor");
        assert!(undated.sets.borrow().is_empty());
        let stuck = Fake { refuses_set: true, ..Fake::at(WOKE) };
        assert_eq!(apply_floor(&stuck, Some(BUILT)), None, "a clock that could not be set was not floored");
    }

    #[test]
    fn a_request_is_the_header_byte_and_a_nonce_and_nothing_of_this_machine() {
        let packet = sntp::request(0x0102_0304_0506_0708);
        assert_eq!(packet.len(), 48);
        assert_eq!(packet[0], 0x23, "no leap warning, version 4, mode 3 (client)");
        assert!(packet[1..40].iter().all(|&b| b == 0), "no stratum, no poll, no times of this machine");
        assert_eq!(packet[40..48], [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_ne!(nonce(), nonce(), "a nonce is not repeated");
    }

    /// A server's answer to `nonce`: stratum 2, the request received at
    /// `secs` and answered 2 ms later (NTP seconds, era 0).
    fn answer(nonce: u64, secs: i64) -> [u8; 48] {
        let mut p = [0u8; 48];
        p[0] = 0x24; // no leap warning, version 4, mode 4 (server)
        p[1] = 2;
        p[24..32].copy_from_slice(&nonce.to_be_bytes());
        let ntp = (secs + 2_208_988_800) as u32;
        p[32..36].copy_from_slice(&ntp.to_be_bytes());
        p[36..40].copy_from_slice(&0x4000_0000u32.to_be_bytes()); // .250
        p[40..44].copy_from_slice(&ntp.to_be_bytes());
        p[44..48].copy_from_slice(&0x4083_126Fu32.to_be_bytes()); // .252
        p
    }

    #[test]
    fn an_answer_is_read_only_when_it_echoes_the_request() {
        let reply = sntp::parse(&answer(7, REAL), 7).expect("the server's answer");
        assert_eq!(reply.stratum, 2);
        assert_eq!(reply.receive, Timestamp { secs: REAL, nanos: 250_000_000 });
        assert_eq!(reply.transmit.secs, REAL);
        assert!((reply.transmit.nanos as i64 - 252_000_000).abs() < 2, "{}", reply.transmit.nanos);

        assert_eq!(sntp::parse(&answer(8, REAL), 7), Err(sntp::Refusal::NotOurs("not an answer to this request")), "another request's answer, or a forged one");
        assert_eq!(sntp::parse(&answer(7, REAL)[..47], 7), Err(sntp::Refusal::NotOurs("shorter than an NTP packet")));
        let mut echo = answer(7, REAL);
        echo[0] = 0x23;
        assert_eq!(sntp::parse(&echo, 7), Err(sntp::Refusal::NotOurs("not a server's answer")), "a client packet is no answer");
        let mut old = answer(7, REAL);
        old[0] = 0x14; // version 2
        assert!(matches!(sntp::parse(&old, 7), Err(sntp::Refusal::NotOurs(_))));
        let mut v3 = answer(7, REAL);
        v3[0] = 0x1c; // version 3, mode 4
        assert!(sntp::parse(&v3, 7).is_ok(), "a version 3 server answers too");
    }

    #[test]
    fn a_kiss_of_death_an_unsynchronized_server_and_an_answer_without_a_time_set_no_clock() {
        let mut kiss = answer(7, REAL);
        kiss[1] = 0;
        kiss[12..16].copy_from_slice(b"RATE");
        assert_eq!(sntp::parse(&kiss, 7), Err(sntp::Refusal::Kiss("RATE".into())));
        assert_eq!(sntp::Refusal::Kiss("RATE".into()).to_string(), "the server declined the request (kiss code RATE)");
        kiss[12..16].copy_from_slice(&[0, 1, b'X', 0xff]);
        assert_eq!(sntp::parse(&kiss, 7), Err(sntp::Refusal::Kiss("??X?".into())), "a code is printed as letters or not at all");
        // The kiss counts only as an answer to this request: a forged one is a stray packet.
        assert!(matches!(sntp::parse(&kiss, 8), Err(sntp::Refusal::NotOurs(_))));

        let mut alarm = answer(7, REAL);
        alarm[0] = 0xe4; // leap indicator 3
        assert_eq!(sntp::parse(&alarm, 7), Err(sntp::Refusal::Unsynchronized));
        let mut deep = answer(7, REAL);
        deep[1] = 16;
        assert_eq!(sntp::parse(&deep, 7), Err(sntp::Refusal::Stratum(16)));
        let mut empty = answer(7, REAL);
        empty[40..48].fill(0);
        assert_eq!(sntp::parse(&empty, 7), Err(sntp::Refusal::NoTime));
    }

    #[test]
    fn ntp_seconds_are_read_in_the_era_their_top_bit_names() {
        // 2026-11-12T02:58:42Z is 4003441122 s after 1900: the top bit is set, era 0.
        assert_eq!(sntp::unix_from_ntp(4_003_441_122, 0), Timestamp { secs: BUILT, nanos: 0 });
        assert_eq!(sntp::unix_from_ntp(4_003_441_122, 0x8000_0000).nanos, 500_000_000);
        // The last second before the wrap, 2036-02-07T06:28:15Z, and the first after it.
        assert_eq!(format_utc(sntp::unix_from_ntp(u32::MAX, 0).secs), "2036-02-07 06:28:15 UTC");
        assert_eq!(format_utc(sntp::unix_from_ntp(0, 0).secs), "2036-02-07 06:28:16 UTC");
        // 2040-01-01T00:00:00Z: 4417977600 s after 1900, sent as that minus 2^32.
        assert_eq!(format_utc(sntp::unix_from_ntp((4_417_977_600u64 - (1 << 32)) as u32, 0).secs), "2040-01-01 00:00:00 UTC");
    }

    #[test]
    fn the_servers_time_at_arrival_is_its_transmit_time_plus_half_the_way() {
        let sent = Instant::now();
        let reply = sntp::Reply { stratum: 2, receive: Timestamp { secs: REAL, nanos: 0 }, transmit: Timestamp { secs: REAL, nanos: 20_000_000 } };
        // 120 ms round trip, 20 ms of it inside the server: 50 ms each way.
        let s = sample(reply, sent, sent + Duration::from_millis(120), "192.0.2.1:123".parse().unwrap());
        assert_eq!(s.round_trip, Duration::from_millis(120));
        assert!((s.time.seconds_since(Timestamp { secs: REAL, nanos: 70_000_000 })).abs() < 1e-6, "{:?}", s.time);
        // A server whose two times make no sense (it held the request longer than the round trip, or its clock stepped) costs no more than the round trip.
        let odd = sntp::Reply { stratum: 2, receive: Timestamp { secs: REAL - 100, nanos: 0 }, transmit: Timestamp { secs: REAL, nanos: 0 } };
        assert_eq!(sample(odd, sent, sent + Duration::from_millis(120), "192.0.2.1:123".parse().unwrap()).time, Timestamp { secs: REAL, nanos: 0 });
        let back = sntp::Reply { stratum: 2, receive: Timestamp { secs: REAL + 5, nanos: 0 }, transmit: Timestamp { secs: REAL, nanos: 0 } };
        assert_eq!(sample(back, sent, sent + Duration::from_millis(120), "192.0.2.1:123".parse().unwrap()).time, Timestamp { secs: REAL, nanos: 60_000_000 });
    }

    #[test]
    fn a_time_adds_and_subtracts_across_a_second_and_before_the_epoch() {
        let t = Timestamp { secs: 10, nanos: 900_000_000 };
        assert_eq!(t.plus(Duration::from_millis(250)), Timestamp { secs: 11, nanos: 150_000_000 });
        assert!((Timestamp { secs: 11, nanos: 150_000_000 }.seconds_since(t) - 0.25).abs() < 1e-9);
        assert!((t.seconds_since(Timestamp { secs: 12, nanos: 0 }) + 1.1).abs() < 1e-9);
        assert_eq!(Timestamp::from_system(UNIX_EPOCH + Duration::new(5, 7)), Timestamp { secs: 5, nanos: 7 });
        assert_eq!(Timestamp::from_system(UNIX_EPOCH - Duration::new(1, 250_000_000)), Timestamp { secs: -2, nanos: 750_000_000 });
        assert_eq!(Timestamp::from_system(UNIX_EPOCH - Duration::from_secs(3)), Timestamp { secs: -3, nanos: 0 });
    }

    /// A UDP listener on the loopback that answers each request with what
    /// `reply` makes of its nonce (nothing for `None`), `times` times.
    fn server(times: usize, reply: impl Fn(u64) -> Vec<Vec<u8>> + Send + 'static) -> SocketAddr {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        std::thread::spawn(move || {
            for _ in 0..times {
                let mut buf = [0u8; 128];
                let Ok((n, from)) = socket.recv_from(&mut buf) else { return };
                assert_eq!(n, 48, "a request is one NTP packet");
                let nonce = u64::from_be_bytes(buf[40..48].try_into().unwrap());
                for packet in reply(nonce) {
                    let _ = socket.send_to(&packet, from);
                }
            }
        });
        address
    }

    #[test]
    fn an_exchange_takes_the_answer_to_its_request_and_leaves_a_stray_packet_alone() {
        // A forged answer first (another nonce, a time in 2018), then the server's own.
        let address = server(1, |nonce| vec![answer(nonce ^ 1, WOKE).to_vec(), b"not ntp".to_vec(), answer(nonce, REAL).to_vec()]);
        let s = exchange(address, Duration::from_secs(5), nonce()).expect("the answer");
        assert_eq!(s.address, address);
        assert_eq!(s.stratum, 2);
        assert_eq!(s.time.secs, REAL, "the time of the answer that echoed the request");
        assert!(s.round_trip < Duration::from_secs(5));
    }

    #[test]
    fn a_server_that_says_nothing_is_a_reason_after_the_wait_and_a_closed_port_at_once() {
        let silent = server(1, |_| Vec::new());
        let started = Instant::now();
        let miss = exchange(silent, Duration::from_millis(200), nonce()).unwrap_err();
        assert_eq!(miss, Miss { reason: "no answer within 0.2 s".into(), stop: false });
        assert!(started.elapsed() >= Duration::from_millis(200) && started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());

        // Only strays within the wait: still no answer, and no wait without end.
        let noisy = server(1, |nonce| (0..20).map(|_| answer(nonce ^ 1, REAL).to_vec()).collect());
        assert_eq!(exchange(noisy, Duration::from_millis(200), nonce()).unwrap_err().reason, "no answer within 0.2 s");

        let closed = {
            let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
            socket.local_addr().unwrap()
        };
        let miss = exchange(closed, Duration::from_secs(5), nonce()).unwrap_err();
        assert!(!miss.stop && !miss.reason.contains("no answer within"), "the refusal, not the wait: {miss:?}");
    }

    #[test]
    fn a_kiss_of_death_ends_the_asking_and_a_fetch_names_the_address_with_its_reason() {
        let kiss = server(1, |nonce| {
            let mut p = answer(nonce, REAL);
            p[1] = 0;
            p[12..16].copy_from_slice(b"DENY");
            vec![p.to_vec()]
        });
        let miss = exchange(kiss, Duration::from_secs(5), nonce()).unwrap_err();
        assert_eq!(miss, Miss { reason: "the server declined the request (kiss code DENY)".into(), stop: true });

        // The whole fetch, by host and port: the one address answers.
        let good = server(1, |nonce| vec![answer(nonce, REAL).to_vec()]);
        let s = fetch("127.0.0.1", good.port(), Duration::from_secs(5)).expect("the answer");
        assert_eq!(s.time.secs, REAL);
        // The kiss server answered its one request and is gone: its port refuses.
        let reason = fetch("127.0.0.1", kiss.port(), Duration::from_millis(100)).unwrap_err();
        assert!(reason.starts_with(&format!("127.0.0.1:{}: ", kiss.port())), "the address is named with its reason: {reason}");
        // An empty label is no host name: the resolver refuses it without asking anyone.
        assert!(fetch("bad..name", 123, Duration::from_millis(100)).unwrap_err().starts_with("the name could not be resolved: "));
    }

    #[test]
    fn the_boots_patience_is_a_bound_on_the_whole_fetch() {
        let silent = server(3, |_| Vec::new());
        let started = Instant::now();
        let reason = fetch_within("127.0.0.1", silent.port(), Duration::from_millis(300)).unwrap_err();
        assert_eq!(reason, "no answer within 0.3 s");
        assert!(started.elapsed() < REPLY_WAIT, "the boot went on before the address's own wait ended: {:?}", started.elapsed());
        let good = server(1, |nonce| vec![answer(nonce, REAL).to_vec()]);
        assert_eq!(fetch_within("127.0.0.1", good.port(), Duration::from_secs(5)).expect("the answer").time.secs, REAL);
    }

    #[test]
    fn an_answer_sets_the_clock_and_the_hardware_clock_only_when_that_was_a_second_off() {
        // The 2018 board: floored to the build time, then the server's time.
        let board = Fake::at(BUILT + 12);
        let status = apply(&board, "time.cloudflare.com", Ok(sample_at(REAL)), Some(WOKE), Some(BUILT));
        let set = *board.sets.borrow().last().expect("the clock was set");
        assert!(set.seconds_since(Timestamp { secs: REAL, nanos: 250_000_000 }) < 0.5 && set.secs >= REAL, "the sample's time plus what passed since: {set:?}");
        assert_eq!(*board.rtc_writes.borrow(), [set.secs], "the hardware clock gets the same time");
        assert_eq!(status.rtc, Rtc::Written);
        assert_eq!(status.floored_from, Some(WOKE));
        assert_eq!(status.built, Some(BUILT));
        let Sync::Synchronized { server, address, offset, stratum } = &status.sync else { panic!("{:?}", status.sync) };
        assert_eq!((server.as_str(), address.as_str(), *stratum), ("time.cloudflare.com", "162.159.200.1:123", 3));
        assert!((offset - (86_400.0 - 12.0 + 0.25)).abs() < 0.5, "{offset}");
        assert!(status.synchronized() && !status.is_a_guess());
        assert!(banner_lines(Some(&status)).is_empty(), "a clock a server set is not named in the banner");

        // A clock 200 ms off: set, and the hardware clock left alone.
        let close = Fake::at(REAL);
        let status = apply(&close, "time.cloudflare.com", Ok(sample_at(REAL)), None, Some(BUILT));
        assert_eq!(close.sets.borrow().len(), 1);
        assert!(close.rtc_writes.borrow().is_empty());
        assert_eq!(status.rtc, Rtc::Unchanged);

        // A clock ahead by an hour (a hardware clock kept in local time): back, and written.
        let ahead = Fake::at(REAL + 3_600);
        let status = apply(&ahead, "time.cloudflare.com", Ok(sample_at(REAL)), None, Some(BUILT));
        assert_eq!(ahead.rtc_writes.borrow().len(), 1);
        assert!(matches!(status.sync, Sync::Synchronized { offset, .. } if (offset + 3_600.0).abs() < 1.0));

        // A machine without a hardware clock, and one that refuses the write: the clock is set all the same.
        for rtc in [Rtc::Absent, Rtc::Failed("/dev/rtc0: Invalid argument".into())] {
            let m = Fake { rtc: rtc.clone(), ..Fake::at(REAL - 50) };
            let status = apply(&m, "time.cloudflare.com", Ok(sample_at(REAL)), None, None);
            assert!(status.synchronized());
            assert_eq!(status.rtc, rtc);
        }
    }

    #[test]
    fn no_answer_an_answer_before_the_build_time_and_a_clock_that_cannot_be_set_leave_the_reason() {
        let board = Fake::at(BUILT + 12);
        let status = apply(&board, "time.cloudflare.com", Err("the name could not be resolved: no address".into()), Some(WOKE), Some(BUILT));
        assert!(board.sets.borrow().is_empty() && board.rtc_writes.borrow().is_empty());
        assert_eq!(status.sync, Sync::Failed { server: "time.cloudflare.com".into(), reason: "the name could not be resolved: no address".into() });
        assert!(status.is_a_guess());

        // A server whose clock is behind this image's build cannot be right either.
        let lied = Fake::at(REAL);
        let status = apply(&lied, "ntp.example.org", Ok(sample_at(WOKE)), None, Some(BUILT));
        assert!(lied.sets.borrow().is_empty(), "the clock is not set from a time before the build");
        let Sync::Failed { reason, .. } = &status.sync else { panic!("{:?}", status.sync) };
        assert_eq!(reason, "162.159.200.1:123 answered 2018-01-01 00:00:03 UTC, earlier than this image was built");
        // Without a build time there is nothing to hold an answer against.
        assert!(apply(&Fake::at(REAL), "ntp.example.org", Ok(sample_at(WOKE)), None, None).synchronized());

        let stuck = Fake { refuses_set: true, ..Fake::at(REAL - 50) };
        let status = apply(&stuck, "time.cloudflare.com", Ok(sample_at(REAL)), None, Some(BUILT));
        assert!(matches!(&status.sync, Sync::Failed { reason, .. } if reason.starts_with("162.159.200.1:123 answered and the clock could not be set: ")), "{:?}", status.sync);
        assert!(stuck.rtc_writes.borrow().is_empty());
    }

    #[test]
    fn embra_ntp_off_asks_nobody_and_a_machine_without_a_network_is_not_made_to_wait() {
        let m = Fake::at(REAL);
        let started = Instant::now();
        let status = sync_at_boot(&m, TimeServer::Off, None, Some(BUILT), None);
        assert_eq!(status.sync, Sync::Off);
        assert!(m.sets.borrow().is_empty());

        let status = sync_at_boot(&m, TimeServer::Default, Some(WOKE), Some(BUILT), Some("no network interface came up"));
        assert_eq!(status.sync, Sync::Failed { server: "time.cloudflare.com".into(), reason: "no network interface came up".into() });
        let status = sync_at_boot(&m, TimeServer::Named { host: "10.0.2.2", port: 12300 }, None, Some(BUILT), Some("the network interface has no link"));
        assert_eq!(status.sync, Sync::Failed { server: "10.0.2.2:12300".into(), reason: "the network interface has no link".into() }, "a port of its own is named with the host");
        assert!(started.elapsed() < Duration::from_secs(1), "nothing was asked: {:?}", started.elapsed());

        assert_eq!(network_missing(false, None), Some("no network interface came up"));
        assert_eq!(network_missing(true, Some("0\n")), Some("the network interface has no link"));
        assert_eq!(network_missing(true, Some("1\n")), None);
        assert_eq!(network_missing(true, None), None, "a link that cannot be read is tried");
        assert_eq!(target(TimeServer::Default), Some(("time.cloudflare.com".into(), "time.cloudflare.com".into(), 123)));
        assert_eq!(target(TimeServer::Named { host: "ntp.example.org", port: 123 }), Some(("ntp.example.org".into(), "ntp.example.org".into(), 123)));
        assert_eq!(target(TimeServer::Off), None);
    }

    #[test]
    fn a_named_server_is_asked_and_its_answer_sets_the_clock() {
        let good = server(1, |nonce| vec![answer(nonce, REAL).to_vec()]);
        let m = Fake::at(BUILT + 3);
        let status = sync_at_boot(&m, TimeServer::Named { host: "127.0.0.1", port: good.port() }, Some(WOKE), Some(BUILT), None);
        assert!(status.synchronized(), "{:?}", status.sync);
        assert_eq!(m.sets.borrow().last().map(|t| t.secs), Some(REAL));
        assert_eq!(*m.rtc_writes.borrow(), [REAL]);
        assert!(matches!(&status.sync, Sync::Synchronized { server, .. } if *server == format!("127.0.0.1:{}", good.port())));
    }

    #[test]
    fn the_banner_names_a_clock_that_is_the_build_time_and_nothing_else() {
        assert!(banner_lines(None).is_empty(), "a development host has no record");
        let unconfirmed = ClockStatus { floored_from: None, built: Some(BUILT), sync: Sync::Failed { server: "time.cloudflare.com".into(), reason: format!("no answer within {}", seconds(FETCH_WAIT)) }, rtc: Rtc::Unchanged };
        assert!(banner_lines(Some(&unconfirmed)).is_empty(), "a hardware clock nothing called into question is not a fault");

        let guess = ClockStatus { floored_from: Some(WOKE), ..unconfirmed.clone() };
        let lines = banner_lines(Some(&guess));
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            "The clock is this image's build time, 2026-11-12 02:58:42 UTC, not the real time: the hardware clock read 2018-01-01 00:00:03 UTC, and no time came from time.cloudflare.com (no answer within 5 s)."
        );
        assert!(lines[1].contains("firmware setup (in UTC)") && lines[1].contains("embra.ntp=<host>"), "{}", lines[1]);
        let off = ClockStatus { sync: Sync::Off, ..guess };
        assert!(banner_lines(Some(&off))[0].ends_with("and embra.ntp=off asks no time server."));
    }

    #[test]
    fn the_hardware_clocks_ioctl_and_fields_are_the_kernels() {
        // <linux/rtc.h>: #define RTC_SET_TIME _IOW('p', 0x0a, struct rtc_time);
        // <asm-generic/ioctl.h>: dir << 30 | size << 16 | type << 8 | nr, _IOC_WRITE = 1.
        assert_eq!(std::mem::size_of::<RtcTime>(), 36, "nine ints");
        assert_eq!(RTC_SET_TIME, (1 << 30) | (36 << 16) | ((b'p' as libc::c_ulong) << 8) | 0x0a);
        // 2026-11-12T02:58:42Z, a Thursday, the 316th day of its year.
        assert_eq!(
            rtc_fields(BUILT),
            RtcTime { tm_sec: 42, tm_min: 58, tm_hour: 2, tm_mday: 12, tm_mon: 10, tm_year: 126, tm_wday: 4, tm_yday: 315, tm_isdst: 0 }
        );
        assert_eq!(rtc_fields(WOKE).tm_year, 118);
    }

    /// Run by hand, it needs the network and sets no clock:
    /// `cargo test -p embrad -- --ignored the_default_time_server_answers --nocapture`
    #[test]
    #[ignore = "asks the default time server over the network"]
    fn the_default_time_server_answers() {
        let s = fetch_within(DEFAULT_TIME_SERVER, TimeServer::NTP_PORT, FETCH_WAIT).expect("an answer from the default time server");
        let offset = s.time.plus(s.at.elapsed()).seconds_since(Timestamp::from_system(SystemTime::now()));
        println!(
            "{DEFAULT_TIME_SERVER} ({}, stratum {}, {:.0} ms away) says {}: this host's clock is {offset:+.3} s from it",
            s.address,
            s.stratum,
            s.round_trip.as_secs_f64() * 1000.0,
            format_utc(s.time.secs)
        );
        assert!((1..=15).contains(&s.stratum));
        assert!(s.time.secs > BUILT, "a time after this test was written: {}", format_utc(s.time.secs));
    }

    #[test]
    fn the_default_server_is_one_name_and_not_a_pool_zone() {
        // The NTP Pool's zone names are not a default without a vendor zone
        // (ntppool.org/vendors.html); the reason is on the constant.
        assert!(!DEFAULT_TIME_SERVER.ends_with("pool.ntp.org"));
        assert_eq!(TimeServer::parse(DEFAULT_TIME_SERVER), Some(TimeServer::Named { host: DEFAULT_TIME_SERVER, port: 123 }), "the default is a name the knob would take");
        assert!(REPLY_WAIT * MAX_ADDRESSES as u32 <= FETCH_WAIT, "every address has its wait inside the boot's patience");
        assert_eq!((seconds(FETCH_WAIT), seconds(REPLY_WAIT)), ("5 s".to_string(), "1.5 s".to_string()));
        // GRUB's knob list and both launchers name the default beside the knob.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for file in ["buildroot/board/embraos/grub.cfg.in", "scripts/run-qemu.sh", "scripts/run-qemu-aarch64.sh"] {
            let text = std::fs::read_to_string(root.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
            let named = text.lines().any(|l| l.contains("embra.ntp") && l.contains(DEFAULT_TIME_SERVER))
                || text.split("embra.ntp").skip(1).any(|after| after.lines().take(2).any(|l| l.contains(DEFAULT_TIME_SERVER)));
            assert!(named, "{file} names the default time server where it names embra.ntp");
        }
    }
}
