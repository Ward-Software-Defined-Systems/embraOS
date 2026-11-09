//! A loop device over a file: the installer boots the rootfs squashfs the
//! medium carries from one. `libc` 0.2 defines none of the loop ioctls; the
//! numbers and the structures are the kernel's `include/uapi/linux/loop.h`
//! (`LOOP_SET_FD` 0x4C00, `LOOP_CONFIGURE` 0x4C0A since 5.8,
//! `LOOP_CTL_GET_FREE` 0x4C82, `LO_FLAGS_READ_ONLY` 1).

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

pub const LOOP_CONTROL: &str = "/dev/loop-control";
const LOOP_CTL_GET_FREE: libc::c_ulong = 0x4C82;
const LOOP_CONFIGURE: libc::c_ulong = 0x4C0A;
const LO_FLAGS_READ_ONLY: u32 = 1;
const LO_NAME_SIZE: usize = 64;
const LO_KEY_SIZE: usize = 32;

/// `struct loop_info64` (232 bytes).
#[repr(C)]
pub struct LoopInfo64 {
    pub lo_device: u64,
    pub lo_inode: u64,
    pub lo_rdevice: u64,
    pub lo_offset: u64,
    pub lo_sizelimit: u64,
    pub lo_number: u32,
    pub lo_encrypt_type: u32,
    pub lo_encrypt_key_size: u32,
    pub lo_flags: u32,
    pub lo_file_name: [u8; LO_NAME_SIZE],
    pub lo_crypt_name: [u8; LO_NAME_SIZE],
    pub lo_encrypt_key: [u8; LO_KEY_SIZE],
    pub lo_init: [u64; 2],
}

/// `struct loop_config` (304 bytes): the backing fd, a block size (0 = the
/// default), the info, and reserved words that must be zero.
#[repr(C)]
pub struct LoopConfig {
    pub fd: u32,
    pub block_size: u32,
    pub info: LoopInfo64,
    pub reserved: [u64; 8],
}

impl LoopConfig {
    /// A read-only loop over `fd`.
    pub fn read_only(fd: u32) -> LoopConfig {
        LoopConfig {
            fd,
            block_size: 0,
            info: LoopInfo64 {
                lo_device: 0,
                lo_inode: 0,
                lo_rdevice: 0,
                lo_offset: 0,
                lo_sizelimit: 0,
                lo_number: 0,
                lo_encrypt_type: 0,
                lo_encrypt_key_size: 0,
                lo_flags: LO_FLAGS_READ_ONLY,
                lo_file_name: [0; LO_NAME_SIZE],
                lo_crypt_name: [0; LO_NAME_SIZE],
                lo_encrypt_key: [0; LO_KEY_SIZE],
                lo_init: [0; 2],
            },
            reserved: [0; 8],
        }
    }
}

/// A free loop device attached read-only to `backing`: `/dev/loopN`, N from
/// `/dev/loop-control` (devtmpfs makes the node for a new N by itself).
pub fn attach_read_only(backing: &Path) -> io::Result<PathBuf> {
    let control = OpenOptions::new().read(true).write(true).open(LOOP_CONTROL)?;
    // SAFETY: LOOP_CTL_GET_FREE takes no argument and returns the number.
    let n = unsafe { libc::ioctl(control.as_raw_fd(), LOOP_CTL_GET_FREE as _) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let dev = PathBuf::from(format!("/dev/loop{n}"));
    let file = File::open(backing)?;
    let loop_dev = OpenOptions::new().read(true).write(true).open(&dev)?;
    let config = LoopConfig::read_only(file.as_raw_fd() as u32);
    // SAFETY: LOOP_CONFIGURE reads one `struct loop_config`, which `config`
    // is, laid out as the kernel's (`repr(C)`, sizes pinned by a test).
    let rc = unsafe { libc::ioctl(loop_dev.as_raw_fd(), LOOP_CONFIGURE as _, &config) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(dev)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The structures match the kernel's (`include/uapi/linux/loop.h`): a
    /// wrong size would hand the ioctl garbage.
    #[test]
    fn the_loop_structures_have_the_kernels_sizes_and_the_read_only_flag() {
        assert_eq!(std::mem::size_of::<LoopInfo64>(), 232);
        assert_eq!(std::mem::size_of::<LoopConfig>(), 304);
        let c = LoopConfig::read_only(7);
        assert_eq!(c.fd, 7);
        assert_eq!(c.info.lo_flags, 1);
        assert_eq!(c.block_size, 0);
        assert!(c.reserved.iter().all(|r| *r == 0));
        assert_eq!(LOOP_CTL_GET_FREE, 0x4C82);
        assert_eq!(LOOP_CONFIGURE, 0x4C0A);
    }
}
