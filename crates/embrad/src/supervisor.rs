//! Service supervisor for embrad.
//!
//! Manages the lifecycle of all embraOS services:
//! - Start in dependency order
//! - Health check polling
//! - Restart on failure with exponential backoff
//! - Graceful shutdown in reverse order

use anyhow::{Result, bail};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tracing::{info, warn, error};

/// Read embra.cols and embra.rows from /proc/cmdline (set by run-qemu.sh)
fn read_terminal_size_from_cmdline() -> (u16, u16) {
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let mut cols = 80u16;
    let mut rows = 24u16;
    for param in cmdline.split_whitespace() {
        if let Some(val) = param.strip_prefix("embra.cols=") {
            cols = val.parse().unwrap_or(80);
        }
        if let Some(val) = param.strip_prefix("embra.rows=") {
            rows = val.parse().unwrap_or(24);
        }
    }
    info!("Terminal size from cmdline: {}x{}", cols, rows);
    (cols, rows)
}

/// Desktop mode detection. True when the kernel cmdline carries
/// `embra.desktop=1` (set by `EMBRA_DESKTOP=1 ./scripts/run-qemu.sh`).
/// In that mode the supervisor spawns `embra-comp` + `embra-desktop`
/// instead of the serial-TTY `embra-console`.
fn is_desktop_mode() -> bool {
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let from_cmdline = cmdline
        .split_whitespace()
        .any(|p| p == "embra.desktop=1");
    let binary_present = std::path::Path::new("/sbin/embra-comp").exists()
        || std::path::Path::new("/usr/bin/embra-comp").exists();
    let mode = from_cmdline && binary_present;
    info!(
        "Desktop mode: {} (cmdline={}, binary_present={})",
        mode, from_cmdline, binary_present
    );
    mode
}

/// Service definition
#[derive(Clone)]
pub struct ServiceDef {
    pub name: String,
    pub binary: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub health_check: HealthCheck,
    pub depends_on: Vec<String>,
    pub restart_policy: RestartPolicy,
}

#[derive(Clone)]
pub enum HealthCheck {
    /// HTTP GET to a URL, expect 200
    Http { url: String, timeout: Duration },
    /// gRPC health check on a port
    Grpc { port: u16, timeout: Duration },
    /// Just check if the process is alive
    ProcessAlive,
    /// Custom: wait for a file to appear
    FileExists { path: String, timeout: Duration },
}

#[derive(Clone)]
pub struct RestartPolicy {
    pub max_restarts: u32,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_restarts: 10,
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
        }
    }
}

/// Running service state
struct ServiceState {
    def: ServiceDef,
    child: Option<Child>,
    pid: Option<u32>,
    started_at: Option<Instant>,
    restart_count: u32,
    status: ServiceStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServiceStatus {
    Stopped,
    Starting,
    Running,
    Failed(String),
    Halted, // Soul verification failed — do not restart
}

pub struct Supervisor {
    services: Vec<ServiceState>,
    service_order: Vec<String>, // Names in start order
}

impl Supervisor {
    pub fn new() -> Self {
        Self {
            services: Vec::new(),
            service_order: Vec::new(),
        }
    }

    /// Register all embraOS services in dependency order.
    pub fn register_services(&mut self) {
        // Set TZ for all child processes (read from STATE, written by config wizard)
        let tz = std::fs::read_to_string("/embra/state/timezone")
            .unwrap_or_default()
            .trim().to_string();
        if !tz.is_empty() {
            // SAFETY: called once at startup before services are spawned
            unsafe { std::env::set_var("TZ", &tz); }
            tracing::info!("System timezone set: TZ={}", tz);
        }
        // Storage engine baked at build time via scripts/build-image.sh --storage-engine.
        // Falls back to rocksdb for dev builds (`cargo check`/`cargo run`) that bypass
        // the build script. See crates/embrad/build.rs.
        let storage_engine: &'static str =
            option_env!("EMBRA_STORAGE_ENGINE").unwrap_or("rocksdb");
        info!("WardSONDB storage engine: {}", storage_engine);
        // 1. WardSONDB — no dependencies
        self.add_service(ServiceDef {
            name: "wardsondb".to_string(),
            binary: "/usr/bin/wardsondb".to_string(),
            args: vec![
                "--storage-engine".to_string(), storage_engine.to_string(),
                "--port".to_string(), "8090".to_string(),
                "--data-dir".to_string(), "/embra/data/wardsondb".to_string(),
                "--log-file".to_string(), "/embra/ephemeral/wardsondb.log".to_string(),
            ],
            env: vec![],
            health_check: HealthCheck::Http {
                url: "http://127.0.0.1:8090/_health".to_string(),
                timeout: Duration::from_secs(30),
            },
            depends_on: vec![],
            restart_policy: RestartPolicy::default(),
        });

        // 2. embra-trustd — depends on wardsondb
        self.add_service(ServiceDef {
            name: "embra-trustd".to_string(),
            binary: "/usr/bin/embra-trustd".to_string(),
            args: vec![
                "--port".to_string(), "50001".to_string(),
                "--state-dir".to_string(), "/embra/state".to_string(),
                "--wardsondb-url".to_string(), "http://127.0.0.1:8090".to_string(),
            ],
            env: vec![],
            health_check: HealthCheck::Grpc {
                port: 50001,
                timeout: Duration::from_secs(15),
            },
            depends_on: vec!["wardsondb".to_string()],
            restart_policy: RestartPolicy::default(),
        });

        // 3. embra-apid — depends on embra-trustd
        self.add_service(ServiceDef {
            name: "embra-apid".to_string(),
            binary: "/usr/bin/embra-apid".to_string(),
            args: vec![
                "--grpc-port".to_string(), "50000".to_string(),
                "--rest-port".to_string(), "8443".to_string(),
                "--brain-addr".to_string(), "http://127.0.0.1:50002".to_string(),
                "--trust-addr".to_string(), "http://127.0.0.1:50001".to_string(),
            ],
            env: vec![],
            health_check: HealthCheck::Grpc {
                port: 50000,
                timeout: Duration::from_secs(15),
            },
            depends_on: vec!["embra-trustd".to_string()],
            restart_policy: RestartPolicy::default(),
        });

        // 4. embra-brain — depends on wardsondb, embra-apid
        // Provider selection (Sprint 4): STATE file wins over env var
        // (persisted config is authoritative). When neither is set,
        // default to Anthropic for backward compat.
        let api_provider = std::fs::read_to_string("/embra/state/api_provider")
            .or_else(|_| std::env::var("EMBRA_PROVIDER"))
            .unwrap_or_default()
            .trim().to_string();

        // API key: prefer the per-provider STATE file (D2). Falls
        // back to the legacy /embra/state/api_key (mirrors active),
        // then to {ANTHROPIC,GEMINI}_API_KEY env.
        // OpenAI-compat presets don't have an api_key; bearer is read
        // separately below and threaded via EMBRA_*_BEARER env vars.
        let per_provider_path = match api_provider.as_str() {
            "gemini" => "/embra/state/api_key_gemini",
            "ollama" | "lm_studio" => "", // no api_key for OpenAI-compat
            _ => "/embra/state/api_key_anthropic",
        };
        let env_key_name = match api_provider.as_str() {
            "gemini" => "GEMINI_API_KEY",
            "ollama" | "lm_studio" => "", // no api_key env for OpenAI-compat
            _ => "ANTHROPIC_API_KEY",
        };
        let api_key = if per_provider_path.is_empty() {
            String::new()
        } else {
            std::fs::read_to_string(per_provider_path)
                .or_else(|_| std::fs::read_to_string("/embra/state/api_key"))
                .or_else(|_| std::env::var(env_key_name))
                .unwrap_or_default()
                .trim().to_string()
        };
        let mut brain_args = vec![
            "--port".to_string(), "50002".to_string(),
            "--wardsondb-url".to_string(), "http://127.0.0.1:8090".to_string(),
        ];
        if !api_key.is_empty() {
            brain_args.push("--api-key".to_string());
            brain_args.push(api_key);
        }
        if !api_provider.is_empty() {
            brain_args.push("--api-provider".to_string());
            brain_args.push(api_provider.clone());
        }
        // GitHub token: read from STATE partition for boot propagation
        let github_token = std::fs::read_to_string("/embra/state/github_token")
            .unwrap_or_default()
            .trim().to_string();
        if !github_token.is_empty() {
            brain_args.push("--github-token".to_string());
            brain_args.push(github_token);
        }

        // Sprint 5: read OpenAI-compat bearers from STATE files (preferred)
        // or env-var fallback, thread to brain via EMBRA_*_BEARER env vars.
        // STATE wins over env per Locked Decision #8 precedence rules.
        // Read for BOTH presets unconditionally so post-`/provider`-swap
        // turns find the bearer they need without a brain restart.
        let mut brain_env: Vec<(String, String)> = Vec::new();
        let ollama_bearer = std::fs::read_to_string("/embra/state/bearer_ollama")
            .or_else(|_| std::env::var("EMBRA_OLLAMA_BEARER"))
            .unwrap_or_default()
            .trim()
            .to_string();
        if !ollama_bearer.is_empty() {
            brain_env.push(("EMBRA_OLLAMA_BEARER".to_string(), ollama_bearer));
        }
        let lm_studio_bearer = std::fs::read_to_string("/embra/state/bearer_lm_studio")
            .or_else(|_| std::env::var("EMBRA_LM_STUDIO_BEARER"))
            .unwrap_or_default()
            .trim()
            .to_string();
        if !lm_studio_bearer.is_empty() {
            brain_env.push(("EMBRA_LM_STUDIO_BEARER".to_string(), lm_studio_bearer));
        }

        self.add_service(ServiceDef {
            name: "embra-brain".to_string(),
            binary: "/usr/bin/embra-brain".to_string(),
            args: brain_args,
            env: brain_env,
            health_check: HealthCheck::Grpc {
                port: 50002,
                timeout: Duration::from_secs(30),
            },
            depends_on: vec!["wardsondb".to_string(), "embra-apid".to_string()],
            restart_policy: RestartPolicy::default(),
        });

        // 5. Operator interface — desktop (embra-comp + embra-desktop) or
        // serial TUI (embra-console), based on kernel cmdline + binary
        // presence. Both paths depend on embra-brain healthy.
        if is_desktop_mode() {
            // 5a. embra-comp — Wayland kiosk compositor. Owns /dev/tty1,
            // /dev/dri/card0, /dev/input/*. Health check: readiness
            // sentinel file written after globals are advertised.
            self.add_service(ServiceDef {
                name: "embra-comp".to_string(),
                binary: "/sbin/embra-comp".to_string(),
                args: vec![
                    "--ready-sentinel".to_string(),
                    "/run/embra-comp.ready".to_string(),
                ],
                env: vec![
                    ("XDG_RUNTIME_DIR".to_string(), "/run/user/0".to_string()),
                ],
                health_check: HealthCheck::FileExists {
                    path: "/run/embra-comp.ready".to_string(),
                    timeout: Duration::from_secs(20),
                },
                depends_on: vec!["embra-brain".to_string()],
                restart_policy: RestartPolicy::default(),
            });

            // 5b. embra-desktop — iced GUI client. Connects to apid via
            // gRPC, renders against embra-comp's wayland-0 socket.
            self.add_service(ServiceDef {
                name: "embra-desktop".to_string(),
                binary: "/usr/bin/embra-desktop".to_string(),
                args: vec![
                    "--apid-addr".to_string(),
                    "http://127.0.0.1:50000".to_string(),
                ],
                env: vec![
                    ("XDG_RUNTIME_DIR".to_string(), "/run/user/0".to_string()),
                    ("WAYLAND_DISPLAY".to_string(), "wayland-0".to_string()),
                ],
                health_check: HealthCheck::ProcessAlive,
                depends_on: vec!["embra-comp".to_string()],
                restart_policy: RestartPolicy::default(),
            });
        } else {
            // 5. embra-console — depends on embra-brain
            // Read terminal size from kernel cmdline (set by run-qemu.sh from host terminal)
            let (term_cols, term_rows) = read_terminal_size_from_cmdline();
            self.add_service(ServiceDef {
                name: "embra-console".to_string(),
                binary: "/usr/bin/embra-console".to_string(),
                args: vec![
                    "--apid-addr".to_string(),
                    "http://127.0.0.1:50000".to_string(),
                    "--device".to_string(),
                    "/dev/ttyS0".to_string(),
                    "--columns".to_string(),
                    term_cols.to_string(),
                    "--rows".to_string(),
                    term_rows.to_string(),
                ],
                env: vec![],
                health_check: HealthCheck::ProcessAlive,
                depends_on: vec!["embra-brain".to_string()],
                restart_policy: RestartPolicy::default(),
            });
        }
    }

    fn add_service(&mut self, def: ServiceDef) {
        let name = def.name.clone();
        self.services.push(ServiceState {
            def,
            child: None,
            pid: None,
            started_at: None,
            restart_count: 0,
            status: ServiceStatus::Stopped,
        });
        self.service_order.push(name);
    }

    /// Start all services in dependency order.
    /// After embra-trustd starts, verifies the soul. HALTs if verification fails.
    pub async fn start_all(&mut self) -> Result<()> {
        for i in 0..self.services.len() {
            let name = self.services[i].def.name.clone();
            info!("Starting service: {}", name);

            self.start_service(i).await?;

            // After spawning the operator-interface service, redirect
            // embrad's stdio to a log file so the interface (TUI on
            // serial OR compositor on /dev/tty1) gets clean control of
            // its surface. embra-comp takes /dev/tty1 to KD_GRAPHICS;
            // embra-console owns the serial line.
            if (name == "embra-console" || name == "embra-comp")
                && std::process::id() == 1
            {
                info!("Redirecting embrad output to log file for {}", name);
                if let Ok(log) = std::fs::File::create("/embra/ephemeral/embrad.log") {
                    use std::os::unix::io::AsRawFd;
                    let fd = log.as_raw_fd();
                    unsafe {
                        libc::dup2(fd, 1); // stdout
                        libc::dup2(fd, 2); // stderr
                    }
                    std::mem::forget(log);
                }
            }

            // After embra-trustd is up, verify the soul
            if name == "embra-trustd" {
                self.verify_soul().await?;
            }
        }
        Ok(())
    }

    async fn start_service(&mut self, index: usize) -> Result<()> {
        let svc = &mut self.services[index];
        svc.status = ServiceStatus::Starting;

        let mut cmd = Command::new(&svc.def.binary);
        cmd.args(&svc.def.args);
        for (key, val) in &svc.def.env {
            cmd.env(key, val);
        }
        // The operator-interface service inherits stdin/stdout —
        // embra-console on serial, embra-comp on /dev/tty1.
        // stderr goes to log file to prevent embrad log bleed-through.
        if svc.def.name == "embra-console" || svc.def.name == "embra-comp" {
            cmd.stdin(Stdio::inherit());
            cmd.stdout(Stdio::inherit());
            let log_path = format!("/embra/ephemeral/{}.log", svc.def.name);
            let log_file = std::fs::File::create(&log_path)
                .unwrap_or_else(|_| std::fs::File::create("/dev/null").unwrap());
            cmd.stderr(Stdio::from(log_file));
        } else {
            let log_path = format!("/embra/ephemeral/{}.log", svc.def.name);
            let log_file = std::fs::File::create(&log_path)
                .unwrap_or_else(|_| std::fs::File::create("/dev/null").unwrap());
            let log_file2 = log_file.try_clone().unwrap_or_else(|_| std::fs::File::create("/dev/null").unwrap());
            cmd.stdout(Stdio::from(log_file));
            cmd.stderr(Stdio::from(log_file2));
        }
        cmd.kill_on_drop(true);

        let child = cmd.spawn().map_err(|e| {
            let msg = format!("Failed to spawn {}: {}", svc.def.name, e);
            svc.status = ServiceStatus::Failed(msg.clone());
            anyhow::anyhow!(msg)
        })?;

        let pid = child.id();
        svc.child = Some(child);
        svc.pid = pid;
        svc.started_at = Some(Instant::now());

        info!("Spawned {} (pid={:?})", svc.def.name, pid);

        // Give the process a moment to start (or crash)
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Check if it already exited (crash on startup)
        if let Some(ref mut child) = svc.child {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let name = svc.def.name.clone();
                    let msg = format!("{} exited immediately with status: {:?}", name, status);
                    // stdout+stderr were redirected to the log file — read its tail
                    // via eprintln! so it lands on the serial console before halt.
                    eprintln!("[embrad] DIAG: {}", msg);
                    let log_path = format!("/embra/ephemeral/{}.log", name);
                    match std::fs::read_to_string(&log_path) {
                        Ok(log_content) if !log_content.is_empty() => {
                            eprintln!("[embrad] DIAG: {} log tail:", name);
                            for line in log_content.lines().rev().take(30).collect::<Vec<_>>().into_iter().rev() {
                                eprintln!("  {}", line);
                            }
                        }
                        Ok(_) => {
                            eprintln!("[embrad] DIAG: {} log file is empty — service produced no output", name);
                        }
                        Err(log_err) => {
                            eprintln!("[embrad] DIAG: {} could not read log file {}: {}", name, log_path, log_err);
                        }
                    }
                    error!("{}", msg);
                    svc.status = ServiceStatus::Failed(msg.clone());
                    svc.child = None;
                    return Err(anyhow::anyhow!(msg));
                }
                Ok(None) => {} // Still running, good
                Err(e) => {
                    warn!("Could not check {} status: {}", svc.def.name, e);
                }
            }
        }

        // Wait for health check
        if let Err(e) = self.wait_for_health(index).await {
            let name = self.services[index].def.name.clone();
            // Use eprintln! for guaranteed serial output (tracing may buffer)
            eprintln!("[embrad] DIAG: {} health check failed: {}", name, e);

            // Dump the service log for debugging
            let log_path = format!("/embra/ephemeral/{}.log", name);
            match std::fs::read_to_string(&log_path) {
                Ok(log_content) if !log_content.is_empty() => {
                    eprintln!("[embrad] DIAG: {} log tail:", name);
                    for line in log_content.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev() {
                        eprintln!("  {}", line);
                    }
                }
                Ok(_) => {
                    eprintln!("[embrad] DIAG: {} log file is empty — service produced no output", name);
                }
                Err(log_err) => {
                    eprintln!("[embrad] DIAG: {} could not read log file {}: {}", name, log_path, log_err);
                }
            }
            // Also check if process is still alive
            if let Some(ref mut child) = self.services[index].child {
                match child.try_wait() {
                    Ok(Some(status)) => eprintln!("[embrad] DIAG: {} process exited with: {:?}", name, status),
                    Ok(None) => eprintln!("[embrad] DIAG: {} process is still running but not responding on health endpoint", name),
                    Err(e2) => eprintln!("[embrad] DIAG: {} could not check process status: {}", name, e2),
                }
            }
            return Err(e);
        }

        self.services[index].status = ServiceStatus::Running;
        info!("Service {} is healthy", self.services[index].def.name);
        Ok(())
    }

    async fn wait_for_health(&self, index: usize) -> Result<()> {
        let svc = &self.services[index];
        let check = &svc.def.health_check;

        match check {
            HealthCheck::Http { url, timeout } => {
                let deadline = Instant::now() + *timeout;

                // Parse host:port from URL for raw TCP health check
                // Avoids reqwest/rustls issues in minimal rootfs
                let addr = url.replace("http://", "").split('/').next().unwrap_or("127.0.0.1:8090").to_string();
                let path = url.find("/_").map(|i| &url[i..]).unwrap_or("/_health");

                while Instant::now() < deadline {
                    // Try raw HTTP request over TCP
                    match tokio::net::TcpStream::connect(&addr).await {
                        Ok(mut stream) => {
                            use tokio::io::{AsyncWriteExt, AsyncReadExt};
                            let req = format!("GET {} HTTP/1.0\r\nHost: {}\r\n\r\n", path, addr);
                            if stream.write_all(req.as_bytes()).await.is_ok() {
                                let mut buf = vec![0u8; 4096];
                                if let Ok(n) = stream.read(&mut buf).await {
                                    let response = String::from_utf8_lossy(&buf[..n]);
                                    if response.contains("200") {
                                        return Ok(());
                                    }
                                }
                            }
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }
                        Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
                    }
                }
                bail!("{} failed health check (HTTP {} did not respond within {:?})",
                    svc.def.name, url, timeout);
            }
            HealthCheck::Grpc { port, timeout } => {
                let deadline = Instant::now() + *timeout;

                while Instant::now() < deadline {
                    // Simple TCP connect check — full gRPC health check can come later
                    match tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port)).await {
                        Ok(_) => return Ok(()),
                        Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
                    }
                }
                bail!("{} failed health check (gRPC port {} did not open within {:?})",
                    svc.def.name, port, timeout);
            }
            HealthCheck::ProcessAlive => {
                // Just check the process exists
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(())
            }
            HealthCheck::FileExists { path, timeout } => {
                let deadline = Instant::now() + *timeout;
                while Instant::now() < deadline {
                    if std::path::Path::new(path).exists() {
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                bail!("{} failed health check (file {} not created within {:?})",
                    svc.def.name, path, timeout);
            }
        }
    }

    /// Verify soul via embra-trustd gRPC.
    /// HALTs the system if soul verification fails.
    async fn verify_soul(&self) -> Result<()> {
        info!("Verifying soul via embra-trustd...");

        // Connect to embra-trustd and call VerifySoul
        use embra_common::proto::trust::trust_service_client::TrustServiceClient;
        use embra_common::proto::trust::VerifySoulRequest;

        let mut client = TrustServiceClient::connect("http://127.0.0.1:50001")
            .await
            .map_err(|e| anyhow::anyhow!("Failed to connect to embra-trustd: {}", e))?;

        let response = client.verify_soul(VerifySoulRequest {
            expected_hash: String::new(),
        }).await.map_err(|e| anyhow::anyhow!("Soul verification RPC failed: {}", e))?;

        let result = response.into_inner();

        if result.valid {
            info!("Soul verification PASSED (hash={})", result.computed_hash);
            Ok(())
        } else {
            let reason = if result.error.is_empty() {
                format!("Soul hash mismatch: computed={}, stored={}",
                    result.computed_hash, result.stored_hash)
            } else {
                result.error
            };
            error!("Soul verification FAILED: {}", reason);

            // First run — no soul exists yet. This is expected.
            // embra-trustd should return a specific error for "no soul found"
            // that we can distinguish from "soul exists but hash doesn't match".
            if reason.contains("no soul") || reason.contains("not found") {
                warn!("No soul found — this appears to be a first run. Continuing boot for Learning Mode.");
                return Ok(());
            }

            // Soul exists but verification failed — this is a HALT condition
            error!("HALTING: Soul integrity violation. This system cannot be trusted.");
            let halt_msg = format!("Soul verification failed: {}", reason);
            // Desktop mode: spawn embra-comp in halt mode so the operator
            // sees a visible failure on /dev/tty1 instead of a black
            // graphical-boot screen. Best-effort — if it can't spawn we
            // still halt; the failure is captured in /embra/state/halt_reason
            // for post-mortem.
            if is_desktop_mode() {
                spawn_halt_screen(&halt_msg);
            }
            halt_system(&halt_msg)
        }
    }

    /// Stop all services in reverse dependency order.
    pub async fn stop_all(&mut self) {
        info!("Stopping all services");
        for i in (0..self.services.len()).rev() {
            let name = self.services[i].def.name.clone();
            info!("Stopping {}", name);
            if let Some(ref mut child) = self.services[i].child {
                // Send SIGTERM first
                let _ = child.kill().await;
                // Wait up to 5 seconds for graceful shutdown
                let _ = tokio::time::timeout(
                    Duration::from_secs(5),
                    child.wait()
                ).await;
            }
            self.services[i].status = ServiceStatus::Stopped;
            self.services[i].child = None;
        }
    }

    /// Check if a service is still running. Returns true if alive.
    pub async fn check_service(&mut self, index: usize) -> bool {
        if let Some(ref mut child) = self.services[index].child {
            match child.try_wait() {
                Ok(Some(status)) => {
                    // Process exited
                    warn!("{} exited with status: {:?}", self.services[index].def.name, status);
                    self.services[index].status = ServiceStatus::Failed(
                        format!("Exited: {:?}", status)
                    );
                    self.services[index].child = None;
                    false
                }
                Ok(None) => true, // Still running
                Err(e) => {
                    error!("Error checking {}: {}", self.services[index].def.name, e);
                    false
                }
            }
        } else {
            false
        }
    }

    /// Attempt to restart a failed service with backoff.
    pub async fn restart_service(&mut self, index: usize) -> Result<()> {
        let svc = &mut self.services[index];

        if svc.status == ServiceStatus::Halted {
            return Ok(()); // Don't restart halted services
        }

        if svc.restart_count >= svc.def.restart_policy.max_restarts {
            error!("{} exceeded max restarts ({})", svc.def.name, svc.def.restart_policy.max_restarts);
            svc.status = ServiceStatus::Halted;
            bail!("{} permanently failed", svc.def.name);
        }

        let backoff = std::cmp::min(
            svc.def.restart_policy.backoff_base * 2u32.saturating_pow(svc.restart_count),
            svc.def.restart_policy.backoff_max,
        );
        svc.restart_count += 1;

        warn!("Restarting {} (attempt {}, backoff {:?})", svc.def.name, svc.restart_count, backoff);
        tokio::time::sleep(backoff).await;

        self.start_service(index).await
    }

    pub fn service_count(&self) -> usize {
        self.services.len()
    }

    pub fn service_name(&self, index: usize) -> &str {
        &self.services[index].def.name
    }

    pub fn service_status(&self, index: usize) -> &ServiceStatus {
        &self.services[index].status
    }
}

/// Spawn embra-comp in halt mode so /dev/tty1 shows a visible halt
/// screen. Best-effort — failures are logged but don't prevent
/// halt_system from running. The spawned compositor parks forever
/// rendering the halt reason.
fn spawn_halt_screen(reason: &str) {
    let binary = if std::path::Path::new("/sbin/embra-comp").exists() {
        "/sbin/embra-comp"
    } else if std::path::Path::new("/usr/bin/embra-comp").exists() {
        "/usr/bin/embra-comp"
    } else {
        warn!("embra-comp binary not found — cannot render halt screen");
        return;
    };
    match std::process::Command::new(binary)
        .arg("--halt-reason")
        .arg(reason)
        .env("XDG_RUNTIME_DIR", "/run/user/0")
        .spawn()
    {
        Ok(child) => {
            info!("Spawned halt-mode embra-comp pid={} for visible halt screen", child.id());
            // Give the compositor a moment to take /dev/tty1 before
            // halt_system() reboots — without this the operator might
            // see only a brief flash before the kernel halts.
            std::thread::sleep(Duration::from_secs(2));
        }
        Err(e) => {
            warn!("Failed to spawn halt-mode embra-comp: {}", e);
        }
    }
}

#[cfg(target_os = "linux")]
fn halt_system(reason: &str) -> ! {
    error!("SYSTEM HALT: {}", reason);
    // Write reason to STATE partition for post-mortem
    let _ = std::fs::write("/embra/state/halt_reason", reason);
    // Sync filesystems
    unsafe { libc::sync(); }
    // Halt
    unsafe { libc::reboot(libc::LINUX_REBOOT_CMD_HALT); }
    // If reboot() fails, loop forever
    loop { std::thread::sleep(Duration::from_secs(3600)); }
}

#[cfg(not(target_os = "linux"))]
fn halt_system(reason: &str) -> ! {
    error!("SYSTEM HALT (dev mode, would halt on Linux): {}", reason);
    let _ = std::fs::write("/tmp/embra-halt-reason", reason);
    std::process::exit(1);
}
