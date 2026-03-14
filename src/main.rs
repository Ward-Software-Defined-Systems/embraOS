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
    // Initialize tracing (log to file to avoid polluting TUI)
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("embraos=info".parse()?)
                .add_directive("wardsondb=warn".parse()?),
        )
        .with_target(false)
        .with_writer(|| {
            // Write logs to stderr so they don't interfere with TUI
            std::io::stderr()
        })
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

    // 3. Start proactive engine (background task)
    let notification_rx = proactive::start_proactive_engine(&db);

    // 4. Enter TUI — handles setup, learning, and operational modes
    terminal::run_terminal(&db, is_first_run, notification_rx).await?;

    info!("embraOS shutting down gracefully");
    Ok(())
}
