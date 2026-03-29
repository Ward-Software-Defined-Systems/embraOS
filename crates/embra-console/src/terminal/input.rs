// Input handling is integrated directly into terminal/mod.rs handle_key_event
// This module is reserved for future input processing utilities

use anyhow::Result;

/// Validate user input before sending
pub fn validate_input(input: &str) -> Result<()> {
    if input.len() > 100_000 {
        anyhow::bail!("Input too long (max 100,000 characters)");
    }
    Ok(())
}
