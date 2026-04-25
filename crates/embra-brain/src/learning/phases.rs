use anyhow::Result;

use crate::brain::prompts;
use crate::config::SystemConfig;
use crate::db::WardsonDbClient;

use super::{LearningPhase, LearningState};

pub fn phase_kickoff(phase: &LearningPhase) -> String {
    match phase {
        LearningPhase::UserConfiguration => {
            "Hello! I'm here and ready to begin the setup process. Let's get started.".into()
        }
        LearningPhase::IdentityFormation => {
            "User profile confirmed. Let's move on to defining your identity.".into()
        }
        LearningPhase::SoulDefinition => {
            "Identity confirmed. Now let's define your soul — your immutable core values.".into()
        }
        LearningPhase::InitialToolset => String::new(),
        LearningPhase::Confirmation => {
            "Tools configured. Let's do a final review of everything.".into()
        }
        LearningPhase::Complete => String::new(),
    }
}

pub fn system_prompt_for_phase(state: &LearningState, config: &SystemConfig) -> String {
    match state.phase {
        LearningPhase::UserConfiguration => {
            prompts::learning_user_configuration(&config.name)
        }
        LearningPhase::IdentityFormation => {
            let user_profile = state
                .user_profile
                .as_ref()
                .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                .unwrap_or_default();
            let user_name = state
                .user_profile
                .as_ref()
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("User");
            prompts::learning_identity_formation(&config.name, user_name, &user_profile)
        }
        LearningPhase::SoulDefinition => {
            let user_profile = state
                .user_profile
                .as_ref()
                .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                .unwrap_or_default();
            let identity = state
                .identity
                .as_ref()
                .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                .unwrap_or_default();
            prompts::learning_soul_definition(&config.name, &user_profile, &identity)
        }
        LearningPhase::InitialToolset => String::new(),
        LearningPhase::Confirmation => {
            let user_profile = state
                .user_profile
                .as_ref()
                .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                .unwrap_or_default();
            let user_name = state
                .user_profile
                .as_ref()
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("User");
            let identity = state
                .identity
                .as_ref()
                .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                .unwrap_or_default();
            let soul = state
                .soul
                .as_ref()
                .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                .unwrap_or_default();
            let tools = state
                .tools_config
                .as_ref()
                .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                .unwrap_or_default();
            prompts::learning_confirmation(
                &config.name,
                user_name,
                &user_profile,
                &identity,
                &soul,
                &tools,
            )
        }
        LearningPhase::Complete => String::new(),
    }
}

pub fn phase_label(phase: &LearningPhase) -> &'static str {
    match phase {
        LearningPhase::UserConfiguration => "User Configuration",
        LearningPhase::IdentityFormation => "Identity Formation",
        LearningPhase::SoulDefinition => "Soul Definition",
        LearningPhase::InitialToolset => "Initial Toolset",
        LearningPhase::Confirmation => "Final Confirmation",
        LearningPhase::Complete => "Complete",
    }
}

// Single source of truth for Phase 4 tool category counts.
// (json_key, display_label, count). Sums to 90 — matches the descriptor count
// in `tools::registry::REGISTRY`. Aliases (`memory_search`, `search_memory`,
// `file_rename`, `rmdir`) are folded into their target's category so the
// displayed total matches what the model sees in the tools manifest. Keep in
// sync with `tools::registry::REGISTRY.len()` when tools are added or removed.
//
// Engineering went 28 → 33 across Sprint 3 post-merge passes:
//   pass #1 added `plan_delete` + `task_delete` (+2),
//   pass #2 added `gh_issue_view` + `gh_pr_view` + `git_merge` (+3).
// Sprint 4 (GEMINI-PROVIDER-01) did not add or remove any tools.
const CATEGORY_COUNTS: &[(&str, &str, usize)] = &[
    ("system", "System", 3),
    ("memory_knowledge", "Memory & Knowledge", 7),
    ("self_awareness", "Self-Awareness", 4),
    ("time_context", "Time & Context", 3),
    ("utility", "Utility", 2),
    ("security", "Security", 6),
    ("engineering", "Engineering", 33),
    ("filesystem", "Filesystem", 10),
    ("scheduling", "Scheduling", 3),
    ("sessions", "Sessions", 10),
    ("knowledge_graph", "Knowledge Graph", 9),
];

pub fn default_tools_registry_doc() -> serde_json::Value {
    let categories: serde_json::Map<String, serde_json::Value> = CATEGORY_COUNTS
        .iter()
        .map(|(key, _, count)| ((*key).to_string(), serde_json::json!(count)))
        .collect();
    let total: usize = CATEGORY_COUNTS.iter().map(|(_, _, c)| c).sum();
    serde_json::json!({
        "policy": "all_enabled",
        "sealed_at": chrono::Utc::now().to_rfc3339(),
        "categories": categories,
        "tool_count": total,
    })
}

pub fn tool_summary_message(name: &str) -> String {
    let total: usize = CATEGORY_COUNTS.iter().map(|(_, _, c)| c).sum();
    let categories_list = CATEGORY_COUNTS
        .iter()
        .map(|(_, label, count)| format!("  - {label} ({count})"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "=== Phase 4: Initial Toolset ===\n\
         All {total} built-in tools are enabled by default for {name}.\n\
         \n\
         {categories_list}\n\
         \n\
         Safety:\n\
         \x20 - Filesystem and git writes are restricted to /embra/workspace.\n\
         \x20 - SSH and port scans are restricted to RFC 1918 / loopback addresses.\n\
         \n\
         \u{2192} Advancing to Final Confirmation..."
    )
}

pub async fn handle_phase_complete(
    state: &mut LearningState,
    db: &WardsonDbClient,
    _config: &SystemConfig,
) -> Result<()> {
    // Extract the most recent JSON document from conversation
    let doc = extract_last_json_document(&state.conversation_history);

    match state.phase {
        LearningPhase::UserConfiguration => {
            if let Some(profile) = doc {
                state.user_profile = Some(profile.clone());
                persist_document(db, "memory.user", &profile, Some("user")).await?;
                tracing::info!("User profile persisted");
            }
            state.phase = LearningPhase::IdentityFormation;
        }
        LearningPhase::IdentityFormation => {
            if let Some(identity) = doc {
                state.identity = Some(identity.clone());
                persist_document(db, "memory.identity", &identity, Some("identity")).await?;
                tracing::info!("Identity persisted");
            }
            state.phase = LearningPhase::SoulDefinition;
        }
        LearningPhase::SoulDefinition => {
            if let Some(soul) = doc {
                state.soul = Some(soul.clone());
                // Soul gets sealed — written and marked immutable (seal_soul sets _id: "soul")
                super::seal_soul(db, &soul).await?;
                tracing::info!("Soul sealed");
            }
            state.phase = LearningPhase::InitialToolset;
        }
        LearningPhase::InitialToolset => {
            let tools = default_tools_registry_doc();
            state.tools_config = Some(tools.clone());
            persist_document(db, "tools.registry", &tools, None).await?;
            tracing::info!("Tools config persisted (all_enabled policy)");
            state.phase = LearningPhase::Confirmation;
        }
        LearningPhase::Confirmation => {
            state.phase = LearningPhase::Complete;
            tracing::info!("Learning mode complete");
        }
        LearningPhase::Complete => {}
    }

    Ok(())
}

async fn persist_document(
    db: &WardsonDbClient,
    collection: &str,
    doc: &serde_json::Value,
    doc_id: Option<&str>,
) -> Result<()> {
    if !db.collection_exists(collection).await? {
        db.create_collection(collection).await?;
    }
    let mut write_doc = doc.clone();
    if let (Some(id), Some(obj)) = (doc_id, write_doc.as_object_mut()) {
        obj.insert("_id".into(), serde_json::json!(id));
    }
    db.write(collection, &write_doc).await?;
    Ok(())
}

fn extract_last_json_document(history: &[crate::brain::Message]) -> Option<serde_json::Value> {
    // Search backwards through assistant messages for JSON code blocks
    for msg in history.iter().rev() {
        if msg.role != "assistant" {
            continue;
        }
        if let Some(json) = extract_json_from_text(&msg.content) {
            return Some(json);
        }
    }
    None
}

fn extract_json_from_text(text: &str) -> Option<serde_json::Value> {
    // Look for ```json ... ``` blocks
    let mut search = text;
    while let Some(start) = search.find("```json") {
        let after_marker = &search[start + 7..];
        if let Some(end) = after_marker.find("```") {
            let json_str = after_marker[..end].trim();
            if let Ok(val) = serde_json::from_str(json_str) {
                return Some(val);
            }
        }
        search = &search[start + 7..];
    }

    // Fallback: try to find any JSON object in the text
    if let Some(start) = text.rfind('{') {
        // Find matching closing brace
        let mut depth = 0;
        let mut end = None;
        for (i, ch) in text[start..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(start + i + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        if let Some(end) = end {
            if let Ok(val) = serde_json::from_str(&text[start..end]) {
                return Some(val);
            }
        }
    }

    None
}
