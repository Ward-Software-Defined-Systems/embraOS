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
        Self { http: Some(transport), http_policy: policy, search: None }
    }
    /// Grant `web_search` backed by `provider`.
    pub fn with_search(provider: Arc<dyn SearchProvider>) -> Self {
        Self { search: Some(provider), ..Self::default() }
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
                    o.insert("description".into(), truncate(&r.description, 1000).into());
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
        let arr = v
            .get("web")
            .and_then(|w| w.get("results"))
            .and_then(|r| r.as_array())
            .ok_or_else(|| "unexpected brave response shape".to_string())?;
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
        let infobox = trim_infobox(&v);
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
}
