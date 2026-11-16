//! `/network wifi`: the Wi-Fi networks in range, the one a card is on, and
//! the networks the operator stored.
//!
//! The brain stores a network (`/embra/state/network/wifi`, mode 0600,
//! `embra_boot::wifi`); embrad sees the file change, renders the
//! supplicant's configuration and has it read. What is in range and what a
//! card is on are asked of the supplicant's control socket, a datagram
//! socket a card under `/embra/ephemeral/net/wpa`.
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
const ANSWER_WAIT: Duration = Duration::from_secs(3);

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
    /// The supplicant sends us its events (`ATTACH`): it is told to stop.
    attached: bool,
}

impl Ctrl {
    fn open(ctrl_dir: &Path, card: &str) -> std::io::Result<Ctrl> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let own = ctrl_dir.with_file_name(format!("wpa-client-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_file(&own);
        let socket = UnixDatagram::bind(&own)?;
        let ctrl = Ctrl { socket, own, attached: false };
        ctrl.socket.connect(ctrl_dir.join(card))?;
        Ok(ctrl)
    }

    fn receive(&self, until: Instant) -> std::io::Result<String> {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        self.socket.set_read_timeout(Some(left))?;
        let mut buf = vec![0u8; 16 * 1024];
        let n = self.socket.recv(&mut buf)?;
        Ok(String::from_utf8_lossy(buf.get(..n).unwrap_or(&[])).into_owned())
    }

    /// One command and its answer. What the supplicant says of its own
    /// accord meanwhile (an event: it starts with `<`) is not the answer.
    fn request(&self, command: &str) -> std::io::Result<String> {
        self.socket.send(command.as_bytes())?;
        let until = Instant::now() + ANSWER_WAIT;
        loop {
            let said = self.receive(until)?;
            if !said.starts_with('<') {
                return Ok(said);
            }
        }
    }

    /// The networks in range: a scan is asked for and its end waited for,
    /// `wait` at most (a scan of every band takes a few seconds, and a card
    /// that is already scanning answers busy and ends its own), then the
    /// results are read. The supplicant announces a scan's end as an
    /// event, so its events are taken for that long.
    fn scan(&mut self, wait: Duration) -> Option<String> {
        self.attached = self.request("ATTACH").is_ok_and(|a| a.starts_with("OK"));
        let asked = self.request("SCAN").is_ok_and(|a| a.starts_with("OK") || a.starts_with("FAIL-BUSY"));
        if asked && self.attached {
            let until = Instant::now() + wait;
            while let Ok(said) = self.receive(until) {
                if said.contains("CTRL-EVENT-SCAN-RESULTS") || said.contains("CTRL-EVENT-SCAN-FAILED") {
                    break;
                }
            }
        }
        if self.attached {
            self.attached = !self.request("DETACH").is_ok_and(|a| a.starts_with("OK"));
        }
        self.request("SCAN_RESULTS").ok()
    }
}

impl Drop for Ctrl {
    fn drop(&mut self) {
        if self.attached {
            let _ = self.socket.send(b"DETACH");
        }
        let _ = std::fs::remove_file(&self.own);
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
        text.push_str("  Card: none. The kernel shows no Wi-Fi card; the driver of one that is plugged in is loaded when it appears (embra.wifi=0 on the kernel line loads none).\n");
    }
    let mut in_range: Vec<wifi::Seen> = Vec::new();
    for card in &radios {
        let mut ctrl = Ctrl::open(&env.wifi_ctrl, &card.name).ok();
        // The scan first: what the card is on is asked after it, as it
        // stands when the answer is written.
        let found = ctrl.as_mut().and_then(|c| c.scan(env.scan_wait));
        let status = ctrl.as_ref().map(|c| c.request("STATUS"));
        let line = match &status {
            None | Some(Err(_)) => "its supplicant does not answer (embrad starts it within a few seconds of the card's appearance)".to_string(),
            Some(Ok(answer)) => {
                let link = wifi::parse_status(answer);
                match (&link.ssid, link.joined()) {
                    (Some(ssid), true) => {
                        let signal = ctrl.as_ref().and_then(|c| c.request("SIGNAL_POLL").ok()).and_then(|a| wifi::parse_signal(&a));
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
        if let Some(answer) = found {
            for seen in wifi::parse_scan_results(&answer) {
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
        if in_range.is_empty() {
            text.push_str("  In range: nothing was found by this scan\n");
        } else {
            text.push_str("  In range:\n");
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
            while let Ok((n, from)) = socket.recv_from(&mut buf) {
                let command = String::from_utf8_lossy(&buf[..n]).into_owned();
                let answer = match command.as_str() {
                    "ATTACH" | "DETACH" => "OK\n",
                    "SCAN_RESULTS" if !scanned.load(Ordering::SeqCst) => "bssid / frequency / signal level / flags / ssid\n",
                    other => answers.iter().find(|(c, _)| *c == other).map(|(_, a)| *a).unwrap_or("UNKNOWN COMMAND\n"),
                };
                let Some(path) = from.as_pathname().map(Path::to_path_buf) else { continue };
                let _ = socket.send_to(answer.as_bytes(), &path);
                let _ = tx.send(command.clone());
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
        assert!(shown.contains("  Card: none.") && shown.contains("  Stored: Home (open)\n") && !shown.contains("In range"), "{shown}");
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
        assert_eq!(commands, ["ATTACH", "SCAN", "DETACH", "SCAN_RESULTS", "STATUS", "SIGNAL_POLL"], "the scan and its end first, then what the card is on");
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
        assert!(shown.contains("  wlan0 (02:00:00:00:00:00): its supplicant does not answer (embrad starts it within a few seconds of the card's appearance)\n"), "{shown}");
        assert!(shown.contains("  In range: nothing was found by this scan\n"));
        // The supplicant is there, not on a network, and its log says why.
        // This supplicant never announces a scan's end: the results are read after the wait all the same.
        let _asked = supplicant(&env, &[("SCAN", "OK\n"), ("STATUS", "wpa_state=SCANNING\naddress=02:00:00:00:00:00\n"), ("SCAN_RESULTS", "bssid / frequency / signal level / flags / ssid\n02:00:00:00:04:00\t2462\t-80\t[ESS]\tguest\n")], None);
        std::fs::write(env.wifi_logs.join("wlan0.log"), "wlan0: CTRL-EVENT-SSID-TEMP-DISABLED id=0 ssid=\"Home Net\" auth_failures=1 duration=10 reason=WRONG_KEY\n").unwrap();
        let shown = answer("", &env).text;
        assert!(shown.contains("  wlan0 (02:00:00:00:00:00): on no network yet (scanning)\n           Home Net refused the passphrase that is stored for it: /network wifi join Home Net --key <passphrase> stores another\n"), "{shown}");
        assert!(shown.contains("  In range:\n    guest  weak, -80 dBm  open\n"), "{shown}");
        assert!(!shown.contains("wrong passphrase"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
