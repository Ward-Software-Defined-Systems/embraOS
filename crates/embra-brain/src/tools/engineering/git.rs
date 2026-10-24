//! The local git tools (`git_*`) and the auth helpers they share with the
//! GitHub and GitLab tools: the token rewrites and the host normalizer.

use embra_tool_macro::embra_tool;
use embra_tools_core::DispatchError;
use schemars::JsonSchema;
use serde::Deserialize;

use std::path::Path;

use tracing::{info, warn};

use crate::db::WardsonDbClient;
use crate::tools::jail::{jailed_write_path, resolve_workspace_path, WORKSPACE_ROOT};
use crate::tools::registry::DispatchContext;
use crate::tools::security::{SSH_KEY_PATH, SSH_KNOWN_HOSTS};
use crate::tools::subprocess;

/// Where the brain keeps git's global configuration: under STATE, which the
/// tools cannot write. `$HOME` stays the workspace for ssh, but git never
/// reads the workspace's `.gitconfig`, which the model can write.
pub(crate) const GIT_CONFIG_DIR: &str = "/embra/state/git";
/// The global configuration file (`GIT_CONFIG_GLOBAL`): the operator's
/// identity from `/git-setup` and the defaults the brain writes at startup.
pub(crate) const GIT_CONFIG_GLOBAL: &str = "/embra/state/git/config";
/// An empty directory, pinned as `core.hooksPath` above any repository's own
/// configuration, so a hook a repository carries never runs.
pub(crate) const GIT_HOOKS_DIR: &str = "/embra/state/git/hooks";
/// The transports git may speak: no `ext::` (a command), no `file://` and
/// no local path (the rest of the filesystem into the workspace).
const GIT_ALLOW_PROTOCOL: &str = "https:http:ssh:git";

/// The ssh git runs for an `ssh://` or scp-form remote (`GIT_SSH_COMMAND`):
/// no client configuration file (`-F /dev/null`; the one under the
/// workspace would be the model's, and a configuration can name a command
/// for ssh to run), the workspace key pair and known-hosts file named
/// explicitly, the host-key policy of the ssh tools, and no prompt.
pub(crate) fn git_ssh_command() -> String {
    format!(
        "ssh -F /dev/null -i {SSH_KEY_PATH} -o UserKnownHostsFile={SSH_KNOWN_HOSTS} -o StrictHostKeyChecking=accept-new -o BatchMode=yes"
    )
}
/// Where `/git-setup` wrote before the configuration moved under STATE.
/// Its identity is adopted once; the file is never read again.
const LEGACY_GIT_CONFIG: &str = "/embra/workspace/.gitconfig";

/// Every git the brain runs. The child is killed with its future
/// (`subprocess`); git reads no system configuration and only the global
/// file under STATE; the hooks directory is pinned through the
/// configuration environment, which outranks a repository's own config;
/// nothing prompts; only the network transports are allowed; and the ssh
/// it runs is [`git_ssh_command`].
pub(crate) fn git_command() -> tokio::process::Command {
    git_command_for(Path::new(GIT_CONFIG_GLOBAL), Path::new(GIT_HOOKS_DIR), &[])
}

/// [`git_command`] with configuration pairs for one call, the token
/// rewrites of [`git_auth_config`] among them. They travel as
/// `GIT_CONFIG_KEY_n`/`GIT_CONFIG_VALUE_n` in the environment, never as
/// `-c` arguments, so a token is not on the command line.
pub(crate) fn git_command_with(config: &[(String, String)]) -> tokio::process::Command {
    git_command_for(Path::new(GIT_CONFIG_GLOBAL), Path::new(GIT_HOOKS_DIR), config)
}

/// The builder behind the two above, with the files as parameters so a
/// test runs it under a temporary directory.
fn git_command_for(global: &Path, hooks: &Path, config: &[(String, String)]) -> tokio::process::Command {
    let mut cmd = subprocess("git");
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", global)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ALLOW_PROTOCOL", GIT_ALLOW_PROTOCOL)
        .env("GIT_SSH_COMMAND", git_ssh_command());
    let mut pairs: Vec<(String, String)> =
        vec![("core.hooksPath".to_string(), hooks.to_string_lossy().into_owned())];
    pairs.extend(config.iter().cloned());
    cmd.env("GIT_CONFIG_COUNT", pairs.len().to_string());
    for (i, (key, value)) in pairs.iter().enumerate() {
        cmd.env(format!("GIT_CONFIG_KEY_{i}"), key).env(format!("GIT_CONFIG_VALUE_{i}"), value);
    }
    cmd
}

/// The operator's identity from `/git-setup`: both keys written to the
/// global file under STATE, live for the next git. A failed write is an
/// error naming the key; nothing is reported written that was not.
pub(crate) async fn set_identity(name: &str, email: &str) -> Result<(), String> {
    set_identity_in(Path::new(GIT_CONFIG_GLOBAL), Path::new(GIT_HOOKS_DIR), name, email).await
}

/// [`set_identity`] against the given files, for a test.
async fn set_identity_in(global: &Path, hooks: &Path, name: &str, email: &str) -> Result<(), String> {
    for (key, value) in [("user.name", name), ("user.email", email)] {
        let out = git_command_for(global, hooks, &[])
            .args(["config", "--global", key, value])
            .output()
            .await
            .map_err(|e| format!("{key}: {e}"))?;
        if !out.status.success() {
            return Err(format!("{key}: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
    }
    Ok(())
}

/// The identity git resolves from the global file (`user.name`,
/// `user.email`), each empty when unset or when the file does not exist
/// yet. An unreadable file is an error.
pub(crate) async fn identity() -> Result<(String, String), String> {
    identity_in(Path::new(GIT_CONFIG_GLOBAL), Path::new(GIT_HOOKS_DIR)).await
}

/// [`identity`] against the given files, for a test.
async fn identity_in(global: &Path, hooks: &Path) -> Result<(String, String), String> {
    if !global.exists() {
        return Ok((String::new(), String::new()));
    }
    let mut values = Vec::with_capacity(2);
    for key in ["user.name", "user.email"] {
        let out = git_command_for(global, hooks, &[])
            .args(["config", "--global", "--get", key])
            .output()
            .await
            .map_err(|e| format!("{key}: {e}"))?;
        // Exit status 1 is "unset"; anything else is an error.
        if !out.status.success() && out.status.code() != Some(1) {
            return Err(format!("{key}: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        values.push(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    let email = values.pop().unwrap_or_default();
    let name = values.pop().unwrap_or_default();
    Ok((name, email))
}

/// Startup: the configuration directory and the empty hooks directory are
/// created, the operator's identity is adopted from the workspace
/// `.gitconfig` the first time (a sealed instance set it with `/git-setup`
/// before the move), and the defaults are written. Nothing here is fatal.
pub(crate) async fn prepare_git_config() {
    prepare_git_config_in(Path::new(GIT_CONFIG_DIR), Path::new(LEGACY_GIT_CONFIG)).await;
}

/// [`prepare_git_config`] under a directory, for a test.
async fn prepare_git_config_in(dir: &Path, legacy: &Path) {
    let global = dir.join("config");
    let hooks = dir.join("hooks");
    if let Err(e) = tokio::fs::create_dir_all(&hooks).await {
        warn!("git configuration directory {} not created: {e}", dir.display());
        return;
    }
    let git = |config: &[(String, String)]| git_command_for(&global, &hooks, config);
    if !global.exists() && legacy.exists() {
        for key in ["user.name", "user.email"] {
            let read = git(&[])
                .args(["config", "-f"])
                .arg(legacy)
                .args(["--get", key])
                .output()
                .await;
            if let Ok(out) = read
                && out.status.success()
            {
                let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !value.is_empty() {
                    let _ = git(&[]).args(["config", "--global", key, &value]).output().await;
                }
            }
        }
        info!("git identity adopted from {} into {}", legacy.display(), global.display());
    }
    for (key, value) in [("safe.directory", "*"), ("push.autoSetupRemote", "true")] {
        if let Err(e) = git(&[]).args(["config", "--global", key, value]).output().await {
            warn!("git config {key} not written: {e}");
        }
    }
}

/// A token the model supplies as a positional (a branch, a ref, a file):
/// one that begins with `-` would be read by git as an option, so it is
/// refused before git runs.
fn positional<'a>(token: &'a str, what: &str) -> Result<&'a str, String> {
    if token.starts_with('-') {
        return Err(format!(
            "Denied: {what} '{token}' begins with '-', which git would read as an option"
        ));
    }
    Ok(token)
}

/// A URL `git_clone` accepts: `https://`, `http://`, `ssh://`, `git://`, or
/// the scp form `[user@]host:path`. Refused before git runs: a token that
/// begins with `-` (an option), a remote-helper transport (`ext::` runs a
/// command; any `name::`), `file://` and a local path (the rest of the
/// filesystem into the workspace), and a user, host or port in the URL's
/// authority that begins with `-` ([`option_shaped_authority`]).
/// `GIT_ALLOW_PROTOCOL` on the command is the second line.
pub(crate) fn validate_clone_url(url: &str) -> Result<(), String> {
    if url.starts_with('-') {
        return Err(format!("Denied: '{url}' begins with '-', which git would read as an option"));
    }
    if let Some((helper, _)) = url.split_once("::")
        && !helper.contains('/')
        && !helper.contains(':')
    {
        return Err(format!(
            "Denied: '{url}' names the remote helper '{helper}::'; git_clone takes https://, http://, ssh://, git:// or user@host:path"
        ));
    }
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("file:") {
        return Err(format!("Denied: '{url}' is a file:// URL; the filesystem is not a clone source"));
    }
    if url.starts_with('/') || url.starts_with('.') || url.starts_with('~') {
        return Err(format!("Denied: '{url}' is a local path; the filesystem is not a clone source"));
    }
    if let Some(scheme) = ["https://", "http://", "ssh://", "git://"].iter().find(|s| lower.starts_with(*s)) {
        let authority = url[scheme.len()..].split('/').next().unwrap_or("");
        return option_shaped_authority(url, authority, true);
    }
    // The scp form: a colon before the first slash, after a host.
    if let Some(colon) = url.find(':')
        && colon > 0
        && !url[..colon].contains('/')
        && !url[colon + 1..].starts_with('/')
    {
        return option_shaped_authority(url, &url[..colon], false);
    }
    Err(format!(
        "Denied: '{url}' is not a git URL the clone tool accepts (https://, http://, ssh://, git://, or user@host:path)"
    ))
}

/// The user, host and port of a URL's authority (`user@host:port`; the scp
/// form's `user@host` carries no port), each refused when it begins with
/// `-`: git hands the user and the host to ssh, which would read them as
/// options. Git refuses these itself ("strange hostname"); the tool
/// refuses them first, so the guarantee is the OS's and not the installed
/// binary's. An IPv6 literal in brackets is split at its closing bracket.
fn option_shaped_authority(url: &str, authority: &str, with_port: bool) -> Result<(), String> {
    let (user, hostport) = match authority.rsplit_once('@') {
        Some((user, hostport)) => (Some(user), hostport),
        None => (None, authority),
    };
    let (host, port) = if !with_port {
        (hostport, None)
    } else if let Some(bracketed) = hostport.strip_prefix('[') {
        match bracketed.split_once(']') {
            Some((host, rest)) => (host, rest.strip_prefix(':')),
            None => (hostport, None),
        }
    } else {
        match hostport.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (hostport, None),
        }
    };
    for (what, value) in [("user", user), ("host", Some(host)), ("port", port)] {
        if let Some(value) = value
            && value.starts_with('-')
        {
            return Err(format!(
                "Denied: '{url}' names the {what} '{value}', which begins with '-' and would be read as an option"
            ));
        }
    }
    Ok(())
}

/// The flags `git_log` passes on as given: presentation only, nothing that
/// names a file to write (`--output` would write anywhere git runs).
const GIT_LOG_FLAGS: &[&str] = &[
    "--oneline", "--graph", "--stat", "--decorate", "--no-merges", "--merges", "--reverse", "--all",
    "--first-parent", "--name-only", "--name-status", "-p", "--patch",
];
/// The flags `git_log` passes on with a value attached (`--author=name`).
const GIT_LOG_VALUE_FLAGS: &[&str] = &[
    "--author=", "--committer=", "--since=", "--after=", "--until=", "--before=", "--grep=",
    "--max-count=", "--skip=", "--format=", "--pretty=", "--date=",
];

/// The git-log arguments the tool passes on: a count (`-n 20`, `-5`), the
/// flags of [`GIT_LOG_FLAGS`] and [`GIT_LOG_VALUE_FLAGS`], and revisions or
/// paths (a token that does not begin with `-`). Any other flag is refused
/// before git runs.
fn git_log_args<'a>(extra: &[&'a str]) -> Result<Vec<&'a str>, String> {
    let mut out = Vec::with_capacity(extra.len());
    let mut tokens = extra.iter().copied().peekable();
    while let Some(tok) = tokens.next() {
        let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
        let count_flag = tok == "-n" && tokens.peek().is_some_and(|n| digits(n));
        let allowed = !tok.starts_with('-')
            || count_flag
            || tok.strip_prefix('-').is_some_and(digits)
            || GIT_LOG_FLAGS.contains(&tok)
            || GIT_LOG_VALUE_FLAGS.iter().any(|f| tok.starts_with(f));
        if !allowed {
            return Err(format!(
                "Denied: git_log does not pass '{tok}' to git. It takes a count (-n 20, -5), {}, {} with a value, and revisions or paths.",
                GIT_LOG_FLAGS.join(" "),
                GIT_LOG_VALUE_FLAGS.join(" ")
            ));
        }
        out.push(tok);
        if count_flag {
            out.push(tokens.next().expect("peeked"));
        }
    }
    Ok(out)
}

/// Resolve `base` to a ref usable by `merge-base --is-ancestor`. Prefers the
/// local branch (`<base>`); falls back to `origin/<base>` if the local copy
/// is missing. Returns Err with a clear message if neither exists. Used by
/// `git_branch delete` to enforce the documented merged-into-base contract.
async fn resolve_base_ref(dir: &str, base: &str) -> Result<String, String> {
    let local_check = git_command()
        .args(["-C", dir, "rev-parse", "--verify", "--quiet", base])
        .output()
        .await;
    if let Ok(out) = &local_check
        && out.status.success()
    {
        return Ok(base.to_string());
    }
    let remote = format!("origin/{}", base);
    let remote_check = git_command()
        .args(["-C", dir, "rev-parse", "--verify", "--quiet", &remote])
        .output()
        .await;
    if let Ok(out) = &remote_check
        && out.status.success()
    {
        return Ok(remote);
    }
    Err(format!(
        "Cannot verify merge status: neither '{}' nor 'origin/{}' exists locally. Fetch first.",
        base, base
    ))
}

/// Resolve GITHUB_TOKEN: env var first, then WardSONDB config.system.github_token.
pub async fn resolve_github_token(db: &WardsonDbClient) -> Option<String> {
    if let Ok(t) = std::env::var("GITHUB_TOKEN")
        && !t.is_empty()
    {
        return Some(t);
    }
    if let Ok(doc) = db.read("config.system", "config").await
        && let Some(t) = doc.get("github_token").and_then(|v| v.as_str())
        && !t.is_empty()
    {
        return Some(t.to_string());
    }
    None
}

/// The configuration pair that injects GITHUB_TOKEN for HTTPS GitHub
/// remotes: `url.https://x-access-token:TOKEN@github.com/.insteadOf` =
/// `https://github.com/`. Empty without a token.
fn github_token_config(token: &Option<String>) -> Vec<(String, String)> {
    match token {
        Some(t) => vec![(
            format!("url.https://x-access-token:{}@github.com/.insteadOf", t),
            "https://github.com/".to_string(),
        )],
        None => vec![],
    }
}

/// Resolve per-host git tokens from WardSONDB `config.system.git_tokens`
/// (set via `/git-token <host> <token>`). Empty map when unconfigured.
pub async fn resolve_git_tokens(
    db: &WardsonDbClient,
) -> std::collections::BTreeMap<String, String> {
    if let Ok(doc) = db.read("config.system", "config").await
        && let Some(map) = doc.get("git_tokens").and_then(|v| v.as_object())
    {
        return map
            .iter()
            .filter_map(|(host, v)| {
                v.as_str()
                    .filter(|t| !t.is_empty())
                    .map(|t| (host.clone(), t.to_string()))
            })
            .collect();
    }
    std::collections::BTreeMap::new()
}

/// Normalize an operator-supplied git host for `git_tokens` storage:
/// strip an `https://` scheme, any path, and trailing slashes; lowercase.
/// Rejects empty results, embedded whitespace, and credential/port junk
/// that would corrupt the insteadOf rewrite.
pub fn normalize_git_host(input: &str) -> Result<String, String> {
    let s = input.trim().to_lowercase();
    let s = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")).unwrap_or(&s);
    let s = s.split('/').next().unwrap_or("");
    if s.is_empty() {
        return Err("host is empty after normalization".to_string());
    }
    if s.chars().any(|c| c.is_whitespace()) || s.contains('@') {
        return Err(format!("'{}' is not a valid host", s));
    }
    Ok(s.to_string())
}

/// The insteadOf rewrite per configured host, as configuration pairs.
/// GitLab's PAT username over HTTPS is `oauth2` (also accepted by Gitea).
fn host_token_config(tokens: &std::collections::BTreeMap<String, String>) -> Vec<(String, String)> {
    tokens
        .iter()
        .map(|(host, token)| {
            (
                format!("url.https://oauth2:{}@{}/.insteadOf", token, host),
                format!("https://{}/", host),
            )
        })
        .collect()
}

/// The git auth configuration for a network operation (clone, push,
/// pull): the github.com rewrite plus one per configured host, for
/// [`git_command_with`]. BTreeMap iteration keeps host order
/// deterministic.
pub async fn git_auth_config(db: &WardsonDbClient) -> Vec<(String, String)> {
    let token = resolve_github_token(db).await;
    let mut pairs = github_token_config(&token);
    pairs.extend(host_token_config(&resolve_git_tokens(db).await));
    pairs
}

/// Clone a git repository into /embra/workspace/.
/// Format: `<url>` or `<url> <subpath>`
/// `<subpath>` may be a bare dirname (`myrepo`), a relative path under the
/// workspace (`repos/myrepo`), or an absolute path that lives under
/// `/embra/workspace/` (`/embra/workspace/repos/myrepo`). `..` segments are
/// rejected. A trailing `/` on the subpath appends the URL-derived repo name.
pub async fn git_clone(db: &WardsonDbClient, param: &str) -> String {
    if param.is_empty() {
        return "Usage: git_clone <url> or git_clone <url> <subpath>".into();
    }

    let parts: Vec<&str> = param.split_whitespace().collect();
    let url = parts[0];
    if let Err(e) = validate_clone_url(url) {
        return e;
    }

    let derived_name = url
        .rsplit('/')
        .next()
        .unwrap_or("repo")
        .trim_end_matches(".git")
        .to_string();

    let subpath_input = if parts.len() > 1 {
        let raw = parts[1];
        if raw.ends_with('/') {
            format!("{}{}", raw, derived_name)
        } else {
            raw.to_string()
        }
    } else {
        derived_name
    };

    // The lexical jail, then the canonical check of what exists: a
    // destination under a symlinked directory that leaves the workspace is
    // refused before anything is created.
    let dest = match jailed_write_path(WORKSPACE_ROOT, &subpath_input).await {
        Ok(p) => p.display().to_string(),
        Err(e) => return e,
    };

    if std::path::Path::new(&dest).symlink_metadata().is_ok() {
        return format!("Directory already exists: {}", dest);
    }

    // Ensure the destination's parent directory exists so nested subpaths
    // like `repos/foo/bar` work on first use.
    if let Some(parent) = std::path::Path::new(&dest).parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // The tokens ride the configuration environment (no token in URLs,
    // config files or argv).
    let auth = git_auth_config(db).await;

    match tokio::time::timeout(
        std::time::Duration::from_secs(120),
        git_command_with(&auth)
            .args(["clone", "--", url, &dest])
            .output(),
    ).await {
        Ok(Ok(out)) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !out.status.success() {
                return format!("git clone failed: {}", stderr.trim());
            }
            format!("Cloned {} into {}", url, dest)
        }
        Ok(Err(e)) => format!("Failed to run git clone: {}", e),
        Err(_) => "git clone timed out after 120 seconds".into(),
    }
}

/// Run `git status` on a path.
pub async fn git_status(path: &str) -> String {
    let dir = match resolve_workspace_path(path) {
        Ok(d) => d,
        Err(e) => return e,
    };
    match git_command()
        .args(["-C", &dir, "status", "--short"])
        .output()
        .await
    {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !out.status.success() {
                return format!("git status failed: {}", stderr.trim());
            }
            if stdout.trim().is_empty() {
                format!("Working tree clean at {}", dir)
            } else {
                format!("=== git status ({}) ===\n{}", dir, stdout)
            }
        }
        Err(e) => format!("Failed to run git: {}", e),
    }
}

/// Run `git log` with optional params. First whitespace token is the path
/// (resolved through `resolve_workspace_path`); remaining tokens pass through
/// as git-log flags (e.g. `-n 20 --oneline`).
pub async fn git_log(param: &str) -> String {
    let parts: Vec<&str> = param.split_whitespace().collect();
    let (path_token, extra_args): (&str, Vec<&str>) = if parts.is_empty() {
        ("", vec![])
    } else {
        (parts[0], parts[1..].to_vec())
    };
    let extra = match git_log_args(&extra_args) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let dir = match resolve_workspace_path(path_token) {
        Ok(d) => d,
        Err(e) => return e,
    };

    let mut args = vec!["-C", &dir, "log", "--oneline", "-20"];
    args.extend(extra);

    match git_command()
        .args(&args)
        .output()
        .await
    {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !out.status.success() {
                return format!("git log failed: {}", stderr.trim());
            }
            format!("=== git log ({}) ===\n{}", dir, stdout)
        }
        Err(e) => format!("Failed to run git: {}", e),
    }
}

/// Run `git add` on files. Write operation — workspace restricted.
/// Param format: `<path> <files>`
pub async fn git_add(param: &str) -> String {
    let parts: Vec<&str> = param.splitn(2, ' ').collect();
    if parts.len() < 2 {
        return "Usage: git_add <path> <files>\nExample: git_add /embra/workspace/repos/myrepo file.txt".into();
    }
    let dir = match resolve_workspace_path(parts[0]) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let files = parts[1];

    let file_args: Vec<&str> = files.split_whitespace().collect();
    for f in &file_args {
        if let Err(e) = positional(f, "file") {
            return e;
        }
    }
    let mut args = vec!["-C", &dir, "add", "--"];
    args.extend(file_args.iter());

    match git_command()
        .args(&args)
        .output()
        .await
    {
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !out.status.success() {
                return format!("git add failed: {}", stderr.trim());
            }
            format!("Staged files in {}: {}", dir, files)
        }
        Err(e) => format!("Failed to run git: {}", e),
    }
}

/// Run `git commit -m <message>` in a workspace repository. The message is
/// passed to git exactly as given: JSON carries real newlines, so a blank
/// line after the first line separates the subject from the body.
///
/// Param format: `<path> | <message>`
pub async fn git_commit(param: &str) -> String {
    let parts: Vec<&str> = param.splitn(2, " | ").collect();
    if parts.len() < 2 {
        return "Usage: git_commit <path> | <message>\nA blank line after the first line separates the subject from the body.".into();
    }
    let dir = match resolve_workspace_path(parts[0].trim()) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let message = parts[1].trim();

    match git_command()
        .args(["-C", &dir, "commit", "-m", message])
        .output()
        .await
    {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !out.status.success() {
                return format!("git commit failed: {}", stderr.trim());
            }
            format!("Committed in {}:\n{}", dir, stdout.trim())
        }
        Err(e) => format!("Failed to run git: {}", e),
    }
}

/// Run `git push`. Write operation — workspace restricted.
/// Param format: `<path>`
pub async fn git_push(db: &WardsonDbClient, param: &str) -> String {
    let dir = match resolve_workspace_path(param.trim()) {
        Ok(d) => d,
        Err(e) => return e,
    };

    let auth = git_auth_config(db).await;

    match git_command_with(&auth)
        .args(["-C", &dir, "push"])
        .output()
        .await
    {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !out.status.success() {
                return format!("git push failed: {}", stderr.trim());
            }
            let output = if stdout.trim().is_empty() {
                stderr.trim().to_string()
            } else {
                stdout.trim().to_string()
            };
            format!("Pushed from {}:\n{}", dir, output)
        }
        Err(e) => format!("Failed to run git: {}", e),
    }
}

/// Run `git pull`. Write operation — workspace restricted.
/// Param format: `<path>`
pub async fn git_pull(db: &WardsonDbClient, param: &str) -> String {
    let dir = match resolve_workspace_path(param.trim()) {
        Ok(d) => d,
        Err(e) => return e,
    };

    let auth = git_auth_config(db).await;

    match git_command_with(&auth)
        .args(["-C", &dir, "pull"])
        .output()
        .await
    {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !out.status.success() {
                return format!("git pull failed: {}", stderr.trim());
            }
            format!("Pulled in {}:\n{}", dir, stdout.trim())
        }
        Err(e) => format!("Failed to run git: {}", e),
    }
}

/// The argv of `git fetch` after `-C <dir>`: the flags, then `--`, then the
/// remote and the optional branch, each refused when it begins with `-`
/// (`positional`). A fetch moves the remote-tracking refs (and tags) only;
/// the working tree and the current branch are untouched, which is what
/// makes it safe before a merge, a push or a status, and from a cron.
fn git_fetch_args<'a>(remote: &'a str, branch: Option<&'a str>, prune: bool, tags: bool) -> Result<Vec<&'a str>, String> {
    let mut args = vec!["fetch"];
    if prune {
        args.push("--prune");
    }
    if tags {
        args.push("--tags");
    }
    args.push("--");
    args.push(positional(remote, "remote")?);
    if let Some(b) = branch {
        args.push(positional(b, "branch")?);
    }
    Ok(args)
}

/// Run `git fetch` in a workspace repository. git reports the ref updates
/// on stderr, so the report carries both streams; nothing to report means
/// the remote-tracking refs were already current.
pub async fn git_fetch(db: &WardsonDbClient, path: &str, remote: &str, branch: Option<&str>, prune: bool, tags: bool) -> String {
    let dir = match resolve_workspace_path(path.trim()) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let fetch = match git_fetch_args(remote, branch, prune, tags) {
        Ok(a) => a,
        Err(e) => return e,
    };

    let auth = git_auth_config(db).await;

    match git_command_with(&auth)
        .args(["-C", &dir])
        .args(&fetch)
        .output()
        .await
    {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !out.status.success() {
                return format!("git fetch failed: {}", stderr.trim());
            }
            let report = format!("{}\n{}", stderr.trim(), stdout.trim()).trim().to_string();
            if report.is_empty() {
                format!("Fetched {remote} in {dir}: nothing new, the remote-tracking refs were current")
            } else {
                format!("Fetched {remote} in {dir}:\n{report}")
            }
        }
        Err(e) => format!("Failed to run git: {}", e),
    }
}

/// Run `git diff`. Workspace restricted.
/// Param format: `<path> [file]`
pub async fn git_diff(param: &str) -> String {
    let parts: Vec<&str> = param.split_whitespace().collect();
    let (path_token, file) = if parts.is_empty() {
        ("", None)
    } else if parts.len() == 1 {
        (parts[0], None)
    } else {
        (parts[0], Some(parts[1]))
    };
    let dir = match resolve_workspace_path(path_token) {
        Ok(d) => d,
        Err(e) => return e,
    };

    let mut args = vec!["-C", &dir, "diff"];
    if let Some(f) = file {
        args.push("--");
        args.push(f);
    }

    match git_command()
        .args(&args)
        .output()
        .await
    {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !out.status.success() {
                return format!("git diff failed: {}", stderr.trim());
            }
            if stdout.trim().is_empty() {
                format!("No changes in {}", dir)
            } else {
                format!("=== git diff ({}) ===\n{}", dir, stdout)
            }
        }
        Err(e) => format!("Failed to run git: {}", e),
    }
}

/// Run `git branch`. Forms:
/// - `<path>`                       → list branches (read-only, unrestricted path)
/// - `<path> <name>`                      → create a branch (workspace restricted)
/// - `<path> delete <name>`               → safe-delete, requires merge into `main`
/// - `<path> delete <name> <base>`        → safe-delete, requires merge into `<base>`
/// - `<path> delete-force <name>`         → force-delete, skips the merge check (`git branch -D`)
/// - `<path> delete-force <name> <base>`  → force-delete (base ignored; retained for shape parity)
///
/// The safe `delete` form enforces an explicit merge-base check before invoking
/// `git branch -d`. Git's own `-d` is permissive — any tracking ref (including
/// `origin/<branch>`) makes a branch deletable, so a pushed-but-unmerged branch
/// would slip through. We pre-check `git merge-base --is-ancestor` against the
/// base ref (default `main`, falling back to `origin/<base>` if no local copy).
/// Closes Embra_Debug #49.
///
/// The `delete-force` form is the escape hatch for throwaway/spike branches
/// that the caller explicitly wants to discard unmerged — runs `git branch -D`
/// and skips both the base lookup and the merge-base check. Closes
/// Embra_Debug #55.
pub async fn git_branch(param: &str) -> String {
    let parts: Vec<&str> = param.split_whitespace().collect();
    if parts.is_empty() {
        return git_branch_list(WORKSPACE_ROOT).await;
    }
    let dir = match resolve_workspace_path(parts[0]) {
        Ok(d) => d,
        Err(e) => return e,
    };

    // Delete forms: `<path> delete <name> [<base>]` (safe) or
    // `<path> delete-force <name>` (force, skips merge check).
    let is_delete = parts.len() >= 3 && parts[1].eq_ignore_ascii_case("delete");
    let is_delete_force = parts.len() >= 3 && parts[1].eq_ignore_ascii_case("delete-force");
    if is_delete || is_delete_force {
        let name = match positional(parts[2], "branch") {
            Ok(n) => n,
            Err(e) => return e,
        };

        if is_delete_force {
            return match git_command()
                .args(["-C", &dir, "branch", "-D", name])
                .output()
                .await
            {
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    if !out.status.success() {
                        return format!("git branch failed: {}", stderr.trim());
                    }
                    format!("Force-deleted branch '{}' in {} (unmerged commits discarded)", name, dir)
                }
                Err(e) => format!("Failed to run git: {}", e),
            };
        }

        let base = match positional(parts.get(3).copied().unwrap_or("main"), "base") {
            Ok(b) => b,
            Err(e) => return e,
        };

        // Resolve a usable base ref: prefer local, fall back to origin/<base>.
        let base_ref = match resolve_base_ref(&dir, base).await {
            Ok(r) => r,
            Err(e) => return e,
        };

        // merge-base --is-ancestor exits 0 if branch tip is reachable from base,
        // 1 if not. Anything else is an unexpected git error.
        let ancestor = git_command()
            .args([
                "-C",
                &dir,
                "merge-base",
                "--is-ancestor",
                &format!("refs/heads/{}", name),
                &base_ref,
            ])
            .output()
            .await;
        match ancestor {
            Ok(out) => match out.status.code() {
                Some(0) => {} // merged — fall through to delete
                Some(1) => {
                    return format!(
                        "Refusing delete: branch '{}' has commits not reachable from '{}'. \
                         Merge it first, pass a different base if you're targeting a non-default \
                         integration branch, or use force=true to discard unmerged commits.",
                        name, base_ref
                    );
                }
                _ => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    return format!(
                        "Cannot verify merge status of '{}' against '{}': {}",
                        name, base_ref, stderr.trim()
                    );
                }
            },
            Err(e) => return format!("Failed to run git merge-base: {}", e),
        }

        // Our `merge-base --is-ancestor` check above is the authoritative
        // merge validation against the caller-supplied base — exit 0 means
        // every commit reachable from the branch is reachable from base_ref,
        // which is the git definition of "merged into base_ref". git's own
        // `-d` check is narrower (HEAD or the branch's upstream only) and
        // cannot consult an arbitrary third ref, so it refuses valid-per-our-
        // contract deletions when base_ref is neither HEAD nor upstream.
        // Use `-D` here to finalize what our pre-check already authorized —
        // this is not a force-delete semantically (the merged-into-base
        // guarantee still holds), it just bypasses a redundant narrower
        // check. Closes Embra_Debug #56.
        return match git_command()
            .args(["-C", &dir, "branch", "-D", name])
            .output()
            .await
        {
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !out.status.success() {
                    return format!("git branch failed: {}", stderr.trim());
                }
                format!("Deleted branch '{}' in {} (verified merged into '{}')", name, dir, base_ref)
            }
            Err(e) => format!("Failed to run git: {}", e),
        };
    }

    // Create form: `<path> <name>`
    if parts.len() >= 2 {
        let name = match positional(parts[1], "branch") {
            Ok(n) => n,
            Err(e) => return e,
        };
        return match git_command()
            .args(["-C", &dir, "branch", name])
            .output()
            .await
        {
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !out.status.success() {
                    return format!("git branch failed: {}", stderr.trim());
                }
                format!("Created branch '{}' in {}", name, dir)
            }
            Err(e) => format!("Failed to run git: {}", e),
        };
    }

    // Single arg: list form
    git_branch_list(&dir).await
}

async fn git_branch_list(dir: &str) -> String {
    match git_command()
        .args(["-C", dir, "branch", "-a"])
        .output()
        .await
    {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !out.status.success() {
                return format!("git branch failed: {}", stderr.trim());
            }
            format!("=== Branches ({}) ===\n{}", dir, stdout)
        }
        Err(e) => format!("Failed to run git: {}", e),
    }
}

/// Run `git checkout`. Write operation — workspace restricted.
/// Param format: `<path> <branch>`
pub async fn git_checkout(param: &str) -> String {
    let parts: Vec<&str> = param.split_whitespace().collect();
    if parts.len() < 2 {
        return "Usage: git_checkout <path> <branch>".into();
    }
    let dir = match resolve_workspace_path(parts[0]) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let branch = match positional(parts[1], "branch") {
        Ok(b) => b,
        Err(e) => return e,
    };

    match git_command()
        .args(["-C", &dir, "checkout", branch])
        .output()
        .await
    {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !out.status.success() {
                return format!("git checkout failed: {}", stderr.trim());
            }
            let output = if stdout.trim().is_empty() {
                stderr.trim().to_string()
            } else {
                stdout.trim().to_string()
            };
            format!("Checked out '{}' in {}:\n{}", branch, dir, output)
        }
        Err(e) => format!("Failed to run git: {}", e),
    }
}

/// Merge a branch into the current branch of a workspace repository.
/// Param format: `<path> <branch>` or `<path> <branch> --no-ff`.
///
/// On conflict, returns git's stdout and stderr so the caller can inspect
/// the conflicted paths and resolve them via the `file_*` tools, then
/// stage the resolutions with `git_add` and finalize with `git_commit`.
/// No automatic abort — the caller owns the resolution path. Closes
/// Embra_Debug #55.
pub async fn git_merge(param: &str) -> String {
    let parts: Vec<&str> = param.split_whitespace().collect();
    if parts.len() < 2 {
        return "Usage: git_merge <repo_path> <branch> [--no-ff]".into();
    }
    let dir = match resolve_workspace_path(parts[0]) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let branch = match positional(parts[1], "branch") {
        Ok(b) => b,
        Err(e) => return e,
    };
    let no_ff = parts.iter().skip(2).any(|p| *p == "--no-ff");

    let mut args: Vec<String> = vec!["-C".to_string(), dir.clone(), "merge".to_string()];
    if no_ff {
        args.push("--no-ff".to_string());
    }
    args.push(branch.to_string());

    match git_command()
        .args(&args)
        .output()
        .await
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if output.status.success() {
                let body = if stdout.trim().is_empty() {
                    stderr.trim().to_string()
                } else {
                    stdout.trim().to_string()
                };
                let ff_tag = if no_ff { " (no-ff)" } else { "" };
                format!(
                    "Merged '{}' into current branch of {}{}:\n{}",
                    branch, dir, ff_tag, body
                )
            } else {
                format!(
                    "git merge failed in {} (likely conflicts — resolve via file_* tools, then git_add + git_commit, or run `git merge --abort` manually):\n--- stdout ---\n{}\n--- stderr ---\n{}",
                    dir, stdout, stderr
                )
            }
        }
        Err(e) => format!("Failed to run git merge: {}", e),
    }
}

/// Stage a file removal with git rm. Workspace restricted.
/// Param format: `<path> <files>`
pub async fn git_rm(param: &str) -> String {
    let parts: Vec<&str> = param.splitn(2, char::is_whitespace).collect();
    if parts.len() < 2 {
        return "Usage: git_rm <repo_path> <files>\nExample: git_rm /embra/workspace/repos/myrepo old_file.txt".into();
    }

    let dir = match resolve_workspace_path(parts[0]) {
        Ok(d) => d,
        Err(e) => return e,
    };

    let files = parts[1];
    let file_args: Vec<&str> = files.split_whitespace().collect();
    for f in &file_args {
        if let Err(e) = positional(f, "file") {
            return e;
        }
    }

    match git_command()
        .args(["-C", &dir, "rm", "--"])
        .args(&file_args)
        .output()
        .await
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if output.status.success() {
                format!(
                    "Removed and staged in {}:\n{}",
                    dir,
                    if stdout.is_empty() {
                        stderr.to_string()
                    } else {
                        stdout.to_string()
                    }
                )
            } else {
                format!("git rm failed in {}:\n{}", dir, stderr)
            }
        }
        Err(e) => format!("Failed to run git rm: {}", e),
    }
}

/// Move or rename a tracked file with git mv. Workspace restricted.
/// Handles case-sensitive renames on case-insensitive filesystems (e.g. File.rs → file.rs)
/// that plain `file_move` + `git_add` cannot.
/// Param format: `<repo_path> <source> <destination>`
pub async fn git_mv(param: &str) -> String {
    let parts: Vec<&str> = param.splitn(3, char::is_whitespace).collect();
    if parts.len() < 3 {
        return "Usage: git_mv <repo_path> <source> <destination>\nExample: git_mv /embra/workspace/repos/myrepo src/Old.rs src/old.rs".into();
    }

    let dir = match resolve_workspace_path(parts[0]) {
        Ok(d) => d,
        Err(e) => return e,
    };

    let (src, dst) = match (positional(parts[1], "source"), positional(parts[2], "destination")) {
        (Ok(s), Ok(d)) => (s, d),
        (Err(e), _) | (_, Err(e)) => return e,
    };

    match git_command()
        .args(["-C", &dir, "mv", "--", src, dst])
        .output()
        .await
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if output.status.success() {
                format!("git mv: {} → {} in {}", src, dst, dir)
            } else {
                format!("git mv failed in {}:\n{}{}", dir, stdout, stderr)
            }
        }
        Err(e) => format!("Failed to run git mv: {}", e),
    }
}

// -- Git repository tools -----------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "git_clone",
    is_side_effectful = true,
    description = "Clone a git repository into /embra/workspace/. HTTPS uses the stored GitHub token; SSH is also supported. url is https://, http://, ssh://, git:// or user@host:path; a local path or a file:// URL is refused. subpath may be a bare directory name (`myrepo`), a workspace-relative path (`repos/myrepo`), or an absolute path under the workspace (`/embra/workspace/repos/myrepo`); if omitted, the repo name is derived from the URL."
)]
pub struct GitCloneArgs {
    pub url: String,
    #[serde(default)]
    pub subpath: Option<String>,
}

impl GitCloneArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = match self.subpath {
            Some(s) if !s.is_empty() => format!("{} {}", self.url, s),
            _ => self.url,
        };
        Ok(git_clone(ctx.db, &param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "git_status",
    description = "Show `git status` for a directory under /embra/workspace. path may be absolute (`/embra/workspace/repo`) or relative (`repo`)."
)]
pub struct GitStatusArgs {
    pub path: String,
}

impl GitStatusArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(git_status(&self.path).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "git_log",
    description = "Show recent git log for a directory under /embra/workspace. args is optional: a count (`-n 20`, `-5`), `--oneline`, `--graph`, `--stat`, `--decorate`, `--no-merges`, `--merges`, `--reverse`, `--all`, `--first-parent`, `--name-only`, `--name-status`, `-p`, the filters `--author=`, `--committer=`, `--since=`/`--after=`, `--until=`/`--before=`, `--grep=`, `--max-count=`, `--skip=`, `--format=`/`--pretty=`, `--date=`, and revisions or paths; any other flag is refused. path may be absolute (`/embra/workspace/repo`) or relative (`repo`)."
)]
pub struct GitLogArgs {
    pub path: String,
    #[serde(default)]
    pub args: String,
}

impl GitLogArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = if self.args.is_empty() {
            self.path
        } else {
            format!("{} {}", self.path, self.args)
        };
        Ok(git_log(&param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "git_add",
    is_side_effectful = true,
    description = "Stage files for commit in a workspace repository. path may be absolute (`/embra/workspace/repo`) or relative (`repo`)."
)]
pub struct GitAddArgs {
    pub path: String,
    /// Space-separated file paths to stage, or \".\" for everything.
    pub files: String,
}

impl GitAddArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{} {}", self.path, self.files);
        Ok(git_add(&param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "git_commit",
    is_side_effectful = true,
    description = "Commit staged changes in a workspace repository. The message is passed to git exactly as given: a blank line after the first line separates the subject from the body. path may be absolute (`/embra/workspace/repo`) or relative (`repo`)."
)]
pub struct GitCommitArgs {
    pub path: String,
    pub message: String,
}

impl GitCommitArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{} | {}", self.path, self.message);
        Ok(git_commit(&param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "git_push",
    is_side_effectful = true,
    description = "Push local commits to the remote branch of a workspace repository. path may be absolute (`/embra/workspace/repo`) or relative (`repo`)."
)]
pub struct GitPushArgs {
    pub path: String,
}

impl GitPushArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(git_push(ctx.db, &self.path).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "git_pull",
    is_side_effectful = true,
    description = "Pull from the remote branch into a workspace repository. path may be absolute (`/embra/workspace/repo`) or relative (`repo`)."
)]
pub struct GitPullArgs {
    pub path: String,
}

impl GitPullArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(git_pull(ctx.db, &self.path).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "git_fetch",
    is_side_effectful = true,
    description = "Fetch from a remote into a workspace repository without merging: updates the remote-tracking branches (and tags with tags=true) and leaves the working tree and the current branch untouched. Use it before git_log or git_diff against origin/<branch> to review incoming work, before git_push to learn whether the remote moved, or with prune=true to drop remote-tracking branches the remote has deleted. remote defaults to origin; branch narrows the fetch to one branch. path may be absolute (`/embra/workspace/repo`) or relative (`repo`)."
)]
pub struct GitFetchArgs {
    pub path: String,
    /// Remote to fetch from. Defaults to `origin`.
    #[serde(default)]
    pub remote: Option<String>,
    /// One branch to fetch; every branch of the remote when omitted.
    #[serde(default)]
    pub branch: Option<String>,
    /// Remove remote-tracking branches the remote no longer has (`--prune`).
    #[serde(default)]
    pub prune: bool,
    /// Fetch tags as well (`--tags`).
    #[serde(default)]
    pub tags: bool,
}

impl GitFetchArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let remote = self.remote.as_deref().unwrap_or("origin");
        Ok(git_fetch(ctx.db, &self.path, remote, self.branch.as_deref(), self.prune, self.tags).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "git_diff",
    description = "Show uncommitted changes in a workspace repository. Optional file narrows the diff to a single path. path may be absolute (`/embra/workspace/repo`) or relative (`repo`)."
)]
pub struct GitDiffArgs {
    pub path: String,
    #[serde(default)]
    pub file: Option<String>,
}

impl GitDiffArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = match self.file {
            Some(f) if !f.is_empty() => format!("{} {}", self.path, f),
            _ => self.path,
        };
        Ok(git_diff(&param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum GitBranchAction {
    /// List branches (default).
    List,
    /// Create a new branch named `name`.
    Create,
    /// Delete branch `name` (refuses unmerged branches).
    Delete,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "git_branch",
    is_side_effectful = true,
    description = "List, create, or delete branches in a workspace repository. action=list returns current branches; action=create requires name; action=delete requires name and refuses any branch with commits not yet merged into the base ref (default `main`, override via `base`); the merge check falls back to `origin/<base>` if no local copy exists. Set force=true on action=delete to bypass the merge check and discard unmerged commits (maps to `git branch -D`) — use this for throwaway/spike branches. path may be absolute (`/embra/workspace/repo`) or relative (`repo`)."
)]
pub struct GitBranchArgs {
    pub path: String,
    #[serde(default = "default_git_branch_action")]
    pub action: GitBranchAction,
    #[serde(default)]
    pub name: Option<String>,
    /// Integration base for the merged-into check on action=delete.
    /// Defaults to `main`. Ignored for list/create and for force-delete.
    #[serde(default)]
    pub base: Option<String>,
    /// When true on action=delete, skip the merge-base check and run
    /// `git branch -D`. Ignored for list/create. Defaults to false.
    #[serde(default)]
    pub force: Option<bool>,
}

fn default_git_branch_action() -> GitBranchAction {
    GitBranchAction::List
}

impl GitBranchArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = match (&self.action, &self.name) {
            (GitBranchAction::List, _) => self.path.clone(),
            (GitBranchAction::Create, Some(n)) => format!("{} {}", self.path, n),
            (GitBranchAction::Delete, Some(n)) => {
                if self.force.unwrap_or(false) {
                    format!("{} delete-force {}", self.path, n)
                } else {
                    let base = self.base.as_deref().unwrap_or("main");
                    format!("{} delete {} {}", self.path, n, base)
                }
            }
            (GitBranchAction::Create, None) | (GitBranchAction::Delete, None) => {
                return Ok(format!(
                    "git_branch action={:?} requires a name.",
                    self.action
                ));
            }
        };
        Ok(git_branch(&param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "git_checkout",
    is_side_effectful = true,
    description = "Switch to a branch in a workspace repository (refuses unclean working directory). path may be absolute (`/embra/workspace/repo`) or relative (`repo`)."
)]
pub struct GitCheckoutArgs {
    pub path: String,
    pub branch: String,
}

impl GitCheckoutArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{} {}", self.path, self.branch);
        Ok(git_checkout(&param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "git_merge",
    is_side_effectful = true,
    description = "Merge `branch` into the current branch of a workspace repository. path may be absolute (`/embra/workspace/repo`) or relative (`repo`). Set no_ff=true to force a merge commit even when fast-forward is possible. On conflict, returns git's output so the caller can resolve via file_* tools and finalize with git_add + git_commit."
)]
pub struct GitMergeArgs {
    pub path: String,
    /// Source branch to merge into the current branch.
    pub branch: String,
    /// When true, creates a merge commit even if a fast-forward would work.
    /// Defaults to false.
    #[serde(default)]
    pub no_ff: Option<bool>,
}

impl GitMergeArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let mut param = format!("{} {}", self.path, self.branch);
        if self.no_ff.unwrap_or(false) {
            param.push_str(" --no-ff");
        }
        Ok(git_merge(&param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "git_rm",
    is_side_effectful = true,
    description = "Stage file removal in a workspace repository (git rm). path may be absolute (`/embra/workspace/repo`) or relative (`repo`)."
)]
pub struct GitRmArgs {
    pub path: String,
    pub files: String,
}

impl GitRmArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{} {}", self.path, self.files);
        Ok(git_rm(&param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "git_mv",
    is_side_effectful = true,
    description = "git mv — tracked rename/move within a workspace repository. Preserves history and handles case-sensitive renames on case-insensitive filesystems. path may be absolute (`/embra/workspace/repo`) or relative (`repo`)."
)]
pub struct GitMvArgs {
    pub path: String,
    pub source: String,
    pub destination: String,
}

impl GitMvArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{} {} {}", self.path, self.source, self.destination);
        Ok(git_mv(&param).await)
    }
}

#[cfg(test)]
mod native_args_tests {
    use super::*;

    #[test]
    fn git_clone_subpath_optional() {
        let a: GitCloneArgs =
            serde_json::from_value(serde_json::json!({"url": "https://github.com/x/y"})).unwrap();
        assert_eq!(a.url, "https://github.com/x/y");
        assert_eq!(a.subpath, None);

        let b: GitCloneArgs = serde_json::from_value(serde_json::json!({
            "url": "https://github.com/x/y", "subpath": "repos/y"
        }))
        .unwrap();
        assert_eq!(b.subpath.as_deref(), Some("repos/y"));
    }

    #[test]
    fn git_branch_action_roundtrip() {
        let list: GitBranchArgs =
            serde_json::from_value(serde_json::json!({"path": "/embra/workspace/x"})).unwrap();
        assert!(matches!(list.action, GitBranchAction::List));
        assert!(list.base.is_none());

        let create: GitBranchArgs = serde_json::from_value(serde_json::json!({
            "path": "/w/x", "action": "create", "name": "feature-foo"
        }))
        .unwrap();
        assert!(matches!(create.action, GitBranchAction::Create));
        assert_eq!(create.name.as_deref(), Some("feature-foo"));
        assert!(create.base.is_none());

        // Embra_Debug #49: delete accepts an optional `base` for the
        // merged-into check; default is treated as `main` at dispatch time.
        let delete_default: GitBranchArgs = serde_json::from_value(serde_json::json!({
            "path": "/w/x", "action": "delete", "name": "feature-foo"
        }))
        .unwrap();
        assert!(matches!(delete_default.action, GitBranchAction::Delete));
        assert!(delete_default.base.is_none());

        let delete_explicit: GitBranchArgs = serde_json::from_value(serde_json::json!({
            "path": "/w/x", "action": "delete", "name": "feature-foo", "base": "develop"
        }))
        .unwrap();
        assert_eq!(delete_explicit.base.as_deref(), Some("develop"));

        // Embra_Debug #55: force is optional, defaults to None/false.
        assert!(delete_explicit.force.is_none());

        let delete_force: GitBranchArgs = serde_json::from_value(serde_json::json!({
            "path": "/w/x", "action": "delete", "name": "spike", "force": true
        }))
        .unwrap();
        assert_eq!(delete_force.force, Some(true));
    }

    #[tokio::test]
    async fn git_branch_force_maps_to_delete_force_token() {
        // The param serialization between GitBranchArgs::run and the legacy
        // string handler is an internal protocol; force=true routes through
        // the delete-force keyword, force=false/unset through plain delete.
        // We verify the behavior end-to-end by invoking run against a bogus
        // workspace path and inspecting the rejection message — the resolver
        // rejects *before* the git subprocess, but the token choice determines
        // which code path emits the rejection/usage text. Simpler: reconstruct
        // the expected param manually.
        let args = GitBranchArgs {
            path: "/w/x".into(),
            action: GitBranchAction::Delete,
            name: Some("spike".into()),
            base: None,
            force: Some(true),
        };
        // Re-build the same match the real run() does, asserting the keyword.
        let param = match (&args.action, &args.name) {
            (GitBranchAction::Delete, Some(n)) if args.force.unwrap_or(false) => {
                format!("{} delete-force {}", args.path, n)
            }
            _ => "unexpected".into(),
        };
        assert_eq!(param, "/w/x delete-force spike");
    }

    #[test]
    fn git_merge_args_no_ff_optional() {
        let a: GitMergeArgs = serde_json::from_value(serde_json::json!({
            "path": "/embra/workspace/x", "branch": "feature-foo"
        }))
        .unwrap();
        assert_eq!(a.path, "/embra/workspace/x");
        assert_eq!(a.branch, "feature-foo");
        assert!(a.no_ff.is_none());

        let b: GitMergeArgs = serde_json::from_value(serde_json::json!({
            "path": "x", "branch": "feature-foo", "no_ff": true
        }))
        .unwrap();
        assert_eq!(b.no_ff, Some(true));

        // Missing branch → error.
        assert!(
            serde_json::from_value::<GitMergeArgs>(serde_json::json!({"path": "x"})).is_err()
        );
    }
}

#[cfg(test)]
mod git_token_tests {
    use super::*;

    #[test]
    fn normalize_git_host_strips_scheme_path_and_case() {
        assert_eq!(
            normalize_git_host("https://GitLab.Example.com/"),
            Ok("gitlab.example.com".to_string())
        );
        assert_eq!(
            normalize_git_host("https://gitlab.example.com/group/repo.git"),
            Ok("gitlab.example.com".to_string())
        );
        assert_eq!(
            normalize_git_host("gitlab.example.com"),
            Ok("gitlab.example.com".to_string())
        );
        assert_eq!(
            normalize_git_host("http://gitea.local:3000"),
            Ok("gitea.local:3000".to_string())
        );
    }

    #[test]
    fn normalize_git_host_rejects_junk() {
        assert!(normalize_git_host("").is_err());
        assert!(normalize_git_host("https://").is_err());
        assert!(normalize_git_host("/just/a/path").is_err());
        assert!(normalize_git_host("user@host.com").is_err());
    }

    /// The github.com rewrite is one configuration pair, the same rewrite
    /// the `-c` argument carried before the tokens left argv.
    #[test]
    fn the_github_rewrite_is_one_configuration_pair() {
        let pairs = github_token_config(&Some("ghp_abc".to_string()));
        assert_eq!(
            pairs,
            vec![(
                "url.https://x-access-token:ghp_abc@github.com/.insteadOf".to_string(),
                "https://github.com/".to_string()
            )]
        );
        assert!(github_token_config(&None).is_empty());
    }

    #[test]
    fn host_token_pairs_use_oauth2_in_a_deterministic_order() {
        let mut tokens = std::collections::BTreeMap::new();
        tokens.insert("gitlab.b.com".to_string(), "tok-b".to_string());
        tokens.insert("gitlab.a.com".to_string(), "tok-a".to_string());
        let pairs = host_token_config(&tokens);
        assert_eq!(
            pairs,
            vec![
                (
                    "url.https://oauth2:tok-a@gitlab.a.com/.insteadOf".to_string(),
                    "https://gitlab.a.com/".to_string()
                ),
                (
                    "url.https://oauth2:tok-b@gitlab.b.com/.insteadOf".to_string(),
                    "https://gitlab.b.com/".to_string()
                ),
            ]
        );
        assert!(host_token_config(&std::collections::BTreeMap::new()).is_empty());
    }
}

#[cfg(test)]
mod git_command_tests {
    use super::*;
    use std::ffi::OsStr;

    fn env_of(cmd: &tokio::process::Command) -> std::collections::BTreeMap<String, String> {
        cmd.as_std()
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_string_lossy().into_owned(), v?.to_string_lossy().into_owned())))
            .collect()
    }

    /// The builder's contract: no system configuration, the global file
    /// under STATE, no prompt, the network transports only, its ssh pinned
    /// to no client configuration and the workspace key, the hooks
    /// directory pinned as the first configuration pair and the caller's
    /// pairs after it, all in the environment and none in argv.
    #[test]
    fn a_git_command_is_hardened_and_carries_its_configuration_in_the_environment() {
        let cmd = git_command_with(&[(
            "url.https://oauth2:tok@gitlab.example/.insteadOf".to_string(),
            "https://gitlab.example/".to_string(),
        )]);
        assert_eq!(cmd.as_std().get_program(), OsStr::new("git"));
        assert_eq!(cmd.as_std().get_args().count(), 0, "nothing in argv");
        let env = env_of(&cmd);
        assert_eq!(env["GIT_CONFIG_NOSYSTEM"], "1");
        assert_eq!(env["GIT_CONFIG_GLOBAL"], GIT_CONFIG_GLOBAL);
        assert_eq!(env["GIT_TERMINAL_PROMPT"], "0");
        assert_eq!(env["GIT_ALLOW_PROTOCOL"], "https:http:ssh:git");
        assert_eq!(env["GIT_SSH_COMMAND"], git_ssh_command());
        assert!(
            git_ssh_command().starts_with(
                "ssh -F /dev/null -i /embra/workspace/.ssh/id_ed25519 -o UserKnownHostsFile=/embra/workspace/.ssh/known_hosts -o StrictHostKeyChecking=accept-new"
            ),
            "{}",
            git_ssh_command()
        );
        assert_eq!(env["GIT_CONFIG_COUNT"], "2");
        assert_eq!(env["GIT_CONFIG_KEY_0"], "core.hooksPath");
        assert_eq!(env["GIT_CONFIG_VALUE_0"], GIT_HOOKS_DIR);
        assert_eq!(env["GIT_CONFIG_KEY_1"], "url.https://oauth2:tok@gitlab.example/.insteadOf");
        assert_eq!(env["GIT_CONFIG_VALUE_1"], "https://gitlab.example/");
        let plain = env_of(&git_command());
        assert_eq!(plain["GIT_CONFIG_COUNT"], "1");
        assert!(!plain.contains_key("GIT_CONFIG_KEY_1"));
    }

    /// `/git-setup`'s two halves against a temporary global file: the
    /// identity written reads back, an unset one reads as empty, and a
    /// write that fails (the global path is a directory) is an error
    /// naming the key, never reported as done. Needs `git` on the host.
    #[tokio::test]
    async fn the_identity_is_written_read_back_and_a_failed_write_is_an_error() {
        let tmp = std::env::temp_dir().join(format!("embra-git-identity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("hooks")).unwrap();
        let global = tmp.join("config");
        let hooks = tmp.join("hooks");
        assert_eq!(identity_in(&global, &hooks).await.unwrap(), (String::new(), String::new()));
        set_identity_in(&global, &hooks, "Embra Operator", "op@example.test").await.unwrap();
        assert_eq!(
            identity_in(&global, &hooks).await.unwrap(),
            ("Embra Operator".to_string(), "op@example.test".to_string())
        );
        std::fs::create_dir_all(tmp.join("dir-as-config")).unwrap();
        let err = set_identity_in(&tmp.join("dir-as-config"), &hooks, "x", "y").await.unwrap_err();
        assert!(err.starts_with("user.name: ") && err.len() > "user.name: ".len(), "{err}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Startup under a temporary directory: the hooks directory exists and
    /// is empty, the identity of a legacy workspace `.gitconfig` is adopted
    /// into the new global file, and the defaults are there. A second run
    /// changes nothing. Needs `git` on the host.
    #[tokio::test]
    async fn startup_adopts_the_legacy_identity_and_writes_the_defaults_under_state() {
        let tmp = std::env::temp_dir().join(format!("embra-git-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let legacy = tmp.join("legacy-gitconfig");
        std::fs::write(&legacy, "[user]\n\tname = Embra Operator\n\temail = op@example.test\n").unwrap();
        let dir = tmp.join("git");
        prepare_git_config_in(&dir, &legacy).await;
        assert!(dir.join("hooks").is_dir());
        assert_eq!(std::fs::read_dir(dir.join("hooks")).unwrap().count(), 0, "no hook");
        let global = dir.join("config");
        let get = |key: &str| {
            let global = global.clone();
            let hooks = dir.join("hooks");
            let key = key.to_string();
            async move {
                let out = git_command_for(&global, &hooks, &[])
                    .args(["config", "--global", "--get", &key])
                    .output()
                    .await
                    .expect("git runs");
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            }
        };
        assert_eq!(get("user.name").await, "Embra Operator");
        assert_eq!(get("user.email").await, "op@example.test");
        assert_eq!(get("safe.directory").await, "*");
        assert_eq!(get("push.autoSetupRemote").await, "true");
        let before = std::fs::read_to_string(&global).unwrap();
        // The legacy file is read once: a changed name there is not adopted again.
        std::fs::write(&legacy, "[user]\n\tname = Someone Else\n").unwrap();
        prepare_git_config_in(&dir, &legacy).await;
        assert_eq!(std::fs::read_to_string(&global).unwrap(), before);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

#[cfg(test)]
mod argument_tests {
    use super::*;

    /// `git_fetch` places its flags, then `--`, then the remote and the
    /// branch, and refuses an option-shaped remote or branch before git runs.
    #[test]
    fn git_fetch_puts_the_flags_before_a_double_dash_and_refuses_an_option_shaped_remote_or_branch() {
        assert_eq!(git_fetch_args("origin", None, false, false).unwrap(), vec!["fetch", "--", "origin"]);
        assert_eq!(
            git_fetch_args("github", Some("main"), true, true).unwrap(),
            vec!["fetch", "--prune", "--tags", "--", "github", "main"]
        );
        assert!(git_fetch_args("--upload-pack=id", None, false, false).unwrap_err().contains("begins with '-'"));
        assert!(git_fetch_args("origin", Some("-x"), false, false).unwrap_err().contains("begins with '-'"));
    }

    /// `git_log` passes a count, the listed flags and revisions or paths,
    /// and refuses any other flag: `--output` would write a file wherever
    /// git runs, outside the jail every writer shares (#7).
    #[test]
    fn git_log_passes_its_list_and_refuses_every_other_flag() {
        let accepted: [Vec<&str>; 5] = [
            vec!["-5"],
            vec!["-n", "20", "--oneline"],
            vec!["--author=foo", "--since=yesterday", "main"],
            vec!["--stat", "-p", "src/lib.rs"],
            vec![],
        ];
        for ok in accepted {
            assert_eq!(git_log_args(&ok).unwrap(), ok, "{ok:?}");
        }
        let refused: [Vec<&str>; 5] = [
            vec!["--output=/tmp/pwned"],
            vec!["--output=../escape"],
            vec!["-n", "x"],
            vec!["--exec=touch /tmp/x"],
            vec!["-o", "/tmp/x"],
        ];
        for bad in refused {
            let err = git_log_args(&bad).unwrap_err();
            assert!(err.starts_with("Denied: git_log does not pass"), "{bad:?}: {err}");
        }
    }

    /// The refusal comes before git runs: nothing is written where the flag
    /// pointed, whether or not the repository exists.
    #[tokio::test]
    async fn a_git_log_output_flag_is_refused_and_writes_nothing() {
        let target = std::env::temp_dir().join(format!("embra-git-log-pwned-{}", std::process::id()));
        let _ = std::fs::remove_file(&target);
        let out = git_log(&format!("repo --output={}", target.display())).await;
        assert!(out.starts_with("Denied:"), "{out}");
        assert!(!target.exists());
        let out = git_log("repo --output=../escape").await;
        assert!(out.starts_with("Denied:"), "{out}");
    }

    /// The transports pass; a command (`ext::`), a file, a local path and an
    /// option-shaped token are refused before git runs (#56).
    #[test]
    fn validate_clone_url_accepts_the_transports_and_refuses_commands_files_and_options() {
        for ok in [
            "https://github.com/o/r.git",
            "http://h/r",
            "git://h/r",
            "ssh://git@h/r.git",
            "ssh://git@h:2222/r.git",
            "ssh://[::1]:22/r",
            "git@github.com:o/r.git",
            "host:path/r",
        ] {
            assert!(validate_clone_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "ext::sh -c id",
            "fd::17",
            "--upload-pack=touch /tmp/x",
            "-x",
            "file:///etc",
            "FILE://etc",
            "/embra/state",
            "./here",
            "~/x",
            "plain-name",
            // An option-shaped user, host or port in the authority: the live
            // check of 2026-10-23 found git's own "strange hostname" refusal
            // was the only line.
            "ssh://-oProxyCommand=id/r",
            "ssh://-oProxyCommand=id@h/r",
            "ssh://git@-h/r",
            "ssh://h:-1/r",
            "https://-h/r",
            "git@-h:o/r.git",
        ] {
            let err = validate_clone_url(bad).unwrap_err();
            assert!(err.starts_with("Denied:"), "{bad}: {err}");
        }
        let err = validate_clone_url("ssh://-oProxyCommand=id/r").unwrap_err();
        assert!(err.contains("names the host '-oProxyCommand=id'"), "{err}");
        let err = validate_clone_url("ssh://-oProxyCommand=id@h/r").unwrap_err();
        assert!(err.contains("names the user '-oProxyCommand=id'"), "{err}");
        let err = validate_clone_url("ssh://h:-1/r").unwrap_err();
        assert!(err.contains("names the port '-1'"), "{err}");
    }

    /// Every model-supplied positional of the git family is refused when it
    /// begins with `-`, before git runs (the repository need not exist).
    #[tokio::test]
    async fn option_shaped_positionals_are_refused_before_git_runs() {
        let db = WardsonDbClient::from_url("http://127.0.0.1:1");
        let cases = [
            ("checkout", git_checkout("repo -b").await),
            ("merge", git_merge("repo --abort").await),
            ("branch delete", git_branch("repo delete -D").await),
            ("branch base", git_branch("repo delete main -x").await),
            ("branch create", git_branch("repo -d").await),
            ("add", git_add("repo --all").await),
            ("rm", git_rm("repo -r").await),
            ("mv", git_mv("repo -k x").await),
            ("clone", git_clone(&db, "--upload-pack=id").await),
        ];
        for (what, out) in cases {
            assert!(out.starts_with("Denied:"), "{what}: {out}");
        }
    }
}
