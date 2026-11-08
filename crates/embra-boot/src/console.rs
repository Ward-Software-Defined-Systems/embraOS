//! The boot console: where embra-init and embrad speak to the operator before
//! a service exists.
//!
//! The kernel line names two consoles, `console=tty0` (the display) and then
//! the serial line, and the kernel writes its own messages to both. A
//! program's write to `/dev/console` reaches one of them: the last named
//! (`Documentation/admin-guide/serial-console.rst`), the serial line. So the
//! serial line alone heard embra-init and embrad, and a machine with a display
//! and no serial port — an ESXi or Hyper-V VM, bare metal — showed nothing of
//! a boot that stopped. [`BootConsole`] writes to `/dev/console` and, when the
//! display is not that console, to `/dev/tty0` as well: a boot message is on
//! both. Which one `/dev/console` is comes from `TIOCGDEV`, the device number
//! behind it; a kernel line that names the display last gets no second copy.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};

/// What a program's console write reaches: the last `console=` of the kernel line.
pub const CONSOLE: &str = "/dev/console";
/// The display: the active virtual terminal, fbcon's when the kernel has a framebuffer.
pub const DISPLAY: &str = "/dev/tty0";

/// The Linux virtual terminals are character devices of major 4, minors 0 to
/// 63 (`Documentation/admin-guide/devices.txt`); tty0 is the active one. The
/// serial lines are major 4 from minor 64 (ttyS), major 204 (ttyAMA) and others.
const VT_MAJOR: u32 = 4;
const VT_MINORS: u32 = 64;

/// Whether a character device is one of the virtual terminals, the display.
pub fn is_virtual_terminal(major: u32, minor: u32) -> bool {
    major == VT_MAJOR && minor < VT_MINORS
}

/// A `TIOCGDEV` answer as (major, minor): the kernel's `new_encode_dev`
/// packing (`include/linux/kdev_t.h`), which is not glibc's `dev_t`.
pub fn decode_dev(dev: u32) -> (u32, u32) {
    ((dev & 0x000f_ff00) >> 8, (dev & 0xff) | ((dev >> 12) & 0x000f_ff00))
}

/// The two consoles a boot message goes to. Every write is best effort: a
/// console that cannot be opened or written is skipped, never an error, and
/// without `/dev/console` the message goes to stderr, which the kernel
/// opened on the console for init.
pub struct BootConsole {
    console: Option<File>,
    display: Option<File>,
}

impl BootConsole {
    /// `/dev/console`, and `/dev/tty0` unless `/dev/console` is the display.
    pub fn open() -> BootConsole {
        let console = OpenOptions::new().write(true).open(CONSOLE).ok();
        let console_is_the_display = console
            .as_ref()
            .and_then(device_of)
            .is_some_and(|(major, minor)| is_virtual_terminal(major, minor));
        let display = if console_is_the_display {
            None
        } else {
            OpenOptions::new().write(true).open(DISPLAY).ok()
        };
        BootConsole { console, display }
    }

    /// The display alone, for a program whose stdio is the console already
    /// and copies to the display (embrad): `None` when `/dev/console` is the
    /// display or `/dev/tty0` cannot be opened.
    pub fn display_mirror() -> Option<File> {
        BootConsole::open().display
    }

    /// A console over the files given (tests; a caller that holds them).
    pub fn from_parts(console: Option<File>, display: Option<File>) -> BootConsole {
        BootConsole { console, display }
    }

    /// `from_parts` for a prompt: the files given are read as well.
    pub fn interactive_from_parts(console: Option<File>, display: Option<File>) -> BootConsole {
        BootConsole { console, display }
    }

    /// The consoles for a prompt: `/dev/console` and, unless it is the
    /// display, `/dev/tty0`, each opened to read and write without being
    /// taken as the controlling terminal (`O_NOCTTY`): a prompt needs
    /// `read(2)` alone, and the kernel's canonical line discipline, on by
    /// default on both, delivers a line on Enter with the echo the operator
    /// expects. A console that will not open read-write is kept write-only.
    pub fn open_interactive() -> BootConsole {
        fn read_write(path: &str) -> Option<File> {
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOCTTY).open(path).ok()
        }
        let console = read_write(CONSOLE).or_else(|| OpenOptions::new().write(true).open(CONSOLE).ok());
        let console_is_the_display = console
            .as_ref()
            .and_then(device_of)
            .is_some_and(|(major, minor)| is_virtual_terminal(major, minor));
        let display = if console_is_the_display {
            None
        } else {
            read_write(DISPLAY).or_else(|| OpenOptions::new().write(true).open(DISPLAY).ok())
        };
        BootConsole { console, display }
    }

    /// A line typed on either console. The prompt goes to both; the first
    /// complete line from either answers and is echoed on the other, so the
    /// two transcripts read the same. A console that cannot be read drops
    /// out — a UART that is not there answers EIO or never, a closed pipe
    /// EOF — and its buffered bytes count as its line at EOF. `None` when no
    /// console can answer, or when the timeout passes.
    pub fn prompt_line(&mut self, text: &str) -> io::Result<Option<String>> {
        self.prompt_line_timeout(text, None)
    }

    pub fn prompt_line_timeout(&mut self, text: &str, timeout: Option<Duration>) -> io::Result<Option<String>> {
        let _ = self.write_all(text.as_bytes());
        let _ = self.flush();
        let started = Instant::now();
        // Without /dev/console the prompt went to stderr; the answer comes
        // from stdin, the terminal of a developer's run.
        let console_fd = self.console.as_ref().map(|f| f.as_raw_fd()).unwrap_or(0);
        let mut readers: Vec<Reader> = vec![Reader { fd: console_fd, which: Which::Console, buf: String::new(), alive: true }];
        if let Some(d) = self.display.as_ref() {
            readers.push(Reader { fd: d.as_raw_fd(), which: Which::Display, buf: String::new(), alive: true });
        }
        let answer = loop {
            let live: Vec<usize> = readers.iter().enumerate().filter(|(_, r)| r.alive).map(|(i, _)| i).collect();
            if live.is_empty() {
                break None;
            }
            let wait_ms: libc::c_int = match timeout {
                None => -1,
                Some(t) => {
                    let elapsed = started.elapsed();
                    if elapsed >= t {
                        break None;
                    }
                    (t - elapsed).as_millis().min(libc::c_int::MAX as u128) as libc::c_int
                }
            };
            let mut pfds: Vec<libc::pollfd> = live.iter().map(|&i| libc::pollfd { fd: readers[i].fd, events: libc::POLLIN, revents: 0 }).collect();
            // SAFETY: `pfds` is a live array of `pfds.len()` pollfd structs for the call.
            let rc = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, wait_ms) };
            if rc < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if rc == 0 {
                break None;
            }
            let mut found: Option<(Which, String)> = None;
            for (pfd, &i) in pfds.iter().zip(live.iter()) {
                let r = &mut readers[i];
                if pfd.revents == 0 {
                    continue;
                }
                if pfd.revents & libc::POLLIN != 0 {
                    let mut b = [0u8; 256];
                    // SAFETY: `b` is a 256-byte buffer the read may fill.
                    let n = unsafe { libc::read(r.fd, b.as_mut_ptr().cast(), b.len()) };
                    if n > 0 {
                        r.buf.push_str(&String::from_utf8_lossy(&b[..n as usize]));
                        if let Some(pos) = r.buf.find('\n') {
                            found = Some((r.which, r.buf[..pos].trim().to_string()));
                            break;
                        }
                        continue;
                    }
                    if n < 0 {
                        let e = io::Error::last_os_error();
                        if matches!(e.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) {
                            continue;
                        }
                    }
                    // EOF, or a console that refuses to be read (EIO): gone.
                    r.alive = false;
                } else {
                    // POLLHUP, POLLERR, POLLNVAL without data: gone.
                    r.alive = false;
                }
                if !r.alive && !r.buf.trim().is_empty() {
                    found = Some((r.which, r.buf.trim().to_string()));
                    break;
                }
            }
            if found.is_some() {
                break found;
            }
        };
        if let Some((which, line)) = &answer {
            // The kernel echoed the line where it was typed; the other
            // console gets it too, so both transcripts read the same.
            let echo = format!("{line}\n");
            match which {
                Which::Console => {
                    if let Some(d) = self.display.as_mut() {
                        let _ = d.write_all(echo.as_bytes());
                    }
                }
                Which::Display => {
                    if let Some(c) = self.console.as_mut() {
                        let _ = c.write_all(echo.as_bytes());
                    }
                }
            }
        }
        Ok(answer.map(|(_, line)| line))
    }

    pub fn has_display(&self) -> bool {
        self.display.is_some()
    }

    /// One line, on every console this has.
    pub fn line(&mut self, text: &str) {
        let _ = self.write_all(text.as_bytes());
        let _ = self.write_all(b"\n");
        let _ = self.flush();
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Which {
    Console,
    Display,
}

/// One console a prompt listens on.
struct Reader {
    fd: std::os::unix::io::RawFd,
    which: Which,
    buf: String,
    alive: bool,
}

impl Write for BootConsole {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.console.as_mut() {
            Some(console) => {
                let _ = console.write_all(buf);
            }
            None => {
                let _ = io::stderr().write_all(buf);
            }
        }
        if let Some(display) = self.display.as_mut() {
            let _ = display.write_all(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(console) = self.console.as_mut() {
            let _ = console.flush();
        }
        if let Some(display) = self.display.as_mut() {
            let _ = display.flush();
        }
        Ok(())
    }
}

/// The device number behind an open console, through `TIOCGDEV`.
#[cfg(target_os = "linux")]
fn device_of(console: &File) -> Option<(u32, u32)> {
    let mut dev: libc::c_uint = 0;
    // SAFETY: TIOCGDEV writes one unsigned int through the pointer it is
    // given, and `dev` lives for the call.
    let rc = unsafe { libc::ioctl(console.as_raw_fd(), libc::TIOCGDEV, &mut dev) };
    (rc == 0).then(|| decode_dev(dev))
}

#[cfg(not(target_os = "linux"))]
fn device_of(_console: &File) -> Option<(u32, u32)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `new_encode_dev` (`include/linux/kdev_t.h`).
    fn encode_dev(major: u32, minor: u32) -> u32 {
        (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)
    }

    #[test]
    fn a_virtual_terminal_is_told_from_a_serial_line() {
        assert!(is_virtual_terminal(4, 0), "tty0, the active virtual terminal");
        assert!(is_virtual_terminal(4, 1), "tty1");
        assert!(is_virtual_terminal(4, 63), "tty63, the last one");
        assert!(!is_virtual_terminal(4, 64), "ttyS0");
        assert!(!is_virtual_terminal(4, 65), "ttyS1");
        assert!(!is_virtual_terminal(204, 64), "ttyAMA0");
        assert!(!is_virtual_terminal(5, 1), "/dev/console itself");
    }

    #[test]
    fn a_tiocgdev_answer_decodes_as_the_kernel_encodes_it() {
        for (major, minor) in [(4, 0), (4, 63), (4, 64), (204, 64), (259, 300), (4095, 1_048_575)] {
            assert_eq!(decode_dev(encode_dev(major, minor)), (major, minor));
        }
    }

    #[test]
    fn a_line_reaches_every_console_it_has_and_a_missing_display_is_skipped() {
        let dir = std::env::temp_dir().join(format!("embra-boot-console-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let console_path = dir.join("console");
        let display_path = dir.join("display");
        let open = |p: &std::path::Path| OpenOptions::new().create(true).append(true).open(p).unwrap();

        let mut both = BootConsole::from_parts(Some(open(&console_path)), Some(open(&display_path)));
        assert!(both.has_display());
        both.line("[embra-init] FATAL: no partition named rootfs_a");
        assert_eq!(std::fs::read_to_string(&console_path).unwrap(), "[embra-init] FATAL: no partition named rootfs_a\n");
        assert_eq!(std::fs::read_to_string(&display_path).unwrap(), "[embra-init] FATAL: no partition named rootfs_a\n");

        let mut serial_only = BootConsole::from_parts(Some(open(&console_path)), None);
        assert!(!serial_only.has_display());
        serial_only.line("second");
        assert!(std::fs::read_to_string(&console_path).unwrap().ends_with("second\n"));
        assert!(!std::fs::read_to_string(&display_path).unwrap().contains("second"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod prompt_tests {
    use super::*;
    use std::os::unix::io::FromRawFd;

    /// A pipe as (reader, writer) files.
    fn pipe() -> (File, File) {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: pipe fills the two-element array it is given.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: both descriptors are fresh and owned here once.
        unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
    }

    #[test]
    fn a_prompt_is_answered_by_the_first_console_that_sends_a_line() {
        let (console_r, mut console_w) = pipe();
        let (display_r, mut display_w) = pipe();
        let mut both = BootConsole::interactive_from_parts(Some(console_r), Some(display_r));
        display_w.write_all(b"2\r\n").unwrap();
        assert_eq!(both.prompt_line_timeout("which? ", Some(Duration::from_secs(5))).unwrap(), Some("2".to_string()));
        console_w.write_all(b"yes\n").unwrap();
        assert_eq!(both.prompt_line_timeout("sure? ", Some(Duration::from_secs(5))).unwrap(), Some("yes".to_string()));
    }

    #[test]
    fn a_dead_console_is_skipped_and_a_line_without_newline_counts_at_eof() {
        let (console_r, console_w) = pipe();
        let (display_r, mut display_w) = pipe();
        drop(console_w); // the serial line is gone: EOF at once, nothing typed
        let mut both = BootConsole::interactive_from_parts(Some(console_r), Some(display_r));
        display_w.write_all(b"partial").unwrap();
        drop(display_w); // EOF after a line without its newline
        assert_eq!(both.prompt_line_timeout("? ", Some(Duration::from_secs(5))).unwrap(), Some("partial".to_string()));
    }

    #[test]
    fn a_prompt_with_no_live_console_returns_none_and_so_does_a_timeout() {
        let (console_r, console_w) = pipe();
        let (display_r, display_w) = pipe();
        drop(console_w);
        drop(display_w);
        let mut dead = BootConsole::interactive_from_parts(Some(console_r), Some(display_r));
        assert_eq!(dead.prompt_line_timeout("? ", Some(Duration::from_secs(5))).unwrap(), None);

        let (console_r, _console_w) = pipe();
        let (display_r, _display_w) = pipe();
        let mut silent = BootConsole::interactive_from_parts(Some(console_r), Some(display_r));
        let started = Instant::now();
        assert_eq!(silent.prompt_line_timeout("? ", Some(Duration::from_millis(150))).unwrap(), None);
        assert!(started.elapsed() < Duration::from_secs(3), "the timeout, not a hang");
    }
}
