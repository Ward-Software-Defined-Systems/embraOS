use chrono::Utc;

use crate::db::WardsonDbClient;

const WORKSPACE_ROOT: &str = "/embra/workspace";

/// Resolve a workspace-scoped tool path. Accepts either an absolute path
/// under `/embra/workspace/` or a path relative to `/embra/workspace/`
/// (so `repo`, `./repo`, and `/embra/workspace/repo` all resolve to the
/// same canonical form). Returns the canonical absolute path or a uniform
/// `Denied:` rejection message.
///
/// Empty input resolves to the workspace root. Leading `./` is stripped
/// before joining so `./repo` and `repo` are equivalent. `..` segments
/// in either form are rejected outright as a defense against traversal.
///
/// Every git_*, file_*, and dir_* tool routes through this resolver so the
/// accepted path shapes are uniform across the tool surface. Closes
/// Embra_Debug #45 (git_* unification) and #52/#53 (file_* / dir_*
/// unification) — prior to these fixes, git_clone took relative subpaths,
/// the rest of the git family required absolute, and the file/dir family
/// required absolute with a looser helper that skipped the `..` check.
fn resolve_workspace_path(path: &str) -> Result<String, String> {
    let trimmed = path.trim();
    let resolved = if trimmed.is_empty() {
        WORKSPACE_ROOT.to_string()
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        let rel = trimmed.trim_start_matches("./");
        format!("{}/{}", WORKSPACE_ROOT, rel)
    };
    // Defeat path traversal in either form: `../etc/passwd` (relative) and
    // `/embra/workspace/../etc/passwd` (absolute) both contain a `..` segment
    // that would let the resolved path escape WORKSPACE_ROOT after kernel
    // canonicalization. The `starts_with(WORKSPACE_ROOT)` check below is a
    // string-prefix check, not a filesystem-canonical one — so we reject `..`
    // outright to keep the contract honest.
    if resolved.split('/').any(|seg| seg == "..") {
        return Err(format!(
            "Denied: path '{}' contains a '..' component",
            resolved
        ));
    }
    if !resolved.starts_with(WORKSPACE_ROOT) {
        return Err(format!(
            "Denied: path '{}' resolves outside {}",
            resolved, WORKSPACE_ROOT
        ));
    }
    Ok(resolved)
}

/// Resolve `base` to a ref usable by `merge-base --is-ancestor`. Prefers the
/// local branch (`<base>`); falls back to `origin/<base>` if the local copy
/// is missing. Returns Err with a clear message if neither exists. Used by
/// `git_branch delete` to enforce the documented merged-into-base contract.
async fn resolve_base_ref(dir: &str, base: &str) -> Result<String, String> {
    let local_check = tokio::process::Command::new("git")
        .args(["-C", dir, "rev-parse", "--verify", "--quiet", base])
        .output()
        .await;
    if let Ok(out) = &local_check {
        if out.status.success() {
            return Ok(base.to_string());
        }
    }
    let remote = format!("origin/{}", base);
    let remote_check = tokio::process::Command::new("git")
        .args(["-C", dir, "rev-parse", "--verify", "--quiet", &remote])
        .output()
        .await;
    if let Ok(out) = &remote_check {
        if out.status.success() {
            return Ok(remote);
        }
    }
    Err(format!(
        "Cannot verify merge status: neither '{}' nor 'origin/{}' exists locally. Fetch first.",
        base, base
    ))
}

/// Resolve GITHUB_TOKEN: env var first, then WardSONDB config.system.github_token.
pub async fn resolve_github_token(db: &WardsonDbClient) -> Option<String> {
    if let Ok(t) = std::env::var("GITHUB_TOKEN") {
        if !t.is_empty() {
            return Some(t);
        }
    }
    if let Ok(doc) = db.read("config.system", "config").await {
        if let Some(t) = doc.get("github_token").and_then(|v| v.as_str()) {
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// Build git `-c` args to inject GITHUB_TOKEN for HTTPS GitHub remotes.
/// Returns args like `["-c", "url.https://x-access-token:TOKEN@github.com/.insteadOf=https://github.com/"]`
/// or an empty vec if no token is available.
fn github_token_git_args(token: &Option<String>) -> Vec<String> {
    match token {
        Some(t) => vec![
            "-c".to_string(),
            format!("url.https://x-access-token:{}@github.com/.insteadOf=https://github.com/", t),
        ],
        None => vec![],
    }
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

    let dest = match resolve_workspace_path(&subpath_input) {
        Ok(p) => p,
        Err(e) => return e,
    };

    if std::path::Path::new(&dest).exists() {
        return format!("Directory already exists: {}", dest);
    }

    // Ensure the destination's parent directory exists so nested subpaths
    // like `repos/foo/bar` work on first use.
    if let Some(parent) = std::path::Path::new(&dest).parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Inject GITHUB_TOKEN via git -c url rewriting (no token in URLs or config files)
    let token = resolve_github_token(db).await;
    let token_args = github_token_git_args(&token);

    let mut args = token_args;
    args.extend(["clone".to_string(), url.to_string(), dest.clone()]);

    match tokio::time::timeout(
        std::time::Duration::from_secs(120),
        tokio::process::Command::new("git")
            .args(&args)
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
    match tokio::process::Command::new("git")
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
    let dir = match resolve_workspace_path(path_token) {
        Ok(d) => d,
        Err(e) => return e,
    };

    let mut args = vec!["-C", &dir, "log", "--oneline", "-20"];
    for a in &extra_args {
        args.push(a);
    }

    match tokio::process::Command::new("git")
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

/// Create or update a plan in WardSONDB.
pub async fn plan(db: &WardsonDbClient, param: &str) -> String {
    if param.is_empty() {
        // List plans
        ensure_collection(db, "plans").await;
        let plans = db
            .query("plans", &serde_json::json!({}))
            .await
            .unwrap_or_default();

        if plans.is_empty() {
            return "No plans found. Create one with: plan <title> | <description>".into();
        }

        let mut output = format!("=== Plans ({}) ===\n", plans.len());
        for doc in &plans {
            let id = doc.get("_id").or(doc.get("id")).and_then(|v| v.as_str()).unwrap_or("?");
            let title = doc.get("title").and_then(|v| v.as_str()).unwrap_or("Untitled");
            let status = doc.get("status").and_then(|v| v.as_str()).unwrap_or("draft");
            output.push_str(&format!("  [{}] {} ({})\n", id, title, status));
        }
        return output;
    }

    ensure_collection(db, "plans").await;

    // Create/update: "title | description"
    let (title, description) = if let Some(pos) = param.find(" | ") {
        (&param[..pos], &param[pos + 3..])
    } else {
        (param, "")
    };

    let doc = serde_json::json!({
        "title": title,
        "description": description,
        "status": "draft",
        "created_at": Utc::now().to_rfc3339(),
    });

    match db.write("plans", &doc).await {
        Ok(id) => format!("Plan created: '{}' (ID: {})", title, id),
        Err(e) => format!("Failed to create plan: {}", e),
    }
}

/// List or manage tasks.
pub async fn tasks(db: &WardsonDbClient, param: &str) -> String {
    ensure_collection(db, "tasks").await;

    if param.is_empty() {
        let all_tasks = db
            .query("tasks", &serde_json::json!({}))
            .await
            .unwrap_or_default();

        if all_tasks.is_empty() {
            return "No tasks found. Add one with: task_add <title>".into();
        }

        let mut output = format!("=== Tasks ({}) ===\n", all_tasks.len());
        for doc in &all_tasks {
            let id = doc.get("_id").or(doc.get("id")).and_then(|v| v.as_str()).unwrap_or("?");
            let title = doc.get("title").and_then(|v| v.as_str()).unwrap_or("Untitled");
            let done = doc.get("done").and_then(|v| v.as_bool()).unwrap_or(false);
            let marker = if done { "x" } else { " " };
            output.push_str(&format!("  [{}] {} (ID: {})\n", marker, title, id));
        }
        return output;
    }

    // Filter by plan_id if param looks like an ID
    let filtered = db
        .query("tasks", &serde_json::json!({}))
        .await
        .unwrap_or_default();

    let matching: Vec<_> = filtered
        .iter()
        .filter(|doc| {
            let plan = doc.get("plan_id").and_then(|v| v.as_str()).unwrap_or("");
            let title = doc.get("title").and_then(|v| v.as_str()).unwrap_or("");
            plan.contains(param) || title.to_lowercase().contains(&param.to_lowercase())
        })
        .collect();

    if matching.is_empty() {
        return format!("No tasks matching '{}'.", param);
    }

    let mut output = format!("=== Tasks matching '{}' ({}) ===\n", param, matching.len());
    for doc in &matching {
        let id = doc.get("_id").or(doc.get("id")).and_then(|v| v.as_str()).unwrap_or("?");
        let title = doc.get("title").and_then(|v| v.as_str()).unwrap_or("Untitled");
        let done = doc.get("done").and_then(|v| v.as_bool()).unwrap_or(false);
        let marker = if done { "x" } else { " " };
        output.push_str(&format!("  [{}] {} (ID: {})\n", marker, title, id));
    }
    output
}

/// Add a task.
pub async fn task_add(db: &WardsonDbClient, param: &str) -> String {
    if param.is_empty() {
        return "Usage: task_add <title> or task_add <title> | <plan_id>".into();
    }

    ensure_collection(db, "tasks").await;

    let (title, plan_id) = if let Some(pos) = param.find(" | ") {
        (&param[..pos], Some(&param[pos + 3..]))
    } else {
        (param, None)
    };

    let mut doc = serde_json::json!({
        "title": title,
        "done": false,
        "created_at": Utc::now().to_rfc3339(),
    });

    if let Some(plan) = plan_id {
        doc["plan_id"] = serde_json::json!(plan.trim());
    }

    match db.write("tasks", &doc).await {
        Ok(id) => format!("Task added: '{}' (ID: {})", title, id),
        Err(e) => format!("Failed to add task: {}", e),
    }
}

/// Mark a task as done.
pub async fn task_done(db: &WardsonDbClient, task_id: &str) -> String {
    if task_id.is_empty() {
        return "Usage: task_done <task_id>".into();
    }

    match db.read("tasks", task_id.trim()).await {
        Ok(mut doc) => {
            doc["done"] = serde_json::json!(true);
            doc["completed_at"] = serde_json::json!(Utc::now().to_rfc3339());
            match db.update("tasks", task_id.trim(), &doc).await {
                Ok(()) => {
                    let title = doc.get("title").and_then(|v| v.as_str()).unwrap_or("?");
                    format!("Task completed: '{}'", title)
                }
                Err(e) => format!("Failed to update task: {}", e),
            }
        }
        Err(e) => format!("Task not found: {}", e),
    }
}

/// Delete a task by id (irreversible). Read first so we can echo the title
/// in the success message — gives the operator confirmation that the right
/// row went away.
pub async fn task_delete(db: &WardsonDbClient, task_id: &str) -> String {
    let id = task_id.trim();
    if id.is_empty() {
        return "Usage: task_delete <task_id>".into();
    }
    let title = match db.read("tasks", id).await {
        Ok(doc) => doc
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("(no title)")
            .to_string(),
        Err(e) => return format!("Task not found: {}", e),
    };
    match db.delete("tasks", id).await {
        Ok(()) => format!("Task deleted: '{}' (ID: {})", title, id),
        Err(e) => format!("Failed to delete task: {}", e),
    }
}

/// Delete a plan by id. When `cascade_tasks` is true, also remove every task
/// whose `plan_id` matches — useful for clearing a throwaway diagnostic plan
/// in one call. Default behavior leaves child tasks orphaned (their `plan_id`
/// will dangle but they remain queryable).
pub async fn plan_delete(db: &WardsonDbClient, plan_id: &str, cascade_tasks: bool) -> String {
    let id = plan_id.trim();
    if id.is_empty() {
        return "Usage: plan_delete <plan_id> [cascade_tasks=true]".into();
    }
    let title = match db.read("plans", id).await {
        Ok(doc) => doc
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("(no title)")
            .to_string(),
        Err(e) => return format!("Plan not found: {}", e),
    };

    let cascade_msg = if cascade_tasks {
        match db
            .delete_by_query("tasks", &serde_json::json!({"plan_id": id}))
            .await
        {
            Ok(n) => format!(" (cascade removed {} task(s))", n),
            Err(e) => return format!("Failed to cascade-delete tasks: {}", e),
        }
    } else {
        String::new()
    };

    match db.delete("plans", id).await {
        Ok(()) => format!("Plan deleted: '{}' (ID: {}){}", title, id, cascade_msg),
        Err(e) => format!("Failed to delete plan: {}", e),
    }
}

/// Fetch GitHub issues via API.
pub async fn gh_issues(db: &WardsonDbClient, param: &str) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };

    if param.is_empty() {
        return "Usage: gh_issues <owner/repo>\nExample: gh_issues ward-software-defined-systems/wardsondb".into();
    }

    let url = format!("https://api.github.com/repos/{}/issues?state=open&per_page=10", param);

    let client = reqwest::Client::new();
    match client
        .get(&url)
        .header("User-Agent", "embraOS/0.1.0")
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
    {
        Ok(resp) => {
            if !resp.status().is_success() {
                return format!("GitHub API error: {}", resp.status());
            }
            let issues: Vec<serde_json::Value> = resp.json().await.unwrap_or_default();
            if issues.is_empty() {
                return format!("No open issues for {}.", param);
            }
            let mut output = format!("=== Open Issues: {} ({}) ===\n", param, issues.len());
            for issue in &issues {
                let number = issue.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
                let title = issue.get("title").and_then(|v| v.as_str()).unwrap_or("?");
                let labels: Vec<&str> = issue
                    .get("labels")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|l| l.get("name").and_then(|n| n.as_str()))
                            .collect()
                    })
                    .unwrap_or_default();
                let label_str = if labels.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", labels.join(", "))
                };
                output.push_str(&format!("  #{} {}{}\n", number, title, label_str));
            }
            output
        }
        Err(e) => format!("Failed to fetch issues: {}", e),
    }
}

/// Fetch GitHub PRs via API.
pub async fn gh_prs(db: &WardsonDbClient, param: &str) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };

    if param.is_empty() {
        return "Usage: gh_prs <owner/repo>\nExample: gh_prs ward-software-defined-systems/wardsondb".into();
    }

    let url = format!("https://api.github.com/repos/{}/pulls?state=open&per_page=10", param);

    let client = reqwest::Client::new();
    match client
        .get(&url)
        .header("User-Agent", "embraOS/0.1.0")
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
    {
        Ok(resp) => {
            if !resp.status().is_success() {
                return format!("GitHub API error: {}", resp.status());
            }
            let prs: Vec<serde_json::Value> = resp.json().await.unwrap_or_default();
            if prs.is_empty() {
                return format!("No open PRs for {}.", param);
            }
            let mut output = format!("=== Open PRs: {} ({}) ===\n", param, prs.len());
            for pr in &prs {
                let number = pr.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
                let title = pr.get("title").and_then(|v| v.as_str()).unwrap_or("?");
                let user = pr
                    .get("user")
                    .and_then(|u| u.get("login"))
                    .and_then(|l| l.as_str())
                    .unwrap_or("?");
                output.push_str(&format!("  #{} {} (by {})\n", number, title, user));
            }
            output
        }
        Err(e) => format!("Failed to fetch PRs: {}", e),
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
    let mut args = vec!["-C", &dir, "add"];
    args.extend(file_args.iter());

    match tokio::process::Command::new("git")
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

/// Run `git commit`. Write operation — workspace restricted.
/// Param format: `<path> | <message>`
///
/// The tool-tag parser collapses literal newlines inside a tag to single
/// spaces, so multi-paragraph commit messages entered as real newlines
/// would arrive here as one line. To get a multi-line commit message
/// through, use `\n` escape sequences (and `\t` for tabs, `\\` for a
/// literal backslash) — `expand_escapes` turns them into real whitespace
/// right before the `git commit -m` invocation. Same pattern as
/// `file_write` / `file_append`.
///
/// Example: `git_commit /path | Subject line\n\nBody paragraph`
/// produces a commit with a proper subject/body split.
pub async fn git_commit(param: &str) -> String {
    let parts: Vec<&str> = param.splitn(2, " | ").collect();
    if parts.len() < 2 {
        return "Usage: git_commit <path> | <message>\nUse \\n in the message for line breaks (multi-paragraph commits).".into();
    }
    let dir = match resolve_workspace_path(parts[0].trim()) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let message = expand_escapes(parts[1].trim());

    match tokio::process::Command::new("git")
        .args(["-C", &dir, "commit", "-m", &message])
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

    let token = resolve_github_token(db).await;
    let mut args = github_token_git_args(&token);
    args.extend(["-C".to_string(), dir.clone(), "push".to_string()]);

    match tokio::process::Command::new("git")
        .args(&args)
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

    let token = resolve_github_token(db).await;
    let mut args = github_token_git_args(&token);
    args.extend(["-C".to_string(), dir.clone(), "pull".to_string()]);

    match tokio::process::Command::new("git")
        .args(&args)
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

    match tokio::process::Command::new("git")
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
/// - `<path> <name>`                → create a branch (workspace restricted)
/// - `<path> delete <name>`         → delete a branch, requires merge into `main`
/// - `<path> delete <name> <base>`  → delete a branch, requires merge into `<base>`
///
/// Delete enforces an explicit merge-base check before invoking `git branch -d`.
/// Git's own `-d` safe-delete is permissive — any tracking ref (including
/// `origin/<branch>`) makes a branch deletable, so a pushed-but-unmerged branch
/// would slip through. We pre-check `git merge-base --is-ancestor` against the
/// base ref (default `main`, falling back to `origin/<base>` if no local copy).
/// Closes Embra_Debug #49.
pub async fn git_branch(param: &str) -> String {
    let parts: Vec<&str> = param.split_whitespace().collect();
    if parts.is_empty() {
        return git_branch_list(WORKSPACE_ROOT).await;
    }
    let dir = match resolve_workspace_path(parts[0]) {
        Ok(d) => d,
        Err(e) => return e,
    };

    // Delete form: `<path> delete <name>` or `<path> delete <name> <base>`
    if parts.len() >= 3 && parts[1].eq_ignore_ascii_case("delete") {
        let name = parts[2];
        let base = parts.get(3).copied().unwrap_or("main");

        // Resolve a usable base ref: prefer local, fall back to origin/<base>.
        let base_ref = match resolve_base_ref(&dir, base).await {
            Ok(r) => r,
            Err(e) => return e,
        };

        // merge-base --is-ancestor exits 0 if branch tip is reachable from base,
        // 1 if not. Anything else is an unexpected git error.
        let ancestor = tokio::process::Command::new("git")
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
                         Merge it first, or pass a different base if you're targeting a non-default integration branch.",
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

        return match tokio::process::Command::new("git")
            .args(["-C", &dir, "branch", "-d", name])
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
        let name = parts[1];
        return match tokio::process::Command::new("git")
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
    match tokio::process::Command::new("git")
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
    let branch = parts[1];

    match tokio::process::Command::new("git")
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

/// Create a GitHub issue.
/// Param format: `<owner/repo> | <title> | <body>`
pub async fn gh_issue_create(db: &WardsonDbClient, param: &str) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };

    let parts: Vec<&str> = param.splitn(3, " | ").collect();
    if parts.len() < 2 {
        return "Usage: gh_issue_create <owner/repo> | <title> | <body>".into();
    }

    let repo = parts[0].trim();
    let title = parts[1].trim();
    let body = if parts.len() > 2 { parts[2].trim() } else { "" };

    let url = format!("https://api.github.com/repos/{}/issues", repo);
    let payload = serde_json::json!({
        "title": title,
        "body": body,
    });

    let client = reqwest::Client::new();
    match client
        .post(&url)
        .header("User-Agent", "embraOS/0.1.0")
        .header("Authorization", format!("Bearer {}", token))
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) => {
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return format!("GitHub API error {}: {}", status, body);
            }
            let issue: serde_json::Value = resp.json().await.unwrap_or_default();
            let number = issue.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
            let html_url = issue.get("html_url").and_then(|v| v.as_str()).unwrap_or("");
            format!("Issue #{} created: {}\n{}", number, title, html_url)
        }
        Err(e) => format!("Failed to create issue: {}", e),
    }
}

/// Post a comment on a GitHub issue or PR. GitHub's comment endpoint treats
/// PRs as issues for comment purposes (both types share the same API), so
/// `gh_issue_comment` and `gh_pr_comment` both route through this helper.
async fn post_github_comment(
    token: &str,
    repo: &str,
    number: &str,
    body: &str,
) -> Result<(u64, String), String> {
    let url = format!(
        "https://api.github.com/repos/{}/issues/{}/comments",
        repo, number
    );
    let payload = serde_json::json!({ "body": body });

    let client = reqwest::Client::new();
    match client
        .post(&url)
        .header("User-Agent", "embraOS/0.1.0")
        .header("Authorization", format!("Bearer {}", token))
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                let body_text = resp.text().await.unwrap_or_default();
                return Err(format!("GitHub API error {}: {}", status, body_text));
            }
            let v: serde_json::Value = resp.json().await.unwrap_or_default();
            let id = v.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
            let html_url = v
                .get("html_url")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            Ok((id, html_url))
        }
        Err(e) => Err(format!("transport error: {}", e)),
    }
}

/// Parse `<owner/repo> <number> | <body>` for comment tools.
fn parse_comment_param(param: &str) -> Result<(&str, &str, &str), String> {
    let Some((head, body)) = param.split_once(" | ") else {
        return Err("missing `|` separator before body".into());
    };
    let head = head.trim();
    let body = body.trim();
    if body.is_empty() {
        return Err("body is empty".into());
    }
    let head_parts: Vec<&str> = head.split_whitespace().collect();
    if head_parts.len() < 2 {
        return Err("need `<owner/repo> <number>` before the `|`".into());
    }
    Ok((head_parts[0], head_parts[1], body))
}

/// Post a comment on a GitHub issue.
/// Param format: `<owner/repo> <number> | <body>`
pub async fn gh_issue_comment(db: &WardsonDbClient, param: &str) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };
    let (repo, number, body) = match parse_comment_param(param) {
        Ok(v) => v,
        Err(e) => return format!("gh_issue_comment rejected ({}). Usage: gh_issue_comment <owner/repo> <number> | <body>", e),
    };
    match post_github_comment(&token, repo, number, body).await {
        Ok((_id, html_url)) => format!("Comment posted on issue #{} in {}: {}", number, repo, html_url),
        Err(e) => format!("gh_issue_comment failed: {}", e),
    }
}

/// Post a comment on a GitHub pull request. Hits the same underlying endpoint
/// as `gh_issue_comment` — GitHub treats PR comments (the conversation tab) as
/// issue comments. Separate tool for semantic clarity in the catalog.
/// Param format: `<owner/repo> <number> | <body>`
pub async fn gh_pr_comment(db: &WardsonDbClient, param: &str) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };
    let (repo, number, body) = match parse_comment_param(param) {
        Ok(v) => v,
        Err(e) => return format!("gh_pr_comment rejected ({}). Usage: gh_pr_comment <owner/repo> <number> | <body>", e),
    };
    match post_github_comment(&token, repo, number, body).await {
        Ok((_id, html_url)) => format!("Comment posted on PR #{} in {}: {}", number, repo, html_url),
        Err(e) => format!("gh_pr_comment failed: {}", e),
    }
}

/// Close a GitHub issue.
/// Param format: `<owner/repo> <number>`
pub async fn gh_issue_close(db: &WardsonDbClient, param: &str) -> String {
    patch_issue_or_pr_state(db, "issues", param, "closed", "gh_issue_close", "Issue", "closed").await
}

/// Reopen a previously-closed GitHub issue.
/// Param format: `<owner/repo> <number>`
pub async fn gh_issue_reopen(db: &WardsonDbClient, param: &str) -> String {
    patch_issue_or_pr_state(db, "issues", param, "open", "gh_issue_reopen", "Issue", "reopened").await
}

/// Close a GitHub pull request without merging.
/// Param format: `<owner/repo> <number>`
pub async fn gh_pr_close(db: &WardsonDbClient, param: &str) -> String {
    patch_issue_or_pr_state(db, "pulls", param, "closed", "gh_pr_close", "PR", "closed").await
}

/// Shared implementation for issue/PR state PATCH operations.
/// `kind` is "issues" or "pulls" (URL path segment). `new_state` is "open" or
/// "closed". Tool name + artifact label + verb drive the error/success strings.
async fn patch_issue_or_pr_state(
    db: &WardsonDbClient,
    kind: &str,
    param: &str,
    new_state: &str,
    tool_name: &str,
    artifact_label: &str,
    verb: &str,
) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };

    let parts: Vec<&str> = param.split_whitespace().collect();
    if parts.len() < 2 {
        return format!("{} rejected (missing args). Usage: {} <owner/repo> <number>", tool_name, tool_name);
    }

    let repo = parts[0];
    let number = parts[1];

    let url = format!("https://api.github.com/repos/{}/{}/{}", repo, kind, number);
    let payload = serde_json::json!({ "state": new_state });

    let client = reqwest::Client::new();
    match client
        .patch(&url)
        .header("User-Agent", "embraOS/0.1.0")
        .header("Authorization", format!("Bearer {}", token))
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return format!("{} failed: GitHub API {}: {}", tool_name, status, body);
            }
            let v: serde_json::Value = resp.json().await.unwrap_or_default();
            let html_url = v.get("html_url").and_then(|x| x.as_str()).unwrap_or("");
            if html_url.is_empty() {
                format!("{} #{} {} in {}", artifact_label, number, verb, repo)
            } else {
                format!("{} #{} {}: {}", artifact_label, number, verb, html_url)
            }
        }
        Err(e) => format!("{} failed: transport error: {}", tool_name, e),
    }
}

/// Create a GitHub pull request.
/// Param format: `<owner/repo> | <title> | <head> | <base>`
pub async fn gh_pr_create(db: &WardsonDbClient, param: &str) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };

    let parts: Vec<&str> = param.splitn(4, " | ").collect();
    if parts.len() < 4 {
        return "Usage: gh_pr_create <owner/repo> | <title> | <head> | <base>".into();
    }

    let repo = parts[0].trim();
    let title = parts[1].trim();
    let head = parts[2].trim();
    let base = parts[3].trim();

    let url = format!("https://api.github.com/repos/{}/pulls", repo);
    let payload = serde_json::json!({
        "title": title,
        "head": head,
        "base": base,
    });

    let client = reqwest::Client::new();
    match client
        .post(&url)
        .header("User-Agent", "embraOS/0.1.0")
        .header("Authorization", format!("Bearer {}", token))
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) => {
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return format!("GitHub API error {}: {}", status, body);
            }
            let pr: serde_json::Value = resp.json().await.unwrap_or_default();
            let number = pr.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
            let html_url = pr.get("html_url").and_then(|v| v.as_str()).unwrap_or("");
            format!("PR #{} created: {}\n{}", number, title, html_url)
        }
        Err(e) => format!("Failed to create PR: {}", e),
    }
}

/// Merge a GitHub pull request. Destructive-to-upstream: writes to shared
/// GitHub state under the operator's token scope — authorization is inherited,
/// not re-confirmed at call time (same model as gh_pr_create / gh_issue_close).
/// Param format: `<owner/repo> <number>` (defaults merge_method = "merge") or
/// `<owner/repo> <number> | <method>` where method is merge / squash / rebase.
pub async fn gh_pr_merge(db: &WardsonDbClient, param: &str) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };

    // Parse: split on " | " first to separate optional method.
    let (head, method) = match param.split_once(" | ") {
        Some((h, m)) => (h.trim(), m.trim()),
        None => (param.trim(), "merge"),
    };
    let parts: Vec<&str> = head.split_whitespace().collect();
    if parts.len() < 2 {
        return "gh_pr_merge rejected (missing args). Usage: gh_pr_merge <owner/repo> <number> or gh_pr_merge <owner/repo> <number> | <method> where method is merge|squash|rebase".into();
    }
    let repo = parts[0];
    let number = parts[1];
    if !matches!(method, "merge" | "squash" | "rebase") {
        return format!("gh_pr_merge rejected (invalid method '{}'; use merge, squash, or rebase)", method);
    }

    let url = format!(
        "https://api.github.com/repos/{}/pulls/{}/merge",
        repo, number
    );
    let payload = serde_json::json!({ "merge_method": method });

    let client = reqwest::Client::new();
    match client
        .put(&url)
        .header("User-Agent", "embraOS/0.1.0")
        .header("Authorization", format!("Bearer {}", token))
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            match status.as_u16() {
                200 => {
                    let v: serde_json::Value = resp.json().await.unwrap_or_default();
                    let sha = v.get("sha").and_then(|x| x.as_str()).unwrap_or("");
                    let sha_short = if sha.len() >= 7 { &sha[..7] } else { sha };
                    let merged = v.get("merged").and_then(|x| x.as_bool()).unwrap_or(false);
                    if merged {
                        format!("PR #{} merged in {} (method: {}, sha: {})", number, repo, method, sha_short)
                    } else {
                        format!("PR #{} merge API returned 200 but merged=false; check the response", number)
                    }
                }
                405 => "gh_pr_merge rejected (PR not mergeable — check approvals, required status checks, or conflicts)".into(),
                409 => "gh_pr_merge rejected (merge conflict; resolve and retry)".into(),
                _ => {
                    let body = resp.text().await.unwrap_or_default();
                    format!("gh_pr_merge failed: GitHub API {}: {}", status, body)
                }
            }
        }
        Err(e) => format!("gh_pr_merge failed: transport error: {}", e),
    }
}

/// Outcome of a classic-projects fetch.
enum ProjectsFetch {
    Ok(Vec<serde_json::Value>),
    /// 404 on both user and org endpoints — owner has no classic projects.
    NotFound,
    /// 410 Gone — classic projects deprecated for this owner.
    Gone,
    /// Transport or other API error (non-404/410 status, or request failure).
    Err(String),
}

/// Fetch classic GitHub projects for `owner`, trying the user endpoint first
/// and falling back to the org endpoint on 404. Projects v2 is NOT supported;
/// this only talks to the classic REST API.
async fn fetch_classic_projects(token: &str, owner: &str) -> ProjectsFetch {
    let client = reqwest::Client::new();
    let endpoints = [
        format!("https://api.github.com/users/{}/projects", owner),
        format!("https://api.github.com/orgs/{}/projects", owner),
    ];
    let mut last_status: Option<reqwest::StatusCode> = None;
    for url in &endpoints {
        match client
            .get(url)
            .header("User-Agent", "embraOS/0.1.0")
            .header("Authorization", format!("Bearer {}", token))
            .header("Accept", "application/vnd.github.inertia-preview+json")
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    let body: Vec<serde_json::Value> = resp.json().await.unwrap_or_default();
                    return ProjectsFetch::Ok(body);
                }
                if status.as_u16() == 404 {
                    last_status = Some(status);
                    continue; // try the other endpoint
                }
                if status.as_u16() == 410 {
                    return ProjectsFetch::Gone;
                }
                return ProjectsFetch::Err(format!("GitHub API error: {}", status));
            }
            Err(e) => return ProjectsFetch::Err(format!("Failed to fetch projects: {}", e)),
        }
    }
    if matches!(last_status.map(|s| s.as_u16()), Some(404)) {
        ProjectsFetch::NotFound
    } else {
        ProjectsFetch::Err(format!(
            "GitHub API error: {}",
            last_status.map(|s| s.to_string()).unwrap_or_else(|| "unknown".into())
        ))
    }
}

/// Shared 404/410 messages for classic-projects endpoints.
fn classic_projects_not_found_msg(owner: &str) -> String {
    format!(
        "No classic projects found for '{}'.\n\
         Note: this tool uses GitHub's classic Projects REST API. \
         Projects v2 (org-level boards) is not supported — use the GitHub web UI for those.",
        owner
    )
}

fn classic_projects_gone_msg(owner: &str) -> String {
    format!(
        "GitHub has deprecated classic projects for '{}' (410 Gone).\n\
         Use Projects v2 via the GitHub web UI (not yet supported by embraOS).",
        owner
    )
}

/// List GitHub projects for a user or org (classic REST API).
/// Param format: `<owner>`
pub async fn gh_project_list(db: &WardsonDbClient, param: &str) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };

    if param.is_empty() {
        return "Usage: gh_project_list <owner>".into();
    }

    let owner = param.trim();
    match fetch_classic_projects(&token, owner).await {
        ProjectsFetch::Ok(projects) => {
            if projects.is_empty() {
                return format!("No projects found for {}.", owner);
            }
            let mut output = format!("=== Projects: {} ({}) ===\n", owner, projects.len());
            for proj in &projects {
                let id = proj.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                let name = proj.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let state = proj.get("state").and_then(|v| v.as_str()).unwrap_or("?");
                output.push_str(&format!("  #{} {} ({})\n", id, name, state));
            }
            output
        }
        ProjectsFetch::NotFound => classic_projects_not_found_msg(owner),
        ProjectsFetch::Gone => classic_projects_gone_msg(owner),
        ProjectsFetch::Err(e) => e,
    }
}

/// View a specific GitHub classic project.
/// Param format: `<owner> <number>`
pub async fn gh_project_view(db: &WardsonDbClient, param: &str) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };

    let parts: Vec<&str> = param.split_whitespace().collect();
    if parts.len() < 2 {
        return "Usage: gh_project_view <owner> <number>".into();
    }

    let owner = parts[0];
    let number = parts[1];
    let target_num: u64 = number.parse().unwrap_or(0);

    match fetch_classic_projects(&token, owner).await {
        ProjectsFetch::Ok(projects) => {
            if let Some(proj) = projects.iter().find(|p| {
                p.get("number").and_then(|v| v.as_u64()).unwrap_or(0) == target_num
            }) {
                serde_json::to_string_pretty(proj).unwrap_or_else(|_| "Failed to format project".into())
            } else {
                format!("Project #{} not found for {}", number, owner)
            }
        }
        ProjectsFetch::NotFound => classic_projects_not_found_msg(owner),
        ProjectsFetch::Gone => classic_projects_gone_msg(owner),
        ProjectsFetch::Err(e) => e,
    }
}

/// Maximum bytes a single file_read call can return. Matches the global
/// MAX_TOOL_RESULT_SIZE in tools/mod.rs so there is no disparity between the
/// tool's internal limit and the wrapper's truncation.
const FILE_READ_MAX: usize = 2_097_152; // 2 MiB

/// Read a file's contents. Unrestricted path.
///
/// Param format: `<path>[|<offset>[|<limit>]]`
/// - `path`: file or directory path
/// - `offset`: byte offset to start reading (default 0)
/// - `limit`: max bytes to read (default and ceiling: FILE_READ_MAX)
///
/// When the read doesn't reach EOF, a trailer tells the model how to continue
/// with the next slice.
pub async fn file_read(params: &str) -> String {
    if params.is_empty() {
        return "Usage: file_read <path>[|<offset>[|<limit>]]\n\
                Example: file_read /embra/workspace/repos/myrepo/README.md\n\
                Example: file_read /path/to/big.log|2097152|1048576".into();
    }

    let mut parts = params.splitn(3, '|').map(str::trim);
    let path = parts.next().unwrap_or("");
    let offset: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let limit: usize = parts
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(FILE_READ_MAX)
        .min(FILE_READ_MAX);

    if path.is_empty() {
        return "Usage: file_read <path>[|<offset>[|<limit>]]".into();
    }

    let file_path = std::path::Path::new(path);

    if !file_path.exists() {
        return format!("File not found: {}", path);
    }
    if file_path.is_dir() {
        // List directory contents instead — offset/limit are meaningless here.
        match tokio::fs::read_dir(path).await {
            Ok(mut entries) => {
                let mut listing = format!("=== Directory: {} ===\n", path);
                let mut count = 0u32;
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                    let suffix = if is_dir { "/" } else { "" };
                    listing.push_str(&format!("  {}{}\n", name, suffix));
                    count += 1;
                    if count >= 200 {
                        listing.push_str("  ... (truncated at 200 entries)\n");
                        break;
                    }
                }
                if count == 0 {
                    listing.push_str("  (empty directory)\n");
                }
                return listing;
            }
            Err(e) => return format!("Failed to read directory: {}", e),
        }
    }

    // File branch — seek + read_exact the requested slice.
    let meta = match tokio::fs::metadata(path).await {
        Ok(m) => m,
        Err(e) => return format!("Failed to stat file: {}", e),
    };
    let size = meta.len();
    if offset >= size && size > 0 {
        return format!(
            "Offset {} is at or past EOF ({} bytes)",
            offset, size
        );
    }
    let read_end = (offset + limit as u64).min(size);
    let read_bytes = (read_end - offset) as usize;

    use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
    let mut f = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) => return format!("Failed to open file: {}", e),
    };
    if let Err(e) = f.seek(SeekFrom::Start(offset)).await {
        return format!("Failed to seek file: {}", e);
    }
    let mut buf = vec![0u8; read_bytes];
    if let Err(e) = f.read_exact(&mut buf).await {
        return format!("Failed to read file: {}", e);
    }

    // Preserve the existing binary-file detection: a null byte in the first
    // 1KB strongly suggests non-text content.
    let head = &buf[..buf.len().min(1024)];
    if head.contains(&0u8) {
        return format!("{} is a binary file ({} bytes)", path, size);
    }

    let content = String::from_utf8_lossy(&buf);
    let more = if read_end < size {
        format!(
            "\n[... {} more bytes at offset {}. Continue with file_read {}|{}|{} ]",
            size - read_end,
            read_end,
            path,
            read_end,
            limit
        )
    } else {
        String::new()
    };
    format!(
        "=== {} ({} bytes, showing {}..{}) ===\n{}{}",
        path, size, offset, read_end, content, more
    )
}

/// Expand escape sequences in tool content: `\\n` → newline, `\\t` → tab, `\\\\` → backslash.
fn expand_escapes(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('t') => result.push('\t'),
                Some('\\') => result.push('\\'),
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(ch);
        }
    }
    result
}

/// Ensure parent directories exist for a path. Workspace restricted.
async fn ensure_parent_dirs(path: &str) -> Result<(), String> {
    let file_path = std::path::Path::new(path);
    if let Some(parent) = file_path.parent() {
        if !parent.exists() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("Failed to create directory {}: {}", parent.display(), e))?;
        }
    }
    Ok(())
}

/// Write content to a file (overwrite). Workspace restricted.
/// Param format: `<path> | <content>`
/// Supports `\n` for newlines and `\t` for tabs in content.
/// Creates parent directories if they don't exist.
pub async fn file_write(param: &str) -> String {
    if param.is_empty() {
        return "Usage: file_write <path> | <content>\n\
                Use \\n for newlines, \\t for tabs.\n\
                Example: file_write /embra/workspace/repos/myrepo/notes.txt | line 1\\nline 2\\nline 3"
            .into();
    }

    let parts: Vec<&str> = param.splitn(2, " | ").collect();
    if parts.len() < 2 {
        return "Usage: file_write <path> | <content>".into();
    }

    let raw_path = parts[0].trim();
    let content = expand_escapes(parts[1]);

    let path = match resolve_workspace_path(raw_path) {
        Ok(p) => p,
        Err(e) => return e,
    };

    if let Err(e) = ensure_parent_dirs(&path).await {
        return e;
    }

    match tokio::fs::write(&path, &content).await {
        Ok(()) => {
            let line_count = content.lines().count();
            format!("Written {} bytes ({} lines) to {}", content.len(), line_count, path)
        }
        Err(e) => format!("Failed to write file: {}", e),
    }
}

/// Append content to a file. Workspace restricted.
/// Param format: `<path> | <content>`
/// Supports `\n` for newlines and `\t` for tabs in content.
/// Creates the file (and parent directories) if it doesn't exist.
pub async fn file_append(param: &str) -> String {
    if param.is_empty() {
        return "Usage: file_append <path> | <content>\n\
                Use \\n for newlines, \\t for tabs.\n\
                Example: file_append /embra/workspace/repos/myrepo/log.txt | New entry\\n"
            .into();
    }

    let parts: Vec<&str> = param.splitn(2, " | ").collect();
    if parts.len() < 2 {
        return "Usage: file_append <path> | <content>".into();
    }

    let raw_path = parts[0].trim();
    let content = expand_escapes(parts[1]);

    let path = match resolve_workspace_path(raw_path) {
        Ok(p) => p,
        Err(e) => return e,
    };

    if let Err(e) = ensure_parent_dirs(&path).await {
        return e;
    }

    use tokio::io::AsyncWriteExt;
    let result = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await;

    match result {
        Ok(mut file) => match file.write_all(content.as_bytes()).await {
            Ok(()) => format!("Appended {} bytes to {}", content.len(), path),
            Err(e) => format!("Failed to append: {}", e),
        },
        Err(e) => format!("Failed to open file for append: {}", e),
    }
}

/// Create a directory (and parents). Workspace restricted.
/// Param format: `<path>`
pub async fn mkdir(param: &str) -> String {
    if param.is_empty() {
        return "Usage: mkdir <path>\nExample: mkdir /embra/workspace/repos/myrepo/src/utils".into();
    }

    let raw_path = param.trim();

    let path = match resolve_workspace_path(raw_path) {
        Ok(p) => p,
        Err(e) => return e,
    };

    if std::path::Path::new(&path).exists() {
        return format!("Directory already exists: {}", path);
    }

    match tokio::fs::create_dir_all(&path).await {
        Ok(()) => format!("Created directory: {}", path),
        Err(e) => format!("Failed to create directory: {}", e),
    }
}

/// Create a symbolic link. Workspace restricted (both target and link path must
/// be under `/embra/workspace/`). Dangling targets are allowed — operators can
/// create a forward-reference symlink and populate the target afterwards.
/// Param format: `<target> | <link_path>`
pub async fn file_symlink(param: &str) -> String {
    let parts: Vec<&str> = param.splitn(2, '|').collect();
    if parts.len() < 2 {
        return "file_symlink rejected (missing arguments). Usage: file_symlink <target> | <link_path>\nExample: file_symlink /embra/workspace/repos/foo/src | /embra/workspace/src-link".into();
    }

    let raw_target = parts[0].trim();
    let raw_link = parts[1].trim();

    if raw_target.is_empty() {
        return "file_symlink rejected (target is empty before the '|')".into();
    }
    if raw_link.is_empty() {
        return "file_symlink rejected (link path is empty after the '|')".into();
    }

    let target = match resolve_workspace_path(raw_target) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let link = match resolve_workspace_path(raw_link) {
        Ok(p) => p,
        Err(e) => return e,
    };

    if std::path::Path::new(&link).exists() {
        return format!("file_symlink rejected (link path already exists: {})", link);
    }

    match tokio::fs::symlink(&target, &link).await {
        Ok(()) => format!("Symlink created: {} → {}", link, target),
        Err(e) => format!("file_symlink failed: {}", e),
    }
}

/// Delete a file or symlink. Workspace restricted. Refuses real directories
/// (use a shell command for recursive removal). Handles symlinks cleanly:
/// - Dangling symlinks (target missing): unlinked normally; `Path::exists()`
///   would have returned false and misled the old implementation into a
///   "File not found" error.
/// - Symlinks to directories: unlinked (only the link goes; the target
///   directory is untouched). `Path::is_dir()` follows symlinks and would
///   have refused these.
/// Param format: `<path>`
pub async fn file_delete(param: &str) -> String {
    if param.is_empty() {
        return "Usage: file_delete <path>\nExample: file_delete /embra/workspace/repos/myrepo/old_file.txt".into();
    }

    let raw_path = param.trim();

    let path = match resolve_workspace_path(raw_path) {
        Ok(p) => p,
        Err(e) => return e,
    };

    // symlink_metadata does NOT follow symlinks — critical for correct handling
    // of dangling links and links pointing at directories.
    let meta = match tokio::fs::symlink_metadata(&path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return format!("File not found: {}", path);
        }
        Err(e) => return format!("Failed to stat {}: {}", path, e),
    };

    if meta.file_type().is_symlink() {
        // Unlink the link itself. Target state is irrelevant.
        return match tokio::fs::remove_file(&path).await {
            Ok(()) => format!("Deleted symlink: {}", path),
            Err(e) => format!("Failed to delete {}: {}", path, e),
        };
    }

    if meta.is_dir() {
        return format!("Cannot delete directory with file_delete (use a shell command for recursive removal): {}", path);
    }

    match tokio::fs::remove_file(&path).await {
        Ok(()) => format!("Deleted: {}", path),
        Err(e) => format!("Failed to delete {}: {}", path, e),
    }
}

/// Move or rename a file or directory. Workspace restricted (both source and destination).
/// Param format: `<source> | <destination>`
pub async fn file_move(param: &str) -> String {
    let parts: Vec<&str> = param.splitn(2, '|').collect();
    if parts.len() < 2 {
        return "Usage: file_move <source> | <destination>\nExample: file_move /embra/workspace/repos/myrepo/old.rs | /embra/workspace/repos/myrepo/new.rs".into();
    }

    let raw_src = parts[0].trim();
    let raw_dst = parts[1].trim();

    let src = match resolve_workspace_path(raw_src) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let dst = match resolve_workspace_path(raw_dst) {
        Ok(p) => p,
        Err(e) => return e,
    };

    if !std::path::Path::new(&src).exists() {
        return format!("Source not found: {}", src);
    }

    if std::path::Path::new(&dst).exists() {
        return format!("Destination already exists: {}", dst);
    }

    // Ensure parent directory of destination exists
    if let Err(e) = ensure_parent_dirs(&dst).await {
        return e;
    }

    match tokio::fs::rename(&src, &dst).await {
        Ok(()) => format!("Moved: {} → {}", src, dst),
        Err(e) => format!("Failed to move {} → {}: {}", src, dst, e),
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

    match tokio::process::Command::new("git")
        .args(&["-C", &dir, "rm"])
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

    let src = parts[1];
    let dst = parts[2];

    match tokio::process::Command::new("git")
        .args(["-C", &dir, "mv", src, dst])
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

/// Remove a directory. Workspace restricted.
/// Removes empty directories by default; use `--force` suffix to remove non-empty directories.
/// Param format: `<path>` or `<path> --force`
pub async fn dir_delete(param: &str) -> String {
    if param.is_empty() {
        return "Usage: dir_delete <path> or dir_delete <path> --force\nWithout --force, only empty directories are removed.".into();
    }

    let (raw_path, force) = if param.trim_end().ends_with("--force") {
        (param.trim_end().trim_end_matches("--force").trim(), true)
    } else {
        (param.trim(), false)
    };

    let path = match resolve_workspace_path(raw_path) {
        Ok(p) => p,
        Err(e) => return e,
    };

    let p = std::path::Path::new(&path);
    if !p.exists() {
        return format!("Directory not found: {}", path);
    }

    if !p.is_dir() {
        return format!("Not a directory (use file_delete for files): {}", path);
    }

    if force {
        match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => format!("Deleted directory and all contents: {}", path),
            Err(e) => format!("Failed to delete directory {}: {}", path, e),
        }
    } else {
        match tokio::fs::remove_dir(&path).await {
            Ok(()) => format!("Deleted empty directory: {}", path),
            Err(e) => {
                if e.kind() == std::io::ErrorKind::DirectoryNotEmpty
                    || e.to_string().contains("not empty")
                    || e.to_string().contains("Directory not empty")
                {
                    format!(
                        "Directory is not empty: {}. Use dir_delete {} --force to remove with contents.",
                        path, path
                    )
                } else {
                    format!("Failed to delete directory {}: {}", path, e)
                }
            }
        }
    }
}

async fn ensure_collection(db: &WardsonDbClient, name: &str) {
    if !db.collection_exists(name).await.unwrap_or(true) {
        let _ = db.create_collection(name).await;
    }
}

// ── Native tool-use registrations (NATIVE-TOOLS-01) ──

use embra_tool_macro::embra_tool;
use embra_tools_core::DispatchError;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::tools::registry::DispatchContext;

// -- Git repository tools -----------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "git_clone",
    is_side_effectful = true,
    description = "Clone a git repository into /embra/workspace/. HTTPS uses the stored GitHub token; SSH is also supported. subpath may be a bare directory name (`myrepo`), a workspace-relative path (`repos/myrepo`), or an absolute path under the workspace (`/embra/workspace/repos/myrepo`); if omitted, the repo name is derived from the URL."
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
    description = "Show recent git log for a directory under /embra/workspace. args is an optional free-form git-log argument string (e.g. `-n 20 --oneline`). path may be absolute (`/embra/workspace/repo`) or relative (`repo`)."
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
    description = "Commit staged changes in a workspace repository. The message may include \\n for newlines (expanded before git invocation) to create multi-paragraph messages with subject line + body. path may be absolute (`/embra/workspace/repo`) or relative (`repo`)."
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
    description = "List, create, or delete branches in a workspace repository. action=list returns current branches; action=create requires name; action=delete requires name and refuses any branch with commits not yet merged into the base ref (default `main`, override via `base`). The merge check falls back to `origin/<base>` if no local copy exists. path may be absolute (`/embra/workspace/repo`) or relative (`repo`)."
)]
pub struct GitBranchArgs {
    pub path: String,
    #[serde(default = "default_git_branch_action")]
    pub action: GitBranchAction,
    #[serde(default)]
    pub name: Option<String>,
    /// Integration base for the merged-into check on action=delete.
    /// Defaults to `main`. Ignored for list/create.
    #[serde(default)]
    pub base: Option<String>,
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
                let base = self.base.as_deref().unwrap_or("main");
                format!("{} delete {} {}", self.path, n, base)
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

// -- Planning tools -----------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "plan",
    is_side_effectful = true,
    description = "Plan management. Without fields, lists all plans. With title (and optional description), creates a new plan."
)]
pub struct PlanArgs {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

impl PlanArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = match (self.title, self.description) {
            (None, _) => String::new(),
            (Some(t), None) => t,
            (Some(t), Some(d)) => format!("{} | {}", t, d),
        };
        Ok(plan(ctx.db, &param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "tasks",
    description = "List tasks. filter narrows by plan_id or title substring."
)]
pub struct TasksArgs {
    #[serde(default)]
    pub filter: String,
}

impl TasksArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(tasks(ctx.db, &self.filter).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "task_add",
    is_side_effectful = true,
    description = "Create a new task. plan_id associates it with an existing plan."
)]
pub struct TaskAddArgs {
    pub title: String,
    #[serde(default)]
    pub plan_id: Option<String>,
}

impl TaskAddArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = match self.plan_id {
            Some(p) if !p.is_empty() => format!("{} | {}", self.title, p),
            _ => self.title,
        };
        Ok(task_add(ctx.db, &param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "task_done",
    is_side_effectful = true,
    description = "Mark a task as done by its id."
)]
pub struct TaskDoneArgs {
    pub id: String,
}

impl TaskDoneArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(task_done(ctx.db, &self.id).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "task_delete",
    is_side_effectful = true,
    description = "Delete a task by id (irreversible). Use task_done if you only want to mark it complete."
)]
pub struct TaskDeleteArgs {
    pub id: String,
}

impl TaskDeleteArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(task_delete(ctx.db, &self.id).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "plan_delete",
    is_side_effectful = true,
    description = "Delete a plan by id (irreversible). cascade_tasks=true also removes tasks whose plan_id matches; default false leaves them orphaned."
)]
pub struct PlanDeleteArgs {
    pub id: String,
    #[serde(default)]
    pub cascade_tasks: bool,
}

impl PlanDeleteArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(plan_delete(ctx.db, &self.id, self.cascade_tasks).await)
    }
}

// -- GitHub tools -------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gh_issues",
    description = "List open GitHub issues for owner/repo (requires GITHUB_TOKEN)."
)]
pub struct GhIssuesArgs {
    /// owner/repo, e.g. `Ward-Software-Defined-Systems/embraOS`.
    pub repo: String,
}

impl GhIssuesArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(gh_issues(ctx.db, &self.repo).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gh_prs",
    description = "List open GitHub pull requests for owner/repo (requires GITHUB_TOKEN)."
)]
pub struct GhPrsArgs {
    pub repo: String,
}

impl GhPrsArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(gh_prs(ctx.db, &self.repo).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gh_issue_create",
    is_side_effectful = true,
    description = "Create a GitHub issue in owner/repo."
)]
pub struct GhIssueCreateArgs {
    pub repo: String,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
}

impl GhIssueCreateArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = match self.body {
            Some(b) => format!("{} | {} | {}", self.repo, self.title, b),
            None => format!("{} | {}", self.repo, self.title),
        };
        Ok(gh_issue_create(ctx.db, &param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gh_issue_close",
    is_side_effectful = true,
    description = "Close a GitHub issue by owner/repo and number."
)]
pub struct GhIssueCloseArgs {
    pub repo: String,
    pub number: u32,
}

impl GhIssueCloseArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{} {}", self.repo, self.number);
        Ok(gh_issue_close(ctx.db, &param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gh_issue_reopen",
    is_side_effectful = true,
    description = "Reopen a previously-closed GitHub issue."
)]
pub struct GhIssueReopenArgs {
    pub repo: String,
    pub number: u32,
}

impl GhIssueReopenArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{} {}", self.repo, self.number);
        Ok(gh_issue_reopen(ctx.db, &param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gh_issue_comment",
    is_side_effectful = true,
    description = "Post a comment on a GitHub issue."
)]
pub struct GhIssueCommentArgs {
    pub repo: String,
    pub number: u32,
    pub body: String,
}

impl GhIssueCommentArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{} {} | {}", self.repo, self.number, self.body);
        Ok(gh_issue_comment(ctx.db, &param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gh_pr_create",
    is_side_effectful = true,
    description = "Create a GitHub pull request. head is the source branch (e.g. \"feature-foo\"), base is the target (usually \"main\")."
)]
pub struct GhPrCreateArgs {
    pub repo: String,
    pub title: String,
    pub head: String,
    pub base: String,
}

impl GhPrCreateArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!(
            "{} | {} | {} | {}",
            self.repo, self.title, self.head, self.base
        );
        Ok(gh_pr_create(ctx.db, &param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gh_pr_close",
    is_side_effectful = true,
    description = "Close a GitHub pull request without merging."
)]
pub struct GhPrCloseArgs {
    pub repo: String,
    pub number: u32,
}

impl GhPrCloseArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{} {}", self.repo, self.number);
        Ok(gh_pr_close(ctx.db, &param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gh_pr_comment",
    is_side_effectful = true,
    description = "Post a comment on a GitHub PR's conversation tab."
)]
pub struct GhPrCommentArgs {
    pub repo: String,
    pub number: u32,
    pub body: String,
}

impl GhPrCommentArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{} {} | {}", self.repo, self.number, self.body);
        Ok(gh_pr_comment(ctx.db, &param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PrMergeMethod {
    Merge,
    Squash,
    Rebase,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gh_pr_merge",
    is_side_effectful = true,
    description = "Merge a GitHub PR. Destructive to the upstream branch — writes to shared state. method defaults to \"merge\"; other options are \"squash\" and \"rebase\"."
)]
pub struct GhPrMergeArgs {
    pub repo: String,
    pub number: u32,
    #[serde(default = "default_pr_merge_method")]
    pub method: PrMergeMethod,
}

fn default_pr_merge_method() -> PrMergeMethod {
    PrMergeMethod::Merge
}

impl GhPrMergeArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let method_str = match self.method {
            PrMergeMethod::Merge => "merge",
            PrMergeMethod::Squash => "squash",
            PrMergeMethod::Rebase => "rebase",
        };
        let param = format!("{} {} | {}", self.repo, self.number, method_str);
        Ok(gh_pr_merge(ctx.db, &param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gh_project_list",
    description = "List GitHub projects for an owner (classic Projects API)."
)]
pub struct GhProjectListArgs {
    pub owner: String,
}

impl GhProjectListArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(gh_project_list(ctx.db, &self.owner).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gh_project_view",
    description = "View a specific GitHub project by owner and number."
)]
pub struct GhProjectViewArgs {
    pub owner: String,
    pub number: u32,
}

impl GhProjectViewArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{} {}", self.owner, self.number);
        Ok(gh_project_view(ctx.db, &param).await)
    }
}

// -- Filesystem tools ---------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "file_read",
    description = "Read a file or list a directory. offset starts the read at a byte position; limit caps the number of bytes returned. Unrestricted read path."
)]
pub struct FileReadArgs {
    pub path: String,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default)]
    pub limit: Option<u64>,
}

impl FileReadArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let mut param = self.path;
        if let Some(o) = self.offset {
            param = format!("{}|{}", param, o);
            if let Some(l) = self.limit {
                param = format!("{}|{}", param, l);
            }
        } else if self.limit.is_some() {
            // Edge case: limit without offset — retain both in the legacy format.
            param = format!("{}|0|{}", param, self.limit.unwrap());
        }
        Ok(file_read(&param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "file_write",
    is_side_effectful = true,
    description = "Write (overwrite) a file under /embra/workspace. path may be absolute (`/embra/workspace/repo/notes.txt`) or workspace-relative (`repo/notes.txt`). Use \\n for newlines, \\t for tabs — these are expanded before writing."
)]
pub struct FileWriteArgs {
    pub path: String,
    pub content: String,
}

impl FileWriteArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{} | {}", self.path, self.content);
        Ok(file_write(&param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "file_append",
    is_side_effectful = true,
    description = "Append to a file under /embra/workspace (creates if missing). path may be absolute or workspace-relative. Use \\n for newlines."
)]
pub struct FileAppendArgs {
    pub path: String,
    pub content: String,
}

impl FileAppendArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{} | {}", self.path, self.content);
        Ok(file_append(&param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "file_delete",
    is_side_effectful = true,
    description = "Delete a file under /embra/workspace (files only, not directories). path may be absolute or workspace-relative. Handles symlinks without following them."
)]
pub struct FileDeleteArgs {
    pub path: String,
}

impl FileDeleteArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(file_delete(&self.path).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "file_move",
    is_side_effectful = true,
    description = "Move or rename a file or directory under /embra/workspace. source and destination may each be absolute or workspace-relative."
)]
pub struct FileMoveArgs {
    pub source: String,
    pub destination: String,
}

impl FileMoveArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{}|{}", self.source, self.destination);
        Ok(file_move(&param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "file_rename",
    is_side_effectful = true,
    description = "Alias for file_move. Move or rename a file or directory under /embra/workspace. source and destination may each be absolute or workspace-relative."
)]
pub struct FileRenameArgs {
    pub source: String,
    pub destination: String,
}

impl FileRenameArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        FileMoveArgs {
            source: self.source,
            destination: self.destination,
        }
        .run(ctx)
        .await
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "file_symlink",
    is_side_effectful = true,
    description = "Create a symbolic link at link_path pointing to target. Both paths must resolve under /embra/workspace and may each be absolute or workspace-relative. Dangling targets allowed."
)]
pub struct FileSymlinkArgs {
    pub target: String,
    pub link_path: String,
}

impl FileSymlinkArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{}|{}", self.target, self.link_path);
        Ok(file_symlink(&param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "dir_delete",
    is_side_effectful = true,
    description = "Remove a directory under /embra/workspace. path may be absolute or workspace-relative. By default refuses non-empty directories; force=true recursively deletes contents."
)]
pub struct DirDeleteArgs {
    pub path: String,
    #[serde(default)]
    pub force: bool,
}

impl DirDeleteArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = if self.force {
            format!("{} --force", self.path)
        } else {
            self.path
        };
        Ok(dir_delete(&param).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "rmdir",
    is_side_effectful = true,
    description = "Alias for dir_delete. Remove a directory under /embra/workspace; path may be absolute or workspace-relative. Set force=true to recursively delete contents."
)]
pub struct RmdirArgs {
    pub path: String,
    #[serde(default)]
    pub force: bool,
}

impl RmdirArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        DirDeleteArgs {
            path: self.path,
            force: self.force,
        }
        .run(ctx)
        .await
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "mkdir",
    is_side_effectful = true,
    description = "Create a directory (and parents as needed) under /embra/workspace. path may be absolute or workspace-relative."
)]
pub struct MkdirArgs {
    pub path: String,
}

impl MkdirArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(mkdir(&self.path).await)
    }
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

    #[tokio::test]
    async fn file_family_rejects_traversal_uniformly() {
        // Embra_Debug #52/#53: the file_* and dir_* families now share the
        // same resolver as the git_* family, so traversal rejections carry
        // the uniform `'..'` message (from resolve_workspace_path) instead
        // of silently slipping past the old prefix-only check.
        let rejections = [
            file_write("../etc/passwd | content").await,
            file_append("../etc/passwd | content").await,
            file_delete("../etc/passwd").await,
            file_move("../src|../dst").await,
            file_symlink("/embra/workspace/a|../etc/foo").await,
            mkdir("../etc").await,
            dir_delete("../etc").await,
        ];
        for msg in rejections.iter() {
            assert!(
                msg.contains("'..'"),
                "expected uniform traversal rejection, got: {}",
                msg
            );
        }
    }

    #[tokio::test]
    async fn file_family_accepts_relative_workspace_paths() {
        // Relative inputs should join through resolve_workspace_path and
        // surface filesystem-level errors (not the old "is not under
        // /embra/workspace" rejection) when /embra/workspace/ is unavailable
        // on the test host. A filesystem error proves the resolver let the
        // path through and the kernel rejected it — the outcome we want.
        let msg = file_write("Embra_Debug/_relpath_probe.txt | ok").await;
        assert!(
            !msg.contains("is not under"),
            "old validate_workspace_path rejection leaked back in, got: {}",
            msg
        );
    }

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
    fn gh_pr_merge_method_default_merge() {
        let a: GhPrMergeArgs = serde_json::from_value(serde_json::json!({
            "repo": "x/y", "number": 42
        }))
        .unwrap();
        assert!(matches!(a.method, PrMergeMethod::Merge));

        let b: GhPrMergeArgs = serde_json::from_value(serde_json::json!({
            "repo": "x/y", "number": 42, "method": "squash"
        }))
        .unwrap();
        assert!(matches!(b.method, PrMergeMethod::Squash));
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
    }

    #[test]
    fn file_read_optional_offset_limit() {
        let a: FileReadArgs =
            serde_json::from_value(serde_json::json!({"path": "/tmp/x"})).unwrap();
        assert!(a.offset.is_none());
        assert!(a.limit.is_none());

        let b: FileReadArgs = serde_json::from_value(serde_json::json!({
            "path": "/tmp/x", "offset": 100, "limit": 500
        }))
        .unwrap();
        assert_eq!(b.offset, Some(100));
        assert_eq!(b.limit, Some(500));
    }

    #[test]
    fn dir_delete_force_default_false() {
        let a: DirDeleteArgs =
            serde_json::from_value(serde_json::json!({"path": "/tmp/x"})).unwrap();
        assert!(!a.force);

        let b: DirDeleteArgs = serde_json::from_value(serde_json::json!({
            "path": "/tmp/x", "force": true
        }))
        .unwrap();
        assert!(b.force);
    }

    #[test]
    fn plan_delete_cascade_tasks_defaults_false() {
        // Embra_Debug #46: cascade is opt-in to avoid surprising the operator
        // when they only meant to clear a plan record.
        let a: PlanDeleteArgs =
            serde_json::from_value(serde_json::json!({"id": "plan-1"})).unwrap();
        assert_eq!(a.id, "plan-1");
        assert!(!a.cascade_tasks);

        let b: PlanDeleteArgs = serde_json::from_value(serde_json::json!({
            "id": "plan-1", "cascade_tasks": true
        }))
        .unwrap();
        assert!(b.cascade_tasks);
    }

    #[test]
    fn engineering_tools_register() {
        let names: Vec<&'static str> = inventory::iter::<crate::tools::registry::ToolDescriptor>()
            .into_iter()
            .map(|d| d.name)
            .collect();
        for expected in [
            "git_clone", "git_status", "git_log", "git_add", "git_commit",
            "git_push", "git_pull", "git_diff", "git_branch", "git_checkout",
            "git_rm", "git_mv",
            "plan", "plan_delete", "tasks", "task_add", "task_done", "task_delete",
            "gh_issues", "gh_prs", "gh_issue_create", "gh_issue_close",
            "gh_issue_reopen", "gh_issue_comment",
            "gh_pr_create", "gh_pr_close", "gh_pr_comment", "gh_pr_merge",
            "gh_project_list", "gh_project_view",
            "file_read", "file_write", "file_append", "file_delete",
            "file_move", "file_rename", "file_symlink",
            "dir_delete", "rmdir", "mkdir",
        ] {
            assert!(
                names.contains(&expected),
                "{} not registered in tool inventory",
                expected
            );
        }
    }
}
