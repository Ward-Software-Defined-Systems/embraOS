//! `/network wifi`: the Wi-Fi networks in range, the one a card is on, and
//! the networks the operator stored.
//!
//! The brain stores a network (`/embra/state/network/wifi`, mode 0600,
//! `embra_boot::wifi`); embrad sees the file change, renders the
//! supplicant's configuration and has it read. What is in range and what a
//! card is on are asked of the supplicant's control socket, a datagram
//! socket a card under `/embra/ephemeral/net/wpa`. The access points are
//! read a page at a time (`BSS RANGE=`): one answer of the supplicant holds
//! 4096 bytes, and the list in one answer (`SCAN_RESULTS`) is cut there. `system_status` asks the
//! same socket one thing, the name of the network a card is on
//! ([`joined_network`]): the intelligence is shown that name and no other.
//!
//! A passphrase is typed as the command's last argument, after `--key`, and
//! runs to the end of the line: the consoles mask it while it is typed and
//! in what they echo (`embra_tools_core::mask_secret_argument`), a slash
//! command never reaches the model or a session's history, and no answer
//! here repeats it. It is not among the values masked in a tool's output
//! (`secrets::collect`): that mask matches exact strings, and a passphrase
//! that is a word would mark ordinary text.

use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use embra_boot::net::{self as cards, Kind};
use embra_boot::wifi::{self, Wifi, WifiNetwork};

use crate::network::{NetEnv, Reply};

pub(crate) const USAGE: &str = "Usage: /network wifi | /network wifi join <name> --key <passphrase> | /network wifi join <name> --open | /network wifi forget <name> | /network wifi country <CC>\n\
  /network wifi                     The Wi-Fi card, the network it is on, the stored networks and the networks in range (a scan: a few seconds).\n\
  /network wifi join <name> --key <passphrase>   Store a network and join it. The name may hold spaces; the passphrase is everything after --key, masked while it is typed. --hidden before --key for a network that does not announce its name.\n\
  /network wifi join <name> --open  A network without a passphrase.\n\
  /network wifi forget <name>       Take a stored network away.\n\
  /network wifi country <CC>        The regulatory domain, two letters (US, DE, JP): which channels the card may use.";

/// How long one answer of the supplicant is waited for.
/// How long a question waits for room in the supplicant's queue.
const SEND_WAIT: Duration = Duration::from_secs(1);
/// How long `system_status` waits for a card's supplicant to say what the
/// card is on. The tool is called often, by the model: a supplicant that
/// does not answer costs it this much and no more.
const STATUS_WAIT: Duration = Duration::from_secs(1);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Show,
    Join(WifiNetwork),
    Forget(Vec<u8>),
    Country(String),
    /// Not a form of the command, or a value that is refused: the sentence.
    Refused(String),
}

/// Whitespace-separated words of `text` as byte ranges.
fn words(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        match (c.is_whitespace(), start) {
            (false, None) => start = Some(i),
            (true, Some(s)) => {
                spans.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        spans.push((s, text.len()));
    }
    spans
}

fn name_of(text: &str) -> Result<Vec<u8>, String> {
    let name = text.trim();
    if name.is_empty() {
        return Err("no network is named".to_string());
    }
    wifi::ssid_typed(name).ok_or_else(|| "a network's name is 1 to 32 bytes, typed as it is shown (hex:<bytes> for a name that is no text)".to_string())
}

/// `join <name> [--hidden] --key <passphrase>` or `join <name> [--hidden]
/// --open`. The name is everything before the options, spaces included;
/// the passphrase is everything after the first `--key`, to the line's end.
fn parse_join(rest: &str) -> Action {
    let spans = words(rest);
    let word = |i: usize| spans.get(i).map(|&(s, e)| rest.get(s..e).unwrap_or(""));
    let key_at = (0..spans.len()).find(|&i| word(i) == Some("--key"));
    let (head, key) = match key_at {
        Some(i) => {
            let head = spans.get(i).map(|&(s, _)| rest.get(..s).unwrap_or("")).unwrap_or("");
            let key = spans.get(i + 1).map(|&(s, _)| rest.get(s..).unwrap_or("").trim_end()).unwrap_or("");
            (head, Some(key))
        }
        None => (rest, None),
    };
    // The options after the name, from the end: --hidden and --open.
    let mut name = head.trim_end();
    let (mut hidden, mut open) = (false, false);
    loop {
        if let Some(before) = name.strip_suffix("--hidden").filter(|b| b.is_empty() || b.ends_with(char::is_whitespace)) {
            hidden = true;
            name = before.trim_end();
        } else if let Some(before) = name.strip_suffix("--open").filter(|b| b.is_empty() || b.ends_with(char::is_whitespace)) {
            open = true;
            name = before.trim_end();
        } else {
            break;
        }
    }
    let ssid = match name_of(name) {
        Ok(ssid) => ssid,
        Err(reason) => return Action::Refused(format!("Not stored: {reason}.")),
    };
    match (key, open) {
        (Some(_), true) => Action::Refused("Not stored: a network is joined with --key <passphrase> or with --open, not both.".to_string()),
        (None, false) => Action::Refused(format!(
            "Not stored: {} needs its passphrase: /network wifi join {} --key <passphrase>, or --open for a network without one.",
            wifi::ssid_shown(&ssid),
            wifi::ssid_shown(&ssid)
        )),
        (None, true) => Action::Join(WifiNetwork { ssid, key: None, hidden }),
        (Some(key), false) => match wifi::passphrase_ok(key) {
            Ok(()) => Action::Join(WifiNetwork { ssid, key: Some(key.to_string()), hidden }),
            Err(reason) => Action::Refused(format!("Not stored: {reason}.")),
        },
    }
}

/// The words after `/network wifi`.
pub(crate) fn parse(rest: &str) -> Action {
    let rest = rest.trim_start();
    let (verb, tail) = match rest.split_once(char::is_whitespace) {
        Some((verb, tail)) => (verb, tail),
        None => (rest.trim_end(), ""),
    };
    match verb {
        "" | "show" if tail.trim().is_empty() => Action::Show,
        "join" => parse_join(tail),
        "forget" => match name_of(tail) {
            Ok(ssid) => Action::Forget(ssid),
            Err(reason) => Action::Refused(format!("Nothing changed: {reason}.")),
        },
        "country" => {
            let code = tail.trim().to_ascii_uppercase();
            if wifi::country_ok(&code) {
                Action::Country(code)
            } else {
                Action::Refused("Not stored: a country is its two letters (US, DE, JP).".to_string())
            }
        }
        _ => Action::Refused(USAGE.to_string()),
    }
}

/// A conversation with one card's supplicant. Its control socket is a
/// datagram socket named after the card; the answers come back to a socket
/// of our own, bound beside the control directory and removed at the end.
struct Ctrl {
    socket: UnixDatagram,
    own: PathBuf,
    /// How long one question waits for its answer.
    answer_wait: Duration,
    /// The supplicant sends us its events (`ATTACH`): it is told to stop.
    attached: bool,
    /// A question went unanswered. The protocol numbers no answer, so one
    /// that comes late would be read as the answer to the next question
    /// (seen with a radio that took half a minute to start a scan: the
    /// scan's `OK` was read as the list of networks, and the list as the
    /// card's state). Nothing more is asked on this socket.
    silent: bool,
}

/// The most pages of the supplicant's table that are read: its table holds
/// 200 access points at most (`bss_max_count`), six pages or so.
const PAGES_MAX: usize = 32;

/// What became of the scan a card was asked for.
enum Scan {
    /// It ended within the wait: the access points the card knows now.
    Ended(Vec<wifi::Seen>),
    /// It was asked for and has not ended yet.
    Running,
    /// It could not be asked for.
    NotAsked,
}

/// A timed call on the control socket came back with nothing, and not
/// because of the other side: it was interrupted, or its timeout was
/// reported. The caller's deadline decides whether the wait goes on.
fn waits_on(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted)
}

impl Ctrl {
    fn open(ctrl_dir: &Path, card: &str, answer_wait: Duration) -> std::io::Result<Ctrl> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let own = ctrl_dir.with_file_name(format!("wpa-client-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_file(&own);
        let socket = UnixDatagram::bind(&own)?;
        let ctrl = Ctrl { socket, own, answer_wait, attached: false, silent: false };
        ctrl.socket.connect(ctrl_dir.join(card))?;
        Ok(ctrl)
    }

    /// One datagram to the supplicant. A supplicant that reads nothing (it
    /// is inside a long call into the kernel) fills its queue, and a
    /// question must not wait for room in it without end: [`SEND_WAIT`] in
    /// all, which the deadline says and nothing else. A signal that reaches
    /// this thread ends a send or a receive that has a timeout, with a
    /// handler or without one, and the kernel never restarts it
    /// (signal(7)); and a receive that was woken so near its end reports
    /// its timeout a fraction of a millisecond early. Either way the wait
    /// goes on for what is left of it.
    fn send(&self, command: &[u8]) -> std::io::Result<()> {
        let until = Instant::now() + SEND_WAIT;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            self.socket.set_write_timeout(Some(left))?;
            match self.socket.send(command) {
                Ok(_) => return Ok(()),
                Err(e) if waits_on(&e) => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// What the supplicant says next, `until` at the latest; the wait ends
    /// as in [`Ctrl::send`].
    fn receive(&self, until: Instant) -> std::io::Result<String> {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            self.socket.set_read_timeout(Some(left))?;
            match self.socket.recv(&mut buf) {
                Ok(n) => return Ok(String::from_utf8_lossy(buf.get(..n).unwrap_or(&[])).into_owned()),
                Err(e) if waits_on(&e) => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// One command and its answer. What the supplicant says of its own
    /// accord meanwhile (an event: it starts with `<`) is not the answer.
    fn request(&mut self, command: &str) -> std::io::Result<String> {
        self.request_within(command, self.answer_wait)
    }

    fn request_within(&mut self, command: &str, wait: Duration) -> std::io::Result<String> {
        if self.silent {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        let answer = self.send(command.as_bytes()).and_then(|_| {
            let until = Instant::now() + wait;
            loop {
                let said = self.receive(until)?;
                if !said.starts_with('<') {
                    return Ok(said);
                }
            }
        });
        self.silent = answer.is_err();
        answer
    }

    /// A scan is asked for and its end waited for, `wait` at most (a scan
    /// of every band takes a few seconds, and a card that is already
    /// scanning answers busy and ends its own); the supplicant announces a
    /// scan's end as an event, so its events are taken for that long. The
    /// networks are read once it has ended, and not before: a radio that
    /// was idle starts again for a scan, inside the supplicant's call into
    /// the kernel, and the supplicant answers nothing until that is done.
    fn scan(&mut self, wait: Duration) -> Scan {
        self.attached = self.request("ATTACH").is_ok_and(|a| a.starts_with("OK"));
        if !self.attached || !self.request("SCAN").is_ok_and(|a| a.starts_with("OK") || a.starts_with("FAIL-BUSY")) {
            return Scan::NotAsked;
        }
        let until = Instant::now() + wait;
        let mut ended = false;
        while let Ok(said) = self.receive(until) {
            if said.contains("CTRL-EVENT-SCAN-RESULTS") || said.contains("CTRL-EVENT-SCAN-FAILED") {
                ended = true;
                break;
            }
        }
        if !ended {
            return Scan::Running;
        }
        self.attached = !self.request("DETACH").is_ok_and(|a| a.starts_with("OK"));
        match self.access_points() {
            Some(found) => Scan::Ended(found),
            None => Scan::Running,
        }
    }

    /// Every access point in the supplicant's table, read a page at a time
    /// (`embra_boot::wifi::parse_bss_page`): one answer holds 4096 bytes,
    /// some forty of them, and `SCAN_RESULTS` stops there without a word.
    /// `None` when the first page did not come; a later page that does not
    /// come leaves what was read.
    fn access_points(&mut self) -> Option<Vec<wifi::Seen>> {
        let mut all = Vec::new();
        let mut from = 0u32;
        for page in 0..PAGES_MAX {
            let answer = match self.request(&format!("BSS RANGE={from}- MASK={}", wifi::BSS_MASK)) {
                Ok(answer) => answer,
                Err(_) if page == 0 => return None,
                Err(_) => break,
            };
            let read = wifi::parse_bss_page(&answer);
            // Nothing at or after that id: the table's end was the page before.
            let Some(last) = read.last_id else { break };
            all.extend(read.seen);
            if read.end {
                break;
            }
            from = last.saturating_add(1);
        }
        Some(all)
    }
}

impl Drop for Ctrl {
    fn drop(&mut self) {
        if self.attached {
            let _ = self.send(b"DETACH");
        }
        let _ = std::fs::remove_file(&self.own);
    }
}

/// The Wi-Fi network a card is on, by the name `/network wifi` shows, as the
/// card's supplicant says it now: what `system_status` shows the
/// intelligence. `None` for a card that is on no network, for one that is
/// still joining, and for a supplicant that is not there or does not answer
/// within [`STATUS_WAIT`]. One question, `STATUS`: no scan is started, and
/// the answer carries no passphrase and no network that is merely stored
/// or in range.
pub(crate) fn joined_network(env: &NetEnv, card: &str) -> Option<String> {
    let mut ctrl = Ctrl::open(&env.wifi_ctrl, card, STATUS_WAIT).ok()?;
    let link = wifi::parse_status(&ctrl.request("STATUS").ok()?);
    match (link.joined(), link.ssid) {
        (true, Some(ssid)) => Some(wifi::ssid_shown(&ssid)),
        _ => None,
    }
}

fn stored_words(network: &WifiNetwork) -> String {
    let mut marks = Vec::new();
    if network.key.is_none() {
        marks.push("open");
    }
    if network.hidden {
        marks.push("hidden");
    }
    match marks.is_empty() {
        true => wifi::ssid_shown(&network.ssid),
        false => format!("{} ({})", wifi::ssid_shown(&network.ssid), marks.join(", ")),
    }
}

fn show(env: &NetEnv) -> Reply {
    let (stored, refused) = Wifi::read(&env.wifi);
    let radios: Vec<_> = cards::cards(&env.sys).into_iter().filter(|c| c.kind == Kind::Wifi).collect();
    let mut text = String::from("Wi-Fi\n");
    if radios.is_empty() {
        // This boot's kernel line decides whether embrad loads a driver at
        // all: with embra.wifi=0 a card that is plugged in stays without one.
        let drivers = embra_boot::Cmdline::parse(&std::fs::read_to_string(&env.cmdline).unwrap_or_default()).wifi();
        text.push_str(match drivers {
            true => "  Card: none. The kernel shows no Wi-Fi card; the driver of one that is plugged in is loaded when it appears (embra.wifi=0 on the kernel line loads none).\n",
            false => "  Card: none. embra.wifi=0 is on this boot's kernel line: no wireless driver is loaded, for a card that is plugged in now either. A boot without it loads them.\n",
        });
    }
    let mut in_range: Vec<wifi::Seen> = Vec::new();
    // The cards whose scan ended within the wait, and those whose scan was
    // asked for and still runs.
    let (mut scanned, mut scanning) = (0usize, 0usize);
    for card in &radios {
        let mut ctrl = Ctrl::open(&env.wifi_ctrl, &card.name, env.answer_wait).ok();
        // What the card is on and what the scan before found are asked
        // first, while the supplicant answers; the scan after them. A radio
        // that was idle is started again for a scan inside the supplicant's
        // call into the kernel, and the supplicant answers nothing until
        // that is done: seconds on a USB adapter.
        let status = ctrl.as_mut().map(|c| c.request("STATUS"));
        let line = match &status {
            None => "not ready yet: its radio is starting, and its supplicant answers once the card is up; ask again in a moment".to_string(),
            Some(Err(_)) => format!(
                "its radio is busy: the supplicant gave no answer within {} s (a radio that was idle takes seconds to come up or to start a scan); ask again in a moment",
                env.answer_wait.as_secs().max(1)
            ),
            Some(Ok(answer)) => {
                let link = wifi::parse_status(answer);
                match (&link.ssid, link.joined()) {
                    (Some(ssid), true) => {
                        let signal = ctrl.as_mut().and_then(|c| c.request("SIGNAL_POLL").ok()).and_then(|a| wifi::parse_signal(&a));
                        let how = link.key_mgmt.as_deref().map(|k| format!(" ({k})")).unwrap_or_default();
                        match signal {
                            Some(dbm) => format!("on {}{how}, signal {}", wifi::ssid_shown(ssid), wifi::signal_words(dbm)),
                            None => format!("on {}{how}", wifi::ssid_shown(ssid)),
                        }
                    }
                    _ if stored.networks.is_empty() => "on no network: none is stored".to_string(),
                    _ => format!("on no network yet ({})", link.state.to_ascii_lowercase().replace('_', " ")),
                }
            }
        };
        text.push_str(&format!("  {} ({}): {line}\n", card.name, card.mac));
        let log = std::fs::read_to_string(env.wifi_logs.join(format!("{}.log", card.name))).unwrap_or_default();
        if let Some(ssid) = wifi::wrong_key_in_log(&log) {
            text.push_str(&format!(
                "           {} refused the passphrase that is stored for it: /network wifi join {} --key <passphrase> stores another\n",
                wifi::ssid_shown(&ssid),
                wifi::ssid_shown(&ssid)
            ));
        }
        let before = ctrl.as_mut().and_then(|c| c.access_points());
        let found = match ctrl.as_mut().map(|c| c.scan(env.scan_wait)) {
            Some(Scan::Ended(found)) => {
                scanned += 1;
                Some(found)
            }
            Some(Scan::Running) => {
                scanning += 1;
                before
            }
            Some(Scan::NotAsked) | None => before,
        };
        if let Some(found) = found {
            // One line a name: the strongest of the access points that carry it.
            for seen in found {
                match in_range.iter_mut().find(|s| s.ssid == seen.ssid) {
                    Some(known) if known.signal >= seen.signal => {}
                    Some(known) => *known = seen,
                    None => in_range.push(seen),
                }
            }
        }
    }
    text.push_str(&format!(
        "  Stored: {}\n",
        if stored.networks.is_empty() { "none".to_string() } else { stored.networks.iter().rev().map(stored_words).collect::<Vec<_>>().join(", ") }
    ));
    text.push_str(&match &stored.country {
        Some(code) => format!("  Country: {code}\n"),
        None => "  Country: not set (the card keeps to the channels every country allows; /network wifi country <CC>)\n".to_string(),
    });
    if !radios.is_empty() {
        in_range.sort_by(|a, b| b.signal.cmp(&a.signal).then_with(|| a.ssid.cmp(&b.ssid)));
        let all_scanned = scanned == radios.len();
        if in_range.is_empty() {
            text.push_str(if all_scanned {
                "  In range: nothing was found by this scan\n"
            } else if scanning > 0 {
                "  In range: the scan has not ended yet (a radio that was idle takes seconds to start one); ask again in a moment\n"
            } else {
                "  In range: no scan could be asked for: the card is not ready yet\n"
            });
        } else {
            text.push_str(if all_scanned { "  In range:\n" } else { "  In range, as the scan before found (a new scan has not ended yet; ask again for it):\n" });
            let width = in_range.iter().map(|s| wifi::ssid_shown(&s.ssid).chars().count()).max().unwrap_or(0).min(40);
            for seen in &in_range {
                let name = wifi::ssid_shown(&seen.ssid);
                let known = if stored.networks.iter().any(|n| n.ssid == seen.ssid) { "  (stored)" } else { "" };
                text.push_str(&format!("    {name:<width$}  {}  {}{known}\n", wifi::signal_words(seen.signal), wifi::security_words(&seen.flags)));
            }
        }
    }
    for line in refused {
        text.push_str(&format!("  The stored networks: {line}\n"));
    }
    text.push_str("  /network wifi join <name> --key <passphrase> · /network wifi join <name> --open · /network wifi forget <name> · /network wifi country <CC>");
    Reply::said(text)
}

fn store(env: &NetEnv, stored: &Wifi, done: String) -> Reply {
    match stored.write(&env.wifi) {
        Ok(()) => Reply::said(done),
        Err(e) => Reply::refusal(format!("The networks could not be written to {}: {e}. Nothing changed.", env.wifi.display())),
    }
}

/// Runs `/network wifi <rest>`.
pub(crate) fn answer(rest: &str, env: &NetEnv) -> Reply {
    match parse(rest) {
        Action::Show => show(env),
        Action::Refused(sentence) => Reply::refusal(sentence),
        Action::Join(network) => {
            let (mut stored, _) = Wifi::read(&env.wifi);
            let name = wifi::ssid_shown(&network.ssid);
            let with = match (&network.key, network.hidden) {
                (Some(_), false) => "with its passphrase",
                (Some(_), true) => "with its passphrase, as a network that does not announce its name",
                (None, false) => "as a network without a passphrase",
                (None, true) => "as a network without a passphrase that does not announce its name",
            };
            if !stored.join(network) {
                return Reply::refusal(format!("Not stored: {} networks are stored already. /network wifi forget <name> makes room.", wifi::NETWORKS_MAX));
            }
            let here = cards::cards(&env.sys).iter().any(|c| c.kind == Kind::Wifi);
            let next = if here {
                "embrad has the supplicant read it within a few seconds; /network wifi shows whether the card is on it, and /network its address."
            } else {
                "No Wi-Fi card is here now: the network is joined when one appears."
            };
            store(env, &stored, format!("Stored: {name}, {with}.\n{next}"))
        }
        Action::Forget(ssid) => {
            let (mut stored, _) = Wifi::read(&env.wifi);
            if !stored.forget(&ssid) {
                return Reply::refusal(format!("No stored network is named {}. Nothing changed.", wifi::ssid_shown(&ssid)));
            }
            store(env, &stored, format!("Forgotten: {}. A card that is on it leaves it within a few seconds.", wifi::ssid_shown(&ssid)))
        }
        Action::Country(code) => {
            let (mut stored, _) = Wifi::read(&env.wifi);
            stored.country = Some(code.clone());
            store(env, &stored, format!("Stored: the regulatory domain is {code}. The supplicant reads it within a few seconds."))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn net(name: &str, key: Option<&str>, hidden: bool) -> WifiNetwork {
        WifiNetwork { ssid: name.as_bytes().to_vec(), key: key.map(str::to_string), hidden }
    }

    #[test]
    fn a_name_with_spaces_is_taken_whole_before_the_options() {
        assert_eq!(parse(""), Action::Show);
        assert_eq!(parse("  show "), Action::Show);
        assert_eq!(parse("join Home Net --key correct horse battery"), Action::Join(net("Home Net", Some("correct horse battery"), false)));
        // The passphrase is everything after the first --key, its own spaces and a later --key included.
        assert_eq!(parse("join Home --key pass --key word  "), Action::Join(net("Home", Some("pass --key word"), false)));
        assert_eq!(parse("join My  Two  Spaces --hidden --key 12345678"), Action::Join(net("My  Two  Spaces", Some("12345678"), true)));
        assert_eq!(parse("join guest net --open"), Action::Join(net("guest net", None, false)));
        assert_eq!(parse("join guest --open --hidden"), Action::Join(net("guest", None, true)));
        assert_eq!(parse("join hex:ff00 --open"), Action::Join(WifiNetwork { ssid: vec![0xff, 0x00], key: None, hidden: false }));
        // A name that ends in the letters of an option is a name.
        assert_eq!(parse("join not--open --open"), Action::Join(net("not--open", None, false)));
        assert_eq!(parse("forget Home Net"), Action::Forget(b"Home Net".to_vec()));
        assert_eq!(parse("country de"), Action::Country("DE".into()));
    }

    #[test]
    fn what_cannot_be_stored_is_refused_without_repeating_a_passphrase() {
        for (line, starts) in [
            ("join Home", "Not stored: Home needs its passphrase"),
            ("join Home --key short", "Not stored: a passphrase is 8 to 63 characters, and this one has 5"),
            ("join Home --key", "Not stored: a passphrase is 8 to 63 characters, and this one has 0"),
            ("join Home --open --key 12345678", "Not stored: a network is joined with --key <passphrase> or with --open, not both"),
            ("join --key 12345678", "Not stored: no network is named"),
            ("join --open", "Not stored: no network is named"),
            ("forget", "Nothing changed: no network is named"),
            ("country usa", "Not stored: a country is its two letters"),
            ("frobnicate", "Usage: /network wifi"),
            ("show extra", "Usage: /network wifi"),
        ] {
            match parse(line) {
                Action::Refused(sentence) => assert!(sentence.starts_with(starts), "{line}: {sentence}"),
                other => panic!("{line}: {other:?}"),
            }
        }
        let name = "x".repeat(33);
        assert!(matches!(parse(&format!("join {name} --open")), Action::Refused(s) if s.contains("1 to 32 bytes")));
        let secret = "s3cret one \" then # more";
        match parse(&format!("join Home --key {secret}")) {
            Action::Refused(sentence) => assert!(!sentence.contains("s3cret"), "{sentence}"),
            other => panic!("{other:?}"),
        }
    }

    /// A machine in a directory with one Wi-Fi card, and a stand-in
    /// supplicant on its control socket that answers from `answers`.
    fn machine(tag: &str, radio: bool) -> (PathBuf, NetEnv) {
        let root = std::env::temp_dir().join(format!("embra-brain-wifi-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut env = NetEnv::in_force();
        env.sys = root.join("sys");
        env.net_dir = root.join("net");
        env.wifi = root.join("state").join("network").join("wifi");
        env.wifi_ctrl = root.join("net").join("wpa");
        env.wifi_logs = root.join("net").join("wpa-log");
        // The stand-in machine's own kernel line (none until a test writes
        // one), name, resolvers and settings, never the test host's.
        env.cmdline = root.join("cmdline");
        env.hostname = root.join("hostname");
        env.resolv = root.join("resolv.conf");
        env.settings = root.join("state").join("network").join("settings");
        env.clock = root.join("clock-status");
        env.pki = root.join("state").join("pki");
        env.scan_wait = Duration::from_millis(20);
        std::fs::create_dir_all(&env.wifi_ctrl).unwrap();
        std::fs::create_dir_all(&env.wifi_logs).unwrap();
        std::fs::create_dir_all(&env.sys).unwrap();
        if radio {
            let dir = env.sys.join("wlan0");
            std::fs::create_dir_all(dir.join("device")).unwrap();
            std::fs::create_dir_all(dir.join("wireless")).unwrap();
            std::fs::write(dir.join("type"), "1\n").unwrap();
            std::fs::write(dir.join("ifindex"), "3\n").unwrap();
            std::fs::write(dir.join("address"), "02:00:00:00:00:00\n").unwrap();
        }
        (root, env)
    }

    /// What the supplicant answers to `BSS RANGE=<from>- MASK=…`, for a
    /// table given as `SCAN_RESULTS` prints one (a header, then `bssid \t
    /// frequency \t signal \t flags \t name` an access point; the id is
    /// the row's number): the entries from that id on, as many as fit whole
    /// in 4096 bytes, and `####` in place of the last entry's `====` when
    /// the table ends with it.
    fn bss_page(table: &str, command: &str) -> String {
        let from: u32 = command.strip_prefix("BSS RANGE=").and_then(|r| r.split('-').next()).and_then(|n| n.parse().ok()).unwrap_or(0);
        let rows: Vec<&str> = table.lines().skip(1).collect();
        let mut answer = String::new();
        for (id, row) in rows.iter().enumerate().skip(from as usize) {
            let fields: Vec<&str> = row.splitn(5, '\t').collect();
            let [bssid, freq, level, flags, name] = fields[..] else { continue };
            let end = if id + 1 == rows.len() { "####" } else { "====" };
            let entry = format!("id={id}\nbssid={bssid}\nfreq={freq}\nlevel={level}\nflags={flags}\nssid={name}\n{end}\n");
            if answer.len() + entry.len() > 4096 {
                break;
            }
            answer.push_str(&entry);
        }
        answer
    }

    /// A question as a test names it: a page of the table without its mask.
    fn named(command: &str) -> String {
        command.split(" MASK=").next().unwrap_or(command).to_string()
    }

    /// A stand-in's next question, `None` once its socket's wait has run
    /// out. A wait that a signal interrupted is not its end: the stand-in's
    /// socket has a timeout as the brain's has, and a stand-in that left
    /// would fail its test.
    fn next_question(socket: &UnixDatagram, buf: &mut [u8]) -> Option<(usize, std::os::unix::net::SocketAddr)> {
        loop {
            match socket.recv_from(buf) {
                Ok(question) => return Some(question),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            }
        }
    }

    /// A stand-in supplicant on wlan0's control socket. It answers from
    /// `answers` and takes `ATTACH` and `DETACH`. With `scan_ends`, a
    /// `SCAN` runs for that long, as the supplicant's does: an event at its
    /// start, the results only once it has ended (the list is empty until
    /// then), an event at its end and one of another kind after it. `None`
    /// has the results at once and announces nothing.
    fn supplicant(env: &NetEnv, answers: &'static [(&'static str, &'static str)], scan_ends: Option<Duration>) -> std::sync::mpsc::Receiver<String> {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        let socket = UnixDatagram::bind(env.wifi_ctrl.join("wlan0")).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let scanned = Arc::new(AtomicBool::new(scan_ends.is_none()));
        std::thread::spawn(move || {
            let mut buf = [0u8; 256];
            while let Some((n, from)) = next_question(&socket, &mut buf) {
                let command = String::from_utf8_lossy(&buf[..n]).into_owned();
                let table = |key: &str| answers.iter().find(|(c, _)| *c == key).map(|(_, a)| *a);
                let answer = match command.as_str() {
                    "ATTACH" | "DETACH" => "OK\n".to_string(),
                    // Its table, a page at a time. Until its scan has ended:
                    // what the scan before found (`BEFORE`), or nothing.
                    page if page.starts_with("BSS RANGE=") => {
                        let known = if scanned.load(Ordering::SeqCst) { table("SCAN_RESULTS") } else { table("BEFORE") };
                        bss_page(known.unwrap_or(""), page)
                    }
                    other => table(other).unwrap_or("UNKNOWN COMMAND\n").to_string(),
                };
                let Some(path) = from.as_pathname().map(Path::to_path_buf) else { continue };
                // On the record before it is answered: a test reads the
                // record as soon as the brain has its last answer.
                let _ = tx.send(named(&command));
                let _ = socket.send_to(answer.as_bytes(), &path);
                if command == "SCAN" && let Some(after) = scan_ends {
                    let (events, scanned) = (socket.try_clone().unwrap(), scanned.clone());
                    std::thread::spawn(move || {
                        let _ = events.send_to(b"<3>CTRL-EVENT-SCAN-STARTED ", &path);
                        std::thread::sleep(after);
                        scanned.store(true, Ordering::SeqCst);
                        let _ = events.send_to(b"<3>CTRL-EVENT-SCAN-RESULTS ", &path);
                        let _ = events.send_to(b"<3>CTRL-EVENT-NETWORK-NOT-FOUND ", &path);
                    });
                }
            }
        });
        rx
    }

    #[test]
    fn a_network_is_stored_with_its_passphrase_and_never_said_back() {
        let (root, env) = machine("store", true);
        let stored = answer("join Home Net --key correct horse battery", &env);
        assert_eq!(stored.text, "Stored: Home Net, with its passphrase.\nembrad has the supplicant read it within a few seconds; /network wifi shows whether the card is on it, and /network its address.");
        assert!(!stored.refused);
        assert_eq!(Wifi::read(&env.wifi).0.networks, [net("Home Net", Some("correct horse battery"), false)]);
        // The file is private, and the passphrase is not in it as typed.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&env.wifi).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(!std::fs::read_to_string(&env.wifi).unwrap().contains("correct horse"));
        assert_eq!(answer("join guest --open --hidden", &env).text.lines().next().unwrap(), "Stored: guest, as a network without a passphrase that does not announce its name.");
        assert_eq!(answer("country de", &env).text, "Stored: the regulatory domain is DE. The supplicant reads it within a few seconds.");
        // Joined again with another passphrase: replaced, and preferred.
        assert!(!answer("join Home Net --key another passphrase", &env).refused);
        let (now, _) = Wifi::read(&env.wifi);
        assert_eq!(now.networks, [net("guest", None, true), net("Home Net", Some("another passphrase"), false)]);
        assert_eq!(now.country.as_deref(), Some("DE"));
        // Taken away; a name that is not stored changes nothing.
        assert_eq!(answer("forget guest", &env).text, "Forgotten: guest. A card that is on it leaves it within a few seconds.");
        assert!(answer("forget guest", &env).refused);
        // A refusal writes nothing.
        let before = std::fs::read_to_string(&env.wifi).unwrap();
        assert!(answer("join Other --key short", &env).refused);
        assert_eq!(std::fs::read_to_string(&env.wifi).unwrap(), before);
        std::fs::remove_dir_all(&root).unwrap();

        // Without a card the network is stored all the same.
        let (root, env) = machine("no-card", false);
        assert!(answer("join Home --open", &env).text.ends_with("No Wi-Fi card is here now: the network is joined when one appears."));
        let shown = answer("", &env).text;
        assert!(shown.contains("  Card: none. The kernel shows no Wi-Fi card; the driver of one that is plugged in is loaded when it appears") && shown.contains("  Stored: Home (open)\n") && !shown.contains("In range"), "{shown}");
        // With embra.wifi=0 on this boot's kernel line no driver is loaded,
        // and the answer does not promise one.
        std::fs::write(&env.cmdline, "console=ttyS0 embra.wifi=0 embra.slot=a\n").unwrap();
        let shown = answer("", &env).text;
        assert!(shown.contains("  Card: none. embra.wifi=0 is on this boot's kernel line: no wireless driver is loaded") && !shown.contains("is loaded when it appears"), "{shown}");
        assert!(shown.contains("  Stored: Home (open)\n"), "the stored networks are kept and shown: {shown}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A stand-in supplicant that is inside a long call into the kernel:
    /// it reads nothing for `busy`, then answers every question that has
    /// queued up, in the order they came.
    fn busy_supplicant(env: &NetEnv, answers: &'static [(&'static str, &'static str)], busy: Duration) -> std::sync::mpsc::Receiver<String> {
        let socket = UnixDatagram::bind(env.wifi_ctrl.join("wlan0")).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            std::thread::sleep(busy);
            let mut buf = [0u8; 256];
            while let Some((n, from)) = next_question(&socket, &mut buf) {
                let command = String::from_utf8_lossy(&buf[..n]).into_owned();
                let table = |key: &str| answers.iter().find(|(c, _)| *c == key).map(|(_, a)| *a);
                let answer = match command.as_str() {
                    "ATTACH" | "DETACH" => "OK\n".to_string(),
                    page if page.starts_with("BSS RANGE=") => bss_page(table("SCAN_RESULTS").unwrap_or(""), page),
                    other => table(other).unwrap_or("UNKNOWN COMMAND\n").to_string(),
                };
                // On the record before it is answered, as in `supplicant`.
                let _ = tx.send(named(&command));
                if let Some(path) = from.as_pathname() {
                    let _ = socket.send_to(answer.as_bytes(), path);
                }
            }
        });
        rx
    }

    /// embrad's status for a machine with a wired card and a Wi-Fi card.
    fn two_cards(env: &NetEnv) {
        use embra_boot::net_lease::{self, CardStatus, Mode};
        std::fs::create_dir_all(&env.net_dir).unwrap();
        let cards = [
            CardStatus { name: "eth0".into(), kind: Kind::Wired, mac: "52:54:00:aa:00:01".into(), link: Some(true), mode: Mode::Dhcp, note: None },
            CardStatus { name: "wlan0".into(), kind: Kind::Wifi, mac: "02:00:00:00:00:00".into(), link: Some(true), mode: Mode::Dhcp, note: None },
        ];
        std::fs::write(env.net_dir.join(net_lease::STATUS_FILE), net_lease::status_text(&cards)).unwrap();
    }

    fn networks_of(report: &crate::network::NetworkReport) -> Vec<(&str, Option<&str>)> {
        report.cards.iter().map(|c| (c.name.as_str(), c.wifi_network.as_deref())).collect()
    }

    #[test]
    fn the_status_names_the_network_a_card_is_on_and_no_other() {
        let (root, env) = machine("status-on", true);
        two_cards(&env);
        // Two networks are stored, one with a passphrase; a third is in
        // range. The card is on the first.
        assert!(!answer("join Home Net --key correct horse battery", &env).refused);
        assert!(!answer("join Guest --open", &env).refused);
        let asked = supplicant(&env, &[
            ("STATUS", "bssid=02:00:00:00:01:00\nfreq=2412\nssid=Home Net\nid=0\nmode=station\nkey_mgmt=WPA2-PSK\nwpa_state=COMPLETED\nip_address=192.168.7.20\n"),
            ("SCAN_RESULTS", "bssid / frequency / signal level / flags / ssid\n02:00:00:00:01:00\t2412\t-47\t[WPA2-PSK-CCMP][ESS]\tHome Net\n02:00:00:00:03:00\t2437\t-70\t[WPA2-PSK-CCMP][ESS]\tNext Door\n"),
        ], None);
        // A socket by the wired card's name: a wired card has no supplicant,
        // and none is asked for one.
        let wired = UnixDatagram::bind(env.wifi_ctrl.join("eth0")).unwrap();
        wired.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let started = Instant::now();
        let report = crate::network::report(&env).unwrap();
        assert!(started.elapsed() < Duration::from_millis(500), "{:?}", started.elapsed());
        assert!(wired.recv(&mut [0u8; 32]).is_err(), "nothing is asked on a wired card's name");
        assert_eq!(networks_of(&report), [("eth0", None), ("wlan0", Some("Home Net"))]);
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains(r#""wifi_network":"Home Net""#), "{json}");
        assert_eq!(json.matches("wifi_network").count(), 1, "a wired card has no such field: {json}");
        for kept_back in ["correct horse", "Guest", "Next Door", "WPA2", "02:00:00:00:01:00"] {
            assert!(!json.contains(kept_back), "{kept_back} is not the status tool's to show: {json}");
        }
        assert_eq!(asked.try_iter().collect::<Vec<String>>(), ["STATUS"], "one question, and no scan is started for it");
        std::fs::remove_dir_all(&root).unwrap();

        // A name that is no plain text is shown as /network wifi shows it.
        let (root, env) = machine("status-name", true);
        two_cards(&env);
        let _asked = supplicant(&env, &[("STATUS", "ssid=caf\\xc3\\xa9 \\\"2\\\"\nwpa_state=COMPLETED\n")], None);
        assert_eq!(networks_of(&crate::network::report(&env).unwrap()), [("eth0", None), ("wlan0", Some("café \"2\""))]);
        std::fs::remove_dir_all(&root).unwrap();
        let (root, env) = machine("status-bytes", true);
        two_cards(&env);
        let _asked = supplicant(&env, &[("STATUS", "ssid=\\xff\\x00\\x1b[2J\nwpa_state=COMPLETED\n")], None);
        assert_eq!(networks_of(&crate::network::report(&env).unwrap()), [("eth0", None), ("wlan0", Some("hex:ff001b5b324a"))], "never a control character");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_card_that_is_on_no_network_has_no_name_in_the_status() {
        // Still joining: the supplicant names the network it is trying.
        let (root, env) = machine("status-joining", true);
        two_cards(&env);
        let _asked = supplicant(&env, &[("STATUS", "ssid=Home Net\nwpa_state=4WAY_HANDSHAKE\n")], None);
        assert_eq!(networks_of(&crate::network::report(&env).unwrap()), [("eth0", None), ("wlan0", None)]);
        std::fs::remove_dir_all(&root).unwrap();

        // Scanning, with nothing to name.
        let (root, env) = machine("status-scanning", true);
        two_cards(&env);
        let _asked = supplicant(&env, &[("STATUS", "wpa_state=SCANNING\naddress=02:00:00:00:00:00\n")], None);
        let report = crate::network::report(&env).unwrap();
        assert_eq!(networks_of(&report), [("eth0", None), ("wlan0", None)]);
        assert!(!serde_json::to_string(&report).unwrap().contains("wifi_network"));
        std::fs::remove_dir_all(&root).unwrap();

        // No supplicant on the card (embrad has not started one, or
        // embra.wifi=0): no name, and no wait.
        let (root, env) = machine("status-none", true);
        two_cards(&env);
        let started = Instant::now();
        assert_eq!(networks_of(&crate::network::report(&env).unwrap()), [("eth0", None), ("wlan0", None)]);
        assert!(started.elapsed() < Duration::from_millis(500), "{:?}", started.elapsed());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_supplicant_that_does_not_answer_holds_the_status_a_second_and_no_longer() {
        let (root, env) = machine("status-silent", true);
        two_cards(&env);
        // A socket on the card's name that takes the question and says nothing.
        let silent = UnixDatagram::bind(env.wifi_ctrl.join("wlan0")).unwrap();
        let started = Instant::now();
        let report = crate::network::report(&env).unwrap();
        let took = started.elapsed();
        assert_eq!(networks_of(&report), [("eth0", None), ("wlan0", None)]);
        assert!(took >= Duration::from_millis(900) && took < Duration::from_millis(2500), "the status tool's wait, not the command's three seconds: {took:?}");
        let mut buf = [0u8; 32];
        silent.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        assert_eq!(silent.recv(&mut buf).map(|n| buf[..n].to_vec()).unwrap(), b"STATUS".to_vec(), "the question did arrive");
        // The brain's own socket is gone again.
        let left: Vec<String> = std::fs::read_dir(env.wifi_ctrl.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("wpa-client")).collect();
        assert!(left.is_empty(), "{left:?}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_card_the_network_it_is_on_and_the_networks_in_range_are_shown() {
        let (root, env) = machine("show", true);
        // A scan that ends after a fifth of a second, and a patience of ten.
        let mut env = env;
        env.scan_wait = Duration::from_secs(10);
        let started = Instant::now();
        let asked = supplicant(&env, &[
            ("SCAN", "OK\n"),
            ("STATUS", "bssid=02:00:00:00:01:00\nfreq=2412\nssid=Home Net\nid=0\nmode=station\nkey_mgmt=WPA2-PSK\nwpa_state=COMPLETED\nip_address=192.168.7.20\n"),
            ("SIGNAL_POLL", "RSSI=-47\nLINKSPEED=65\nNOISE=9999\nFREQUENCY=2412\n"),
            ("SCAN_RESULTS", "bssid / frequency / signal level / flags / ssid\n02:00:00:00:01:00\t2412\t-47\t[WPA2-PSK-CCMP][ESS]\tHome Net\n02:00:00:00:03:00\t2437\t-70\t[WPA2-SAE-CCMP][ESS]\tcaf\\xc3\\xa9\n02:00:00:00:05:00\t2462\t-82\t[WPA2-EAP-CCMP][ESS]\tcorp\n"),
        ], Some(Duration::from_millis(200)));
        assert!(!answer("join Home Net --key correct horse battery", &env).refused);
        let shown = answer("", &env).text;
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(200) && took < Duration::from_secs(4), "the scan's end is waited for, and no longer: {took:?}");
        assert_eq!(
            shown,
            "Wi-Fi\n  wlan0 (02:00:00:00:00:00): on Home Net (WPA2-PSK), signal strong, -47 dBm\n  Stored: Home Net\n  Country: not set (the card keeps to the channels every country allows; /network wifi country <CC>)\n  In range:\n    Home Net  strong, -47 dBm  WPA2  (stored)\n    café      fair, -70 dBm  WPA3\n    corp      weak, -82 dBm  802.1X (a company network: not supported)\n  /network wifi join <name> --key <passphrase> · /network wifi join <name> --open · /network wifi forget <name> · /network wifi country <CC>"
        );
        assert!(!shown.contains("correct horse"));
        let commands: Vec<String> = asked.try_iter().collect();
        assert_eq!(commands, ["STATUS", "SIGNAL_POLL", "BSS RANGE=0-", "ATTACH", "SCAN", "DETACH", "BSS RANGE=0-"], "what the card is on and what it knows first, then the scan, its end, and what it found");
        // No socket of the brain's own is left beside the control directory.
        let left: Vec<String> = std::fs::read_dir(env.wifi_ctrl.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("wpa-client")).collect();
        assert!(left.is_empty(), "{left:?}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_refused_passphrase_and_a_supplicant_that_is_away_are_said() {
        let (root, env) = machine("refused", true);
        // Nothing listens on the control socket yet.
        assert!(!answer("join Home Net --key wrong passphrase", &env).refused);
        let shown = answer("", &env).text;
        assert!(shown.contains("  wlan0 (02:00:00:00:00:00): not ready yet: its radio is starting, and its supplicant answers once the card is up; ask again in a moment\n"), "{shown}");
        assert!(shown.contains("  In range: no scan could be asked for: the card is not ready yet\n"), "{shown}");
        // The supplicant is there, not on a network, and its log says why.
        // This supplicant never announces a scan's end: what it had before
        // the scan was asked for is shown, as that.
        let _asked = supplicant(&env, &[("SCAN", "OK\n"), ("STATUS", "wpa_state=SCANNING\naddress=02:00:00:00:00:00\n"), ("SCAN_RESULTS", "bssid / frequency / signal level / flags / ssid\n02:00:00:00:04:00\t2462\t-80\t[ESS]\tguest\n")], None);
        std::fs::write(env.wifi_logs.join("wlan0.log"), "wlan0: CTRL-EVENT-SSID-TEMP-DISABLED id=0 ssid=\"Home Net\" auth_failures=1 duration=10 reason=WRONG_KEY\n").unwrap();
        let shown = answer("", &env).text;
        assert!(shown.contains("  wlan0 (02:00:00:00:00:00): on no network yet (scanning)\n           Home Net refused the passphrase that is stored for it: /network wifi join Home Net --key <passphrase> stores another\n"), "{shown}");
        assert!(shown.contains("  In range, as the scan before found (a new scan has not ended yet; ask again for it):\n    guest  weak, -80 dBm  open\n"), "{shown}");
        assert!(!shown.contains("wrong passphrase"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    const HEADER: &str = "bssid / frequency / signal level / flags / ssid\n";

    #[test]
    fn a_radio_that_is_busy_is_said_so_and_a_late_answer_is_never_read_as_another_questions() {
        let (root, mut env) = machine("busy", true);
        env.answer_wait = Duration::from_millis(100);
        // The card is on a network, and its supplicant is inside a long
        // call into the kernel (a radio that was idle is started again for
        // a scan): nothing is read for a while, then every question that
        // queued up is answered, in order.
        let asked = busy_supplicant(
            &env,
            &[
                ("STATUS", "ssid=Home Net\nkey_mgmt=WPA2-PSK\nwpa_state=COMPLETED\n"),
                ("SIGNAL_POLL", "RSSI=-50\n"),
                ("SCAN", "OK\n"),
                ("SCAN_RESULTS", "bssid / frequency / signal level / flags / ssid\n02:00:00:00:01:00\t2412\t-50\t[WPA2-PSK-CCMP][ESS]\tHome Net\n"),
            ],
            Duration::from_millis(500),
        );
        let started = Instant::now();
        let shown = answer("", &env).text;
        let took = started.elapsed();
        assert!(
            shown.contains("  wlan0 (02:00:00:00:00:00): its radio is busy: the supplicant gave no answer within 1 s (a radio that was idle takes seconds to come up or to start a scan); ask again in a moment\n"),
            "{shown}"
        );
        assert!(shown.contains("  In range: no scan could be asked for: the card is not ready yet\n"), "{shown}");
        assert!(took >= Duration::from_millis(100) && took < Duration::from_millis(400), "one question's wait, and no other question after it: {took:?}");
        // The answers come, late: none of them was read as the answer to a
        // later question, because no later question was asked.
        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(asked.try_iter().collect::<Vec<String>>(), ["STATUS"]);
        // The supplicant answers again: the card's line and its list are its own.
        let shown = answer("", &env).text;
        assert!(shown.contains("  wlan0 (02:00:00:00:00:00): on Home Net (WPA2-PSK), signal strong, -50 dBm\n"), "{shown}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_scan_that_outlasts_the_wait_shows_what_the_scan_before_found_and_says_so() {
        let (root, mut env) = machine("slow-scan", true);
        env.scan_wait = Duration::from_millis(100);
        let asked = supplicant(
            &env,
            &[
                ("SCAN", "OK\n"),
                ("STATUS", "wpa_state=INACTIVE\n"),
                ("BEFORE", "bssid / frequency / signal level / flags / ssid\n02:00:00:00:07:00\t2437\t-61\t[WPA2-PSK-CCMP][ESS]\tEarlier\n"),
                ("SCAN_RESULTS", "bssid / frequency / signal level / flags / ssid\n02:00:00:00:08:00\t5180\t-44\t[WPA2-SAE-CCMP][ESS]\tFresh\n"),
            ],
            Some(Duration::from_millis(600)),
        );
        let shown = answer("", &env).text;
        assert!(shown.contains("  wlan0 (02:00:00:00:00:00): on no network: none is stored\n"), "the card's own line is asked before the scan: {shown}");
        assert!(shown.contains("  In range, as the scan before found (a new scan has not ended yet; ask again for it):\n    Earlier  good, -61 dBm  WPA2\n"), "{shown}");
        assert!(!shown.contains("Fresh"), "{shown}");
        let commands: Vec<String> = asked.try_iter().collect();
        assert_eq!(commands.get(..4), Some(&["STATUS".to_string(), "BSS RANGE=0-".to_string(), "ATTACH".to_string(), "SCAN".to_string()][..]), "nothing is asked of a supplicant whose scan has not ended: {commands:?}");
        // Asked again once that scan has ended: what it found is what the
        // card knows now.
        std::thread::sleep(Duration::from_millis(800));
        let shown = answer("", &env).text;
        assert!(shown.contains("    Fresh  strong, -44 dBm  WPA3\n") && !shown.contains("Earlier"), "{shown}");
        std::fs::remove_dir_all(&root).unwrap();

        // Nothing from before: the answer says the scan runs, and not that
        // nothing is in range.
        let (root, mut env) = machine("slow-first-scan", true);
        env.scan_wait = Duration::from_millis(100);
        let _asked = supplicant(&env, &[("SCAN", "OK\n"), ("STATUS", "wpa_state=INACTIVE\n"), ("SCAN_RESULTS", HEADER)], Some(Duration::from_millis(600)));
        let shown = answer("", &env).text;
        assert!(shown.contains("  In range: the scan has not ended yet (a radio that was idle takes seconds to start one); ask again in a moment\n"), "{shown}");
        assert!(!shown.contains("nothing was found"), "{shown}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_list_longer_than_one_answer_of_the_supplicant_is_read_to_its_end() {
        let (root, mut env) = machine("pages", true);
        env.scan_wait = Duration::from_secs(10);
        // A site with 150 access points, each a name of its own, the
        // weakest first: more than three of the supplicant's answers hold.
        let mut table = String::from(HEADER);
        for n in 0..150 {
            table.push_str(&format!("02:00:00:00:{:02x}:{:02x}\t{}\t-{}\t[WPA2-PSK+SAE-CCMP][WPS][ESS]\tsite network {n:03}\n", n / 256, n % 256, 2412 + 5 * (n % 11), 95 - n / 2));
        }
        let table: &'static str = Box::leak(table.into_boxed_str());
        let answers: &'static [(&'static str, &'static str)] = Box::leak(vec![("SCAN", "OK\n"), ("STATUS", "wpa_state=INACTIVE\n"), ("SCAN_RESULTS", table)].into_boxed_slice());
        assert!(bss_page(table, "BSS RANGE=0-").len() <= 4096 && !bss_page(table, "BSS RANGE=0-").contains("####"), "the stand-in's first answer is full and is not the table's end");
        let asked = supplicant(&env, answers, Some(Duration::from_millis(50)));
        let shown = answer("", &env).text;
        let listed: Vec<&str> = shown.lines().filter(|l| l.starts_with("    site network ")).collect();
        assert_eq!(listed.len(), 150, "every access point of the table, not the first answer's: {shown}");
        let names: std::collections::BTreeSet<&str> = listed.iter().filter_map(|l| l.get(4..20)).collect();
        assert_eq!(names.len(), 150, "each name once");
        assert!(names.contains("site network 000") && names.contains("site network 149"), "the first found and the last found");
        let signals: Vec<i32> = listed.iter().filter_map(|l| l.split(", ").nth(1)?.split(" dBm").next()?.parse().ok()).collect();
        assert!(signals.len() == 150 && signals.windows(2).all(|w| w[0] >= w[1]) && signals[0] == -21 && signals[149] == -95, "the strongest first: {signals:?}");
        let pages = asked.try_iter().filter(|c| c.starts_with("BSS RANGE=")).collect::<Vec<String>>();
        // Before the scan the table is empty (one question); after it, a
        // page at a time, each from the id after the last one read.
        assert_eq!(pages, ["BSS RANGE=0-", "BSS RANGE=0-", "BSS RANGE=36-", "BSS RANGE=72-", "BSS RANGE=108-", "BSS RANGE=143-"]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A supplicant on wlan0 that reads nothing, and its queue filled to
    /// the brim. One sender is stopped by its own allowance before the
    /// queue is full, so senders are added until a new one finds no room
    /// for its first question. The supplicant's socket, the senders' (kept
    /// open by the test) and how many questions queued.
    fn full_queue(env: &NetEnv) -> (UnixDatagram, Vec<UnixDatagram>, u32) {
        let silent = UnixDatagram::bind(env.wifi_ctrl.join("wlan0")).unwrap();
        let mut fillers = Vec::new();
        let mut queued = 0u32;
        loop {
            let filler = UnixDatagram::unbound().unwrap();
            filler.set_nonblocking(true).unwrap();
            let mut sent = 0u32;
            while filler.send_to(b"STATUS", env.wifi_ctrl.join("wlan0")).is_ok() {
                sent += 1;
            }
            queued += sent;
            fillers.push(filler);
            if sent == 0 {
                break;
            }
            assert!(fillers.len() < 256, "the queue never filled ({queued} questions)");
        }
        (silent, fillers, queued)
    }

    #[test]
    fn a_question_does_not_wait_without_end_for_room_in_a_supplicants_queue() {
        let (root, env) = machine("queue", true);
        let (silent, fillers, queued) = full_queue(&env);
        // The status tool's question finds no room. It gives up after the
        // wait for room; on a thread, so that a question that waits without
        // end fails this test and does not hang it.
        let (tx, rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        std::thread::spawn(move || {
            let _ = tx.send(joined_network(&env, "wlan0"));
        });
        let answer = rx.recv_timeout(Duration::from_secs(5));
        assert_eq!(answer, Ok(None), "after {:?} with {queued} questions queued", started.elapsed());
        assert!(started.elapsed() >= Duration::from_millis(900));
        drop((silent, fillers));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A signal for a test to send to one of its own threads: a real-time
    /// signal nothing else in the process uses, with a handler that does
    /// nothing (without one its default action ends the process). The
    /// handler asks for an interrupted call to be restarted, as the
    /// runtime's own handlers do: a send or a receive that has a timeout
    /// is not.
    fn test_signal() -> libc::c_int {
        extern "C" fn nothing(_: libc::c_int) {}
        static INSTALLED: std::sync::Once = std::sync::Once::new();
        let signal = libc::SIGRTMIN() + 7;
        INSTALLED.call_once(|| {
            // SAFETY: a zeroed sigaction with an empty mask and a handler
            // that touches nothing, for a signal no other code here uses.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = nothing as extern "C" fn(libc::c_int) as libc::sighandler_t;
                action.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut action.sa_mask);
                assert_eq!(libc::sigaction(signal, &action, std::ptr::null_mut()), 0);
            }
        });
        signal
    }

    /// How long [`under_signals`] goes on sending.
    const SIGNALS_FOR: Duration = Duration::from_secs(12);

    /// Runs `call` on a thread of its own and sends that thread a signal
    /// every `gap` until it has returned, counting them in `sent`. The
    /// call's answer, and whether it came while the signals still did.
    fn under_signals<T: Send + 'static>(sent: &std::sync::atomic::AtomicU32, gap: Duration, call: impl FnOnce() -> T + Send + 'static) -> (T, bool) {
        use std::os::unix::thread::JoinHandleExt;
        let signal = test_signal();
        let thread = std::thread::spawn(call);
        let until = Instant::now() + SIGNALS_FOR;
        while !thread.is_finished() && Instant::now() < until {
            std::thread::sleep(gap);
            // SAFETY: the handle is held until the join below, so the id
            // names this thread, running or ended.
            if !thread.is_finished() && unsafe { libc::pthread_kill(thread.as_pthread_t(), signal) } == 0 {
                sent.fetch_add(1, Ordering::SeqCst);
            }
        }
        let in_time = thread.is_finished();
        (thread.join().expect("the call"), in_time)
    }

    /// One question on wlan0's control socket, on the thread that calls.
    fn ask(ctrl_dir: &Path, wait: Duration) -> Result<String, std::io::ErrorKind> {
        Ctrl::open(ctrl_dir, "wlan0", wait).and_then(|mut ctrl| ctrl.request("STATUS")).map_err(|e| e.kind())
    }

    /// A signal that reaches the thread inside a question ends its receive
    /// and not the question. An answer that comes after the thread was
    /// interrupted is taken. A supplicant that says nothing is waited for
    /// the whole of the wait, which ends while the signals still come: a
    /// receive that a signal woke near its end reports its timeout early,
    /// which is not the wait's end either.
    #[test]
    fn a_signal_during_the_wait_is_not_the_end_of_a_question() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicU32;
        // A supplicant that answers once three signals have been sent to
        // the thread that asked (ten seconds at most, so a failure leaves
        // no thread behind).
        let (root, env) = machine("signals", true);
        let socket = UnixDatagram::bind(env.wifi_ctrl.join("wlan0")).unwrap();
        let sent = Arc::new(AtomicU32::new(0));
        let seen = sent.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 256];
            let Some((_, from)) = next_question(&socket, &mut buf) else { return };
            for _ in 0..2000 {
                if seen.load(Ordering::SeqCst) >= 3 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            if let Some(path) = from.as_pathname() {
                let _ = socket.send_to(b"ssid=Home Net\nwpa_state=COMPLETED\n", path);
            }
        });
        let ctrl_dir = env.wifi_ctrl.clone();
        let (answer, _) = under_signals(&sent, Duration::from_millis(30), move || ask(&ctrl_dir, Duration::from_secs(60)));
        assert_eq!(answer, Ok("ssid=Home Net\nwpa_state=COMPLETED\n".to_string()));
        assert!(sent.load(Ordering::SeqCst) >= 3, "the answer was held back until three signals were sent");
        std::fs::remove_dir_all(&root).unwrap();

        // A supplicant that takes the questions and says nothing, and a
        // signal every tenth of a millisecond, twelve short waits over: in
        // most of them a receive is woken in the wait's last millisecond.
        let (root, env) = machine("signals-silent", true);
        let silent = UnixDatagram::bind(env.wifi_ctrl.join("wlan0")).unwrap();
        for round in 0..12 {
            let sent = AtomicU32::new(0);
            let ctrl_dir = env.wifi_ctrl.clone();
            let started = Instant::now();
            let (answer, in_time) = under_signals(&sent, Duration::from_micros(100), move || ask(&ctrl_dir, Duration::from_millis(25)));
            let took = started.elapsed();
            assert_eq!(answer, Err(std::io::ErrorKind::TimedOut), "round {round}: the deadline's word, and no other");
            assert!(took >= Duration::from_millis(25), "round {round}: the whole wait, after {} signals: {took:?}", sent.load(Ordering::SeqCst));
            assert!(in_time, "round {round}: the wait ended while the signals still came");
        }
        drop(silent);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The wait for room in a supplicant's queue is a send that has a
    /// timeout: under a signal every tenth of a millisecond it is the whole
    /// of [`SEND_WAIT`], and it ends while the signals still come.
    #[test]
    fn a_signal_does_not_cut_short_the_wait_for_room_in_a_supplicants_queue() {
        use std::sync::atomic::AtomicU32;
        let (root, env) = machine("signals-queue", true);
        let (silent, fillers, queued) = full_queue(&env);
        let sent = AtomicU32::new(0);
        let ctrl_dir = env.wifi_ctrl.clone();
        let started = Instant::now();
        let (answer, in_time) = under_signals(&sent, Duration::from_micros(100), move || ask(&ctrl_dir, Duration::from_secs(60)));
        let took = started.elapsed();
        assert_eq!(answer, Err(std::io::ErrorKind::TimedOut), "no room within the wait, with {queued} questions queued");
        assert!(took >= SEND_WAIT, "the whole wait, after {} signals: {took:?}", sent.load(Ordering::SeqCst));
        assert!(in_time, "the wait ended while the signals still came");
        drop((silent, fillers));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
