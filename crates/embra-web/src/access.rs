//! The web console's gate: a per-boot access token. embrad mints one and
//! hands it over in `EMBRA_WEB_TOKEN`; the URL it prints at boot carries it
//! (`https://localhost:3345/embraOS?token=…`). The first visit with the
//! token sets a cookie and redirects to the clean URL; every later request,
//! WebSocket handshakes included, is let through on the cookie; anything
//! else on `:3345` gets 401. The token is 128 bits as 32 lowercase hex
//! characters, compared over its whole length whatever matches. The
//! arbiter keeps its role: who types, among those who hold the token.
//!
//! The cookie is `SameSite=Strict`, which keeps it from a page on another
//! site; a page on another port of the same host is the same site, gets the
//! cookie attached, and a WebSocket handshake is not held back by CORS. So a
//! handshake or a write that carries a browser's `Origin` must name the
//! console it reached (`origin_allowed`, 403 otherwise) (#5).
//!
//! On the host, without embrad, embra-web mints its own token and prints
//! the URL once to its stdout; in the OS the token is never logged — the
//! boot banner is the one place it is shown.

use std::io::Read;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

/// The environment variable embrad sets for embra-web.
pub const TOKEN_ENV: &str = "EMBRA_WEB_TOKEN";
/// The cookie the first visit sets.
pub const COOKIE: &str = "embra_token";
/// What a request without the token is told. Never the token.
pub const REFUSAL: &str = "embraOS web console: open the URL printed on the serial console at boot; it carries this boot's access token.\n";
/// What a socket handshake or a write from another origin is told.
pub const ORIGIN_REFUSAL: &str = "embraOS web console: refused — the request came from a page on another origin; the console answers the pages it serves itself.\n";

/// 16 bytes of `/dev/urandom` as 32 lowercase hex characters. A failed read
/// is an error, never a weaker source.
pub fn generate_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(hex_of(&bytes))
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The startup line: the URL with the token only when embra-web minted it
/// itself (the host-side stack, whose stdout is the developer's terminal).
/// A token from the environment is embrad's to show, in the boot banner,
/// and never enters a log.
pub fn startup_line(from_env: bool, port: u16, token: &str) -> String {
    if from_env {
        "access token: from the environment; the URL is on the serial console".to_string()
    } else {
        format!("web console: https://localhost:{port}/embraOS?token={token}")
    }
}

/// Compared over the whole length whatever matches: no early return on a
/// byte, so the time taken says nothing about where the first difference
/// is. The lengths may differ, and that is visible; the token's is fixed.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// `token=…` from the query, as typed.
fn query_token(uri: &Uri) -> Option<String> {
    uri.query()?
        .split('&')
        .find_map(|pair| pair.strip_prefix("token=").map(str::to_string))
}

/// `embra_token=…` from the `Cookie` headers: the one that matches when
/// several are sent (a same-site page can set one of its own on the host,
/// and the browser sends both), else the first, so a wrong one is still
/// refused as such.
fn cookie_token(headers: &HeaderMap, expected: &str) -> Option<String> {
    let prefix = format!("{COOKIE}=");
    let mut first = None;
    for candidate in headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .map(str::trim)
        .filter_map(|c| c.strip_prefix(prefix.as_str()))
    {
        if constant_time_eq(candidate.as_bytes(), expected.as_bytes()) {
            return Some(candidate.to_string());
        }
        first.get_or_insert_with(|| candidate.to_string());
    }
    first
}

/// Whether a browser's `Origin` names the console it reached: `https://`
/// and the request's own host and port, since the console is HTTPS only
/// and embrad fixes neither the name the operator types nor the forward.
/// A request without `Origin` is not one a browser sends on another page's
/// behalf and passes; the token still gates it.
pub fn origin_allowed(origin: Option<&str>, host: Option<&str>) -> bool {
    match (origin, host) {
        (None, _) => true,
        (Some(origin), Some(host)) => origin.eq_ignore_ascii_case(&format!("https://{host}")),
        (Some(_), None) => false,
    }
}

/// The requests the origin rule applies to: a WebSocket handshake, which
/// CORS never holds back, and anything but a read, which a browser sends
/// before it knows the answer may not be read. A plain GET is left to CORS,
/// which gives a cross-origin page no way to read what it gets.
pub fn origin_gated(method: &Method, headers: &HeaderMap) -> bool {
    is_websocket_upgrade(headers) || !matches!(*method, Method::GET | Method::HEAD)
}

fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// The host the request named: the `Host` header, or the URI's authority
/// where HTTP/2 carries it instead.
fn request_host(uri: &Uri, headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| uri.authority().map(|a| a.to_string()))
}

/// The same path and query without the `token` parameter.
fn clean_location(path: &str, query: Option<&str>) -> String {
    let rest: Vec<&str> = query
        .unwrap_or("")
        .split('&')
        .filter(|pair| !pair.is_empty() && !pair.starts_with("token="))
        .collect();
    if rest.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", rest.join("&"))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// The request goes through.
    Pass,
    /// The entry hit: set the cookie and send the browser to the URL
    /// without the token.
    SetCookieAndRedirect { location: String, set_cookie: String },
    /// 401 with the notice.
    Refuse,
    /// 403: the token was right, but the handshake or write came from a
    /// page on another origin.
    RefuseOrigin,
}

/// What a request gets. A matching cookie passes. A matching query token
/// passes a `/ws/` or `/api/` request as it is — a WebSocket client cannot
/// follow a redirect, and the host-side probes pass `?token=` — and sends
/// any other request on to the clean URL with the cookie set. Anything else
/// is refused.
pub fn decide(
    path: &str,
    query: Option<&str>,
    query_token: Option<&str>,
    cookie_token: Option<&str>,
    expected: &str,
) -> Decision {
    let ok = |candidate: Option<&str>| {
        candidate.is_some_and(|c| constant_time_eq(c.as_bytes(), expected.as_bytes()))
    };
    if ok(cookie_token) {
        return Decision::Pass;
    }
    if ok(query_token) {
        if path.starts_with("/ws/") || path.starts_with("/api/") {
            return Decision::Pass;
        }
        return Decision::SetCookieAndRedirect {
            location: clean_location(path, query),
            set_cookie: format!("{COOKIE}={expected}; Path=/; Secure; HttpOnly; SameSite=Strict"),
        };
    }
    Decision::Refuse
}

/// The whole rule for one request: the token (`decide`), then the origin
/// rule for the requests it applies to. Pure over the request's parts, so
/// the layer is a match on it and the tests build requests.
pub fn admit(method: &Method, uri: &Uri, headers: &HeaderMap, expected: &str) -> Decision {
    let decision = decide(
        uri.path(),
        uri.query(),
        query_token(uri).as_deref(),
        cookie_token(headers, expected).as_deref(),
        expected,
    );
    if decision != Decision::Pass {
        return decision;
    }
    if origin_gated(method, headers) {
        let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
        if !origin_allowed(origin, request_host(uri, headers).as_deref()) {
            return Decision::RefuseOrigin;
        }
    }
    Decision::Pass
}

fn refusal(status: StatusCode, notice: &'static str) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        notice,
    )
        .into_response()
}

/// The layer over every route and the fallback.
pub async fn require_token(State(st): State<AppState>, req: Request, next: Next) -> Response {
    match admit(req.method(), req.uri(), req.headers(), &st.token) {
        Decision::Pass => next.run(req).await,
        Decision::SetCookieAndRedirect { location, set_cookie } => Response::builder()
            .status(StatusCode::SEE_OTHER)
            .header(header::LOCATION, location)
            .header(header::SET_COOKIE, set_cookie)
            .header(header::CACHE_CONTROL, "no-store")
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Decision::Refuse => refusal(StatusCode::UNAUTHORIZED, REFUSAL),
        Decision::RefuseOrigin => refusal(StatusCode::FORBIDDEN, ORIGIN_REFUSAL),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn a_request_without_a_token_is_refused() {
        assert_eq!(decide("/embraOS", None, None, None, T), Decision::Refuse);
        assert_eq!(decide("/ws/terminal", None, None, None, T), Decision::Refuse);
        assert_eq!(decide("/assets/app.css", None, None, None, T), Decision::Refuse);
    }

    #[test]
    fn a_matching_cookie_passes() {
        assert_eq!(decide("/embraOS", None, None, Some(T), T), Decision::Pass);
        assert_eq!(decide("/ws/tools", None, None, Some(T), T), Decision::Pass);
    }

    #[test]
    fn a_matching_query_token_sets_the_cookie_and_redirects_to_the_clean_url() {
        let q = format!("token={T}");
        assert_eq!(
            decide("/embraOS", Some(&q), Some(T), None, T),
            Decision::SetCookieAndRedirect {
                location: "/embraOS".into(),
                set_cookie: format!("embra_token={T}; Path=/; Secure; HttpOnly; SameSite=Strict"),
            }
        );
        let q = format!("token={T}&x=1");
        let Decision::SetCookieAndRedirect { location, .. } = decide("/embraOS", Some(&q), Some(T), None, T) else {
            panic!("the entry hit redirects");
        };
        assert_eq!(location, "/embraOS?x=1", "the other parameters stay");
    }

    #[test]
    fn a_query_token_on_a_socket_or_api_path_passes_without_a_redirect() {
        let q = format!("token={T}");
        assert_eq!(decide("/ws/tools", Some(&q), Some(T), None, T), Decision::Pass);
        assert_eq!(decide("/api/status", Some(&q), Some(T), None, T), Decision::Pass);
    }

    #[test]
    fn a_wrong_token_is_refused_and_sets_no_cookie() {
        let wrong = "ffffffffffffffffffffffffffffffff";
        assert_eq!(decide("/embraOS", Some("token=ff"), Some(wrong), None, T), Decision::Refuse);
        assert_eq!(decide("/embraOS", None, None, Some(wrong), T), Decision::Refuse);
        assert_eq!(decide("/embraOS", None, None, Some(""), T), Decision::Refuse);
    }

    #[test]
    fn the_cookie_is_http_only_secure_and_strict() {
        let q = format!("token={T}");
        let Decision::SetCookieAndRedirect { set_cookie, .. } = decide("/embraOS", Some(&q), Some(T), None, T) else {
            panic!("the entry hit redirects");
        };
        assert_eq!(set_cookie, format!("embra_token={T}; Path=/; Secure; HttpOnly; SameSite=Strict"));
    }

    #[test]
    fn the_comparison_runs_over_the_whole_token() {
        assert!(constant_time_eq(T.as_bytes(), T.as_bytes()));
        let last_byte_off = format!("{}e", &T[..T.len() - 1]);
        assert!(!constant_time_eq(T.as_bytes(), last_byte_off.as_bytes()));
        assert!(!constant_time_eq(T.as_bytes(), &T.as_bytes()[..31]));
        assert!(!constant_time_eq(b"", T.as_bytes()));
    }

    #[test]
    fn a_generated_token_is_thirty_two_lowercase_hex_characters() {
        let t = generate_token().expect("/dev/urandom");
        assert_eq!(t.len(), 32);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(t, generate_token().unwrap(), "two tokens differ");
        assert_eq!(hex_of(&[0xde, 0xad, 0x00, 0x0f]), "dead000f");
    }

    #[test]
    fn the_refusal_names_the_serial_console_and_not_the_token() {
        assert!(REFUSAL.contains("serial console"));
        assert!(!REFUSAL.contains(T));
        assert!(ORIGIN_REFUSAL.contains("another origin"));
        assert!(!ORIGIN_REFUSAL.contains(T));
    }

    /// #5: `SameSite=Strict` compares sites, not origins, so a page on
    /// another port of the same host sends the cookie. A browser's `Origin`
    /// has to be the console the request reached; nothing else is the same
    /// origin, `null` and `http://` included. No `Origin` is not a browser
    /// acting for another page.
    #[test]
    fn a_browser_origin_must_be_the_console_it_reached() {
        assert!(origin_allowed(Some("https://localhost:3345"), Some("localhost:3345")));
        assert!(origin_allowed(Some("HTTPS://LOCALHOST:3345"), Some("localhost:3345")));
        assert!(origin_allowed(Some("https://embra.example"), Some("embra.example")));
        assert!(origin_allowed(Some("https://[::1]:3345"), Some("[::1]:3345")));
        assert!(origin_allowed(None, Some("localhost:3345")));
        assert!(!origin_allowed(Some("https://localhost:8080"), Some("localhost:3345")), "another port");
        assert!(!origin_allowed(Some("https://evil.example"), Some("localhost:3345")), "another host");
        assert!(!origin_allowed(Some("http://localhost:3345"), Some("localhost:3345")), "another scheme");
        assert!(!origin_allowed(Some("null"), Some("localhost:3345")), "an opaque origin");
        assert!(!origin_allowed(Some("https://localhost:3345"), None), "no host to compare with");
    }

    /// The rule applies to a WebSocket handshake and to anything but a read;
    /// a plain GET is left to CORS, which lets no cross-origin page read it.
    #[test]
    fn only_socket_handshakes_and_writes_are_origin_gated() {
        let plain = HeaderMap::new();
        let mut upgrade = HeaderMap::new();
        upgrade.insert(header::UPGRADE, "websocket".parse().unwrap());
        assert!(!origin_gated(&Method::GET, &plain));
        assert!(!origin_gated(&Method::HEAD, &plain));
        assert!(origin_gated(&Method::GET, &upgrade));
        assert!(origin_gated(&Method::POST, &plain));
        assert!(origin_gated(&Method::PUT, &plain));
        assert!(origin_gated(&Method::DELETE, &plain));
    }

    fn request(method: Method, target: &str, headers: &[(header::HeaderName, &str)]) -> (Method, Uri, HeaderMap) {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.append(name.clone(), value.parse().unwrap());
        }
        (method, target.parse().unwrap(), map)
    }

    /// The whole rule over built requests: the token first (401), then the
    /// origin (403) for a handshake or a write with the cookie, while a read
    /// from another origin and a request without `Origin` pass.
    #[test]
    fn a_socket_handshake_or_a_write_from_another_origin_is_refused_even_with_the_cookie() {
        let cookie = format!("embra_token={T}");
        let ws = [(header::UPGRADE, "websocket"), (header::CONNECTION, "upgrade"), (header::HOST, "localhost:3345")];
        let admit_with = |method: Method, target: &str, extra: &[(header::HeaderName, &str)]| {
            let mut all: Vec<(header::HeaderName, &str)> = ws.to_vec();
            all.extend_from_slice(extra);
            let (m, u, h) = request(method, target, &all);
            admit(&m, &u, &h, T)
        };
        // No cookie: 401 before any origin question.
        assert_eq!(admit_with(Method::GET, "/ws/terminal", &[(header::ORIGIN, "https://localhost:8080")]), Decision::Refuse);
        // The cookie and the console's own origin: the handshake passes.
        assert_eq!(
            admit_with(Method::GET, "/ws/terminal", &[(header::COOKIE, &cookie), (header::ORIGIN, "https://localhost:3345")]),
            Decision::Pass
        );
        // The cookie, attached because the page is same-site, and another port: 403.
        assert_eq!(
            admit_with(Method::GET, "/ws/terminal", &[(header::COOKIE, &cookie), (header::ORIGIN, "https://localhost:8080")]),
            Decision::RefuseOrigin
        );
        // The same for a write.
        let (m, u, h) = request(
            Method::POST,
            "/api/stop",
            &[(header::HOST, "localhost:3345"), (header::COOKIE, &cookie), (header::ORIGIN, "https://localhost:8080")],
        );
        assert_eq!(admit(&m, &u, &h, T), Decision::RefuseOrigin);
        let (m, u, h) = request(
            Method::POST,
            "/api/stop",
            &[(header::HOST, "localhost:3345"), (header::COOKIE, &cookie), (header::ORIGIN, "https://localhost:3345")],
        );
        assert_eq!(admit(&m, &u, &h, T), Decision::Pass);
        // A read from another origin is CORS's to refuse, not the gate's.
        let (m, u, h) = request(
            Method::GET,
            "/api/status",
            &[(header::HOST, "localhost:3345"), (header::COOKIE, &cookie), (header::ORIGIN, "https://localhost:8080")],
        );
        assert_eq!(admit(&m, &u, &h, T), Decision::Pass);
        // No `Origin` at all (a host-side probe with the token): passes.
        let (m, u, h) = request(Method::POST, &format!("/api/stop?token={T}"), &[(header::HOST, "localhost:3345")]);
        assert_eq!(admit(&m, &u, &h, T), Decision::Pass);
        // HTTP/2 carries the host in the authority, not a `Host` header.
        let (m, u, h) = request(
            Method::POST,
            "https://localhost:3345/api/stop",
            &[(header::COOKIE, &cookie), (header::ORIGIN, "https://localhost:3345")],
        );
        assert_eq!(admit(&m, &u, &h, T), Decision::Pass);
    }

    /// A same-site page can set an `embra_token` cookie of its own on the
    /// host and the browser sends both: the matching one counts, so the
    /// bogus one is no denial of service; a lone wrong one is still refused.
    #[test]
    fn the_matching_cookie_is_taken_when_several_are_sent() {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, format!("embra_token=bogus; embra_token={T}").parse().unwrap());
        assert_eq!(cookie_token(&headers, T).as_deref(), Some(T));
        let mut headers = HeaderMap::new();
        headers.append(header::COOKIE, "embra_token=bogus".parse().unwrap());
        headers.append(header::COOKIE, format!("embra_token={T}").parse().unwrap());
        assert_eq!(cookie_token(&headers, T).as_deref(), Some(T));
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, "embra_token=bogus".parse().unwrap());
        assert_eq!(cookie_token(&headers, T).as_deref(), Some("bogus"));
    }

    #[test]
    fn the_token_from_the_environment_is_never_written_to_the_log() {
        let from_env = startup_line(true, 3345, T);
        assert!(!from_env.contains(T), "{from_env}");
        assert!(from_env.contains("serial console"));
        let minted = startup_line(false, 13345, T);
        assert_eq!(minted, format!("web console: https://localhost:13345/embraOS?token={T}"));
    }

    #[test]
    fn the_cookie_is_read_from_the_header_and_the_token_from_the_query() {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, format!("theme=dark; embra_token={T}; other=1").parse().unwrap());
        assert_eq!(cookie_token(&headers, T).as_deref(), Some(T));
        assert_eq!(cookie_token(&HeaderMap::new(), T), None);
        let uri: Uri = format!("/embraOS?x=1&token={T}").parse().unwrap();
        assert_eq!(query_token(&uri).as_deref(), Some(T));
        assert_eq!(query_token(&"/embraOS".parse().unwrap()), None);
    }
}
