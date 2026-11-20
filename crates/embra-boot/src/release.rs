//! The release stamp a slot carries, `/etc/embraos-release`, written by the
//! image build (`scripts/lib/image-common.sh::write_release_file`): the
//! workspace version, the commit, the architecture, the storage engine baked
//! into embrad, and when it was built.

use std::fmt;
use std::io;
use std::path::Path;

pub const PATH: &str = "/etc/embraos-release";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub build_id: String,
    pub arch: String,
    pub engine: String,
    pub built: String,
}

#[derive(Debug)]
pub enum ReleaseError {
    MissingKey(&'static str),
    Io(io::Error),
}

impl fmt::Display for ReleaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReleaseError::MissingKey(k) => write!(f, "the release file lacks {k}"),
            ReleaseError::Io(e) => write!(f, "the release file could not be read: {e}"),
        }
    }
}

impl std::error::Error for ReleaseError {}

impl From<io::Error> for ReleaseError {
    fn from(e: io::Error) -> Self {
        ReleaseError::Io(e)
    }
}

impl Release {
    /// `KEY=VALUE` lines; blank lines and `#` comments are skipped; a value
    /// may be quoted. The five keys are required.
    pub fn parse(text: &str) -> Result<Release, ReleaseError> {
        let get = |key: &'static str| -> Result<String, ReleaseError> {
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .filter_map(|l| l.split_once('='))
                .filter(|(k, _)| k.trim() == key)
                .map(|(_, v)| v.trim().trim_matches('"').to_string())
                .rfind(|v| !v.is_empty())
                .ok_or(ReleaseError::MissingKey(key))
        };
        Ok(Release {
            version: get("VERSION")?,
            build_id: get("BUILD_ID")?,
            arch: get("ARCH")?,
            engine: get("ENGINE")?,
            built: get("BUILT")?,
        })
    }

    pub fn read() -> Result<Release, ReleaseError> {
        Release::read_from(Path::new(PATH))
    }

    pub fn read_from(path: &Path) -> Result<Release, ReleaseError> {
        Release::parse(&std::fs::read_to_string(path)?)
    }

    /// When the image was built, in seconds after the epoch: the earliest
    /// the clock can truthfully read while this release runs (embrad's
    /// floor, `clock.rs`). `None` when `BUILT` is not an RFC 3339 time.
    pub fn built_unix(&self) -> Option<i64> {
        crate::clock::parse_rfc3339(&self.built)
    }
}

impl fmt::Display for Release {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}, {}, {}, built {})", self.version, self.build_id, self.arch, self.engine, self.built)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_release_file_parses_its_five_keys_and_a_missing_one_is_an_error() {
        let text = "VERSION=0.24.0-phase2\nBUILD_ID=2cb4d14abc-dirty\nARCH=x86_64\nENGINE=\"fjall\"\n# when\nBUILT=2026-11-05T10:12:00Z\n";
        let r = Release::parse(text).unwrap();
        assert_eq!(r.version, "0.24.0-phase2");
        assert_eq!(r.build_id, "2cb4d14abc-dirty");
        assert_eq!(r.arch, "x86_64");
        assert_eq!(r.engine, "fjall");
        assert_eq!(r.built, "2026-11-05T10:12:00Z");
        assert_eq!(r.built_unix(), Some(1_793_873_520));
        assert_eq!(r.to_string(), "0.24.0-phase2 (2cb4d14abc-dirty, x86_64, fjall, built 2026-11-05T10:12:00Z)");
        let err = Release::parse("VERSION=1\nBUILD_ID=x\nARCH=aarch64\nBUILT=now\n").unwrap_err();
        assert!(matches!(err, ReleaseError::MissingKey("ENGINE")));
        let undated = Release::parse("VERSION=1\nBUILD_ID=x\nARCH=aarch64\nENGINE=fjall\nBUILT=now\n").unwrap();
        assert_eq!(undated.built_unix(), None, "a stamp that is no time gives no floor");
        assert!(err.to_string().contains("ENGINE"));
        assert!(matches!(Release::read_from(Path::new("/nonexistent/embraos-release")), Err(ReleaseError::Io(_))));
    }

    /// What a build leaves of its host in what it produces
    /// (`scripts/lib/build-paths.sh`), run on stand-in directories. rustc is
    /// handed a stand-in for every directory of the build host, the home
    /// first (it applies the last prefix that matches); the read-back stops
    /// on a program that names the builder's home and passes one that names
    /// a neighbour's; a root filesystem loses the records of its build and
    /// nothing else. Both image scripts hand the stand-ins over before the
    /// first cargo runs and read the programs back after the last.
    #[cfg(unix)]
    #[test]
    fn a_build_hands_rustc_a_stand_in_for_each_of_its_hosts_directories_and_reads_the_result_back() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let script = repo.join("scripts/lib/build-paths.sh");
        let root = std::env::temp_dir().join(format!("embra-build-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // As the kernel names it: a directory reached through a link is
        // listed under both names, and a temporary directory can be one.
        let root = root.canonicalize().unwrap();
        let (home, checkout) = (root.join("home/ann"), root.join("home/ann/projects/embraOS"));
        std::fs::create_dir_all(&checkout).unwrap();
        let text = |p: &Path| p.to_str().unwrap().to_string();
        let run = |env: &[(&str, &str)], args: &[&str]| {
            let out = Command::new("bash").arg(&script).args(args).current_dir(&root).env_clear().env("PATH", std::env::var_os("PATH").unwrap_or_default()).envs(env.iter().copied()).output().expect("bash runs the script");
            (out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
        };
        let (h, c) = (text(&home), text(&checkout));

        // The stand-ins, behind the builder's own flags, the home first.
        let (code, flags, _) = run(&[("HOME", &h), ("RUSTFLAGS", "-D warnings")], &["export_builder_path_remaps", &c]);
        assert_eq!(code, Some(0));
        assert_eq!(
            flags,
            format!("-D\nwarnings\n--remap-path-prefix={h}=/builder\n--remap-path-prefix={c}=/embraOS\n--remap-path-prefix={h}/.cargo=/cargo\n--remap-path-prefix={h}/.rustup=/rustup\n")
        );
        // What the read-back looks for: the home alone here; in a container
        // neither /root nor /work, which name nobody.
        assert_eq!(run(&[("HOME", &h)], &["builder_needles", &c]).1, format!("{h}/\n"));
        assert_eq!(run(&[("HOME", "/root")], &["builder_needles", "/work"]).1, "/root/.cargo/\n/root/.rustup/\n");

        // The read-back: two programs and a bundle that carry the stand-ins.
        let (release, bundle) = (root.join("release"), root.join("bundle"));
        std::fs::create_dir_all(release.join("deps")).unwrap();
        std::fs::create_dir_all(&bundle).unwrap();
        let program = |name: &str, body: &str| {
            let file = release.join(name);
            std::fs::write(&file, format!("\u{7f}ELF\0{body}\0")).unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        program("embrad", "/cargo/registry/src/tonic-0.14.9/src/codec/decode.rs");
        // a neighbour whose login begins with the builder's
        program("embra-web", &format!("{h}a/.cargo/registry/src/x-1.0/src/lib.rs"));
        // cargo's own records beside the programs name the directories and are not programs
        std::fs::write(release.join("embrad.d"), format!("{h}/.cargo/registry/src/x-1.0/src/lib.rs\n")).unwrap();
        std::fs::write(release.join("deps/libx.rlib"), format!("{h}/.cargo/registry/src/x-1.0/src/lib.rs\n")).unwrap();
        std::fs::write(bundle.join("app.wasm"), "\0asm\0/embraOS/crates/embra-tools-core/src/mask.rs\0").unwrap();
        let read_back = || run(&[("HOME", &h)], &["assert_programs_name_no_builder_dir", &c, "release", "bundle"]);
        let (code, said, _) = read_back();
        assert_eq!((code, said.as_str()), (Some(0), "build paths: none of this host's directories in the 2 programs under release or in the web bundle\n"));
        // A program built without them stops the build and is named with its text.
        program("embra-brain", &format!("{h}/.cargo/registry/src/tonic-0.14.9/src/codec/decode.rs:290"));
        let (code, _, refusal) = read_back();
        assert_eq!(code, Some(1));
        assert!(refusal.contains(&format!("       release/embra-brain\n         {h}/.cargo/registry/src/tonic-0.14.9/src/codec/decode.rs:290\n")), "{refusal}");
        assert!(!refusal.contains("release/embra-web") && !refusal.contains("release/embrad"), "{refusal}");
        std::fs::remove_file(release.join("embra-brain")).unwrap();
        // So does a bundle that names the checkout.
        std::fs::write(bundle.join("app.wasm"), format!("\0asm\0{c}/crates/embra-tools-core/src/mask.rs\0")).unwrap();
        let (code, _, refusal) = read_back();
        assert_eq!(code, Some(1));
        assert!(refusal.contains("       bundle/app.wasm\n"), "{refusal}");

        // The records of a build go from a root filesystem, and nothing else.
        let rootfs = root.join("rootfs");
        let kept = ["opt/rust/RUST_VERSION", "opt/rust/lib/rustlib/components", "usr/bin/sqlite3", "usr/lib/libicuuc.so.78.2", "usr/lib/libicuio.so.78.2", "usr/lib/libstdc++.so.6.0.33", "usr/lib/libxslt.so.1.1.43"];
        let records = [
            "opt/rust/lib/rustlib/install.log",
            "opt/rust/lib/rustlib/uninstall.sh",
            "opt/rust/lib/rustlib/manifest-rustc",
            "opt/rust/lib/rustlib/manifest-rust-std-wasm32-unknown-unknown",
            "usr/lib/icu/78.2/Makefile.inc",
            "usr/lib/icu/78.2/pkgdata.inc",
            "usr/bin/icuexportdata",
            "usr/bin/pkgdata",
            "usr/lib/libicutu.so.78.2",
            "usr/lib/libicutest.so.78.2",
            "usr/lib/xsltConf.sh",
            "usr/lib/libstdc++.so.6.0.33-gdb.py",
        ];
        for file in kept.iter().chain(&records) {
            let file = rootfs.join(file);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, "x").unwrap();
        }
        assert_eq!(run(&[], &["remove_build_records", "rootfs"]).0, Some(0));
        for file in kept {
            assert!(rootfs.join(file).exists(), "{file} stays");
        }
        for file in records {
            assert!(!rootfs.join(file).exists(), "{file} goes");
        }
        assert!(!rootfs.join("usr/lib/icu").exists());
        assert_eq!(run(&[], &["remove_build_records", "rootfs"]).0, Some(0), "a second run over a clean tree succeeds");
        let _ = std::fs::remove_dir_all(&root);

        // Both image scripts hand the stand-ins over before the first cargo
        // runs (trunk's) and read the programs back after the cross-build,
        // and post_build.sh removes the records.
        for name in ["scripts/build-image.sh", "scripts/build-image-aarch64.sh"] {
            let text = std::fs::read_to_string(repo.join(name)).unwrap();
            let at = |needle: &str| text.find(needle).unwrap_or_else(|| panic!("{name}: no `{needle}`"));
            let (remaps, trunk, cargo, read) = (at("\n    export_builder_path_remaps "), at("trunk build --release --locked)"), at("\n    cargo build --release --locked --target "), at("\n    assert_programs_name_no_builder_dir "));
            assert!(remaps < trunk && trunk < cargo && cargo < read, "{name}: the stand-ins, trunk, the cross-build, the read-back, in that order");
            assert_eq!(text.matches("\n    cargo build ").count(), 1, "{name}: one cross-build, the one that is read back");
        }
        let post_build = std::fs::read_to_string(repo.join("buildroot/board/embraos/post_build.sh")).unwrap();
        assert!(post_build.contains("\nremove_build_records \"${TARGET_DIR}\"\n"));
    }
}
