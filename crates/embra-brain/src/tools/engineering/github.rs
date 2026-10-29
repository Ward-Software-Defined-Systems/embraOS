//! The GitHub API tools (`gh_*`): issues, pull requests, projects.

use embra_tool_macro::embra_tool;
use embra_tools_core::{DispatchError, USER_AGENT};
use schemars::JsonSchema;
use serde::Deserialize;

use super::git::resolve_github_token;
use crate::db::WardsonDbClient;
use crate::tools::registry::DispatchContext;

/// A request to the GitHub API (#37): the brain's shared client (the
/// compiled-in roots and the redirect policy of `crate::net`: the same
/// origin only, so the bearer never travels to a third host; one pool per
/// process, #36), the user agent and the token. Every `gh_*` request is
/// built here and nowhere else.
fn github_request(method: reqwest::Method, token: &str, url: &str) -> reqwest::RequestBuilder {
    crate::net::shared()
        .request(method, url)
        .header("User-Agent", USER_AGENT)
        .header("Authorization", format!("Bearer {token}"))
}

/// GET a GitHub API document. A transport failure is `Failed to fetch
/// <what>: …`, a non-2xx `GitHub API error: <status>`, an undecodable
/// body `Failed to parse GitHub response: …` — the three shapes the simple
/// read tools answered with, in one place.
async fn github_get(token: &str, url: &str, what: &str) -> Result<serde_json::Value, String> {
    let resp = github_request(reqwest::Method::GET, token, url)
        .send()
        .await
        .map_err(|e| format!("Failed to fetch {what}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GitHub API error: {}", resp.status()));
    }
    resp.json().await.map_err(|e| format!("Failed to parse GitHub response: {e}"))
}

/// The label names and the assignee logins of an issue or a pull request.
fn labels_and_assignees(item: &serde_json::Value) -> (Vec<&str>, Vec<&str>) {
    let names = |key: &str, field: &str| -> Vec<&str> {
        item.get(key)
            .and_then(|arr| arr.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.get(field).and_then(|n| n.as_str())).collect())
            .unwrap_or_default()
    };
    (names("labels", "name"), names("assignees", "login"))
}

/// Fetch GitHub issues via API.
pub async fn gh_issues(db: &WardsonDbClient, repo: &str) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };

    let url = format!("https://api.github.com/repos/{}/issues?state=open&per_page=10", repo);

    let issues = match github_get(&token, &url, "issues").await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let issues: Vec<serde_json::Value> = match serde_json::from_value(issues) {
        Ok(v) => v,
        Err(e) => return format!("Failed to parse GitHub response: {}", e),
    };
    if issues.is_empty() {
        return format!("No open issues for {}.", repo);
    }
    let mut output = format!("=== Open Issues: {} ({}) ===\n", repo, issues.len());
    for issue in &issues {
        let number = issue.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
        let title = issue.get("title").and_then(|v| v.as_str()).unwrap_or("?");
        let (labels, _) = labels_and_assignees(issue);
        let label_str = if labels.is_empty() {
            String::new()
        } else {
            format!(" [{}]", labels.join(", "))
        };
        output.push_str(&format!("  #{} {}{}\n", number, title, label_str));
    }
    output
}

/// Fetch the conversation-thread comments on an issue or PR. GitHub stores
/// both behind `/repos/<repo>/issues/<number>/comments` — the `/pulls/.../comments`
/// endpoint returns review comments on the diff, not the conversation tab.
///
/// Returns a list of `{ id, author, created_at, body }` objects. Silently
/// returns an empty Vec on HTTP error so a missing/gated comment thread does
/// not fail the parent view call — the absence is obvious in the rendered
/// response.
async fn fetch_issue_thread_comments(token: &str, repo: &str, number: u32) -> Vec<serde_json::Value> {
    let url = format!(
        "https://api.github.com/repos/{}/issues/{}/comments?per_page=100",
        repo, number
    );
    let resp = github_request(reqwest::Method::GET, token, &url).send().await;
    // Best-effort enrichment: the issue itself was read; a thread that
    // cannot be fetched or decoded is shown as no comments.
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
pub async fn gh_issue_view(db: &WardsonDbClient, repo: &str, number: u32) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };

    let repo = repo.trim();

    let issue_url = format!("https://api.github.com/repos/{}/issues/{}", repo, number);
    let issue = match github_get(&token, &issue_url, "issue").await {
        Ok(v) => v,
        Err(e) => return e,
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

    let comments = fetch_issue_thread_comments(&token, repo, number).await;
    let (labels, assignees) = labels_and_assignees(&issue);

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
pub async fn gh_prs(db: &WardsonDbClient, repo: &str) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };

    let url = format!("https://api.github.com/repos/{}/pulls?state=open&per_page=10", repo);

    let prs = match github_get(&token, &url, "PRs").await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let prs: Vec<serde_json::Value> = match serde_json::from_value(prs) {
        Ok(v) => v,
        Err(e) => return format!("Failed to parse GitHub response: {}", e),
    };
    if prs.is_empty() {
        return format!("No open PRs for {}.", repo);
    }
    let mut output = format!("=== Open PRs: {} ({}) ===\n", repo, prs.len());
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

/// View a single GitHub pull request with its conversation-thread comments.
pub async fn gh_pr_view(db: &WardsonDbClient, repo: &str, number: u32) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };

    let repo = repo.trim();

    let pr_url = format!("https://api.github.com/repos/{}/pulls/{}", repo, number);
    let pr = match github_get(&token, &pr_url, "PR").await {
        Ok(v) => v,
        Err(e) => return e,
    };

    let comments = fetch_issue_thread_comments(&token, repo, number).await;
    let (labels, assignees) = labels_and_assignees(&pr);

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

/// Create a GitHub issue. A body not given is sent empty, as before.
pub async fn gh_issue_create(db: &WardsonDbClient, repo: &str, title: &str, body: Option<&str>) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };

    let repo = repo.trim();
    let title = title.trim();
    let body = body.map(str::trim).unwrap_or("");

    let url = format!("https://api.github.com/repos/{}/issues", repo);
    let payload = serde_json::json!({
        "title": title,
        "body": body,
    });

    match github_request(reqwest::Method::POST, &token, &url)
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
            let issue: serde_json::Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => return format!("Failed to parse GitHub response: {}", e),
            };
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
    number: u32,
    body: &str,
) -> Result<(u64, String), String> {
    let url = format!(
        "https://api.github.com/repos/{}/issues/{}/comments",
        repo, number
    );
    let payload = serde_json::json!({ "body": body });

    match github_request(reqwest::Method::POST, token, &url)
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
            let v: serde_json::Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => return Err(format!("Failed to parse GitHub response: {}", e)),
            };
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

/// Post a comment on a GitHub issue.
pub async fn gh_issue_comment(db: &WardsonDbClient, repo: &str, number: u32, body: &str) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };
    let (repo, body) = (repo.trim(), body.trim());
    match post_github_comment(&token, repo, number, body).await {
        Ok((_id, html_url)) => format!("Comment posted on issue #{} in {}: {}", number, repo, html_url),
        Err(e) => format!("gh_issue_comment failed: {}", e),
    }
}

/// Post a comment on a GitHub pull request. Hits the same underlying endpoint
/// as `gh_issue_comment` — GitHub treats PR comments (the conversation tab) as
/// issue comments. Separate tool for semantic clarity in the catalog.
pub async fn gh_pr_comment(db: &WardsonDbClient, repo: &str, number: u32, body: &str) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };
    let (repo, body) = (repo.trim(), body.trim());
    match post_github_comment(&token, repo, number, body).await {
        Ok((_id, html_url)) => format!("Comment posted on PR #{} in {}: {}", number, repo, html_url),
        Err(e) => format!("gh_pr_comment failed: {}", e),
    }
}

/// Close a GitHub issue.
pub async fn gh_issue_close(db: &WardsonDbClient, repo: &str, number: u32) -> String {
    let change = StateChange { kind: "issues", new_state: "closed", tool_name: "gh_issue_close", artifact_label: "Issue", verb: "closed" };
    patch_issue_or_pr_state(db, repo, number, change).await
}

/// Reopen a previously-closed GitHub issue.
pub async fn gh_issue_reopen(db: &WardsonDbClient, repo: &str, number: u32) -> String {
    let change = StateChange { kind: "issues", new_state: "open", tool_name: "gh_issue_reopen", artifact_label: "Issue", verb: "reopened" };
    patch_issue_or_pr_state(db, repo, number, change).await
}

/// Close a GitHub pull request without merging.
pub async fn gh_pr_close(db: &WardsonDbClient, repo: &str, number: u32) -> String {
    let change = StateChange { kind: "pulls", new_state: "closed", tool_name: "gh_pr_close", artifact_label: "PR", verb: "closed" };
    patch_issue_or_pr_state(db, repo, number, change).await
}

/// One issue/PR state PATCH: `kind` is "issues" or "pulls" (URL path
/// segment), `new_state` is "open" or "closed", and the tool name, the
/// artifact label and the verb drive the error/success strings.
struct StateChange {
    kind: &'static str,
    new_state: &'static str,
    tool_name: &'static str,
    artifact_label: &'static str,
    verb: &'static str,
}

/// Shared implementation for issue/PR state PATCH operations.
async fn patch_issue_or_pr_state(db: &WardsonDbClient, repo: &str, number: u32, change: StateChange) -> String {
    let StateChange { kind, new_state, tool_name, artifact_label, verb } = change;
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };

    let repo = repo.trim();

    let url = format!("https://api.github.com/repos/{}/{}/{}", repo, kind, number);
    let payload = serde_json::json!({ "state": new_state });

    match github_request(reqwest::Method::PATCH, &token, &url)
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
            let v: serde_json::Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => return format!("Failed to parse GitHub response: {}", e),
            };
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
pub async fn gh_pr_create(db: &WardsonDbClient, repo: &str, title: &str, head: &str, base: &str) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };

    let repo = repo.trim();
    let title = title.trim();
    let head = head.trim();
    let base = base.trim();

    let url = format!("https://api.github.com/repos/{}/pulls", repo);
    let payload = serde_json::json!({
        "title": title,
        "head": head,
        "base": base,
    });

    match github_request(reqwest::Method::POST, &token, &url)
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
            let pr: serde_json::Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => return format!("Failed to parse GitHub response: {}", e),
            };
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
/// `method` is merge / squash / rebase, checked here as it always was.
pub async fn gh_pr_merge(db: &WardsonDbClient, repo: &str, number: u32, method: &str) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };

    let repo = repo.trim();
    let method = method.trim();
    if !matches!(method, "merge" | "squash" | "rebase") {
        return format!("gh_pr_merge rejected (invalid method '{}'; use merge, squash, or rebase)", method);
    }

    let url = format!(
        "https://api.github.com/repos/{}/pulls/{}/merge",
        repo, number
    );
    let payload = serde_json::json!({ "merge_method": method });

    match github_request(reqwest::Method::PUT, &token, &url)
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            match status.as_u16() {
                200 => {
                    let v: serde_json::Value = match resp.json().await {
                        Ok(v) => v,
                        Err(e) => return format!("Failed to parse GitHub response: {}", e),
                    };
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
    let endpoints = [
        format!("https://api.github.com/users/{}/projects", owner),
        format!("https://api.github.com/orgs/{}/projects", owner),
    ];
    let mut last_status: Option<reqwest::StatusCode> = None;
    for url in &endpoints {
        match github_request(reqwest::Method::GET, token, url)
            .header("Accept", "application/vnd.github.inertia-preview+json")
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return match resp.json().await {
                        Ok(body) => ProjectsFetch::Ok(body),
                        Err(e) => ProjectsFetch::Err(format!("Failed to parse GitHub response: {}", e)),
                    };
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
pub async fn gh_project_list(db: &WardsonDbClient, owner: &str) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };

    let owner = owner.trim();
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
pub async fn gh_project_view(db: &WardsonDbClient, owner: &str, number: u32) -> String {
    let token = match resolve_github_token(db).await {
        Some(t) => t,
        None => return "GITHUB_TOKEN not set. Use /github-token <token> or set GITHUB_TOKEN env var.".into(),
    };

    let owner = owner.trim();
    let target_num = u64::from(number);

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
        Ok(gh_issue_view(ctx.db, &self.repo, self.number).await)
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
        Ok(gh_pr_view(ctx.db, &self.repo, self.number).await)
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
        Ok(gh_issue_create(ctx.db, &self.repo, &self.title, self.body.as_deref()).await)
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
        Ok(gh_issue_close(ctx.db, &self.repo, self.number).await)
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
        Ok(gh_issue_reopen(ctx.db, &self.repo, self.number).await)
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
        Ok(gh_issue_comment(ctx.db, &self.repo, self.number, &self.body).await)
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
        Ok(gh_pr_create(ctx.db, &self.repo, &self.title, &self.head, &self.base).await)
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
        Ok(gh_pr_close(ctx.db, &self.repo, self.number).await)
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
        Ok(gh_pr_comment(ctx.db, &self.repo, self.number, &self.body).await)
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

impl PrMergeMethod {
    /// The `merge_method` GitHub takes.
    fn as_str(&self) -> &'static str {
        match self {
            PrMergeMethod::Merge => "merge",
            PrMergeMethod::Squash => "squash",
            PrMergeMethod::Rebase => "rebase",
        }
    }
}

impl GhPrMergeArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(gh_pr_merge(ctx.db, &self.repo, self.number, self.method.as_str()).await)
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
        Ok(gh_project_view(ctx.db, &self.owner, self.number).await)
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

#[cfg(test)]
mod request_builder_tests {
    /// #37: every GitHub request is built by `github_request` — the one
    /// place a header, the client or the error shape changes.
    #[test]
    fn every_github_request_is_built_by_the_one_builder() {
        let src = include_str!("github.rs");
        let builder = src.find("fn github_request(").expect("the builder");
        let body_end = builder + src[builder..].find("\n}\n").expect("its end");
        let outside = format!("{}{}", &src[..builder], &src[body_end..]);
        assert!(!outside.contains("\"User-Agent\""), "a user agent set outside the builder");
        assert!(!outside.contains("\"Authorization\""), "a token set outside the builder");
        let (bare, fresh) = (["net::", "client()"].concat(), ["Client::", "new()"].concat());
        assert!(!outside.contains(&bare) && !outside.contains(&fresh), "a client built outside the builder");
    }
}
