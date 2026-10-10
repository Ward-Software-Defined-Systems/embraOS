//! Capability broker — host-side, policy-guarded primitives the wasm guest
//! may invoke *only* via Guardian-mediated imports. The guest has no
//! ambient authority; every capability is added here, "at the guard
//! level", and gated by per-tool grants + an egress policy.
//!
//! Two capabilities: [`guarded_http_get`] and [`guarded_web_search`]. The
//! raw transport is behind [`HttpTransport`] and the search backend behind
//! [`SearchProvider`], so embra-guardian stays decoupled and each guard is
//! unit-tested with a mock (no live network in CI). The fetch guard runs
//! *before* the transport: scheme, SSRF/RFC1918 (literal + DNS-resolved),
//! optional domain allowlist, then size + content-type caps after. The
//! search guard clamps the request, filters and reduces the results.

use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Minimal HTTP response the guard inspects + forwards to the guest.
pub struct HttpResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    /// The `Location` header of a redirect, as sent; the guard resolves
    /// and checks it before following. `None` on every other response.
    pub location: Option<String>,
    /// Every response header, as sent (the request guard lowercases,
    /// filters and caps them before the guest sees any).
    pub headers: Vec<(String, String)>,
}

/// The methods a guest may ask for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Head => "HEAD",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }

    fn parse(s: &str) -> Option<Method> {
        match s.trim().to_ascii_uppercase().as_str() {
            "GET" => Some(Method::Get),
            "HEAD" => Some(Method::Head),
            "POST" => Some(Method::Post),
            "PUT" => Some(Method::Put),
            "PATCH" => Some(Method::Patch),
            "DELETE" => Some(Method::Delete),
            _ => None,
        }
    }

    /// Only an idempotent read follows a redirect; a POST to a moved
    /// resource comes back as the 3xx, for the caller to decide.
    fn follows_redirects(self) -> bool {
        matches!(self, Method::Get | Method::Head)
    }
}

/// One request as the guard hands it to the transport: the policy has run
/// on the URL and the operator's secrets are among the headers.
#[derive(Clone, Debug)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    /// Lowercase names.
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub timeout: Duration,
    pub max_bytes: usize,
}

impl HttpRequest {
    /// A plain GET: no headers, no body.
    pub fn get(url: &str, timeout: Duration, max_bytes: usize) -> Self {
        Self {
            method: Method::Get,
            url: url.to_string(),
            headers: Vec::new(),
            body: None,
            timeout,
            max_bytes,
        }
    }
}

/// Raw transport. Implementations perform the request and nothing else —
/// **all policy is enforced by the guards**, never here. `get` is what
/// the fetch guard calls; `request` what the request guard calls, and a
/// transport that answers plain GETs only (the test stubs) keeps the
/// default.
pub trait HttpTransport: Send + Sync {
    fn get(&self, url: &str, timeout: Duration, max_bytes: usize)
        -> Result<HttpResponse, String>;

    fn request(&self, req: &HttpRequest) -> Result<HttpResponse, String> {
        if req.method == Method::Get && req.headers.is_empty() && req.body.is_none() {
            return self.get(&req.url, req.timeout, req.max_bytes);
        }
        Err("this transport answers a plain GET only".to_string())
    }
}

/// A credential the host adds to a request for one host, as the
/// operator stored it (`/guardian secret <host> <header> <value>`). The
/// guest never sees it; a redirect to another host never carries it.
#[derive(Clone, Debug)]
pub struct SecretHeader {
    /// Lowercase host, with the port when the operator gave one.
    pub host: String,
    /// Lowercase header name.
    pub name: String,
    pub value: String,
}

/// Egress policy applied to every `http_get`. Tunable by the brain later;
/// the defaults are the safe v1 baseline.
#[derive(Clone)]
pub struct EgressPolicy {
    /// `None` = any host allowed (after scheme + SSRF). `Some` = the host
    /// must equal or be a subdomain of an entry.
    pub allow_domains: Option<Vec<String>>,
    /// Hosts a request may reach although they resolve to a private
    /// address: a name (itself and its subdomains) or an IP literal. The
    /// operator's egress allowlist. Loopback, link-local, multicast and
    /// reserved addresses are refused whatever stands here.
    pub private_hosts: Vec<String>,
    pub max_bytes: usize,
    /// The budget of one guarded call, every redirect hop included; each
    /// hop gets what is left of it.
    pub timeout: Duration,
}

impl Default for EgressPolicy {
    fn default() -> Self {
        Self {
            allow_domains: None,
            private_hosts: Vec::new(),
            max_bytes: 256 * 1024,
            timeout: Duration::from_secs(10),
        }
    }
}

/// Per-call capability grants, carried in the wasmtime `StoreData`. A tool
/// that did not declare/was not granted a capability gets `None` and the
/// import returns a structured "not granted" error to the guest.
#[derive(Clone, Default)]
pub struct Capabilities {
    pub http: Option<Arc<dyn HttpTransport>>,
    pub http_policy: EgressPolicy,
    /// The operator's per-host credentials, injected by the request
    /// guard on a hop whose host matches. Empty for a tool without
    /// `http_request`.
    pub secrets: Vec<SecretHeader>,
    /// `web_search` provider (Brave-backed in v1). `None` ⇒ the host
    /// method returns a structured "not configured" envelope. The
    /// provider holds the API key host-side; it never reaches the guest.
    pub search: Option<Arc<dyn SearchProvider>>,
}

impl Capabilities {
    /// Pure-compute tool: no capabilities.
    pub fn none() -> Self {
        Self::default()
    }
    /// Grant `http_get` backed by `transport` under `policy`.
    pub fn with_http(transport: Arc<dyn HttpTransport>, policy: EgressPolicy) -> Self {
        Self { http: Some(transport), http_policy: policy, secrets: Vec::new(), search: None }
    }
    /// Grant `web_search` backed by `provider`.
    pub fn with_search(provider: Arc<dyn SearchProvider>) -> Self {
        Self { search: Some(provider), ..Self::default() }
    }
}

fn err_json(msg: &str) -> String {
    serde_json::json!({ "ok": false, "error": msg }).to_string()
}

/// What an address is, for the egress guard. `Public` is the only class
/// a request reaches by default; `Private` when the operator put the host
/// on the egress allowlist (`EgressPolicy::private_hosts`, `/guardian
/// egress allow <host>`); the rest never, whatever the list says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressClass {
    Public,
    /// RFC 1918, CGNAT 100.64/10, IPv6 ULA fc00::/7 and site-local fec0::/10.
    Private,
    /// 127/8, ::1, and the unspecified addresses.
    Loopback,
    /// 169.254/16 (where cloud metadata services live) and fe80::/10.
    LinkLocal,
    /// 224/4 and ff00::/8.
    Multicast,
    /// 0/8, 240/4 with the broadcast address, the documentation ranges
    /// (192.0.2/24, 198.51.100/24, 203.0.113/24), the benchmark range
    /// 198.18/15, and the IPv6 transition forms (64:ff9b::/96, 2002::/16,
    /// the deprecated `::a.b.c.d`).
    Reserved,
}

/// The class of an address. An IPv4-mapped IPv6 address is judged as the
/// IPv4 it carries.
pub fn classify(ip: &IpAddr) -> AddressClass {
    match ip {
        IpAddr::V4(v) => classify_v4(v),
        IpAddr::V6(v) => {
            if v.is_loopback() || v.is_unspecified() {
                return AddressClass::Loopback;
            }
            if let Some(v4) = v.to_ipv4_mapped() {
                return classify_v4(&v4);
            }
            if v.is_multicast() {
                return AddressClass::Multicast;
            }
            let s = v.segments();
            if s[..6] == [0, 0, 0, 0, 0, 0] {
                // IPv4-compatible `::a.b.c.d`, deprecated.
                return AddressClass::Reserved;
            }
            if (s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0]) || s[0] == 0x2002 {
                // NAT64 well-known prefix, 6to4.
                return AddressClass::Reserved;
            }
            if (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfec0 {
                return AddressClass::Private;
            }
            if (s[0] & 0xffc0) == 0xfe80 {
                return AddressClass::LinkLocal;
            }
            AddressClass::Public
        }
    }
}

fn classify_v4(v: &Ipv4Addr) -> AddressClass {
    let o = v.octets();
    if v.is_loopback() || v.is_unspecified() {
        AddressClass::Loopback
    } else if v.is_link_local() {
        AddressClass::LinkLocal
    } else if v.is_multicast() {
        AddressClass::Multicast
    } else if v.is_private() || (o[0] == 100 && (64..=127).contains(&o[1])) {
        AddressClass::Private
    } else if v.is_broadcast()
        || v.is_documentation()
        || o[0] == 0
        || o[0] >= 240
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
    {
        AddressClass::Reserved
    } else {
        AddressClass::Public
    }
}

/// Anything a request may not reach by default: every class but `Public`.
pub fn is_blocked_ip(ip: &IpAddr) -> bool {
    classify(ip) != AddressClass::Public
}

/// Whether `host` (lowercase, no port) is `entry` or a subdomain of it.
/// An IP literal, as the host or as the entry, matches only itself: an
/// address has no subdomains, and a partial entry like `0.0.5` must not
/// stand for every address that ends in it.
fn host_matches(host: &str, entry: &str) -> bool {
    let entry = entry.trim().to_ascii_lowercase();
    if entry.is_empty() {
        return false;
    }
    if host.parse::<IpAddr>().is_ok() || entry.parse::<IpAddr>().is_ok() {
        return host == entry;
    }
    host == entry || host.ends_with(&format!(".{entry}"))
}

/// The guard's verdict on an address class: public passes, private passes
/// for a host on the allowlist, and the rest are refused with the class
/// named, so the operator knows which command (if any) would open it.
fn refuse_class(class: AddressClass, private_ok: bool, verb: &str) -> Result<(), String> {
    match class {
        AddressClass::Public => Ok(()),
        AddressClass::Private if private_ok => Ok(()),
        AddressClass::Private => Err(format!(
            "destination {verb} a private address (SSRF blocked; not on the egress allowlist \
             — /guardian egress allow <host>)"
        )),
        AddressClass::Loopback => Err(format!(
            "destination {verb} a loopback address (SSRF blocked; refused always)"
        )),
        AddressClass::LinkLocal => Err(format!(
            "destination {verb} a link-local address (SSRF blocked; refused always)"
        )),
        AddressClass::Multicast => Err(format!(
            "destination {verb} a multicast address (SSRF blocked; refused always)"
        )),
        AddressClass::Reserved => Err(format!(
            "destination {verb} a reserved address (SSRF blocked; refused always)"
        )),
    }
}

/// Redirects the guard follows on its own, each hop checked like the
/// first. Brave indexes stale URLs routinely (www/non-www, a dropped
/// `.html`, a moved path); a 301 used to come back as an empty page.
pub const MAX_REDIRECTS: usize = 3;

/// The policy on one URL: https only, no userinfo, a host, the allowlist,
/// and no private or loopback address, as a literal or as DNS resolves it.
fn check_url(caps: &Capabilities, url: &str) -> Result<url::Url, String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("invalid url: {e}"))?;
    if parsed.scheme() != "https" {
        return Err("only https:// destinations are allowed".to_string());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("url userinfo (user:pass@) is not allowed".to_string());
    }
    let Some(host) = parsed.host_str().map(str::to_string) else {
        return Err("url has no host".to_string());
    };
    // Domain allowlist (if configured).
    if let Some(allow) = &caps.http_policy.allow_domains {
        let ok = allow
            .iter()
            .any(|d| host == *d || host.ends_with(&format!(".{d}")));
        if !ok {
            return Err("destination domain is not in the allowlist".to_string());
        }
    }
    // SSRF: the class of a literal address, and of every address DNS
    // resolves to. Public passes; private passes when the host is on the
    // operator's egress allowlist; the rest never.
    let host_lc = host.to_ascii_lowercase();
    let private_ok = caps
        .http_policy
        .private_hosts
        .iter()
        .any(|entry| host_matches(&host_lc, entry));
    if let Ok(ip) = host.parse::<IpAddr>() {
        refuse_class(classify(&ip), private_ok, "is")?;
    }
    let port = parsed.port_or_known_default().unwrap_or(443);
    match (host.as_str(), port).to_socket_addrs() {
        Ok(addrs) => {
            let mut resolved = false;
            for a in addrs {
                resolved = true;
                refuse_class(classify(&a.ip()), private_ok, "resolves to")?;
            }
            if !resolved {
                return Err("destination did not resolve".to_string());
            }
        }
        Err(e) => return Err(format!("dns resolution failed: {e}")),
    }
    Ok(parsed)
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// The guard. Returns the JSON string handed back to the guest (always
/// well-formed JSON — success or a structured error; never panics). A
/// redirect is followed up to [`MAX_REDIRECTS`] times, every hop through
/// `check_url` again, so a chain cannot land where a first request could
/// not; `url` in the envelope is the URL that answered, `redirects` the
/// hops taken, and a redirect past the limit is returned as it came, with
/// its target under `redirect`.
pub fn guarded_http_get(caps: &Capabilities, url: &str) -> String {
    let Some(http) = caps.http.as_ref() else {
        return err_json("capability 'http_get' not granted to this tool");
    };
    let mut current = url.to_string();
    let mut hops = 0usize;
    let budget = caps.http_policy.timeout;
    let started = Instant::now();
    loop {
        let parsed = match check_url(caps, &current) {
            Ok(u) => u,
            Err(e) => return err_json(&e),
        };
        let remaining = match budget.checked_sub(started.elapsed()) {
            Some(r) if !r.is_zero() => r,
            _ => return err_json(&format!("fetch budget of {budget:?} spent")),
        };
        let resp = match http.get(&current, remaining, caps.http_policy.max_bytes) {
            Ok(r) => r,
            Err(e) => return err_json(&e),
        };
        if is_redirect(resp.status)
            && let Some(loc) = resp.location.as_deref()
        {
            if hops < MAX_REDIRECTS {
                current = match parsed.join(loc) {
                    Ok(next) => next.to_string(),
                    Err(e) => return err_json(&format!("invalid redirect target: {e}")),
                };
                hops += 1;
                continue;
            }
            return serde_json::json!({
                "ok": true,
                "status": resp.status,
                "url": current,
                "redirects": hops,
                "redirect": loc,
                "content_type": resp.content_type,
                "body": "",
            })
            .to_string();
        }
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
        let mut env = serde_json::json!({
            "ok": true,
            "status": resp.status,
            "url": current,
            "content_type": resp.content_type,
            "body": String::from_utf8_lossy(&body),
        });
        if hops > 0 {
            env["redirects"] = serde_json::json!(hops);
        }
        return env.to_string();
    }
}

// ── http_request capability ──

/// A request body a guest may send.
pub const HTTP_REQUEST_BODY_MAX: usize = 256 * 1024;
/// The response a guest may ask for (`max_bytes`); the default is the
/// policy's. Under `host::MAX_OUTPUT` with room for the envelope.
pub const HTTP_RESPONSE_MAX_BYTES: usize = 1024 * 1024;
/// Headers each way: at most this many, and this many bytes in all.
pub const HTTP_HEADERS_MAX: usize = 32;
pub const HTTP_HEADERS_BYTES_MAX: usize = 8 * 1024;

/// Headers a guest may not set: a credential comes from the operator's
/// store only, never from a tool's input.
const CREDENTIAL_HEADERS: &[&str] = &["authorization", "proxy-authorization", "cookie"];
/// Headers the transport owns; a guest's value is dropped without a word.
const TRANSPORT_HEADERS: &[&str] = &[
    "host", "content-length", "transfer-encoding", "connection", "expect", "upgrade", "te",
    "keep-alive",
];
/// Response headers that never reach the guest.
const HIDDEN_RESPONSE_HEADERS: &[&str] = &["set-cookie", "set-cookie2"];

fn is_header_token(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
        })
}

/// The guest's JSON request, vetted: method, URL with the query appended,
/// headers (lowercase, no credential or transport header), body, caps.
fn parse_http_request(input: &str, policy: &EgressPolicy) -> Result<HttpRequest, String> {
    let v: serde_json::Value = serde_json::from_str(input)
        .map_err(|e| format!("request must be a JSON object: {e}"))?;
    let obj = v
        .as_object()
        .ok_or_else(|| "request must be a JSON object".to_string())?;
    let url = obj
        .get("url")
        .and_then(|u| u.as_str())
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| "url is required".to_string())?;
    let method = match obj.get("method") {
        None | Some(serde_json::Value::Null) => Method::Get,
        Some(m) => m
            .as_str()
            .and_then(Method::parse)
            .ok_or_else(|| "method must be one of GET, HEAD, POST, PUT, PATCH, DELETE".to_string())?,
    };
    let mut parsed = url::Url::parse(url).map_err(|e| format!("invalid url: {e}"))?;
    if let Some(query) = obj.get("query") {
        let pairs = query
            .as_object()
            .ok_or_else(|| "query must be an object of scalars".to_string())?;
        let mut q = parsed.query_pairs_mut();
        for (k, val) in pairs {
            let text = match val {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::Bool(b) => b.to_string(),
                _ => return Err(format!("query '{k}' must be a string, a number or a boolean")),
            };
            q.append_pair(k, &text);
        }
        drop(q);
    }
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(accept) = obj.get("accept").and_then(|a| a.as_str()) {
        headers.push(("accept".to_string(), accept.to_string()));
    }
    if let Some(h) = obj.get("headers") {
        let map = h
            .as_object()
            .ok_or_else(|| "headers must be an object of strings".to_string())?;
        for (name, val) in map {
            let value = val
                .as_str()
                .ok_or_else(|| format!("header '{name}' must be a string"))?;
            let lname = name.trim().to_ascii_lowercase();
            if !is_header_token(&lname) {
                return Err(format!("header '{name}' is not a valid header name"));
            }
            if value.contains(['\r', '\n']) {
                return Err(format!("header '{name}' must not contain a line break"));
            }
            if CREDENTIAL_HEADERS.contains(&lname.as_str()) {
                return Err(format!(
                    "header '{name}' is a credential: set it with /guardian secret <host> {name} <value>, \
                     never in a request"
                ));
            }
            if TRANSPORT_HEADERS.contains(&lname.as_str()) {
                continue;
            }
            headers.retain(|(n, _)| n != &lname);
            headers.push((lname, value.to_string()));
        }
    }
    let header_bytes: usize = headers.iter().map(|(n, v)| n.len() + v.len()).sum();
    if headers.len() > HTTP_HEADERS_MAX || header_bytes > HTTP_HEADERS_BYTES_MAX {
        return Err(format!(
            "too many request headers (at most {HTTP_HEADERS_MAX}, {HTTP_HEADERS_BYTES_MAX} bytes)"
        ));
    }
    let body = match (obj.get("body"), obj.get("json")) {
        (Some(_), Some(_)) => return Err("give body or json, not both".to_string()),
        (Some(serde_json::Value::String(b)), None) => Some(b.as_bytes().to_vec()),
        (Some(serde_json::Value::Null), None) | (None, None) => None,
        (Some(_), None) => return Err("body must be a string (use json for a JSON value)".to_string()),
        (None, Some(j)) => {
            if !headers.iter().any(|(n, _)| n == "content-type") {
                headers.push(("content-type".to_string(), "application/json".to_string()));
            }
            Some(serde_json::to_vec(j).map_err(|e| e.to_string())?)
        }
    };
    if let Some(b) = &body {
        if matches!(method, Method::Get | Method::Head) {
            return Err(format!("a {} carries no body", method.as_str()));
        }
        if b.len() > HTTP_REQUEST_BODY_MAX {
            return Err(format!(
                "request body is {} bytes; the cap is {HTTP_REQUEST_BODY_MAX}",
                b.len()
            ));
        }
    }
    let max_bytes = obj
        .get("max_bytes")
        .and_then(|m| m.as_u64())
        .map(|m| usize::try_from(m).unwrap_or(usize::MAX).clamp(1, HTTP_RESPONSE_MAX_BYTES))
        .unwrap_or(policy.max_bytes);
    let timeout = obj
        .get("timeout_ms")
        .and_then(|t| t.as_u64())
        .map(|ms| Duration::from_millis(ms).clamp(Duration::from_millis(1), policy.timeout))
        .unwrap_or(policy.timeout);
    Ok(HttpRequest {
        method,
        url: parsed.to_string(),
        headers,
        body,
        timeout,
        max_bytes,
    })
}

/// The media type, without parameters, lowercase.
fn media_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// What a request may read back: text, JSON and XML in their forms, form
/// data and NDJSON. A binary body is refused by name.
fn request_content_type_allowed(content_type: &str) -> bool {
    let mt = media_type(content_type);
    mt.is_empty()
        || mt.starts_with("text/")
        || mt == "application/json"
        || mt == "application/xml"
        || mt == "application/x-ndjson"
        || mt == "application/x-www-form-urlencoded"
        || (mt.starts_with("application/") && (mt.ends_with("+json") || mt.ends_with("+xml")))
}

/// The response headers a guest sees: lowercase, no cookies, at most
/// `HTTP_HEADERS_MAX` and `HTTP_HEADERS_BYTES_MAX` in all, in the order
/// sent.
fn guest_headers(headers: &[(String, String)]) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    let mut bytes = 0usize;
    for (name, value) in headers {
        let lname = name.to_ascii_lowercase();
        if HIDDEN_RESPONSE_HEADERS.contains(&lname.as_str()) {
            continue;
        }
        if out.len() >= HTTP_HEADERS_MAX || bytes + lname.len() + value.len() > HTTP_HEADERS_BYTES_MAX {
            break;
        }
        bytes += lname.len() + value.len();
        out.insert(lname, serde_json::Value::String(value.clone()));
    }
    out
}

/// The host a secret is stored for, as the URL names it: lowercase, with
/// the port when the URL carries one.
fn secret_hosts_of(parsed: &url::Url) -> [String; 2] {
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    let with_port = match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.clone(),
    };
    [host, with_port]
}

/// The request guard. Returns the JSON string handed back to the guest
/// (always well-formed JSON — success or a structured error). Every hop
/// goes through `check_url`; the operator's secrets whose host is the
/// hop's host ride that hop and no other, over any header of the same
/// name; a GET or HEAD follows up to [`MAX_REDIRECTS`] redirects within
/// the budget, the other methods get the 3xx back with its target under
/// `redirect`. The envelope never carries a request header.
pub fn guarded_http_request(caps: &Capabilities, input: &str) -> String {
    let Some(http) = caps.http.as_ref() else {
        return err_json("capability 'http_request' not granted to this tool");
    };
    let req = match parse_http_request(input, &caps.http_policy) {
        Ok(r) => r,
        Err(e) => return err_json(&e),
    };
    let budget = req.timeout;
    let started = Instant::now();
    let mut current = req.url.clone();
    let mut hops = 0usize;
    loop {
        let parsed = match check_url(caps, &current) {
            Ok(u) => u,
            Err(e) => return err_json(&e),
        };
        let remaining = match budget.checked_sub(started.elapsed()) {
            Some(r) if !r.is_zero() => r,
            _ => return err_json(&format!("fetch budget of {budget:?} spent")),
        };
        let hosts = secret_hosts_of(&parsed);
        let mut headers = req.headers.clone();
        for secret in caps.secrets.iter().filter(|s| hosts.contains(&s.host)) {
            headers.retain(|(n, _)| n != &secret.name);
            headers.push((secret.name.clone(), secret.value.clone()));
        }
        let hop = HttpRequest {
            method: req.method,
            url: current.clone(),
            headers,
            body: req.body.clone(),
            timeout: remaining,
            max_bytes: req.max_bytes,
        };
        let resp = match http.request(&hop) {
            Ok(r) => r,
            Err(e) => return err_json(&e),
        };
        if is_redirect(resp.status)
            && let Some(loc) = resp.location.as_deref()
        {
            if req.method.follows_redirects() && hops < MAX_REDIRECTS {
                current = match parsed.join(loc) {
                    Ok(next) => next.to_string(),
                    Err(e) => return err_json(&format!("invalid redirect target: {e}")),
                };
                hops += 1;
                continue;
            }
            return serde_json::json!({
                "ok": true,
                "status": resp.status,
                "url": current,
                "redirects": hops,
                "redirect": loc,
                "content_type": resp.content_type,
                "headers": guest_headers(&resp.headers),
                "body": "",
            })
            .to_string();
        }
        if !request_content_type_allowed(&resp.content_type) {
            return err_json(&format!(
                "response content-type '{}' is not allowed (text, JSON, XML, NDJSON and form data are)",
                resp.content_type
            ));
        }
        let mut body = resp.body;
        let cut = body.len() > req.max_bytes;
        if cut {
            body.truncate(req.max_bytes);
        }
        let mut env = serde_json::json!({
            "ok": true,
            "status": resp.status,
            "url": current,
            "content_type": resp.content_type,
            "headers": guest_headers(&resp.headers),
            "body": String::from_utf8_lossy(&body),
        });
        if hops > 0 {
            env["redirects"] = serde_json::json!(hops);
        }
        if cut {
            // A body cut mid-way is not the answer: the guest says so.
            env["truncated_at"] = serde_json::json!(req.max_bytes);
        }
        return env.to_string();
    }
}

// ── web_search capability ──

/// A parsed, validated `web_search` request. The guest sends either a
/// bare query string (→ `{ q: <that> }`) or a JSON object with these
/// fields; everything is clamped / whitelisted host-side by
/// [`parse_request`] before it reaches a provider, so a hostile guest
/// cannot smuggle a provider parameter we did not vet.
#[derive(Clone, Debug)]
pub struct SearchRequest {
    pub q: String,
    /// Brave `freshness`: `pd|pw|pm|py` or a `YYYY-MM-DDtoYYYY-MM-DD`
    /// range. `None` ⇒ no recency filter.
    pub freshness: Option<String>,
    /// Brave `offset`: 0-based page index, clamped `0..=9`.
    pub offset: u32,
    /// Brave `count`: results per page, clamped `1..=20`.
    pub count: usize,
    /// Domains to exclude — appended to `q` as `-site:<d>` (Brave has no
    /// exclude parameter). Sanitized, at most 10.
    pub exclude: Vec<String>,
    /// Request Brave `extra_snippets` (extra excerpt text per result —
    /// cheaper than a fetch; partly answers "search is half-blind").
    pub extra_snippets: bool,
}

/// One normalized search hit. The Guardian flattens the provider's schema
/// to this stable shape so guests are provider-agnostic. The text is
/// still attacker-controlled — guests must injection-scrub it.
#[derive(Clone, Debug)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub description: String,
    /// Best-effort published/modified date (Brave `age` ‖ `page_age`).
    /// Provider-defined format, not guaranteed; `None` when absent.
    pub age: Option<String>,
    /// Brave `extra_snippets` (additional excerpts), when requested.
    pub snippets: Vec<String>,
}

/// A provider's full reply: the normalized result list plus optional
/// top-level enrichments Brave returns in the *same* web-search response.
/// `infobox` is best-effort (entity-type queries only; provider-defined
/// shape) — surfaced when present, omitted otherwise, exactly like
/// [`SearchResult::age`]. (Brave's `summarizer` web-response key is only
/// an opaque pointer to a *deprecated* separate endpoint, so it is
/// deliberately not surfaced — see the Answers API as the future path.)
#[derive(Clone, Debug, Default)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
    pub infobox: Option<serde_json::Value>,
}

impl From<Vec<SearchResult>> for SearchResponse {
    fn from(results: Vec<SearchResult>) -> Self {
        Self { results, infobox: None }
    }
}

/// Pluggable search backend. The impl performs the request + parsing;
/// **policy/normalization is enforced by [`guarded_web_search`]**, never
/// here. A future browser-driven backend is just another impl.
pub trait SearchProvider: Send + Sync {
    fn search(&self, req: &SearchRequest, timeout: Duration)
        -> Result<SearchResponse, String>;
}

/// Parse the guest-supplied bytes into a vetted [`SearchRequest`]. A JSON
/// **object** is treated as a structured request; anything else (bare
/// string, number, parse error) is treated as a plain query. Every field
/// is clamped / whitelisted here — never trust the guest's numbers.
fn parse_request(input: &str) -> SearchRequest {
    let mut req = SearchRequest {
        q: String::new(),
        freshness: None,
        offset: 0,
        count: 10,
        exclude: Vec::new(),
        extra_snippets: false,
    };
    match serde_json::from_str::<serde_json::Value>(input) {
        Ok(serde_json::Value::Object(m)) => {
            req.q = m.get("q").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            req.count = m
                .get("count")
                .and_then(|v| v.as_u64())
                .map(|n| n.clamp(1, 20) as usize)
                .unwrap_or(10);
            req.offset = m
                .get("offset")
                .and_then(|v| v.as_u64())
                .map(|n| n.min(9) as u32)
                .unwrap_or(0);
            req.extra_snippets =
                m.get("extra_snippets").and_then(|v| v.as_bool()).unwrap_or(false);
            req.freshness =
                m.get("freshness").and_then(|v| v.as_str()).and_then(normalize_freshness);
            if let Some(arr) = m.get("exclude").and_then(|v| v.as_array()) {
                req.exclude = arr
                    .iter()
                    .filter_map(|v| v.as_str())
                    .filter_map(sanitize_domain)
                    .take(10)
                    .collect();
            }
        }
        _ => req.q = input.trim().to_string(),
    }
    req
}

/// Map a freshness token to Brave's wire value. Accepts the friendly
/// `day|week|month|year`, the raw `pd|pw|pm|py`, or a validated
/// `YYYY-MM-DDtoYYYY-MM-DD` range. Anything else ⇒ `None` (filter
/// silently dropped, never passed through unvetted).
fn normalize_freshness(s: &str) -> Option<String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "day" | "pd" => Some("pd".into()),
        "week" | "pw" => Some("pw".into()),
        "month" | "pm" => Some("pm".into()),
        "year" | "py" => Some("py".into()),
        other => is_date_range(other).then(|| other.to_string()),
    }
}

fn is_date_range(s: &str) -> bool {
    matches!(s.split_once("to"), Some((a, b)) if is_ymd(a) && is_ymd(b))
}

fn is_ymd(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() }
        })
}

/// Reduce a guest-supplied exclude entry to a bare hostname. Strips a
/// scheme / path, lowercases, and accepts only `[a-z0-9.-]` with a dot —
/// so it cannot inject extra `q` operators when concatenated as `-site:`.
fn sanitize_domain(s: &str) -> Option<String> {
    let d = s
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let d = d.split('/').next().unwrap_or("").to_ascii_lowercase();
    (!d.is_empty()
        && d.len() <= 253
        && d.contains('.')
        && d.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-'))
    .then_some(d)
}

/// The guarded host method for `guardian::web_search`. Returns the JSON
/// envelope handed back to the guest (always well-formed; never panics).
pub fn guarded_web_search(caps: &Capabilities, input: &str) -> String {
    let Some(provider) = caps.search.as_ref() else {
        return err_json("search capability not configured (no Brave API key set)");
    };
    let req = parse_request(input);
    if req.q.is_empty() {
        return err_json("empty search query");
    }
    match provider.search(&req, Duration::from_secs(10)) {
        Ok(resp) => {
            let items: Vec<serde_json::Value> = resp
                .results
                .into_iter()
                .filter(|r| r.url.starts_with("https://"))
                .take(req.count.min(20))
                .map(|r| {
                    let mut o = serde_json::Map::new();
                    o.insert("title".into(), truncate(&r.title, 300).into());
                    o.insert("url".into(), r.url.into());
                    o.insert("description".into(), text_field(&r.description, 1000).into());
                    if let Some(age) = r.age.filter(|a| !a.is_empty()) {
                        o.insert("age".into(), age.into());
                    }
                    if !r.snippets.is_empty() {
                        let snips: Vec<serde_json::Value> = r
                            .snippets
                            .iter()
                            .take(5)
                            .map(|s| truncate(s, 500).into())
                            .collect();
                        o.insert("snippets".into(), snips.into());
                    }
                    serde_json::Value::Object(o)
                })
                .collect();
            let mut env = serde_json::json!({
                "ok": true, "query": req.q, "count": items.len(), "results": items,
            });
            // Top-level enrichment: surface Brave's `infobox` only when it
            // is present, non-null, and small enough to not bloat the
            // envelope (defense-in-depth before the brain 2 MiB cap).
            if let Some(ib) = resp.infobox
                && !ib.is_null()
                && ib.to_string().len() <= 8 * 1024
            {
                env["infobox"] = ib;
            }
            env.to_string()
        }
        Err(e) => err_json(&e),
    }
}

/// The description as the model should read it: reduced to text with the
/// reducer every guest ships, then cut at `cap`. Brave sends `description`
/// HTML-escaped with `<strong>` highlighting; a tool's injection scan then
/// runs over decoded text, where `you&#x27;re` and `you're` are the same
/// words (Embra#17). `title` and `extra_snippets` arrive as plain text and
/// are NOT reduced: a literal `MaybeUninit<u8>` or `#include <vector>` in
/// plain text would be stripped as a tag, and `<<` would open a tag that
/// never closes (the rerun of Embra#17).
fn text_field(s: &str, cap: usize) -> String {
    truncate(&crate::html_text::to_text(s), cap)
}

fn truncate(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Brave Search backend. Holds the API key host-side only; fixed
/// endpoint (`api.search.brave.com`) so there is no guest-controlled
/// URL / SSRF surface. reqwest blocking + rustls, no redirects.
pub struct BraveSearch {
    client: reqwest::blocking::Client,
    api_key: String,
}

impl BraveSearch {
    pub fn new(api_key: &str) -> Result<Self, String> {
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("embra-guardian/0.5")
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { client, api_key: api_key.trim().to_string() })
    }
}

impl SearchProvider for BraveSearch {
    fn search(&self, req: &SearchRequest, timeout: Duration)
        -> Result<SearchResponse, String> {
        // Brave has no exclude param — fold sanitized excludes into `q`
        // as `-site:` operators (sanitize_domain already removed anything
        // that could inject a second operator).
        let mut q = req.q.clone();
        for d in &req.exclude {
            q.push_str(" -site:");
            q.push_str(d);
        }
        let mut params: Vec<(&str, String)> = vec![
            ("q", q),
            ("count", req.count.clamp(1, 20).to_string()),
            ("offset", req.offset.min(9).to_string()),
        ];
        if let Some(f) = &req.freshness {
            params.push(("freshness", f.clone()));
        }
        if req.extra_snippets {
            params.push(("extra_snippets", "1".to_string()));
        }
        let resp = self
            .client
            .get("https://api.search.brave.com/res/v1/web/search")
            .query(&params)
            .header("Accept", "application/json")
            .header("X-Subscription-Token", &self.api_key)
            .timeout(timeout)
            .send()
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("brave search HTTP {}", status.as_u16()));
        }
        let v: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
        parse_brave_response(&v)
    }
}

/// Brave's web-search reply as a [`SearchResponse`]. The reply is an object
/// with `query` and the result containers Brave has for it; `web` is one
/// of them and is absent when nothing matched, so a missing or null
/// `web.results` is an empty set, not an error (the error used to stand
/// for "no results": a quoted nonsense query, Embra#17). A body that is
/// not an object is the one shape the parser refuses.
fn parse_brave_response(v: &serde_json::Value) -> Result<SearchResponse, String> {
    if !v.is_object() {
        return Err("unexpected brave response shape".to_string());
    }
    let arr: &[serde_json::Value] = v
        .get("web")
        .and_then(|w| w.get("results"))
        .and_then(|r| r.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    {
        // Brave's date field is undocumented (community-confirmed gap):
        // `age` and `page_age` both appear, format not guaranteed. Try
        // both, surface as an opaque string, never fail on its absence.
        let results: Vec<SearchResult> = arr
            .iter()
            .map(|r| {
                let age = r
                    .get("age")
                    .and_then(|x| x.as_str())
                    .or_else(|| r.get("page_age").and_then(|x| x.as_str()))
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                let snippets = r
                    .get("extra_snippets")
                    .and_then(|x| x.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str())
                            .map(|s| s.to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                SearchResult {
                    title: r.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    url: r.get("url").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    description: r
                        .get("description")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    age,
                    snippets,
                }
            })
            .collect();
        // Same defensive posture as `age`: Brave's `infobox` / GraphInfobox
        // child fields are undocumented + JS-rendered in the dashboard, so
        // treat it as opaque — whitelist known string fields, else a
        // size-capped shallow subset, else `None`. Never an error.
        let infobox = trim_infobox(v);
        Ok(SearchResponse { results, infobox })
    }
}

/// Reduce Brave's top-level `infobox` to a small, safe JSON object.
/// `infobox` is a `ResultContainer` (`{type, results:[GraphInfobox], …}`);
/// we take the first entry, keep a whitelist of known string fields, and
/// otherwise fall back to a shallow, size-capped clone. Returns `None`
/// when absent/empty so the envelope simply omits it.
fn trim_infobox(v: &serde_json::Value) -> Option<serde_json::Value> {
    let ib = v.get("infobox")?;
    // The entity object: `infobox.results[0]`, else the infobox itself.
    let entity = ib
        .get("results")
        .and_then(|r| r.as_array())
        .and_then(|a| a.first())
        .unwrap_or(ib);
    let obj = entity.as_object()?;

    let mut out = serde_json::Map::new();
    for key in ["type", "subtype", "label", "title", "category"] {
        if let Some(s) = obj.get(key).and_then(|x| x.as_str())
            && !s.is_empty()
        {
            out.insert(key.into(), truncate(s, 200).into());
        }
    }
    if let Some(d) = obj
        .get("long_desc")
        .or_else(|| obj.get("description"))
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
    {
        out.insert("description".into(), truncate(d, 2000).into());
    }
    if let Some(u) = obj.get("url").and_then(|x| x.as_str())
        && u.starts_with("https://")
    {
        out.insert("url".into(), u.to_string().into());
    }

    // Fallback: nothing matched the whitelist — surface a shallow,
    // size-capped clone so an unknown-but-useful shape is not lost, but a
    // hostile/huge blob cannot bloat the envelope.
    if out.is_empty() {
        let shallow = serde_json::Value::Object(obj.clone());
        if shallow.to_string().len() <= 4 * 1024 {
            return Some(shallow);
        }
        return None;
    }
    Some(serde_json::Value::Object(out))
}

/// Default transport: `reqwest` blocking + rustls (the workspace already
/// links this stack static-musl for the WardSONDB client). Redirects are
/// **not** followed here — an auto-followed 3xx is an SSRF bypass; the
/// guard follows them itself, each hop checked (`MAX_REDIRECTS`), from the
/// `location` this transport reports.
pub struct ReqwestTransport {
    client: reqwest::blocking::Client,
}

impl ReqwestTransport {
    pub fn new() -> Result<Self, String> {
        Self::with_roots(Vec::new())
    }

    /// With the operator's CA drop-ins beside the compiled-in roots, so a
    /// private host behind an internal CA (on the egress allowlist) is
    /// reachable the way the GitLab client reaches it.
    pub fn with_roots(roots: Vec<reqwest::Certificate>) -> Result<Self, String> {
        let mut builder = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("embra-guardian/0.5");
        for cert in roots {
            builder = builder.add_root_certificate(cert);
        }
        let client = builder.build().map_err(|e| e.to_string())?;
        Ok(Self { client })
    }
}

impl HttpTransport for ReqwestTransport {
    fn get(&self, url: &str, timeout: Duration, max_bytes: usize)
        -> Result<HttpResponse, String> {
        self.request(&HttpRequest::get(url, timeout, max_bytes))
    }

    fn request(&self, req: &HttpRequest) -> Result<HttpResponse, String> {
        use std::io::Read;
        let method = reqwest::Method::from_bytes(req.method.as_str().as_bytes())
            .map_err(|e| e.to_string())?;
        let mut builder = self.client.request(method, &req.url).timeout(req.timeout);
        for (name, value) in &req.headers {
            let n = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| format!("header '{name}': {e}"))?;
            let v = reqwest::header::HeaderValue::from_str(value)
                .map_err(|e| format!("header '{name}': {e}"))?;
            builder = builder.header(n, v);
        }
        if let Some(body) = &req.body {
            builder = builder.body(body.clone());
        }
        let mut resp = builder.send().map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(n, v)| (n.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
            .collect();
        // Read bounded: one byte past the cap, so a guard can tell a body
        // it cut from one that fit; never the whole of a large body.
        let mut body = Vec::new();
        (&mut resp)
            .take(req.max_bytes as u64 + 1)
            .read_to_end(&mut body)
            .map_err(|e| e.to_string())?;
        Ok(HttpResponse { status, content_type, body, location, headers })
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
            location: None,
            headers: Vec::new(),
        })
    }
}

/// A transport that answers per URL: `(url, status, location, body)`.
#[cfg(test)]
pub(crate) struct RouteTransport(pub Vec<(&'static str, u16, Option<&'static str>, &'static str)>);

#[cfg(test)]
impl HttpTransport for RouteTransport {
    fn get(&self, u: &str, _t: Duration, _m: usize) -> Result<HttpResponse, String> {
        let (_, status, location, body) = self
            .0
            .iter()
            .find(|(url, ..)| *url == u)
            .ok_or_else(|| format!("no route for {u}"))?;
        Ok(HttpResponse {
            status: *status,
            content_type: "text/html".into(),
            body: body.as_bytes().to_vec(),
            location: location.map(str::to_string),
            headers: Vec::new(),
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

    fn routed(routes: Vec<(&'static str, u16, Option<&'static str>, &'static str)>) -> Capabilities {
        Capabilities::with_http(Arc::new(RouteTransport(routes)), EgressPolicy::default())
    }

    #[test]
    fn a_redirect_is_followed_through_the_guard_and_named() {
        // Brave hands back a stale URL; the page moved. One hop, absolute.
        let caps = routed(vec![
            ("https://1.1.1.1/old.html", 301, Some("https://1.1.1.1/new"), ""),
            ("https://1.1.1.1/new", 200, None, "<p>moved here</p>"),
        ]);
        let v = parse_ok(&guarded_http_get(&caps, "https://1.1.1.1/old.html"));
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["status"], 200);
        assert_eq!(v["url"], "https://1.1.1.1/new");
        assert_eq!(v["redirects"], 1);
        assert_eq!(v["body"], "<p>moved here</p>");
        // A relative Location resolves against the URL that answered.
        let caps = routed(vec![
            ("https://1.1.1.1/a/old", 302, Some("../b/new"), ""),
            ("https://1.1.1.1/b/new", 200, None, "ok"),
        ]);
        let v = parse_ok(&guarded_http_get(&caps, "https://1.1.1.1/a/old"));
        assert_eq!(v["url"], "https://1.1.1.1/b/new", "{v}");
        // A direct answer carries no `redirects` key.
        let caps = routed(vec![("https://1.1.1.1/", 200, None, "ok")]);
        let v = parse_ok(&guarded_http_get(&caps, "https://1.1.1.1/"));
        assert!(v.get("redirects").is_none(), "{v}");
    }

    #[test]
    fn a_redirect_is_checked_like_a_first_request() {
        // To a private address: SSRF blocked on the hop.
        let caps = routed(vec![("https://1.1.1.1/", 302, Some("https://10.0.0.5/admin"), "")]);
        let v = parse_ok(&guarded_http_get(&caps, "https://1.1.1.1/"));
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("SSRF"), "{v}");
        // To http: refused like a first request.
        let caps = routed(vec![("https://1.1.1.1/", 301, Some("http://1.1.1.1/x"), "")]);
        let v = parse_ok(&guarded_http_get(&caps, "https://1.1.1.1/"));
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("https"), "{v}");
    }

    /// Answers a redirect, then a page, sleeping `delay` on every request.
    struct SlowRedirect(Duration);
    impl HttpTransport for SlowRedirect {
        fn get(&self, u: &str, _t: Duration, _m: usize) -> Result<HttpResponse, String> {
            std::thread::sleep(self.0);
            if u.ends_with("/a") {
                Ok(HttpResponse { status: 301, content_type: String::new(), body: vec![], location: Some("/b".into()), headers: vec![] })
            } else {
                Ok(HttpResponse { status: 200, content_type: "text/html".into(), body: b"ok".to_vec(), location: None, headers: vec![] })
            }
        }
    }

    #[test]
    fn the_fetch_budget_covers_every_hop() {
        // A 50 ms budget and a transport that takes 60 ms a hop: the
        // first hop answers, the second has no budget left.
        let caps = Capabilities::with_http(
            Arc::new(SlowRedirect(Duration::from_millis(60))),
            EgressPolicy { timeout: Duration::from_millis(50), ..Default::default() },
        );
        let v = parse_ok(&guarded_http_get(&caps, "https://1.1.1.1/a"));
        assert_eq!(v["ok"], false, "{v}");
        assert_eq!(v["error"], "fetch budget of 50ms spent");
        // With budget to spare, the chain completes.
        let caps = Capabilities::with_http(
            Arc::new(SlowRedirect(Duration::from_millis(1))),
            EgressPolicy { timeout: Duration::from_secs(1), ..Default::default() },
        );
        let v = parse_ok(&guarded_http_get(&caps, "https://1.1.1.1/a"));
        assert_eq!(v["url"], "https://1.1.1.1/b", "{v}");
    }

    #[test]
    fn redirects_stop_after_three_hops_and_name_the_target() {
        let caps = routed(vec![
            ("https://1.1.1.1/0", 301, Some("https://1.1.1.1/1"), ""),
            ("https://1.1.1.1/1", 301, Some("https://1.1.1.1/2"), ""),
            ("https://1.1.1.1/2", 301, Some("https://1.1.1.1/3"), ""),
            ("https://1.1.1.1/3", 301, Some("https://1.1.1.1/4"), ""),
            ("https://1.1.1.1/4", 200, None, "never reached"),
        ]);
        let v = parse_ok(&guarded_http_get(&caps, "https://1.1.1.1/0"));
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["status"], 301);
        assert_eq!(v["url"], "https://1.1.1.1/3");
        assert_eq!(v["redirects"], MAX_REDIRECTS);
        assert_eq!(v["redirect"], "https://1.1.1.1/4");
        assert_eq!(v["body"], "");
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
    fn every_address_class_is_named() {
        use AddressClass::*;
        let table: &[(&str, AddressClass)] = &[
            ("1.1.1.1", Public), ("2606:4700:4700::1111", Public), ("::ffff:1.1.1.1", Public),
            ("10.0.0.5", Private), ("172.16.9.9", Private), ("192.168.1.1", Private),
            ("100.64.0.1", Private), ("fd00::1", Private), ("fec0::1", Private), ("::ffff:10.0.0.1", Private),
            ("127.0.0.1", Loopback), ("127.9.9.9", Loopback), ("0.0.0.0", Loopback), ("::1", Loopback), ("::", Loopback),
            ("169.254.169.254", LinkLocal), ("fe80::1", LinkLocal),
            ("224.0.0.1", Multicast), ("239.255.255.250", Multicast), ("ff02::1", Multicast),
            ("0.1.2.3", Reserved), ("255.255.255.255", Reserved), ("240.0.0.1", Reserved),
            ("192.0.2.1", Reserved), ("198.51.100.7", Reserved), ("203.0.113.9", Reserved),
            ("198.18.0.1", Reserved), ("198.19.255.255", Reserved),
            ("64:ff9b::1.1.1.1", Reserved), ("2002::1", Reserved), ("::1.2.3.4", Reserved),
        ];
        for (lit, want) in table {
            let ip: IpAddr = lit.parse().unwrap();
            assert_eq!(classify(&ip), *want, "{lit}");
        }
        assert!(!is_blocked_ip(&"1.1.1.1".parse().unwrap()));
        assert!(is_blocked_ip(&"198.18.0.1".parse().unwrap()));
    }

    #[test]
    fn a_private_host_on_the_list_passes_and_loopback_never_does() {
        let mock = || MockTransport { status: 200, content_type: "text/plain".into(), body: b"in".to_vec() };
        let caps = Capabilities::with_http(
            Arc::new(mock()),
            EgressPolicy {
                private_hosts: vec!["10.0.0.5".into(), "127.0.0.1".into(), "169.254.169.254".into()],
                ..Default::default()
            },
        );
        let v = parse_ok(&guarded_http_get(&caps, "https://10.0.0.5/api"));
        assert_eq!(v["ok"], true, "a listed private host passes: {v}");
        assert_eq!(v["body"], "in");
        for (u, class) in [
            ("https://127.0.0.1/", "loopback"),
            ("https://169.254.169.254/latest/meta-data/", "link-local"),
        ] {
            let v = parse_ok(&guarded_http_get(&caps, u));
            assert_eq!(v["ok"], false, "{u}");
            let err = v["error"].as_str().unwrap();
            assert!(err.contains(class) && err.contains("refused always"), "{u}: {err}");
        }
        // A private host that is not on the list names the command.
        let v = parse_ok(&guarded_http_get(&caps, "https://10.0.0.6/"));
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("/guardian egress allow"), "{v}");
        // A redirect from a listed host to loopback is refused on the hop.
        let caps = Capabilities::with_http(
            Arc::new(RouteTransport(vec![("https://10.0.0.5/", 302, Some("https://127.0.0.1/"), "")])),
            EgressPolicy { private_hosts: vec!["10.0.0.5".into()], ..Default::default() },
        );
        let v = parse_ok(&guarded_http_get(&caps, "https://10.0.0.5/"));
        assert_eq!(v["ok"], false, "{v}");
        assert!(v["error"].as_str().unwrap().contains("loopback"), "{v}");
    }

    #[test]
    fn the_list_matches_a_name_its_subdomains_and_an_ip_literal() {
        assert!(host_matches("gitlab.ops.wsds", "ops.wsds"));
        assert!(host_matches("ops.wsds", "ops.wsds"));
        assert!(host_matches("gitlab.ops.wsds", "GitLab.OPS.wsds"));
        assert!(!host_matches("evilops.wsds", "ops.wsds"));
        assert!(!host_matches("ops.wsds.example", "ops.wsds"));
        assert!(host_matches("10.0.0.5", "10.0.0.5"));
        assert!(!host_matches("110.0.0.5", "10.0.0.5"));
        assert!(!host_matches("10.0.0.5", "0.0.5"), "an IP entry matches only itself");
        assert!(!host_matches("anything", " "));
    }

    #[test]
    fn ipv4_mapped_v6_private_is_blocked() {
        let mapped: IpAddr = "::ffff:10.0.0.1".parse().unwrap();
        assert!(is_blocked_ip(&mapped));
        let pub_v6: IpAddr = "2606:4700:4700::1111".parse().unwrap();
        assert!(!is_blocked_ip(&pub_v6));
    }
}

#[cfg(test)]
mod request_tests {
    use super::*;
    use std::sync::Mutex;

    /// Answers per URL and records every request it saw:
    /// `(url, status, location, body, content_type, response headers)`.
    struct Recorder {
        routes: Vec<(&'static str, u16, Option<&'static str>, &'static str, &'static str)>,
        extra_headers: Vec<(String, String)>,
        seen: Mutex<Vec<HttpRequest>>,
    }

    impl Recorder {
        fn new(routes: Vec<(&'static str, u16, Option<&'static str>, &'static str, &'static str)>) -> Arc<Self> {
            Arc::new(Self { routes, extra_headers: Vec::new(), seen: Mutex::new(Vec::new()) })
        }
        fn seen(&self) -> Vec<HttpRequest> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl HttpTransport for Recorder {
        fn get(&self, url: &str, timeout: Duration, max_bytes: usize) -> Result<HttpResponse, String> {
            self.request(&HttpRequest::get(url, timeout, max_bytes))
        }
        fn request(&self, req: &HttpRequest) -> Result<HttpResponse, String> {
            self.seen.lock().unwrap().push(req.clone());
            let (_, status, location, body, ct) = self
                .routes
                .iter()
                .find(|(u, ..)| *u == req.url)
                .ok_or_else(|| format!("no route for {}", req.url))?;
            let mut headers = vec![
                ("Content-Type".to_string(), ct.to_string()),
                ("Set-Cookie".to_string(), "sid=1".to_string()),
                ("X-RateLimit-Remaining".to_string(), "9".to_string()),
            ];
            headers.extend(self.extra_headers.iter().cloned());
            Ok(HttpResponse {
                status: *status,
                content_type: ct.to_string(),
                body: body.as_bytes().to_vec(),
                location: location.map(str::to_string),
                headers,
            })
        }
    }

    fn caps_with(rec: &Arc<Recorder>) -> Capabilities {
        let transport: Arc<dyn HttpTransport> = rec.clone();
        Capabilities::with_http(transport, EgressPolicy::default())
    }

    fn parse(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("the guard emits valid JSON")
    }

    fn header<'a>(req: &'a HttpRequest, name: &str) -> Option<&'a str> {
        req.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    #[test]
    fn a_post_carries_its_method_headers_and_body() {
        let rec = Recorder::new(vec![("https://1.1.1.1/api", 201, None, r#"{"id":7}"#, "application/json")]);
        let v = parse(&guarded_http_request(
            &caps_with(&rec),
            r#"{"url":"https://1.1.1.1/api","method":"post","headers":{"X-Trace":"abc","Accept":"application/json"},"json":{"a":1}}"#,
        ));
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["status"], 201);
        assert_eq!(v["body"], r#"{"id":7}"#);
        assert_eq!(v["headers"]["x-ratelimit-remaining"], "9");
        assert!(v["headers"].get("set-cookie").is_none(), "{v}");
        assert!(v.get("redirects").is_none());
        let seen = rec.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].method, Method::Post);
        assert_eq!(header(&seen[0], "x-trace"), Some("abc"));
        assert_eq!(header(&seen[0], "accept"), Some("application/json"));
        assert_eq!(header(&seen[0], "content-type"), Some("application/json"));
        assert_eq!(seen[0].body.as_deref(), Some(br#"{"a":1}"#.as_slice()));
    }

    #[test]
    fn a_secret_is_injected_for_its_host_and_not_for_a_redirect_to_another() {
        let rec = Recorder::new(vec![
            ("https://1.1.1.1/a", 302, Some("https://1.0.0.1/b"), "", "text/html"),
            ("https://1.0.0.1/b", 200, None, "there", "text/html"),
            ("https://1.1.1.1:8443/x", 200, None, "port", "text/plain"),
            ("https://1.1.1.1/x", 200, None, "noport", "text/plain"),
        ]);
        let mut caps = caps_with(&rec);
        caps.secrets = vec![
            SecretHeader { host: "1.1.1.1".into(), name: "authorization".into(), value: "Bearer t".into() },
            SecretHeader { host: "1.1.1.1:8443".into(), name: "x-api-key".into(), value: "k".into() },
        ];
        let v = parse(&guarded_http_request(&caps, r#"{"url":"https://1.1.1.1/a"}"#));
        assert_eq!(v["url"], "https://1.0.0.1/b", "{v}");
        assert_eq!(v["body"], "there");
        let seen = rec.seen();
        assert_eq!(header(&seen[0], "authorization"), Some("Bearer t"), "the first hop carries it");
        assert_eq!(header(&seen[1], "authorization"), None, "the other host does not");
        // A secret stored with a port matches only the URL with that port.
        let _ = guarded_http_request(&caps, r#"{"url":"https://1.1.1.1:8443/x"}"#);
        let _ = guarded_http_request(&caps, r#"{"url":"https://1.1.1.1/x"}"#);
        let seen = rec.seen();
        assert_eq!(header(&seen[2], "x-api-key"), Some("k"));
        assert_eq!(header(&seen[3], "x-api-key"), None);
        assert_eq!(header(&seen[3], "authorization"), Some("Bearer t"));
        // The envelope never echoes a request header.
        let text = guarded_http_request(&caps, r#"{"url":"https://1.1.1.1/x"}"#);
        assert!(!text.contains("Bearer"), "{text}");
    }

    #[test]
    fn a_guest_may_not_set_a_credential_header() {
        let rec = Recorder::new(vec![]);
        for h in ["Authorization", "cookie", "Proxy-Authorization"] {
            let v = parse(&guarded_http_request(
                &caps_with(&rec),
                &format!(r#"{{"url":"https://1.1.1.1/","headers":{{"{h}":"x"}}}}"#),
            ));
            assert_eq!(v["ok"], false, "{h}");
            assert!(v["error"].as_str().unwrap().contains("/guardian secret"), "{v}");
        }
        assert!(rec.seen().is_empty(), "nothing was sent");
    }

    #[test]
    fn transport_headers_are_dropped_and_bad_names_refused() {
        let rec = Recorder::new(vec![("https://1.1.1.1/", 200, None, "ok", "text/plain")]);
        let v = parse(&guarded_http_request(
            &caps_with(&rec),
            r#"{"url":"https://1.1.1.1/","headers":{"Host":"evil","Content-Length":"5","Connection":"close","X-Ok":"1"}}"#,
        ));
        assert_eq!(v["ok"], true, "{v}");
        let seen = rec.seen();
        assert_eq!(seen[0].headers, vec![("x-ok".to_string(), "1".to_string())]);
        for bad in [r#"{"url":"https://1.1.1.1/","headers":{"X Space":"1"}}"#, r#"{"url":"https://1.1.1.1/","headers":{"X-Ok":"a
b"}}"#] {
            let v = parse(&guarded_http_request(&caps_with(&rec), bad));
            assert_eq!(v["ok"], false, "{bad}");
        }
    }

    #[test]
    fn query_pairs_are_appended_encoded() {
        // The pairs come out in key order (a JSON object's keys are sorted
        // here; `preserve_order` is never enabled), form-encoded.
        let rec = Recorder::new(vec![("https://1.1.1.1/s?a=1&b=true&n=2&q=two+words", 200, None, "ok", "text/plain")]);
        let v = parse(&guarded_http_request(
            &caps_with(&rec),
            r#"{"url":"https://1.1.1.1/s?a=1","query":{"q":"two words","n":2,"b":true}}"#,
        ));
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["url"], "https://1.1.1.1/s?a=1&b=true&n=2&q=two+words");
        let v = parse(&guarded_http_request(&caps_with(&rec), r#"{"url":"https://1.1.1.1/s","query":{"q":[1]}}"#));
        assert_eq!(v["ok"], false);
    }

    #[test]
    fn a_redirect_is_followed_for_get_and_head_only() {
        let rec = Recorder::new(vec![
            ("https://1.1.1.1/old", 301, Some("/new"), "", "text/html"),
            ("https://1.1.1.1/new", 200, None, "moved", "text/html"),
        ]);
        let v = parse(&guarded_http_request(&caps_with(&rec), r#"{"url":"https://1.1.1.1/old"}"#));
        assert_eq!(v["url"], "https://1.1.1.1/new", "{v}");
        assert_eq!(v["redirects"], 1);
        assert_eq!(v["body"], "moved");
        let v = parse(&guarded_http_request(&caps_with(&rec), r#"{"url":"https://1.1.1.1/old","method":"POST","body":"x"}"#));
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["status"], 301);
        assert_eq!(v["redirect"], "/new");
        assert_eq!(v["body"], "");
        assert_eq!(rec.seen().len(), 3, "the POST was not followed");
    }

    #[test]
    fn text_json_xml_and_ndjson_pass_and_an_image_is_refused() {
        let rec = Recorder::new(vec![
            ("https://1.1.1.1/j", 200, None, "{}", "application/problem+json; charset=utf-8"),
            ("https://1.1.1.1/x", 200, None, "<a/>", "application/xml"),
            ("https://1.1.1.1/n", 200, None, "{}\n{}", "application/x-ndjson"),
            ("https://1.1.1.1/f", 200, None, "a=b", "application/x-www-form-urlencoded"),
            ("https://1.1.1.1/t", 404, None, "no such thing", "text/plain"),
            ("https://1.1.1.1/i", 200, None, "PNG", "image/png"),
        ]);
        for path in ["j", "x", "n", "f"] {
            let v = parse(&guarded_http_request(&caps_with(&rec), &format!(r#"{{"url":"https://1.1.1.1/{path}"}}"#)));
            assert_eq!(v["ok"], true, "{path}: {v}");
        }
        let v = parse(&guarded_http_request(&caps_with(&rec), r#"{"url":"https://1.1.1.1/t"}"#));
        assert_eq!(v["ok"], true, "a 404 is an answer, not a guard error: {v}");
        assert_eq!(v["status"], 404);
        assert_eq!(v["body"], "no such thing");
        let v = parse(&guarded_http_request(&caps_with(&rec), r#"{"url":"https://1.1.1.1/i"}"#));
        assert_eq!(v["ok"], false, "{v}");
        assert!(v["error"].as_str().unwrap().contains("image/png"), "{v}");
    }

    #[test]
    fn the_body_is_cut_at_max_bytes_and_the_caps_are_clamped() {
        let long: &'static str = Box::leak("x".repeat(2000).into_boxed_str());
        let rec = Recorder::new(vec![("https://1.1.1.1/", 200, None, long, "text/plain")]);
        let v = parse(&guarded_http_request(&caps_with(&rec), r#"{"url":"https://1.1.1.1/","max_bytes":100}"#));
        assert_eq!(v["body"].as_str().unwrap().len(), 100);
        assert_eq!(v["truncated_at"], 100, "{v}");
        assert_eq!(rec.seen()[0].max_bytes, 100);
        let v = parse(&guarded_http_request(&caps_with(&rec), r#"{"url":"https://1.1.1.1/","max_bytes":2000}"#));
        assert_eq!(v["body"].as_str().unwrap().len(), 2000);
        assert!(v.get("truncated_at").is_none(), "a body that fit: {v}");
        let _ = guarded_http_request(&caps_with(&rec), r#"{"url":"https://1.1.1.1/","max_bytes":99999999,"timeout_ms":999999}"#);
        let seen = rec.seen();
        assert_eq!(seen[2].max_bytes, HTTP_RESPONSE_MAX_BYTES);
        assert!(seen[2].timeout <= EgressPolicy::default().timeout);
        let v = parse(&guarded_http_request(&caps_with(&rec), r#"{"url":"https://1.1.1.1/","method":"GET","body":"x"}"#));
        assert_eq!(v["ok"], false, "a GET carries no body: {v}");
        let v = parse(&guarded_http_request(&caps_with(&rec), r#"{"url":"https://1.1.1.1/","method":"TRACE"}"#));
        assert_eq!(v["ok"], false, "{v}");
        let big = format!(r#"{{"url":"https://1.1.1.1/","method":"POST","body":"{}"}}"#, "y".repeat(HTTP_REQUEST_BODY_MAX + 1));
        let v = parse(&guarded_http_request(&caps_with(&rec), &big));
        assert!(v["error"].as_str().unwrap().contains("request body"), "{v}");
    }

    #[test]
    fn response_headers_are_lowercased_capped_and_without_cookies() {
        let mut rec = Recorder::new(vec![("https://1.1.1.1/", 200, None, "ok", "text/plain")]);
        Arc::get_mut(&mut rec).unwrap().extra_headers =
            (0..40).map(|i| (format!("X-H{i}"), "v".to_string())).collect();
        let v = parse(&guarded_http_request(&caps_with(&rec), r#"{"url":"https://1.1.1.1/"}"#));
        let headers = v["headers"].as_object().unwrap();
        assert!(headers.len() <= HTTP_HEADERS_MAX, "{}", headers.len());
        assert!(headers.keys().all(|k| k == &k.to_ascii_lowercase()));
        assert!(headers.get("set-cookie").is_none());
        assert_eq!(headers["content-type"], "text/plain");
    }

    #[test]
    fn the_request_guard_runs_the_egress_policy_on_every_hop() {
        let rec = Recorder::new(vec![("https://1.1.1.1/", 302, Some("https://127.0.0.1/"), "", "text/html")]);
        let v = parse(&guarded_http_request(&caps_with(&rec), r#"{"url":"https://1.1.1.1/"}"#));
        assert_eq!(v["ok"], false, "{v}");
        assert!(v["error"].as_str().unwrap().contains("loopback"), "{v}");
        let v = parse(&guarded_http_request(&caps_with(&rec), r#"{"url":"http://1.1.1.1/"}"#));
        assert!(v["error"].as_str().unwrap().contains("https"), "{v}");
        let v = parse(&guarded_http_request(&Capabilities::none(), r#"{"url":"https://1.1.1.1/"}"#));
        assert!(v["error"].as_str().unwrap().contains("not granted"), "{v}");
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;

    // Results-only mock: existing call sites pass `Ok(vec![..])` / `Err`
    // unchanged; the `From<Vec<SearchResult>>` impl lifts it to a
    // `SearchResponse` (no infobox).
    struct MockSearch(Result<Vec<SearchResult>, String>);
    impl SearchProvider for MockSearch {
        fn search(&self, _r: &SearchRequest, _t: Duration) -> Result<SearchResponse, String> {
            self.0.clone().map(SearchResponse::from)
        }
    }

    // Full mock for the enrichment tests: returns a `SearchResponse`
    // verbatim (so a test can inject an `infobox`).
    struct MockResp(Result<SearchResponse, String>);
    impl SearchProvider for MockResp {
        fn search(&self, _r: &SearchRequest, _t: Duration) -> Result<SearchResponse, String> {
            self.0.clone()
        }
    }

    fn sr(title: &str, url: &str, desc: &str) -> SearchResult {
        SearchResult {
            title: title.into(),
            url: url.into(),
            description: desc.into(),
            age: None,
            snippets: vec![],
        }
    }

    fn parse(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("guard must emit valid JSON")
    }

    #[test]
    fn not_configured_when_no_provider() {
        let v = parse(&guarded_web_search(&Capabilities::none(), "rust async"));
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("not configured"));
    }

    #[test]
    fn empty_query_rejected_bare_and_json() {
        let caps = Capabilities::with_search(Arc::new(MockSearch(Ok(vec![]))));
        for input in ["   ", r#"{"q":"  "}"#, r#"{"count":5}"#] {
            let v = parse(&guarded_web_search(&caps, input));
            assert_eq!(v["ok"], false, "input {input:?}");
            assert!(v["error"].as_str().unwrap().contains("empty"));
        }
    }

    #[test]
    fn normalizes_and_filters_non_https() {
        let caps = Capabilities::with_search(Arc::new(MockSearch(Ok(vec![
            sr("Tokio", "https://tokio.rs", "async runtime"),
            sr("Insecure", "http://nope.test", "dropped"),
        ]))));
        let v = parse(&guarded_web_search(&caps, "rust async"));
        assert_eq!(v["ok"], true);
        assert_eq!(v["query"], "rust async");
        assert_eq!(v["count"], 1, "non-https result is dropped");
        assert_eq!(v["results"][0]["url"], "https://tokio.rs");
        assert_eq!(v["results"][0]["title"], "Tokio");
    }

    #[test]
    fn age_and_snippets_only_when_present() {
        let with = SearchResult {
            age: Some("2024-10-08T10:30:00Z".into()),
            snippets: vec!["extra one".into(), "extra two".into()],
            ..sr("Doc", "https://docs.rs/x", "d")
        };
        let caps = Capabilities::with_search(Arc::new(MockSearch(Ok(vec![
            with,
            sr("Bare", "https://bare.rs", "b"),
        ]))));
        let v = parse(&guarded_web_search(&caps, "q"));
        assert_eq!(v["results"][0]["age"], "2024-10-08T10:30:00Z");
        assert_eq!(v["results"][0]["snippets"][1], "extra two");
        assert!(v["results"][1].get("age").is_none(), "no age key when absent");
        assert!(v["results"][1].get("snippets").is_none(), "no snippets key when empty");
    }

    #[test]
    fn provider_error_surfaces() {
        let caps =
            Capabilities::with_search(Arc::new(MockSearch(Err("brave search HTTP 401".into()))));
        let v = parse(&guarded_web_search(&caps, "x"));
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("401"));
    }

    // The provider (`BraveSearch`) is responsible for trimming via
    // `trim_infobox`; the guard only presence/size-gates and passes the
    // already-trimmed value through. So the mock injects the post-trim
    // shape; `trim_infobox` itself is unit-tested separately below.
    #[test]
    fn infobox_surfaced_when_present() {
        let resp = SearchResponse {
            results: vec![sr("R", "https://r.rs", "d")],
            infobox: Some(serde_json::json!({
                "title": "Rust",
                "description": "A systems programming language.",
                "url": "https://www.rust-lang.org",
            })),
        };
        let caps = Capabilities::with_search(Arc::new(MockResp(Ok(resp))));
        let v = parse(&guarded_web_search(&caps, "rust"));
        assert_eq!(v["ok"], true);
        assert_eq!(v["infobox"]["title"], "Rust");
        assert_eq!(v["infobox"]["description"], "A systems programming language.");
        assert_eq!(v["infobox"]["url"], "https://www.rust-lang.org");
        assert_eq!(v["results"][0]["title"], "R");
    }

    #[test]
    fn infobox_omitted_when_absent_or_oversized() {
        // absent
        let caps = Capabilities::with_search(Arc::new(MockResp(Ok(SearchResponse::from(
            vec![sr("R", "https://r.rs", "d")],
        )))));
        let v = parse(&guarded_web_search(&caps, "ordinary query"));
        assert_eq!(v["ok"], true);
        assert!(v.get("infobox").is_none(), "no infobox key when absent");
        // oversized → guard drops it (defense-in-depth before the 2 MiB cap)
        let big = SearchResponse {
            results: vec![sr("R", "https://r.rs", "d")],
            infobox: Some(serde_json::json!({ "blob": "x".repeat(9 * 1024) })),
        };
        let caps = Capabilities::with_search(Arc::new(MockResp(Ok(big))));
        let v = parse(&guarded_web_search(&caps, "q"));
        assert_eq!(v["ok"], true);
        assert!(v.get("infobox").is_none(), "oversized infobox dropped");
    }

    #[test]
    fn trim_infobox_whitelists_and_falls_back() {
        // Brave shape: infobox → results[0]; whitelist kept, junk dropped,
        // non-https url dropped, long_desc → description.
        let raw = serde_json::json!({
            "infobox": { "type": "infobox", "results": [{
                "title": "Rust",
                "long_desc": "A systems programming language.",
                "url": "https://www.rust-lang.org",
                "junk": "z".repeat(50),
            }]}
        });
        let t = trim_infobox(&raw).expect("present");
        assert_eq!(t["title"], "Rust");
        assert_eq!(t["description"], "A systems programming language.");
        assert_eq!(t["url"], "https://www.rust-lang.org");
        assert!(t.get("junk").is_none());

        // non-https url is dropped
        let raw = serde_json::json!({
            "infobox": { "results": [{ "title": "X", "url": "http://insecure" }] }
        });
        let t = trim_infobox(&raw).expect("present");
        assert_eq!(t["title"], "X");
        assert!(t.get("url").is_none(), "non-https url dropped");

        // absent → None
        assert!(trim_infobox(&serde_json::json!({ "web": {} })).is_none());

        // unknown shape, small → shallow fallback; huge → None
        let small = serde_json::json!({ "infobox": { "results": [{ "weird": 1 }] } });
        assert!(trim_infobox(&small).is_some(), "small unknown shape kept shallow");
        let huge = serde_json::json!({
            "infobox": { "results": [{ "weird": "y".repeat(5 * 1024) }] }
        });
        assert!(trim_infobox(&huge).is_none(), "oversized unknown shape dropped");
    }

    #[test]
    fn bare_string_is_the_query() {
        let r = parse_request("  rust async  ");
        assert_eq!(r.q, "rust async");
        assert_eq!(r.count, 10);
        assert_eq!(r.offset, 0);
        assert!(r.freshness.is_none() && r.exclude.is_empty() && !r.extra_snippets);
    }

    #[test]
    fn json_request_is_parsed_and_clamped() {
        let r = parse_request(
            r#"{"q":"x","count":999,"offset":50,"freshness":"week",
                "exclude":["https://Pinterest.com/board","ok.dev","b@d"],
                "extra_snippets":true}"#,
        );
        assert_eq!(r.q, "x");
        assert_eq!(r.count, 20, "count clamped to 20");
        assert_eq!(r.offset, 9, "offset clamped to 9");
        assert_eq!(r.freshness.as_deref(), Some("pw"));
        assert!(r.extra_snippets);
        assert_eq!(r.exclude, vec!["pinterest.com".to_string(), "ok.dev".to_string()],
            "scheme/path stripped, lowercased, junk dropped");
    }

    #[test]
    fn count_zero_floor_and_freshness_variants() {
        assert_eq!(parse_request(r#"{"q":"x","count":0}"#).count, 1);
        assert_eq!(parse_request(r#"{"q":"x","freshness":"day"}"#).freshness.as_deref(), Some("pd"));
        assert_eq!(parse_request(r#"{"q":"x","freshness":"py"}"#).freshness.as_deref(), Some("py"));
        assert!(parse_request(r#"{"q":"x","freshness":"bogus"}"#).freshness.is_none());
        assert_eq!(
            parse_request(r#"{"q":"x","freshness":"2024-01-01to2024-12-31"}"#).freshness.as_deref(),
            Some("2024-01-01to2024-12-31"),
            "validated date range passes through"
        );
        assert!(
            parse_request(r#"{"q":"x","freshness":"2024-1-1to2024-12-31"}"#).freshness.is_none(),
            "malformed date range dropped"
        );
    }

    #[test]
    fn json_string_or_array_is_treated_as_bare_query() {
        // Only an object is a structured request; a JSON string/array is
        // the literal query text.
        assert_eq!(parse_request(r#""hello world""#).q, r#""hello world""#);
        assert_eq!(parse_request("[1,2]").q, "[1,2]");
    }

    #[test]
    fn a_brave_reply_without_web_is_an_empty_set() {
        // Brave omits `web` when nothing matched (a quoted nonsense query,
        // Embra#17). That used to be "unexpected brave response shape".
        let empty = serde_json::json!({"type": "search", "query": {"original": "\"qzvxkjp0193\""}});
        let r = parse_brave_response(&empty).unwrap();
        assert!(r.results.is_empty());
        assert!(r.infobox.is_none());
        // An infobox with no web results survives the empty set.
        let with_box = serde_json::json!({"type": "search", "query": {"original": "x"},
            "infobox": {"type": "graph", "results": [{"title": "T", "description": "D"}]}});
        let r = parse_brave_response(&with_box).unwrap();
        assert!(r.results.is_empty());
        assert_eq!(r.infobox, trim_infobox(&with_box));
        // And a null `results` is the same empty set.
        let null_results = serde_json::json!({"type": "search", "web": {"results": null}});
        assert!(parse_brave_response(&null_results).unwrap().results.is_empty());
    }

    #[test]
    fn a_brave_reply_that_is_not_an_object_is_an_error() {
        for v in [serde_json::json!([]), serde_json::json!("search"), serde_json::json!(null)] {
            let err = parse_brave_response(&v).unwrap_err();
            assert!(err.contains("unexpected brave response shape"), "{v}: {err}");
        }
    }

    #[test]
    fn a_brave_reply_with_results_parses_as_before() {
        let v = serde_json::json!({"type": "search", "query": {"original": "tokio"}, "web": {"results": [
            {"title": "Tokio", "url": "https://tokio.rs", "description": "async <strong>runtime</strong>",
             "page_age": "2024-10-08T10:30:00Z", "extra_snippets": ["one", "two"]},
            {"title": "No url"}
        ]}});
        let r = parse_brave_response(&v).unwrap();
        assert_eq!(r.results.len(), 2);
        assert_eq!(r.results[0].title, "Tokio");
        // The provider keeps Brave's bytes; the guard reduces them.
        assert_eq!(r.results[0].description, "async <strong>runtime</strong>");
        assert_eq!(r.results[0].age.as_deref(), Some("2024-10-08T10:30:00Z"));
        assert_eq!(r.results[0].snippets, vec!["one".to_string(), "two".to_string()]);
        assert_eq!(r.results[1].url, "");
    }

    #[test]
    fn the_description_is_reduced_to_text_and_title_and_snippets_are_kept_as_sent() {
        // Brave: the description HTML-escaped with <strong> markup; the
        // title and the extra snippets plain text, angle brackets and all
        // (the samples are from Embra#17 and its rerun).
        let hit = SearchResult {
            title: "std::vector<T,Allocator>::push_back - cppreference.com".into(),
            url: "https://en.cppreference.com/w/cpp/container/vector/push_back".into(),
            description: "In May 2023, <strong>Brave</strong> announced it&#x27;s &quot;own&quot; index &lt;u8&gt;".into(),
            age: None,
            snippets: vec![
                "#include <vector> int main() { std::vector<int> numbers; std::cout << 1 << '\\n'; }".into(),
                "have a fixed static MaybeUninit<u8> array".into(),
            ],
        };
        let long = SearchResult { description: format!("<b>{}</b>", "x".repeat(1200)), ..hit.clone() };
        let caps = Capabilities::with_search(Arc::new(MockSearch(Ok(vec![hit.clone(), long]))));
        let v = parse(&guarded_web_search(&caps, "vector"));
        assert_eq!(v["results"][0]["title"], hit.title);
        assert_eq!(v["results"][0]["description"], "In May 2023, Brave announced it's \"own\" index <u8>");
        assert_eq!(v["results"][0]["snippets"][0], hit.snippets[0]);
        assert_eq!(v["results"][0]["snippets"][1], hit.snippets[1]);
        // Reduced first, then cut: the cut falls on text, never inside a tag.
        let d = v["results"][1]["description"].as_str().unwrap();
        assert!(d.starts_with("xxxx") && d.ends_with('…') && !d.contains('<'), "{d}");
        assert_eq!(d.chars().filter(|c| *c == 'x').count(), 1000);
    }
}
