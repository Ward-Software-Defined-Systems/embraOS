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
//! The model's reads are not jailed here (`media::store::resolve_read_path`).

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

#[cfg(test)]
mod tests {
    use super::*;

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
