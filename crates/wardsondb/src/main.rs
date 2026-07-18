#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

mod config;
mod engine;
mod error;
mod index;
mod query;
mod schema;
mod server;

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use tracing::{info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

use config::Config;
use engine::storage::Storage;
use server::metrics::Metrics;
use server::{AppState, build_router};

#[tokio::main]
async fn main() {
    let config = Config::parse();

    // Build logging layers. Per-request lines (wardsondb::requests) are
    // opt-in via --verbose on BOTH sinks: an always-on request log grew
    // without bound on dev (multi-GiB wardsondb.log) and silently filled the
    // 256 MB tmpfs in production. The file sink writes through a
    // non-blocking appender so request handling never does synchronous file
    // I/O on a worker thread.
    let base_filter = &config.log_level;
    let make_filter = |verbose: bool| {
        if verbose {
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(base_filter))
        } else {
            EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                EnvFilter::new(format!("{base_filter},wardsondb::requests=off"))
            })
        }
    };

    let terminal_layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(make_filter(config.verbose));

    // File layer: non-panicking open — an unwritable --log-file path must
    // not take the server down; warn and run without file logging instead.
    // The guard must stay alive for the life of the process: dropping it
    // stops the background writer thread and flushes buffered lines.
    let (file_layer, _file_log_guard) = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config.log_file)
    {
        Ok(file) => {
            let (writer, guard) = tracing_appender::non_blocking(file);
            let layer = fmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .with_filter(make_filter(config.verbose));
            (Some(layer), Some(guard))
        }
        Err(e) => {
            eprintln!(
                "WARNING: cannot open log file '{}': {e} — continuing without file logging",
                config.log_file
            );
            (None, None)
        }
    };

    tracing_subscriber::registry()
        .with(terminal_layer)
        .with(file_layer)
        .init();

    // Check file descriptor limits
    check_file_descriptor_limit();

    // Open storage
    let data_dir = Path::new(&config.data_dir);
    std::fs::create_dir_all(data_dir).expect("Failed to create data directory");
    let mem_config = engine::storage::MemoryConfig {
        cache_size: config.cache_size_mb * 1024 * 1024,
        max_write_buffer_size: config.write_buffer_mb * 1024 * 1024,
        max_memtable_size: config.memtable_mb * 1024 * 1024,
        flush_workers: config.flush_workers,
        compaction_workers: config.compaction_workers,
    };
    let storage = Storage::open_with_config(data_dir, &config.storage_engine, mem_config)
        .expect("Failed to open database");
    info!(data_dir = %config.data_dir, engine = storage.engine_name, "Database opened");

    // Configure scan accelerator
    if !config.no_bitmap {
        storage
            .scan_accelerator
            .set_sample_size(config.bitmap_sample_size);

        let bitmap_fields: Vec<String> = config
            .bitmap_fields
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        if !bitmap_fields.is_empty() {
            storage
                .scan_accelerator
                .configure_fields(bitmap_fields.clone());
            storage
                .scan_accelerator
                .set_max_cardinality(config.bitmap_max_cardinality);
            storage
                .scan_accelerator
                .set_max_memory_bytes(resolve_bitmap_memory_limit(config.bitmap_memory_mb));

            // Try loading from disk first — but a snapshot is only served if
            // it agrees with storage on the document count. It is at most
            // one persist-interval stale, and serving a stale one silently
            // drops every doc written after the last persist from bitmap
            // results (S2-2). On any disagreement, rebuild from storage.
            let loaded = storage.scan_accelerator.load_from_disk(data_dir, "_all");
            let consistent = loaded && {
                // Per-collection reconcile (F1 upgraded S2-2's total-count
                // check): every collection's membership must match storage's
                // count, and no membership may outlive a dropped collection.
                let expected: std::collections::HashMap<String, u64> = storage
                    .list_collections()
                    .map(|cols| cols.iter().map(|c| (c.name.clone(), c.doc_count)).collect())
                    .unwrap_or_default();
                let ok = storage.scan_accelerator.snapshot_matches(&expected);
                if !ok {
                    warn!(
                        snapshot_docs = storage.scan_accelerator.positions.len(),
                        storage_docs = expected.values().sum::<u64>(),
                        "Bitmap snapshot disagrees with storage (per-collection); rebuilding"
                    );
                }
                ok
            };
            if consistent {
                storage.scan_accelerator.set_ready(true);
            } else {
                // Sets ready itself once the rebuild (+ delta drain) is done.
                rebuild_all_accelerators(&storage);
            }
            info!(fields = ?bitmap_fields, "Scan accelerator configured");
        }
        // With no explicit fields, the profiler samples inserts and logs a
        // --bitmap-fields recommendation; nothing activates without the flag.
    } else {
        info!("Scan accelerator disabled (--no-bitmap)");
    }

    let metrics = Arc::new(Metrics::new());

    // Load API keys from CLI flags and key file
    let api_keys = load_api_keys(&config);
    if !api_keys.is_empty() {
        info!(count = api_keys.len(), "API key authentication enabled");
    }

    let state = Arc::new(AppState {
        storage,
        config: config.clone(),
        started_at: Instant::now(),
        metrics: metrics.clone(),
        api_keys,
    });

    // Spawn periodic stats reporter (every 10 seconds)
    server::metrics::spawn_stats_reporter(metrics.clone(), 10);

    // Spawn bitmap persistence + compaction task (every 60 seconds).
    //
    // The persist call and `recompute_cached_memory` both do unbounded
    // in-memory work plus blocking `fs::write`, so they run inside
    // `spawn_blocking` to keep them off the async worker pool. The outer
    // interval driver stays on the runtime so the 60s cadence is accurate.
    if !config.no_bitmap {
        let persist_state = state.clone();
        let data_dir_owned = config.data_dir.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(tokio::time::Duration::from_secs(60));
            tick.tick().await; // Skip first immediate tick
            loop {
                tick.tick().await;
                if !persist_state.storage.scan_accelerator.is_ready() {
                    continue;
                }

                let persist_task_state = persist_state.clone();
                let dir = std::path::PathBuf::from(&data_dir_owned);
                let persist_result = tokio::task::spawn_blocking(move || {
                    let result = persist_task_state
                        .storage
                        .scan_accelerator
                        .persist_to_disk(&dir, "_all");
                    persist_task_state
                        .storage
                        .scan_accelerator
                        .recompute_cached_memory();
                    result
                })
                .await;

                match persist_result {
                    Ok(Err(e)) => warn!(error = %e, "Failed to persist scan accelerator"),
                    Err(e) => warn!(error = %e, "Persist task panicked"),
                    Ok(Ok(())) => {}
                }

                // Compact if >25% holes from TTL deletes
                if persist_state.storage.scan_accelerator.needs_compaction() {
                    info!("Bitmap position map has >25% holes, triggering compaction rebuild");
                    let compact_state = persist_state.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        rebuild_all_accelerators(&compact_state.storage);
                    })
                    .await;
                }
            }
        });
    }

    // Spawn TTL cleanup worker
    {
        let state_clone = state.clone();
        let ttl_interval = config.ttl_interval;
        tokio::spawn(async move {
            server::ttl_worker::run_ttl_loop(state_clone, ttl_interval).await;
        });
    }

    let app = build_router(state);

    let addr = format!("0.0.0.0:{}", config.port);

    if config.verbose {
        info!(log_file = %config.log_file, "Verbose mode: per-request logs shown in terminal and written to file");
    } else {
        info!("Per-request logging disabled (enable with --verbose)");
    }

    if config.tls {
        // rustls only auto-selects its CryptoProvider when exactly one provider
        // feature is enabled process-wide. Dev-dependency feature unification
        // (cargo test / bench / --all-targets builds) can compile rustls with a
        // second provider, which panics here at TLS init. Install ours
        // explicitly so the binary never depends on which cargo invocation
        // produced it.
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .expect("rustls CryptoProvider installed twice");

        let (cert_path, key_path) = resolve_tls_paths(&config);
        let scheme = "https";
        info!(addr = %addr, scheme = scheme, cert = %cert_path, "Starting WardSONDB with TLS");

        let tls_config =
            axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert_path, &key_path)
                .await
                .expect("Failed to load TLS certificate/key");

        let bind_addr: std::net::SocketAddr = addr.parse().expect("Invalid bind address");
        axum_server::bind_rustls(bind_addr, tls_config)
            .serve(app.into_make_service())
            .await
            .expect("Server error");
    } else {
        info!(addr = %addr, "Starting WardSONDB");

        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .expect("Failed to bind address");

        axum::serve(listener, app).await.expect("Server error");
    }
}

fn load_api_keys(config: &Config) -> Vec<String> {
    let mut keys: Vec<String> = config.api_keys.clone();

    if let Some(path) = &config.api_key_file {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                for line in contents.lines() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty()
                        && !trimmed.starts_with('#')
                        && !keys.contains(&trimmed.to_string())
                    {
                        keys.push(trimmed.to_string());
                    }
                }
            }
            Err(e) => {
                warn!(path = path, error = %e, "Failed to read API key file");
            }
        }
    }

    keys
}

fn resolve_tls_paths(config: &Config) -> (String, String) {
    if let (Some(cert), Some(key)) = (&config.tls_cert, &config.tls_key) {
        return (cert.clone(), key.clone());
    }

    // Auto-generate self-signed certificate
    let tls_dir = Path::new(&config.data_dir).join("tls");
    let cert_path = tls_dir.join("cert.pem");
    let key_path = tls_dir.join("key.pem");

    // Reuse existing certs if already generated
    if cert_path.exists() && key_path.exists() {
        info!("Reusing existing self-signed certificate");
        return (
            cert_path.to_string_lossy().to_string(),
            key_path.to_string_lossy().to_string(),
        );
    }

    info!("Generating self-signed TLS certificate");
    std::fs::create_dir_all(&tls_dir).expect("Failed to create TLS directory");

    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .expect("Failed to create certificate params");
    params
        .subject_alt_names
        .push(rcgen::SanType::IpAddress(std::net::IpAddr::V4(
            std::net::Ipv4Addr::new(0, 0, 0, 0),
        )));
    params
        .subject_alt_names
        .push(rcgen::SanType::IpAddress(std::net::IpAddr::V4(
            std::net::Ipv4Addr::new(127, 0, 0, 1),
        )));
    // Valid for 365 days
    params.not_after = rcgen::date_time_ymd(2027, 3, 9);

    let key_pair = rcgen::KeyPair::generate().expect("Failed to generate key pair");
    let cert = params
        .self_signed(&key_pair)
        .expect("Failed to generate self-signed certificate");

    std::fs::write(&cert_path, cert.pem()).expect("Failed to write certificate");
    std::fs::write(&key_path, key_pair.serialize_pem()).expect("Failed to write private key");

    info!(
        cert = %cert_path.display(),
        key = %key_path.display(),
        "Self-signed certificate generated (valid 365 days)"
    );

    (
        cert_path.to_string_lossy().to_string(),
        key_path.to_string_lossy().to_string(),
    )
}

/// Rebuild scan accelerator from all existing collections using batched iteration.
/// Peak memory: ~BATCH_SIZE documents instead of all documents.
fn rebuild_all_accelerators(storage: &Storage) {
    const BATCH_SIZE: usize = 10_000;

    let collections = match storage.list_collections() {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "Failed to list collections for accelerator rebuild");
            return;
        }
    };

    // Hooks queue their deltas from here until finish_rebuild drains them —
    // the maps are cleared and re-filled without live writers in them.
    storage.scan_accelerator.begin_rebuild();

    // Re-create columns after the clear
    let fields = storage.scan_accelerator.config_read().bitmap_fields.clone();
    if !fields.is_empty() {
        storage.scan_accelerator.configure_fields(fields);
    }

    let start = std::time::Instant::now();
    let mut total_docs: usize = 0;

    for col in &collections {
        let docs_partition = match storage.get_docs_partition(&col.name) {
            Ok(p) => p,
            Err(e) => {
                warn!(collection = col.name, error = %e, "Skipping collection for rebuild");
                continue;
            }
        };
        use engine::backend::StorageBackend;
        let mut batch: Vec<(String, serde_json::Value)> = Vec::with_capacity(BATCH_SIZE);
        let mut over_budget = false;

        let scan = storage.engine.scan_full(&docs_partition, &mut |_, value| {
            if let Ok(doc) = serde_json::from_slice::<serde_json::Value>(value)
                && let Some(id) = doc.get("_id").and_then(|v| v.as_str())
            {
                batch.push((id.to_string(), doc));
            }
            if batch.len() >= BATCH_SIZE {
                total_docs += batch.len();
                storage.scan_accelerator.rebuild_batch(&col.name, &batch);
                batch.clear();
                if storage.scan_accelerator.is_over_budget() {
                    over_budget = true;
                    return std::ops::ControlFlow::Break(());
                }
            }
            std::ops::ControlFlow::Continue(())
        });
        if let Err(e) = scan {
            warn!(collection = col.name, error = ?e, "Skipping collection for rebuild");
            continue;
        }
        if over_budget {
            info!(
                docs_indexed = total_docs,
                "Bitmap rebuild stopped early: memory budget exceeded"
            );
        }
        if !batch.is_empty() {
            total_docs += batch.len();
            storage.scan_accelerator.rebuild_batch(&col.name, &batch);
        }
    }

    info!(
        docs = total_docs,
        elapsed_ms = start.elapsed().as_millis(),
        "Scan accelerator rebuilt (batched)"
    );
    storage.scan_accelerator.finish_rebuild();
}

/// Check the OS file descriptor limit and warn if too low.
/// fjall opens file handles for each SST segment and will hit "Too many open files"
/// past ~900K documents if the limit is too low (macOS default: 256, Linux: 1024).
fn check_file_descriptor_limit() {
    const MIN_RECOMMENDED: u64 = 4096;

    let mut rlim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };

    let ret = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) };
    if ret != 0 {
        warn!("Could not check file descriptor limit (getrlimit failed)");
        return;
    }

    let current = rlim.rlim_cur;
    let max = rlim.rlim_max;

    info!(
        current = current,
        max = max,
        "File descriptor limit (ulimit -n)"
    );

    if current < MIN_RECOMMENDED {
        // Attempt to raise to min(max, MIN_RECOMMENDED)
        let target = if max >= MIN_RECOMMENDED || max == libc::RLIM_INFINITY {
            MIN_RECOMMENDED
        } else {
            max
        };

        let new_rlim = libc::rlimit {
            rlim_cur: target,
            rlim_max: max,
        };
        let raise_ret = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &new_rlim) };
        if raise_ret == 0 && target >= MIN_RECOMMENDED {
            info!(
                from = current,
                to = target,
                "Raised file descriptor limit automatically"
            );
        } else {
            warn!(
                current = current,
                recommended = MIN_RECOMMENDED,
                "Low file descriptor limit — fjall may crash with 'Too many open files' \
                 at large document counts. Fix: ulimit -n 65536"
            );
        }
    }
}

/// Resolve the bitmap memory limit from the CLI flag.
/// 0 = auto: min(4GB, 10% of system RAM).
fn resolve_bitmap_memory_limit(configured_mb: u64) -> u64 {
    if configured_mb > 0 {
        let bytes = configured_mb * 1024 * 1024;
        info!(
            bitmap_memory_mb = configured_mb,
            "Bitmap memory budget set (explicit)"
        );
        return bytes;
    }
    let four_gb: u64 = 4 * 1024 * 1024 * 1024;
    let budget = match system_ram_bytes() {
        Some(ram) => std::cmp::min(four_gb, ram / 10),
        None => four_gb,
    };
    info!(
        bitmap_memory_mb = budget / (1024 * 1024),
        "Bitmap memory budget set (auto)"
    );
    budget
}

/// Detect total system RAM in bytes.
fn system_ram_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        use std::mem;
        let mut size: u64 = 0;
        let mut len = mem::size_of::<u64>();
        let mut mib: [libc::c_int; 2] = [libc::CTL_HW, libc::HW_MEMSIZE];
        let ret = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                &mut size as *mut u64 as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if ret == 0 && size > 0 {
            return Some(size);
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(contents) = std::fs::read_to_string("/proc/meminfo") {
            for line in contents.lines() {
                if let Some(rest) = line.strip_prefix("MemTotal:") {
                    let rest = rest.trim();
                    if let Some(kb_str) = rest.strip_suffix("kB")
                        && let Ok(kb) = kb_str.trim().parse::<u64>()
                    {
                        return Some(kb * 1024);
                    }
                }
            }
        }
    }

    None
}
