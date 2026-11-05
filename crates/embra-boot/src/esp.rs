//! The EFI System Partition, mounted only while its environment block is
//! read or written: embrad confirms a trial boot there, embra-update stages
//! a slot and rolls back there. One helper, one lock (`flock` on a file
//! under `/run`), so the two writers never interleave, and the partition is
//! unmounted the rest of the time.

use crate::grubenv::GrubEnv;
use std::io;
use std::path::{Path, PathBuf};

pub const MOUNT_POINT: &str = "/run/embra/esp";
pub const LOCK_PATH: &str = "/run/embra/esp.lock";
/// `grubenv` beside the loader and `grub.cfg`, relative to the mount.
pub const GRUBENV_RELATIVE: &str = "EFI/BOOT/grubenv";

pub fn grubenv_path() -> PathBuf {
    Path::new(MOUNT_POINT).join(GRUBENV_RELATIVE)
}

/// Read the block (the partition mounted read-only for the call).
#[cfg(target_os = "linux")]
pub fn with_env<T>(dev: &Path, f: impl FnOnce(&GrubEnv) -> T) -> io::Result<T> {
    let _mounted = Mounted::mount(dev, true)?;
    let env = GrubEnv::read_file(&grubenv_path())?;
    Ok(f(&env))
}

/// Read the block, let `f` change it, write it back in place and fsync, then
/// unmount; the partition is mounted read-write for the call alone.
#[cfg(target_os = "linux")]
pub fn with_env_mut<T>(dev: &Path, f: impl FnOnce(&mut GrubEnv) -> T) -> io::Result<T> {
    let _mounted = Mounted::mount(dev, false)?;
    let path = grubenv_path();
    let mut env = GrubEnv::read_file(&path)?;
    let out = f(&mut env);
    env.write_file(&path)?;
    Ok(out)
}

#[cfg(not(target_os = "linux"))]
pub fn with_env<T>(_dev: &Path, _f: impl FnOnce(&GrubEnv) -> T) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "the ESP is mounted on Linux only"))
}

#[cfg(not(target_os = "linux"))]
pub fn with_env_mut<T>(_dev: &Path, _f: impl FnOnce(&mut GrubEnv) -> T) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "the ESP is mounted on Linux only"))
}

/// The mount, held under the lock; dropped, it unmounts and then releases
/// the lock (the lock file closes after `drop` ran).
#[cfg(target_os = "linux")]
struct Mounted {
    _lock: std::fs::File,
}

#[cfg(target_os = "linux")]
impl Mounted {
    fn mount(dev: &Path, read_only: bool) -> io::Result<Mounted> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::io::AsRawFd;

        std::fs::create_dir_all(MOUNT_POINT)?;
        let lock = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(LOCK_PATH)?;
        // SAFETY: a valid descriptor; flock takes no pointer.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let source = CString::new(dev.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "device path with a NUL byte"))?;
        let target = CString::new(MOUNT_POINT).expect("a literal without NUL");
        let fstype = CString::new("vfat").expect("a literal without NUL");
        let mut flags = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
        if read_only {
            flags |= libc::MS_RDONLY;
        }
        // SAFETY: three NUL-terminated strings that outlive the call; no data pointer.
        let rc = unsafe { libc::mount(source.as_ptr(), target.as_ptr(), fstype.as_ptr(), flags, std::ptr::null()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Mounted { _lock: lock })
    }
}

#[cfg(target_os = "linux")]
impl Drop for Mounted {
    fn drop(&mut self) {
        let target = std::ffi::CString::new(MOUNT_POINT).expect("a literal without NUL");
        // SAFETY: a NUL-terminated string; a failed unmount is left for the
        // next mount to report (EBUSY), never a panic in PID 1.
        unsafe {
            libc::umount2(target.as_ptr(), 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_block_lives_beside_the_loader() {
        assert_eq!(grubenv_path(), PathBuf::from("/run/embra/esp/EFI/BOOT/grubenv"));
    }

    #[test]
    fn a_device_that_is_not_an_esp_is_an_error_and_not_a_panic() {
        // Not root, or no such device: either way an error comes back and the
        // lock and the mount point are released.
        let err = with_env(Path::new("/dev/null"), |_| ()).unwrap_err();
        assert!(!err.to_string().is_empty());
    }
}
