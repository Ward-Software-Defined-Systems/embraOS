use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::Semaphore;

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
fn is_private_address(host: &str) -> bool {
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
