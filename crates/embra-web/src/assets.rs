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
    use super::{Route, route};

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
