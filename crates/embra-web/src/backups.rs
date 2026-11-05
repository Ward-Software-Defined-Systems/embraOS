//! `GET /api/backups/{name}` — a backup the OS took (`embra-update`), as a
//! browser download, streamed from DATA. The archive carries STATE's secrets,
//! so it lives outside the workspace the model's tools reach and is served
//! here alone, behind the console's token like every route. The route
//! reads the directory itself and reaches no service.

use std::path::{Path, PathBuf};

use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use tokio::io::AsyncReadExt;

use crate::media::json_error;
use crate::state::AppState;

/// Where the OS keeps its backups.
pub const BACKUPS_ROOT: &str = "/embra/data/backups";
/// The archive and its record inside an entry.
pub const ARCHIVE: &str = "backup.tar.gz";
pub const RECORD: &str = "backup.json";
const CHUNK: usize = 64 * 1024;

/// A backup's name: one path component of letters, digits, `_`, `.` and
/// `-`, starting with a letter or digit, at most 80 characters: never a
/// slash, never `..`, never a hidden entry.
pub fn valid_backup_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    name.len() <= 80 && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// The archive to serve, when the entry is complete (its record beside it).
pub fn archive_path(root: &Path, name: &str) -> Option<PathBuf> {
    if !valid_backup_name(name) {
        return None;
    }
    let dir = root.join(name);
    if !dir.join(RECORD).is_file() {
        return None;
    }
    let archive = dir.join(ARCHIVE);
    archive.is_file().then_some(archive)
}

pub async fn api_backup_get(State(st): State<AppState>, UrlPath(name): UrlPath<String>) -> Response {
    if !valid_backup_name(&name) {
        return json_error(StatusCode::BAD_REQUEST, format!("invalid backup name '{name}'"));
    }
    let Some(path) = archive_path(&st.backups_root, &name) else {
        return json_error(StatusCode::NOT_FOUND, format!("no complete backup named '{name}'"));
    };
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("open {}: {e}", path.display())),
    };
    let len = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("stat {}: {e}", path.display())),
    };
    let body = Body::from_stream(stream::try_unfold(file, |mut f| async move {
        let mut buf = vec![0u8; CHUNK];
        let n = f.read(&mut buf).await?;
        if n == 0 {
            Ok::<_, std::io::Error>(None)
        } else {
            buf.truncate(n);
            Ok(Some((Bytes::from(buf), f)))
        }
    }));
    let mut resp = (StatusCode::OK, body).into_response();
    download_headers(resp.headers_mut(), &name, len);
    resp
}

/// The headers of a backup download (unit-tested): an attachment named
/// `<name>.tar.gz`, octet-stream, nosniff, no-store, its length.
pub fn download_headers(h: &mut axum::http::HeaderMap, name: &str, len: u64) {
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if let Ok(v) = HeaderValue::from_str(&len.to_string()) {
        h.insert(header::CONTENT_LENGTH, v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}.tar.gz\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backup_name_grammar_refuses_traversal_slashes_and_controls() {
        assert!(valid_backup_name("2026-11-05_101200_pre-v0.24.0-phase2"));
        assert!(valid_backup_name("nightly"));
        for bad in ["", "..", ".hidden", "a/b", "a\\b", "a b", "a\nb", "../etc", "a\u{7}b", &"x".repeat(81)] {
            assert!(!valid_backup_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_backup_download_is_an_attachment_with_a_length_and_an_incomplete_entry_is_not_found() {
        let root = std::env::temp_dir().join(format!("embra-web-backups-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let complete = root.join("2026-11-05_101200_manual");
        std::fs::create_dir_all(&complete).unwrap();
        std::fs::write(complete.join(ARCHIVE), b"tar bytes").unwrap();
        std::fs::write(complete.join(RECORD), b"{}").unwrap();
        let partial = root.join("2026-11-04_101200_manual.partial");
        std::fs::create_dir_all(&partial).unwrap();
        std::fs::write(partial.join(ARCHIVE), b"half").unwrap();
        assert_eq!(archive_path(&root, "2026-11-05_101200_manual"), Some(complete.join(ARCHIVE)));
        assert_eq!(archive_path(&root, "2026-11-04_101200_manual.partial"), None, "no record beside it");
        assert_eq!(archive_path(&root, "missing"), None);
        assert_eq!(archive_path(&root, "../etc"), None);
        std::fs::remove_dir_all(&root).unwrap();

        let mut h = axum::http::HeaderMap::new();
        download_headers(&mut h, "2026-11-05_101200_manual", 9);
        assert_eq!(h[header::CONTENT_TYPE], "application/octet-stream");
        assert_eq!(h[header::CACHE_CONTROL], "no-store");
        assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(h[header::CONTENT_LENGTH], "9");
        assert_eq!(h[header::CONTENT_DISPOSITION], "attachment; filename=\"2026-11-05_101200_manual.tar.gz\"");
    }
}
