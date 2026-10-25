use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use futures::StreamExt;

use crate::db::WardsonDbClient;

// SSH paths on the writable DATA partition (rootfs is read-only SquashFS)
pub(crate) const SSH_KEY_PATH: &str = "/embra/workspace/.ssh/id_ed25519";
pub(crate) const SSH_KNOWN_HOSTS: &str = "/embra/workspace/.ssh/known_hosts";

/// The storage mounts and the flags the boot gives each one: the rootfs a
/// read-only SquashFS; STATE and DATA `nosuid,nodev,noexec` (embra-init);
/// the workspace bind remounted with the same flags and the ephemeral
/// tmpfs `nosuid,nodev,noexec` (embrad's `mount.rs`). `security_check`
/// reads `/proc/mounts` and prints each mount's options against this
/// table: the storage integrity flags its description promises.
const STORAGE_MOUNTS: [(&str, &[&str]); 5] = [
    ("/", &["ro"]),
    ("/embra/state", &["nosuid", "nodev", "noexec"]),
    ("/embra/data", &["nosuid", "nodev", "noexec"]),
    ("/embra/workspace", &["nosuid", "nodev", "noexec"]),
    ("/embra/ephemeral", &["nosuid", "nodev", "noexec"]),
];

/// The `Storage:` block of `security_check` from the text of
/// `/proc/mounts`: one line per mount of `STORAGE_MOUNTS` with its
/// filesystem type, its options as the kernel reports them and a verdict,
/// `ok` or `MISSING` with the expected flags the options lack. A mount
/// point listed more than once is read from its last line, the one in
/// effect; a mount point not listed is `not mounted`.
fn storage_report(mounts: &str) -> String {
    let mut out = String::from("Storage:\n");
    for (point, expected) in STORAGE_MOUNTS {
        let Some(line) = mounts.lines().rfind(|l| l.split_whitespace().nth(1) == Some(point)) else {
            out.push_str(&format!("  {point:<17} not mounted\n"));
            continue;
        };
        let mut fields = line.split_whitespace().skip(2);
        let fstype = fields.next().unwrap_or("?");
        let options = fields.next().unwrap_or("");
        let missing: Vec<&str> = expected
            .iter()
            .copied()
            .filter(|flag| !options.split(',').any(|o| o == *flag))
            .collect();
        let verdict = if missing.is_empty() {
            "ok".to_string()
        } else {
            format!("MISSING {}", missing.join(","))
        };
        out.push_str(&format!("  {point:<17} {fstype:<9} {options}  [{verdict}]\n"));
    }
    out
}

/// The system overview the tool's description promises, and nothing it
/// does not: processes, load, the listening ports, the seal's state and
/// the storage mounts' flags.
pub async fn security_check(db: &WardsonDbClient) -> String {
    let mut output = String::from("=== Security Check ===\n");

    // Process count
    match super::subprocess("sh")
        .arg("-c")
        .arg("ls /proc/*/status 2>/dev/null | wc -l")
        .output()
        .await
    {
        Ok(out) => {
            let count = String::from_utf8_lossy(&out.stdout).trim().to_string();
            output.push_str(&format!("Processes: {}\n", count));
        }
        Err(_) => output.push_str("Processes: unavailable\n"),
    }

    // Load average
    match tokio::fs::read_to_string("/proc/loadavg").await {
        Ok(loadavg) => {
            output.push_str(&format!("Load average: {}\n", loadavg.trim()));
        }
        Err(_) => output.push_str("Load average: unavailable\n"),
    }

    // Open listening ports from /proc/net/tcp
    match tokio::fs::read_to_string("/proc/net/tcp").await {
        Ok(tcp) => {
            let listening: Vec<String> = tcp
                .lines()
                .skip(1) // header
                .filter_map(|line| {
                    let fields: Vec<&str> = line.split_whitespace().collect();
                    if fields.len() > 3 {
                        let state = fields[3];
                        if state == "0A" {
                            // 0A = LISTEN
                            // Parse port from local_address (hex)
                            let local = fields[1];
                            if let Some(port_hex) = local.split(':').nth(1)
                                && let Ok(port) = u16::from_str_radix(port_hex, 16)
                            {
                                return Some(format!("{}", port));
                            }
                        }
                    }
                    None
                })
                .collect();

            if listening.is_empty() {
                output.push_str("Listening ports: none detected\n");
            } else {
                output.push_str(&format!("Listening ports: {}\n", listening.join(", ")));
            }
        }
        Err(_) => output.push_str("Listening ports: /proc/net/tcp unavailable\n"),
    }

    // The seal as the other status surfaces print it (`system_status`,
    // `GetMode`): re-verified now against the stored hash.
    output.push_str(&format!(
        "Soul: {}\n",
        crate::learning::seal_state(db).await.status_word()
    ));

    // The storage mounts and their flags, from the kernel's own table.
    match tokio::fs::read_to_string("/proc/mounts").await {
        Ok(mounts) => output.push_str(&storage_report(&mounts)),
        Err(_) => output.push_str("Storage: /proc/mounts unavailable\n"),
    }

    output
}

/// Check if an IP address is in a private/loopback range (RFC 1918 + loopback).
///
/// The gate of every SSH tool and of `/ssh-copy-id`: they connect to these
/// addresses and to no other. A name is not resolved — only `localhost`
/// passes — so nothing can point a name at a public address after the check.
pub fn is_private_address(host: &str) -> bool {
    let ip: std::net::IpAddr = match host.parse() {
        Ok(ip) => ip,
        Err(_) => {
            // Could be a hostname — resolve it and check
            // For safety, reject hostnames that aren't "localhost"
            return host == "localhost";
        }
    };

    match ip {
        std::net::IpAddr::V4(v4) => {
            let octets = v4.octets();
            // 127.0.0.0/8 (loopback)
            if octets[0] == 127 {
                return true;
            }
            // 10.0.0.0/8
            if octets[0] == 10 {
                return true;
            }
            // 172.16.0.0/12
            if octets[0] == 172 && (16..=31).contains(&octets[1]) {
                return true;
            }
            // 192.168.0.0/16
            if octets[0] == 192 && octets[1] == 168 {
                return true;
            }
            false
        }
        std::net::IpAddr::V6(v6) => {
            // ::1 loopback
            v6.is_loopback()
        }
    }
}

/// Default common ports list.
const COMMON_PORTS: &[(u16, &str)] = &[
    (21, "FTP"),
    (22, "SSH"),
    (23, "Telnet"),
    (25, "SMTP"),
    (53, "DNS"),
    (80, "HTTP"),
    (110, "POP3"),
    (143, "IMAP"),
    (443, "HTTPS"),
    (993, "IMAPS"),
    (995, "POP3S"),
    (3306, "MySQL"),
    (5432, "PostgreSQL"),
    (6379, "Redis"),
    (8080, "HTTP-Alt"),
    (8443, "HTTPS-Alt"),
    (27017, "MongoDB"),
];

/// Look up a COMMON_PORTS label or synthesize one.
fn port_label(p: u16) -> String {
    COMMON_PORTS
        .iter()
        .find(|&&(cp, _)| cp == p)
        .map(|&(_, s)| s.to_string())
        .unwrap_or_else(|| format!("port-{}", p))
}

/// Max number of ports selected from a user-supplied spec (comma/range mix).
/// Presets (`low`, `all`) are exempt — the operator has explicitly asked for them.
const MAX_USER_PORTS: usize = 2048;

/// The scan's own budget. An `all` scan is 65,535 connects at two seconds
/// each under [`PORT_SCAN_IN_FLIGHT`], far past what the dispatcher allows;
/// the scan reports what it found by this point, with the count checked.
const PORT_SCAN_MAX: Duration = Duration::from_secs(120);
/// Connections in flight at once.
const PORT_SCAN_IN_FLIGHT: usize = 50;

/// Parse a port specification into a list of (port, label) pairs. Accepts:
/// - empty → default 17 common ports
/// - `web` / `db` → named presets
/// - `low` → 1–1024 (all well-known ports)
/// - `all` → 1–65535 (exhaustive; may take many minutes)
/// - otherwise comma-separated list, each token either `N` or `A-B`.
///   Duplicates are de-duplicated; total capped at `MAX_USER_PORTS`.
fn parse_port_spec(spec: &str) -> Vec<(u16, String)> {
    let spec = spec.trim();
    if spec.is_empty() {
        return COMMON_PORTS.iter().map(|&(p, s)| (p, s.to_string())).collect();
    }

    match spec {
        "web" => return vec![
            (80, "HTTP".into()),
            (443, "HTTPS".into()),
            (8080, "HTTP-Alt".into()),
            (8443, "HTTPS-Alt".into()),
        ],
        "db" => return vec![
            (3306, "MySQL".into()),
            (5432, "PostgreSQL".into()),
            (6379, "Redis".into()),
            (27017, "MongoDB".into()),
            (5984, "CouchDB".into()),
        ],
        "low" => return (1u16..=1024).map(|p| (p, port_label(p))).collect(),
        "all" => return (1u16..=65535).map(|p| (p, port_label(p))).collect(),
        _ => {}
    }

    // Generic parse: comma-separated tokens, each either "N" or "A-B".
    let mut out: Vec<(u16, String)> = Vec::new();
    let mut seen = std::collections::HashSet::<u16>::new();
    for tok in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if let Some((a, b)) = tok.split_once('-') {
            let (Ok(start), Ok(end)) = (a.trim().parse::<u16>(), b.trim().parse::<u16>()) else {
                continue;
            };
            if start > end {
                continue;
            }
            for p in start..=end {
                if seen.insert(p) {
                    out.push((p, port_label(p)));
                    if out.len() >= MAX_USER_PORTS {
                        return out;
                    }
                }
            }
        } else if let Ok(p) = tok.parse::<u16>()
            && seen.insert(p)
        {
            out.push((p, port_label(p)));
            if out.len() >= MAX_USER_PORTS {
                return out;
            }
        }
    }
    out
}

/// Attempt banner grabbing on an open TCP connection.
async fn grab_banner(host: &str, port: u16) -> Option<String> {
    let addr = format!("{}:{}", host, port);
    let result = tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(&addr)).await;

    let mut stream = match result {
        Ok(Ok(s)) => s,
        _ => return None,
    };

    let mut buf = [0u8; 256];
    match tokio::time::timeout(Duration::from_secs(1), stream.read(&mut buf)).await {
        Ok(Ok(n)) if n > 0 => {
            let data = &buf[..n];
            let text = String::from_utf8_lossy(data);
            let banner = if text.starts_with("SSH-") {
                format!("SSH: {}", text.trim())
            } else if text.starts_with("HTTP/") {
                format!("HTTP: {}", text.lines().next().unwrap_or("").trim())
            } else if text.starts_with("220 ") {
                format!("SMTP/FTP: {}", text.trim())
            } else if text.starts_with("+OK") {
                format!("POP3: {}", text.trim())
            } else if text.starts_with("* OK") {
                format!("IMAP: {}", text.trim())
            } else {
                let preview: String = text.chars().take(64).collect();
                format!("Banner: {}", preview.trim())
            };
            Some(banner)
        }
        _ => None,
    }
}

/// TCP connect scan with port specs, banner grabbing, and concurrency.
/// Restricted to private/loopback addresses only (RFC 1918 + 127.0.0.0/8).
///
/// Param format: `<host> [ports]`
/// Port specs: empty (default 17 common), `80,443,8080`, `8000-8100`,
/// `8000-8100,443` (mixed), `web`, `db`, `low`, `all`.
pub async fn port_scan(param: &str) -> String {
    if param.is_empty() {
        return "Usage: port_scan <host> [ports]\n\
                Examples:\n  \
                  port_scan localhost              — default 17 common ports\n  \
                  port_scan 192.168.1.1 80,443     — specific ports\n  \
                  port_scan localhost 8000-8100    — port range\n  \
                  port_scan localhost 8000-8100,443 — mixed range and list\n  \
                  port_scan localhost web          — preset: 80, 443, 8080, 8443\n  \
                  port_scan localhost db           — preset: 3306, 5432, 6379, 27017, 5984\n  \
                  port_scan localhost low          — well-known 1-1024\n  \
                  port_scan localhost all          — exhaustive 1-65535 (cut at 120 s with what was found)\n\
                Custom ranges/lists are capped at 2048 distinct ports; presets are exempt.\n\
                Restricted to private/loopback addresses only."
            .into();
    }

    let parts: Vec<&str> = param.splitn(2, ' ').collect();
    let host = parts[0];
    let port_spec = if parts.len() > 1 { parts[1] } else { "" };

    if !is_private_address(host) {
        return format!(
            "Denied: '{}' is not a private address. port_scan is restricted to RFC 1918 \
             private ranges (10.x.x.x, 172.16-31.x.x, 192.168.x.x) and loopback (127.x.x.x).",
            host
        );
    }

    let ports = parse_port_spec(port_spec);
    if ports.is_empty() {
        return format!("Could not parse port spec: '{}'", port_spec);
    }

    let total = ports.len();
    let mut output = format!("=== Port Scan: {} ({} ports) ===\n", host, total);

    let deadline = tokio::time::Instant::now() + PORT_SCAN_MAX;
    let Scanned { mut open_ports, checked, cut } = scan_ports(host, ports, deadline).await;

    // Sort by port number (extracted from the formatted string)
    open_ports.sort();

    if open_ports.is_empty() {
        output.push_str("No open ports found.\n");
    } else {
        output.push_str(&format!("{} open port(s) found:\n", open_ports.len()));
        for p in &open_ports {
            output.push_str(p);
            output.push('\n');
        }
    }
    if cut {
        output.push_str(&cut_note(checked, total));
    }

    output
}

/// What a scan found by its deadline.
struct Scanned {
    /// One formatted line per open port, unsorted.
    open_ports: Vec<String>,
    /// How many ports were probed before the scan ended or was cut.
    checked: usize,
    /// Whether the deadline ended the scan with ports unchecked.
    cut: bool,
}

/// The ports, probed at most [`PORT_SCAN_IN_FLIGHT`] at once as a stream
/// rather than one task per port (an `all` scan is 65,535 of them), until
/// the last answers or `deadline` passes. Guard
/// `a_scan_past_its_deadline_is_cut_with_the_count_checked`.
async fn scan_ports(host: &str, ports: Vec<(u16, String)>, deadline: tokio::time::Instant) -> Scanned {
    let host_str = host.to_string();
    let mut probes = futures::stream::iter(ports)
        .map(|(port, label)| {
            let h = host_str.clone();
            async move { probe_port(&h, port, &label).await }
        })
        .buffer_unordered(PORT_SCAN_IN_FLIGHT);
    let mut open_ports = Vec::new();
    let mut checked = 0usize;
    let cut = loop {
        match tokio::time::timeout_at(deadline, probes.next()).await {
            Ok(Some(found)) => {
                checked += 1;
                if let Some(line) = found {
                    open_ports.push(line);
                }
            }
            Ok(None) => break false,
            Err(_) => break true,
        }
    };
    Scanned { open_ports, checked, cut }
}

/// One port: a connect under a two-second timeout, then a banner grab on
/// an open one. `Some(line)` for an open port.
async fn probe_port(host: &str, port: u16, label: &str) -> Option<String> {
    let addr = format!("{}:{}", host, port);
    let result = tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(&addr)).await;
    match result {
        Ok(Ok(stream)) => {
            drop(stream);
            let banner = grab_banner(host, port).await;
            let banner_str = banner.map(|b| format!(" [{}]", b)).unwrap_or_default();
            Some(format!("  {:>5} ({}) — OPEN{}", port, label, banner_str))
        }
        _ => None,
    }
}

/// The line a scan that ran out of its budget ends with.
fn cut_note(checked: usize, total: usize) -> String {
    format!(
        "Scan cut at {} s: {checked} of {total} ports checked. Narrow the port list to finish.\n",
        PORT_SCAN_MAX.as_secs()
    )
}

// ── SSH Remote Admin (EXPERIMENTAL) ──

/// Every ssh the tools run: the child is killed with its future
/// (`tools::subprocess`), so a timed-out connection does not linger.
/// Every ssh a tool runs: the child dies with its future (`subprocess`),
/// and no client configuration file is read (`-F /dev/null`): the one under
/// the workspace would be the model's, and a configuration can name a
/// command for ssh to run. The callers name the key, the known-hosts file
/// and the host-key policy explicitly. Git's ssh is pinned the same way
/// (`engineering/git.rs::git_ssh_command`).
fn ssh_command() -> tokio::process::Command {
    let mut cmd = super::subprocess("ssh");
    cmd.arg("-F").arg("/dev/null");
    cmd
}

/// The comment `/ssh-keygen` writes into the key, which is what the
/// operator reads beside it in an `authorized_keys` file: the
/// intelligence's name (`SystemConfig.name`) as a slug — lowercased, ASCII
/// letters, digits and hyphens, a run of anything else one hyphen — at
/// `embraos`, so two instances' keys read apart. No name reads `embra`.
/// The comment is a label with no part in authentication, and a key
/// generated earlier keeps the comment it was made with.
pub(crate) fn ssh_key_comment(name: &str) -> String {
    let mut slug = String::new();
    for c in name.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    let who = if slug.is_empty() { "embra" } else { slug };
    format!("{who}@embraos")
}

#[cfg(test)]
mod ssh_key_comment_tests {
    use super::ssh_key_comment;

    #[test]
    fn the_key_comment_is_the_instances_name_as_a_slug() {
        assert_eq!(ssh_key_comment("Meridian"), "meridian@embraos");
        assert_eq!(ssh_key_comment("Pathfinder Prime"), "pathfinder-prime@embraos");
        assert_eq!(ssh_key_comment("  Ode_7!  "), "ode-7@embraos");
        assert_eq!(ssh_key_comment("Émile"), "mile@embraos", "ASCII only, by design");
        for empty in ["", "   ", "!!!", "_-_"] {
            assert_eq!(ssh_key_comment(empty), "embra@embraos", "{empty:?}");
        }
    }
}

struct SshSession {
    user_host: String,      // "user@host"
    control_path: String,   // "/tmp/embra-ssh-{uuid}"
}

fn ssh_session_lock() -> &'static tokio::sync::Mutex<Option<SshSession>> {
    static SSH_SESSION: OnceLock<tokio::sync::Mutex<Option<SshSession>>> = OnceLock::new();
    SSH_SESSION.get_or_init(|| tokio::sync::Mutex::new(None))
}

/// Parse an SSH target string into (user, host, port). Supported forms:
/// - `user@host`       → (user, host, 22)
/// - `host`            → (root, host, 22)
/// - `user@host:port`  → (user, host, port)
/// - `host:port`       → (root, host, port)
///
/// If `port` is present but unparseable, it is silently ignored and 22 is
/// used (keeps the caller's `is_private_address` path robust; bad input still
/// fails the RFC 1918 check or the subsequent SSH connection, not this parser).
/// IPv6 bracketed hosts (`[::1]:22`) are deliberately not supported in this
/// pass — the existing `is_private_address` check only handles v4/v6 without
/// brackets, and adding full IPv6 URI parsing is out of scope here.
///
/// A user or host that begins with `-` is refused: ssh would read it as an
/// option (`-oProxyCommand=…` runs a command locally before any
/// connection). The user reaches ssh as `-l user` and the host alone, never
/// as one `user@host` argument.
fn parse_ssh_target(target: &str) -> Result<(String, String, u16), String> {
    if target.chars().any(char::is_whitespace) {
        return Err(
            "Denied: ssh target contains whitespace; give it as user@host or host[:port] with no spaces"
                .to_string(),
        );
    }
    let (user_part, host_port) = if let Some(at_pos) = target.find('@') {
        (target[..at_pos].to_string(), &target[at_pos + 1..])
    } else {
        ("root".to_string(), target)
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => match p.parse::<u16>() {
            Ok(pnum) if pnum > 0 => (h.to_string(), pnum),
            _ => (host_port.to_string(), 22),
        },
        None => (host_port.to_string(), 22),
    };
    for (what, value) in [("user", user_part.as_str()), ("host", host.as_str())] {
        if value.starts_with('-') {
            return Err(format!(
                "Denied: ssh {what} '{value}' begins with '-', which ssh would read as an option"
            ));
        }
    }
    Ok((user_part, host, port))
}

#[cfg(test)]
mod storage_report_tests {
    use super::storage_report;

    /// `/proc/mounts` as the OS shows it once embra-init and embrad have
    /// mounted: the rootfs read-only, the four writable mounts with the
    /// three flags, the workspace a bind of DATA.
    const BOOTED: &str = "\
/dev/vda2 / squashfs ro,relatime,errors=continue 0 0
proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0
sysfs /sys sysfs rw,nosuid,nodev,noexec,relatime 0 0
devtmpfs /dev devtmpfs rw,nosuid,relatime,size=1986104k,nr_inodes=496526,mode=755 0 0
/dev/vda3 /embra/state ext4 rw,nosuid,nodev,noexec,relatime 0 0
/dev/vda4 /embra/data ext4 rw,nosuid,nodev,noexec,relatime 0 0
tmpfs /embra/ephemeral tmpfs rw,nosuid,nodev,noexec,relatime,size=524288k 0 0
/dev/vda4 /embra/workspace ext4 rw,nosuid,nodev,noexec,relatime 0 0
";

    fn words(report: &str, point: &str) -> Vec<String> {
        report
            .lines()
            .find(|l| l.split_whitespace().next() == Some(point))
            .unwrap_or_else(|| panic!("no line for {point} in {report}"))
            .split_whitespace()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn every_boot_mount_reads_ok_and_a_flag_that_is_lacking_is_named() {
        let report = storage_report(BOOTED);
        assert_eq!(report.lines().next(), Some("Storage:"));
        assert_eq!(report.lines().count(), 6, "{report}");
        for line in report.lines().skip(1) {
            assert!(line.ends_with("  [ok]"), "{line}");
        }
        assert_eq!(words(&report, "/"), ["/", "squashfs", "ro,relatime,errors=continue", "[ok]"]);
        assert_eq!(
            words(&report, "/embra/ephemeral"),
            ["/embra/ephemeral", "tmpfs", "rw,nosuid,nodev,noexec,relatime,size=524288k", "[ok]"]
        );

        let relaxed = BOOTED
            .replace("/embra/data ext4 rw,nosuid,nodev,noexec,relatime", "/embra/data ext4 rw,nosuid,nodev,relatime")
            .replace("/ squashfs ro,", "/ squashfs rw,");
        let report = storage_report(&relaxed);
        assert_eq!(words(&report, "/"), ["/", "squashfs", "rw,relatime,errors=continue", "[MISSING", "ro]"]);
        assert_eq!(
            words(&report, "/embra/data"),
            ["/embra/data", "ext4", "rw,nosuid,nodev,relatime", "[MISSING", "noexec]"]
        );
        assert_eq!(
            words(&report, "/embra/state"),
            ["/embra/state", "ext4", "rw,nosuid,nodev,noexec,relatime", "[ok]"]
        );
    }

    #[test]
    fn an_absent_mount_is_said_and_an_over_mount_is_read_from_its_last_line() {
        let without_workspace: String = BOOTED
            .lines()
            .filter(|l| !l.contains("/embra/workspace"))
            .map(|l| format!("{l}\n"))
            .collect();
        assert_eq!(
            words(&storage_report(&without_workspace), "/embra/workspace"),
            ["/embra/workspace", "not", "mounted"]
        );

        // A plain bind over the hardened one is what is in effect ...
        let relaxed_last = format!("{BOOTED}/dev/vda4 /embra/workspace ext4 rw,relatime 0 0\n");
        assert_eq!(
            words(&storage_report(&relaxed_last), "/embra/workspace"),
            ["/embra/workspace", "ext4", "rw,relatime", "[MISSING", "nosuid,nodev,noexec]"]
        );
        // ... and the remount's line after it is, again.
        let hardened_last =
            format!("{relaxed_last}/dev/vda4 /embra/workspace ext4 rw,nosuid,nodev,noexec,relatime 0 0\n");
        assert_eq!(
            words(&storage_report(&hardened_last), "/embra/workspace"),
            ["/embra/workspace", "ext4", "rw,nosuid,nodev,noexec,relatime", "[ok]"]
        );

        // A mount point is matched whole, and an empty table names every mount.
        assert_eq!(
            words(&storage_report("/dev/vda4 /embra/data2 ext4 rw,nosuid,nodev,noexec 0 0\n"), "/embra/data"),
            ["/embra/data", "not", "mounted"]
        );
        let empty = storage_report("");
        assert_eq!(empty.lines().count(), 6, "{empty}");
        assert!(empty.lines().skip(1).all(|l| l.ends_with(" not mounted")), "{empty}");
    }
}

#[cfg(test)]
mod private_address_tests {
    use super::{
        is_private_address, parse_ssh_target, port_scan, ssh_remote_admin, ssh_remote_admin_argv,
        ssh_session_start,
    };

    #[test]
    fn a_target_with_whitespace_is_refused() {
        // The bypassing shape from the Part 3 log: a space splits off a
        // second token. The parser refuses any whitespace outright.
        for t in [
            "root@192.168.1.10 -oProxyCommand=id",
            "192.168.1.10\t-oProxyCommand=id",
            "192.168.1.10\n-oProxyCommand=id",
            " 192.168.1.10",
        ] {
            let e = parse_ssh_target(t).unwrap_err();
            assert!(e.contains("whitespace"), "{t}: {e}");
        }
        assert_eq!(
            parse_ssh_target("root@192.168.1.10").unwrap(),
            ("root".to_string(), "192.168.1.10".to_string(), 22)
        );
    }

    #[test]
    fn ssh_remote_admin_puts_a_double_dash_before_the_destination() {
        // An option-shaped remote command sits after `--`, where ssh reads
        // it as the command and never permutes it ahead of the destination.
        let argv = ssh_remote_admin_argv("root", "192.168.1.10", 22, "-oProxyCommand=id true");
        let dd = argv.iter().position(|a| a == "--").expect("-- present");
        assert_eq!(
            &argv[dd + 1..],
            &[
                "192.168.1.10".to_string(),
                "-oProxyCommand=id true".to_string()
            ]
        );
        assert!(argv[..dd].iter().any(|a| a == "-l"), "options precede --");
    }

    #[test]
    fn the_private_ranges_and_loopback_pass() {
        for host in [
            "10.0.0.1",
            "10.255.255.254",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.20",
            "127.0.0.1",
            "127.8.9.10",
            "::1",
            "localhost",
        ] {
            assert!(is_private_address(host), "{host}");
        }
    }

    #[test]
    fn every_other_address_is_refused() {
        for host in [
            "8.8.8.8",
            // One step outside each range.
            "9.255.255.255",
            "11.0.0.1",
            "172.15.255.255",
            "172.32.0.1",
            "192.167.255.255",
            "192.169.0.1",
            "126.255.255.255",
            "128.0.0.1",
            // Link-local, carrier-grade NAT, unspecified.
            "169.254.1.1",
            "100.64.0.1",
            "0.0.0.0",
            // IPv6 other than loopback, a private IPv4 address mapped into it too.
            "fc00::1",
            "2001:db8::1",
            "::ffff:10.0.0.1",
            // A name is not resolved, whatever it looks like.
            "example.com",
            "gitlab.ops.wsds",
            "10.0.0.1.example.com",
            "localhost.example.com",
            // Not an address.
            "",
            "10.0.0",
            "010.0.0.1",
            "10.0.0.1:22",
        ] {
            assert!(!is_private_address(host), "{host}");
        }
    }

    #[tokio::test]
    async fn the_ssh_tools_refuse_a_public_address_before_they_connect() {
        let out = ssh_remote_admin("root@8.8.8.8 uptime").await;
        assert!(out.starts_with("Denied:"), "{out}");
        let out = ssh_session_start("root@8.8.8.8").await;
        assert!(out.starts_with("Denied:"), "{out}");
    }

    /// An option-shaped user is refused before ssh runs: as one `user@host`
    /// argument it would reach ssh's option parser, and `-oProxyCommand=…`
    /// runs a command locally before any connection (#8, #56).
    #[tokio::test]
    async fn an_option_shaped_ssh_user_is_refused_before_ssh_runs() {
        let out = ssh_session_start("-oProxyCommand=id@192.168.1.10").await;
        assert!(out.starts_with("Denied: ssh user"), "{out}");
        let out = ssh_remote_admin("-oProxyCommand=id@192.168.1.10 uptime").await;
        assert!(out.starts_with("Denied: ssh user"), "{out}");
    }

    /// `port_scan` refuses a public host at its call site, before any
    /// connect; loopback is the positive control (#56).
    #[tokio::test]
    async fn port_scan_refuses_a_public_host_before_it_connects() {
        assert!(port_scan("8.8.8.8 80").await.starts_with("Denied:"));
        assert!(!port_scan("127.0.0.1 1-2").await.starts_with("Denied:"));
    }
}

#[cfg(test)]
mod parse_ssh_target_tests {
    use super::parse_ssh_target;

    #[test]
    fn bare_host_defaults_user_and_port() {
        assert_eq!(
            parse_ssh_target("192.168.1.10").unwrap(),
            ("root".into(), "192.168.1.10".into(), 22)
        );
    }

    #[test]
    fn user_at_host_keeps_port_default() {
        assert_eq!(
            parse_ssh_target("will@192.168.1.10").unwrap(),
            ("will".into(), "192.168.1.10".into(), 22)
        );
    }

    #[test]
    fn host_with_port_defaults_user() {
        assert_eq!(
            parse_ssh_target("192.168.1.10:2222").unwrap(),
            ("root".into(), "192.168.1.10".into(), 2222)
        );
    }

    #[test]
    fn user_at_host_with_port() {
        assert_eq!(
            parse_ssh_target("will@192.168.1.10:2222").unwrap(),
            ("will".into(), "192.168.1.10".into(), 2222)
        );
    }

    #[test]
    fn unparseable_port_falls_back_to_22() {
        // "abc" is not a valid port; the whole host_port is treated as the host.
        let (u, h, p) = parse_ssh_target("192.168.1.10:abc").unwrap();
        assert_eq!(u, "root");
        assert_eq!(p, 22);
        assert_eq!(h, "192.168.1.10:abc");
    }

    #[test]
    fn port_zero_falls_back_to_22() {
        let (_u, _h, p) = parse_ssh_target("host:0").unwrap();
        assert_eq!(p, 22);
    }

    /// An option-shaped user or host is refused, in the words the tools
    /// print; a plain target still parses (#8).
    #[test]
    fn an_option_shaped_user_or_host_is_refused() {
        let err = parse_ssh_target("-oProxyCommand=id@192.168.1.10").unwrap_err();
        assert!(err.starts_with("Denied: ssh user '-oProxyCommand=id'"), "{err}");
        let err = parse_ssh_target("-h").unwrap_err();
        assert!(err.starts_with("Denied: ssh host '-h'"), "{err}");
        let err = parse_ssh_target("will@-x:22").unwrap_err();
        assert!(err.starts_with("Denied: ssh host '-x'"), "{err}");
        assert!(parse_ssh_target("will@192.168.1.10:22").is_ok());
    }
}

/// The per-call arguments for the one-shot ssh, after `ssh_command`'s
/// `-F /dev/null`. `--` ends option parsing before the destination, so a
/// host or a remote command that begins with `-` is a positional and is
/// never permuted ahead of the destination to be read as a local option.
/// The destination and the command are the only positionals, in that order.
/// Guard `ssh_remote_admin_puts_a_double_dash_before_the_destination`.
fn ssh_remote_admin_argv(user: &str, host: &str, port: u16, command: &str) -> Vec<String> {
    vec![
        "-p".into(),
        port.to_string(),
        "-i".into(),
        SSH_KEY_PATH.into(),
        "-o".into(),
        format!("UserKnownHostsFile={SSH_KNOWN_HOSTS}"),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "PasswordAuthentication=no".into(),
        "-l".into(),
        user.into(),
        "--".into(),
        host.into(),
        command.into(),
    ]
}

/// Execute a single command on a remote host via SSH (EXPERIMENTAL).
/// Param format: `<user@host> <command>` or `<host> <command>`
pub async fn ssh_remote_admin(param: &str) -> String {
    if param.is_empty() {
        return "Usage: ssh_remote_admin <host> <command> or ssh_remote_admin user@host[:port <command>]".into();
    }

    let parts: Vec<&str> = param.splitn(2, ' ').collect();
    if parts.len() < 2 {
        return "Usage: ssh_remote_admin <host> <command>".into();
    }

    let target = parts[0];
    let command = parts[1];
    let (user, host, port) = match parse_ssh_target(target) {
        Ok(t) => t,
        Err(e) => return e,
    };

    if !is_private_address(&host) {
        return format!(
            "Denied: '{}' is not a private address. ssh_remote_admin is restricted to RFC 1918 \
             private ranges (10.x.x.x, 172.16-31.x.x, 192.168.x.x) and loopback (127.x.x.x).",
            host
        );
    }

    let host_display = if port == 22 {
        format!("{}@{}", user, host)
    } else {
        format!("{}@{}:{}", user, host, port)
    };

    let result = ssh_command()
        .args(ssh_remote_admin_argv(&user, &host, port, command))
        .output()
        .await;

    match result {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let code = output.status.code().unwrap_or(-1);

            format!(
                "[EXPERIMENTAL] SSH Remote Admin — use at your own risk\n\
                 Host: {}\n\
                 Command: {}\n\
                 Exit code: {}\n\n\
                 --- stdout ---\n{}\n\
                 --- stderr ---\n{}",
                host_display, command, code, stdout.trim(), stderr.trim()
            )
        }
        Err(e) => format!(
            "[EXPERIMENTAL] SSH Remote Admin — use at your own risk\n\
             Host: {}\n\
             Command: {}\n\
             Error: {}",
            host_display, command, e
        ),
    }
}

/// Open a persistent SSH session via ControlMaster (EXPERIMENTAL).
/// Param: `<user@host[:port]>` or `<host[:port]>` (default user: root, default port: 22)
pub async fn ssh_session_start(param: &str) -> String {
    if param.is_empty() {
        return "Usage: ssh_session_start <user@host> or ssh_session_start <user@host:port> or ssh_session_start <host>".into();
    }

    let (user, host, port) = match parse_ssh_target(param.trim()) {
        Ok(t) => t,
        Err(e) => return e,
    };

    if !is_private_address(&host) {
        return format!(
            "Denied: '{}' is not a private address. SSH sessions are restricted to RFC 1918 \
             private ranges and loopback.",
            host
        );
    }

    let mut lock = ssh_session_lock().lock().await;
    if lock.is_some() {
        return "An SSH session is already open. Use ssh_session_end first.".into();
    }

    let user_host = format!("{}@{}", user, host);
    let user_host_port_display = if port == 22 {
        user_host.clone()
    } else {
        format!("{}:{}", user_host, port)
    };
    let control_path = format!("/tmp/embra-ssh-{}", &uuid::Uuid::new_v4().to_string()[..8]);

    // Start ControlMaster in background (-MNf): authenticates, then backgrounds itself
    let master_result = tokio::time::timeout(
        Duration::from_secs(15),
        ssh_command()
            .arg("-MNf")
            .arg("-p").arg(port.to_string())
            .arg("-i").arg(SSH_KEY_PATH)
            .arg("-o").arg(format!("UserKnownHostsFile={}", SSH_KNOWN_HOSTS))
            .arg("-o").arg(format!("ControlPath={}", control_path))
            .arg("-o").arg("ControlPersist=no")
            .arg("-o").arg("StrictHostKeyChecking=accept-new")
            .arg("-o").arg("ConnectTimeout=10")
            .arg("-o").arg("BatchMode=yes")
            .arg("-o").arg("ServerAliveInterval=15")
            .arg("-o").arg("ServerAliveCountMax=3")
            .arg("-l").arg(&user)
            .arg("--")
            .arg(&host)
            .output(),
    )
    .await;

    match master_result {
        Ok(Ok(output)) if output.status.success() => {
            // ControlMaster backgrounded successfully — validate with a probe command.
            // Subsequent connections via the ControlPath socket inherit the
            // master's port, so `-p` is not needed here.
            let probe_result = tokio::time::timeout(
                Duration::from_secs(10),
                ssh_command()
                    .arg("-o").arg(format!("ControlPath={}", control_path))
                    .arg("--")
                    .arg(&user_host)
                    .arg("echo embra_probe_ok")
                    .output(),
            )
            .await;

            match probe_result {
                Ok(Ok(probe_out)) => {
                    let stdout = String::from_utf8_lossy(&probe_out.stdout);
                    if stdout.contains("embra_probe_ok") {
                        *lock = Some(SshSession {
                            user_host: user_host.clone(),
                            control_path,
                        });
                        format!(
                            "[EXPERIMENTAL] SSH session opened to {}. Use ssh_session_exec to run commands, ssh_session_end to close.",
                            user_host_port_display
                        )
                    } else {
                        // Probe didn't return expected output — tear down
                        let _ = ssh_command()
                            .arg("-O").arg("exit")
                            .arg("-o").arg(format!("ControlPath={}", control_path))
                            .arg("--")
                            .arg(&user_host)
                            .output().await;
                        let _ = std::fs::remove_file(&control_path);
                        let stderr = String::from_utf8_lossy(&probe_out.stderr);
                        format!(
                            "[EXPERIMENTAL] SSH connection to {} failed: probe returned unexpected output. {}",
                            user_host_port_display, stderr.trim()
                        )
                    }
                }
                Ok(Err(e)) => {
                    let _ = std::fs::remove_file(&control_path);
                    format!("[EXPERIMENTAL] SSH connection to {} failed: {}", user_host_port_display, e)
                }
                Err(_) => {
                    let _ = ssh_command()
                        .arg("-O").arg("exit")
                        .arg("-o").arg(format!("ControlPath={}", control_path))
                        .arg("--")
                        .arg(&user_host)
                        .output().await;
                    let _ = std::fs::remove_file(&control_path);
                    format!("[EXPERIMENTAL] SSH connection to {} failed: probe timed out", user_host_port_display)
                }
            }
        }
        Ok(Ok(output)) => {
            // ControlMaster exited with non-zero — connection failed
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = std::fs::remove_file(&control_path);
            format!("[EXPERIMENTAL] SSH connection to {} failed: {}", user_host_port_display, stderr.trim())
        }
        Ok(Err(e)) => {
            format!("[EXPERIMENTAL] Failed to start SSH session: {}", e)
        }
        Err(_) => {
            let _ = std::fs::remove_file(&control_path);
            format!("[EXPERIMENTAL] SSH connection to {} failed: connection timed out", user_host_port_display)
        }
    }
}

/// Run a command in the open SSH session (EXPERIMENTAL).
/// Each command runs as a discrete SSH process through the ControlMaster socket.
/// Param: command to execute
pub async fn ssh_session_exec(param: &str) -> String {
    if param.is_empty() {
        return "Usage: ssh_session_exec <command>".into();
    }

    let lock = ssh_session_lock().lock().await;
    let session = match lock.as_ref() {
        Some(s) => s,
        None => return "No SSH session open. Use ssh_session_start first.".into(),
    };

    // Check that the ControlMaster socket still exists
    if !std::path::Path::new(&session.control_path).exists() {
        return format!(
            "[EXPERIMENTAL] SSH session to {} lost — control socket missing. \
             Use ssh_session_end and start a new session.",
            session.user_host
        );
    }

    // Run command as a discrete SSH process through the ControlMaster socket
    let result = ssh_command()
        .arg("-o").arg(format!("ControlPath={}", session.control_path))
        .arg("-o").arg("ConnectTimeout=10")
        .arg("--")
        .arg(&session.user_host)
        .arg(param)
        .output()
        .await;

    match result {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let code = output.status.code().unwrap_or(-1);

            let mut combined = stdout.to_string();

            if !stderr.trim().is_empty() {
                combined.push_str(&format!("\n[stderr]:\n{}", stderr.trim()));
            }

            if code != 0 {
                combined.push_str(&format!("\n[exit code: {}]", code));
            }

            format!(
                "[EXPERIMENTAL] SSH session exec on {}\n\
                 Command: {}\n\n\
                 --- output ---\n{}",
                session.user_host, param, combined.trim()
            )
        }
        Err(e) => format!(
            "[EXPERIMENTAL] SSH session exec on {}\n\
             Command: {}\n\
             Error: {}",
            session.user_host, param, e
        ),
    }
}

/// Close the open SSH session (EXPERIMENTAL).
/// Tears down the ControlMaster connection and cleans up the socket file.
pub async fn ssh_session_end() -> String {
    let mut lock = ssh_session_lock().lock().await;
    match lock.take() {
        Some(session) => {
            // Send exit signal to ControlMaster
            let exit_result = tokio::time::timeout(
                Duration::from_secs(5),
                ssh_command()
                    .arg("-O").arg("exit")
                    .arg("-o").arg(format!("ControlPath={}", session.control_path))
                    .arg("--")
                    .arg(&session.user_host)
                    .output(),
            )
            .await;

            // Best-effort socket cleanup
            let _ = std::fs::remove_file(&session.control_path);

            match exit_result {
                Ok(Ok(_)) => format!(
                    "[EXPERIMENTAL] SSH session to {} closed.",
                    session.user_host
                ),
                _ => format!(
                    "[EXPERIMENTAL] SSH session to {} closed (forced cleanup).",
                    session.user_host
                ),
            }
        }
        None => "No SSH session is open.".into(),
    }
}

#[cfg(test)]
mod parse_port_spec_tests {
    use super::parse_port_spec;

    #[test]
    fn empty_returns_common_ports() {
        let out = parse_port_spec("");
        assert_eq!(out.len(), 17); // matches COMMON_PORTS
        assert!(out.iter().any(|(p, _)| *p == 22));
    }

    #[test]
    fn preset_low_is_1024_ports() {
        assert_eq!(parse_port_spec("low").len(), 1024);
    }

    #[test]
    fn preset_all_is_65535_ports() {
        assert_eq!(parse_port_spec("all").len(), 65535);
    }

    #[test]
    fn range_plus_commas_with_dedup() {
        let out = parse_port_spec("80,443,8000-8002,443");
        let nums: Vec<u16> = out.iter().map(|(p, _)| *p).collect();
        assert_eq!(nums, vec![80, 443, 8000, 8001, 8002]);
    }

    #[test]
    fn cap_applies_only_to_user_ranges() {
        // 3000-port request clamps to MAX_USER_PORTS (2048)
        assert_eq!(parse_port_spec("1-3000").len(), 2048);
    }

    #[test]
    fn mixed_range_comma_reproducer_finding8() {
        // Finding 8 repro: "50000-50002,8090" used to scan only 8090.
        let nums: Vec<u16> = parse_port_spec("50000-50002,8090")
            .iter()
            .map(|(p, _)| *p)
            .collect();
        assert_eq!(nums, vec![50000, 50001, 50002, 8090]);
    }

    #[test]
    fn web_and_db_presets() {
        assert_eq!(parse_port_spec("web").len(), 4);
        assert_eq!(parse_port_spec("db").len(), 5);
    }
}

// ── Native tool-use registrations (NATIVE-TOOLS-01) ──

use embra_tool_macro::embra_tool;
use embra_tools_core::DispatchError;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::tools::registry::DispatchContext;

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "security_check",
    description = "System security overview: processes, load, open ports, soul status, and storage integrity flags."
)]
pub struct SecurityCheckArgs {}

impl SecurityCheckArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(security_check(ctx.db).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "port_scan",
    description = "TCP scan with banner grabbing. Restricted to RFC 1918 private ranges and loopback. Default (no ports) scans 17 common ports. Accepts port specs: \"80,443,8080\" (list), \"8000-8100\" (range), mixed, or presets \"web\" (80,443,8080,8443), \"db\" (3306,5432,6379,27017,5984), \"low\" (1-1024), \"all\" (1-65535)."
)]
pub struct PortScanArgs {
    /// Host/IP (private/loopback only).
    pub host: String,
    /// Port spec. Empty = 17 common ports.
    #[serde(default)]
    pub ports: String,
}

impl PortScanArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = if self.ports.is_empty() {
            self.host
        } else {
            format!("{} {}", self.host, self.ports)
        };
        Ok(port_scan(&param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "ssh_remote_admin",
    is_side_effectful = true,
    description = "Execute a single command on a remote host via SSH (EXPERIMENTAL). Restricted to RFC 1918 private ranges and loopback. target format: host OR user@host OR user@host:port."
)]
pub struct SshRemoteAdminArgs {
    /// SSH target: host OR user@host OR user@host:port.
    pub target: String,
    /// Command to execute on the remote host.
    pub command: String,
}

impl SshRemoteAdminArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{} {}", self.target, self.command);
        Ok(ssh_remote_admin(&param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "ssh_session_start",
    is_side_effectful = true,
    description = "Open a persistent SSH session (EXPERIMENTAL; private/loopback only). target format: user@host or user@host:port or host. At most one session open at a time."
)]
pub struct SshSessionStartArgs {
    /// SSH target. Required.
    pub target: String,
}

impl SshSessionStartArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(ssh_session_start(&self.target).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "ssh_session_exec",
    is_side_effectful = true,
    description = "Run a command in the open SSH session. Each command runs in a fresh process; state between commands is not preserved by the shell."
)]
pub struct SshSessionExecArgs {
    /// Command to run on the remote host.
    pub command: String,
}

impl SshSessionExecArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(ssh_session_exec(&self.command).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "ssh_session_end",
    description = "Close the currently open SSH session and tear down the ControlMaster connection."
)]
pub struct SshSessionEndArgs {}

impl SshSessionEndArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(ssh_session_end().await)
    }
}

#[cfg(test)]
mod native_args_tests {
    use super::*;

    /// Every ssh the tools run ignores the client configuration files; the
    /// callers' options follow.
    #[test]
    fn every_ssh_the_tools_run_reads_no_client_configuration() {
        let cmd = ssh_command();
        assert_eq!(cmd.as_std().get_program(), std::ffi::OsStr::new("ssh"));
        let args: Vec<_> = cmd.as_std().get_args().collect();
        assert_eq!(args, [std::ffi::OsStr::new("-F"), std::ffi::OsStr::new("/dev/null")]);
    }

    #[test]
    fn port_scan_host_required_ports_optional() {
        let a: PortScanArgs =
            serde_json::from_value(serde_json::json!({"host": "127.0.0.1"})).unwrap();
        assert_eq!(a.host, "127.0.0.1");
        assert_eq!(a.ports, "");

        let b: PortScanArgs =
            serde_json::from_value(serde_json::json!({"host": "127.0.0.1", "ports": "web"}))
                .unwrap();
        assert_eq!(b.ports, "web");

        let err =
            serde_json::from_value::<PortScanArgs>(serde_json::json!({"ports": "web"})).unwrap_err();
        assert!(err.to_string().contains("host"));
    }

    #[test]
    fn ssh_remote_admin_requires_both_fields() {
        let a: SshRemoteAdminArgs = serde_json::from_value(serde_json::json!({
            "target": "user@192.168.1.10", "command": "uname -a"
        }))
        .unwrap();
        assert_eq!(a.target, "user@192.168.1.10");
        assert_eq!(a.command, "uname -a");

        let err = serde_json::from_value::<SshRemoteAdminArgs>(
            serde_json::json!({"target": "192.168.1.10"}),
        )
        .unwrap_err();
        assert!(err.to_string().contains("command"));
    }

    #[test]
    fn security_tools_register() {
        let names: Vec<&'static str> = inventory::iter::<crate::tools::registry::ToolDescriptor>()
            .map(|d| d.name)
            .filter(|n| {
                matches!(
                    *n,
                    "security_check"
                        | "port_scan"
                        | "ssh_remote_admin"
                        | "ssh_session_start"
                        | "ssh_session_exec"
                        | "ssh_session_end"
                )
            })
            .collect();
        assert_eq!(names.len(), 6, "all 6 security tools registered: {:?}", names);
    }
}

#[cfg(test)]
mod scan_budget_tests {
    use super::{cut_note, port_scan, scan_ports, PORT_SCAN_IN_FLIGHT, PORT_SCAN_MAX};

    /// The budget path itself, without a network that drops packets: a
    /// deadline already reached cuts the scan on its first poll, and the
    /// report counts what was checked; a deadline far away lets the same
    /// loopback ports finish.
    #[tokio::test]
    async fn a_scan_past_its_deadline_is_cut_with_the_count_checked() {
        let ports = |n: u16| (1..=n).map(|p| (p, format!("port-{p}"))).collect::<Vec<_>>();
        let cut = scan_ports("127.0.0.1", ports(300), tokio::time::Instant::now()).await;
        assert!(cut.cut, "a deadline already reached cuts the scan");
        assert!(cut.checked < 300, "ports were left unchecked: {}", cut.checked);
        let done = scan_ports(
            "127.0.0.1",
            ports(3),
            tokio::time::Instant::now() + std::time::Duration::from_secs(30),
        )
        .await;
        assert!(!done.cut);
        assert_eq!(done.checked, 3);
    }

    #[test]
    fn the_cut_note_names_the_budget_and_the_count() {
        assert_eq!(PORT_SCAN_MAX.as_secs(), 120);
        assert_eq!(PORT_SCAN_IN_FLIGHT, 50);
        let note = cut_note(3, 65535);
        assert!(note.starts_with("Scan cut at 120 s: 3 of 65535 ports checked."), "{note}");
    }

    /// The stream path end to end on a port that answers at once: the
    /// scan completes, counts its one port and is not cut.
    #[tokio::test]
    async fn a_scan_of_one_loopback_port_completes_within_its_budget() {
        let out = port_scan("127.0.0.1 1").await;
        assert!(out.starts_with("=== Port Scan: 127.0.0.1 (1 ports) ==="), "{out}");
        assert!(out.contains("No open ports found.") || out.contains("open port(s) found"), "{out}");
        assert!(!out.contains("Scan cut"), "{out}");
    }
}
