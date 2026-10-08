//! Text uploads: the files an operator attaches that are not images.
//!
//! A text upload is a plain workspace file under `/embra/workspace/uploads/`,
//! named after the upload (its basename, sanitized, numbered on a
//! collision). It has no sidecar and no id: the path is the handle, the
//! model reads it with `file_read` like any other file. What counts
//! as text: valid UTF-8 with no NUL byte, up to [`TEXT_UPLOAD_MAX`].
//! Anything else is refused with the reason.

use std::path::PathBuf;

use embra_common::proto::brain::FileRef;

use super::store::sanitize_name;

/// Production uploads directory, inside the write jail (`MEDIA_DIR`
/// precedent).
pub const UPLOADS_DIR: &str = "/embra/workspace/uploads";
/// Dev/test override for the uploads directory (exclusive when set).
pub const UPLOADS_DIR_ENV: &str = "EMBRA_UPLOADS_DIR";
/// Largest text upload accepted (raw bytes).
pub const TEXT_UPLOAD_MAX: usize = 2 * 1024 * 1024;
/// Name used when the upload names nothing usable.
pub const FALLBACK_NAME: &str = "upload.txt";
/// Numbered names tried on a collision before giving up.
const SUFFIX_MAX: u32 = 1000;

#[derive(Debug, thiserror::Error)]
pub enum TextError {
    #[error("not UTF-8 text (a NUL byte or invalid UTF-8)")]
    NotText,
    #[error("text file is {0} bytes; the limit is {1} bytes")]
    TooLarge(usize, usize),
    #[error("uploads I/O error: {0}")]
    Io(String),
}

/// Text or not: under the cap, valid UTF-8, no NUL byte anywhere. The
/// whole body is checked; `file_read` stops at the first kilobyte because
/// it reads windows, an upload is read once.
pub fn classify_text(bytes: &[u8]) -> Result<(), TextError> {
    if bytes.len() > TEXT_UPLOAD_MAX {
        return Err(TextError::TooLarge(bytes.len(), TEXT_UPLOAD_MAX));
    }
    if bytes.contains(&0) || std::str::from_utf8(bytes).is_err() {
        return Err(TextError::NotText);
    }
    Ok(())
}

/// Media type by extension; `text/plain` for everything unnamed.
pub fn media_type_for_name(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "md" | "markdown" => "text/markdown",
        "json" => "application/json",
        "csv" => "text/csv",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        _ => "text/plain",
    }
}

/// One stored text upload: what the frame, the staging and the persisted
/// ref carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextUpload {
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub media_type: String,
}

/// Where text uploads land.
#[derive(Debug, Clone)]
pub struct UploadsDir {
    dir: PathBuf,
}

impl UploadsDir {
    /// The production directory (or the `EMBRA_UPLOADS_DIR` override).
    pub fn default_dir() -> Self {
        let dir = std::env::var(UPLOADS_DIR_ENV)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(UPLOADS_DIR));
        Self::at(dir)
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Store a text upload under its name. The name is reduced to a
    /// basename without control characters; `.` and `..` fall back to
    /// [`FALLBACK_NAME`], so the file always lands inside the directory.
    /// A name already taken gets `-2`, `-3`, … before its extension. The
    /// bytes are written atomically; between the free-name probe and the
    /// write another upload could take the name, which this accepts.
    pub async fn store_text(&self, name: &str, bytes: &[u8]) -> Result<TextUpload, TextError> {
        classify_text(bytes)?;
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|e| TextError::Io(format!("create {}: {e}", self.dir.display())))?;
        let base = safe_basename(name);
        let (stem, ext) = split_extension(&base);
        for n in 1..=SUFFIX_MAX {
            let candidate = if n == 1 { base.clone() } else { format!("{stem}-{n}{ext}") };
            let path = self.dir.join(&candidate);
            if tokio::fs::metadata(&path).await.is_ok() {
                continue;
            }
            crate::tools::file_patch::write_atomic_create(&path, bytes)
                .await
                .map_err(TextError::Io)?;
            return Ok(TextUpload {
                media_type: media_type_for_name(&candidate).to_string(),
                name: candidate,
                path,
                bytes: bytes.len() as u64,
            });
        }
        Err(TextError::Io(format!(
            "{} names taken for '{}' in {}",
            SUFFIX_MAX,
            base,
            self.dir.display()
        )))
    }
}

/// `sanitize_name` plus the two names a basename can carry that would
/// leave the directory.
fn safe_basename(name: &str) -> String {
    let base = sanitize_name(name, FALLBACK_NAME);
    if base == "." || base == ".." {
        FALLBACK_NAME.to_string()
    } else {
        base
    }
}

/// `notes.md` → (`notes`, `.md`); `README` → (`README`, ``); a leading dot
/// is part of the stem (`.env` → (`.env`, ``)).
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// The operator-facing frame for a text upload.
pub fn file_ref_frame(u: &TextUpload, origin: &str, replay: bool, tool_use_id: &str, caption: &str) -> FileRef {
    FileRef {
        path: u.path.display().to_string(),
        name: u.name.clone(),
        byte_size: u.bytes,
        media_type: u.media_type.clone(),
        origin: origin.to_string(),
        caption: caption.to_string(),
        tool_use_id: tool_use_id.to_string(),
        replay,
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal self-contained temp dir (no tempfile dep in the tree).
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "embra-uploads-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_utf8_body_without_nul_is_text_and_a_nul_byte_or_bad_utf8_is_not() {
        assert!(classify_text("# Notes\n\nplain text, ümlauts, 日本語\n".as_bytes()).is_ok());
        assert!(classify_text(b"").is_ok());
        assert!(matches!(classify_text(b"abc\0def"), Err(TextError::NotText)));
        assert!(matches!(classify_text(&[0x89, b'P', b'N', b'G', 0xff]), Err(TextError::NotText)));
        assert!(matches!(classify_text(&[0xc3, 0x28]), Err(TextError::NotText)));
    }

    #[test]
    fn a_text_upload_over_the_cap_is_refused_and_the_cap_is_pinned() {
        assert_eq!(TEXT_UPLOAD_MAX, 2 * 1024 * 1024);
        let over = vec![b'a'; TEXT_UPLOAD_MAX + 1];
        assert!(matches!(classify_text(&over), Err(TextError::TooLarge(n, cap)) if n == over.len() && cap == TEXT_UPLOAD_MAX));
        let at = vec![b'a'; TEXT_UPLOAD_MAX];
        assert!(classify_text(&at).is_ok());
    }

    #[test]
    fn the_media_type_follows_the_extension_and_defaults_to_plain_text() {
        assert_eq!(media_type_for_name("notes.md"), "text/markdown");
        assert_eq!(media_type_for_name("NOTES.MARKDOWN"), "text/markdown");
        assert_eq!(media_type_for_name("data.json"), "application/json");
        assert_eq!(media_type_for_name("rows.csv"), "text/csv");
        assert_eq!(media_type_for_name("stack.yaml"), "application/yaml");
        assert_eq!(media_type_for_name("stack.yml"), "application/yaml");
        assert_eq!(media_type_for_name("Cargo.toml"), "application/toml");
        assert_eq!(media_type_for_name("README"), "text/plain");
        assert_eq!(media_type_for_name("main.rs"), "text/plain");
    }

    #[tokio::test]
    async fn a_second_upload_with_the_same_name_gets_a_numbered_suffix() {
        let tmp = TempDir::new();
        let dir = UploadsDir::at(&tmp.0);
        let first = dir.store_text("notes.md", b"one").await.unwrap();
        let second = dir.store_text("notes.md", b"two").await.unwrap();
        let third = dir.store_text("notes.md", b"three").await.unwrap();
        assert_eq!(first.name, "notes.md");
        assert_eq!(second.name, "notes-2.md");
        assert_eq!(third.name, "notes-3.md");
        assert_eq!(std::fs::read(&second.path).unwrap(), b"two");
        assert_eq!(second.path, tmp.0.join("notes-2.md"));
        assert_eq!(second.media_type, "text/markdown");
        assert_eq!(second.bytes, 3);
        // No extension: the number goes at the end.
        dir.store_text("README", b"a").await.unwrap();
        let again = dir.store_text("README", b"b").await.unwrap();
        assert_eq!(again.name, "README-2");
    }

    #[tokio::test]
    async fn a_name_that_is_a_dot_or_a_path_lands_on_a_safe_name_inside_the_directory() {
        let tmp = TempDir::new();
        let dir = UploadsDir::at(&tmp.0);
        for name in ["..", ".", "", "   ", "../../etc/passwd", "/etc/passwd", "C:\\x\\..", "a\u{7}b.md"] {
            let stored = dir.store_text(name, b"x").await.unwrap();
            assert_eq!(stored.path.parent().unwrap(), tmp.0.as_path(), "{name:?}");
            assert!(!stored.name.contains('/') && !stored.name.contains('\\'), "{name:?}");
            assert_ne!(stored.name, "..");
            assert_ne!(stored.name, ".");
        }
        let names: Vec<String> = std::fs::read_dir(&tmp.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n == "passwd"));
        assert!(names.iter().any(|n| n == "ab.md"));
        assert!(names.iter().filter(|n| n.starts_with("upload")).count() >= 4);
    }

    #[tokio::test]
    async fn a_stored_upload_leaves_no_temp_file_behind_and_a_binary_writes_nothing() {
        let tmp = TempDir::new();
        let dir = UploadsDir::at(&tmp.0);
        dir.store_text("notes.md", b"hello").await.unwrap();
        assert!(matches!(dir.store_text("blob.bin", b"\0\x01\x02").await, Err(TextError::NotText)));
        let names: Vec<String> = std::fs::read_dir(&tmp.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["notes.md".to_string()]);
    }

    #[test]
    fn the_frame_carries_the_path_as_the_handle() {
        let u = TextUpload {
            name: "notes.md".into(),
            path: PathBuf::from("/embra/workspace/uploads/notes.md"),
            bytes: 12,
            media_type: "text/markdown".into(),
        };
        let f = file_ref_frame(&u, "attached", true, "", "");
        assert_eq!(f.path, "/embra/workspace/uploads/notes.md");
        assert_eq!(f.name, "notes.md");
        assert_eq!(f.byte_size, 12);
        assert_eq!(f.media_type, "text/markdown");
        assert_eq!(f.origin, "attached");
        assert!(f.replay);
    }
}
