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

/// Router fallback: serve a real embedded asset, else SPA-fallback to
/// `index.html` (so `/embraOS` and client routing resolve).
pub async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(resp) = serve(path) {
        return resp;
    }
    if let Some(resp) = serve("index.html") {
        return resp;
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}
