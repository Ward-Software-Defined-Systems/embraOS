//! The GitLab API tools (`gl_*`) against a self-hosted instance: the
//! client with the operator's CA drop-ins, the host chooser, the shared
//! `gl_get`/`gl_send` plumbing and the tools.

use embra_tool_macro::embra_tool;
use embra_tools_core::{DispatchError, USER_AGENT};
use schemars::JsonSchema;
use serde::Deserialize;

use super::git::{normalize_git_host, resolve_git_tokens};
use super::load_operator_ca_certs;
use crate::db::WardsonDbClient;
use crate::tools::registry::DispatchContext;

/// Build the reqwest client for GitLab API calls: stock webpki roots plus
/// the operator CA drop-ins, so a self-hosted instance behind a private CA
/// (e.g. mkcert) verifies. Falls back to a stock client on builder failure.
///
/// Only this client adds them. The other reqwest clients in this file keep
/// the compiled-in roots, deliberately.
fn gitlab_client() -> reqwest::Client {
    let mut builder = crate::net::builder();
    for cert in load_operator_ca_certs() {
        builder = builder.add_root_certificate(cert);
    }
    builder.build().unwrap_or_else(|e| {
        tracing::warn!("GitLab client builder failed ({}); using the stock client", e);
        crate::net::client()
    })
}

/// Percent-encode a GitLab project path (`namespace/project`) for use as a
/// single URL path segment — GitLab's /api/v4/projects/ takes the full path
/// with `/` encoded as `%2F`. Unreserved chars (RFC 3986) pass through.
fn encode_gitlab_project(path: &str) -> String {
    let mut out = String::with_capacity(path.len() * 3 / 2);
    for b in path.trim().trim_matches('/').bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Resolve the PAT for a GitLab host from `git_tokens`, with a uniform
/// error message when unconfigured. Host is normalized first so
/// `https://gitlab.example.com/` and `gitlab.example.com` both resolve.
///
/// `host` is optional: when omitted, a single configured `git_tokens` host is
/// used as the default (the common deployment — one internal instance), and
/// ambiguity is an error that NAMES the candidates rather than guessing.
/// github.com is excluded from that default; it rides `github_token` and the
/// `gh_*` tools, and `/git-token` refuses it outright.
/// Pick the GitLab host for a `gl_*` call. Pure so the ambiguity rules are
/// testable without a database: an explicit host always wins (normalized);
/// otherwise a single configured non-github host is the default, and
/// anything else is an error that NAMES the candidates rather than guessing.
/// github.com is never a candidate — it rides `github_token` and the `gh_*`
/// tools, and `/git-token` refuses it outright.
fn choose_gitlab_host(
    explicit: Option<&str>,
    configured: &std::collections::BTreeMap<String, String>,
) -> Result<String, String> {
    if let Some(h) = explicit.map(str::trim).filter(|h| !h.is_empty()) {
        return normalize_git_host(h);
    }
    let candidates: Vec<&str> = configured
        .keys()
        .map(|h| h.as_str())
        .filter(|h| *h != "github.com")
        .collect();
    match candidates.len() {
        1 => Ok(candidates[0].to_string()),
        0 => Err("No GitLab host configured. Use /git-token <host> \
<personal-access-token> (scope: api), or pass host explicitly."
            .to_string()),
        _ => Err(format!(
            "Several git hosts are configured ({}). Pass host explicitly.",
            candidates.join(", ")
        )),
    }
}

async fn gitlab_host_token(
    db: &WardsonDbClient,
    host: Option<&str>,
) -> Result<(String, String), String> {
    let mut tokens = resolve_git_tokens(db).await;
    let host = choose_gitlab_host(host, &tokens)?;
    match tokens.remove(&host) {
        Some(token) => Ok((host, token)),
        None => Err(format!(
            "No token configured for {}. Use /git-token {} <personal-access-token> (scope: api).",
            host, host
        )),
    }
}

/// GET a GitLab API path and decode JSON. `path` is everything after
/// `/api/v4/`. Shared by the read-side `gl_*` tools so the client, headers,
/// and error shape stay in one place.
async fn gl_get(host: &str, token: &str, path: &str) -> Result<serde_json::Value, String> {
    let url = format!("https://{}/api/v4/{}", host, path);
    let resp = gitlab_client()
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .header("PRIVATE-TOKEN", token)
        .send()
        .await
        .map_err(|e| format!("GitLab request failed: {}", e))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("GitLab API error {}: {}", status, body));
    }
    resp.json().await.map_err(|e| format!("GitLab response decode failed: {}", e))
}

/// POST/PUT a JSON body to a GitLab API path and decode the result.
async fn gl_send(
    method: reqwest::Method,
    host: &str,
    token: &str,
    path: &str,
    payload: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let url = format!("https://{}/api/v4/{}", host, path);
    let resp = gitlab_client()
        .request(method, &url)
        .header("User-Agent", USER_AGENT)
        .header("PRIVATE-TOKEN", token)
        .json(payload)
        .send()
        .await
        .map_err(|e| format!("GitLab request failed: {}", e))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("GitLab API error {}: {}", status, body));
    }
    resp.json().await.map_err(|e| format!("GitLab response decode failed: {}", e))
}

/// Render GitLab discussion notes (`/notes`), skipping system notes — the
/// "changed the description" churn GitLab records alongside real comments.
fn render_gl_notes(notes: &serde_json::Value) -> String {
    let Some(arr) = notes.as_array() else { return String::new() };
    let human: Vec<&serde_json::Value> = arr
        .iter()
        .filter(|n| !n.get("system").and_then(|v| v.as_bool()).unwrap_or(false))
        .collect();
    if human.is_empty() {
        return "\n(no comments)\n".to_string();
    }
    let mut out = format!("\n--- Comments ({}) ---\n", human.len());
    for n in human {
        let author = n
            .get("author")
            .and_then(|a| a.get("username"))
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let created = n.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        let body = n.get("body").and_then(|v| v.as_str()).unwrap_or("");
        out.push_str(&format!("\n[{} at {}]\n{}\n", author, created, body));
    }
    out
}

/// List open GitLab issues for a project on a configured host.
pub async fn gl_issues(db: &WardsonDbClient, host: Option<&str>, project: &str) -> String {
    let (host, token) = match gitlab_host_token(db, host).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    let url = format!(
        "https://{}/api/v4/projects/{}/issues?state=opened&per_page=10",
        host,
        encode_gitlab_project(project)
    );
    match gitlab_client()
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .header("PRIVATE-TOKEN", &token)
        .send()
        .await
    {
        Ok(resp) => {
            if !resp.status().is_success() {
                return format!("GitLab API error: {}", resp.status());
            }
            let issues: Vec<serde_json::Value> = resp.json().await.unwrap_or_default();
            if issues.is_empty() {
                return format!("No open issues for {} on {}.", project, host);
            }
            let mut output = format!("=== Open Issues: {} ({}) ===\n", project, issues.len());
            for issue in &issues {
                let iid = issue.get("iid").and_then(|v| v.as_u64()).unwrap_or(0);
                let title = issue.get("title").and_then(|v| v.as_str()).unwrap_or("?");
                // GitLab labels are plain strings, not {name} objects.
                let labels: Vec<&str> = issue
                    .get("labels")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|l| l.as_str()).collect())
                    .unwrap_or_default();
                let label_str = if labels.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", labels.join(", "))
                };
                output.push_str(&format!("  #{} {}{}\n", iid, title, label_str));
            }
            output
        }
        Err(e) => format!("Failed to fetch issues: {}", e),
    }
}

/// List open GitLab merge requests for a project on a configured host.
pub async fn gl_mrs(db: &WardsonDbClient, host: Option<&str>, project: &str) -> String {
    let (host, token) = match gitlab_host_token(db, host).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    let url = format!(
        "https://{}/api/v4/projects/{}/merge_requests?state=opened&per_page=10",
        host,
        encode_gitlab_project(project)
    );
    match gitlab_client()
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .header("PRIVATE-TOKEN", &token)
        .send()
        .await
    {
        Ok(resp) => {
            if !resp.status().is_success() {
                return format!("GitLab API error: {}", resp.status());
            }
            let mrs: Vec<serde_json::Value> = resp.json().await.unwrap_or_default();
            if mrs.is_empty() {
                return format!("No open merge requests for {} on {}.", project, host);
            }
            let mut output = format!("=== Open MRs: {} ({}) ===\n", project, mrs.len());
            for mr in &mrs {
                let iid = mr.get("iid").and_then(|v| v.as_u64()).unwrap_or(0);
                let title = mr.get("title").and_then(|v| v.as_str()).unwrap_or("?");
                let author = mr
                    .get("author")
                    .and_then(|u| u.get("username"))
                    .and_then(|l| l.as_str())
                    .unwrap_or("?");
                let branches = format!(
                    "{} → {}",
                    mr.get("source_branch").and_then(|v| v.as_str()).unwrap_or("?"),
                    mr.get("target_branch").and_then(|v| v.as_str()).unwrap_or("?")
                );
                output.push_str(&format!("  !{} {} (by {}, {})\n", iid, title, author, branches));
            }
            output
        }
        Err(e) => format!("Failed to fetch merge requests: {}", e),
    }
}

/// Create a GitLab issue.
pub async fn gl_issue_create(
    db: &WardsonDbClient,
    host: Option<&str>,
    project: &str,
    title: &str,
    description: &str,
) -> String {
    let (host, token) = match gitlab_host_token(db, host).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    if title.trim().is_empty() {
        return "Issue title must not be empty.".into();
    }
    let url = format!(
        "https://{}/api/v4/projects/{}/issues",
        host,
        encode_gitlab_project(project)
    );
    let payload = serde_json::json!({
        "title": title.trim(),
        "description": description,
    });
    match gitlab_client()
        .post(&url)
        .header("User-Agent", USER_AGENT)
        .header("PRIVATE-TOKEN", &token)
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) => {
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return format!("GitLab API error {}: {}", status, body);
            }
            let issue: serde_json::Value = resp.json().await.unwrap_or_default();
            let iid = issue.get("iid").and_then(|v| v.as_u64()).unwrap_or(0);
            let web_url = issue.get("web_url").and_then(|v| v.as_str()).unwrap_or("");
            format!("Issue #{} created: {}\n{}", iid, title.trim(), web_url)
        }
        Err(e) => format!("Failed to create issue: {}", e),
    }
}

/// Create a GitLab merge request.
pub async fn gl_mr_create(
    db: &WardsonDbClient,
    host: Option<&str>,
    project: &str,
    title: &str,
    source_branch: &str,
    target_branch: &str,
) -> String {
    let (host, token) = match gitlab_host_token(db, host).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    if title.trim().is_empty() || source_branch.trim().is_empty() || target_branch.trim().is_empty()
    {
        return "Merge request title, source_branch, and target_branch must not be empty.".into();
    }
    let url = format!(
        "https://{}/api/v4/projects/{}/merge_requests",
        host,
        encode_gitlab_project(project)
    );
    let payload = serde_json::json!({
        "title": title.trim(),
        "source_branch": source_branch.trim(),
        "target_branch": target_branch.trim(),
    });
    match gitlab_client()
        .post(&url)
        .header("User-Agent", USER_AGENT)
        .header("PRIVATE-TOKEN", &token)
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) => {
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return format!("GitLab API error {}: {}", status, body);
            }
            let mr: serde_json::Value = resp.json().await.unwrap_or_default();
            let iid = mr.get("iid").and_then(|v| v.as_u64()).unwrap_or(0);
            let web_url = mr.get("web_url").and_then(|v| v.as_str()).unwrap_or("");
            format!(
                "MR !{} created: {} ({} → {})\n{}",
                iid,
                title.trim(),
                source_branch.trim(),
                target_branch.trim(),
                web_url
            )
        }
        Err(e) => format!("Failed to create merge request: {}", e),
    }
}

/// View one GitLab issue with its discussion thread.
pub async fn gl_issue_view(
    db: &WardsonDbClient,
    host: Option<&str>,
    project: &str,
    iid: u64,
) -> String {
    let (host, token) = match gitlab_host_token(db, host).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    let proj = encode_gitlab_project(project);
    let issue = match gl_get(&host, &token, &format!("projects/{}/issues/{}", proj, iid)).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let notes = gl_get(&host, &token, &format!("projects/{}/issues/{}/notes?per_page=100", proj, iid))
        .await
        .unwrap_or_else(|_| serde_json::Value::Array(vec![]));
    let labels: Vec<&str> = issue
        .get("labels")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|l| l.as_str()).collect())
        .unwrap_or_default();
    format!(
        "=== Issue #{}: {} ===\nState: {}\nAuthor: {}\nLabels: {}\nCreated: {}\nURL: {}\n\n{}\n{}",
        iid,
        issue.get("title").and_then(|v| v.as_str()).unwrap_or("?"),
        issue.get("state").and_then(|v| v.as_str()).unwrap_or("?"),
        issue
            .get("author")
            .and_then(|a| a.get("username"))
            .and_then(|v| v.as_str())
            .unwrap_or("?"),
        if labels.is_empty() { "none".to_string() } else { labels.join(", ") },
        issue.get("created_at").and_then(|v| v.as_str()).unwrap_or("?"),
        issue.get("web_url").and_then(|v| v.as_str()).unwrap_or(""),
        issue.get("description").and_then(|v| v.as_str()).unwrap_or("(no description)"),
        render_gl_notes(&notes)
    )
}

/// View one GitLab merge request with its discussion thread.
pub async fn gl_mr_view(
    db: &WardsonDbClient,
    host: Option<&str>,
    project: &str,
    iid: u64,
) -> String {
    let (host, token) = match gitlab_host_token(db, host).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    let proj = encode_gitlab_project(project);
    let mr = match gl_get(&host, &token, &format!("projects/{}/merge_requests/{}", proj, iid)).await
    {
        Ok(v) => v,
        Err(e) => return e,
    };
    let notes = gl_get(
        &host,
        &token,
        &format!("projects/{}/merge_requests/{}/notes?per_page=100", proj, iid),
    )
    .await
    .unwrap_or_else(|_| serde_json::Value::Array(vec![]));
    let s = |k: &str| mr.get(k).and_then(|v| v.as_str()).unwrap_or("?").to_string();
    format!(
        "=== MR !{}: {} ===\nState: {} (merge_status: {}, draft: {})\nAuthor: {}\nBranches: {} \u{2192} {}\nHead: {}\nCreated: {}\nURL: {}\n\n{}\n{}",
        iid,
        s("title"),
        s("state"),
        s("detailed_merge_status"),
        mr.get("draft").and_then(|v| v.as_bool()).unwrap_or(false),
        mr.get("author")
            .and_then(|a| a.get("username"))
            .and_then(|v| v.as_str())
            .unwrap_or("?"),
        s("source_branch"),
        s("target_branch"),
        // The head of the source branch: what `gl_mr_merge` takes as `sha`.
        s("sha"),
        s("created_at"),
        mr.get("web_url").and_then(|v| v.as_str()).unwrap_or(""),
        mr.get("description").and_then(|v| v.as_str()).unwrap_or("(no description)"),
        render_gl_notes(&notes)
    )
}

/// Close or reopen a GitLab issue or merge request via `state_event`.
/// `kind` is the API segment: "issues" or "merge_requests".
async fn gl_state_change(
    db: &WardsonDbClient,
    host: Option<&str>,
    project: &str,
    kind: &str,
    iid: u64,
    state_event: &str,
) -> String {
    let (host, token) = match gitlab_host_token(db, host).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    let path = format!("projects/{}/{}/{}", encode_gitlab_project(project), kind, iid);
    let payload = serde_json::json!({ "state_event": state_event });
    match gl_send(reqwest::Method::PUT, &host, &token, &path, &payload).await {
        Ok(v) => {
            let marker = if kind == "issues" { "#" } else { "!" };
            format!(
                "{}{} is now {}: {}\n{}",
                marker,
                iid,
                v.get("state").and_then(|x| x.as_str()).unwrap_or("?"),
                v.get("title").and_then(|x| x.as_str()).unwrap_or("?"),
                v.get("web_url").and_then(|x| x.as_str()).unwrap_or("")
            )
        }
        Err(e) => e,
    }
}

/// Close a GitLab issue.
pub async fn gl_issue_close(db: &WardsonDbClient, host: Option<&str>, project: &str, iid: u64) -> String {
    gl_state_change(db, host, project, "issues", iid, "close").await
}

/// Reopen a GitLab issue.
pub async fn gl_issue_reopen(db: &WardsonDbClient, host: Option<&str>, project: &str, iid: u64) -> String {
    gl_state_change(db, host, project, "issues", iid, "reopen").await
}

/// Close a GitLab merge request without merging it.
pub async fn gl_mr_close(db: &WardsonDbClient, host: Option<&str>, project: &str, iid: u64) -> String {
    gl_state_change(db, host, project, "merge_requests", iid, "close").await
}

/// Post a comment (GitLab "note") on an issue or merge request.
async fn gl_note_create(
    db: &WardsonDbClient,
    host: Option<&str>,
    project: &str,
    kind: &str,
    iid: u64,
    body: &str,
) -> String {
    if body.trim().is_empty() {
        return "Comment body must not be empty.".into();
    }
    let (host, token) = match gitlab_host_token(db, host).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    let path = format!("projects/{}/{}/{}/notes", encode_gitlab_project(project), kind, iid);
    let payload = serde_json::json!({ "body": body });
    match gl_send(reqwest::Method::POST, &host, &token, &path, &payload).await {
        Ok(v) => format!(
            "Comment posted on {}{} (note {}).",
            if kind == "issues" { "#" } else { "!" },
            iid,
            v.get("id").and_then(|x| x.as_u64()).unwrap_or(0)
        ),
        Err(e) => e,
    }
}

/// Comment on a GitLab issue.
pub async fn gl_issue_comment(
    db: &WardsonDbClient,
    host: Option<&str>,
    project: &str,
    iid: u64,
    body: &str,
) -> String {
    gl_note_create(db, host, project, "issues", iid, body).await
}

/// Comment on a GitLab merge request.
pub async fn gl_mr_comment(
    db: &WardsonDbClient,
    host: Option<&str>,
    project: &str,
    iid: u64,
    body: &str,
) -> String {
    gl_note_create(db, host, project, "merge_requests", iid, body).await
}

/// How `gl_mr_merge` is asked to merge.
pub struct GlMergeOptions<'a> {
    pub squash: bool,
    pub when_pipeline_succeeds: bool,
    pub remove_source_branch: bool,
    /// The commit the caller reviewed. `None` merges the branch as it stands.
    pub sha: Option<&'a str>,
}

/// A git object name short enough to type and long enough to mean one
/// commit: what `git log --oneline` prints.
const MIN_SHA_PREFIX: usize = 7;

/// The `sha` a merge request is merged at: the head of its source branch.
///
/// GitLab documents `sha` as optional — "if present, this SHA must match the
/// HEAD of the source branch" — and answers 400 "SHA must be provided when
/// merging" to a merge without it when the project's namespace requires one
/// (`require_sha_for_merge?`, `lib/api/merge_requests.rb`). The tool sent
/// none, so on such a namespace it could merge nothing (Embra#4). It always
/// sends one now.
///
/// A caller that names the commit it reviewed gets the check the parameter
/// exists for: when the branch has moved past that commit, nothing is
/// merged and no request is made. A prefix is accepted and completed.
fn merge_sha(iid: u64, given: Option<&str>, head: &str) -> Result<String, String> {
    if head.is_empty() {
        return Err(format!("MR !{} has no head commit to merge.", iid));
    }
    let Some(given) = given.map(str::trim).filter(|g| !g.is_empty()) else {
        return Ok(head.to_string());
    };
    if given.len() < MIN_SHA_PREFIX || !given.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "sha must be the head commit gl_mr_view shows, at least {} hex characters of it; got '{}'. Nothing was merged.",
            MIN_SHA_PREFIX, given
        ));
    }
    if head.to_ascii_lowercase().starts_with(&given.to_ascii_lowercase()) {
        Ok(head.to_string())
    } else {
        Err(format!(
            "MR !{} was not merged: its source branch is at {}, not at {}. Commits were pushed after the one that was reviewed; read them with gl_mr_view and merge with the new sha.",
            iid, head, given
        ))
    }
}

/// The body of the merge request. `merge_when_pipeline_succeeds` has been
/// deprecated since GitLab 17.11 in favour of `auto_merge`, and a server
/// reads either; both are sent, so the flag works on a server older than
/// 17.11 and on one that has dropped the old name.
fn merge_payload(opts: &GlMergeOptions<'_>, sha: &str) -> serde_json::Value {
    serde_json::json!({
        "squash": opts.squash,
        "merge_when_pipeline_succeeds": opts.when_pipeline_succeeds,
        "auto_merge": opts.when_pipeline_succeeds,
        "should_remove_source_branch": opts.remove_source_branch,
        "sha": sha,
    })
}

/// What the tool says about a merge the server accepted. A merge queued
/// behind a pipeline leaves the merge request open; saying "opened" there
/// read as if nothing had happened.
fn merge_reply(iid: u64, sha: &str, mr: &serde_json::Value) -> String {
    let state = mr.get("state").and_then(|x| x.as_str()).unwrap_or("?");
    let queued = mr.get("merge_when_pipeline_succeeds").and_then(|x| x.as_bool()).unwrap_or(false);
    let short = sha.get(..8).unwrap_or(sha);
    let what = if state == "merged" {
        format!("merged at {}", short)
    } else if queued {
        format!("set to merge at {} when its pipeline passes", short)
    } else {
        state.to_string()
    };
    format!(
        "MR !{} {}: {}\n{}",
        iid,
        what,
        mr.get("title").and_then(|x| x.as_str()).unwrap_or("?"),
        mr.get("web_url").and_then(|x| x.as_str()).unwrap_or("")
    )
}

/// Merge a GitLab merge request.
///
/// GitLab's merge params are not GitHub's `merge_method`: squashing is a
/// boolean, and "merge when the pipeline passes" is a separate flag rather
/// than a method. Both are exposed as-is instead of being forced into the
/// GitHub vocabulary.
///
/// The merge request is read first, for the head of its source branch
/// (`merge_sha`).
pub async fn gl_mr_merge(
    db: &WardsonDbClient,
    host: Option<&str>,
    project: &str,
    iid: u64,
    opts: GlMergeOptions<'_>,
) -> String {
    let (host, token) = match gitlab_host_token(db, host).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    let proj = encode_gitlab_project(project);
    let mr = match gl_get(&host, &token, &format!("projects/{}/merge_requests/{}", proj, iid)).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let head = mr.get("sha").and_then(|v| v.as_str()).unwrap_or("");
    let sha = match merge_sha(iid, opts.sha, head) {
        Ok(sha) => sha,
        Err(why) => return why,
    };
    let path = format!("projects/{}/merge_requests/{}/merge", proj, iid);
    match gl_send(reqwest::Method::PUT, &host, &token, &path, &merge_payload(&opts, &sha)).await {
        Ok(v) => merge_reply(iid, &sha, &v),
        Err(e) => e,
    }
}

/// List a project's issue boards.
///
/// GitLab's answer to GitHub Projects is the issue board; "project" in
/// GitLab means repository, so these tools are named for boards to avoid
/// colliding with that meaning.
pub async fn gl_boards(db: &WardsonDbClient, host: Option<&str>, project: &str) -> String {
    let (host, token) = match gitlab_host_token(db, host).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    let path = format!("projects/{}/boards?per_page=20", encode_gitlab_project(project));
    let boards = match gl_get(&host, &token, &path).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let Some(arr) = boards.as_array().filter(|a| !a.is_empty()) else {
        return format!("No issue boards for {} on {}.", project, host);
    };
    let mut out = format!("=== Issue Boards: {} ({}) ===\n", project, arr.len());
    for b in arr {
        let lists = b.get("lists").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
        out.push_str(&format!(
            "  [{}] {} ({} lists)\n",
            b.get("id").and_then(|v| v.as_u64()).unwrap_or(0),
            b.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
            lists
        ));
    }
    out
}

/// View one issue board: its lists, in position order, with their labels.
pub async fn gl_board_view(
    db: &WardsonDbClient,
    host: Option<&str>,
    project: &str,
    board_id: u64,
) -> String {
    let (host, token) = match gitlab_host_token(db, host).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    let path = format!(
        "projects/{}/boards/{}",
        encode_gitlab_project(project),
        board_id
    );
    let board = match gl_get(&host, &token, &path).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let mut out = format!(
        "=== Board {}: {} ===\n",
        board_id,
        board.get("name").and_then(|v| v.as_str()).unwrap_or("?")
    );
    let mut lists: Vec<&serde_json::Value> = board
        .get("lists")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    if lists.is_empty() {
        out.push_str("  (no lists)\n");
        return out;
    }
    lists.sort_by_key(|l| l.get("position").and_then(|v| v.as_u64()).unwrap_or(u64::MAX));
    for l in lists {
        let label = l
            .get("label")
            .and_then(|lb| lb.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("(no label)");
        out.push_str(&format!(
            "  {}. {}\n",
            l.get("position").and_then(|v| v.as_u64()).unwrap_or(0),
            label
        ));
    }
    out
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gl_issues",
    description = "List open issues on a self-hosted GitLab instance. host is the GitLab hostname (token set via /git-token), project is the full path e.g. `group/repo`. Trusts operator CA drop-ins for private-CA instances."
)]
pub struct GlIssuesArgs {
    /// GitLab hostname, e.g. `gitlab.example.com`. Optional when exactly one
    /// host is configured via /git-token.
    #[serde(default)]
    pub host: Option<String>,
    /// Full project path, e.g. `group/repo`.
    pub project: String,
}

impl GlIssuesArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(gl_issues(ctx.db, self.host.as_deref(), &self.project).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gl_mrs",
    description = "List open merge requests on a self-hosted GitLab instance (GitLab's pull requests). host is the GitLab hostname (token set via /git-token), project is the full path e.g. `group/repo`."
)]
pub struct GlMrsArgs {
    /// GitLab hostname, e.g. `gitlab.example.com`. Optional when exactly one
    /// host is configured via /git-token.
    #[serde(default)]
    pub host: Option<String>,
    /// Full project path, e.g. `group/repo`.
    pub project: String,
}

impl GlMrsArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(gl_mrs(ctx.db, self.host.as_deref(), &self.project).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gl_issue_create",
    is_side_effectful = true,
    description = "Create an issue on a self-hosted GitLab instance. host is the GitLab hostname (token set via /git-token), project is the full path e.g. `group/repo`."
)]
pub struct GlIssueCreateArgs {
    /// GitLab hostname, e.g. `gitlab.example.com`. Optional when exactly one
    /// host is configured via /git-token.
    #[serde(default)]
    pub host: Option<String>,
    /// Full project path, e.g. `group/repo`.
    pub project: String,
    pub title: String,
    /// Issue body (Markdown). Optional.
    #[serde(default)]
    pub description: String,
}

impl GlIssueCreateArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(gl_issue_create(ctx.db, self.host.as_deref(), &self.project, &self.title, &self.description)
            .await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gl_mr_create",
    is_side_effectful = true,
    description = "Create a merge request on a self-hosted GitLab instance. source_branch is the branch with your changes, target_branch is where it merges (usually \"main\"). host/token via /git-token, project is the full path e.g. `group/repo`."
)]
pub struct GlMrCreateArgs {
    /// GitLab hostname, e.g. `gitlab.example.com`. Optional when exactly one
    /// host is configured via /git-token.
    #[serde(default)]
    pub host: Option<String>,
    /// Full project path, e.g. `group/repo`.
    pub project: String,
    pub title: String,
    pub source_branch: String,
    pub target_branch: String,
}

impl GlMrCreateArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(gl_mr_create(
            ctx.db,
            self.host.as_deref(),
            &self.project,
            &self.title,
            &self.source_branch,
            &self.target_branch,
        )
        .await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gl_issue_view",
    description = "View one issue on a self-hosted GitLab instance, with its full description and comment thread (system notes are filtered out). iid is the per-project issue number shown as #N. host/token via /git-token, project is the full path e.g. `group/repo`."
)]
pub struct GlIssueViewArgs {
    /// GitLab hostname, e.g. `gitlab.example.com`. Optional when exactly one
    /// host is configured via /git-token.
    #[serde(default)]
    pub host: Option<String>,
    /// Full project path, e.g. `group/repo`.
    pub project: String,
    /// Issue number within the project (the `#N` shown in the UI).
    pub iid: u64,
}

impl GlIssueViewArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(gl_issue_view(ctx.db, self.host.as_deref(), &self.project, self.iid).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gl_mr_view",
    description = "View one merge request on a self-hosted GitLab instance, with branches, head commit, merge status, description, and comment thread. iid is the per-project MR number shown as !N. host/token via /git-token, project is the full path e.g. `group/repo`."
)]
pub struct GlMrViewArgs {
    /// GitLab hostname, e.g. `gitlab.example.com`. Optional when exactly one
    /// host is configured via /git-token.
    #[serde(default)]
    pub host: Option<String>,
    /// Full project path, e.g. `group/repo`.
    pub project: String,
    /// Merge request number within the project (the `!N` shown in the UI).
    pub iid: u64,
}

impl GlMrViewArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(gl_mr_view(ctx.db, self.host.as_deref(), &self.project, self.iid).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gl_issue_close",
    is_side_effectful = true,
    description = "Close an issue on a self-hosted GitLab instance. iid is the per-project issue number. host/token via /git-token, project is the full path e.g. `group/repo`."
)]
pub struct GlIssueCloseArgs {
    /// GitLab hostname, e.g. `gitlab.example.com`. Optional when exactly one
    /// host is configured via /git-token.
    #[serde(default)]
    pub host: Option<String>,
    /// Full project path, e.g. `group/repo`.
    pub project: String,
    /// Issue number within the project.
    pub iid: u64,
}

impl GlIssueCloseArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(gl_issue_close(ctx.db, self.host.as_deref(), &self.project, self.iid).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gl_issue_reopen",
    is_side_effectful = true,
    description = "Reopen a closed issue on a self-hosted GitLab instance. iid is the per-project issue number. host/token via /git-token, project is the full path e.g. `group/repo`."
)]
pub struct GlIssueReopenArgs {
    /// GitLab hostname, e.g. `gitlab.example.com`. Optional when exactly one
    /// host is configured via /git-token.
    #[serde(default)]
    pub host: Option<String>,
    /// Full project path, e.g. `group/repo`.
    pub project: String,
    /// Issue number within the project.
    pub iid: u64,
}

impl GlIssueReopenArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(gl_issue_reopen(ctx.db, self.host.as_deref(), &self.project, self.iid).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gl_mr_close",
    is_side_effectful = true,
    description = "Close a merge request on a self-hosted GitLab instance WITHOUT merging it. To merge, use gl_mr_merge. iid is the per-project MR number. host/token via /git-token, project is the full path e.g. `group/repo`."
)]
pub struct GlMrCloseArgs {
    /// GitLab hostname, e.g. `gitlab.example.com`. Optional when exactly one
    /// host is configured via /git-token.
    #[serde(default)]
    pub host: Option<String>,
    /// Full project path, e.g. `group/repo`.
    pub project: String,
    /// Merge request number within the project.
    pub iid: u64,
}

impl GlMrCloseArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(gl_mr_close(ctx.db, self.host.as_deref(), &self.project, self.iid).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gl_issue_comment",
    is_side_effectful = true,
    description = "Post a comment on an issue on a self-hosted GitLab instance. iid is the per-project issue number. host/token via /git-token, project is the full path e.g. `group/repo`."
)]
pub struct GlIssueCommentArgs {
    /// GitLab hostname, e.g. `gitlab.example.com`. Optional when exactly one
    /// host is configured via /git-token.
    #[serde(default)]
    pub host: Option<String>,
    /// Full project path, e.g. `group/repo`.
    pub project: String,
    /// Issue number within the project.
    pub iid: u64,
    /// Comment body (Markdown).
    pub body: String,
}

impl GlIssueCommentArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(gl_issue_comment(ctx.db, self.host.as_deref(), &self.project, self.iid, &self.body).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gl_mr_comment",
    is_side_effectful = true,
    description = "Post a comment on a merge request on a self-hosted GitLab instance. iid is the per-project MR number. host/token via /git-token, project is the full path e.g. `group/repo`."
)]
pub struct GlMrCommentArgs {
    /// GitLab hostname, e.g. `gitlab.example.com`. Optional when exactly one
    /// host is configured via /git-token.
    #[serde(default)]
    pub host: Option<String>,
    /// Full project path, e.g. `group/repo`.
    pub project: String,
    /// Merge request number within the project.
    pub iid: u64,
    /// Comment body (Markdown).
    pub body: String,
}

impl GlMrCommentArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(gl_mr_comment(ctx.db, self.host.as_deref(), &self.project, self.iid, &self.body).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gl_mr_merge",
    is_side_effectful = true,
    description = "Merge a merge request on a self-hosted GitLab instance. GitLab's options are not GitHub's merge_method: squash is a boolean, and when_pipeline_succeeds queues the merge behind a green pipeline instead of merging now. Pass sha, the head commit gl_mr_view shows, to merge only what was reviewed: when the source branch has moved, nothing is merged. iid is the per-project MR number. host/token via /git-token, project is the full path e.g. `group/repo`."
)]
pub struct GlMrMergeArgs {
    /// GitLab hostname, e.g. `gitlab.example.com`. Optional when exactly one
    /// host is configured via /git-token.
    #[serde(default)]
    pub host: Option<String>,
    /// Full project path, e.g. `group/repo`.
    pub project: String,
    /// Merge request number within the project.
    pub iid: u64,
    /// Squash the MR's commits into one. Default false.
    #[serde(default)]
    pub squash: bool,
    /// Queue the merge until the pipeline passes instead of merging now. Default false.
    #[serde(default)]
    pub when_pipeline_succeeds: bool,
    /// Delete the source branch after merging. Default false.
    #[serde(default)]
    pub remove_source_branch: bool,
    /// Head commit of the source branch that was reviewed, as gl_mr_view
    /// shows it (at least 7 characters). The merge is refused when the
    /// branch has moved past it. Omit to merge the branch as it stands.
    #[serde(default)]
    pub sha: Option<String>,
}

impl GlMrMergeArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let opts = GlMergeOptions {
            squash: self.squash,
            when_pipeline_succeeds: self.when_pipeline_succeeds,
            remove_source_branch: self.remove_source_branch,
            sha: self.sha.as_deref(),
        };
        Ok(gl_mr_merge(ctx.db, self.host.as_deref(), &self.project, self.iid, opts).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gl_boards",
    description = "List the issue boards of a project on a self-hosted GitLab instance. Issue boards are GitLab's equivalent of GitHub Projects; note that \"project\" in GitLab means the repository itself. host/token via /git-token, project is the full path e.g. `group/repo`."
)]
pub struct GlBoardsArgs {
    /// GitLab hostname, e.g. `gitlab.example.com`. Optional when exactly one
    /// host is configured via /git-token.
    #[serde(default)]
    pub host: Option<String>,
    /// Full project path, e.g. `group/repo`.
    pub project: String,
}

impl GlBoardsArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(gl_boards(ctx.db, self.host.as_deref(), &self.project).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "gl_board_view",
    description = "View one issue board on a self-hosted GitLab instance: its lists in position order with the label backing each. board_id comes from gl_boards. host/token via /git-token, project is the full path e.g. `group/repo`."
)]
pub struct GlBoardViewArgs {
    /// GitLab hostname, e.g. `gitlab.example.com`. Optional when exactly one
    /// host is configured via /git-token.
    #[serde(default)]
    pub host: Option<String>,
    /// Full project path, e.g. `group/repo`.
    pub project: String,
    /// Board id, as reported by gl_boards.
    pub board_id: u64,
}

impl GlBoardViewArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(gl_board_view(ctx.db, self.host.as_deref(), &self.project, self.board_id).await)
    }
}

#[cfg(test)]
mod git_token_tests {
    use super::*;

    #[test]
    fn choose_gitlab_host_explicit_wins_and_normalizes() {
        let mut m = std::collections::BTreeMap::new();
        m.insert("gitlab.a.com".to_string(), "t1".to_string());
        m.insert("gitlab.b.com".to_string(), "t2".to_string());
        // Explicit host resolves even when the map is ambiguous.
        assert_eq!(
            choose_gitlab_host(Some("https://GitLab.B.com/group/repo.git"), &m),
            Ok("gitlab.b.com".to_string())
        );
        // Blank/whitespace host is treated as absent, not as an empty host.
        assert!(choose_gitlab_host(Some("   "), &m).is_err());
    }

    #[test]
    fn choose_gitlab_host_defaults_to_the_single_configured_host() {
        let mut m = std::collections::BTreeMap::new();
        m.insert("gitlab.ops.example".to_string(), "t".to_string());
        assert_eq!(choose_gitlab_host(None, &m), Ok("gitlab.ops.example".to_string()));
    }

    #[test]
    fn choose_gitlab_host_never_defaults_to_github() {
        // github.com rides github_token + the gh_* tools; it must never be
        // picked as the implicit GitLab host, even when it is the only entry.
        let mut m = std::collections::BTreeMap::new();
        m.insert("github.com".to_string(), "ghp_x".to_string());
        assert!(choose_gitlab_host(None, &m).is_err());

        // With one real GitLab host alongside it, that host wins outright.
        m.insert("gitlab.ops.example".to_string(), "t".to_string());
        assert_eq!(choose_gitlab_host(None, &m), Ok("gitlab.ops.example".to_string()));
    }

    #[test]
    fn choose_gitlab_host_ambiguity_names_the_candidates() {
        let mut m = std::collections::BTreeMap::new();
        m.insert("gitlab.a.com".to_string(), "t1".to_string());
        m.insert("gitea.b.com:3000".to_string(), "t2".to_string());
        let err = choose_gitlab_host(None, &m).unwrap_err();
        assert!(err.contains("gitlab.a.com"), "{err}");
        assert!(err.contains("gitea.b.com:3000"), "{err}");
    }

    #[test]
    fn choose_gitlab_host_empty_map_explains_the_fix() {
        let m = std::collections::BTreeMap::new();
        let err = choose_gitlab_host(None, &m).unwrap_err();
        assert!(err.contains("/git-token"), "{err}");
    }

    #[test]
    fn render_gl_notes_filters_system_notes() {
        // GitLab records "changed the description" churn as system notes;
        // only human comments belong in the rendered thread.
        let notes = serde_json::json!([
            {"system": true,  "body": "changed the description",
             "author": {"username": "bot"}, "created_at": "2026-01-01T00:00:00Z"},
            {"system": false, "body": "looks good to me",
             "author": {"username": "will"}, "created_at": "2026-01-02T00:00:00Z"},
        ]);
        let out = render_gl_notes(&notes);
        assert!(out.contains("Comments (1)"), "{out}");
        assert!(out.contains("looks good to me"), "{out}");
        assert!(!out.contains("changed the description"), "{out}");
        assert!(out.contains("will"), "{out}");
    }

    #[test]
    fn render_gl_notes_handles_empty_and_all_system() {
        assert!(render_gl_notes(&serde_json::json!([])).contains("(no comments)"));
        let all_system = serde_json::json!([{"system": true, "body": "x"}]);
        assert!(render_gl_notes(&all_system).contains("(no comments)"));
        // A non-array (a failed fetch decoded to null) renders nothing, never panics.
        assert_eq!(render_gl_notes(&serde_json::Value::Null), "");
    }

    #[test]
    fn all_fourteen_gl_tools_are_registered() {
        // gh_* parity set (2026-09-08). gl_boards/gl_board_view deliberately
        // do NOT mirror gh_project_* by name: in GitLab "project" means the
        // repository, so board naming avoids a misleading tool name.
        let names: Vec<&'static str> =
            crate::tools::registry::all_descriptors().map(|d| d.name).collect();
        for t in [
            "gl_issues", "gl_mrs", "gl_issue_create", "gl_mr_create",
            "gl_issue_view", "gl_mr_view", "gl_issue_close", "gl_issue_reopen",
            "gl_mr_close", "gl_issue_comment", "gl_mr_comment", "gl_mr_merge",
            "gl_boards", "gl_board_view",
        ] {
            assert!(names.contains(&t), "gl_* tool not registered: {t}");
        }
        assert_eq!(names.iter().filter(|n| n.starts_with("gl_")).count(), 14);
        // Mutating tools must be marked side-effectful so the dispatcher and
        // the operator-facing surfaces treat them as writes.
        for d in crate::tools::registry::all_descriptors() {
            let mutating = matches!(
                d.name,
                "gl_issue_create" | "gl_mr_create" | "gl_issue_close" | "gl_issue_reopen"
                    | "gl_mr_close" | "gl_issue_comment" | "gl_mr_comment" | "gl_mr_merge"
            );
            if mutating {
                assert!(d.is_side_effectful, "{} must be side-effectful", d.name);
            }
        }
    }

    const HEAD: &str = "35a8fcfb4f7eb630b541c8a4f0e476621bb621ce";

    fn merge_opts(sha: Option<&str>) -> GlMergeOptions<'_> {
        GlMergeOptions { squash: false, when_pipeline_succeeds: false, remove_source_branch: true, sha }
    }

    /// Embra#4: the namespace requires a sha, the tool sent none, and every
    /// merge answered 400. Without an argument the head of the source
    /// branch is sent.
    #[test]
    fn a_merge_always_carries_the_head_of_the_source_branch() {
        let sha = merge_sha(29, None, HEAD).expect("the head");
        assert_eq!(sha, HEAD);
        assert_eq!(merge_sha(29, Some("  "), HEAD).as_deref(), Ok(HEAD), "an empty argument is none");
        let body = merge_payload(&merge_opts(None), &sha);
        assert_eq!(body["sha"], HEAD);
        assert_eq!(body["should_remove_source_branch"], true);
        assert_eq!(body["squash"], false);
    }

    #[test]
    fn a_reviewed_commit_merges_only_while_it_is_the_head() {
        // The full name and a prefix of it, in either case.
        assert_eq!(merge_sha(29, Some(HEAD), HEAD).as_deref(), Ok(HEAD));
        assert_eq!(merge_sha(29, Some("35a8fcf"), HEAD).as_deref(), Ok(HEAD));
        assert_eq!(merge_sha(29, Some("35A8FCFB4F"), HEAD).as_deref(), Ok(HEAD));
        // The branch moved: no merge, and both commits are named.
        let moved = merge_sha(29, Some("1111111aaaa"), HEAD).expect_err("refused");
        assert!(moved.starts_with("MR !29 was not merged"), "{moved}");
        assert!(moved.contains(HEAD) && moved.contains("1111111aaaa"), "{moved}");
        assert!(moved.contains("gl_mr_view"), "{moved}");
    }

    #[test]
    fn a_sha_that_names_no_commit_is_refused_before_any_request() {
        for bad in ["35a8fc", "main", "HEAD", "35a8fcfz"] {
            let why = merge_sha(29, Some(bad), HEAD).expect_err("refused");
            assert!(why.contains("at least 7 hex characters"), "{bad}: {why}");
            assert!(why.contains("Nothing was merged"), "{bad}: {why}");
        }
        let empty = merge_sha(29, None, "").expect_err("refused");
        assert!(empty.contains("no head commit"), "{empty}");
    }

    /// `merge_when_pipeline_succeeds` is the deprecated name of `auto_merge`
    /// (GitLab 17.11). Both travel with one value.
    #[test]
    fn the_pipeline_flag_is_sent_under_both_of_its_names() {
        let mut opts = merge_opts(None);
        opts.when_pipeline_succeeds = true;
        let body = merge_payload(&opts, HEAD);
        assert_eq!(body["merge_when_pipeline_succeeds"], true);
        assert_eq!(body["auto_merge"], true);
        let now = merge_payload(&merge_opts(None), HEAD);
        assert_eq!(now["merge_when_pipeline_succeeds"], false);
        assert_eq!(now["auto_merge"], false);
    }

    #[test]
    fn the_reply_says_merged_queued_or_the_state() {
        let mr = |state: &str, queued: bool| {
            serde_json::json!({
                "state": state, "merge_when_pipeline_succeeds": queued,
                "title": "test: a merge", "web_url": "https://gitlab.example.com/g/r/-/merge_requests/29",
            })
        };
        assert_eq!(
            merge_reply(29, HEAD, &mr("merged", false)),
            "MR !29 merged at 35a8fcfb: test: a merge\nhttps://gitlab.example.com/g/r/-/merge_requests/29"
        );
        assert!(merge_reply(29, HEAD, &mr("opened", true))
            .starts_with("MR !29 set to merge at 35a8fcfb when its pipeline passes: "));
        assert!(merge_reply(29, HEAD, &mr("opened", false)).starts_with("MR !29 opened: "));
    }

    #[test]
    fn gl_mr_merge_takes_an_optional_sha() {
        let d = crate::tools::registry::all_descriptors().find(|d| d.name == "gl_mr_merge").expect("registered");
        let schema = (d.input_schema)();
        assert_eq!(schema["required"], serde_json::json!(["iid", "project"]));
        assert!(schema["properties"]["sha"].is_object(), "{schema}");
        assert!(d.description.contains("gl_mr_view"), "the description says where the sha comes from");
    }

    #[test]
    fn encode_gitlab_project_encodes_path_segment() {
        assert_eq!(encode_gitlab_project("group/repo"), "group%2Frepo");
        assert_eq!(
            encode_gitlab_project("group/sub-group/my.repo_1"),
            "group%2Fsub-group%2Fmy.repo_1"
        );
        // Leading/trailing slashes are trimmed, not encoded.
        assert_eq!(encode_gitlab_project("/group/repo/"), "group%2Frepo");
        assert_eq!(encode_gitlab_project("répo"), "r%C3%A9po");
    }
}
