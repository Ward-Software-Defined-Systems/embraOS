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

/// Parse a port specification into a list of (port, label) pairs.
fn parse_port_spec(spec: &str) -> Vec<(u16, String)> {
    let spec = spec.trim();
    if spec.is_empty() {
        return COMMON_PORTS.iter().map(|&(p, s)| (p, s.to_string())).collect();
    }

    match spec {
        "web" => vec![
            (80, "HTTP".into()),
            (443, "HTTPS".into()),
            (8080, "HTTP-Alt".into()),
            (8443, "HTTPS-Alt".into()),
        ],
        "db" => vec![
            (3306, "MySQL".into()),
            (5432, "PostgreSQL".into()),
            (6379, "Redis".into()),
            (27017, "MongoDB".into()),
            (5984, "CouchDB".into()),
        ],
        "all" => (1..=1024).map(|p| {
            let label = COMMON_PORTS.iter()
                .find(|&&(cp, _)| cp == p)
                .map(|&(_, s)| s.to_string())
                .unwrap_or_else(|| format!("port-{}", p));
            (p, label)
        }).collect(),
        _ => {
            // Try range: "8000-8100"
            if let Some((start_str, end_str)) = spec.split_once('-') {
                if let (Ok(start), Ok(end)) = (start_str.parse::<u16>(), end_str.parse::<u16>()) {
                    if start <= end && end - start <= 2048 {
                        return (start..=end).map(|p| {
                            let label = COMMON_PORTS.iter()
                                .find(|&&(cp, _)| cp == p)
                                .map(|&(_, s)| s.to_string())
                                .unwrap_or_else(|| format!("port-{}", p));
                            (p, label)
                        }).collect();
                    }
                }
            }
            // Try comma-separated: "80,443,8080"
            spec.split(',')
                .filter_map(|s| {
                    let p: u16 = s.trim().parse().ok()?;
                    let label = COMMON_PORTS.iter()
                        .find(|&&(cp, _)| cp == p)
                        .map(|&(_, s)| s.to_string())
                        .unwrap_or_else(|| format!("port-{}", p));
                    Some((p, label))
                })
                .collect()
        }
    }
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
/// Port specs: empty (default 17 common), `80,443,8080`, `8000-8100`, `web`, `db`, `all`
pub async fn port_scan(param: &str) -> String {
    if param.is_empty() {
        return "Usage: [TOOL:port_scan <host> [ports]]\n\
                Examples:\n\
                  [TOOL:port_scan localhost]          — default 17 common ports\n\
                  [TOOL:port_scan 192.168.1.1 80,443] — specific ports\n\
                  [TOOL:port_scan localhost 8000-8100] — port range\n\
                  [TOOL:port_scan localhost web]       — preset: 80, 443, 8080, 8443\n\
                  [TOOL:port_scan localhost db]        — preset: 3306, 5432, 6379, 27017, 5984\n\
                  [TOOL:port_scan localhost all]       — well-known 1-1024\n\
                Note: restricted to private/loopback addresses only."
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

/// Stub for firewall status — not available in container mode.
pub fn firewall_status() -> String {
    "Firewall status: not implemented in container mode. Full firewall inspection requires host-level access.".into()
}

/// Stub for SSH sessions — not available in container mode.
pub fn ssh_sessions() -> String {
    "SSH sessions: not implemented in container mode. Container uses stdin/stdout for interaction.".into()
}

/// Stub for security audit — not available in container mode.
pub fn security_audit() -> String {
    "Security audit: not implemented in container mode. Full audit capabilities will be available in later phases with host-level access.".into()
}

// ── SSH Remote Admin (EXPERIMENTAL) ──

struct SshSession {
    user_host: String,      // "user@host"
    control_path: String,   // "/tmp/embra-ssh-{uuid}"
}

fn ssh_session_lock() -> &'static tokio::sync::Mutex<Option<SshSession>> {
    static SSH_SESSION: OnceLock<tokio::sync::Mutex<Option<SshSession>>> = OnceLock::new();
    SSH_SESSION.get_or_init(|| tokio::sync::Mutex::new(None))
}

/// Parse `user@host` or `host` (default user: root).
fn parse_ssh_target(target: &str) -> (String, String) {
    if let Some(at_pos) = target.find('@') {
        (target[..at_pos].to_string(), target[at_pos + 1..].to_string())
    } else {
        ("root".to_string(), target.to_string())
    }
}

/// Execute a single command on a remote host via SSH (EXPERIMENTAL).
/// Param format: `<user@host> <command>` or `<host> <command>`
pub async fn ssh_remote_admin(param: &str) -> String {
    if param.is_empty() {
        return "Usage: [TOOL:ssh_remote_admin <host> <command>] or [TOOL:ssh_remote_admin user@host <command>]".into();
    }

    let parts: Vec<&str> = param.splitn(2, ' ').collect();
    if parts.len() < 2 {
        return "Usage: [TOOL:ssh_remote_admin <host> <command>]".into();
    }

    let target = parts[0];
    let command = parts[1];
    let (user, host) = parse_ssh_target(target);

    if !is_private_address(&host) {
        return format!(
            "Denied: '{}' is not a private address. ssh_remote_admin is restricted to RFC 1918 \
             private ranges (10.x.x.x, 172.16-31.x.x, 192.168.x.x) and loopback (127.x.x.x).",
            host
        );
    }

    let result = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new("ssh")
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
                 Host: {}@{}\n\
                 Command: {}\n\
                 Exit code: {}\n\n\
                 --- stdout ---\n{}\n\
                 --- stderr ---\n{}",
                user, host, command, code, stdout_trunc.trim(), stderr_trunc.trim()
            )
        }
        Ok(Err(e)) => format!(
            "[EXPERIMENTAL] SSH Remote Admin — use at your own risk\n\
             Host: {}@{}\n\
             Command: {}\n\
             Error: {}",
            user, host, command, e
        ),
        Err(_) => format!(
            "[EXPERIMENTAL] SSH Remote Admin — use at your own risk\n\
             Host: {}@{}\n\
             Command: {}\n\
             Error: command timed out after 30 seconds",
            user, host, command
        ),
    }
}

/// Open a persistent SSH session via ControlMaster (EXPERIMENTAL).
/// Param: `<user@host>` or `<host>` (default user: root)
pub async fn ssh_session_start(param: &str) -> String {
    if param.is_empty() {
        return "Usage: [TOOL:ssh_session_start <user@host>] or [TOOL:ssh_session_start <host>]".into();
    }

    let (user, host) = parse_ssh_target(param.trim());

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
    let control_path = format!("/tmp/embra-ssh-{}", &uuid::Uuid::new_v4().to_string()[..8]);

    // Start ControlMaster in background (-MNf): authenticates, then backgrounds itself
    let master_result = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new("ssh")
            .arg("-MNf")
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
            // ControlMaster backgrounded successfully — validate with a probe command
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
                            control_path,
                        });
                        format!(
                            "[EXPERIMENTAL] SSH session opened to {}. Use ssh_session_exec to run commands, ssh_session_end to close.",
                            user_host
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
                            user_host, stderr.trim()
                        )
                    }
                }
                Ok(Err(e)) => {
                    let _ = std::fs::remove_file(&control_path);
                    format!("[EXPERIMENTAL] SSH connection to {} failed: {}", user_host, e)
                }
                Err(_) => {
                    let _ = tokio::process::Command::new("ssh")
                        .arg("-O").arg("exit")
                        .arg("-o").arg(format!("ControlPath={}", control_path))
                        .arg(&user_host)
                        .output().await;
                    let _ = std::fs::remove_file(&control_path);
                    format!("[EXPERIMENTAL] SSH connection to {} failed: probe timed out", user_host)
                }
            }
        }
        Ok(Ok(output)) => {
            // ControlMaster exited with non-zero — connection failed
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = std::fs::remove_file(&control_path);
            format!("[EXPERIMENTAL] SSH connection to {} failed: {}", user_host, stderr.trim())
        }
        Ok(Err(e)) => {
            format!("[EXPERIMENTAL] Failed to start SSH session: {}", e)
        }
        Err(_) => {
            let _ = std::fs::remove_file(&control_path);
            format!("[EXPERIMENTAL] SSH connection to {} failed: connection timed out", user_host)
        }
    }
}

/// Run a command in the open SSH session (EXPERIMENTAL).
/// Each command runs as a discrete SSH process through the ControlMaster socket.
/// Param: command to execute
pub async fn ssh_session_exec(param: &str) -> String {
    if param.is_empty() {
        return "Usage: [TOOL:ssh_session_exec <command>]".into();
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
