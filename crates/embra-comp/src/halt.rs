//! Halt-reason rendering — soul-mismatch UX.
//!
//! When `embrad` boots and trustd reports a soul invariant mismatch, the
//! supervisor launches `embra-comp --halt-reason "<reason>"` instead of
//! the GUI client. This replaces the prior "black screen on graphical
//! boot" hazard with a visible operator-facing failure.
//!
//! Stage 3c-initial implementation: writes the halt reason to stderr (so
//! the host's `-serial file:/tmp/embra-serial.log` captures it) and
//! parks. A fully-rendered fullscreen halt screen on the framebuffer
//! lands as a follow-up — the rendering path is the same DRM/EGL/GLES
//! pipeline that the production tty-udev backend needs, so the two are
//! best landed together.

use std::time::Duration;

pub fn run(reason: &str) -> ! {
    eprintln!("");
    eprintln!("====================================================");
    eprintln!("  embraOS HALTED — soul invariant verification failed");
    eprintln!("====================================================");
    eprintln!("");
    eprintln!("  Reason: {reason}");
    eprintln!("");
    eprintln!("  The intelligence's identity has changed since the last");
    eprintln!("  boot. embraOS will not proceed without operator review.");
    eprintln!("");
    eprintln!("  Power-cycle the machine after addressing the mismatch.");
    eprintln!("====================================================");
    eprintln!("");

    tracing::error!(reason, "halt mode active — blocking forever");

    // Park indefinitely. embrad's supervisor watches the process; if it
    // exits, supervisor restarts it. We park rather than exit so the
    // halt screen persists until power-cycle.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
