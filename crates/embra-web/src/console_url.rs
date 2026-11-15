//! `GET /api/console-url`: the console's URLs with this boot's token, for
//! the display console's menu.
//!
//! The boot banner prints the URLs once, on the serial line and on the
//! display, and the display console then covers its copy: an operator at
//! the machine's own screen has no way back to them, and a browser on
//! another machine cannot be pointed at the console without them. This
//! route gives them to the display console's own browser and to nobody
//! else. Its caller has passed the token gate like every other (`access.rs`),
//! so it holds the token it is shown; and its connection comes from this
//! machine itself, a loopback peer, as cog's does (`https://127.0.0.1:3345`).
//! A browser elsewhere is refused: it has the URL it came by, and a script
//! on its page gets no way to read the token the HttpOnly cookie keeps from
//! it. Nothing here is logged.
//!
//! The URLs are the banner's (`embra_boot::net_lease::console_urls`), read
//! from the cards' records as they stand at the request: a lease that came
//! after the banner was printed is in the menu all the same.

use std::net::IpAddr;

use axum::Json;
use axum::extract::{ConnectInfo, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use embra_boot::net_lease::{self, Lease, Source};
use serde_json::{Value, json};

use crate::server::Peer;
use crate::state::AppState;

/// What a caller that is not on this machine is told. Never the token.
pub const ELSEWHERE: &str = "embraOS web console: the console's URL is shown on the machine's own display only.\n";

/// Whether a connection came from this machine itself: a loopback address,
/// an IPv4 one mapped into IPv6 included.
fn on_this_machine(peer: IpAddr) -> bool {
    peer.to_canonical().is_loopback()
}

/// The banner's URLs, each with where it works, in the banner's order.
fn named_urls(token: &str, leases: &[Lease]) -> Value {
    let rows: Vec<Value> = net_lease::console_urls(token, leases)
        .iter()
        .map(|u| {
            let place = match u.lease {
                None => "from the host of a QEMU VM, through its port forward".to_string(),
                Some(l) if l.source == Source::Static => format!("on the network, at the address set for {} ({}/{})", l.interface, l.ip, l.prefix),
                Some(l) => format!("on the network, at the address DHCP leased to this machine ({}/{})", l.ip, l.prefix),
            };
            json!({ "url": u.url, "where": place })
        })
        .collect();
    json!({ "urls": rows })
}

pub async fn api_console_url(State(st): State<AppState>, ConnectInfo(Peer(peer)): ConnectInfo<Peer>) -> Response {
    if !on_this_machine(peer.ip()) {
        return (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8"), (header::CACHE_CONTROL, "no-store")],
            ELSEWHERE,
        )
            .into_response();
    }
    ([(header::CACHE_CONTROL, "no-store")], Json(named_urls(&st.token, &net_lease::read()))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    const T: &str = "0123456789abcdef0123456789abcdef";

    async fn ask(peer: &str) -> (StatusCode, String, String) {
        let (state, _input) = AppState::stub();
        let peer: SocketAddr = peer.parse().unwrap();
        let resp = api_console_url(State(state), ConnectInfo(Peer(peer))).await;
        let status = resp.status();
        let cache = resp.headers().get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let body = axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        (status, cache, String::from_utf8(body.to_vec()).unwrap())
    }

    /// The display console's own browser, a loopback peer, is shown the
    /// URL with the token; nothing of it is cached.
    #[tokio::test]
    async fn the_machines_own_browser_is_shown_the_url_with_the_token() {
        for peer in ["127.0.0.1:41000", "[::1]:41000", "[::ffff:127.0.0.1]:41000"] {
            let (status, cache, body) = ask(peer).await;
            assert_eq!(status, StatusCode::OK, "{peer}");
            assert_eq!(cache, "no-store", "{peer}");
            let v: Value = serde_json::from_str(&body).unwrap();
            let first = v["urls"][0]["url"].as_str().unwrap();
            assert_eq!(first, format!("https://localhost:3345/embraOS?token={T}"), "{peer}");
        }
    }

    /// A caller elsewhere holds the token already (it passed the gate) and
    /// is told nothing more: no token in the answer, whatever its address.
    #[tokio::test]
    async fn a_browser_elsewhere_is_refused_and_told_no_token() {
        for peer in ["10.0.2.2:50112", "192.168.1.20:50112", "[2001:db8::7]:50112", "[::ffff:10.0.2.2]:50112"] {
            let (status, cache, body) = ask(peer).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{peer}");
            assert_eq!(cache, "no-store", "{peer}");
            assert_eq!(body, ELSEWHERE, "{peer}");
            assert!(!body.contains(T), "{peer}");
        }
    }

    /// The rows are the banner's URLs in the banner's order: QEMU's port
    /// forward always, the leased address when the machine has one of its
    /// own.
    #[test]
    fn the_urls_are_the_banners_each_with_where_it_works() {
        // No address on a network: the forwarded one alone.
        let alone = named_urls(T, &[]);
        assert_eq!(alone["urls"].as_array().unwrap().len(), 1);
        assert_eq!(alone["urls"][0]["url"], format!("https://localhost:3345/embraOS?token={T}"));
        assert!(alone["urls"][0]["where"].as_str().unwrap().contains("port forward"));

        let leases = [
            net_lease::parse("interface=eth0\nip=10.20.0.5\nmask=24\nmetric=102\nsource=static\n").unwrap(),
            net_lease::parse("interface=eth1\nip=192.168.1.42\nmask=24\nrouter=192.168.1.1\ndns=192.168.1.1\nmetric=103\nsource=dhcp\n").unwrap(),
        ];
        let rows = named_urls(T, &leases);
        let urls: Vec<&str> = rows["urls"].as_array().unwrap().iter().map(|r| r["url"].as_str().unwrap()).collect();
        assert_eq!(urls, net_lease::console_urls(T, &leases).iter().map(|u| u.url.as_str()).collect::<Vec<_>>());
        assert_eq!(urls.len(), 3, "a row for every address");
        assert_eq!(rows["urls"][1]["where"], "on the network, at the address set for eth0 (10.20.0.5/24)");
        assert_eq!(rows["urls"][2]["where"], "on the network, at the address DHCP leased to this machine (192.168.1.42/24)");
    }
}
