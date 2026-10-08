//! Files offered to the operator for download.
//!
//! `file_offer` (the intelligence) and `/download` (the operator) name a
//! workspace file; embra-web fetches it through the `GetFile` RPC and
//! hands it to the browser as an attachment. This is a door out of the
//! OS, so the check is stronger than the write jail's string prefix: the
//! path must stay under the workspace as written AND after every symlink
//! is followed, name a regular file, and fit under [`FILE_DOWNLOAD_MAX`].

use std::path::{Component, Path, PathBuf};

use embra_common::proto::brain::FileRef;

use crate::tools::engineering::WORKSPACE_ROOT;

/// Largest file served for download: the media upload ceiling, so every
/// `GetFile` response stays under `GRPC_MAX_MESSAGE_BYTES` with framing.
pub const FILE_DOWNLOAD_MAX: u64 = super::MEDIA_UPLOAD_MAX as u64;

#[derive(Debug, thiserror::Error)]
pub enum OfferError {
    #[error("Denied: {0}")]
    Denied(String),
    #[error("{0}: not found")]
    NotFound(String),
    #[error("{0} is not a regular file")]
    NotAFile(String),
    #[error("file is {0} bytes; the download limit is {1} bytes")]
    TooLarge(u64, u64),
    #[error("I/O error: {0}")]
    Io(String),
}

/// A workspace file that may be served: `path` is the canonical file
/// (what is read), `shown` the absolute path as it was asked for (what
/// the frame carries), `rel` its workspace-relative form (what the
/// download URL names), `name` its basename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferedFile {
    pub path: PathBuf,
    pub shown: PathBuf,
    pub rel: String,
    pub name: String,
    pub bytes: u64,
    pub media_type: String,
}

/// Resolve an offer against the production workspace.
pub async fn resolve_offer(path: &str) -> Result<OfferedFile, OfferError> {
    resolve_offer_in(Path::new(WORKSPACE_ROOT), path, FILE_DOWNLOAD_MAX).await
}

/// The resolver behind [`resolve_offer`], against any root and cap, so a
/// test can run it under a temporary directory with a small cap.
pub async fn resolve_offer_in(root: &Path, path: &str, cap: u64) -> Result<OfferedFile, OfferError> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(OfferError::Denied("a file path is required".into()));
    }
    let joined = if trimmed.starts_with('/') {
        PathBuf::from(trimmed)
    } else {
        root.join(trimmed.trim_start_matches("./"))
    };
    if joined.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(OfferError::Denied(format!(
            "path '{}' contains a '..' component",
            joined.display()
        )));
    }
    if !joined.starts_with(root) {
        return Err(OfferError::Denied(format!(
            "path '{}' resolves outside {}",
            joined.display(),
            root.display()
        )));
    }
    let shown = joined.display().to_string();
    let canonical = match tokio::fs::canonicalize(&joined).await {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(OfferError::NotFound(shown)),
        Err(e) => return Err(OfferError::Io(format!("{shown}: {e}"))),
    };
    let root_canonical = tokio::fs::canonicalize(root)
        .await
        .map_err(|e| OfferError::Io(format!("{}: {e}", root.display())))?;
    if !canonical.starts_with(&root_canonical) {
        return Err(OfferError::Denied(format!(
            "path '{}' resolves outside {} (symlink target {})",
            shown,
            root.display(),
            canonical.display()
        )));
    }
    let md = tokio::fs::metadata(&canonical)
        .await
        .map_err(|e| OfferError::Io(format!("{shown}: {e}")))?;
    if !md.is_file() {
        return Err(OfferError::NotAFile(shown));
    }
    if md.len() > cap {
        return Err(OfferError::TooLarge(md.len(), cap));
    }
    let rel = joined
        .strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| shown.clone());
    let name = joined
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();
    Ok(OfferedFile {
        media_type: download_media_type(&name).to_string(),
        path: canonical,
        shown: joined,
        rel,
        name,
        bytes: md.len(),
    })
}

/// What a tool hands the loop for an offer it made.
pub fn to_ref_meta(f: &OfferedFile, caption: &str) -> embra_tools_core::FileRefMeta {
    embra_tools_core::FileRefMeta {
        path: f.shown.display().to_string(),
        name: f.name.clone(),
        byte_size: f.bytes,
        media_type: f.media_type.clone(),
        caption: caption.to_string(),
    }
}

/// Proto `FileRef` from a tool's `FileRefMeta` (tool loop emit).
pub fn file_ref_from_tool(m: &embra_tools_core::FileRefMeta, tool_use_id: &str) -> FileRef {
    FileRef {
        path: m.path.clone(),
        name: m.name.clone(),
        byte_size: m.byte_size,
        media_type: m.media_type.clone(),
        origin: "offered".to_string(),
        caption: m.caption.clone(),
        tool_use_id: tool_use_id.to_string(),
        replay: false,
    }
}

/// Persisted ref for a file a tool offered: no id, no dimensions.
pub fn attachment_ref_from_tool(m: &embra_tools_core::FileRefMeta) -> crate::brain::AttachmentRef {
    crate::brain::AttachmentRef {
        id: String::new(),
        name: m.name.clone(),
        media_type: m.media_type.clone(),
        width: 0,
        height: 0,
        bytes: m.byte_size,
        path: m.path.clone(),
    }
}

/// The type a download is labelled with, by extension; a type nobody
/// recognizes is `application/octet-stream`. What embra-web actually
/// sends is its decision (it never lets a download activate).
pub fn download_media_type(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "md" | "markdown" => "text/markdown",
        "txt" | "log" | "rs" | "py" | "sh" | "js" | "ts" | "html" | "css" => "text/plain",
        "json" => "application/json",
        "csv" => "text/csv",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// The operator-facing frame for an offered file.
pub fn file_ref_frame(f: &OfferedFile, replay: bool, tool_use_id: &str, caption: &str) -> FileRef {
    FileRef {
        path: f.shown.display().to_string(),
        name: f.name.clone(),
        byte_size: f.bytes,
        media_type: f.media_type.clone(),
        origin: "offered".to_string(),
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
                "embra-offer-{}-{}",
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

    fn workspace() -> (TempDir, PathBuf) {
        let tmp = TempDir::new();
        let root = tmp.0.join("workspace");
        std::fs::create_dir_all(root.join("uploads")).unwrap();
        std::fs::write(root.join("uploads/notes.md"), "# notes\n").unwrap();
        (tmp, root)
    }

    #[tokio::test]
    async fn an_offer_keeps_its_workspace_relative_path_and_name() {
        let (_tmp, root) = workspace();
        let f = resolve_offer_in(&root, "uploads/notes.md", FILE_DOWNLOAD_MAX).await.unwrap();
        assert_eq!(f.rel, "uploads/notes.md");
        assert_eq!(f.name, "notes.md");
        assert_eq!(f.bytes, 8);
        assert_eq!(f.media_type, "text/markdown");
        assert_eq!(f.path, std::fs::canonicalize(root.join("uploads/notes.md")).unwrap());
        assert_eq!(f.shown, root.join("uploads/notes.md"));
        // Absolute under the root, and `./`, say the same.
        let abs = root.join("uploads/notes.md").display().to_string();
        assert_eq!(resolve_offer_in(&root, &abs, FILE_DOWNLOAD_MAX).await.unwrap().rel, "uploads/notes.md");
        assert_eq!(resolve_offer_in(&root, "./uploads/notes.md", FILE_DOWNLOAD_MAX).await.unwrap().rel, "uploads/notes.md");
        let frame = file_ref_frame(&f, false, "toolu_1", "the notes");
        assert_eq!(frame.origin, "offered");
        assert_eq!(frame.tool_use_id, "toolu_1");
        assert_eq!(frame.caption, "the notes");
        assert_eq!(frame.name, "notes.md");
        assert_eq!(frame.path, root.join("uploads/notes.md").display().to_string());
        let meta = to_ref_meta(&f, "the notes");
        assert_eq!(meta.path, frame.path);
        let from_tool = file_ref_from_tool(&meta, "toolu_2");
        assert_eq!(from_tool.origin, "offered");
        assert_eq!(from_tool.tool_use_id, "toolu_2");
        assert_eq!(from_tool.caption, "the notes");
        let r = attachment_ref_from_tool(&meta);
        assert_eq!(r.id, "");
        assert!(!r.is_image());
        assert_eq!(r.bytes, 8);
    }

    #[tokio::test]
    async fn an_offer_outside_the_workspace_is_refused() {
        let (tmp, root) = workspace();
        let outside = tmp.0.join("secret.txt");
        std::fs::write(&outside, "x").unwrap();
        for bad in ["../secret.txt", "uploads/../../secret.txt", "/etc/passwd", "", "   "] {
            assert!(matches!(resolve_offer_in(&root, bad, FILE_DOWNLOAD_MAX).await, Err(OfferError::Denied(_))), "{bad:?}");
        }
        let abs_outside = outside.display().to_string();
        assert!(matches!(resolve_offer_in(&root, &abs_outside, FILE_DOWNLOAD_MAX).await, Err(OfferError::Denied(_))));
        // A sibling whose name starts with the root's is outside too.
        let sibling = tmp.0.join("workspace-evil");
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("a.txt"), "x").unwrap();
        let s = sibling.join("a.txt").display().to_string();
        assert!(matches!(resolve_offer_in(&root, &s, FILE_DOWNLOAD_MAX).await, Err(OfferError::Denied(_))));
        assert!(matches!(resolve_offer_in(&root, "uploads/missing.md", FILE_DOWNLOAD_MAX).await, Err(OfferError::NotFound(_))));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_offer_through_a_symlink_that_leaves_the_workspace_is_refused() {
        let (tmp, root) = workspace();
        let outside = tmp.0.join("secret.txt");
        std::fs::write(&outside, "x").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("uploads/link.txt")).unwrap();
        assert!(matches!(
            resolve_offer_in(&root, "uploads/link.txt", FILE_DOWNLOAD_MAX).await,
            Err(OfferError::Denied(d)) if d.contains("symlink target")
        ));
        // A symlink that stays inside is followed and served as its target.
        std::os::unix::fs::symlink(root.join("uploads/notes.md"), root.join("uploads/alias.md")).unwrap();
        let f = resolve_offer_in(&root, "uploads/alias.md", FILE_DOWNLOAD_MAX).await.unwrap();
        assert_eq!(f.rel, "uploads/alias.md");
        assert_eq!(f.path, std::fs::canonicalize(root.join("uploads/notes.md")).unwrap());
    }

    #[tokio::test]
    async fn a_directory_cannot_be_offered() {
        let (_tmp, root) = workspace();
        assert!(matches!(resolve_offer_in(&root, "uploads", FILE_DOWNLOAD_MAX).await, Err(OfferError::NotAFile(_))));
        assert!(matches!(resolve_offer_in(&root, "/", FILE_DOWNLOAD_MAX).await, Err(OfferError::Denied(_))));
    }

    #[tokio::test]
    async fn an_offer_over_the_download_cap_is_refused() {
        let (_tmp, root) = workspace();
        assert!(matches!(resolve_offer_in(&root, "uploads/notes.md", 7).await, Err(OfferError::TooLarge(8, 7))));
        assert!(resolve_offer_in(&root, "uploads/notes.md", 8).await.is_ok());
        assert_eq!(FILE_DOWNLOAD_MAX, 12 * 1024 * 1024);
    }

    #[test]
    fn the_download_media_type_follows_the_extension_and_defaults_to_octet_stream() {
        assert_eq!(download_media_type("notes.md"), "text/markdown");
        assert_eq!(download_media_type("REPORT.TXT"), "text/plain");
        assert_eq!(download_media_type("main.rs"), "text/plain");
        assert_eq!(download_media_type("data.json"), "application/json");
        assert_eq!(download_media_type("shot.png"), "image/png");
        assert_eq!(download_media_type("archive.tar.gz"), "application/octet-stream");
        assert_eq!(download_media_type("README"), "application/octet-stream");
    }
}
