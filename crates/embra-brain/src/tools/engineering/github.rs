//! The GitHub API tools (`gh_*`): issues, pull requests, projects.

use embra_tool_macro::embra_tool;
use embra_tools_core::{DispatchError, USER_AGENT};
use schemars::JsonSchema;
use serde::Deserialize;

use super::git::resolve_github_token;
use crate::db::WardsonDbClient;
use crate::tools::registry::DispatchContext;

/// The reqwest client for GitHub API calls: the compiled-in roots and the
/// brain's redirect policy (`crate::net`: the same origin only, so the
/// bearer never travels to a third host).
fn github_client() -> reqwest::Client {
    crate::net::client()
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

    let client = github_client();
    match client
        .get(&url)
        .header("User-Agent", USER_AGENT)
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

/// Fetch the conversation-thread comments on an issue or PR. GitHub stores
/// both behind `/repos/<repo>/issues/<number>/comments` — the `/pulls/.../comments`
/// endpoint returns review comments on the diff, not the conversation tab.
///
/// Returns a list of `{ id, author, created_at, body }` objects. Silently
/// returns an empty Vec on HTTP error so a missing/gated comment thread does
/// not fail the parent view call — the absence is obvious in the rendered
/// response.
async fn fetch_issue_thread_comments(
    client: &reqwest::Client,
    token: &str,
    repo: &str,
    number: &str,
) -> Vec<serde_json::Value> {
    let url = format!(
        "https://api.github.com/repos/{}/issues/{}/comments?per_page=100",
        repo, number
    );
    let resp = client
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await;
    let raw: Vec<serde_json::Value> = match resp {
        Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
        _ => Vec::new(),
    };
    raw.into_iter()
        .map(|c| {
            serde_json::json!({
                "id": c.get("id").and_then(|v| v.as_u64()),
                "author": c.get("user").and_then(|u| u.get("login")).and_then(|l| l.as_str()),
                "created_at": c.get("created_at").and_then(|v| v.as_str()),
                "body": c.get("body").and_then(|v| v.as_str()),
            })
        })
        .collect()
}

/// View a single GitHub issue with its comment thread.
/// Param format: `<owner/repo> <number>`
pub async fn gh_issue_view(db: &WardsonDbClient, param: &str) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };

    let parts: Vec<&str> = param.split_whitespace().collect();
    if parts.len() < 2 {
        return "Usage: gh_issue_view <owner/repo> <number>".into();
    }
    let repo = parts[0];
    let number = parts[1];

    let client = github_client();

    let issue_url = format!("https://api.github.com/repos/{}/issues/{}", repo, number);
    let issue: serde_json::Value = match client
        .get(&issue_url)
        .header("User-Agent", USER_AGENT)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => match resp.json().await {
            Ok(v) => v,
            Err(e) => return format!("Failed to parse issue response: {}", e),
        },
        Ok(resp) => return format!("GitHub API error: {}", resp.status()),
        Err(e) => return format!("Failed to fetch issue: {}", e),
    };

    // GitHub returns PRs from the /issues/ endpoint too (they carry a
    // `pull_request` field). Redirect the caller to the PR tool so they get
    // head/base/merge state instead of a silently truncated view.
    if issue.get("pull_request").is_some() {
        return format!(
            "Item #{} in {} is a pull request, not an issue — use gh_pr_view for the full PR view.",
            number, repo
        );
    }

    let comments = fetch_issue_thread_comments(&client, &token, repo, number).await;

    let labels: Vec<&str> = issue
        .get("labels")
        .and_then(|arr| arr.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.get("name").and_then(|n| n.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let assignees: Vec<&str> = issue
        .get("assignees")
        .and_then(|arr| arr.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|u| u.get("login").and_then(|l| l.as_str()))
                .collect()
        })
        .unwrap_or_default();

    let result = serde_json::json!({
        "number": issue.get("number").and_then(|v| v.as_u64()),
        "title": issue.get("title").and_then(|v| v.as_str()),
        "state": issue.get("state").and_then(|v| v.as_str()),
        "author": issue.get("user").and_then(|u| u.get("login")).and_then(|l| l.as_str()),
        "created_at": issue.get("created_at").and_then(|v| v.as_str()),
        "updated_at": issue.get("updated_at").and_then(|v| v.as_str()),
        "closed_at": issue.get("closed_at").and_then(|v| v.as_str()),
        "labels": labels,
        "assignees": assignees,
        "body": issue.get("body").and_then(|v| v.as_str()),
        "comments": comments,
    });
    serde_json::to_string_pretty(&result).unwrap_or_else(|_| "Failed to format issue".into())
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

    let client = github_client();
    match client
        .get(&url)
        .header("User-Agent", USER_AGENT)
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

/// View a single GitHub pull request with its conversation-thread comments.
/// Param format: `<owner/repo> <number>`
pub async fn gh_pr_view(db: &WardsonDbClient, param: &str) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };

    let parts: Vec<&str> = param.split_whitespace().collect();
    if parts.len() < 2 {
        return "Usage: gh_pr_view <owner/repo> <number>".into();
    }
    let repo = parts[0];
    let number = parts[1];

    let client = github_client();

    let pr_url = format!("https://api.github.com/repos/{}/pulls/{}", repo, number);
    let pr: serde_json::Value = match client
        .get(&pr_url)
        .header("User-Agent", USER_AGENT)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => match resp.json().await {
            Ok(v) => v,
            Err(e) => return format!("Failed to parse PR response: {}", e),
        },
        Ok(resp) => return format!("GitHub API error: {}", resp.status()),
        Err(e) => return format!("Failed to fetch PR: {}", e),
    };

    let comments = fetch_issue_thread_comments(&client, &token, repo, number).await;

    let labels: Vec<&str> = pr
        .get("labels")
        .and_then(|arr| arr.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.get("name").and_then(|n| n.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let assignees: Vec<&str> = pr
        .get("assignees")
        .and_then(|arr| arr.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|u| u.get("login").and_then(|l| l.as_str()))
                .collect()
        })
        .unwrap_or_default();

    let result = serde_json::json!({
        "number": pr.get("number").and_then(|v| v.as_u64()),
        "title": pr.get("title").and_then(|v| v.as_str()),
        "state": pr.get("state").and_then(|v| v.as_str()),
        "author": pr.get("user").and_then(|u| u.get("login")).and_then(|l| l.as_str()),
        "created_at": pr.get("created_at").and_then(|v| v.as_str()),
        "updated_at": pr.get("updated_at").and_then(|v| v.as_str()),
        "closed_at": pr.get("closed_at").and_then(|v| v.as_str()),
        "merged_at": pr.get("merged_at").and_then(|v| v.as_str()),
        "labels": labels,
        "assignees": assignees,
        "head_ref": pr.get("head").and_then(|h| h.get("ref")).and_then(|r| r.as_str()),
        "base_ref": pr.get("base").and_then(|b| b.get("ref")).and_then(|r| r.as_str()),
        "merged": pr.get("merged").and_then(|v| v.as_bool()),
        "mergeable": pr.get("mergeable").and_then(|v| v.as_bool()),
        "draft": pr.get("draft").and_then(|v| v.as_bool()),
        "body": pr.get("body").and_then(|v| v.as_str()),
        "comments": comments,
    });
    serde_json::to_string_pretty(&result).unwrap_or_else(|_| "Failed to format PR".into())
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

    let client = github_client();
    match client
        .post(&url)
        .header("User-Agent", USER_AGENT)
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

    let client = github_client();
    match client
        .post(&url)
        .header("User-Agent", USER_AGENT)
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

    let client = github_client();
    match client
        .patch(&url)
        .header("User-Agent", USER_AGENT)
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

    let client = github_client();
    match client
        .post(&url)
        .header("User-Agent", USER_AGENT)
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

    let client = github_client();
    match client
        .put(&url)
        .header("User-Agent", USER_AGENT)
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
    let client = github_client();
    let endpoints = [
        format!("https://api.github.com/users/{}/projects", owner),
        format!("https://api.github.com/orgs/{}/projects", owner),
    ];
    let mut last_status: Option<reqwest::StatusCode> = None;
    for url in &endpoints {
        match client
            .get(url)
            .header("User-Agent", USER_AGENT)
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
    name = "gh_issue_view",
    description = "Fetch a single GitHub issue by number. Returns title, body, author, state, labels, assignees, and the full conversation-thread comments as JSON. Use this before acting on an issue so the body and prior discussion are in context — the list view only carries the title."
)]
pub struct GhIssueViewArgs {
    /// owner/repo, e.g. `Ward-Software-Defined-Systems/embraOS`.
    pub repo: String,
    pub number: u32,
}

impl GhIssueViewArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{} {}", self.repo, self.number);
        Ok(gh_issue_view(ctx.db, &param).await)
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
    name = "gh_pr_view",
    description = "Fetch a single GitHub pull request by number. Returns title, body, author, state, head/base refs, merge status (merged, mergeable, draft), labels, assignees, and the conversation-thread comments as JSON. Symmetric with gh_issue_view — same core fields plus PR-specific merge metadata."
)]
pub struct GhPrViewArgs {
    /// owner/repo, e.g. `Ward-Software-Defined-Systems/embraOS`.
    pub repo: String,
    pub number: u32,
}

impl GhPrViewArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let param = format!("{} {}", self.repo, self.number);
        Ok(gh_pr_view(ctx.db, &param).await)
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

#[cfg(test)]
mod native_args_tests {
    use super::*;

    #[test]
    fn gh_issue_view_args_require_repo_and_number() {
        let a: GhIssueViewArgs = serde_json::from_value(serde_json::json!({
            "repo": "Ward-Software-Defined-Systems/Embra_Debug",
            "number": 52
        }))
        .unwrap();
        assert_eq!(a.repo, "Ward-Software-Defined-Systems/Embra_Debug");
        assert_eq!(a.number, 52);

        // Missing number → error; missing repo → error.
        assert!(
            serde_json::from_value::<GhIssueViewArgs>(serde_json::json!({"repo": "x/y"})).is_err()
        );
        assert!(
            serde_json::from_value::<GhIssueViewArgs>(serde_json::json!({"number": 1})).is_err()
        );
    }

    #[test]
    fn gh_pr_view_args_require_repo_and_number() {
        let a: GhPrViewArgs = serde_json::from_value(serde_json::json!({
            "repo": "Ward-Software-Defined-Systems/embraOS",
            "number": 1
        }))
        .unwrap();
        assert_eq!(a.repo, "Ward-Software-Defined-Systems/embraOS");
        assert_eq!(a.number, 1);

        assert!(
            serde_json::from_value::<GhPrViewArgs>(serde_json::json!({"repo": "x/y"})).is_err()
        );
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
}
