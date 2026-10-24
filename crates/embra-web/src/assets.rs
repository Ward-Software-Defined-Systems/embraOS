//! Embedded frontend (Leptos/WASM `dist/`) + SPA fallback.
//!
//! `rust-embed` compiles the built `crates/embra-web-ui/dist` into the
//! binary, so the OS image build stays a pure cargo + Buildroot flow —
//! no Node or web assets in the rootfs. Release musl builds embed; host
//! debug builds read the folder live (rust-embed default), which is fine
//! for iteration.

use axum::body::Body;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

// rust-embed resolves `folder` relative to CARGO_MANIFEST_DIR
// (crates/embra-web), so this points at crates/embra-web-ui/dist.
#[derive(RustEmbed)]
#[folder = "../embra-web-ui/dist"]
struct Assets;

/// The console is never framed by another page: a click on the real console
/// inside an attacker's page is a click on the console (#30). The CSP
/// directive is the current rule; `X-Frame-Options` is the one older
/// browsers read. Every embedded asset carries both.
pub(crate) const FRAME_ANCESTORS_NONE: &str = "frame-ancestors 'none'";

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

fn serve(path: &str) -> Option<Response> {
    let file = Assets::get(path)?;
    Some(
        (
            [
                (header::CONTENT_TYPE, content_type(path)),
                // Bust browser caches between OS image rebuilds. The
                // index.html → SRI → wasm chain reloads correctly when
                // the HTML itself is fresh, but mobile Safari and
                // others will happily serve a cached HTML (and thus a
                // stale SRI → stale wasm) across iterations. `no-store`
                // is heavier than `no-cache` w/ revalidation, but the
                // embra-web bundle is small (<500 KB) and served over
                // LAN, so the bandwidth cost is negligible compared to
                // the time spent debugging stale-cache mysteries.
                (header::CACHE_CONTROL, "no-store"),
                (header::CONTENT_SECURITY_POLICY, FRAME_ANCESTORS_NONE),
                (header::X_FRAME_OPTIONS, "DENY"),
            ],
            Body::from(file.data),
        )
            .into_response(),
    )
}

/// Where a path goes. The app lives at `/embraOS` alone (William's
/// choice, 2026-10-15): `/` and the app's file name answer 404, so the
/// console has one address, the one the boot banner prints. The assets
/// keep their root paths (`/assets/…`, the Trunk-emitted `.js`/`.wasm`),
/// which `index.html` references absolutely.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    /// `index.html`.
    App,
    /// An embedded file, if one exists at the path.
    Lookup,
    NotFound,
}

pub fn route(path: &str) -> Route {
    match path {
        "embraOS" => Route::App,
        p if p.starts_with("embraOS/") => Route::App,
        "" | "index.html" => Route::NotFound,
        _ => Route::Lookup,
    }
}

/// Router fallback: the app under `/embraOS`, a real embedded asset at
/// its own path, 404 for everything else.
pub async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let served = match route(path) {
        Route::App => serve("index.html"),
        Route::Lookup => serve(path),
        Route::NotFound => None,
    };
    served.unwrap_or_else(|| (StatusCode::NOT_FOUND, "not found").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn get(path: &'static str) -> (StatusCode, header::HeaderMap, Vec<u8>) {
        let resp = static_handler(Uri::from_static(path)).await;
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec();
        (status, headers, body)
    }

    /// The handler over the embedded `dist/`: the app page at `/embraOS` and
    /// under it, with the headers that keep it fresh and out of a frame; a
    /// path that climbs out of the folder, encoded or not, finds nothing;
    /// an unknown path is 404, never the app (#30, #54).
    #[tokio::test]
    async fn the_app_page_is_fresh_and_unframed_and_a_path_out_of_the_folder_finds_nothing() {
        let (status, headers, body) = get("/embraOS").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(headers[header::CONTENT_SECURITY_POLICY], "frame-ancestors 'none'");
        assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
        assert!(std::str::from_utf8(&body).unwrap().contains("<html"), "the app page");
        let (status, _, under) = get("/embraOS/sessions").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(under, body, "every path under the app is the one page");

        for climb in [
            "/../../etc/passwd",
            "/%2e%2e/%2e%2e/etc/passwd",
            "/assets/../../Cargo.toml",
            "/embra-web-ui_bg.wasm/../../Cargo.toml",
        ] {
            let (status, _, body) = get(climb).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{climb}");
            assert_eq!(body, b"not found", "{climb}");
        }
        let (status, _, _) = get("/nonexistent/route").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "an unknown path is not the app");
        let (status, _, _) = get("/").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "nor is the root");
    }

    #[test]
    fn the_content_type_follows_the_extension_and_the_rest_is_a_download() {
        for (path, ct) in [
            ("index.html", "text/html; charset=utf-8"),
            ("embra-web-ui.js", "text/javascript; charset=utf-8"),
            ("embra-web-ui_bg.wasm", "application/wasm"),
            ("assets/app.css", "text/css; charset=utf-8"),
            ("manifest.json", "application/json"),
            ("assets/logo.svg", "image/svg+xml"),
            ("assets/icon.png", "image/png"),
            ("favicon.ico", "image/x-icon"),
            ("README", "application/octet-stream"),
            ("archive.tar.gz", "application/octet-stream"),
        ] {
            assert_eq!(content_type(path), ct, "{path}");
        }
    }

    #[test]
    fn the_app_is_served_under_embraos_and_the_assets_at_their_root_paths() {
        assert_eq!(route("embraOS"), Route::App);
        assert_eq!(route("embraOS/"), Route::App);
        assert_eq!(route("embraOS/anything"), Route::App);
        assert_eq!(route(""), Route::NotFound, "the root is not the app");
        assert_eq!(route("index.html"), Route::NotFound, "nor is the app's file name");
        assert_eq!(route("assets/app.css"), Route::Lookup);
        assert_eq!(route("embra-web-ui_bg.wasm"), Route::Lookup);
        assert_eq!(route("nope"), Route::Lookup, "an unknown path is looked up and found missing");
    }
}
