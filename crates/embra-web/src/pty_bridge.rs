//! Shared PTY bridge: one PTY + one `embra-console` child for the whole
//! `embra-web` process lifetime.
//!
//! The brain is single-conversation by construction, so there is exactly
//! one console process regardless of how many browsers connect. All
//! connections share this bridge: PTY output is broadcast to every client;
//! input is funnelled through a single channel that the arbiter only feeds
//! from the current *writer* client.
//!
//! portable-pty's reader/writer/child are blocking std types, so the
//! session is driven by a dedicated OS thread with a short poll loop
//! (cheap, and well under the console's own 200 ms redraw cadence). This
//! also makes console restart trivial: the whole session is rebuilt at the
//! top of the outer loop while the public channels persist, so connected
//! WebSocket clients survive a console crash.
//!
//! Fresh-attach repaint contract. A browser that (re)loads the page starts
//! with an EMPTY xterm, and the console only ever writes diffs (ratatui
//! re-diffs every ~200 ms but emits changed cells only — an idle screen
//! sends nothing). Nothing is replayed to a new subscriber, so without
//! help the new tab stays blank until something on screen changes. Nor
//! does the client's own resize frame help: same window → same cols/rows
//! → the TIOCSWINSZ is a kernel no-op (`tty_do_resize` memcmp's the
//! winsize and sends no SIGWINCH). So `/ws/terminal` calls
//! [`PtyBridge::repaint`] on every attach and the pump thread sends the
//! console child a bare SIGWINCH; crossterm turns it into
//! `Event::Resize`, which the web-pty console answers with `clear()` + a
//! full redraw (`embra-console/src/terminal/mod.rs`, the Resize arm). A
//! full repaint is sufficient: no terminal MODE needs replaying — the
//! console's bracketed-paste enable is sent once, but the /ml editor
//! wraps its paste itself and crossterm parses `ESC[200~` regardless;
//! cursor visibility is re-emitted per draw; no alt-screen, no mouse.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::{broadcast, mpsc};

/// Handle shared across the axum app (Clone, Send + Sync).
#[derive(Clone)]
pub struct PtyBridge {
    output_tx: broadcast::Sender<Bytes>,
    input_tx: mpsc::UnboundedSender<Vec<u8>>,
    resize_tx: mpsc::UnboundedSender<(u16, u16, u16, u16)>,
    /// Set by [`PtyBridge::repaint`], consumed (coalesced) by the pump
    /// thread — see the module doc.
    repaint: Arc<AtomicBool>,
}

/// Minimum spacing between two console repaints. Requests inside the
/// window are DEFERRED (the flag stays set), never dropped: a flapping
/// tab reconnects every 2 s, and a client looping on `/ws/terminal`
/// must not turn into a full-screen repaint for every client per tick.
const REPAINT_COOLDOWN: Duration = Duration::from_millis(250);

impl PtyBridge {
    /// Spawn the PTY session manager thread and return a shared handle.
    pub fn spawn(console_bin: String, apid_addr: String) -> Self {
        // Capacity large enough that a briefly-slow xterm.js client doesn't
        // lag out during a full-screen repaint. On Lagged the WS handler
        // just continues and the gap is NOT healed — the console re-diffs
        // every ~200 ms but only emits changed cells, so an idle screen
        // sends nothing. (Wiring `repaint()` into the Lagged arm is a
        // deliberate non-goal: a chronically slow client would loop
        // lag → repaint → lag; a VT snapshot replay is the real answer.)
        let (output_tx, _) = broadcast::channel::<Bytes>(2048);
        let (input_tx, input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, resize_rx) = mpsc::unbounded_channel::<(u16, u16, u16, u16)>();
        let repaint = Arc::new(AtomicBool::new(false));

        let bridge = PtyBridge {
            output_tx: output_tx.clone(),
            input_tx,
            resize_tx,
            repaint: repaint.clone(),
        };

        std::thread::Builder::new()
            .name("embra-web-pty".into())
            .spawn(move || {
                session_manager(console_bin, apid_addr, output_tx, input_rx, resize_rx, repaint)
            })
            .expect("spawn pty session manager thread");

        bridge
    }

    /// Subscribe to the PTY output stream (one receiver per WS client).
    pub fn subscribe(&self) -> broadcast::Receiver<Bytes> {
        self.output_tx.subscribe()
    }

    /// Write input bytes to the PTY. The arbiter only calls this for the
    /// current writer client; non-writer frames never reach here.
    pub fn write_input(&self, data: Vec<u8>) {
        let _ = self.input_tx.send(data);
    }

    /// Request a PTY winsize change (cols, rows).
    /// `xpixel`/`ypixel` = screen pixel size (0 = unknown); lands in the
    /// PTY winsize so the console can derive its cell size for sixel.
    pub fn resize(&self, cols: u16, rows: u16, xpixel: u16, ypixel: u16) {
        let _ = self.resize_tx.send((cols, rows, xpixel, ypixel));
    }

    /// Ask the console for a full-screen repaint (the fresh-attach
    /// contract in the module doc). Coalesced per pump tick, spaced by
    /// `REPAINT_COOLDOWN`, delivered as SIGWINCH to the console child.
    pub fn repaint(&self) {
        self.repaint.store(true, Ordering::SeqCst);
    }
}

/// Outer loop: (re)build the PTY + console child for the process lifetime.
fn session_manager(
    console_bin: String,
    apid_addr: String,
    output_tx: broadcast::Sender<Bytes>,
    mut input_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    mut resize_rx: mpsc::UnboundedReceiver<(u16, u16, u16, u16)>,
    repaint: Arc<AtomicBool>,
) {
    // Last requested size, so a restart reopens at the operator's size.
    let mut last_size = PtySize::default();

    loop {
        match run_one_session(
            &console_bin,
            &apid_addr,
            &output_tx,
            &mut input_rx,
            &mut resize_rx,
            &mut last_size,
            &repaint,
        ) {
            Ok(()) => {
                tracing::warn!("embra-console exited; restarting in 1s");
            }
            Err(e) => {
                tracing::error!(error = %e, "PTY session error; restarting in 1s");
            }
        }
        let _ = output_tx.send(Bytes::from_static(
            b"\r\n\x1b[2m[embra-web] embra-console exited \xe2\x80\x94 restarting\xe2\x80\xa6\x1b[0m\r\n",
        ));
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// One console lifetime: open PTY, spawn child, pump until it exits.
fn run_one_session(
    console_bin: &str,
    apid_addr: &str,
    output_tx: &broadcast::Sender<Bytes>,
    input_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    resize_rx: &mut mpsc::UnboundedReceiver<(u16, u16, u16, u16)>,
    last_size: &mut PtySize,
    repaint: &AtomicBool,
) -> anyhow::Result<()> {
    let pair = native_pty_system().openpty(*last_size)?;

    let mut cmd = CommandBuilder::new(console_bin);
    cmd.arg("--apid-addr");
    cmd.arg(apid_addr);
    cmd.env("EMBRA_WEB_PTY", "1");
    // Media wave: the console's in-TUI media pane renders sixel on this
    // PTY — xterm.js paints it via the vendored @xterm/addon-image. Cell
    // geometry reaches the console through the PTY winsize pixel fields
    // (the browser's resize frames carry xpixel/ypixel), not a stdin
    // query — the console starts before any browser is attached. The
    // serial console never gets this env (its default is halfblocks).
    cmd.env("EMBRA_TUI_GRAPHICS", "sixel");
    cmd.env("TERM", "xterm-256color");

    // Spawn on the slave, then drop our slave handle so the master read
    // EOFs when the child exits (otherwise it would block forever).
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    let mut writer = pair.master.take_writer()?;
    let mut reader = pair.master.try_clone_reader()?;
    let master = pair.master;

    // Reader thread: blocking read → broadcast. Sets `reader_done` on
    // EOF/error so the pump loop can tear the session down.
    let reader_done = Arc::new(AtomicBool::new(false));
    {
        let reader_done = reader_done.clone();
        let output_tx = output_tx.clone();
        std::thread::Builder::new()
            .name("embra-web-pty-read".into())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            // Err only means "no subscribers yet" — ignore.
                            let _ = output_tx.send(Bytes::copy_from_slice(&buf[..n]));
                        }
                    }
                }
                reader_done.store(true, Ordering::SeqCst);
            })?;
    }

    // Pump loop: drain input/resize/repaint, watch for child exit. The
    // 15 ms tick is imperceptible for a TUI (its own event poll is 50 ms,
    // re-diff 200 ms) and avoids juggling three blocking sources.
    let mut last_repaint: Option<Instant> = None;
    loop {
        while let Ok(data) = input_rx.try_recv() {
            if writer.write_all(&data).is_err() {
                break;
            }
            let _ = writer.flush();
        }

        let mut resized = false;
        while let Ok((cols, rows, xpixel, ypixel)) = resize_rx.try_recv() {
            last_size.cols = cols;
            last_size.rows = rows;
            last_size.pixel_width = xpixel;
            last_size.pixel_height = ypixel;
            resized = true;
        }
        if resized {
            let _ = master.resize(*last_size);
        }

        // Fresh-attach repaint (module doc): a same-size TIOCSWINSZ above
        // is a kernel no-op, so signal the console directly. AFTER the
        // resize block so a same-tick resize lands first; the cooldown
        // defers rather than drops.
        if repaint.load(Ordering::SeqCst)
            && last_repaint.is_none_or(|t| t.elapsed() >= REPAINT_COOLDOWN)
        {
            repaint.store(false, Ordering::SeqCst);
            last_repaint = Some(Instant::now());
            if let Some(pid) = child.process_id() {
                // The child is reaped only by try_wait()/wait() below (a
                // `Some` returns before the next iteration), so the pid is
                // never stale here; ESRCH would be ignored anyway.
                // SAFETY: kill(2) on a pid this thread owns, with a signal
                // whose default disposition is "ignore".
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGWINCH);
                }
            }
        }

        if child.try_wait()?.is_some() {
            return Ok(());
        }
        if reader_done.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }

        std::thread::sleep(Duration::from_millis(15));
    }
}
