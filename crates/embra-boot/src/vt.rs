//! The display's virtual terminal while the display console runs.
//!
//! The kernel's text console lives on the VT the display shows (`tty1`, the
//! current one at boot: `console=tty0`). The display console (cog on the
//! DRM device) draws past it, and the VT keeps its two habits: fbcon
//! repaints the text console over the kiosk's frame on its next kernel
//! message, and the keyboard driver still reads the keys for itself —
//! Ctrl+Alt+Fn switches to an empty terminal, SysRq and Ctrl+Alt+Del reach
//! the kernel and PID 1. `take_graphics` parks both: `KD_GRAPHICS` keeps
//! fbcon off the framebuffer, `K_OFF` takes the keyboard out of the VT
//! layer (libinput reads the event devices on its own). `release` gives
//! both back, which embrad does when the display console stops and before a
//! halt reason is written to the display. Every ioctl here is from
//! `<linux/kd.h>`; `libc` carries none of them.

use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// The current VT, whichever number it is.
pub const CURRENT_VT: &str = "/dev/tty0";

/// `KDSETMODE`: the console's mode, text or graphics.
pub const KDSETMODE: libc::c_ulong = 0x4B3A;
/// `KDGETMODE`.
pub const KDGETMODE: libc::c_ulong = 0x4B3B;
/// `KDSKBMODE`: the keyboard's mode.
pub const KDSKBMODE: libc::c_ulong = 0x4B45;
/// `KDGKBMODE`.
pub const KDGKBMODE: libc::c_ulong = 0x4B44;
pub const KD_TEXT: libc::c_int = 0x00;
pub const KD_GRAPHICS: libc::c_int = 0x01;
pub const K_UNICODE: libc::c_int = 0x03;
pub const K_OFF: libc::c_int = 0x04;

/// The display console is about to draw: fbcon off the framebuffer, the
/// keyboard out of the VT layer. Idempotent.
pub fn take_graphics() -> io::Result<()> {
    take_graphics_on(Path::new(CURRENT_VT))
}

/// The display console stopped, or a halt reason is about to be written:
/// the text console and its keyboard are back.
pub fn release() -> io::Result<()> {
    release_on(Path::new(CURRENT_VT))
}

pub fn take_graphics_on(vt: &Path) -> io::Result<()> {
    let f = OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOCTTY).open(vt)?;
    ioctl_int(f.as_raw_fd(), KDSETMODE, KD_GRAPHICS)?;
    ioctl_int(f.as_raw_fd(), KDSKBMODE, K_OFF)
}

pub fn release_on(vt: &Path) -> io::Result<()> {
    let f = OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOCTTY).open(vt)?;
    let kbd = ioctl_int(f.as_raw_fd(), KDSKBMODE, K_UNICODE);
    let mode = ioctl_int(f.as_raw_fd(), KDSETMODE, KD_TEXT);
    kbd.and(mode)
}

fn ioctl_int(fd: libc::c_int, request: libc::c_ulong, value: libc::c_int) -> io::Result<()> {
    // SAFETY: the three ioctls take an int by value (`<linux/kd.h>`), and
    // the descriptor is the open file's for the call's duration.
    let r = unsafe { libc::ioctl(fd, request as _, value as libc::c_ulong) };
    if r == -1 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The numbers are the kernel's (`include/uapi/linux/kd.h`): a change
    /// here is a typo, never an update.
    #[test]
    fn the_vt_mode_constants_are_the_kernels() {
        assert_eq!(KDSETMODE, 0x4B3A);
        assert_eq!(KDGETMODE, 0x4B3B);
        assert_eq!(KDSKBMODE, 0x4B45);
        assert_eq!(KDGKBMODE, 0x4B44);
        assert_eq!((KD_TEXT, KD_GRAPHICS), (0, 1));
        assert_eq!((K_UNICODE, K_OFF), (3, 4));
    }

    /// No VT (a container, a machine without a display): an error the
    /// caller logs, never a panic, and a release is as harmless.
    #[test]
    fn without_a_vt_the_guard_reports_and_does_nothing() {
        let none = Path::new("/nonexistent/tty0");
        assert!(take_graphics_on(none).is_err());
        assert!(release_on(none).is_err());
        let dir = std::env::temp_dir().join(format!("embra-vt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("not-a-tty");
        std::fs::write(&file, b"").unwrap();
        // A plain file opens and refuses the ioctl (ENOTTY).
        assert!(take_graphics_on(&file).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
