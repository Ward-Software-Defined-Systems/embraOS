//! The installation medium: the block devices a boot tries in turn to find
//! it, and the manifest that marks it (`/embraos/install.manifest`, written
//! beside the rootfs by the image build; `install.v1`).

use std::ffi::CString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The manifest's path on the medium.
pub const MANIFEST_RELATIVE: &str = "embraos/install.manifest";
pub const FORMAT: &str = "install.v1";

/// The block devices worth a mount attempt, in sorted order: whole disks and
/// partitions, the optical drives among them; not the loop, RAM, zram,
/// device-mapper, MD, NBD and floppy devices, and nothing of size 0 (an
/// empty drive).
pub fn candidates(sys_class_block: &Path) -> Vec<String> {
    let Ok(dir) = std::fs::read_dir(sys_class_block) else { return Vec::new() };
    let mut names: Vec<String> = dir
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !["loop", "ram", "zram", "dm-", "md", "nbd", "fd"].iter().any(|p| n.starts_with(p)))
        .filter(|n| {
            std::fs::read_to_string(sys_class_block.join(n).join("size"))
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .is_some_and(|sectors| sectors > 0)
        })
        .collect();
    names.sort();
    names
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallManifest {
    pub version: String,
    pub build_id: String,
    pub arch: String,
    pub engine: String,
    pub built: String,
    /// The kernel's path on the medium.
    pub kernel: String,
    /// The rootfs squashfs's path on the medium, its size and sha256.
    pub rootfs: String,
    pub rootfs_size: u64,
    pub rootfs_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaError {
    /// Not an installer manifest, or another format of one.
    Format(String),
    Missing(&'static str),
    Bad(&'static str),
}

impl fmt::Display for MediaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MediaError::Format(got) => write!(f, "install.manifest: FORMAT is {got:?}, not {FORMAT}"),
            MediaError::Missing(key) => write!(f, "install.manifest lacks {key}"),
            MediaError::Bad(key) => write!(f, "install.manifest: {key} is not usable"),
        }
    }
}

impl std::error::Error for MediaError {}

impl InstallManifest {
    pub fn parse(text: &str) -> Result<InstallManifest, MediaError> {
        let value = |key: &'static str| -> Result<String, MediaError> {
            text.lines()
                .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .ok_or(MediaError::Missing(key))
        };
        let format = value("FORMAT")?;
        if format != FORMAT {
            return Err(MediaError::Format(format));
        }
        let rootfs_size = value("ROOTFS_SIZE")?.parse::<u64>().map_err(|_| MediaError::Bad("ROOTFS_SIZE"))?;
        let rootfs_sha256 = value("ROOTFS_SHA256")?;
        if rootfs_sha256.len() != 64 || !rootfs_sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(MediaError::Bad("ROOTFS_SHA256"));
        }
        Ok(InstallManifest {
            version: value("VERSION")?,
            build_id: value("BUILD_ID")?,
            arch: value("ARCH")?,
            engine: value("ENGINE")?,
            built: value("BUILT")?,
            kernel: value("KERNEL")?,
            rootfs: value("ROOTFS")?,
            rootfs_size,
            rootfs_sha256: rootfs_sha256.to_ascii_lowercase(),
        })
    }

    /// The manifest under a mounted medium, if it is one.
    pub fn read(medium: &Path) -> Result<InstallManifest, MediaError> {
        let text = std::fs::read_to_string(medium.join(MANIFEST_RELATIVE)).map_err(|_| MediaError::Missing("the file"))?;
        InstallManifest::parse(&text)
    }
}

/// The medium found: the device it was mounted from and its manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub device: PathBuf,
    pub manifest: InstallManifest,
}

/// Find and mount the installation medium: every candidate under
/// `dev_dir` is mounted ISO 9660 read-only on `mountpoint` in turn until one
/// holds the manifest; the others are unmounted again. A pinned device
/// (`embra.media=/dev/sr0`) is the only candidate. New devices are tried as
/// they appear (a USB stick enumerates seconds after the kernel starts),
/// every 500 ms up to `timeout`; `say` gets a line every ten seconds of
/// waiting. `None` when the timeout passes.
pub fn probe(
    sys_class_block: &Path,
    dev_dir: &Path,
    mountpoint: &Path,
    timeout: Duration,
    pin: Option<&str>,
    say: &mut dyn FnMut(&str),
) -> Option<Found> {
    let started = Instant::now();
    let mut last_word = Instant::now();
    let _ = std::fs::create_dir_all(mountpoint);
    loop {
        let names: Vec<String> = match pin {
            Some(p) => vec![p.trim_start_matches("/dev/").to_string()],
            None => candidates(sys_class_block),
        };
        for name in names {
            let dev = dev_dir.join(&name);
            if !dev.exists() || mount_iso9660(&dev, mountpoint).is_err() {
                continue;
            }
            match InstallManifest::read(mountpoint) {
                Ok(manifest) => return Some(Found { device: dev, manifest }),
                Err(_) => unmount(mountpoint),
            }
        }
        if started.elapsed() >= timeout {
            return None;
        }
        if last_word.elapsed() >= Duration::from_secs(10) {
            say(&format!("still looking for the installation medium ({} s)", started.elapsed().as_secs()));
            last_word = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn mount_iso9660(dev: &Path, mountpoint: &Path) -> std::io::Result<()> {
    let src = CString::new(dev.to_string_lossy().as_bytes()).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let dst = CString::new(mountpoint.to_string_lossy().as_bytes()).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let fstype = CString::new("iso9660").expect("a literal");
    // SAFETY: the three strings live for the call; no data string is passed.
    let rc = unsafe { libc::mount(src.as_ptr(), dst.as_ptr(), fstype.as_ptr(), libc::MS_RDONLY, std::ptr::null()) };
    if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

fn unmount(mountpoint: &Path) {
    if let Ok(dst) = CString::new(mountpoint.to_string_lossy().as_bytes()) {
        // SAFETY: the string lives for the call.
        unsafe {
            libc::umount2(dst.as_ptr(), libc::MNT_DETACH);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_sys() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("embra-boot-media-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (name, sectors) in [
            ("sda", 83886080u64), ("sda1", 2048), ("sr0", 1083392), ("loop0", 2048), ("ram0", 32768),
            ("zram0", 1024), ("dm-0", 4096), ("md127", 4096), ("nbd0", 4096), ("fd0", 2880), ("vda", 0),
        ] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
            std::fs::write(dir.join(name).join("size"), format!("{sectors}\n")).unwrap();
        }
        dir
    }

    #[test]
    fn block_candidates_skip_loop_ram_and_zram_and_keep_partitions() {
        let dir = fake_sys();
        assert_eq!(candidates(&dir), ["sda", "sda1", "sr0"]);
        assert!(candidates(&dir.join("none")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    const GOOD: &str = "FORMAT=install.v1\nVERSION=0.23.0-phase1\nBUILD_ID=6b5f5355fe\nARCH=x86_64\nENGINE=fjall\nBUILT=2026-11-08T21:28:40Z\nKERNEL=boot/bzImage\nROOTFS=embraos/rootfs.squashfs\nROOTFS_SIZE=466931712\nROOTFS_SHA256=3F0a6d1e9c2b7e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f\n";

    #[test]
    fn an_install_manifest_parses_its_rootfs_size_and_hash_and_refuses_another_format() {
        let m = InstallManifest::parse(GOOD).unwrap();
        assert_eq!(m.version, "0.23.0-phase1");
        assert_eq!(m.arch, "x86_64");
        assert_eq!(m.rootfs, "embraos/rootfs.squashfs");
        assert_eq!(m.rootfs_size, 466_931_712);
        assert!(m.rootfs_sha256.starts_with("3f0a6d1e"), "lower-cased");
        assert_eq!(InstallManifest::parse(&GOOD.replace("install.v1", "update.v1")), Err(MediaError::Format("update.v1".into())));
        assert_eq!(InstallManifest::parse(&GOOD.replace("ROOTFS_SHA256=", "X=")), Err(MediaError::Missing("ROOTFS_SHA256")));
        assert_eq!(InstallManifest::parse(&GOOD.replace("466931712", "lots")), Err(MediaError::Bad("ROOTFS_SIZE")));
        assert_eq!(InstallManifest::parse(&GOOD.replace("ROOTFS_SHA256=3F0a", "ROOTFS_SHA256=zz0a")), Err(MediaError::Bad("ROOTFS_SHA256")));
        assert_eq!(InstallManifest::parse("hello\n"), Err(MediaError::Missing("FORMAT")));
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    /// Without a candidate the probe waits out its timeout and says so,
    /// mounting nothing.
    #[test]
    fn a_probe_with_no_medium_waits_its_timeout_and_returns_none() {
        let dir = std::env::temp_dir().join(format!("embra-boot-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sys")).unwrap();
        std::fs::create_dir_all(dir.join("dev")).unwrap();
        let mut said = Vec::new();
        let started = Instant::now();
        let found = probe(&dir.join("sys"), &dir.join("dev"), &dir.join("mnt"), Duration::from_millis(600), None, &mut |l| said.push(l.to_string()));
        assert_eq!(found, None);
        assert!(started.elapsed() >= Duration::from_millis(600) && started.elapsed() < Duration::from_secs(5));
        assert!(said.is_empty(), "nothing to say inside ten seconds");
        // a pinned device that does not exist is no candidate either
        assert_eq!(probe(&dir.join("sys"), &dir.join("dev"), &dir.join("mnt"), Duration::from_millis(100), Some("/dev/sr9"), &mut |_| {}), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
