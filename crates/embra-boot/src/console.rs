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
    use std::os::unix::io::AsRawFd;
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
