use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;

/// Strip ANSI escape sequences and OSC sequences from a string.
/// Covers CSI sequences (\x1b[...X), OSC sequences (\x1b]...BEL), and
/// simple two-byte escapes (\x1b followed by one character).
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    // CSI sequence: consume until a letter
                    chars.next(); // skip '['
                    while let Some(&c) = chars.peek() {
                        chars.next();
                        if c.is_ascii_alphabetic() || c == '~' {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC sequence: consume until BEL (\x07) or ST (\x1b\\)
                    chars.next(); // skip ']'
                    while let Some(&c) = chars.peek() {
                        if c == '\x07' {
                            chars.next();
                            break;
                        }
                        if c == '\x1b' {
                            chars.next();
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                        chars.next();
                    }
                }
                Some(_) => {
                    // Simple two-byte escape
                    chars.next();
                }
                None => {}
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Clean SSH session output: strip ANSI escapes, remove prompt echo lines,
/// and trim carriage returns.
fn clean_ssh_output(raw: &str, command: &str) -> String {
    let stripped = strip_ansi(raw);
    let cmd_trimmed = command.trim();
    stripped
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            // Skip empty lines that are just whitespace/CR remnants
            if trimmed.is_empty() {
                return false;
            }
            // Skip lines that are just the echoed command (with or without prompt)
            if trimmed == cmd_trimmed || trimmed.ends_with(&format!("$ {}", cmd_trimmed)) {
                return false;
            }
            // Skip bare prompt lines (e.g. "user@host:~$")
            if trimmed.ends_with('$') && !trimmed.contains(' ') {
                return false;
            }
            // Skip prompt lines like "user@host:~$ echo ..."
            if trimmed.contains("$ echo ___EMBRA_") {
                return false;
            }
            // Skip any leaked probe/drain sentinels from ssh_session_start
            if trimmed.contains("___EMBRA_SSH_PROBE_OK___")
                || trimmed.contains("___EMBRA_DRAIN_DONE___")
            {
                return false;
            }
            true
        })
        .map(|line| line.trim_end_matches('\r'))
        .collect::<Vec<_>>()
        .join("\n")
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
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    host: String,
    user: String,
    cmd_counter: u64,
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

            // Truncate to 10KB
            let stdout_trunc = if stdout.len() > 10240 {
                format!("{}... (truncated)", &stdout[..10240])
            } else {
                stdout.to_string()
            };
            let stderr_trunc = if stderr.len() > 10240 {
                format!("{}... (truncated)", &stderr[..10240])
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

/// Open a persistent SSH session (EXPERIMENTAL).
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

    let mut child = match tokio::process::Command::new("ssh")
        .arg("-tt")
        .arg("-o").arg("StrictHostKeyChecking=accept-new")
        .arg("-o").arg("ConnectTimeout=10")
        .arg("-o").arg("BatchMode=yes")
        .arg("-o").arg("PasswordAuthentication=no")
        .arg(format!("{}@{}", user, host))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return format!("[EXPERIMENTAL] Failed to start SSH session: {}", e),
    };

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut stdout_reader = BufReader::new(stdout);

    // Validate the connection is actually live by sending a probe command
    // and waiting for its output before declaring success.
    let probe_sentinel = "___EMBRA_SSH_PROBE_OK___";
    let probe_cmd = format!("echo {}\n", probe_sentinel);
    if let Err(e) = stdin.write_all(probe_cmd.as_bytes()).await {
        let _ = child.kill().await;
        return format!("[EXPERIMENTAL] Failed to write to SSH session: {}", e);
    }
    if let Err(e) = stdin.flush().await {
        let _ = child.kill().await;
        return format!("[EXPERIMENTAL] Failed to flush SSH stdin: {}", e);
    }

    // Wait for the sentinel to appear in stdout (with ConnectTimeout + buffer)
    let probe_result = tokio::time::timeout(Duration::from_secs(15), async {
        let mut line = String::new();
        loop {
            line.clear();
            match stdout_reader.read_line(&mut line).await {
                Ok(0) => return Err("Connection closed before probe completed".to_string()),
                Ok(_) => {
                    if line.contains(probe_sentinel) {
                        return Ok(());
                    }
                }
                Err(e) => return Err(format!("Read error: {}", e)),
            }
        }
    })
    .await;

    match probe_result {
        Ok(Ok(())) => {
            // Drain any remaining MOTD/banner lines from the buffer after the
            // probe sentinel. We do this by sending a *second* sentinel and
            // consuming everything up to it, which guarantees the buffer is
            // clean for the first ssh_session_exec call.
            let drain_sentinel = "___EMBRA_DRAIN_DONE___";
            let drain_cmd = format!("echo {}\n", drain_sentinel);
            let _ = stdin.write_all(drain_cmd.as_bytes()).await;
            let _ = stdin.flush().await;
            let drain_result = tokio::time::timeout(Duration::from_secs(5), async {
                let mut line = String::new();
                loop {
                    line.clear();
                    match stdout_reader.read_line(&mut line).await {
                        Ok(0) => break,
                        Ok(_) => {
                            if strip_ansi(&line).contains(drain_sentinel) {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
            .await;

            if drain_result.is_err() {
                let _ = child.kill().await;
                return format!("[EXPERIMENTAL] SSH connection to {}@{} failed: buffer drain timed out", user, host);
            }

            // Connection verified and buffer clean
            *lock = Some(SshSession {
                child,
                stdin,
                stdout: stdout_reader,
                host: host.clone(),
                user: user.clone(),
                cmd_counter: 0,
            });

            format!(
                "[EXPERIMENTAL] SSH session opened to {}@{}. Use ssh_session_exec to run commands, ssh_session_end to close.",
                user, host
            )
        }
        Ok(Err(e)) => {
            // Probe failed — read stderr for the real error message
            let mut err_buf = vec![0u8; 4096];
            let stderr_msg = match tokio::time::timeout(Duration::from_secs(1), stderr.read(&mut err_buf)).await {
                Ok(Ok(n)) if n > 0 => String::from_utf8_lossy(&err_buf[..n]).trim().to_string(),
                _ => e.clone(),
            };
            let _ = child.kill().await;
            format!("[EXPERIMENTAL] SSH connection to {}@{} failed: {}", user, host, stderr_msg)
        }
        Err(_) => {
            // Timeout — read stderr for clues
            let mut err_buf = vec![0u8; 4096];
            let stderr_msg = match tokio::time::timeout(Duration::from_secs(1), stderr.read(&mut err_buf)).await {
                Ok(Ok(n)) if n > 0 => String::from_utf8_lossy(&err_buf[..n]).trim().to_string(),
                _ => "connection timed out".to_string(),
            };
            let _ = child.kill().await;
            format!("[EXPERIMENTAL] SSH connection to {}@{} failed: {}", user, host, stderr_msg)
        }
    }
}

/// Run a command in the open SSH session (EXPERIMENTAL).
/// Param: command to execute
pub async fn ssh_session_exec(param: &str) -> String {
    if param.is_empty() {
        return "Usage: [TOOL:ssh_session_exec <command>]".into();
    }

    let mut lock = ssh_session_lock().lock().await;
    let session = match lock.as_mut() {
        Some(s) => s,
        None => return "No SSH session open. Use ssh_session_start first.".into(),
    };

    // Use a unique sentinel per command so we never match a stale sentinel
    // from a previous call or the PTY echoing back the command line.
    session.cmd_counter += 1;
    let sentinel = format!("___EMBRA_DONE_{}___", session.cmd_counter);
    // Chain the sentinel echo on the same line so the shell executes it
    // after the command completes. The sentinel output line will be:
    //   ___EMBRA_DONE_N___
    // The PTY will also echo back the command line itself, which contains
    // the sentinel string — we distinguish them by checking that the
    // stripped line is *exactly* the sentinel (not part of a longer line).
    let full_cmd = format!("{} ; echo {}\n", param, sentinel);

    if let Err(e) = session.stdin.write_all(full_cmd.as_bytes()).await {
        return format!("[EXPERIMENTAL] Failed to write to SSH session: {}", e);
    }
    if let Err(e) = session.stdin.flush().await {
        return format!("[EXPERIMENTAL] Failed to flush SSH stdin: {}", e);
    }

    // Read stdout until the sentinel output line appears. The PTY echoes
    // back the full command line (which contains the sentinel string), so we
    // must distinguish the echo from the actual sentinel output:
    //   - Echo line: "command ; echo ___EMBRA_DONE_N___"  (contains more than just sentinel)
    //   - Sentinel output: "___EMBRA_DONE_N___"            (stripped line IS the sentinel)
    let mut raw_output = String::new();
    let read_result = tokio::time::timeout(Duration::from_secs(30), async {
        let mut line = String::new();
        loop {
            line.clear();
            match session.stdout.read_line(&mut line).await {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let stripped = strip_ansi(&line);
                    let trimmed = stripped.trim();
                    // The actual sentinel output line is *exactly* the sentinel
                    // (possibly with whitespace). The echoed command line will
                    // have extra text (the command itself, `echo`, etc).
                    if trimmed == sentinel {
                        break;
                    }
                    // Skip the echoed command line (contains the sentinel as
                    // part of a longer string like "cmd ; echo ___EMBRA_...")
                    if trimmed.contains(&sentinel) {
                        continue;
                    }
                    raw_output.push_str(&line);
                    if raw_output.len() > 10240 {
                        raw_output.truncate(10240);
                        raw_output.push_str("... (truncated)");
                        break;
                    }
                }
                Err(e) => {
                    raw_output.push_str(&format!("\n[read error: {}]", e));
                    break;
                }
            }
        }
    })
    .await;

    // Clean ANSI escapes, prompt echoes, and carriage returns
    let cleaned = clean_ssh_output(&raw_output, param);

    if read_result.is_err() {
        return format!(
            "[EXPERIMENTAL] SSH session exec on {}@{}\n\
             Command: {}\n\
             Error: timed out after 30 seconds\n\n\
             Partial output:\n{}",
            session.user, session.host, param, cleaned.trim()
        );
    }

    format!(
        "[EXPERIMENTAL] SSH session exec on {}@{}\n\
         Command: {}\n\n\
         --- output ---\n{}",
        session.user, session.host, param, cleaned.trim()
    )
}

/// Close the open SSH session (EXPERIMENTAL).
pub async fn ssh_session_end() -> String {
    let mut lock = ssh_session_lock().lock().await;
    match lock.take() {
        Some(mut session) => {
            let _ = session.child.kill().await;
            let _ = session.child.wait().await;
            format!(
                "[EXPERIMENTAL] SSH session to {}@{} closed.",
                session.user, session.host
            )
        }
        None => "No SSH session is open.".into(),
    }
}
