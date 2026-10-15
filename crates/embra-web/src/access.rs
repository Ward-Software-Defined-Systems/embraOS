//! The web console's gate: a per-boot access token. embrad mints one and
//! hands it over in `EMBRA_WEB_TOKEN`; the URL it prints at boot carries it
//! (`https://localhost:3345/embraOS?token=…`). The first visit with the
//! token sets a cookie and redirects to the clean URL; every later request,
//! WebSocket handshakes included, is let through on the cookie; anything
//! else on `:3345` gets 401. The token is 128 bits as 32 lowercase hex
//! characters, compared over its whole length whatever matches. The
//! arbiter keeps its role: who types, among those who hold the token.
//!
//! On the host, without embrad, embra-web mints its own token and prints
//! the URL once to its stdout; in the OS the token is never logged — the
//! boot banner is the one place it is shown.

use std::io::Read;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

/// The environment variable embrad sets for embra-web.
pub const TOKEN_ENV: &str = "EMBRA_WEB_TOKEN";
/// The cookie the first visit sets.
pub const COOKIE: &str = "embra_token";
/// What a request without the token is told. Never the token.
pub const REFUSAL: &str = "embraOS web console: open the URL printed on the serial console at boot; it carries this boot's access token.\n";

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

/// `embra_token=…` from the `Cookie` headers.
fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers.get_all(header::COOKIE).iter().find_map(|value| {
        value
            .to_str()
            .ok()?
            .split(';')
            .map(str::trim)
            .find_map(|c| c.strip_prefix(&format!("{COOKIE}=")).map(str::to_string))
    })
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

/// The layer over every route and the fallback.
pub async fn require_token(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let uri = req.uri().clone();
    let decision = decide(
        uri.path(),
        uri.query(),
        query_token(&uri).as_deref(),
        cookie_token(req.headers()).as_deref(),
        &st.token,
    );
    match decision {
        Decision::Pass => next.run(req).await,
        Decision::SetCookieAndRedirect { location, set_cookie } => Response::builder()
            .status(StatusCode::SEE_OTHER)
            .header(header::LOCATION, location)
            .header(header::SET_COOKIE, set_cookie)
            .header(header::CACHE_CONTROL, "no-store")
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Decision::Refuse => (
            StatusCode::UNAUTHORIZED,
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            REFUSAL,
        )
            .into_response(),
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
        assert_eq!(cookie_token(&headers).as_deref(), Some(T));
        assert_eq!(cookie_token(&HeaderMap::new()), None);
        let uri: Uri = format!("/embraOS?x=1&token={T}").parse().unwrap();
        assert_eq!(query_token(&uri).as_deref(), Some(T));
        assert_eq!(query_token(&"/embraOS".parse().unwrap()), None);
    }
}
