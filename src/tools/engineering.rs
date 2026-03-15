use chrono::Utc;

use crate::db::WardsonDbClient;

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
pub async fn gh_issues(param: &str) -> String {
    let token = match std::env::var("GITHUB_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => return "GITHUB_TOKEN environment variable not set. Required for GitHub API access.".into(),
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
pub async fn gh_prs(param: &str) -> String {
    let token = match std::env::var("GITHUB_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => return "GITHUB_TOKEN environment variable not set. Required for GitHub API access.".into(),
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

async fn ensure_collection(db: &WardsonDbClient, name: &str) {
    if !db.collection_exists(name).await.unwrap_or(true) {
        let _ = db.create_collection(name).await;
    }
}
