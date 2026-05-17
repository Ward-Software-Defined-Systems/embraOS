//! Capability broker — host-side, policy-guarded primitives the wasm guest
//! may invoke *only* via Guardian-mediated imports. The guest has no
//! ambient authority; every capability is added here, "at the guard
//! level", and gated by per-tool grants + an egress policy.
//!
//! v1 capability: [`guarded_http_get`]. The raw transport is behind
//! [`HttpTransport`] so embra-guardian stays decoupled and the guard is
//! unit-tested with a mock (no live network in CI). The guard runs
//! *before* the transport: scheme, SSRF/RFC1918 (literal + DNS-resolved),
//! optional domain allowlist, then size + content-type caps after.

use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

/// Minimal HTTP response the guard inspects + forwards to the guest.
pub struct HttpResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// Raw transport. Implementations perform the request and nothing else —
/// **all policy is enforced by [`guarded_http_get`]**, never here.
pub trait HttpTransport: Send + Sync {
    fn get(&self, url: &str, timeout: Duration, max_bytes: usize)
        -> Result<HttpResponse, String>;
}

/// Egress policy applied to every `http_get`. Tunable by the brain later;
/// the defaults are the safe v1 baseline.
#[derive(Clone)]
pub struct EgressPolicy {
    /// `None` = any host allowed (after scheme + SSRF). `Some` = the host
    /// must equal or be a subdomain of an entry.
    pub allow_domains: Option<Vec<String>>,
    pub max_bytes: usize,
    pub timeout: Duration,
}

impl Default for EgressPolicy {
    fn default() -> Self {
        Self { allow_domains: None, max_bytes: 256 * 1024, timeout: Duration::from_secs(10) }
    }
}

/// Per-call capability grants, carried in the wasmtime `StoreData`. A tool
/// that did not declare/was not granted a capability gets `None` and the
/// import returns a structured "not granted" error to the guest.
#[derive(Clone, Default)]
pub struct Capabilities {
    pub http: Option<Arc<dyn HttpTransport>>,
    pub http_policy: EgressPolicy,
}

impl Capabilities {
    /// Pure-compute tool: no capabilities.
    pub fn none() -> Self {
        Self::default()
    }
    /// Grant `http_get` backed by `transport` under `policy`.
    pub fn with_http(transport: Arc<dyn HttpTransport>, policy: EgressPolicy) -> Self {
        Self { http: Some(transport), http_policy: policy }
    }
}

fn err_json(msg: &str) -> String {
    serde_json::json!({ "ok": false, "error": msg }).to_string()
}

/// True if `ip` is a private / loopback / link-local / ULA / unspecified /
/// CGNAT / IPv4-mapped-private address — i.e. an SSRF target to refuse.
pub fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => is_blocked_v4(v),
        IpAddr::V6(v) => {
            if v.is_loopback() || v.is_unspecified() {
                return true;
            }
            // IPv4-mapped (::ffff:a.b.c.d) → judge the embedded v4.
            if let Some(v4) = v.to_ipv4_mapped() {
                return is_blocked_v4(&v4);
            }
            let s0 = v.segments()[0];
            (s0 & 0xfe00) == 0xfc00      // ULA fc00::/7
                || (s0 & 0xffc0) == 0xfe80 // link-local fe80::/10
        }
    }
}

fn is_blocked_v4(v: &Ipv4Addr) -> bool {
    let o = v.octets();
    v.is_private()
        || v.is_loopback()
        || v.is_link_local()
        || v.is_unspecified()
        || v.is_broadcast()
        || v.is_documentation()
        || o[0] == 0
        || (o[0] == 100 && (64..=127).contains(&o[1])) // CGNAT 100.64.0.0/10
}

/// The guard. Returns the JSON string handed back to the guest (always
/// well-formed JSON — success or a structured error; never panics).
pub fn guarded_http_get(caps: &Capabilities, url: &str) -> String {
    let Some(http) = caps.http.as_ref() else {
        return err_json("capability 'http_get' not granted to this tool");
    };

    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(e) => return err_json(&format!("invalid url: {e}")),
    };
    if parsed.scheme() != "https" {
        return err_json("only https:// destinations are allowed");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return err_json("url userinfo (user:pass@) is not allowed");
    }
    let Some(host) = parsed.host_str().map(str::to_string) else {
        return err_json("url has no host");
    };

    // Domain allowlist (if configured).
    if let Some(allow) = &caps.http_policy.allow_domains {
        let ok = allow
            .iter()
            .any(|d| host == *d || host.ends_with(&format!(".{d}")));
        if !ok {
            return err_json("destination domain is not in the allowlist");
        }
    }

    // SSRF: refuse a literal private IP, and refuse if DNS resolves to one.
    if let Ok(ip) = host.parse::<IpAddr>()
        && is_blocked_ip(&ip)
    {
        return err_json("destination is a private/loopback IP (SSRF blocked)");
    }
    let port = parsed.port_or_known_default().unwrap_or(443);
    match (host.as_str(), port).to_socket_addrs() {
        Ok(addrs) => {
            let mut resolved = false;
            for a in addrs {
                resolved = true;
                if is_blocked_ip(&a.ip()) {
                    return err_json(
                        "destination resolves to a private/loopback address (SSRF blocked)",
                    );
                }
            }
            if !resolved {
                return err_json("destination did not resolve");
            }
        }
        Err(e) => return err_json(&format!("dns resolution failed: {e}")),
    }

    match http.get(url, caps.http_policy.timeout, caps.http_policy.max_bytes) {
        Ok(resp) => {
            let ct = resp.content_type.to_ascii_lowercase();
            let ct_ok = ct.is_empty()
                || ct.starts_with("text/")
                || ct.starts_with("application/json");
            if !ct_ok {
                return err_json(&format!(
                    "response content-type '{}' is not allowed",
                    resp.content_type
                ));
            }
            let mut body = resp.body;
            if body.len() > caps.http_policy.max_bytes {
                body.truncate(caps.http_policy.max_bytes);
            }
            serde_json::json!({
                "ok": true,
                "status": resp.status,
                "url": url,
                "content_type": resp.content_type,
                "body": String::from_utf8_lossy(&body),
            })
            .to_string()
        }
        Err(e) => err_json(&e),
    }
}

/// Default transport: `reqwest` blocking + rustls (the workspace already
/// links this stack static-musl for the WardSONDB client). Redirects are
/// **not** followed — an auto-followed 3xx is an SSRF bypass; a 3xx is
/// surfaced to the guest verbatim instead.
pub struct ReqwestTransport {
    client: reqwest::blocking::Client,
}

impl ReqwestTransport {
    pub fn new() -> Result<Self, String> {
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("embra-guardian/0.5")
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { client })
    }
}

impl HttpTransport for ReqwestTransport {
    fn get(&self, url: &str, timeout: Duration, max_bytes: usize)
        -> Result<HttpResponse, String> {
        let resp = self
            .client
            .get(url)
            .timeout(timeout)
            .send()
            .map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let full = resp.bytes().map_err(|e| e.to_string())?;
        let mut body = full.to_vec();
        if body.len() > max_bytes {
            body.truncate(max_bytes);
        }
        Ok(HttpResponse { status, content_type, body })
    }
}

#[cfg(test)]
pub(crate) struct MockTransport {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

#[cfg(test)]
impl HttpTransport for MockTransport {
    fn get(&self, _u: &str, _t: Duration, _m: usize) -> Result<HttpResponse, String> {
        Ok(HttpResponse {
            status: self.status,
            content_type: self.content_type.clone(),
            body: self.body.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_caps() -> Capabilities {
        Capabilities::with_http(
            Arc::new(MockTransport {
                status: 200,
                content_type: "application/json".into(),
                body: b"{\"hi\":1}".to_vec(),
            }),
            EgressPolicy::default(),
        )
    }

    fn parse_ok(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("guard must always emit valid JSON")
    }

    #[test]
    fn rejects_when_capability_not_granted() {
        let v = parse_ok(&guarded_http_get(&Capabilities::none(), "https://1.1.1.1/"));
        assert_eq!(v["ok"], false);
    }

    #[test]
    fn rejects_non_https() {
        let v = parse_ok(&guarded_http_get(&mock_caps(), "http://1.1.1.1/"));
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("https"));
    }

    #[test]
    fn rejects_userinfo() {
        let v = parse_ok(&guarded_http_get(&mock_caps(), "https://u:p@1.1.1.1/"));
        assert_eq!(v["ok"], false);
    }

    #[test]
    fn blocks_rfc1918_and_loopback_literals() {
        for u in [
            "https://127.0.0.1/",
            "https://10.0.0.5/",
            "https://192.168.1.1/",
            "https://172.16.9.9/",
            "https://169.254.1.1/",
            "https://[::1]/",
            "https://100.64.0.1/",
        ] {
            let v = parse_ok(&guarded_http_get(&mock_caps(), u));
            assert_eq!(v["ok"], false, "should block {u}");
        }
    }

    #[test]
    fn allowlist_denies_outside_domain() {
        let caps = Capabilities::with_http(
            Arc::new(MockTransport { status: 200, content_type: "text/plain".into(), body: vec![] }),
            EgressPolicy { allow_domains: Some(vec!["example.com".into()]), ..Default::default() },
        );
        // 1.1.1.1 passes SSRF (public) but is not in the allowlist.
        let v = parse_ok(&guarded_http_get(&caps, "https://1.1.1.1/"));
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("allowlist"));
    }

    #[test]
    fn public_ip_literal_passes_guard_and_returns_body() {
        // IP literal => no DNS; 1.1.1.1 is public => guard passes => mock body.
        let v = parse_ok(&guarded_http_get(&mock_caps(), "https://1.1.1.1/"));
        assert_eq!(v["ok"], true);
        assert_eq!(v["status"], 200);
        assert_eq!(v["body"], "{\"hi\":1}");
    }

    #[test]
    fn rejects_disallowed_content_type() {
        let caps = Capabilities::with_http(
            Arc::new(MockTransport {
                status: 200,
                content_type: "application/octet-stream".into(),
                body: vec![1, 2, 3],
            }),
            EgressPolicy::default(),
        );
        let v = parse_ok(&guarded_http_get(&caps, "https://1.1.1.1/"));
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("content-type"));
    }

    #[test]
    fn ipv4_mapped_v6_private_is_blocked() {
        let mapped: IpAddr = "::ffff:10.0.0.1".parse().unwrap();
        assert!(is_blocked_ip(&mapped));
        let pub_v6: IpAddr = "2606:4700:4700::1111".parse().unwrap();
        assert!(!is_blocked_ip(&pub_v6));
    }
}
