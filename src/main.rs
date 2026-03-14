use anyhow::Result;
use tracing::{error, info};

mod brain;
mod config;
mod db;
mod learning;
mod proactive;
mod sessions;
mod terminal;
mod tools;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("embraos=info".parse()?)
                .add_directive("wardsondb=warn".parse()?),
        )
        .with_target(false)
        .init();

    tools::init_start_time();

    info!("embraOS Phase 0 v{}", env!("CARGO_PKG_VERSION"));

    // 1. Start WardSONDB
    let db_process = match db::start_wardsondb().await {
        Ok(proc) => proc,
        Err(e) => {
            error!("Failed to start WardSONDB: {}", e);
            eprintln!("ERROR: Failed to start WardSONDB: {}", e);
            eprintln!("Make sure 'wardsondb' is in your PATH.");
            std::process::exit(1);
        }
    };

    let db = db_process.client;
    // Keep child process alive by holding it
    let _db_child = db_process.child;

    // 2. Check if this is first run (no soul documents exist)
    let is_first_run = !db
        .collection_exists("soul.invariant")
        .await
        .unwrap_or(false);

    // 3. If first run: config wizard → learning mode
    if is_first_run {
        let cfg = config::run_config_wizard().await?;
        config::save_config(&db, &cfg).await?;
        learning::run_learning_mode(&db, &cfg).await?;
    }

    // 4. Load configuration
    let cfg = config::load_config(&db).await?;
    info!("{} loaded, entering operational mode", cfg.name);

    // 5. Start proactive engine (background task)
    let notification_rx = proactive::start_proactive_engine(&db);

    // 6. Enter conversational terminal (main loop)
    terminal::run_terminal(&db, &cfg, notification_rx).await?;

    info!("embraOS shutting down gracefully");
    Ok(())
}
