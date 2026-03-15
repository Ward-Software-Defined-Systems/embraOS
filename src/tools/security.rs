use std::time::Duration;
use tokio::net::TcpStream;

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

/// TCP connect scan to common ports on a target.
/// Restricted to private/loopback addresses only (RFC 1918 + 127.0.0.0/8).
pub async fn port_scan(target: &str) -> String {
    if target.is_empty() {
        return "Usage: [TOOL:port_scan <host>]\nExample: [TOOL:port_scan 192.168.1.1]\nNote: restricted to private/loopback addresses only.".into();
    }

    if !is_private_address(target) {
        return format!(
            "Denied: '{}' is not a private address. port_scan is restricted to RFC 1918 \
             private ranges (10.x.x.x, 172.16-31.x.x, 192.168.x.x) and loopback (127.x.x.x).",
            target
        );
    }

    let common_ports: &[(u16, &str)] = &[
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

    let mut output = format!("=== Port Scan: {} ===\n", target);
    let mut open_ports = Vec::new();

    for &(port, service) in common_ports {
        let addr = format!("{}:{}", target, port);
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            TcpStream::connect(&addr),
        )
        .await;

        match result {
            Ok(Ok(_)) => {
                open_ports.push(format!("  {} ({}) — OPEN", port, service));
            }
            _ => {} // closed or timeout
        }
    }

    if open_ports.is_empty() {
        output.push_str("No open ports found on common ports.\n");
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
