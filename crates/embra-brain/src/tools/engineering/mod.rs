//! The engineering tools, by domain: `git` (the local git tools and the
//! auth helpers), `github` (`gh_*`), `gitlab` (`gl_*`), `plan` (plans and
//! tasks) and `files` (the file and directory tools). This module keeps
//! the operator CA drop-ins, shared by the GitLab client and the Guardian
//! broker, and re-exports what the rest of the crate reaches for.

pub mod files;
pub mod git;
pub mod github;
pub mod gitlab;
pub mod plan;

// The children spell the jail as `super::jail`, as the one file did.
use crate::tools::jail;

pub(crate) use crate::tools::jail::{resolve_in, resolve_workspace_path, WORKSPACE_ROOT};
pub(crate) use files::ensure_parent_dirs;
pub(crate) use git::{normalize_git_host, resolve_git_tokens, resolve_github_token};
#[cfg(test)]
pub(crate) use plan::task_delete;

// ---------------------------------------------------------------------------
// GitLab API tools (gl_*) — self-hosted GitLab/instance support.
//
// Mirrors the gh_* set against GitLab's /api/v4. Auth = the per-host PAT
// from `SystemConfig.git_tokens` (`/git-token <host> <token>`), sent as
// `PRIVATE-TOKEN`. Unlike the git tools (which ride the git binary and the
// embrad-merged CA bundle via GIT_SSL_CAINFO), these are reqwest clients
// with compiled-in webpki roots — so the client builder below explicitly
// adds the operator CA drop-ins from /embra/state/ca-certificates/ as
// additional trust anchors. Same dir + EMBRA_CA_DIR override semantics as
// embrad's ca_bundle.rs; certs that fail to parse are skipped (the TLS
// handshake surfaces any real trust problem honestly).
// ---------------------------------------------------------------------------

/// Operator CA drop-in dir (kept in sync with crates/embrad/src/ca_bundle.rs).
const OPERATOR_CA_DIR: &str = "/embra/state/ca-certificates";

const OPERATOR_CA_DIR_ENV: &str = "EMBRA_CA_DIR";

/// Load operator CA certs from the STATE drop-in dir. Unparseable files are
/// skipped with a warn — never fail the call over a bad drop-in; a genuinely
/// missing trust anchor shows up as a TLS error on the request itself.
pub(crate) fn load_operator_ca_certs() -> Vec<reqwest::Certificate> {
    let dir = std::env::var(OPERATOR_CA_DIR_ENV)
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| OPERATOR_CA_DIR.to_string());
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut certs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_cert_ext = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("pem") | Some("crt")
        );
        if !path.is_file() || !is_cert_ext {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        match reqwest::Certificate::from_pem_bundle(&bytes) {
            Ok(parsed) => certs.extend(parsed),
            Err(e) => tracing::warn!(
                "Operator CA file skipped for GitLab client: {}: {}",
                path.display(),
                e
            ),
        }
    }
    certs
}

#[cfg(test)]
mod native_args_tests {
    use super::*;

    #[test]
    fn resolve_workspace_path_accepts_absolute_and_relative() {
        // Embra_Debug #45: git_* tools accept either form, with one
        // resolver doing the joining + the prefix check + traversal
        // rejection in one place.
        assert_eq!(
            resolve_workspace_path("").unwrap(),
            "/embra/workspace"
        );
        assert_eq!(
            resolve_workspace_path("/embra/workspace/repo").unwrap(),
            "/embra/workspace/repo"
        );
        assert_eq!(
            resolve_workspace_path("repo").unwrap(),
            "/embra/workspace/repo"
        );
        assert_eq!(
            resolve_workspace_path("./repo").unwrap(),
            "/embra/workspace/repo"
        );
        assert_eq!(
            resolve_workspace_path("repos/foo/bar").unwrap(),
            "/embra/workspace/repos/foo/bar"
        );

        // Outside the workspace — absolute escape path.
        let denied = resolve_workspace_path("/etc/passwd").unwrap_err();
        assert!(denied.contains("Denied:"), "got: {}", denied);

        // Path traversal — relative form.
        let traversal_rel = resolve_workspace_path("../etc/passwd").unwrap_err();
        assert!(
            traversal_rel.contains("'..'"),
            "expected traversal rejection, got: {}",
            traversal_rel
        );

        // Path traversal — absolute form (would slip past a naive
        // starts_with check before kernel canonicalization).
        let traversal_abs =
            resolve_workspace_path("/embra/workspace/../etc/passwd").unwrap_err();
        assert!(
            traversal_abs.contains("'..'"),
            "expected traversal rejection, got: {}",
            traversal_abs
        );
    }

    #[test]
    fn engineering_tools_register() {
        let names: Vec<&'static str> = inventory::iter::<crate::tools::registry::ToolDescriptor>()
            .map(|d| d.name)
            .collect();
        for expected in [
            "git_clone", "git_status", "git_log", "git_add", "git_commit",
            "git_push", "git_pull", "git_fetch", "git_diff", "git_branch", "git_checkout",
            "git_merge", "git_rm", "git_mv",
            "plan", "plan_delete", "tasks", "task_add", "task_done", "task_delete",
            "gh_issues", "gh_issue_view", "gh_prs", "gh_pr_view",
            "gh_issue_create", "gh_issue_close",
            "gh_issue_reopen", "gh_issue_comment",
            "gh_pr_create", "gh_pr_close", "gh_pr_comment", "gh_pr_merge",
            "gh_project_list", "gh_project_view",
            "file_read", "file_write", "file_append", "file_delete",
            "file_move", "file_rename", "file_symlink",
            "dir_delete", "rmdir", "mkdir", "file_patch", "file_copy",
        ] {
            assert!(
                names.contains(&expected),
                "{} not registered in tool inventory",
                expected
            );
        }
    }
}

#[cfg(test)]
mod git_token_tests {
    use super::*;

    #[test]
    fn load_operator_ca_certs_skips_garbage_and_missing_dir() {
        let dir = std::env::temp_dir().join(format!("brain-ca-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("garbage.pem"), "not a certificate").unwrap();
        std::fs::write(dir.join("ignored.txt"), "also not").unwrap();
        // EMBRA_CA_DIR override scopes the scan to the temp dir. Env mutation
        // is process-global; this is the only test touching this var.
        unsafe { std::env::set_var(OPERATOR_CA_DIR_ENV, &dir) };
        let certs = load_operator_ca_certs();
        unsafe { std::env::remove_var(OPERATOR_CA_DIR_ENV) };
        std::fs::remove_dir_all(&dir).ok();
        assert!(certs.is_empty(), "garbage PEMs must be skipped, not error");
    }
}

#[cfg(test)]
mod user_agent_tests {
    use std::path::{Path, PathBuf};

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// Every `User-Agent` a tool sends is `embra_tools_core::USER_AGENT`,
    /// never a literal: the version in it is the workspace's and moves with
    /// a release, and the Guardian broker sends the same string.
    #[test]
    fn every_user_agent_header_in_the_tools_is_the_shared_constant() {
        let mut files = Vec::new();
        rust_files(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
        let mut sites = 0usize;
        let mut literals = Vec::new();
        for path in files {
            let text = std::fs::read_to_string(&path).unwrap();
            for (n, line) in text.lines().enumerate() {
                let lower = line.to_ascii_lowercase();
                if !lower.contains(".header(") || !lower.contains("user-agent") {
                    continue;
                }
                // `.header("User-Agent", <value>)`: the value is the second argument.
                let value = line.split(',').nth(1).map(str::trim).unwrap_or("");
                if value.starts_with('"') {
                    literals.push(format!("{}:{}", path.display(), n + 1));
                } else {
                    sites += 1;
                }
            }
        }
        assert!(literals.is_empty(), "a literal user agent at {literals:?}; send embra_tools_core::USER_AGENT");
        assert!(sites > 0, "no User-Agent header under src/");
    }
}
