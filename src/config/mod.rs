use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::{self, Write};

use crate::db::WardsonDbClient;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemConfig {
    pub name: String,
    pub api_key: String,
    pub timezone: String,
    pub deployment_mode: String,
    pub created_at: String,
    pub version: String,
}

pub async fn run_config_wizard() -> Result<SystemConfig> {
    println!();
    println!("╔══════════════════════════════════════════╗");
    println!("║     embraOS Phase 0 — First Run Setup    ║");
    println!("╚══════════════════════════════════════════╝");
    println!();

    // Name
    let name = prompt_with_default(
        "What would you like to name your intelligence?",
        "Embra",
    )?;

    // API Key
    let api_key = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(key) if !key.is_empty() => {
            println!("✓ Anthropic API key detected from environment.");
            key
        }
        _ => {
            let key = prompt_required("Enter your Anthropic API key:")?;
            if !key.starts_with("sk-") {
                println!("⚠ Warning: API key doesn't start with 'sk-'. Proceeding anyway.");
            }
            key
        }
    };

    // Timezone
    let tz_default = detect_timezone();
    let timezone = prompt_with_default(
        &format!("Timezone (detected: {}):", tz_default),
        &tz_default,
    )?;

    let config = SystemConfig {
        name,
        api_key,
        timezone,
        deployment_mode: "container".into(),
        created_at: chrono::Utc::now().to_rfc3339(),
        version: env!("CARGO_PKG_VERSION").into(),
    };

    println!();
    println!("Configuration complete:");
    println!("  Name: {}", config.name);
    println!("  Timezone: {}", config.timezone);
    println!("  Mode: {}", config.deployment_mode);
    println!("  Version: {}", config.version);
    println!();

    Ok(config)
}

pub async fn save_config(db: &WardsonDbClient, config: &SystemConfig) -> Result<()> {
    if !db.collection_exists("config.system").await? {
        db.create_collection("config.system").await?;
    }
    let doc = serde_json::to_value(config)?;
    db.write("config.system", &doc).await?;
    Ok(())
}

pub async fn load_config(db: &WardsonDbClient) -> Result<SystemConfig> {
    let results = db
        .query("config.system", &serde_json::json!({}))
        .await?;
    let doc = results
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No system config found"))?;
    let config: SystemConfig = serde_json::from_value(doc)?;
    Ok(config)
}

fn prompt_with_default(prompt: &str, default: &str) -> Result<String> {
    print!("{} [{}] ", prompt, default);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

fn prompt_required(prompt: &str) -> Result<String> {
    loop {
        print!("{} ", prompt);
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let trimmed = input.trim().to_string();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
        println!("This field is required.");
    }
}

fn detect_timezone() -> String {
    // Try to read from system
    if let Ok(tz) = std::env::var("TZ") {
        if !tz.is_empty() {
            return tz;
        }
    }
    // Try reading /etc/timezone (Linux)
    if let Ok(tz) = std::fs::read_to_string("/etc/timezone") {
        let tz = tz.trim().to_string();
        if !tz.is_empty() {
            return tz;
        }
    }
    "UTC".into()
}
