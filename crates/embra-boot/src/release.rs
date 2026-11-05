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
        assert_eq!(r.to_string(), "0.24.0-phase2 (2cb4d14abc-dirty, x86_64, fjall, built 2026-11-05T10:12:00Z)");
        let err = Release::parse("VERSION=1\nBUILD_ID=x\nARCH=aarch64\nBUILT=now\n").unwrap_err();
        assert!(matches!(err, ReleaseError::MissingKey("ENGINE")));
        assert!(err.to_string().contains("ENGINE"));
        assert!(matches!(Release::read_from(Path::new("/nonexistent/embraos-release")), Err(ReleaseError::Io(_))));
    }
}
