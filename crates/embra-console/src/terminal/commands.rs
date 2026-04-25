//! Slash command handling for embra-console.
//!
//! Only local commands (/help, /copy) are handled here.
//! All other commands are forwarded to embra-brain via gRPC.

/// Returns true if this command is handled locally (not sent to brain)
pub fn is_local_command(cmd: &str) -> bool {
    matches!(cmd, "/help" | "/copy" | "/ml")
}

/// Handle a local command, returning the output string
pub fn handle_local_command(cmd: &str, _args: &str, name: &str) -> Option<String> {
    match cmd {
        "/help" => Some(format!(
            r#"=== {} Help ===

Commands:
  /help                          Show this help message
  /ml                            Toggle multi-line mode (. on own line to send)
  /status                        System status
  /sessions                      List all sessions
  /new <name>                    Create a new session
  /switch <name>                 Switch to a session
  /close                         Close current session
  /soul                          Display the soul document
  /identity                      Display identity document
  /mode                          Show operating mode

Provider:
  /provider                      Show active provider, model, session
  /provider <anthropic|gemini>   Switch provider for future turns
  /provider --setup [<kind>]     Add an alternate provider's API key (multi-turn)

Setup:
  /github-token <token>          Set GitHub token
  /ssh-keygen                    Generate SSH key pair
  /ssh-copy-id <user@host>       Copy SSH key to host
  /git-setup <name> | <email>    Set git user config

Experimental:
  /feedback-loop                 Trigger Phase 3 Continuity Engine self-evaluation

Keyboard:
  Enter              Send message (or newline in /ml mode)
  Alt+Enter          New line
  Up/Down            Scroll history
  Ctrl+C / Ctrl+D    Exit"#, name)),
        "/copy" => Some("Clipboard copy not yet implemented in Phase 1.".to_string()),
        _ => None,
    }
}
