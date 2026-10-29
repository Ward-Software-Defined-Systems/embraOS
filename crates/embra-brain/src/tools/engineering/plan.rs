//! The plan and task tools, stored in WardSONDB.

use chrono::Utc;
use embra_tool_macro::embra_tool;
use embra_tools_core::DispatchError;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::db::WardsonDbClient;
use crate::tools::registry::DispatchContext;

/// List the plans.
pub async fn plan_list(db: &WardsonDbClient) -> String {
    let plans = match db.fetch_collection("plans").await {
        Ok(p) => p,
        Err(e) => return format!("Failed to load plans from the database: {}", e),
    };

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
    output
}

/// Create a plan in WardSONDB.
pub async fn plan_create(db: &WardsonDbClient, title: &str, description: &str) -> String {
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

/// List the tasks, every one or those `filter` matches by plan id or
/// title substring.
pub async fn tasks(db: &WardsonDbClient, filter: &str) -> String {
    if filter.is_empty() {
        let all_tasks = match db.fetch_collection("tasks").await {
            Ok(t) => t,
            Err(e) => return format!("Failed to load tasks from the database: {}", e),
        };

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

    // Filter by plan_id if the filter looks like an ID
    let filtered = match db.fetch_collection("tasks").await {
        Ok(t) => t,
        Err(e) => return format!("Failed to load tasks from the database: {}", e),
    };

    let matching: Vec<_> = filtered
        .iter()
        .filter(|doc| {
            let plan = doc.get("plan_id").and_then(|v| v.as_str()).unwrap_or("");
            let title = doc.get("title").and_then(|v| v.as_str()).unwrap_or("");
            plan.contains(filter) || title.to_lowercase().contains(&filter.to_lowercase())
        })
        .collect();

    if matching.is_empty() {
        return format!("No tasks matching '{}'.", filter);
    }

    let mut output = format!("=== Tasks matching '{}' ({}) ===\n", filter, matching.len());
    for doc in &matching {
        let id = doc.get("_id").or(doc.get("id")).and_then(|v| v.as_str()).unwrap_or("?");
        let title = doc.get("title").and_then(|v| v.as_str()).unwrap_or("Untitled");
        let done = doc.get("done").and_then(|v| v.as_bool()).unwrap_or(false);
        let marker = if done { "x" } else { " " };
        output.push_str(&format!("  [{}] {} (ID: {})\n", marker, title, id));
    }
    output
}

/// Add a task, under a plan when `plan_id` names one.
pub async fn task_add(db: &WardsonDbClient, title: &str, plan_id: Option<&str>) -> String {
    if title.is_empty() {
        return "task_add rejected (title is empty)".into();
    }

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
        Err(e) if crate::db::error::is_not_found(&e) => format!("Task not found: {}", task_id.trim()),
        Err(e) => format!("Failed to read task {}: {}", task_id.trim(), e),
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
        Err(e) if crate::db::error::is_not_found(&e) => return format!("Task not found: {}", id),
        Err(e) => return format!("Failed to read task {}: {}", id, e),
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
        Err(e) if crate::db::error::is_not_found(&e) => return format!("Plan not found: {}", id),
        Err(e) => return format!("Failed to read plan {}: {}", id, e),
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

/// What a `plan` call asks for: `None` lists (no title, or an empty one);
/// a title creates, with the description or none.
fn plan_request(title: Option<String>, description: Option<String>) -> Option<(String, String)> {
    match title {
        Some(title) if !title.is_empty() => Some((title, description.unwrap_or_default())),
        _ => None,
    }
}

impl PlanArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(match plan_request(self.title, self.description) {
            None => plan_list(ctx.db).await,
            Some((title, description)) => plan_create(ctx.db, &title, &description).await,
        })
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
        let plan_id = self.plan_id.as_deref().filter(|p| !p.is_empty());
        Ok(task_add(ctx.db, &self.title, plan_id).await)
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

#[cfg(test)]
mod native_args_tests {
    use super::*;

    /// #37: the fields replace the `title | description` string the tool
    /// once rebuilt and re-split. No title, or an empty one, lists the
    /// plans; a title creates one, with the description or none.
    #[test]
    fn a_plan_without_a_title_lists_and_one_with_a_title_creates() {
        assert_eq!(plan_request(None, None), None);
        assert_eq!(plan_request(None, Some("d".into())), None);
        assert_eq!(plan_request(Some(String::new()), Some("d".into())), None);
        assert_eq!(plan_request(Some("t".into()), None), Some(("t".into(), String::new())));
        assert_eq!(plan_request(Some("t".into()), Some("d".into())), Some(("t".into(), "d".into())));
        // A title carrying the old delimiter is a title.
        assert_eq!(
            plan_request(Some("a | b".into()), None),
            Some(("a | b".into(), String::new()))
        );
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
}
