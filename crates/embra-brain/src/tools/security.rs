use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::Semaphore;

// SSH paths on the writable DATA partition (rootfs is read-only SquashFS)
const SSH_KEY_PATH: &str = "/embra/workspace/.ssh/id_ed25519";
const SSH_KNOWN_HOSTS: &str = "/embra/workspace/.ssh/known_hosts";

/// Truncate a string to at most `max_bytes`, snapping to a char boundary.
fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Run a basic security/system check by reading container info.
pub async fn security_check() -> String {
    let mut output = String::from("=== Security Check ===\n");

    // Process count
    match tokio::process::Command::new("sh")
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
                            if let Some(port_hex) = local.split(':').nth(1) {
                                if let Ok(port) = u16::from_str_radix(port_hex, 16) {
                                    return Some(format!("{}", port));
                                }
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

    // Container detection
    let in_container = tokio::fs::metadata("/.dockerenv").await.is_ok();
    output.push_str(&format!(
        "Container: {}\n",
        if in_container { "yes (Docker)" } else { "not detected" }
    ));

    output
}

/// Check if an IP address is in a private/loopback range (RFC 1918 + loopback).
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
        } else if let Ok(p) = tok.parse::<u16>() {
            if seen.insert(p) {
                out.push((p, port_label(p)));
                if out.len() >= MAX_USER_PORTS {
                    return out;
                }
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
                  port_scan localhost all          — exhaustive 1-65535 (may take many minutes)\n\
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

    let mut output = format!("=== Port Scan: {} ({} ports) ===\n", host, ports.len());

    // Use semaphore for concurrency control (max 50 concurrent connections)
    let semaphore = Arc::new(Semaphore::new(50));
    let host_str = host.to_string();

    let handles: Vec<_> = ports
        .into_iter()
        .map(|(port, label)| {
            let sem = semaphore.clone();
            let h = host_str.clone();
            tokio::spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                let addr = format!("{}:{}", h, port);
                let result = tokio::time::timeout(
                    Duration::from_secs(2),
                    TcpStream::connect(&addr),
                )
                .await;

                match result {
                    Ok(Ok(_stream)) => {
                        drop(_stream);
                        // Try banner grab
                        let banner = grab_banner(&h, port).await;
                        let banner_str = banner
                            .map(|b| format!(" [{}]", b))
                            .unwrap_or_default();
                        Some(format!("  {:>5} ({}) — OPEN{}", port, label, banner_str))
                    }
                    _ => None,
                }
            })
        })
        .collect();

    let mut open_ports = Vec::new();
    for handle in handles {
        if let Ok(Some(result)) = handle.await {
            open_ports.push(result);
        }
    }

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

    output
}

// ── SSH Remote Admin (EXPERIMENTAL) ──

struct SshSession {
    user_host: String,      // "user@host"
    port: u16,              // SSH port (default 22)
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
/// If `port` is present but unparseable, it is silently ignored and 22 is
/// used (keeps the caller's `is_private_address` path robust; bad input still
/// fails the RFC 1918 check or the subsequent SSH connection, not this parser).
/// IPv6 bracketed hosts (`[::1]:22`) are deliberately not supported in this
/// pass — the existing `is_private_address` check only handles v4/v6 without
/// brackets, and adding full IPv6 URI parsing is out of scope here.
fn parse_ssh_target(target: &str) -> (String, String, u16) {
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
    (user_part, host, port)
}

#[cfg(test)]
mod parse_ssh_target_tests {
    use super::parse_ssh_target;

    #[test]
    fn bare_host_defaults_user_and_port() {
        assert_eq!(
            parse_ssh_target("192.168.1.10"),
            ("root".into(), "192.168.1.10".into(), 22)
        );
    }

    #[test]
    fn user_at_host_keeps_port_default() {
        assert_eq!(
            parse_ssh_target("will@192.168.1.10"),
            ("will".into(), "192.168.1.10".into(), 22)
        );
    }

    #[test]
    fn host_with_port_defaults_user() {
        assert_eq!(
            parse_ssh_target("192.168.1.10:2222"),
            ("root".into(), "192.168.1.10".into(), 2222)
        );
    }

    #[test]
    fn user_at_host_with_port() {
        assert_eq!(
            parse_ssh_target("will@192.168.1.10:2222"),
            ("will".into(), "192.168.1.10".into(), 2222)
        );
    }

    #[test]
    fn unparseable_port_falls_back_to_22() {
        // "abc" is not a valid port; the whole host_port is treated as the host.
        let (u, h, p) = parse_ssh_target("192.168.1.10:abc");
        assert_eq!(u, "root");
        assert_eq!(p, 22);
        assert_eq!(h, "192.168.1.10:abc");
    }

    #[test]
    fn port_zero_falls_back_to_22() {
        let (_u, _h, p) = parse_ssh_target("host:0");
        assert_eq!(p, 22);
    }
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
    let (user, host, port) = parse_ssh_target(target);

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

    let result = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new("ssh")
            .arg("-p").arg(port.to_string())
            .arg("-i").arg(SSH_KEY_PATH)
            .arg("-o").arg(format!("UserKnownHostsFile={}", SSH_KNOWN_HOSTS))
            .arg("-o").arg("StrictHostKeyChecking=accept-new")
            .arg("-o").arg("ConnectTimeout=10")
            .arg("-o").arg("BatchMode=yes")
            .arg("-o").arg("PasswordAuthentication=no")
            .arg(format!("{}@{}", user, host))
            .arg(command)
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let code = output.status.code().unwrap_or(-1);

            // Truncate to 10KB (char-boundary safe)
            let stdout_trunc = if stdout.len() > 10240 {
                format!("{}\n[OUTPUT TRUNCATED AT 10KB]", truncate_str(&stdout, 10240))
            } else {
                stdout.to_string()
            };
            let stderr_trunc = if stderr.len() > 10240 {
                format!("{}\n[OUTPUT TRUNCATED AT 10KB]", truncate_str(&stderr, 10240))
            } else {
                stderr.to_string()
            };

            format!(
                "[EXPERIMENTAL] SSH Remote Admin — use at your own risk\n\
                 Host: {}\n\
                 Command: {}\n\
                 Exit code: {}\n\n\
                 --- stdout ---\n{}\n\
                 --- stderr ---\n{}",
                host_display, command, code, stdout_trunc.trim(), stderr_trunc.trim()
            )
        }
        Ok(Err(e)) => format!(
            "[EXPERIMENTAL] SSH Remote Admin — use at your own risk\n\
             Host: {}\n\
             Command: {}\n\
             Error: {}",
            host_display, command, e
        ),
        Err(_) => format!(
            "[EXPERIMENTAL] SSH Remote Admin — use at your own risk\n\
             Host: {}\n\
             Command: {}\n\
             Error: command timed out after 30 seconds",
            host_display, command
        ),
    }
}

/// Open a persistent SSH session via ControlMaster (EXPERIMENTAL).
/// Param: `<user@host[:port]>` or `<host[:port]>` (default user: root, default port: 22)
pub async fn ssh_session_start(param: &str) -> String {
    if param.is_empty() {
        return "Usage: ssh_session_start <user@host> or ssh_session_start <user@host:port> or ssh_session_start <host>".into();
    }

    let (user, host, port) = parse_ssh_target(param.trim());

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
        tokio::process::Command::new("ssh")
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
            .arg(&user_host)
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
                tokio::process::Command::new("ssh")
                    .arg("-o").arg(format!("ControlPath={}", control_path))
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
                            port,
                            control_path,
                        });
                        format!(
                            "[EXPERIMENTAL] SSH session opened to {}. Use ssh_session_exec to run commands, ssh_session_end to close.",
                            user_host_port_display
                        )
                    } else {
                        // Probe didn't return expected output — tear down
                        let _ = tokio::process::Command::new("ssh")
                            .arg("-O").arg("exit")
                            .arg("-o").arg(format!("ControlPath={}", control_path))
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
                    let _ = tokio::process::Command::new("ssh")
                        .arg("-O").arg("exit")
                        .arg("-o").arg(format!("ControlPath={}", control_path))
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
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new("ssh")
            .arg("-o").arg(format!("ControlPath={}", session.control_path))
            .arg("-o").arg("ConnectTimeout=10")
            .arg(&session.user_host)
            .arg(param)
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let code = output.status.code().unwrap_or(-1);

            // Combine stdout + stderr
            let mut combined = if stdout.len() > 10240 {
                format!("{}\n[OUTPUT TRUNCATED AT 10KB]", truncate_str(&stdout, 10240))
            } else {
                stdout.to_string()
            };

            if !stderr.trim().is_empty() {
                let stderr_trunc = if stderr.len() > 10240 {
                    format!("{}\n[OUTPUT TRUNCATED AT 10KB]", truncate_str(&stderr, 10240))
                } else {
                    stderr.to_string()
                };
                combined.push_str(&format!("\n[stderr]:\n{}", stderr_trunc.trim()));
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
        Ok(Err(e)) => format!(
            "[EXPERIMENTAL] SSH session exec on {}\n\
             Command: {}\n\
             Error: {}",
            session.user_host, param, e
        ),
        Err(_) => format!(
            "[EXPERIMENTAL] SSH session exec on {}\n\
             Command: {}\n\
             Error: timed out after 30 seconds",
            session.user_host, param
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
                tokio::process::Command::new("ssh")
                    .arg("-O").arg("exit")
                    .arg("-o").arg(format!("ControlPath={}", session.control_path))
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
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(security_check().await)
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
    description = "Execute a single command on a remote host via SSH (EXPERIMENTAL). Restricted to RFC 1918 private ranges and loopback. target format: host OR user@host OR user@host:port. 30s timeout."
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
    description = "Run a command in the open SSH session. 30s timeout, 10KB output truncation. Each command runs in a fresh process; state between commands is not preserved by the shell."
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
            .into_iter()
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
