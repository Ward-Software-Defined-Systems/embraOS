use chrono::Utc;

use crate::db::WardsonDbClient;

const WORKSPACE_ROOT: &str = "/embra/workspace";

fn validate_workspace_path(path: &str) -> Result<String, String> {
    let dir = if path.is_empty() { WORKSPACE_ROOT } else { path };
    if !dir.starts_with(WORKSPACE_ROOT) {
        return Err(format!(
            "Denied: path '{}' is not under {}",
            dir, WORKSPACE_ROOT
        ));
    }
    Ok(dir.to_string())
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
/// `<subpath>` may be a bare dirname (`myrepo`) or a relative path under the
/// workspace (`repos/myrepo`). Absolute paths and `..` segments are rejected.
pub async fn git_clone(db: &WardsonDbClient, param: &str) -> String {
    if param.is_empty() {
        return "Usage: [TOOL:git_clone <url>] or [TOOL:git_clone <url> <subpath>]".into();
    }

    let parts: Vec<&str> = param.split_whitespace().collect();
    let url = parts[0];

    let derived_name = url
        .rsplit('/')
        .next()
        .unwrap_or("repo")
        .trim_end_matches(".git")
        .to_string();

    let subpath = if parts.len() > 1 {
        let raw = parts[1];
        if raw.starts_with('/') {
            return format!("Denied: subpath '{}' must be relative to {}", raw, WORKSPACE_ROOT);
        }
        if std::path::Path::new(raw)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return format!("Denied: subpath '{}' contains '..' segments", raw);
        }
        if raw.ends_with('/') {
            format!("{}{}", raw, derived_name)
        } else {
            raw.to_string()
        }
    } else {
        derived_name
    };

    let dest = format!("{}/{}", WORKSPACE_ROOT, subpath);

    if let Err(e) = validate_workspace_path(&dest) {
        return e;
    }

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
    let dir = if path.is_empty() { "." } else { path };
    match tokio::process::Command::new("git")
        .args(["-C", dir, "status", "--short"])
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

/// Run `git log` with optional params.
pub async fn git_log(param: &str) -> String {
    let parts: Vec<&str> = param.split_whitespace().collect();

    // First arg may be a path, rest are git log args
    let (dir, extra_args) = if parts.is_empty() {
        (".", vec![])
    } else if std::path::Path::new(parts[0]).is_dir() {
        (parts[0], parts[1..].to_vec())
    } else {
        (".", parts)
    };

    let mut args = vec!["-C", dir, "log", "--oneline", "-20"];
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
            return "No plans found. Create one with: [TOOL:plan <title> | <description>]".into();
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
            return "No tasks found. Add one with: [TOOL:task_add <title>]".into();
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
        return "Usage: [TOOL:task_add <title>] or [TOOL:task_add <title> | <plan_id>]".into();
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
        return "Usage: [TOOL:task_done <task_id>]".into();
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

/// Fetch GitHub issues via API.
pub async fn gh_issues(db: &WardsonDbClient, param: &str) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };

    if param.is_empty() {
        return "Usage: [TOOL:gh_issues <owner/repo>]\nExample: [TOOL:gh_issues ward-software-defined-systems/wardsondb]".into();
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
        return "Usage: [TOOL:gh_prs <owner/repo>]\nExample: [TOOL:gh_prs ward-software-defined-systems/wardsondb]".into();
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
        return "Usage: [TOOL:git_add <path> <files>]\nExample: [TOOL:git_add /embra/workspace/repos/myrepo file.txt]".into();
    }
    let dir = match validate_workspace_path(parts[0]) {
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
pub async fn git_commit(param: &str) -> String {
    let parts: Vec<&str> = param.splitn(2, " | ").collect();
    if parts.len() < 2 {
        return "Usage: [TOOL:git_commit <path> | <message>]".into();
    }
    let dir = match validate_workspace_path(parts[0].trim()) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let message = parts[1].trim();

    match tokio::process::Command::new("git")
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
    let dir = match validate_workspace_path(param.trim()) {
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
    let dir = match validate_workspace_path(param.trim()) {
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

/// Run `git diff`. Read-only — unrestricted.
/// Param format: `<path> [file]`
pub async fn git_diff(param: &str) -> String {
    let parts: Vec<&str> = param.split_whitespace().collect();
    let (dir, file) = if parts.is_empty() {
        (".", None)
    } else if parts.len() == 1 {
        (parts[0], None)
    } else {
        (parts[0], Some(parts[1]))
    };

    let mut args = vec!["-C", dir, "diff"];
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

/// Run `git branch`. Three forms:
/// - `<path>`              → list branches (read-only, unrestricted path)
/// - `<path> <name>`       → create a branch (workspace restricted)
/// - `<path> delete <name>` → delete a branch (workspace restricted, `-d` only
///   so unmerged branches require manual removal; operator safety)
pub async fn git_branch(param: &str) -> String {
    let parts: Vec<&str> = param.split_whitespace().collect();
    if parts.is_empty() {
        // List in current dir
        return git_branch_list(".").await;
    }
    let dir = parts[0];

    // Delete form: `<path> delete <name>`
    if parts.len() >= 3 && parts[1].eq_ignore_ascii_case("delete") {
        let name = parts[2];
        if let Err(e) = validate_workspace_path(dir) {
            return e;
        }
        return match tokio::process::Command::new("git")
            .args(["-C", dir, "branch", "-d", name])
            .output()
            .await
        {
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !out.status.success() {
                    // `-d` refuses unmerged branches — surface the error verbatim
                    // so the operator knows why and can decide whether to act.
                    return format!("git branch failed: {}", stderr.trim());
                }
                format!("Deleted branch '{}' in {}", name, dir)
            }
            Err(e) => format!("Failed to run git: {}", e),
        };
    }

    // Create form: `<path> <name>`
    if parts.len() >= 2 {
        let name = parts[1];
        if let Err(e) = validate_workspace_path(dir) {
            return e;
        }
        return match tokio::process::Command::new("git")
            .args(["-C", dir, "branch", name])
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
    git_branch_list(dir).await
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
        return "Usage: [TOOL:git_checkout <path> <branch>]".into();
    }
    let dir = match validate_workspace_path(parts[0]) {
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
        return "Usage: [TOOL:gh_issue_create <owner/repo> | <title> | <body>]".into();
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

/// Close a GitHub issue.
/// Param format: `<owner/repo> <number>`
pub async fn gh_issue_close(db: &WardsonDbClient, param: &str) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };

    let parts: Vec<&str> = param.split_whitespace().collect();
    if parts.len() < 2 {
        return "Usage: [TOOL:gh_issue_close <owner/repo> <number>]".into();
    }

    let repo = parts[0];
    let number = parts[1];

    let url = format!("https://api.github.com/repos/{}/issues/{}", repo, number);
    let payload = serde_json::json!({ "state": "closed" });

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
            if !resp.status().is_success() {
                return format!("GitHub API error: {}", resp.status());
            }
            format!("Issue #{} closed in {}", number, repo)
        }
        Err(e) => format!("Failed to close issue: {}", e),
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
        return "Usage: [TOOL:gh_pr_create <owner/repo> | <title> | <head> | <base>]".into();
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
        return "Usage: [TOOL:gh_project_list <owner>]".into();
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
        return "Usage: [TOOL:gh_project_view <owner> <number>]".into();
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
        return "Usage: [TOOL:file_read <path>[|<offset>[|<limit>]]]\n\
                Example: [TOOL:file_read /embra/workspace/repos/myrepo/README.md]\n\
                Example: [TOOL:file_read /path/to/big.log|2097152|1048576]".into();
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
        return "Usage: [TOOL:file_read <path>[|<offset>[|<limit>]]]".into();
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
            "\n[... {} more bytes at offset {}. Continue with [TOOL:file_read {}|{}|{}] ]",
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
        return "Usage: [TOOL:file_write <path> | <content>]\n\
                Use \\n for newlines, \\t for tabs.\n\
                Example: [TOOL:file_write /embra/workspace/repos/myrepo/notes.txt | line 1\\nline 2\\nline 3]"
            .into();
    }

    let parts: Vec<&str> = param.splitn(2, " | ").collect();
    if parts.len() < 2 {
        return "Usage: [TOOL:file_write <path> | <content>]".into();
    }

    let path = parts[0].trim();
    let content = expand_escapes(parts[1]);

    if let Err(e) = validate_workspace_path(path) {
        return e;
    }

    if let Err(e) = ensure_parent_dirs(path).await {
        return e;
    }

    match tokio::fs::write(path, &content).await {
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
        return "Usage: [TOOL:file_append <path> | <content>]\n\
                Use \\n for newlines, \\t for tabs.\n\
                Example: [TOOL:file_append /embra/workspace/repos/myrepo/log.txt | New entry\\n]"
            .into();
    }

    let parts: Vec<&str> = param.splitn(2, " | ").collect();
    if parts.len() < 2 {
        return "Usage: [TOOL:file_append <path> | <content>]".into();
    }

    let path = parts[0].trim();
    let content = expand_escapes(parts[1]);

    if let Err(e) = validate_workspace_path(path) {
        return e;
    }

    if let Err(e) = ensure_parent_dirs(path).await {
        return e;
    }

    use tokio::io::AsyncWriteExt;
    let result = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
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
        return "Usage: [TOOL:mkdir <path>]\nExample: [TOOL:mkdir /embra/workspace/repos/myrepo/src/utils]".into();
    }

    let path = param.trim();

    if let Err(e) = validate_workspace_path(path) {
        return e;
    }

    if std::path::Path::new(path).exists() {
        return format!("Directory already exists: {}", path);
    }

    match tokio::fs::create_dir_all(path).await {
        Ok(()) => format!("Created directory: {}", path),
        Err(e) => format!("Failed to create directory: {}", e),
    }
}

/// Delete a file. Workspace restricted.
/// Param format: `<path>`
pub async fn file_delete(param: &str) -> String {
    if param.is_empty() {
        return "Usage: [TOOL:file_delete <path>]\nExample: [TOOL:file_delete /embra/workspace/repos/myrepo/old_file.txt]".into();
    }

    let path = param.trim();

    if let Err(e) = validate_workspace_path(path) {
        return e;
    }

    let p = std::path::Path::new(path);
    if !p.exists() {
        return format!("File not found: {}", path);
    }

    if p.is_dir() {
        return format!("Cannot delete directory with file_delete (use a shell command for recursive removal): {}", path);
    }

    match tokio::fs::remove_file(path).await {
        Ok(()) => format!("Deleted: {}", path),
        Err(e) => format!("Failed to delete {}: {}", path, e),
    }
}

/// Move or rename a file or directory. Workspace restricted (both source and destination).
/// Param format: `<source> | <destination>`
pub async fn file_move(param: &str) -> String {
    let parts: Vec<&str> = param.splitn(2, '|').collect();
    if parts.len() < 2 {
        return "Usage: [TOOL:file_move <source> | <destination>]\nExample: [TOOL:file_move /embra/workspace/repos/myrepo/old.rs | /embra/workspace/repos/myrepo/new.rs]".into();
    }

    let src = parts[0].trim();
    let dst = parts[1].trim();

    if let Err(e) = validate_workspace_path(src) {
        return e;
    }
    if let Err(e) = validate_workspace_path(dst) {
        return e;
    }

    if !std::path::Path::new(src).exists() {
        return format!("Source not found: {}", src);
    }

    if std::path::Path::new(dst).exists() {
        return format!("Destination already exists: {}", dst);
    }

    // Ensure parent directory of destination exists
    if let Err(e) = ensure_parent_dirs(dst).await {
        return e;
    }

    match tokio::fs::rename(src, dst).await {
        Ok(()) => format!("Moved: {} → {}", src, dst),
        Err(e) => format!("Failed to move {} → {}: {}", src, dst, e),
    }
}

/// Stage a file removal with git rm. Workspace restricted.
/// Param format: `<path> <files>`
pub async fn git_rm(param: &str) -> String {
    let parts: Vec<&str> = param.splitn(2, char::is_whitespace).collect();
    if parts.len() < 2 {
        return "Usage: [TOOL:git_rm <repo_path> <files>]\nExample: [TOOL:git_rm /embra/workspace/repos/myrepo old_file.txt]".into();
    }

    let dir = match validate_workspace_path(parts[0]) {
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
        return "Usage: [TOOL:git_mv <repo_path> <source> <destination>]\nExample: [TOOL:git_mv /embra/workspace/repos/myrepo src/Old.rs src/old.rs]".into();
    }

    let dir = match validate_workspace_path(parts[0]) {
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
        return "Usage: [TOOL:dir_delete <path>] or [TOOL:dir_delete <path> --force]\nWithout --force, only empty directories are removed.".into();
    }

    let (path, force) = if param.trim_end().ends_with("--force") {
        (param.trim_end().trim_end_matches("--force").trim(), true)
    } else {
        (param.trim(), false)
    };

    if let Err(e) = validate_workspace_path(path) {
        return e;
    }

    let p = std::path::Path::new(path);
    if !p.exists() {
        return format!("Directory not found: {}", path);
    }

    if !p.is_dir() {
        return format!("Not a directory (use file_delete for files): {}", path);
    }

    if force {
        match tokio::fs::remove_dir_all(path).await {
            Ok(()) => format!("Deleted directory and all contents: {}", path),
            Err(e) => format!("Failed to delete directory {}: {}", path, e),
        }
    } else {
        match tokio::fs::remove_dir(path).await {
            Ok(()) => format!("Deleted empty directory: {}", path),
            Err(e) => {
                if e.kind() == std::io::ErrorKind::DirectoryNotEmpty
                    || e.to_string().contains("not empty")
                    || e.to_string().contains("Directory not empty")
                {
                    format!(
                        "Directory is not empty: {}. Use [TOOL:dir_delete {} --force] to remove with contents.",
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
