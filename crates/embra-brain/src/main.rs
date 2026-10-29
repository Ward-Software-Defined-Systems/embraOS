//! embra-brain — Headless AI runtime service.
//!
//! - The providers: Anthropic, Gemini, and Ollama and LM Studio over the
//!   OpenAI-compatible wire (`provider/`)
//! - The tool registry and its dispatcher (`tools/registry.rs`)
//! - Session manager
//! - Proactive engine
//! - WardSONDB client
//! - Schema migrations
//! - Learning Mode
//!
//! Exposes all functionality via gRPC on port 50002.

mod activity;
mod tool_calls;
mod brain;
mod tools;
mod db;
mod sessions;
mod proactive;
mod migrations;
mod config;
mod learning;
mod knowledge;
mod identity_graph;
mod grpc_service;
mod provider;
mod setup;
mod guardian;
mod media;
mod embedding;
mod net;
mod secrets;

use grpc_service::BrainGrpcService;

use embra_common::proto::brain::brain_service_server::BrainServiceServer;
use tonic::transport::Server;
use tracing::{info, warn, debug, error};

/// Log filtering via `EMBRA_LOG` (2026-07-31): env-filter-style Targets
/// specs — "info", "debug", "info,kg::traversal=debug". Parsed with
/// `filter::Targets` (in tracing-subscriber's DEFAULT features — do not
/// enable the env-filter crate feature for this; it drags in new deps and
/// rotates the lockfile). Absent/invalid → the historical INFO floor,
/// byte-identical default behavior. A spec with only per-target entries
/// gets an INFO default added so it never silences the rest of the crate.
/// Appliance surface: the `embra.loglevel=` kernel flag → embrad forwards
/// it as this env on the brain child; readable back via `system_logs`.
fn init_tracing() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::Layer;

    let spec = std::env::var("EMBRA_LOG").unwrap_or_default();
    let mut targets = match spec.trim() {
        "" => tracing_subscriber::filter::Targets::new()
            .with_default(tracing::level_filters::LevelFilter::INFO),
        s => s.parse().unwrap_or_else(|e| {
            eprintln!("[embra-brain] invalid EMBRA_LOG '{s}' ({e}); falling back to info");
            tracing_subscriber::filter::Targets::new()
                .with_default(tracing::level_filters::LevelFilter::INFO)
        }),
    };
    if targets.default_level().is_none() {
        targets = targets.with_default(tracing::level_filters::LevelFilter::INFO);
    }
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(targets))
        .init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    // Parse service args through the one flag parser of the services
    // (`embra_common::args`, #41); a flag without its value stops the brain.
    let flags = embra_common::args::Flags::parse(
        std::env::args().skip(1),
        &["--port", "--wardsondb-url", "--api-key", "--api-provider", "--github-token"],
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let port: u16 = flags.value("--port").map_or(50002, |v| v.parse().expect("Invalid port"));
    let wardsondb_url = flags.value("--wardsondb-url").unwrap_or("http://127.0.0.1:8090").to_string();
    let api_key: Option<String> = flags.value("--api-key").map(str::to_string);
    if let Some(provider) = flags.value("--api-provider") {
        // Sprint 4: persist provider selection to a process
        // env var so handle_request / run_learning_loop can
        // read it without threading another parameter.
        // SAFETY: called once at startup before tokio runtime is multi-threaded
        unsafe { std::env::set_var("EMBRA_PROVIDER", provider); }
        info!("API provider configured: {}", provider);
    }
    if let Some(token) = flags.value("--github-token") {
        // SAFETY: called once at startup before tokio runtime is multi-threaded
        unsafe { std::env::set_var("GITHUB_TOKEN", token); }
        info!("GitHub token configured from --github-token");
    }

    // API key: --api-key flag > ANTHROPIC_API_KEY env var > WardSONDB config.
    // OpenAI-compat presets (Ollama / LM Studio) don't use api_key — bearer
    // is plumbed via EMBRA_OLLAMA_BEARER / EMBRA_LM_STUDIO_BEARER env vars
    // set by the supervisor at boot. Skip the missing-key warning for those.
    let mut api_key = api_key
        .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
        .unwrap_or_default();
    let active_provider = std::env::var("EMBRA_PROVIDER").unwrap_or_default();
    let is_openai_compat = matches!(active_provider.as_str(), "ollama" | "lm_studio");
    if api_key.is_empty() {
        if is_openai_compat {
            let bearer_env = match active_provider.as_str() {
                "ollama" => "EMBRA_OLLAMA_BEARER",
                "lm_studio" => "EMBRA_LM_STUDIO_BEARER",
                _ => unreachable!(),
            };
            let bearer_present = std::env::var(bearer_env).map(|s| !s.is_empty()).unwrap_or(false);
            info!(
                "OpenAI-compat provider '{}' configured (bearer {})",
                active_provider,
                if bearer_present { "present" } else { "absent" }
            );
        } else {
            warn!("No API key provided (--api-key or ANTHROPIC_API_KEY). Brain calls will fail until config wizard runs.");
        }
    } else {
        info!("API key configured ({} characters)", api_key.chars().count());
    }

    info!("embra-brain starting on port {}", port);

    // Initialize start time for uptime tracking
    tools::init_start_time();

    // Initialize WardSONDB client
    let db = db::WardsonDbClient::from_url(&wardsondb_url);

    // Wait for WardSONDB to be ready
    info!("Waiting for WardSONDB...");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if db.health().await.unwrap_or(false) {
            break;
        }
        if std::time::Instant::now() > deadline {
            anyhow::bail!("WardSONDB did not become healthy within 30s");
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    info!("WardSONDB is ready");

    // Run schema migrations
    migrations::run_migrations(&db).await?;
    info!("Migrations complete");

    // Write the native tool-use registry snapshot to tools.registry. Must
    // run after migrations (v7 stamps legacy session format_version) and
    // before the gRPC server accepts connections. Idempotent on every boot.
    match tools::registry::write_snapshot(&db).await {
        Ok(_) => info!(
            "Tools registry snapshot written ({} tools)",
            tools::registry::tool_count()
        ),
        Err(e) => error!("Failed to write tools.registry snapshot: {e}"),
    }

    // Media wave: the attachment/image store lives in the workspace
    // (bind-mounted from DATA). Created at boot so `/attach list` and the
    // store's usage line are meaningful before the first upload; every
    // write path also ensures it (dev runs without the mount).
    {
        let store = media::MediaStore::default_store();
        match store.ensure_dir().await {
            Ok(()) => info!("Media store ready at {}", store.dir().display()),
            Err(e) => warn!("Media store directory unavailable ({e}); uploads will retry on first use"),
        }
    }

    // embra-guardian-v1: init the dynamic-tool runtime overlay and load
    // previously-built artifacts (DATA persists). Ready tools whose
    // toolchain matches are recompiled into the sandbox; missing/stale
    // ones are logged for re-define. Never blocks boot on a build.
    if let Err(e) = guardian::reconcile_on_boot(&db).await {
        error!("guardian reconcile failed (dynamic tools unavailable this boot): {e}");
    }

    // Set HOME to writable workspace so git/ssh config persists on DATA partition
    // SAFETY: called once at startup before multi-threaded work begins
    unsafe { std::env::set_var("HOME", "/embra/workspace"); }

    // Git's global configuration lives under STATE (`GIT_CONFIG_GLOBAL`),
    // out of the tools' reach: the directories, the defaults, and the
    // identity a legacy workspace `.gitconfig` holds, adopted once.
    tools::engineering::git::prepare_git_config().await;
    debug!("Git global config prepared under {}", tools::engineering::git::GIT_CONFIG_DIR);

    // Load config (or default for first run)
    // Also try to get API key from WardSONDB config if not provided via CLI/env
    let config_tz = match config::load_config(&db).await {
        Ok(cfg) => {
            if api_key.is_empty() && !cfg.api_key.is_empty() {
                info!("Loaded API key from WardSONDB config");
                api_key = cfg.api_key.clone();
            }
            // Resolve abbreviations (PDT → America/Los_Angeles) so chrono-tz can parse
            tools::resolve_timezone(&cfg.timezone)
        }
        Err(_) => "Etc/UTC".to_string(),
    };

    // Set TZ env var so child processes (git, ssh) and libraries respect the timezone
    // SAFETY: called once at startup before multi-threaded work begins
    unsafe { std::env::set_var("TZ", &config_tz); }

    // Ensure /embra/state/timezone exists (migration: backups from before this was persisted)
    let tz_state_path = "/embra/state/timezone";
    if !std::path::Path::new(tz_state_path).exists() && config_tz != "Etc/UTC" {
        if let Err(e) = std::fs::write(tz_state_path, &config_tz) {
            warn!("Could not write timezone to STATE: {}", e);
        } else {
            info!("Migrated timezone to {}: {}", tz_state_path, config_tz);
        }
    }

    // The tools the image ships (`embra_guardian::shipped`): installed,
    // updated or adopted in the background, one build at a time. After
    // the set_var calls above, which assume a single thread; never blocks
    // boot.
    guardian::ensure_shipped_tools(db.clone());

    // Start proactive engine
    let proactive_rx = proactive::start_proactive_engine(&db, &config_tz, &api_key);

    // Create the gRPC service
    let service = BrainGrpcService::new(db, config_tz, api_key, proactive_rx);

    let addr = format!("0.0.0.0:{}", port).parse()?;
    info!("embra-brain listening on {}", addr);

    Server::builder()
        .add_service(
            // PutMedia requests carry raw image bytes (≤ MEDIA_UPLOAD_MAX).
            BrainServiceServer::new(service)
                .max_decoding_message_size(embra_common::GRPC_MAX_MESSAGE_BYTES),
        )
        .serve(addr)
        .await?;

    Ok(())
}
