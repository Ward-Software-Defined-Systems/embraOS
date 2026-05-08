//! Production tty-udev backend driver — DRM scanout + libinput input.
//!
//! Stage 3c **structural scaffold**. This module establishes:
//! - libseat session open (`session::libseat::LibSeatSession`)
//! - udev event source (`backend::udev::UdevBackend`)
//! - primary DRM card discovery
//! - structural readiness for DRM/EGL/GLES initialization
//! - readiness sentinel write + park-forever loop
//!
//! **Deferred to Stage 3c-final** (best done in a focused session against
//! the QEMU image, not host-blind):
//! - `DrmDevice` + `GbmDevice` + `EGLDisplay` + `EGLContext` setup
//! - Connector / CRTC scanning, mode selection
//! - `DrmCompositor` or `GbmBufferedSurface` render path
//! - Wayland `Output` registration tied to the chosen connector
//! - libinput device routing through `EmbraComp::process_input_event`
//! - VBlank-driven render loop with frame submission + page-flip
//!
//! The deferred pipeline is ~400-600 LOC modeled on smithay's
//! `anvil/src/udev.rs` (1500+ LOC) but trimmed for the kiosk profile:
//! single GPU (no MultiRenderer / GpuManager), single connected
//! connector, no dmabuf / drm-lease / drm-syncobj features. Until that
//! lands, calling `--tty-udev` (default) bails with a clear message
//! pointing at `--winit` for nested dev iteration.

use std::path::PathBuf;

use anyhow::{Context, Result};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::Session;
use smithay::backend::udev::{primary_gpu, UdevBackend};

pub fn run(_ready_sentinel: String) -> Result<()> {
    tracing::info!("tty-udev backend: opening libseat session");
    let (session, _notifier) =
        LibSeatSession::new().context("failed to open libseat session")?;
    let seat_name = session.seat();
    tracing::info!(seat = %seat_name, "libseat session opened");

    tracing::info!("tty-udev backend: starting udev backend");
    let _udev = UdevBackend::new(&seat_name)
        .context("failed to start udev backend")?;

    let primary = primary_gpu(&seat_name)
        .context("primary_gpu lookup failed")?
        .or_else(|| first_card_path())
        .context("no DRM device found via udev or /dev/dri/card0")?;
    tracing::info!(primary_gpu = %primary.display(), "found DRM device");

    // === Deferred Stage 3c-final implementation ===
    //
    // 1. session.open(&primary, OFlags::RDWR | OFlags::CLOEXEC | OFlags::NONBLOCK)?
    // 2. DrmDevice::new(DrmDeviceFd::new(...))
    // 3. GbmDevice::new(drm_device)
    // 4. EGLDisplay::new(gbm) → EGLContext::new_with_priority
    // 5. GlesRenderer::new(context)
    // 6. Scan connectors via DrmScanner → find Connected one with preferred Mode
    // 7. Allocate GbmAllocator + DrmCompositor for the chosen CRTC
    // 8. Output::new(...) registered as wayland output, mapped into space
    // 9. Insert UdevBackend + DrmDevice + libinput into calloop event loop
    // 10. On VBlank: render space → submit → page-flip
    // 11. write_ready_sentinel() after first successful frame
    //
    // See smithay/anvil/src/udev.rs:run_udev (lines 217-540) for the
    // multi-gpu pattern; kiosk variant skips MultiRenderer/GpuManager
    // and uses GlesRenderer directly against the primary GPU.

    anyhow::bail!(
        "tty-udev DRM scanout pipeline not yet implemented (Stage 3c-final). \
         For host-side dev iteration use `--winit`."
    )
}

fn first_card_path() -> Option<PathBuf> {
    for n in 0..8 {
        let p = PathBuf::from(format!("/dev/dri/card{n}"));
        if p.exists() {
            return Some(p);
        }
    }
    None
}
