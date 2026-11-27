//! The EFI System Partition, mounted only while its environment block is
//! read or written, or its boot menu is: embrad confirms a trial boot
//! there, embra-update stages a slot and rolls back there, and puts the
//! default slot's menu there. One helper, one lock (`flock` on a file under
//! `/run`), so the writers never interleave, and the partition is unmounted
//! the rest of the time.

use crate::grubenv::GrubEnv;
use std::io;
use std::path::{Path, PathBuf};

pub const MOUNT_POINT: &str = "/run/embra/esp";
pub const LOCK_PATH: &str = "/run/embra/esp.lock";
/// `grubenv` beside the loader and `grub.cfg`, relative to the mount.
pub const GRUBENV_RELATIVE: &str = "EFI/BOOT/grubenv";

/// The boot menu, `grub.cfg`, beside the loader and `grubenv`, relative to
/// the mount.
pub const MENU_RELATIVE: &str = "EFI/BOOT/grub.cfg";
/// Where a slot carries its release's boot menu
/// (`buildroot/board/embraos/post_build.sh` renders it there): the file
/// the image's ESP is given, and the one `embra-update boot-menu` puts on
/// the ESP of a machine once the slot is its confirmed default.
pub const SLOT_MENU_PATH: &str = "/usr/share/embra/grub.cfg";

pub fn grubenv_path() -> PathBuf {
    Path::new(MOUNT_POINT).join(GRUBENV_RELATIVE)
}

/// Run `f` on the mounted partition (its mount point), read-only or not,
/// under the lock; the partition is unmounted when `f` returns.
#[cfg(target_os = "linux")]
pub fn with_mounted<T>(dev: &Path, read_only: bool, f: impl FnOnce(&Path) -> T) -> io::Result<T> {
    let _mounted = Mounted::mount(dev, read_only)?;
    Ok(f(Path::new(MOUNT_POINT)))
}

#[cfg(not(target_os = "linux"))]
pub fn with_mounted<T>(_dev: &Path, _read_only: bool, _f: impl FnOnce(&Path) -> T) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "the ESP is mounted on Linux only"))
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
        let mut ran = false;
        let err = with_mounted(Path::new("/dev/null"), true, |_| ran = true).unwrap_err();
        assert!(!err.to_string().is_empty());
        assert!(!ran, "nothing runs on a partition that was not mounted");
    }

    /// An OS update puts a slot's menu on the ESP and leaves GRUB's loader,
    /// which is the installer's: a menu must run on every loader an
    /// installer has written. They all carry this list of built-in modules,
    /// which has not moved since the first image with GRUB on its ESP. A
    /// module added here is in no loader that is installed already: the menu
    /// must not need it, or the update has to learn to bring the loader too
    /// before the list moves.
    #[test]
    fn the_loader_every_installed_disk_has_is_the_one_the_menu_is_written_for() {
        const MODULES: &str = "boot linux normal configfile part_gpt fat squash4 xzio gzio echo test true sleep loadenv regexp smbios probe search search_fs_uuid search_label halt reboot efifwsetup minicmd cat ls help terminal efi_gop";
        for config in ["embraos_x86_64_defconfig", "embraos_aarch64_defconfig"] {
            let text = std::fs::read_to_string(format!("{}/../../buildroot/configs/{config}", env!("CARGO_MANIFEST_DIR"))).unwrap();
            let lines: Vec<&str> = text.lines().filter(|l| l.starts_with("BR2_TARGET_GRUB2_BUILTIN_MODULES_EFI=")).collect();
            assert_eq!(lines, [format!("BR2_TARGET_GRUB2_BUILTIN_MODULES_EFI=\"{MODULES}\"").as_str()], "{config}");
        }
        // What the menu asks of GRUB beyond the script language is in those modules.
        let menu = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../buildroot/board/embraos/grub.cfg.in")).unwrap();
        let has = |module: &str| MODULES.split(' ').any(|m| m == module);
        for (command, module) in [
            ("load_env ", "loadenv"),
            ("save_env ", "loadenv"),
            ("regexp ", "regexp"),
            ("smbios ", "smbios"),
            ("probe ", "probe"),
            ("source ", "configfile"),
            ("configfile ", "configfile"),
            ("echo ", "echo"),
            ("sleep ", "sleep"),
            ("linux ", "linux"),
            ("initrd ", "linux"),
            ("fwsetup", "efifwsetup"),
            ("halt", "halt"),
            ("reboot", "reboot"),
        ] {
            assert!(menu.lines().any(|l| !l.trim_start().starts_with('#') && l.contains(command)), "the menu no longer uses {command:?}");
            assert!(has(module), "{command:?} needs the module {module}");
        }
    }

    /// The build's two functions for the menu, run as the image scripts run
    /// them. The menu is rendered once, with the kernel's name and the
    /// serial console of the architecture, and a template that keeps a
    /// placeholder, a value that is neither architecture's, a kernel and a
    /// console of two architectures, or a configuration that cannot be read,
    /// is refused.
    /// The image check holds the slot's copy, the ESP's and the installer
    /// medium's to one file, and a build where they differ loses its image.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_build_renders_the_menu_once_and_holds_its_three_copies_to_one_file() {
        use std::process::Command;
        let board = concat!(env!("CARGO_MANIFEST_DIR"), "/../../buildroot/board/embraos");
        let common = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/lib/image-common.sh");
        let dir = std::env::temp_dir().join(format!("embra-boot-menu-build-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("bash").arg(format!("{board}/boot-menu.sh")).args(args).output().unwrap();
            (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
        };
        let template = format!("{board}/grub.cfg.in");

        // The serial console follows the architecture of Buildroot's configuration.
        std::fs::write(dir.join("arm.config"), "BR2_arm=n\nBR2_aarch64=y\n").unwrap();
        std::fs::write(dir.join("x86.config"), "BR2_x86_64=y\n# BR2_aarch64 is not set\n").unwrap();
        assert_eq!(run(&["embraos_serial_console", dir.join("arm.config").to_str().unwrap()]).1.trim(), "ttyAMA0");
        assert_eq!(run(&["embraos_serial_console", dir.join("x86.config").to_str().unwrap()]).1.trim(), "ttyS0");
        // A configuration that cannot be read has no answer: the x86 console is no default.
        let (ok, answer, err) = run(&["embraos_serial_console", dir.join("absent.config").to_str().unwrap()]);
        assert!(!ok && answer.trim().is_empty() && err.contains("cannot be read"), "{answer:?} {err}");

        // Both architectures' menus: the kernel line names the image and the console, and no placeholder is left.
        for (kernel, console) in [("bzImage", "ttyS0"), ("Image", "ttyAMA0")] {
            let out = dir.join(format!("slot-{kernel}/usr/share/embra/grub.cfg"));
            let (ok, _, err) = run(&["embraos_render_boot_menu", &template, kernel, console, out.to_str().unwrap()]);
            assert!(ok, "{kernel}: {err}");
            let menu = std::fs::read_to_string(&out).unwrap();
            assert!(!menu.contains('@'), "{kernel}: a placeholder is left");
            assert!(menu.contains(&format!("linux \"$2/boot/{kernel}\" console=tty0 console={console} quiet ")), "{kernel}");
            assert!(menu.contains("probe --set=esp_uuid --part-uuid \"$root\""), "{kernel}");
            assert_eq!(menu.lines().count(), std::fs::read_to_string(&template).unwrap().lines().count(), "{kernel}: the template line for line");
        }
        // A value of neither architecture, a kernel and a console of two, a template that is not there,
        // one with another placeholder.
        let out = dir.join("refused.cfg");
        for (args, said) in [
            (vec!["embraos_render_boot_menu", template.as_str(), "vmlinuz", "ttyS0", out.to_str().unwrap()], "no kernel image name"),
            (vec!["embraos_render_boot_menu", template.as_str(), "bzImage", "ttyUSB0", out.to_str().unwrap()], "no serial console"),
            (vec!["embraos_render_boot_menu", template.as_str(), "bzImage", "ttyAMA0", out.to_str().unwrap()], "not one architecture's"),
            (vec!["embraos_render_boot_menu", template.as_str(), "Image", "ttyS0", out.to_str().unwrap()], "not one architecture's"),
            (vec!["embraos_render_boot_menu", dir.join("absent.in").to_str().unwrap(), "bzImage", "ttyS0", out.to_str().unwrap()], "no template"),
        ] {
            let (ok, _, err) = run(&args);
            assert!(!ok && err.contains(said), "{args:?}: {err}");
        }
        std::fs::write(dir.join("more.in"), "linux /boot/@KERNEL_IMAGE@ console=@SERIAL_CONSOLE@ @ANOTHER@\n").unwrap();
        let (ok, _, err) = run(&["embraos_render_boot_menu", dir.join("more.in").to_str().unwrap(), "bzImage", "ttyS0", out.to_str().unwrap()]);
        assert!(!ok && err.contains("was not rendered") && err.contains("@ANOTHER@"), "{err}");
        // A checker that refuses the result refuses the menu.
        let checker = dir.join("refusing-checker");
        std::fs::write(&checker, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&checker, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let (ok, _, err) = run(&["embraos_render_boot_menu", &template, "bzImage", "ttyS0", out.to_str().unwrap(), checker.to_str().unwrap()]);
        assert!(!ok && err.contains("grub-script-check refused the boot menu"), "{err}");
        let (ok, _, err) = run(&["embraos_render_boot_menu", &template, "bzImage", "ttyS0", out.to_str().unwrap(), dir.join("no-checker").to_str().unwrap()]);
        assert!(!ok && err.contains("is missing"), "{err}");

        // The image check, on a stand-in for a Buildroot output: the slot's copy in the target
        // directory, the tree the installer medium takes, the image, and the ESP's menu read out of it.
        let menu = std::fs::read(dir.join("slot-bzImage/usr/share/embra/grub.cfg")).unwrap();
        let tree = |name: &str, slot: Option<&[u8]>, staged: &[u8], esp: &[u8]| {
            let root = dir.join(name);
            let (target, images) = (root.join("output/target/usr/share/embra"), root.join("output/images"));
            std::fs::create_dir_all(&target).unwrap();
            std::fs::create_dir_all(images.join("efi-part/EFI/BOOT")).unwrap();
            if let Some(slot) = slot {
                std::fs::write(target.join("grub.cfg"), slot).unwrap();
            }
            std::fs::write(images.join("efi-part/EFI/BOOT/grub.cfg"), staged).unwrap();
            std::fs::write(images.join("embraos.img"), b"the image this build wrote").unwrap();
            std::fs::write(root.join("esp-grub.cfg"), esp).unwrap();
            root
        };
        let check = |root: &std::path::Path| {
            let out = Command::new("bash")
                .args(["-c", r#"set -euo pipefail; BUILDROOT_DIR="$1"; source "$2"; assert_slot_boot_menu "$3""#, "_"])
                .arg(root)
                .arg(common)
                .arg(root.join("esp-grub.cfg"))
                .output()
                .unwrap();
            (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned(), root.join("output/images/embraos.img").exists())
        };
        let (ok, said, err, image) = check(&tree("same", Some(&menu), &menu, &menu));
        assert!(ok && image, "{err}");
        assert!(said.contains("the slot's /usr/share/embra/grub.cfg is the ESP's and the installer medium's"), "{said}");
        let other = [menu.as_slice(), b"# one more line\n"].concat();
        for (name, slot, staged, esp, why) in [
            ("no-slot-copy", None, menu.as_slice(), menu.as_slice(), "carries no boot menu at /usr/share/embra/grub.cfg"),
            ("esp-differs", Some(menu.as_slice()), menu.as_slice(), other.as_slice(), "the boot menu on the image's ESP is not the slot's"),
            ("esp-unread", Some(menu.as_slice()), menu.as_slice(), b"".as_slice(), "could not be read from the image's ESP"),
            ("medium-differs", Some(menu.as_slice()), other.as_slice(), menu.as_slice(), "the boot menu the installer medium takes"),
            ("slot-differs", Some(other.as_slice()), menu.as_slice(), menu.as_slice(), "the boot menu on the image's ESP is not the slot's"),
        ] {
            let (ok, _, err, image) = check(&tree(name, slot, staged, esp));
            assert!(!ok, "{name} passed");
            assert!(err.contains(why), "{name}: {err}");
            assert!(!image, "{name}: the image this build wrote is left");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The menu is beside the block on the ESP, and a slot carries its own
    /// where the image build puts it.
    #[test]
    fn the_menu_lives_beside_the_block_and_a_slot_carries_its_own() {
        assert_eq!(Path::new(MENU_RELATIVE).parent(), Path::new(GRUBENV_RELATIVE).parent());
        assert_eq!(SLOT_MENU_PATH, "/usr/share/embra/grub.cfg");
        let board = concat!(env!("CARGO_MANIFEST_DIR"), "/../../buildroot/board/embraos");
        let post_build = std::fs::read_to_string(format!("{board}/post_build.sh")).unwrap();
        assert!(
            post_build.contains(&format!("\"${{TARGET_DIR}}{SLOT_MENU_PATH}\" \"${{HOST_DIR}}/bin/grub-script-check\"")),
            "post_build.sh renders the menu into the slot, linted"
        );
        // The console is the configuration's, taken in a statement of its own: a failure stops the build there.
        assert!(post_build.contains("SLOT_CONSOLE=\"$(embraos_serial_console \"${BR2_CONFIG}\")\"\n"), "post_build.sh takes the console from Buildroot's configuration");
        // The ESP is given the slot's file: the template is rendered once, by post_build.sh.
        let post_image = std::fs::read_to_string(format!("{board}/post_image.sh")).unwrap();
        assert!(post_image.contains(&format!("SLOT_MENU=\"${{TARGET_DIR}}{SLOT_MENU_PATH}\"")));
        assert!(post_image.contains("install -m 0644 \"${SLOT_MENU}\" \"${EFI_PART}/EFI/BOOT/grub.cfg\""));
        for script in [&post_build, &post_image] {
            let code: Vec<&str> = script.lines().map(str::trim).filter(|l| !l.starts_with('#')).collect();
            assert!(!code.iter().any(|l| l.contains("sed ") && l.contains("grub.cfg.in")), "the template is rendered by boot-menu.sh alone");
        }
        // The image build holds the three copies to one file.
        let common = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/lib/image-common.sh")).unwrap();
        assert!(common.contains("assert_slot_boot_menu \"$esp_menu\""), "assert_image_layout reads the ESP's menu out and compares it");
        assert!(common.contains(&format!("local slot_menu=\"$BUILDROOT_DIR/output/target{SLOT_MENU_PATH}\"")));
    }
}
