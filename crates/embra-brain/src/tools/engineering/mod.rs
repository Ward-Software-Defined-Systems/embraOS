//! The engineering tools, by domain: `git` (the local git tools and the
//! auth helpers), `github` (`gh_*`), `gitlab` (`gl_*`), `plan` (plans and
//! tasks) and `files` (the file and directory tools), and re-exports what
//! the rest of the crate reaches for. The operator's CA drop-ins, which the
//! GitLab client and the Guardian broker trust, are `crate::net`'s.

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
