mod phases;
mod soul;

pub use phases::*;
pub use soul::*;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::brain::{Brain, Message};
use crate::config::SystemConfig;
use crate::db::WardsonDbClient;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LearningPhase {
    UserConfiguration,
    IdentityFormation,
    SoulDefinition,
    InitialToolset,
    Confirmation,
    Complete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningState {
    pub phase: LearningPhase,
    pub user_profile: Option<serde_json::Value>,
    pub identity: Option<serde_json::Value>,
    pub soul: Option<serde_json::Value>,
    pub tools_config: Option<serde_json::Value>,
    pub conversation_history: Vec<Message>,
}

impl LearningState {
    pub fn new() -> Self {
        Self {
            phase: LearningPhase::UserConfiguration,
            user_profile: None,
            identity: None,
            soul: None,
            tools_config: None,
            conversation_history: Vec::new(),
        }
    }
}

pub async fn run_learning_mode(
    db: &WardsonDbClient,
    config: &SystemConfig,
) -> Result<()> {
    use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};
    use std::io::Write;

    let mut brain = Brain::new(config.api_key.clone(), String::new());
    let mut state = LearningState::new();

    println!();
    crossterm::execute!(
        std::io::stdout(),
        SetForegroundColor(Color::Cyan),
        Print(format!(
            "═══ {} Learning Mode ═══\n",
            config.name
        )),
        ResetColor
    )?;
    println!("This is a guided conversation to establish identity and values.");
    println!("Take your time — this shapes who {} will be.\n", config.name);

    while state.phase != LearningPhase::Complete {
        // Set system prompt for current phase
        let system_prompt = phases::system_prompt_for_phase(&state, config);
        brain.set_system_prompt(system_prompt);

        // Print phase header
        let phase_label = phases::phase_label(&state.phase);
        println!();
        crossterm::execute!(
            std::io::stdout(),
            SetForegroundColor(Color::Yellow),
            Print(format!("── Phase: {} ──\n", phase_label)),
            ResetColor
        )?;

        // Get initial AI message for this phase
        // Anthropic API requires at least one user message; send a phase kick-off
        let kickoff = phases::phase_kickoff(&state.phase);
        state.conversation_history.push(Message::user(&kickoff));
        let response = brain.send_message(&state.conversation_history).await?;
        state
            .conversation_history
            .push(Message::assistant(&response));
        crossterm::execute!(
            std::io::stdout(),
            SetForegroundColor(Color::Green),
            Print(format!("{}: ", config.name)),
            ResetColor
        )?;
        println!("{}\n", response);

        // Conversation loop within this phase
        loop {
            crossterm::execute!(
                std::io::stdout(),
                SetForegroundColor(Color::Blue),
                Print("You: "),
                ResetColor
            )?;
            std::io::stdout().flush()?;

            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            let input = input.trim().to_string();

            if input.is_empty() {
                continue;
            }

            state.conversation_history.push(Message::user(&input));

            // Send to brain
            let response = brain.send_message(&state.conversation_history).await?;

            // Check for phase completion
            let phase_complete = response.contains("[PHASE_COMPLETE]");
            let display_response = response.replace("[PHASE_COMPLETE]", "").trim().to_string();

            if !display_response.is_empty() {
                state
                    .conversation_history
                    .push(Message::assistant(&display_response));

                crossterm::execute!(
                    std::io::stdout(),
                    SetForegroundColor(Color::Green),
                    Print(format!("\n{}: ", config.name)),
                    ResetColor
                )?;
                println!("{}\n", display_response);
            }

            if phase_complete {
                phases::handle_phase_complete(&mut state, db, config).await?;
                break; // Move to next phase
            }
        }
    }

    println!();
    crossterm::execute!(
        std::io::stdout(),
        SetForegroundColor(Color::Cyan),
        Print(format!(
            "═══ {} Learning Mode Complete ═══\n",
            config.name
        )),
        ResetColor
    )?;
    println!(
        "{} is now configured and ready. Entering operational mode.\n",
        config.name
    );

    // Save learning conversation
    if !db.collection_exists("sessions.learning.history").await? {
        db.create_collection("sessions.learning.history").await?;
    }
    let learning_history = serde_json::json!({
        "session_name": "learning",
        "turns": state.conversation_history,
    });
    db.write("sessions.learning.history", &learning_history)
        .await?;

    Ok(())
}
