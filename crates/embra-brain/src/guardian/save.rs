//! `save_to` for the shipped `http_request`: the workspace sink.
//!
//! The Guardian broker hands a saved body to a [`FileSink`] with the
//! caller's path as written, the bytes and the content type, and never
//! resolves a path itself (`embra_guardian::caps::guarded_http_request`).
//! This module is the brain's sink: the one workspace resolver
//! (`engineering::resolve_workspace_path`, here under its root parameter),
//! the refused directories (the media store, whose files all carry
//! sidecars; the Guardian build tree, where a planted `build.rs` would run
//! at the next rebuild), the canonical check of the nearest existing
//! ancestor that every writer runs (`tools::jail::real`; the string jail
//! does not see a symlinked directory), no overwrite, and the one atomic
//! writer (`file_patch::write_atomic_create`).
//!
//! The broker runs on a blocking-pool thread (`spawn_blocking` in the
//! invoke arm), so the sink drives the async writer with
//! `Handle::block_on`, which is sound there: the blocking pool is not a
//! runtime context. The handle is captured where the sink is built, in
//! the async invoke arm.

use std::path::{Path, PathBuf};

use embra_guardian::caps::{FileSink, SavedFile, HTTP_SAVE_MAX_BYTES};

use crate::media::offer::FILE_DOWNLOAD_MAX;
use crate::media::store::MediaStore;
use crate::media::MEDIA_UPLOAD_MAX;
use crate::tools::engineering::{ensure_parent_dirs, resolve_in, WORKSPACE_ROOT};
use crate::tools::file_patch::write_atomic_create;
use crate::tools::jail;

/// The save cap is the download and media ceiling: what `http_request`
/// saves, `file_offer` can offer and `image_view` can show.
const _: () = assert!(HTTP_SAVE_MAX_BYTES as u64 <= FILE_DOWNLOAD_MAX);
const _: () = assert!(HTTP_SAVE_MAX_BYTES == MEDIA_UPLOAD_MAX);

/// The Guardian build tree under the workspace, refused as a save target.
const GUARDIAN_DIR_NAME: &str = ".guardian";

fn exists(resolved: &str) -> String {
    format!("'{resolved}' exists; choose another name (nothing is overwritten)")
}

/// The string jail and the refused directories, before anything touches
/// the filesystem: the one workspace resolver, then the media store
/// (`media_dir`, itself and anything under it), the Guardian build tree,
/// and a path that names no file (a trailing `/`, or the root itself).
pub(crate) fn resolve_save_target(root: &str, media_dir: &Path, path: &str) -> Result<String, String> {
    let resolved = resolve_in(root, path)?;
    jail::fence_git_dir(Path::new(&resolved))?;
    if path.trim_end().ends_with('/') || resolved == root || Path::new(&resolved).file_name().is_none() {
        return Err(format!("'{path}' names no file; save_to takes a file path under {root}"));
    }
    let under = |dir: &Path| {
        let dir = dir.to_string_lossy();
        let dir = dir.trim_end_matches('/');
        resolved == dir || resolved.starts_with(&format!("{dir}/"))
    };
    if under(media_dir) {
        return Err(format!(
            "'{}' is the media store, which keeps its own files; save elsewhere under {root}",
            media_dir.display()
        ));
    }
    if under(&Path::new(root).join(GUARDIAN_DIR_NAME)) {
        return Err(format!(
            "'{GUARDIAN_DIR_NAME}/' is the Guardian build tree; save elsewhere under {root}"
        ));
    }
    Ok(resolved)
}

/// Where the file will land, once the filesystem has been asked: the
/// target must not exist (a file, a directory or a dangling link counts),
/// and the nearest existing ancestor is canonicalized and checked against
/// the canonical root (`jail::real`), the media store and the build tree
/// before any directory is created. `resolved` has passed
/// [`resolve_save_target`].
async fn plan_save(root: &Path, media_dir: &Path, resolved: &str) -> Result<PathBuf, String> {
    let target = Path::new(resolved);
    if tokio::fs::symlink_metadata(target).await.is_ok() {
        return Err(exists(resolved));
    }
    let real = jail::real(root, target).await?;
    jail::fence_git_dir(&real.path)?;
    if let Ok(media_canon) = tokio::fs::canonicalize(media_dir).await
        && real.ancestor.starts_with(&media_canon)
    {
        return Err(format!(
            "'{resolved}' resolves into the media store (real path {})",
            real.ancestor.display()
        ));
    }
    if real.ancestor.starts_with(real.root.join(GUARDIAN_DIR_NAME)) {
        return Err(format!(
            "'{resolved}' resolves into the Guardian build tree (real path {})",
            real.ancestor.display()
        ));
    }
    if tokio::fs::symlink_metadata(&real.path).await.is_ok() {
        return Err(exists(resolved));
    }
    Ok(real.path)
}

/// The write, after [`plan_save`]: the missing parents under the canonical
/// ancestor, then the one atomic writer. The plan runs again here,
/// immediately before the write, so a target that appeared since the
/// pre-flight is still refused.
pub(crate) async fn save_at(
    root: &Path,
    media_dir: &Path,
    resolved: &str,
    bytes: &[u8],
) -> Result<SavedFile, String> {
    let final_path = plan_save(root, media_dir, resolved).await?;
    ensure_parent_dirs(&final_path).await?;
    write_atomic_create(&final_path, bytes).await?;
    let shown = final_path.display().to_string();
    Ok(SavedFile { path: shown, bytes: bytes.len() })
}

/// The brain's [`FileSink`]: the workspace jail, the refused directories
/// and the atomic writer, driven from the broker's blocking thread.
pub(crate) struct WorkspaceSink {
    handle: tokio::runtime::Handle,
    root: String,
    media_dir: PathBuf,
}

impl WorkspaceSink {
    /// The production sink: `/embra/workspace` and the media store's
    /// directory. Built in the async invoke arm, where the handle is
    /// current.
    pub(crate) fn new(handle: tokio::runtime::Handle) -> Self {
        Self::at(handle, WORKSPACE_ROOT, MediaStore::default_store().dir().to_path_buf())
    }

    /// The same sink under another root, for a test.
    pub(crate) fn at(handle: tokio::runtime::Handle, root: &str, media_dir: PathBuf) -> Self {
        Self { handle, root: root.to_string(), media_dir }
    }
}

impl FileSink for WorkspaceSink {
    fn check(&self, path: &str) -> Result<(), String> {
        let resolved = resolve_save_target(&self.root, &self.media_dir, path)?;
        self.handle
            .block_on(plan_save(Path::new(&self.root), &self.media_dir, &resolved))
            .map(|_| ())
    }

    fn save(&self, path: &str, bytes: &[u8], _content_type: &str) -> Result<SavedFile, String> {
        let resolved = resolve_save_target(&self.root, &self.media_dir, path)?;
        self.handle
            .block_on(save_at(Path::new(&self.root), &self.media_dir, &resolved, bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let d = std::env::temp_dir().join(format!(
                "embra-save-test-{}-{}-{}",
                tag,
                std::process::id(),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&d).unwrap();
            TempDir(d)
        }
        fn root(&self) -> &Path {
            &self.0
        }
        fn root_str(&self) -> String {
            self.0.display().to_string()
        }
        fn media(&self) -> PathBuf {
            self.0.join("MEDIA")
        }
        fn shown(&self, rel: &str) -> String {
            self.0.join(rel).display().to_string()
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|it| {
                it.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.contains("embra-patch"))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn save_at_writes_the_bytes_atomically_under_a_new_parent() {
        let t = TempDir::new("write");
        let bytes = b"\x89PNG\r\n\x1a\nnot really a picture".to_vec();
        let saved = save_at(t.root(), &t.media(), &t.shown("shots/a.png"), &bytes).await.unwrap();
        assert_eq!(saved.bytes, bytes.len());
        assert_eq!(std::fs::read(&saved.path).unwrap(), bytes);
        assert!(t.root().join("shots").is_dir(), "the missing parent was created");
        assert!(leftovers(&t.root().join("shots")).is_empty(), "no temp left behind");
    }

    #[tokio::test]
    async fn save_at_refuses_an_existing_file_a_directory_and_a_dangling_symlink() {
        let t = TempDir::new("exists");
        let media = t.media();
        std::fs::write(t.root().join("a.bin"), b"old").unwrap();
        let err = save_at(t.root(), &media, &t.shown("a.bin"), b"new").await.unwrap_err();
        assert!(err.contains("exists"), "{err}");
        assert_eq!(std::fs::read(t.root().join("a.bin")).unwrap(), b"old", "nothing overwritten");
        std::fs::create_dir(t.root().join("adir")).unwrap();
        let err = save_at(t.root(), &media, &t.shown("adir"), b"x").await.unwrap_err();
        assert!(err.contains("exists"), "{err}");
        std::os::unix::fs::symlink(t.root().join("missing-target"), t.root().join("dangling")).unwrap();
        let err = save_at(t.root(), &media, &t.shown("dangling"), b"x").await.unwrap_err();
        assert!(err.contains("exists"), "{err}");
        assert!(!t.root().join("missing-target").exists(), "a dangling link is not written through");
    }

    #[tokio::test]
    async fn save_at_refuses_a_path_under_media_and_a_symlink_that_leads_there_or_outside() {
        let t = TempDir::new("media");
        let media = t.media();
        std::fs::create_dir_all(&media).unwrap();
        // The store by its real path (the string jail refuses it first in
        // production; the canonical check refuses it on its own).
        let err = save_at(t.root(), &media, &t.shown("MEDIA/x.png"), b"x").await.unwrap_err();
        assert!(err.contains("media store"), "{err}");
        // A symlinked directory that leads into it.
        std::os::unix::fs::symlink(&media, t.root().join("out")).unwrap();
        let err = save_at(t.root(), &media, &t.shown("out/x.png"), b"x").await.unwrap_err();
        assert!(err.contains("media store"), "{err}");
        // A symlinked directory that leads outside the root.
        let elsewhere = TempDir::new("elsewhere");
        std::os::unix::fs::symlink(elsewhere.root(), t.root().join("esc")).unwrap();
        let err = save_at(t.root(), &media, &t.shown("esc/x.png"), b"x").await.unwrap_err();
        assert!(err.contains("outside"), "{err}");
        assert!(!elsewhere.root().join("x.png").exists());
        // The Guardian build tree, through a link as well.
        std::fs::create_dir_all(t.root().join(".guardian/tools")).unwrap();
        std::os::unix::fs::symlink(t.root().join(".guardian"), t.root().join("g")).unwrap();
        let err = save_at(t.root(), &media, &t.shown("g/tools/build.rs"), b"x").await.unwrap_err();
        assert!(err.contains("build tree"), "{err}");
        assert!(std::fs::read_dir(&media).unwrap().next().is_none(), "nothing landed in the store");
    }

    #[test]
    fn resolve_save_target_refuses_dotdot_outside_media_guardian_and_a_trailing_slash() {
        let media = Path::new("/embra/workspace/MEDIA");
        assert_eq!(
            resolve_save_target(WORKSPACE_ROOT, media, "shots/a.png").unwrap(),
            "/embra/workspace/shots/a.png"
        );
        assert_eq!(
            resolve_save_target(WORKSPACE_ROOT, media, "/embra/workspace/files/r.pdf").unwrap(),
            "/embra/workspace/files/r.pdf"
        );
        for bad in [
            "../a.png",
            "/etc/a.png",
            "/embra/workspace/../a.png",
            "MEDIA/x.png",
            "/embra/workspace/MEDIA",
            ".guardian/tools/x/build.rs",
            "repo/.git/hooks/pre-commit",
            "shots/",
            "",
            "/embra/workspace",
        ] {
            assert!(resolve_save_target(WORKSPACE_ROOT, media, bad).is_err(), "{bad:?} must be refused");
        }
        // Each refusal says why.
        let why = |p: &str| resolve_save_target(WORKSPACE_ROOT, media, p).unwrap_err();
        assert!(why("../a.png").contains(".."), "{}", why("../a.png"));
        assert!(why("/etc/a.png").contains("outside"), "{}", why("/etc/a.png"));
        assert!(why("MEDIA/x.png").contains("media store"), "{}", why("MEDIA/x.png"));
        assert!(why(".guardian/x").contains("build tree"), "{}", why(".guardian/x"));
        assert!(why("shots/").contains("names no file"), "{}", why("shots/"));
        // A name that merely starts with the store's name is not the store.
        assert!(resolve_save_target(WORKSPACE_ROOT, media, "MEDIAx/a.png").is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_sink_saves_from_a_blocking_thread() {
        // The production shape: the broker runs on the blocking pool and
        // the sink drives the async writer with the captured handle.
        let t = TempDir::new("sink");
        let sink = Arc::new(WorkspaceSink::at(tokio::runtime::Handle::current(), &t.root_str(), t.media()));
        let s = sink.clone();
        let saved = tokio::task::spawn_blocking(move || {
            s.check("shots/a.png")?;
            s.save("shots/a.png", b"png-bytes", "image/png")
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(saved.bytes, 9);
        assert_eq!(std::fs::read(&saved.path).unwrap(), b"png-bytes");
        assert!(saved.path.ends_with("shots/a.png"), "{}", saved.path);
        // The same name again is refused at the pre-flight.
        let s = sink.clone();
        let err = tokio::task::spawn_blocking(move || s.check("shots/a.png")).await.unwrap().unwrap_err();
        assert!(err.contains("exists"), "{err}");
        // A path the string jail refuses never reaches the filesystem.
        let s = sink.clone();
        let err = tokio::task::spawn_blocking(move || s.check("../a.png")).await.unwrap().unwrap_err();
        assert!(err.contains(".."), "{err}");
    }

    #[test]
    fn the_save_cap_is_the_download_and_media_cap() {
        let save = std::hint::black_box(HTTP_SAVE_MAX_BYTES) as u64;
        assert!(save <= std::hint::black_box(FILE_DOWNLOAD_MAX));
        assert_eq!(save, std::hint::black_box(MEDIA_UPLOAD_MAX) as u64);
        assert_eq!(save, 12 * 1024 * 1024);
    }
}
