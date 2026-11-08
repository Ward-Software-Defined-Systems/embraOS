//! The boot log on both consoles.
//!
//! embrad's stdio is `/dev/console`, the serial line (the last `console=` on
//! the kernel line), so its log lines, the DIAG lines before a halt and the
//! banner reached the serial line alone, and a display showed the kernel's
//! messages and the banner, which was copied there by hand. Here every line
//! embrad writes during the boot phase goes to its stdio and to the display
//! (`embra_boot::console`, which knows when `/dev/console` is the display
//! itself): the tracing lines through [`BootLog`], the DIAG lines and the
//! banner through [`emit`]. The log redirect that follows the operator UI's
//! start ends the boot phase ([`end_boot_phase`]); from then on the log file
//! alone, as before, and a halt writes its reason to both consoles itself
//! (`supervisor::halt_system`). The display is opened as PID 1 only: in
//! development embrad's lines stay on its own stdio.

use std::fs::File;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use embra_boot::console::BootConsole;
use tracing_subscriber::fmt::MakeWriter;

static BOOT_PHASE: AtomicBool = AtomicBool::new(true);
static DISPLAY: OnceLock<Option<Mutex<File>>> = OnceLock::new();

/// Until the log redirect: what embrad prints is for the operator at the
/// console, and the display gets its copy.
pub fn in_boot_phase() -> bool {
    BOOT_PHASE.load(Ordering::Acquire)
}

/// The log redirect: embrad's stdio is a file from here on, and the display
/// gets no further copy.
pub fn end_boot_phase() {
    BOOT_PHASE.store(false, Ordering::Release);
}

/// The display, opened once (PID 1 only), while the boot phase lasts.
fn mirror() -> Option<Mirror> {
    if !in_boot_phase() {
        return None;
    }
    DISPLAY
        .get_or_init(|| {
            if std::process::id() == 1 {
                BootConsole::display_mirror().map(Mutex::new)
            } else {
                None
            }
        })
        .as_ref()
        .map(Mirror)
}

/// One line for the operator, on stdout (the console before the redirect,
/// the log after) and on the display during the boot phase.
pub fn emit(line: &str) {
    let mut tee = Tee::new(io::stdout(), mirror());
    let _ = writeln!(tee, "{line}");
    let _ = tee.flush();
}

/// Formatted text for the operator (the banner), the same way.
pub fn emit_bytes(text: &[u8]) {
    let mut tee = Tee::new(io::stdout(), mirror());
    let _ = tee.write_all(text);
    let _ = tee.flush();
}

/// tracing's writer: stdout, and the display while the boot phase lasts.
#[derive(Clone, Copy, Debug, Default)]
pub struct BootLog;

impl<'a> MakeWriter<'a> for BootLog {
    type Writer = Tee<io::Stdout, Mirror>;

    fn make_writer(&'a self) -> Self::Writer {
        Tee::new(io::stdout(), mirror())
    }
}

/// The display under its lock.
pub struct Mirror(&'static Mutex<File>);

impl Write for Mirror {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Ok(mut display) = self.0.lock() {
            let _ = display.write_all(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A writer with a primary and, when it has one, a mirror. The mirror is
/// written first and whole: the primary is `/dev/console`, and on a machine
/// without a serial port that is a UART which is not there — the kernel
/// opens it all the same (`uart_open` takes the startup's "no port" as
/// success) and answers every write with EIO. A console that refuses a line
/// is never an error here, the way std treats a closed stdout: the line is
/// dropped, PID 1 goes on, and nothing upstream gets an error to act on.
/// The mirror's own failures are nobody's either.
pub struct Tee<P: Write, M: Write> {
    primary: P,
    mirror: Option<M>,
}

impl<P: Write, M: Write> Tee<P, M> {
    pub fn new(primary: P, mirror: Option<M>) -> Tee<P, M> {
        Tee { primary, mirror }
    }
}

impl<P: Write, M: Write> Write for Tee<P, M> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(mirror) = self.mirror.as_mut() {
            let _ = mirror.write_all(buf);
        }
        let _ = self.primary.write_all(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(mirror) = self.mirror.as_mut() {
            let _ = mirror.flush();
        }
        let _ = self.primary.flush();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tee_writes_its_mirror_only_while_it_has_one() {
        let mut primary = Vec::new();
        let mut mirror = Vec::new();
        {
            let mut tee = Tee::new(&mut primary, Some(&mut mirror));
            writeln!(tee, "Starting services").unwrap();
        }
        assert_eq!(primary, b"Starting services\n");
        assert_eq!(mirror, b"Starting services\n");

        let mut none: Option<&mut Vec<u8>> = None;
        {
            let mut tee = Tee::new(&mut primary, none.take());
            writeln!(tee, "after the redirect").unwrap();
        }
        assert_eq!(primary, b"Starting services\nafter the redirect\n");
        assert_eq!(mirror, b"Starting services\n", "the mirror saw nothing after the boot phase");
    }

    /// `/dev/console` on a VM without a serial port: open, and every write EIO.
    struct DeadConsole;

    impl Write for DeadConsole {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::from_raw_os_error(libc::EIO))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_dead_console_does_not_silence_the_display() {
        let mut display = Vec::new();
        {
            let mut tee = Tee::new(DeadConsole, Some(&mut display));
            writeln!(tee, "embrad starting as PID 1").expect("the display took the line");
        }
        assert_eq!(display, b"embrad starting as PID 1\n");

        let mut tee = Tee::new(DeadConsole, None::<&mut Vec<u8>>);
        assert!(writeln!(tee, "nowhere to go").is_ok(), "a dead console is never an error: PID 1's log writer must not fail");
    }

    #[test]
    fn the_boot_phase_ends_at_the_log_redirect_and_the_display_is_not_opened_in_development() {
        // Not PID 1 here: no display is opened, whatever the phase.
        assert!(mirror().is_none() || !in_boot_phase());
        end_boot_phase();
        assert!(!in_boot_phase());
        assert!(mirror().is_none());
    }
}
