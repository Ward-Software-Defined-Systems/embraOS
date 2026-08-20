//! The MEDIA file store: `/embra/workspace/MEDIA/<id>.<ext>` + `<id>.json`.
//!
//! Inside the tool-layer workspace jail (bind-mounted from DATA at boot,
//! `KG_DUMPS/` precedent) so the model can see and reuse the files, but
//! OWNED by the brain: ids are minted here, files are written atomically,
//! and reads rebuild the path from the validated id + an extension
//! allowlist — never from anything stored in the sidecar — then re-sniff
//! the bytes before serving. A model that edits a sidecar gains nothing.
//!
//! Id grammar (hand-validated, no regex crate):
//! - `att-<YYYYMMDDTHHMMSSZ>-<8 hex>`  operator attachment (normalized)
//! - `gen-<YYYYMMDDTHHMMSSZ>-<8 hex>`  tool-generated image
//! - `view-<16 hex>`                   content-addressed `image_view` copy

use std::path::{Path, PathBuf};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use embra_tools_core::MediaRefMeta;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::brain::AttachmentRef;
use crate::provider::ir::ImageData;

use super::ingest::{self, ImageKind, IngestError, Normalized};
use super::{MEDIA_NAME_MAX, MEDIA_UPLOAD_MAX};

/// Production store directory. Full path const per repo precedent
/// (`KG_DUMPS_DIR`); do not widen engineering's `WORKSPACE_ROOT` for this.
pub const MEDIA_DIR: &str = "/embra/workspace/MEDIA";
/// Dev/test override for the store directory (exclusive when set).
pub const MEDIA_DIR_ENV: &str = "EMBRA_MEDIA_DIR";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaOrigin {
    Attached,
    Generated,
    Viewed,
}

impl MediaOrigin {
    pub fn prefix(self) -> &'static str {
        match self {
            MediaOrigin::Attached => "att",
            MediaOrigin::Generated => "gen",
            MediaOrigin::Viewed => "view",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MediaOrigin::Attached => "attached",
            MediaOrigin::Generated => "generated",
            MediaOrigin::Viewed => "viewed",
        }
    }
}

/// Sidecar contents. `file` is informational — reads never use it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaMeta {
    pub id: String,
    pub file: String,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
    pub byte_size: u64,
    pub name: String,
    pub origin: MediaOrigin,
    #[serde(default)]
    pub session: String,
    pub created_at: String,
    pub sha256: String,
}

impl MediaMeta {
    pub fn kind(&self) -> Option<ImageKind> {
        ImageKind::from_media_type(&self.media_type)
    }

    /// Absolute path of the image file (id + allowlisted extension).
    pub fn path_in(&self, dir: &Path) -> PathBuf {
        let ext = self.kind().map(ImageKind::ext).unwrap_or("bin");
        dir.join(format!("{}.{}", self.id, ext))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("{0}")]
    Ingest(#[from] IngestError),
    #[error("invalid media id '{0}'")]
    BadId(String),
    #[error("media '{0}' not found")]
    NotFound(String),
    #[error("media store I/O error: {0}")]
    Io(String),
    #[error("media '{0}' is corrupt: {1}")]
    Corrupt(String, String),
}

/// Validate an id against the grammar and return its origin.
pub fn parse_media_id(id: &str) -> Result<MediaOrigin, MediaError> {
    let bad = || MediaError::BadId(id.to_string());
    if id.len() > 40 || id.is_empty() {
        return Err(bad());
    }
    let (prefix, rest) = id.split_once('-').ok_or_else(bad)?;
    let origin = match prefix {
        "att" => MediaOrigin::Attached,
        "gen" => MediaOrigin::Generated,
        "view" => MediaOrigin::Viewed,
        _ => return Err(bad()),
    };
    let is_hex = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    match origin {
        MediaOrigin::Viewed => {
            if rest.len() != 16 || !is_hex(rest) {
                return Err(bad());
            }
        }
        MediaOrigin::Attached | MediaOrigin::Generated => {
            // <YYYYMMDDTHHMMSSZ>-<8 hex>
            let (stamp, hex) = rest.split_once('-').ok_or_else(bad)?;
            let stamp_ok = stamp.len() == 16
                && stamp.as_bytes()[8] == b'T'
                && stamp.as_bytes()[15] == b'Z'
                && stamp[..8].bytes().all(|b| b.is_ascii_digit())
                && stamp[9..15].bytes().all(|b| b.is_ascii_digit());
            if !stamp_ok || hex.len() != 8 || !is_hex(hex) {
                return Err(bad());
            }
        }
    }
    Ok(origin)
}

fn new_id(origin: MediaOrigin) -> String {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let rand = uuid::Uuid::new_v4().simple().to_string();
    format!("{}-{}-{}", origin.prefix(), stamp, &rand[..8])
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Operator-facing name: basename only, control chars stripped, capped.
pub fn sanitize_name(name: &str, fallback: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>();
    let trimmed = base.trim();
    if trimmed.is_empty() {
        return fallback.to_string();
    }
    trimmed.chars().take(MEDIA_NAME_MAX).collect()
}

#[derive(Debug, Clone)]
pub struct MediaStore {
    dir: PathBuf,
}

impl MediaStore {
    /// The production store (or the `EMBRA_MEDIA_DIR` override).
    pub fn default_store() -> Self {
        let dir = std::env::var(MEDIA_DIR_ENV)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(MEDIA_DIR));
        Self::at(dir)
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub async fn ensure_dir(&self) -> Result<(), MediaError> {
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|e| MediaError::Io(format!("create {}: {e}", self.dir.display())))
    }

    fn sidecar_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Store normalized bytes. `Viewed` origin is content-addressed and
    /// idempotent: a second view of the same bytes returns the existing
    /// meta without rewriting anything.
    pub async fn put(
        &self,
        origin: MediaOrigin,
        name: &str,
        session: &str,
        normalized: Normalized,
    ) -> Result<MediaMeta, MediaError> {
        self.ensure_dir().await?;
        let sha = sha256_hex(&normalized.bytes);
        let id = match origin {
            MediaOrigin::Viewed => format!("view-{}", &sha[..16]),
            _ => new_id(origin),
        };
        if origin == MediaOrigin::Viewed
            && let Ok(existing) = self.meta(&id).await
            && existing.sha256 == sha
            && tokio::fs::metadata(existing.path_in(&self.dir)).await.is_ok()
        {
            return Ok(existing);
        }
        let file = format!("{}.{}", id, normalized.kind.ext());
        let meta = MediaMeta {
            id: id.clone(),
            file: file.clone(),
            media_type: normalized.kind.media_type().to_string(),
            width: normalized.width,
            height: normalized.height,
            byte_size: normalized.bytes.len() as u64,
            name: sanitize_name(name, &id),
            origin,
            session: session.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            sha256: sha,
        };
        let path = self.dir.join(&file);
        crate::tools::file_patch::write_atomic_create(&path, &normalized.bytes)
            .await
            .map_err(MediaError::Io)?;
        let sidecar = serde_json::to_vec_pretty(&meta).map_err(|e| MediaError::Io(e.to_string()))?;
        crate::tools::file_patch::write_atomic_create(&self.sidecar_path(&id), &sidecar)
            .await
            .map_err(MediaError::Io)?;
        Ok(meta)
    }

    /// Import an existing image file (operator `/attach <path>`): size
    /// gate → normalize → store as an attachment. Reads are unjailed like
    /// `file_read` (absolute paths pass through; relative paths resolve
    /// under the workspace root).
    pub async fn import_path(&self, path: &str, session: &str) -> Result<MediaMeta, MediaError> {
        let resolved = resolve_read_path(path);
        let md = tokio::fs::metadata(&resolved)
            .await
            .map_err(|e| MediaError::Io(format!("{}: {e}", resolved.display())))?;
        if !md.is_file() {
            return Err(MediaError::Io(format!("{} is not a file", resolved.display())));
        }
        if md.len() as usize > MEDIA_UPLOAD_MAX {
            return Err(IngestError::TooLarge(md.len() as usize, MEDIA_UPLOAD_MAX).into());
        }
        let bytes = tokio::fs::read(&resolved)
            .await
            .map_err(|e| MediaError::Io(format!("{}: {e}", resolved.display())))?;
        let normalized = ingest::normalize(bytes).await?;
        let name = resolved
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("image");
        self.put(MediaOrigin::Attached, name, session, normalized).await
    }

    pub async fn meta(&self, id: &str) -> Result<MediaMeta, MediaError> {
        parse_media_id(id)?;
        let raw = match tokio::fs::read(self.sidecar_path(id)).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(MediaError::NotFound(id.to_string()));
            }
            Err(e) => return Err(MediaError::Io(e.to_string())),
        };
        let meta: MediaMeta = serde_json::from_slice(&raw)
            .map_err(|e| MediaError::Corrupt(id.to_string(), format!("sidecar: {e}")))?;
        if meta.id != id {
            return Err(MediaError::Corrupt(id.to_string(), "sidecar id mismatch".into()));
        }
        if meta.kind().is_none() {
            return Err(MediaError::Corrupt(id.to_string(), format!("media_type {}", meta.media_type)));
        }
        Ok(meta)
    }

    /// Stored bytes, re-sniffed: the on-disk type must match the sidecar.
    pub async fn get(&self, id: &str) -> Result<(MediaMeta, Vec<u8>), MediaError> {
        let meta = self.meta(id).await?;
        let path = meta.path_in(&self.dir);
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(MediaError::NotFound(id.to_string()));
            }
            Err(e) => return Err(MediaError::Io(e.to_string())),
        };
        if bytes.len() > MEDIA_UPLOAD_MAX {
            return Err(MediaError::Corrupt(id.to_string(), "file exceeds the store ceiling".into()));
        }
        match ingest::sniff(&bytes) {
            Some(kind) if Some(kind) == meta.kind() => Ok((meta, bytes)),
            Some(kind) => Err(MediaError::Corrupt(
                id.to_string(),
                format!("bytes are {} but sidecar says {}", kind.media_type(), meta.media_type),
            )),
            None => Err(MediaError::Corrupt(id.to_string(), "bytes are not a supported image".into())),
        }
    }

    /// `(files, bytes)` across the store's image files (sidecars excluded).
    pub async fn usage(&self) -> Result<(usize, u64), MediaError> {
        let mut rd = match tokio::fs::read_dir(&self.dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
            Err(e) => return Err(MediaError::Io(e.to_string())),
        };
        let (mut files, mut bytes) = (0usize, 0u64);
        while let Ok(Some(entry)) = rd.next_entry().await {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".json") {
                continue;
            }
            if let Ok(md) = entry.metadata().await
                && md.is_file()
            {
                files += 1;
                bytes += md.len();
            }
        }
        Ok((files, bytes))
    }
}

/// `file_read`'s read policy: absolute passes through, relative resolves
/// under the workspace root.
pub(crate) fn resolve_read_path(path: &str) -> PathBuf {
    let trimmed = path.trim();
    if trimmed.starts_with('/') {
        PathBuf::from(trimmed)
    } else {
        Path::new(crate::tools::engineering::WORKSPACE_ROOT).join(trimmed.trim_start_matches("./"))
    }
}

pub fn to_image_data(meta: &MediaMeta, bytes: &[u8]) -> ImageData {
    ImageData {
        media_type: meta.media_type.clone(),
        data_b64: std::sync::Arc::from(STANDARD.encode(bytes)),
        width: meta.width,
        height: meta.height,
        name: meta.name.clone(),
    }
}

pub fn to_ref_meta(meta: &MediaMeta, dir: &Path, caption: &str) -> MediaRefMeta {
    MediaRefMeta {
        id: meta.id.clone(),
        media_type: meta.media_type.clone(),
        width: meta.width,
        height: meta.height,
        byte_size: meta.byte_size,
        name: meta.name.clone(),
        origin: meta.origin.as_str().to_string(),
        path: meta.path_in(dir).display().to_string(),
        caption: caption.to_string(),
    }
}

pub fn to_attachment_ref(meta: &MediaMeta, dir: &Path) -> AttachmentRef {
    AttachmentRef {
        id: meta.id.clone(),
        name: meta.name.clone(),
        media_type: meta.media_type.clone(),
        width: meta.width,
        height: meta.height,
        bytes: meta.byte_size,
        path: meta.path_in(dir).display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::ingest::tests::png_fixture;

    fn tmp() -> tempfile_dir::TempDir {
        tempfile_dir::TempDir::new("embra-media")
    }

    /// Minimal self-contained temp dir (no tempfile dep in the tree).
    mod tempfile_dir {
        use std::path::{Path, PathBuf};
        pub struct TempDir(PathBuf);
        impl TempDir {
            pub fn new(prefix: &str) -> Self {
                let p = std::env::temp_dir().join(format!(
                    "{}-{}-{}",
                    prefix,
                    std::process::id(),
                    uuid::Uuid::new_v4().simple()
                ));
                std::fs::create_dir_all(&p).unwrap();
                TempDir(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn media_id_grammar_accepts_att_gen_view_and_rejects_traversal() {
        assert_eq!(parse_media_id("att-20260820T153012Z-1a2b3c4d").unwrap(), MediaOrigin::Attached);
        assert_eq!(parse_media_id("gen-20260820T153012Z-deadbeef").unwrap(), MediaOrigin::Generated);
        assert_eq!(parse_media_id("view-0123456789abcdef").unwrap(), MediaOrigin::Viewed);
        for bad in [
            "",
            "att",
            "att-",
            "../../etc/passwd",
            "att-20260820T153012Z-1a2b3c4d/../x",
            "att-20260820T153012Z-1A2B3C4D", // uppercase hex
            "att-20260820X153012Z-1a2b3c4d",
            "att-20260820T153012Z-1a2b3c4",
            "view-0123456789abcde",
            "view-0123456789abcdefg",
            "xyz-20260820T153012Z-1a2b3c4d",
            "gen-20260820T153012Z-1a2b3c4d.png",
            "att-20260820T153012Z-1a2b3c4d-extra-long-id-here",
        ] {
            assert!(parse_media_id(bad).is_err(), "should reject {bad:?}");
        }
        let minted = new_id(MediaOrigin::Attached);
        assert_eq!(parse_media_id(&minted).unwrap(), MediaOrigin::Attached);
    }

    #[test]
    fn sanitize_name_strips_paths_and_controls() {
        assert_eq!(sanitize_name("/tmp/../photo.jpg", "id"), "photo.jpg");
        assert_eq!(sanitize_name("C:\\Users\\x\\shot.png", "id"), "shot.png");
        assert_eq!(sanitize_name("a\u{1b}[31mb.png", "id"), "a[31mb.png");
        assert_eq!(sanitize_name("   ", "att-x"), "att-x");
        assert_eq!(sanitize_name(&"n".repeat(500), "id").chars().count(), MEDIA_NAME_MAX);
    }

    #[tokio::test]
    async fn put_writes_file_and_sidecar_atomically_and_get_round_trips() {
        let dir = tmp();
        let store = MediaStore::at(dir.path());
        let n = ingest::normalize_blocking(png_fixture(5, 3)).unwrap();
        let meta = store.put(MediaOrigin::Attached, "shot.png", "main", n).await.unwrap();
        assert_eq!(meta.origin, MediaOrigin::Attached);
        assert_eq!((meta.width, meta.height), (5, 3));
        assert_eq!(meta.name, "shot.png");
        assert!(meta.path_in(dir.path()).exists());
        assert!(dir.path().join(format!("{}.json", meta.id)).exists());
        // No temp files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
        let (got, bytes) = store.get(&meta.id).await.unwrap();
        assert_eq!(got, meta);
        assert_eq!(bytes, png_fixture(5, 3));
        let (files, total) = store.usage().await.unwrap();
        assert_eq!(files, 1);
        assert_eq!(total, bytes.len() as u64);
    }

    #[tokio::test]
    async fn get_refuses_sidecar_pointing_outside_allowlist_and_resniffs() {
        let dir = tmp();
        let store = MediaStore::at(dir.path());
        let n = ingest::normalize_blocking(png_fixture(2, 2)).unwrap();
        let meta = store.put(MediaOrigin::Generated, "g.png", "", n).await.unwrap();
        // Tamper 1: sidecar claims an unsupported type (would be a path escape).
        let side = dir.path().join(format!("{}.json", meta.id));
        let mut doc: serde_json::Value = serde_json::from_slice(&std::fs::read(&side).unwrap()).unwrap();
        doc["media_type"] = serde_json::json!("text/html");
        std::fs::write(&side, serde_json::to_vec(&doc).unwrap()).unwrap();
        assert!(matches!(store.get(&meta.id).await, Err(MediaError::Corrupt(..))));
        // Tamper 2: sidecar says jpeg, bytes are png → re-sniff catches it.
        doc["media_type"] = serde_json::json!("image/jpeg");
        std::fs::write(&side, serde_json::to_vec(&doc).unwrap()).unwrap();
        std::fs::write(dir.path().join(format!("{}.jpg", meta.id)), png_fixture(2, 2)).unwrap();
        assert!(matches!(store.get(&meta.id).await, Err(MediaError::Corrupt(..))));
        // Unknown id → NotFound; bad id → BadId.
        assert!(matches!(store.get("gen-20260820T000000Z-00000000").await, Err(MediaError::NotFound(_))));
        assert!(matches!(store.get("../x").await, Err(MediaError::BadId(_))));
    }

    #[tokio::test]
    async fn view_ids_are_content_addressed_and_idempotent() {
        let dir = tmp();
        let store = MediaStore::at(dir.path());
        let n1 = ingest::normalize_blocking(png_fixture(3, 3)).unwrap();
        let a = store.put(MediaOrigin::Viewed, "diagram.png", "", n1).await.unwrap();
        let n2 = ingest::normalize_blocking(png_fixture(3, 3)).unwrap();
        let b = store.put(MediaOrigin::Viewed, "diagram-copy.png", "", n2).await.unwrap();
        assert_eq!(a, b, "same bytes → same id, first meta kept");
        assert!(a.id.starts_with("view-"));
        assert_eq!(store.usage().await.unwrap().0, 1);
    }

    #[tokio::test]
    async fn import_path_size_gate_and_type_gate() {
        let dir = tmp();
        let store = MediaStore::at(dir.path());
        let src = dir.path().join("big.png");
        let mut big = png_fixture(2, 2);
        big.resize(MEDIA_UPLOAD_MAX + 1, 0);
        std::fs::write(&src, &big).unwrap();
        assert!(matches!(
            store.import_path(src.to_str().unwrap(), "s").await,
            Err(MediaError::Ingest(IngestError::TooLarge(..)))
        ));
        let svg = dir.path().join("x.png");
        std::fs::write(&svg, b"<svg/>").unwrap();
        assert!(matches!(
            store.import_path(svg.to_str().unwrap(), "s").await,
            Err(MediaError::Ingest(IngestError::NotAnImage))
        ));
        let ok = dir.path().join("ok.png");
        std::fs::write(&ok, png_fixture(4, 4)).unwrap();
        let meta = store.import_path(ok.to_str().unwrap(), "s").await.unwrap();
        assert_eq!(meta.name, "ok.png");
        assert_eq!(meta.session, "s");
    }
}
