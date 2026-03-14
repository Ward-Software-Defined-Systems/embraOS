use anyhow::Result;

use super::AppState;
use crate::learning;
use crate::sessions::SessionState;

pub async fn handle_command(input: &str, app: &mut AppState) -> Result<Option<String>> {
    let parts: Vec<&str> = input.trim().splitn(2, ' ').collect();
    let command = parts[0];
    let arg = parts.get(1).map(|s| s.trim());

    match command {
        "/help" => Ok(Some(help_text(&app.config.name))),

        "/status" => {
            let status = crate::tools::system_status(&app.db).await;
            Ok(Some(serde_json::to_string_pretty(&status)?))
        }

        "/sessions" => {
            let sessions = app.session_manager.list().await?;
            if sessions.is_empty() {
                return Ok(Some("No sessions.".into()));
            }
            let mut output = String::from("Sessions:\n");
            for s in &sessions {
                let marker = if app.session_manager.active_session.as_deref() == Some(&s.name) {
                    "*"
                } else {
                    " "
                };
                let state = match s.state {
                    SessionState::Active => "active",
                    SessionState::Detached => "detached",
                    SessionState::Closed => "closed",
                };
                output.push_str(&format!(
                    "  [{}] {} ({}) - last active: {}\n",
                    marker,
                    s.name,
                    state,
                    s.last_active.format("%Y-%m-%d %H:%M")
                ));
            }
            Ok(Some(output))
        }

        "/new" => {
            let name = arg.unwrap_or("unnamed");
            app.session_manager.create(name).await?;
            app.messages.clear();
            Ok(Some(format!("Created and switched to session '{}'", name)))
        }

        "/switch" => {
            if let Some(name) = arg {
                let history = app.session_manager.reattach(name).await?;
                app.messages = history
                    .iter()
                    .map(|m| super::DisplayMessage::new(&m.role, &m.content))
                    .collect();
                Ok(Some(format!("Switched to session '{}'", name)))
            } else {
                Ok(Some("Usage: /switch <session_name>".into()))
            }
        }

        "/close" => {
            if let Some(name) = app.session_manager.active_session.clone() {
                app.session_manager.close(&name).await?;
                app.messages.clear();
                Ok(Some(format!("Closed session '{}'. Use /new or /switch.", name)))
            } else {
                Ok(Some("No active session to close.".into()))
            }
        }

        "/soul" => {
            if let Some(soul) = &app.soul {
                Ok(Some(format!(
                    "Soul Document (IMMUTABLE):\n{}",
                    serde_json::to_string_pretty(soul)?
                )))
            } else {
                Ok(Some("No soul document found.".into()))
            }
        }

        "/identity" => {
            if let Some(identity) = &app.identity {
                Ok(Some(format!(
                    "Identity:\n{}",
                    serde_json::to_string_pretty(identity)?
                )))
            } else {
                Ok(Some("No identity document found.".into()))
            }
        }

        "/mode" => {
            let soul_status = if learning::is_soul_sealed(&app.db).await? {
                "sealed"
            } else {
                "unsealed"
            };
            Ok(Some(format!("Mode: Operational | Soul: {}", soul_status)))
        }

        _ => Ok(Some(format!("Unknown command: {}. Type /help for help.", command))),
    }
}

fn help_text(name: &str) -> String {
    format!(
        r#"{name} — embraOS Phase 0 Commands:

  /help        Show this help
  /status      System status
  /sessions    List all sessions
  /new <name>  Create new session
  /switch <n>  Switch to session
  /close       Close current session
  /soul        Display soul document
  /identity    Display identity
  /mode        Show current mode

Keyboard:
  Enter        Send message
  Shift+Enter  New line
  Up/Down      Scroll history
  Ctrl+C       Graceful detach
  Ctrl+D       Graceful detach"#
    )
}
