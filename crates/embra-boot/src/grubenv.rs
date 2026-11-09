//! GRUB's environment block: a 1024-byte file on the EFI System Partition
//! (`grub-editenv create` makes it) that GRUB reads with `load_env` and
//! writes in place with `save_env`. The format is the header line, then
//! `name=value` lines, then `#` up to the end. It is rewritten in place and
//! never truncated or recreated: GRUB reads the file through its block list.

use std::fmt;
use std::fs::OpenOptions;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

pub const BLOCK_LEN: usize = 1024;
pub const HEADER: &str = "# GRUB Environment Block\n";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrubEnvError {
    /// Not 1024 bytes.
    Size(usize),
    /// The header line is missing.
    Header,
    /// The variables do not fit the block (bytes they would need).
    Overflow(usize),
    /// A key with `=`, a newline, a backslash, or starting with `#`.
    InvalidKey(String),
    /// A value with a newline or a backslash (GRUB would escape them).
    InvalidValue(String),
}

impl fmt::Display for GrubEnvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GrubEnvError::Size(n) => write!(f, "a GRUB environment block is {BLOCK_LEN} bytes, this one is {n}"),
            GrubEnvError::Header => write!(f, "not a GRUB environment block (no header line)"),
            GrubEnvError::Overflow(n) => write!(f, "the variables need {n} bytes, the block holds {BLOCK_LEN}"),
            GrubEnvError::InvalidKey(k) => write!(f, "not a GRUB environment key: {k:?}"),
            GrubEnvError::InvalidValue(v) => write!(f, "not a GRUB environment value (newline or backslash): {v:?}"),
        }
    }
}

impl std::error::Error for GrubEnvError {}

/// The variables of a block, in the order they are written.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GrubEnv {
    vars: Vec<(String, String)>,
}

impl GrubEnv {
    pub fn new() -> GrubEnv {
        GrubEnv::default()
    }

    /// A block of exactly 1024 bytes with the header. Lines starting with
    /// `#` are padding or comments and are skipped, as GRUB skips them; a
    /// line without `=` is skipped the same way.
    pub fn parse(block: &[u8]) -> Result<GrubEnv, GrubEnvError> {
        if block.len() != BLOCK_LEN {
            return Err(GrubEnvError::Size(block.len()));
        }
        let text = std::str::from_utf8(block).map_err(|_| GrubEnvError::Header)?;
        let body = text.strip_prefix(HEADER).ok_or(GrubEnvError::Header)?;
        let mut vars = Vec::new();
        for line in body.split('\n') {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            if k.is_empty() {
                continue;
            }
            vars.push((k.to_string(), v.to_string()));
        }
        Ok(GrubEnv { vars })
    }

    /// The block: the header, the variables, `#` padding; an error before
    /// anything is written when they do not fit.
    pub fn to_block(&self) -> Result<[u8; BLOCK_LEN], GrubEnvError> {
        let mut text = String::from(HEADER);
        for (k, v) in &self.vars {
            text.push_str(k);
            text.push('=');
            text.push_str(v);
            text.push('\n');
        }
        if text.len() > BLOCK_LEN {
            return Err(GrubEnvError::Overflow(text.len()));
        }
        let mut block = [b'#'; BLOCK_LEN];
        block[..text.len()].copy_from_slice(text.as_bytes());
        Ok(block)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    pub fn set(&mut self, key: &str, value: &str) -> Result<(), GrubEnvError> {
        if key.is_empty() || key.starts_with('#') || key.contains(['=', '\n', '\\']) {
            return Err(GrubEnvError::InvalidKey(key.to_string()));
        }
        if value.contains(['\n', '\\']) {
            return Err(GrubEnvError::InvalidValue(value.to_string()));
        }
        match self.vars.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value.to_string(),
            None => self.vars.push((key.to_string(), value.to_string())),
        }
        Ok(())
    }

    pub fn unset(&mut self, key: &str) {
        self.vars.retain(|(k, _)| k != key);
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.vars.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn read_file(path: &Path) -> io::Result<GrubEnv> {
        let bytes = std::fs::read(path)?;
        GrubEnv::parse(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
    }

    /// A new environment block: the file is created, or replaced whole, with
    /// the 1024 bytes (the installer's grubenv on a fresh ESP; `write_file`
    /// refuses a file that is not a block already).
    pub fn write_new(&self, path: &Path) -> io::Result<()> {
        let block = self.to_block().map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let mut f = OpenOptions::new().write(true).create(true).truncate(true).open(path)?;
        f.write_all(&block)?;
        f.sync_all()
    }

    /// Rewrite the block in place: the file must exist and be 1024 bytes
    /// already (never truncated, never created here), then fsync.
    pub fn write_file(&self, path: &Path) -> io::Result<()> {
        let block = self.to_block().map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let len = std::fs::metadata(path)?.len();
        if len != BLOCK_LEN as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is {len} bytes, not a {BLOCK_LEN}-byte GRUB environment block", path.display()),
            ));
        }
        let mut f = OpenOptions::new().write(true).open(path)?;
        f.seek(SeekFrom::Start(0))?;
        f.write_all(&block)?;
        f.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_block_is_created_whole_and_rereads() {
        let dir = std::env::temp_dir().join(format!("embra-boot-grubenv-new-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("grubenv");
        let mut env = GrubEnv::new();
        env.set("slot_active", "a").unwrap();
        env.set("menu_timeout", "3").unwrap();
        env.write_new(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), BLOCK_LEN as u64);
        let back = GrubEnv::read_file(&path).unwrap();
        assert_eq!(back.get("slot_active"), Some("a"));
        assert_eq!(back.get("menu_timeout"), Some("3"));
        GrubEnv::new().write_new(&path).unwrap();
        assert_eq!(GrubEnv::read_file(&path).unwrap().get("slot_active"), None, "written again whole: no stale tail");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fresh_block_is_the_header_and_hash_padding_to_1024_bytes() {
        let block = GrubEnv::new().to_block().unwrap();
        assert_eq!(block.len(), BLOCK_LEN);
        assert!(block.starts_with(HEADER.as_bytes()));
        assert!(block[HEADER.len()..].iter().all(|&b| b == b'#'));
        assert_eq!(GrubEnv::parse(&block).unwrap(), GrubEnv::new());
    }

    #[test]
    fn variables_round_trip_through_the_block_in_order() {
        let mut env = GrubEnv::new();
        env.set("slot_active", "a").unwrap();
        env.set("slot_try", "").unwrap();
        env.set("tries_left", "3").unwrap();
        env.set("menu_timeout", "3").unwrap();
        env.set("slot_active", "b").unwrap();
        let block = env.to_block().unwrap();
        let back = GrubEnv::parse(&block).unwrap();
        assert_eq!(back, env);
        let keys: Vec<&str> = back.iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["slot_active", "slot_try", "tries_left", "menu_timeout"]);
        assert_eq!(back.get("slot_active"), Some("b"));
        assert_eq!(back.get("slot_try"), Some(""));
        assert_eq!(back.get("nothing"), None);
        env.unset("menu_timeout");
        assert_eq!(env.get("menu_timeout"), None);
        // What grub-editenv writes parses the same way.
        let mut editenv = [b'#'; BLOCK_LEN];
        let text = "# GRUB Environment Block\nslot_active=a\ntries_left=0\n";
        editenv[..text.len()].copy_from_slice(text.as_bytes());
        let parsed = GrubEnv::parse(&editenv).unwrap();
        assert_eq!(parsed.get("slot_active"), Some("a"));
        assert_eq!(parsed.get("tries_left"), Some("0"));
    }

    #[test]
    fn a_value_that_overflows_the_block_is_refused_before_anything_is_written() {
        let mut env = GrubEnv::new();
        env.set("big", &"x".repeat(BLOCK_LEN)).unwrap();
        assert!(matches!(env.to_block(), Err(GrubEnvError::Overflow(_))));
        let tmp = std::env::temp_dir().join(format!("embra-boot-grubenv-overflow-{}", std::process::id()));
        std::fs::write(&tmp, GrubEnv::new().to_block().unwrap()).unwrap();
        assert!(env.write_file(&tmp).is_err());
        assert_eq!(GrubEnv::read_file(&tmp).unwrap(), GrubEnv::new(), "the file is untouched");
        std::fs::remove_file(&tmp).unwrap();
    }

    #[test]
    fn a_block_of_another_size_or_without_the_header_is_refused() {
        assert_eq!(GrubEnv::parse(&[b'#'; 512]), Err(GrubEnvError::Size(512)));
        assert_eq!(GrubEnv::parse(&[b'#'; BLOCK_LEN]), Err(GrubEnvError::Header));
        let tmp = std::env::temp_dir().join(format!("embra-boot-grubenv-short-{}", std::process::id()));
        std::fs::write(&tmp, b"# GRUB Environment Block\n").unwrap();
        assert!(GrubEnv::read_file(&tmp).is_err());
        assert!(GrubEnv::new().write_file(&tmp).is_err(), "a short file is never grown into a block");
        std::fs::remove_file(&tmp).unwrap();
        assert!(GrubEnv::new().write_file(Path::new("/nonexistent/grubenv")).is_err());
    }

    #[test]
    fn a_key_or_value_with_a_newline_is_refused() {
        let mut env = GrubEnv::new();
        assert_eq!(env.set("a\nb", "1"), Err(GrubEnvError::InvalidKey("a\nb".into())));
        assert_eq!(env.set("a=b", "1"), Err(GrubEnvError::InvalidKey("a=b".into())));
        assert_eq!(env.set("#a", "1"), Err(GrubEnvError::InvalidKey("#a".into())));
        assert_eq!(env.set("", "1"), Err(GrubEnvError::InvalidKey(String::new())));
        assert_eq!(env.set("a", "1\n2"), Err(GrubEnvError::InvalidValue("1\n2".into())));
        assert_eq!(env.set("a", "1\\2"), Err(GrubEnvError::InvalidValue("1\\2".into())));
        assert!(env.iter().next().is_none());
    }

    #[test]
    fn a_file_is_rewritten_in_place_and_stays_1024_bytes() {
        let tmp = std::env::temp_dir().join(format!("embra-boot-grubenv-rw-{}", std::process::id()));
        std::fs::write(&tmp, GrubEnv::new().to_block().unwrap()).unwrap();
        let mut env = GrubEnv::read_file(&tmp).unwrap();
        env.set("slot_active", "b").unwrap();
        env.write_file(&tmp).unwrap();
        assert_eq!(std::fs::metadata(&tmp).unwrap().len(), BLOCK_LEN as u64);
        assert_eq!(GrubEnv::read_file(&tmp).unwrap().get("slot_active"), Some("b"));
        std::fs::remove_file(&tmp).unwrap();
    }
}
