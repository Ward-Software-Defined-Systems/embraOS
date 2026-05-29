//! Force a `cargo` rebuild of `embra-web` whenever the embedded frontend
//! (`crates/embra-web-ui/dist`) changes.
//!
//! `rust-embed` bakes that `dist/` into the binary at COMPILE time for
//! release builds (`src/assets.rs` — `#[folder = "../embra-web-ui/dist"]`).
//! But there is no source dependency from this crate to those files, so a
//! plain `cargo build --release` after only the frontend changed (e.g.
//! `trunk build` rebuilt `dist/` but `embra-web/src/*` is untouched) can
//! decide this crate is up to date and SKIP recompiling it — leaving the
//! OLD WASM embedded in the binary the OS image ships. The browser then
//! serves stale bytes regardless of `Cache-Control`/browser cache.
//!
//! Emitting `rerun-if-changed` for every file under `dist/` (plus the
//! directories, so added/removed files are caught too) gives cargo the
//! missing dependency edge: any frontend change now invalidates this
//! crate and `rust-embed` re-embeds fresh. rust-embed's own rerun emission
//! is unreliable for release embeds, which is why this is done here.

use std::path::Path;

fn main() {
    // Resolved relative to CARGO_MANIFEST_DIR (crates/embra-web), matching
    // the `#[folder = "../embra-web-ui/dist"]` rust-embed path.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR is always set by cargo");
    let dist = Path::new(&manifest_dir).join("../embra-web-ui/dist");

    // Watch the root even when it doesn't exist yet, so first creation
    // (e.g. the initial `trunk build`) triggers a rebuild.
    println!("cargo:rerun-if-changed={}", dist.display());

    watch_recursively(&dist);
}

/// Emit `rerun-if-changed` for every entry under `dir` (files and
/// subdirectories). Silently no-ops if the tree is missing or unreadable —
/// a build script must never fail the build just because `dist/` is absent
/// (debug builds read it live; a release build without it is a separate,
/// pre-existing error surfaced by rust-embed itself).
fn watch_recursively(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        println!("cargo:rerun-if-changed={}", path.display());
        if path.is_dir() {
            watch_recursively(&path);
        }
    }
}
