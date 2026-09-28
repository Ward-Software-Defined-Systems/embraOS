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
//! Restart pacing. The console exits when it cannot do its work (no
//! gateway, a screen that cannot be set up), and it is started again here.
//! A console that keeps exiting early is started again more and more
//! slowly — [`restart_delay`] — so that a failure that does not go away
//! costs one attempt every 30 s, not one a second. There is no budget: a
//! browser may attach at any time, and the gateway may come back.
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
use portable_pty::{CommandBuilder, ExitStatus, PtySize, native_pty_system};
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

/// Restart pacing — embrad's numbers for the services it supervises.
const RESTART_BASE: Duration = Duration::from_secs(1);
const RESTART_MAX: Duration = Duration::from_secs(30);
/// A console that ran this long was working: whatever ended it, the next
/// one is started after `RESTART_BASE` again. Must stay above `RESTART_MAX`,
/// or a crash loop pacing itself at the cap would reset its own delay.
const STABLE_AFTER: Duration = Duration::from_secs(60);

/// How many consoles in a row have ended early, this one included.
fn early_exits(before: u32, ran_for: Duration) -> u32 {
    if ran_for >= STABLE_AFTER { 0 } else { before.saturating_add(1) }
}

/// The wait before the next console: 1 s, 1 s, 2 s, 4 s, … 30 s.
fn restart_delay(early_exits: u32) -> Duration {
    let doublings = early_exits.saturating_sub(1).min(16);
    (RESTART_BASE * 2u32.pow(doublings)).min(RESTART_MAX)
}

/// "code 2", "killed by Terminated" — for the log and for the operator.
fn describe(status: &ExitStatus) -> String {
    match status.signal() {
        Some(signal) => format!("killed by {signal}"),
        None => format!("code {}", status.exit_code()),
    }
}

/// What an attached browser is shown between two consoles.
fn restart_banner(how: &str, delay: Duration) -> String {
    format!(
        "\r\n\x1b[2m[embra-web] embra-console exited ({how}) \u{2014} restarting in {} s\u{2026}\x1b[0m\r\n",
        delay.as_secs()
    )
}

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
    let mut early = 0;

    loop {
        let started = Instant::now();
        let outcome = run_one_session(
            console_command(&console_bin, &apid_addr),
            &output_tx,
            &mut input_rx,
            &mut resize_rx,
            &mut last_size,
            &repaint,
        );
        let ran_for = started.elapsed();
        early = early_exits(early, ran_for);
        let delay = restart_delay(early);
        let how = match outcome {
            Ok(status) => {
                let how = describe(&status);
                tracing::warn!(
                    exit = %how,
                    ran_for_secs = ran_for.as_secs(),
                    early_exits = early,
                    restart_in_secs = delay.as_secs(),
                    "embra-console exited"
                );
                how
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    early_exits = early,
                    restart_in_secs = delay.as_secs(),
                    "PTY session error"
                );
                "PTY session error".to_string()
            }
        };
        let _ = output_tx.send(Bytes::from(restart_banner(&how, delay)));
        std::thread::sleep(delay);
    }
}

/// The console child's command line and environment.
fn console_command(console_bin: &str, apid_addr: &str) -> CommandBuilder {
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
    cmd
}

/// One console lifetime: open PTY, spawn child, pump until it exits.
/// Returns how it ended.
fn run_one_session(
    cmd: CommandBuilder,
    output_tx: &broadcast::Sender<Bytes>,
    input_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    resize_rx: &mut mpsc::UnboundedReceiver<(u16, u16, u16, u16)>,
    last_size: &mut PtySize,
    repaint: &AtomicBool,
) -> anyhow::Result<ExitStatus> {
    let pair = native_pty_system().openpty(*last_size)?;

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

        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if reader_done.load(Ordering::SeqCst) {
            let _ = child.kill();
            return Ok(child.wait()?);
        }

        std::thread::sleep(Duration::from_millis(15));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    /// Marks the re-executed test binary as the stand-in console.
    const CHILD_ENV: &str = "EMBRA_PTY_FAKE_CONSOLE";
    const DEADLINE: Duration = Duration::from_secs(20);

    static WINCH: AtomicU32 = AtomicU32::new(0);

    extern "C" fn on_winch(_: libc::c_int) {
        WINCH.fetch_add(1, Ordering::SeqCst);
    }

    /// `<tag> <cols>x<rows> <xpixel>x<ypixel>` for the terminal on stdout.
    fn report(tag: &str) {
        // SAFETY: TIOCGWINSZ fills a plain C struct.
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
        println!("{tag} {}x{} {}x{}", ws.ws_col, ws.ws_row, ws.ws_xpixel, ws.ws_ypixel);
        let _ = std::io::stdout().flush();
    }

    /// Not a test of its own: the stand-in console `session_round_trip`
    /// runs on its PTY by re-executing this test binary. It reports the
    /// winsize at start and on every SIGWINCH, and exits on a `quit` line.
    #[test]
    #[ignore = "helper process for session_round_trip"]
    fn fake_console_child() {
        if std::env::var_os(CHILD_ENV).is_none() {
            return;
        }
        // SAFETY: the handler only bumps an atomic.
        unsafe {
            libc::signal(libc::SIGWINCH, on_winch as extern "C" fn(libc::c_int) as libc::sighandler_t);
        }
        report("ready");
        std::thread::spawn(|| {
            let mut seen = 0;
            loop {
                let n = WINCH.load(Ordering::SeqCst);
                if n != seen {
                    seen = n;
                    report(&format!("winch {n}"));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let mut line = String::new();
        loop {
            line.clear();
            match std::io::stdin().read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) if line.trim() == "quit" => {
                    println!("bye");
                    break;
                }
                // What the console does when it cannot do its work.
                Ok(_) if line.trim() == "fail" => {
                    println!("cannot go on");
                    std::process::exit(2);
                }
                Ok(_) => {}
            }
        }
    }

    /// Drain the broadcast into `seen` until `needle` shows up.
    fn expect(rx: &mut broadcast::Receiver<Bytes>, seen: &mut String, needle: &str) {
        let start = Instant::now();
        while !seen.contains(needle) {
            match rx.try_recv() {
                Ok(chunk) => seen.push_str(&String::from_utf8_lossy(&chunk)),
                Err(_) => {
                    assert!(
                        start.elapsed() < DEADLINE,
                        "no {needle:?} within {DEADLINE:?}; PTY output so far:\n{seen}"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    #[test]
    fn console_command_carries_the_web_pty_environment() {
        let cmd = console_command("/usr/bin/embra-console", "http://127.0.0.1:50000");
        let argv: Vec<_> = cmd.get_argv().iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(argv, ["/usr/bin/embra-console", "--apid-addr", "http://127.0.0.1:50000"]);
        let env = |k: &str| cmd.get_env(k).map(|v| v.to_string_lossy().into_owned());
        // The console's PTY-only behavior, its sixel pane and its color
        // depth all hang off these three.
        assert_eq!(env("EMBRA_WEB_PTY").as_deref(), Some("1"));
        assert_eq!(env("EMBRA_TUI_GRAPHICS").as_deref(), Some("sixel"));
        assert_eq!(env("TERM").as_deref(), Some("xterm-256color"));
    }

    /// One console lifetime against a real PTY: the opening size, a resize
    /// with pixel geometry (what the sixel pane derives its cell from), the
    /// fresh-attach repaint signal, input, and exit detection.
    #[test]
    fn session_round_trip() {
        if !std::path::Path::new("/dev/ptmx").exists() {
            eprintln!("skipped: this host has no /dev/ptmx");
            return;
        }
        let mut cmd = CommandBuilder::new(std::env::current_exe().expect("test binary path"));
        cmd.args([
            "--exact",
            "pty_bridge::tests::fake_console_child",
            "--ignored",
            "--nocapture",
        ]);
        cmd.env(CHILD_ENV, "1");

        let (output_tx, mut output_rx) = broadcast::channel::<Bytes>(2048);
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u16, u16, u16, u16)>();
        let repaint = Arc::new(AtomicBool::new(false));

        let session = {
            let output_tx = output_tx.clone();
            let repaint = repaint.clone();
            std::thread::spawn(move || {
                let mut size = PtySize::default();
                run_one_session(cmd, &output_tx, &mut input_rx, &mut resize_rx, &mut size, &repaint)
                    .map(|status| (size, status))
                    .map_err(|e| format!("{e:#}"))
            })
        };

        let mut seen = String::new();
        expect(&mut output_rx, &mut seen, "ready 80x24 0x0");

        // A real size change: the kernel signals the child itself.
        resize_tx.send((100, 30, 900, 540)).unwrap();
        expect(&mut output_rx, &mut seen, "winch 1 100x30 900x540");

        // Same size, so no kernel signal — the bridge sends SIGWINCH.
        repaint.store(true, Ordering::SeqCst);
        expect(&mut output_rx, &mut seen, "winch 2 100x30 900x540");
        assert!(!repaint.load(Ordering::SeqCst), "the pump consumes the repaint request");

        input_tx.send(b"quit\n".to_vec()).unwrap();
        expect(&mut output_rx, &mut seen, "bye");

        let start = Instant::now();
        while !session.is_finished() {
            assert!(start.elapsed() < DEADLINE, "the session did not notice the child's exit");
            std::thread::sleep(Duration::from_millis(5));
        }
        let (size, status) = session.join().expect("session thread").expect("session result");
        // A restart reopens at the operator's last size.
        assert_eq!((size.cols, size.rows, size.pixel_width, size.pixel_height), (100, 30, 900, 540));
        assert!(status.success(), "{status:?}");
        assert_eq!(describe(&status), "code 0");
    }

    /// A console that gives up says so with its exit code, and the code
    /// reaches the log and the banner.
    #[test]
    fn a_console_that_gives_up_is_reported_with_its_code() {
        if !std::path::Path::new("/dev/ptmx").exists() {
            eprintln!("skipped: this host has no /dev/ptmx");
            return;
        }
        let mut cmd = CommandBuilder::new(std::env::current_exe().expect("test binary path"));
        cmd.args([
            "--exact",
            "pty_bridge::tests::fake_console_child",
            "--ignored",
            "--nocapture",
        ]);
        cmd.env(CHILD_ENV, "1");

        let (output_tx, mut output_rx) = broadcast::channel::<Bytes>(2048);
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (_resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u16, u16, u16, u16)>();
        let repaint = AtomicBool::new(false);

        let session = std::thread::spawn({
            let output_tx = output_tx.clone();
            move || {
                let mut size = PtySize::default();
                run_one_session(cmd, &output_tx, &mut input_rx, &mut resize_rx, &mut size, &repaint)
                    .map_err(|e| format!("{e:#}"))
            }
        });

        let mut seen = String::new();
        expect(&mut output_rx, &mut seen, "ready 80x24 0x0");
        input_tx.send(b"fail\n".to_vec()).unwrap();
        // Its last words are delivered, although it is gone a moment later.
        expect(&mut output_rx, &mut seen, "cannot go on");

        let start = Instant::now();
        while !session.is_finished() {
            assert!(start.elapsed() < DEADLINE, "the session did not notice the child's exit");
            std::thread::sleep(Duration::from_millis(5));
        }
        let status = session.join().expect("session thread").expect("session result");
        assert!(!status.success());
        assert_eq!(describe(&status), "code 2");
        let banner = restart_banner(&describe(&status), restart_delay(3));
        assert!(banner.contains("embra-console exited (code 2)"), "{banner:?}");
        assert!(banner.contains("restarting in 4 s"), "{banner:?}");
    }

    #[test]
    fn a_console_that_keeps_exiting_early_is_restarted_more_slowly() {
        let secs = |n: u32| restart_delay(n).as_secs();
        // The first restart is as quick as it always was.
        assert_eq!([secs(0), secs(1)], [1, 1]);
        assert_eq!([secs(2), secs(3), secs(4), secs(5)], [2, 4, 8, 16]);
        assert_eq!([secs(6), secs(7), secs(1000), secs(u32::MAX)], [30, 30, 30, 30]);

        // Early exits add up; one console that stayed up clears them.
        let short = Duration::from_secs(3);
        assert_eq!(early_exits(0, short), 1);
        assert_eq!(early_exits(5, short), 6);
        assert_eq!(early_exits(5, STABLE_AFTER - Duration::from_millis(1)), 6);
        assert_eq!(early_exits(5, STABLE_AFTER), 0);
        assert_eq!(early_exits(u32::MAX, short), u32::MAX);
        // A crash loop pacing itself at the cap must not clear its own count.
        const { assert!(STABLE_AFTER.as_secs() > RESTART_MAX.as_secs()) };
    }
}
