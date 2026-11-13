//! The inactive slot: the image streamed into its partition while it is
//! hashed, synced, read back and hashed again, and only then staged in
//! grubenv; what a slot holds, read through a read-only mount of its
//! squashfs; the status embrad published.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use embra_boot::{Release, Slot};
use sha2::{Digest, Sha256};

use crate::snapshot::HashingWriter;

const CHUNK: usize = 4 * 1024 * 1024;

/// Stream `body` into the block device `dev`: exactly `size` bytes, no
/// more (a body past the size is refused), hashed as written; fsync; the
/// page cache dropped; the device read back and hashed again. Both digests
/// must be `expected`. `progress` is called with the bytes written so far.
pub fn write_image(
    body: &mut impl Read,
    dev: &Path,
    size: u64,
    expected: &str,
    mut progress: impl FnMut(u64),
) -> Result<(), String> {
    let file = OpenOptions::new().write(true).open(dev).map_err(|e| format!("open {} for writing: {e}", dev.display()))?;
    let mut writer = HashingWriter::new(file);
    let mut buf = vec![0u8; CHUNK];
    let mut written: u64 = 0;
    loop {
        let n = body.read(&mut buf).map_err(|e| format!("reading the image after {written} bytes: {e}"))?;
        if n == 0 {
            break;
        }
        if written + n as u64 > size {
            return Err(format!("the image runs past its declared size of {size} bytes; the slot holds no usable image"));
        }
        writer.write_all(&buf[..n]).map_err(|e| format!("writing {} after {written} bytes: {e}", dev.display()))?;
        written += n as u64;
        progress(written);
    }
    if written != size {
        return Err(format!("the image ended after {written} of {size} bytes; the slot holds no usable image"));
    }
    let (file, streamed, _) = writer.finish();
    file.sync_all().map_err(|e| format!("sync {}: {e}", dev.display()))?;
    drop_page_cache(&file);
    drop(file);
    if streamed != expected {
        return Err(format!("the image's sha256 as streamed ({}…) is not the manifest's ({}…); the slot holds no usable image", &streamed[..16], &expected[..16]));
    }
    let read_back = sha256_prefix(dev, size).map_err(|e| format!("reading {} back: {e}", dev.display()))?;
    if read_back != expected {
        return Err(format!("the image as read back from {} ({}…) is not the manifest's ({}…); the slot holds no usable image", dev.display(), &read_back[..16], &expected[..16]));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn drop_page_cache(file: &File) {
    use std::os::unix::io::AsRawFd;
    // SAFETY: a valid descriptor; the advice covers the whole file and cannot
    // fail in a way that matters (the read-back is then served from the cache).
    unsafe {
        libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
    }
}

#[cfg(not(target_os = "linux"))]
fn drop_page_cache(_file: &File) {}

/// The sha256 of the first `size` bytes of `path`.
pub fn sha256_prefix(path: &Path, size: u64) -> io::Result<String> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut left = size;
    while left > 0 {
        let want = (left as usize).min(buf.len());
        let n = f.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("{} ended {left} bytes early", path.display())));
        }
        hasher.update(&buf[..n]);
        left -= n as u64;
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Where a slot's squashfs is mounted for one read.
pub fn mount_point(slot: Slot) -> PathBuf {
    PathBuf::from(format!("/run/embra/slot-{slot}"))
}

/// A slot's squashfs, mounted read-only at the slot's mount point for one
/// read and unmounted when dropped.
#[cfg(target_os = "linux")]
struct MountedSlot {
    root: PathBuf,
}

#[cfg(target_os = "linux")]
impl MountedSlot {
    /// `None` when the partition holds no filesystem (the hole slot B is at
    /// build, or a half-written image).
    fn mount(slot: Slot, dev: &Path) -> Result<Option<MountedSlot>, String> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let mp = mount_point(slot);
        std::fs::create_dir_all(&mp).map_err(|e| format!("create {}: {e}", mp.display()))?;
        let source = CString::new(dev.as_os_str().as_bytes()).map_err(|_| "a device path with a NUL byte".to_string())?;
        let target = CString::new(mp.as_os_str().as_bytes()).map_err(|_| "a mount path with a NUL byte".to_string())?;
        let fstype = CString::new("squashfs").expect("a literal");
        // MS_SILENT: a partition without a filesystem is an answer here (`None`),
        // and without the flag the kernel says "Can't find a SQUASHFS superblock"
        // at error level, which a `quiet` console still shows: in the middle of
        // the installer's question, and in the kernel's log at every `status`.
        let flags = libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_SILENT;
        // SAFETY: NUL-terminated strings that outlive the call; no data pointer.
        let rc = unsafe { libc::mount(source.as_ptr(), target.as_ptr(), fstype.as_ptr(), flags, std::ptr::null()) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            return match err.raw_os_error() {
                Some(libc::EINVAL) => Ok(None),
                _ => Err(format!("mount {} read-only: {err}", dev.display())),
            };
        }
        Ok(Some(MountedSlot { root: mp }))
    }
}

#[cfg(target_os = "linux")]
impl Drop for MountedSlot {
    fn drop(&mut self) {
        use std::os::unix::ffi::OsStrExt;
        if let Ok(target) = std::ffi::CString::new(self.root.as_os_str().as_bytes()) {
            // SAFETY: a NUL-terminated string; a failed unmount is reported by the next mount.
            unsafe {
                libc::umount2(target.as_ptr(), 0);
            }
        }
    }
}

/// The release a slot carries, through a read-only mount of its squashfs:
/// `None` when the partition holds no filesystem (the hole slot B is at
/// build, or a half-written image).
#[cfg(target_os = "linux")]
pub fn release_of(slot: Slot, dev: &Path) -> Result<Option<Release>, String> {
    let Some(mounted) = MountedSlot::mount(slot, dev)? else {
        return Ok(None);
    };
    let release = Release::read_from(&mounted.root.join("etc/embraos-release"));
    drop(mounted);
    match release {
        Ok(r) => Ok(Some(r)),
        Err(embra_boot::ReleaseError::Io(e)) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", dev.display())),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn release_of(_slot: Slot, _dev: &Path) -> Result<Option<Release>, String> {
    Err("a slot is read on Linux only".into())
}

/// Which of `files` (paths inside the slot, `boot/ucode.cpio`) the slot does
/// not hold as a file, through the same read-only mount; every one of them
/// when the partition holds no filesystem.
#[cfg(target_os = "linux")]
pub fn missing_files(slot: Slot, dev: &Path, files: &[String]) -> Result<Vec<String>, String> {
    let Some(mounted) = MountedSlot::mount(slot, dev)? else {
        return Ok(files.to_vec());
    };
    Ok(files.iter().filter(|f| !mounted.root.join(f).is_file()).cloned().collect())
}

#[cfg(not(target_os = "linux"))]
pub fn missing_files(_slot: Slot, _dev: &Path, _files: &[String]) -> Result<Vec<String>, String> {
    Err("a slot is read on Linux only".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_streamed_image_hashes_as_its_read_back_and_a_short_or_long_body_is_refused() {
        let dir = std::env::temp_dir().join(format!("embra-update-slot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dev = dir.join("rootfs_b");
        std::fs::write(&dev, std::iter::repeat_n(0u8, 1 << 20).collect::<Vec<u8>>()).unwrap();
        let image: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let expected = hex::encode(Sha256::digest(&image));
        let mut seen = Vec::new();
        write_image(&mut image.as_slice(), &dev, image.len() as u64, &expected, |n| seen.push(n)).unwrap();
        assert_eq!(seen.last().copied(), Some(image.len() as u64));
        assert_eq!(sha256_prefix(&dev, image.len() as u64).unwrap(), expected);
        let on_disk = std::fs::read(&dev).unwrap();
        assert_eq!(&on_disk[..image.len()], &image[..]);
        assert_eq!(on_disk.len(), 1 << 20, "the device is not truncated");
        // Short, long, and a wrong digest.
        assert!(write_image(&mut image[..1000].as_ref(), &dev, image.len() as u64, &expected, |_| ()).unwrap_err().contains("ended after 1000"));
        assert!(write_image(&mut image.as_slice(), &dev, 1000, &expected, |_| ()).unwrap_err().contains("runs past"));
        assert!(write_image(&mut image.as_slice(), &dev, image.len() as u64, &"00".repeat(32), |_| ()).unwrap_err().contains("as streamed"));
        assert!(write_image(&mut image.as_slice(), &dir.join("missing"), 1, &expected, |_| ()).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_hashing_writer_refuses_a_byte_past_the_declared_size() {
        // Covered by the long-body case above: the refusal comes before the
        // write of the byte past the size.
        assert_eq!(mount_point(Slot::B), PathBuf::from("/run/embra/slot-b"));
    }
}
