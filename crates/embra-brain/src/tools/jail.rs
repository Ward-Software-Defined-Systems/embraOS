//! The workspace jail, in one module.
//!
//! Three checks, each named by the question it answers:
//!
//! - [`lexical`] is the string check every workspace tool runs first. A
//!   relative path is joined under the root, a `..` component is refused,
//!   and the result has to start with the root component-wise, so
//!   `/embra/workspace-evil/x` is not under `/embra/workspace`. It never
//!   touches the filesystem; a symlink is invisible to it.
//! - [`real`] is for a path that is written or created. The nearest
//!   existing ancestor is canonicalized and checked against the canonical
//!   root before any directory is created, and the path is rebuilt under
//!   it. A write through a link that stays inside lands on the link's
//!   target; a link that leaves is refused and nothing is created outside;
//!   a dangling link is refused by name.
//! - [`real_parent`] is for a path that is removed or moved away. The
//!   parent goes through [`real`]; the final component stays as named, so
//!   a symlink is unlinked and never followed.
//!
//! [`within`] is the component-wise comparison of a canonical path with a
//! canonical root, for a caller that canonicalized itself (`file_patch`).
//!
//! [`readable`] is the read jail: the model's readers reach every path by
//! its real path except the places that hold the operator's secrets
//! ([`read_denied`]).

use std::path::{Path, PathBuf};

/// The lexical check. Accepts an absolute path under `root` or a path
/// relative to it (`repo`, `./repo` and `<root>/repo` resolve alike); an
/// empty path is the root. Returns the absolute path as text, or a uniform
/// `Denied:` message.
pub(crate) fn lexical(root: &str, path: &str) -> Result<String, String> {
    let trimmed = path.trim();
    let resolved = if trimmed.is_empty() {
        root.to_string()
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        let rel = trimmed.trim_start_matches("./");
        format!("{}/{}", root, rel)
    };
    // `..` is refused outright in either form: `../etc/passwd` and
    // `<root>/../etc/passwd` would both leave the root once the kernel
    // resolved them, and this check is lexical.
    if resolved.split('/').any(|seg| seg == "..") {
        return Err(format!(
            "Denied: path '{}' contains a '..' component",
            resolved
        ));
    }
    // Component-wise, not a string prefix: `/embra/workspace-evil/x` has
    // the root as a prefix of its text and is not under it.
    if !Path::new(&resolved).starts_with(root) {
        return Err(format!(
            "Denied: path '{}' resolves outside {}",
            resolved, root
        ));
    }
    Ok(resolved)
}

/// What [`real`] answers for a path that is about to be written or created.
#[derive(Debug)]
pub(crate) struct Real {
    /// The canonical root, for a caller that checks a directory under it.
    pub(crate) root: PathBuf,
    /// The canonical nearest existing ancestor of the path. A caller
    /// measures free space on it, or checks it against a refused directory.
    pub(crate) ancestor: PathBuf,
    /// The path to operate on: the components that do not exist yet, under
    /// the canonical ancestor; the ancestor itself when the path exists.
    pub(crate) path: PathBuf,
}

/// The canonical check for a path that is written or created. `lexical`
/// has passed [`lexical`] under `root`. Nothing is created here.
pub(crate) async fn real(root: &Path, lexical: &Path) -> Result<Real, String> {
    let root_canon = tokio::fs::canonicalize(root)
        .await
        .map_err(|e| format!("Error: failed to resolve {}: {e}", root.display()))?;
    let anc = nearest_existing_ancestor(lexical).await;
    let ancestor = match tokio::fs::canonicalize(&anc).await {
        Ok(p) => p,
        // Present by `symlink_metadata`, absent by `canonicalize`: a
        // dangling link, which nothing is written through.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "Denied: path '{}' is a dangling symbolic link at {}; file_delete it first",
                lexical.display(),
                anc.display()
            ));
        }
        Err(e) => return Err(format!("Error: failed to resolve {}: {e}", anc.display())),
    };
    if !within(&ancestor, &root_canon) {
        return Err(format!(
            "Denied: path '{}' resolves outside {} (real path {})",
            lexical.display(),
            root.display(),
            ancestor.display()
        ));
    }
    let path = match lexical.strip_prefix(&anc) {
        // An empty remainder (the path itself exists) is not joined:
        // `join("")` appends a separator, and rename(2) refuses `file/`.
        Ok(rest) if !rest.as_os_str().is_empty() => ancestor.join(rest),
        _ => ancestor.clone(),
    };
    Ok(Real { root: root_canon, ancestor, path })
}

/// The canonical check for a path that is removed or moved away: the
/// parent through [`real`], the final component as named. The root itself
/// is refused.
pub(crate) async fn real_parent(root: &Path, lexical: &Path) -> Result<PathBuf, String> {
    let (Some(parent), Some(name)) = (lexical.parent(), lexical.file_name()) else {
        return Err(format!(
            "Denied: path '{}' names no entry under {}",
            lexical.display(),
            root.display()
        ));
    };
    if !parent.starts_with(root) {
        return Err(format!(
            "Denied: path '{}' is the root {} itself; nothing is removed or moved",
            lexical.display(),
            root.display()
        ));
    }
    let parent = real(root, parent).await?;
    Ok(parent.path.join(name))
}

/// Whether a canonical path lies under a canonical root, component-wise.
pub(crate) fn within(canonical: &Path, root: &Path) -> bool {
    canonical.starts_with(root)
}

/// The directory git owns. A writer never creates, changes or removes
/// anything under a `.git` component: a hook planted there would run as
/// root the next time a git tool touched the repository, and a
/// `.git/config` could point git anywhere. Reads are not fenced, and a
/// repository as a whole can still be removed or moved.
pub(crate) const GIT_DIR: &str = ".git";

/// Refuses a path with a `.git` component. Every writer runs it on the
/// path as resolved and on the real path (a link into `.git` is refused by
/// where it leads); the git tools reach the directory through git.
pub(crate) fn fence_git_dir(path: &Path) -> Result<(), String> {
    if path.components().any(|c| c.as_os_str() == GIT_DIR) {
        return Err(format!(
            "Denied: path '{}' is inside a .git directory, which the git tools own; nothing is written there",
            path.display()
        ));
    }
    Ok(())
}

/// Walk up from `p` until a component exists (by `symlink_metadata`, so a
/// dangling link counts as existing and is refused by [`real`] instead of
/// silently built beside).
pub(crate) async fn nearest_existing_ancestor(p: &Path) -> PathBuf {
    let mut cur = p.to_path_buf();
    loop {
        if tokio::fs::symlink_metadata(&cur).await.is_ok() {
            return cur;
        }
        match cur.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => cur = parent.to_path_buf(),
            _ => return PathBuf::from("/"),
        }
    }
}

// ── The read jail ─────────────────────────────────────────────────────────

/// Why a read was refused.
#[derive(Debug)]
pub(crate) enum ReadRefusal {
    /// The path does not exist. The caller words it as it always has.
    NotFound(PathBuf),
    /// Its real path is one the model's readers do not reach.
    Denied(String),
    /// The path could not be resolved for another reason.
    Io(PathBuf, std::io::Error),
}

impl std::fmt::Display for ReadRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadRefusal::NotFound(p) => write!(f, "File not found: {}", p.display()),
            ReadRefusal::Denied(msg) => f.write_str(msg),
            ReadRefusal::Io(p, e) => write!(f, "Failed to resolve {}: {e}", p.display()),
        }
    }
}

/// The read jail, for every path the model reads (`file_read`, `file_copy`'s
/// source, `image_view`, `image_generate`'s references). The path resolves
/// as `file_read` always resolved it (`media::store::resolve_read_path`:
/// absolute as given, relative under the workspace), is canonicalized, and
/// is refused when its real path is one of [`read_denied`]'s. Everything
/// else stays readable: `/etc`, the service logs, the workspace. The path
/// returned is the real one.
pub(crate) async fn readable(path: &str) -> Result<PathBuf, ReadRefusal> {
    let lexical = crate::media::store::resolve_read_path(path);
    // The path as written first: a refused place is refused whether or
    // not the file exists, so a refusal says nothing about what STATE
    // holds. The real path is checked again below, for a link.
    if let Some(reason) = read_denied(&lexical) {
        return Err(ReadRefusal::Denied(format!(
            "Denied: {} is {reason}; nothing is read",
            lexical.display()
        )));
    }
    let canonical = match tokio::fs::canonicalize(&lexical).await {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ReadRefusal::NotFound(lexical));
        }
        Err(e) => return Err(ReadRefusal::Io(lexical, e)),
    };
    if let Some(reason) = read_denied(&canonical) {
        let real = if canonical == lexical {
            String::new()
        } else {
            format!(" (real path {})", canonical.display())
        };
        return Err(ReadRefusal::Denied(format!(
            "Denied: {} is {reason}{real}; nothing is read",
            lexical.display()
        )));
    }
    Ok(canonical)
}

/// The operator's STATE partition.
const STATE_ROOT: &str = "/embra/state";
/// The operator's drop-in directories under STATE, readable: the CA
/// certificates, the seed packs, the importable identities, the models.
const STATE_READABLE_DIRS: &[&str] =
    &["ca-certificates", "seed-knowledge", "imported-intelligence", "models"];
/// The plain STATE files that hold no secret, readable. `halt_reason` is
/// read through the intelligence: the OS has no shell.
const STATE_READABLE_FILES: &[&str] = &["halt_reason", "timezone", "api_provider", "guardian_declined"];

/// The places the model's readers do not reach, by canonical path: the
/// process tables (a service's command line and environment carry keys),
/// STATE except the operator's drop-ins and four plain files, the
/// database's own files, and the workspace SSH key. `Some` names the rule.
pub(crate) fn read_denied(canonical: &Path) -> Option<&'static str> {
    if canonical.starts_with("/proc") {
        return Some("the process tables");
    }
    if canonical.starts_with("/embra/data/wardsondb") {
        return Some("the database files");
    }
    if canonical == Path::new("/embra/workspace/.ssh/id_ed25519") {
        return Some("the workspace SSH key");
    }
    if let Ok(rest) = canonical.strip_prefix(STATE_ROOT) {
        let mut parts = rest.components();
        let first = parts.next().map(|c| c.as_os_str().to_string_lossy().into_owned());
        let readable = match first.as_deref() {
            None => false,
            Some(name) if STATE_READABLE_DIRS.contains(&name) => true,
            Some(name) if STATE_READABLE_FILES.contains(&name) => parts.next().is_none(),
            Some(_) => false,
        };
        if !readable {
            return Some("the operator's STATE partition");
        }
    }
    None
}

// `pub(crate)`: every writer's canonical check (`jail::real`) and
// file_patch's re-verification run against this root.
pub(crate) const WORKSPACE_ROOT: &str = "/embra/workspace";

/// Resolve a workspace-scoped tool path: the lexical half of the jail
/// (`jail::lexical` under `WORKSPACE_ROOT`). Accepts either an absolute
/// path under `/embra/workspace/` or a path relative to `/embra/workspace/`
/// (so `repo`, `./repo`, and `/embra/workspace/repo` all resolve to the
/// same form). Returns the absolute path as text or a uniform `Denied:`
/// rejection message.
///
/// Empty input resolves to the workspace root. Leading `./` is stripped
/// before joining so `./repo` and `repo` are equivalent. `..` segments
/// in either form are rejected outright as a defense against traversal.
/// The check is lexical: a writer runs `jail::real` (or `jail::real_parent`
/// for a removal) on the result before it touches the filesystem, so a
/// symlink that leaves the workspace is refused by its real path.
///
/// Every git_*, file_*, and dir_* tool routes through this resolver so the
/// accepted path shapes are uniform across the tool surface. Closes
/// Embra_Debug #45 (git_* unification) and #52/#53 (file_* / dir_*
/// unification) — prior to these fixes, git_clone took relative subpaths,
/// the rest of the git family required absolute, and the file/dir family
/// required absolute with a looser helper that skipped the `..` check.
/// `pub(crate)` so guardian_call's `data_file` bridge shares the exact same
/// jail (tools/guardian.rs) instead of growing a second resolver.
pub(crate) fn resolve_workspace_path(path: &str) -> Result<String, String> {
    resolve_in(WORKSPACE_ROOT, path)
}

/// The resolver behind [`resolve_workspace_path`], against any root, so the
/// Guardian's workspace sink can run under a temporary directory in a test.
/// Production callers go through `resolve_workspace_path`.
pub(crate) fn resolve_in(root: &str, path: &str) -> Result<String, String> {
    super::jail::lexical(root, path)
}

/// The two jail steps of a writer: the lexical check, then the canonical
/// check of what exists (`jail::real`). The path returned is the one to
/// write or create.
pub(crate) async fn jailed_write_path(root: &str, raw: &str) -> Result<PathBuf, String> {
    let lexical = super::jail::lexical(root, raw)?;
    fence_git_dir(Path::new(&lexical))?;
    let path = super::jail::real(Path::new(root), Path::new(&lexical)).await?.path;
    fence_git_dir(&path)?;
    Ok(path)
}

/// The two jail steps of a remover: the lexical check, then the canonical
/// check of the parent (`jail::real_parent`). The entry itself is named,
/// never followed.
pub(crate) async fn jailed_remove_path(root: &str, raw: &str) -> Result<PathBuf, String> {
    let lexical = super::jail::lexical(root, raw)?;
    fence_git_dir(Path::new(&lexical))?;
    let path = super::jail::real_parent(Path::new(root), Path::new(&lexical)).await?;
    fence_git_dir(&path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Any `.git` component is fenced; `.gitignore` and a repository's own
    /// path are not.
    #[test]
    fn the_fence_refuses_any_git_component() {
        for refused in ["/r/repo/.git/hooks/pre-commit", "/r/repo/.git", "/r/a/.git/config", ".git/x"] {
            let err = fence_git_dir(Path::new(refused)).unwrap_err();
            assert!(err.starts_with("Denied:") && err.contains(".git directory"), "{refused}: {err}");
        }
        for ok in ["/r/repo/.gitignore", "/r/repo/src/x", "/r/repo", "/r/.github/workflows/ci.yml"] {
            assert!(fence_git_dir(Path::new(ok)).is_ok(), "{ok}");
        }
    }

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let d = std::env::temp_dir().join(format!(
                "embra-jail-test-{}-{}-{}",
                tag,
                std::process::id(),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&d).unwrap();
            // Canonical, so the real paths compare component-wise on a
            // host whose temp directory is a symlink.
            TempDir(std::fs::canonicalize(&d).unwrap())
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn link(target: &Path, at: &Path) {
        std::os::unix::fs::symlink(target, at).unwrap();
    }

    #[test]
    fn the_jail_refuses_a_sibling_directory() {
        let root = "/embra/workspace";
        for bad in [
            "/embra/workspace-evil/secret",
            "/embra/workspaceAAA",
            "/embra/workspace2/x",
            "/embra",
            "/etc/passwd",
            "/",
        ] {
            let err = lexical(root, bad).unwrap_err();
            assert!(
                err.starts_with("Denied:") && err.contains("outside"),
                "{bad}: {err}"
            );
        }
        assert_eq!(lexical(root, "/embra/workspace").unwrap(), "/embra/workspace");
        assert_eq!(lexical(root, "/embra/workspace/").unwrap(), "/embra/workspace/");
        assert_eq!(lexical(root, "").unwrap(), "/embra/workspace");
        assert_eq!(lexical(root, "sub/x").unwrap(), "/embra/workspace/sub/x");
        assert_eq!(lexical(root, "./sub").unwrap(), "/embra/workspace/sub");
        assert!(lexical(root, "../x").unwrap_err().contains("'..'"));
        assert!(lexical(root, "/embra/workspace/../x").unwrap_err().contains("'..'"));
    }

    #[tokio::test]
    async fn real_follows_a_symlink_inside_and_refuses_one_that_leaves() {
        let root = TempDir::new("real");
        let outside = TempDir::new("real-outside");
        std::fs::create_dir(root.0.join("inside")).unwrap();
        link(&root.0.join("inside"), &root.0.join("in"));
        link(&outside.0, &root.0.join("esc"));

        let r = real(&root.0, &root.0.join("in/new/file.txt")).await.unwrap();
        assert_eq!(r.root, root.0);
        assert_eq!(r.ancestor, root.0.join("inside"));
        assert_eq!(r.path, root.0.join("inside/new/file.txt"));

        let err = real(&root.0, &root.0.join("esc/new/file.txt")).await.unwrap_err();
        assert!(err.starts_with("Denied:") && err.contains("outside"), "{err}");
        assert!(
            err.contains(&outside.0.display().to_string()),
            "the refusal names the real path: {err}"
        );
        assert!(!outside.0.join("new").exists(), "nothing created outside");
    }

    #[tokio::test]
    async fn real_of_an_existing_target_is_the_target() {
        let root = TempDir::new("exists");
        let f = root.0.join("f.txt");
        std::fs::write(&f, b"x").unwrap();
        let r = real(&root.0, &f).await.unwrap();
        assert_eq!(r.path, f);
        assert_eq!(r.ancestor, f);
        assert!(!r.path.to_str().unwrap().ends_with('/'), "no trailing separator");
        let r = real(&root.0, &root.0).await.unwrap();
        assert_eq!(r.path, root.0);
    }

    #[tokio::test]
    async fn real_refuses_a_dangling_link() {
        let root = TempDir::new("dangling");
        link(&root.0.join("nowhere"), &root.0.join("dangling"));
        let err = real(&root.0, &root.0.join("dangling")).await.unwrap_err();
        assert!(err.starts_with("Denied:") && err.contains("dangling"), "{err}");
        let err = real(&root.0, &root.0.join("dangling/x")).await.unwrap_err();
        assert!(err.contains("dangling"), "{err}");
        assert!(!root.0.join("nowhere").exists());
    }

    #[tokio::test]
    async fn real_parent_keeps_a_link_as_named() {
        let root = TempDir::new("parent");
        let outside = TempDir::new("parent-outside");
        let sentinel = outside.0.join("sentinel.txt");
        std::fs::write(&sentinel, b"keep").unwrap();
        let l = root.0.join("link");
        link(&sentinel, &l);

        // `real` follows the link to the outside and refuses.
        assert!(real(&root.0, &l).await.unwrap_err().contains("outside"));
        // `real_parent` names the link itself.
        assert_eq!(real_parent(&root.0, &l).await.unwrap(), l);
        // A link in the parent chain that leaves is still refused.
        link(&outside.0, &root.0.join("esc"));
        let err = real_parent(&root.0, &root.0.join("esc/sentinel.txt")).await.unwrap_err();
        assert!(err.contains("outside"), "{err}");
        // The root itself is never removed or moved.
        let err = real_parent(&root.0, &root.0).await.unwrap_err();
        assert!(err.contains("root"), "{err}");
    }

    #[test]
    fn within_is_component_wise() {
        let root = Path::new("/embra/workspace");
        assert!(within(Path::new("/embra/workspace/x"), root));
        assert!(within(Path::new("/embra/workspace"), root));
        assert!(!within(Path::new("/embra/workspace-evil/x"), root));
        assert!(!within(Path::new("/embra"), root));
    }
}

#[cfg(test)]
mod read_tests {
    use super::*;

    #[test]
    fn the_readers_refuse_the_state_partition_the_database_and_proc() {
        for (path, reason) in [
            ("/proc", "the process tables"),
            ("/proc/1/cmdline", "the process tables"),
            ("/proc/1/environ", "the process tables"),
            ("/embra/state", "the operator's STATE partition"),
            ("/embra/state/api_key", "the operator's STATE partition"),
            ("/embra/state/api_key_anthropic", "the operator's STATE partition"),
            ("/embra/state/github_token", "the operator's STATE partition"),
            ("/embra/state/soul.sha256", "the operator's STATE partition"),
            ("/embra/state/pki/ca.key", "the operator's STATE partition"),
            ("/embra/state/halt_reason/x", "the operator's STATE partition"),
            ("/embra/data/wardsondb", "the database files"),
            ("/embra/data/wardsondb/fjall/journal", "the database files"),
            ("/embra/workspace/.ssh/id_ed25519", "the workspace SSH key"),
        ] {
            assert_eq!(read_denied(Path::new(path)), Some(reason), "{path}");
        }
        for path in [
            "/etc/hostname",
            "/var/log/embra-brain.log",
            "/embra/workspace/notes.txt",
            "/embra/workspace/.ssh/id_ed25519.pub",
            "/embra/workspace/.ssh",
            "/embra/data/other",
            "/embra/state/ca-certificates/internal.crt",
            "/embra/state/seed-knowledge/pack.json",
            "/embra/state/imported-intelligence/meridian.graph.json",
            "/embra/state/models/m/model.onnx",
            "/embra/state/halt_reason",
            "/embra/state/timezone",
            "/embra/state/api_provider",
            "/embra/state/guardian_declined",
            "/embra/statex/api_key",
            "/procx",
        ] {
            assert_eq!(read_denied(Path::new(path)), None, "{path}");
        }
    }

    #[tokio::test]
    async fn a_read_of_proc_is_refused_end_to_end_and_a_missing_path_is_not_found() {
        // `/proc/self` canonicalizes to `/proc/<pid>` on every Linux host.
        let err = readable("/proc/self/environ").await.unwrap_err();
        assert!(matches!(&err, ReadRefusal::Denied(m) if m.contains("the process tables")), "{err}");
        assert!(err.to_string().starts_with("Denied: /proc/self/environ is the process tables"), "{err}");
        let err = readable("/definitely/not/here").await.unwrap_err();
        assert!(matches!(&err, ReadRefusal::NotFound(p) if p == Path::new("/definitely/not/here")), "{err}");
        assert_eq!(err.to_string(), "File not found: /definitely/not/here");
        let err = readable("relative/not/here").await.unwrap_err();
        assert_eq!(err.to_string(), "File not found: /embra/workspace/relative/not/here");
    }

    /// A refusal is no existence oracle: a path under a refused place is
    /// refused the same way whether or not the file is there (the dev
    /// host has no `/embra/state` at all).
    #[tokio::test]
    async fn a_missing_file_under_a_refused_place_is_refused_not_reported_missing() {
        for path in ["/embra/state/api_key_gemini", "/embra/state/no-such-file", "/embra/data/wardsondb/x"] {
            let err = readable(path).await.unwrap_err();
            assert!(matches!(&err, ReadRefusal::Denied(_)), "{path}: {err}");
            assert!(!err.to_string().contains("not found"), "{path}: {err}");
        }
        // A readable STATE file that is absent is "not found", as before.
        let err = readable("/embra/state/halt_reason").await.unwrap_err();
        assert!(matches!(&err, ReadRefusal::NotFound(_)), "{err}");
    }

    #[tokio::test]
    async fn a_symlink_into_a_refused_place_is_refused_by_its_real_path() {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "embra-read-jail-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("innocent.txt");
        std::os::unix::fs::symlink("/proc/self/environ", &link).unwrap();
        let err = readable(link.to_str().unwrap()).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.starts_with("Denied:") && msg.contains("real path /proc/"), "{msg}");
        let plain = dir.join("plain.txt");
        std::fs::write(&plain, "x").unwrap();
        assert_eq!(readable(plain.to_str().unwrap()).await.unwrap(), std::fs::canonicalize(&plain).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
