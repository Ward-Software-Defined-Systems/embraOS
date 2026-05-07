//! embra-comp — single-fullscreen-client Wayland kiosk compositor for embraOS.
//!
//! Kiosk policy: the first client `xdg_toplevel` is mapped at origin and
//! kept; a second toplevel is sent `close` immediately. Designed to host
//! exactly one `embra-desktop` iced GUI client per boot.
//!
//! Two backends:
//! - `--winit` — nested compositor running as a Wayland/X11 client. Host-side
//!   dev iteration: `WAYLAND_DISPLAY=wayland-N weston-terminal` smokes
//!   protocol plumbing without booting the QEMU image.
//! - `--tty-udev` (default) — production boot path. Stage 3c — wires
//!   /dev/tty1 + /dev/dri/card0 + libinput via seatd.
//!
//! `--halt-reason <text>` (Stage 3c) renders the supplied text fullscreen
//! and refuses Wayland globals.

#![allow(irrefutable_let_patterns)]

mod handlers;
mod input;
mod state;
mod winit;

use anyhow::{Context, Result};
use clap::Parser;
use smithay::reexports::{calloop::EventLoop, wayland_server::Display};

pub use state::EmbraComp;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "embra-comp — single-fullscreen-client Wayland kiosk for embraOS"
)]
struct Args {
    /// Run as a nested compositor inside the host's Wayland/X11 session
    /// (host-side dev mode). Default is the `tty-udev` production backend.
    #[arg(long)]
    winit: bool,

    /// Render this halt reason fullscreen and refuse all Wayland clients.
    /// Used by embrad when soul invariant verification fails.
    #[arg(long, value_name = "TEXT")]
    halt_reason: Option<String>,

    /// Path to write the readiness sentinel file once Wayland globals are
    /// advertised. embrad's supervisor health-check watches this path.
    #[arg(long, default_value = "/run/embra-comp.ready")]
    ready_sentinel: String,
}

fn main() -> Result<()> {
    init_logging();

    let args = Args::parse();

    if let Some(reason) = args.halt_reason.as_deref() {
        tracing::warn!(reason, "halt mode — no clients will be accepted");
        anyhow::bail!("halt-mode rendering not implemented yet (Stage 3c)");
    }

    if args.winit {
        return run_winit(args.ready_sentinel);
    }

    tracing::info!(
        ready_sentinel = %args.ready_sentinel,
        "tty-udev backend not implemented yet (Stage 3c) — exiting"
    );
    anyhow::bail!("tty-udev backend not implemented yet (Stage 3c)");
}

fn run_winit(ready_sentinel: String) -> Result<()> {
    tracing::info!("starting embra-comp in winit (nested) mode");

    let mut event_loop: EventLoop<EmbraComp> =
        EventLoop::try_new().context("could not create calloop EventLoop")?;
    let display: Display<EmbraComp> = Display::new().context("could not create wl Display")?;

    let mut state = EmbraComp::new(&mut event_loop, display, ready_sentinel);

    winit::init_winit(&mut event_loop, &mut state)
        .map_err(|e| anyhow::anyhow!("winit init failed: {e}"))?;

    // Set WAYLAND_DISPLAY for any child the operator wants to spawn (e.g.
    // weston-terminal or embra-desktop) so they connect to us, not the
    // host compositor.
    unsafe { std::env::set_var("WAYLAND_DISPLAY", &state.socket_name) };
    tracing::info!(
        socket = ?state.socket_name,
        "wayland socket live — set WAYLAND_DISPLAY and run a client"
    );

    event_loop
        .run(None, &mut state, |_| {})
        .context("event loop terminated abnormally")?;
    Ok(())
}

fn init_logging() {
    let env = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(env).init();
}
