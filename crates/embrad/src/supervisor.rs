//! Service supervisor for embrad.
//!
//! Manages the lifecycle of all embraOS services:
//! - Start in dependency order
//! - Health check polling
//! - Restart on failure with exponential backoff
//! - Graceful shutdown in reverse order

use anyhow::{Result, bail};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tracing::{info, warn, error};

use embra_boot::Cmdline;

/// WardSONDB's data directory on DATA: the `--data-dir` the database is
/// started with, and where its engine marker lives (`boot::remove_empty_engine_marker`).
pub(crate) const DATABASE_DIR: &str = "/embra/data/wardsondb";

/// Service definition. There is no dependency list: `register_services`
/// registers each service after everything it needs, and registration
/// order is the start order (and, reversed, the stop order).
#[derive(Clone)]
pub struct ServiceDef {
    pub name: String,
    pub binary: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub health_check: HealthCheck,
    pub restart_policy: RestartPolicy,
    /// An essential service that fails to start halts the boot (`start_all`
    /// returns the error). The display console and its udevd are not
    /// essential: a start that fails is logged and the boot goes on, the
    /// web console at its URLs.
    pub essential: bool,
}

#[derive(Clone)]
pub enum HealthCheck {
    /// HTTP GET to a URL, expect 200
    Http { url: String, timeout: Duration },
    /// gRPC health check on a port
    Grpc { port: u16, timeout: Duration },
    /// Just check if the process is alive
    ProcessAlive,
}

/// Restart budget for a supervised service. `max_restarts` is a BURST
/// limit, not a per-boot total: once a restarted service has run for
/// `stable_after`, its `restart_count` resets to 0 (see
/// [`restart_budget_reset_due`]), so ten isolated exits over a long uptime
/// never halt it — only ten exits in quick succession do. `stable_after`
/// must exceed `backoff_max`, or a crash loop pacing itself at the maximum
/// backoff could reset its own budget (pinned by a test).
#[derive(Clone)]
pub struct RestartPolicy {
    pub max_restarts: u32,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    /// Continuous run time after which the restart budget resets.
    pub stable_after: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_restarts: 10,
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            stable_after: Duration::from_secs(60),
        }
    }
}

/// The budget resets once a restarted service (`restart_count > 0`) has
/// run continuously for `stable_after`. A never-restarted service has
/// nothing to reset; a service that keeps dying before the window never
/// resets, so a crash loop still reaches `max_restarts` and halts.
fn restart_budget_reset_due(restart_count: u32, running_for: Duration, stable_after: Duration) -> bool {
    restart_count > 0 && running_for >= stable_after
}

/// Exponential backoff before the next restart attempt: `base * 2^count`,
/// capped at `backoff_max` (1 s, 2 s, 4 s, … 30 s with the defaults).
fn backoff_for(policy: &RestartPolicy, restart_count: u32) -> Duration {
    std::cmp::min(
        policy.backoff_base * 2u32.saturating_pow(restart_count),
        policy.backoff_max,
    )
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
    Halted, // The restart budget is used up — not restarted again
}

pub struct Supervisor {
    /// This boot's web console access token (web mode): minted once in
    /// `register_services`, handed to embra-web in its environment and
    /// printed in the boot banner, the one place it is shown.
    web_token: Option<String>,
    /// The stdio of the boot, kept before the log redirect for the serial
    /// console's restarts (`keep_boot_stdio`, #29).
    boot_stdio: Option<(std::fs::File, std::fs::File)>,
    /// In start order.
    services: Vec<ServiceState>,
    /// The kernel line, read once: the `embra.*` knobs the services are
    /// registered with (the last occurrence of each wins).
    cmdline: Cmdline,
    /// The display console is registered (udevd and embra-kiosk after
    /// embra-web): the banner says so.
    kiosk: bool,
}

impl Supervisor {
    /// A supervisor over this machine's kernel line (the tests; `main`
    /// reads the line once and passes it to `with_cmdline`).
    #[cfg(test)]
    pub fn new() -> Self {
        Self::with_cmdline(Cmdline::read())
    }

    pub fn with_cmdline(cmdline: Cmdline) -> Self {
        Self {
            web_token: None,
            boot_stdio: None,
            services: Vec::new(),
            cmdline,
            kiosk: false,
        }
    }

    /// Register all embraOS services in dependency order: `start_all`
    /// starts them in the order they are registered here.
    pub fn register_services(&mut self) {
        self.register_services_with(display_present(), std::path::Path::new(KIOSK_BIN).exists());
    }

    /// `register_services` with the machine's facts passed in: whether a
    /// display exists (`/dev/dri/card*`) and whether this image carries the
    /// display console's launcher. The tests pass both.
    pub fn register_services_with(&mut self, display_present: bool, kiosk_available: bool) {
        // Which engine: fixed in THIS binary when the image is built
        // (scripts/build-image.sh --storage-engine, see crates/embrad/build.rs),
        // rocksdb for a dev build that bypasses the script. WardSONDB itself
        // has both engines compiled in and takes the choice as a flag when
        // it starts.
        let storage_engine: &'static str =
            option_env!("EMBRA_STORAGE_ENGINE").unwrap_or("rocksdb");
        info!("WardSONDB storage engine: {}", storage_engine);
        // 1. WardSONDB — no dependencies
        // Loopback alone (#32): every in-guest client reaches the database at
        // 127.0.0.1:8090, the QEMU scripts forward no 8090, and the one
        // unauthenticated store stays off the wire of a bridged VM or bare
        // metal. The flag is the fork's (`wardsondb --bind`).
        let mut wardsondb_args = vec![
            "--storage-engine".to_string(), storage_engine.to_string(),
            "--bind".to_string(), "127.0.0.1".to_string(),
            "--port".to_string(), "8090".to_string(),
            "--data-dir".to_string(), DATABASE_DIR.to_string(),
            "--log-file".to_string(), "/embra/ephemeral/wardsondb.log".to_string(),
        ];
        // `embra.dbverbose=1`: WardSONDB's per-request log lines are opt-in
        // (`--verbose`); the default boot keeps the tmpfs log at
        // lifecycle/warn/error. An older WardSONDB rejects the flag and
        // does not start.
        if self.cmdline.db_verbose() {
            info!("WardSONDB verbose request logging enabled (embra.dbverbose=1)");
            wardsondb_args.push("--verbose".to_string());
        }
        self.add_service(ServiceDef {
            name: "wardsondb".to_string(),
            binary: "/usr/bin/wardsondb".to_string(),
            args: wardsondb_args,
            env: vec![],
            health_check: HealthCheck::Http {
                url: "http://127.0.0.1:8090/_health".to_string(),
                timeout: Duration::from_secs(30),
            },
            restart_policy: RestartPolicy::default(),
            essential: true,
        });

        // 2. embra-trustd — depends on wardsondb
        self.add_service(ServiceDef {
            name: "embra-trustd".to_string(),
            binary: "/usr/bin/embra-trustd".to_string(),
            args: vec![
                // Loopback alone, like the database: its clients are the
                // services beside it, and it mints certificates.
                "--bind".to_string(), "127.0.0.1".to_string(),
                "--port".to_string(), "50001".to_string(),
                "--state-dir".to_string(), "/embra/state".to_string(),
                "--wardsondb-url".to_string(), "http://127.0.0.1:8090".to_string(),
            ],
            env: vec![],
            health_check: HealthCheck::Grpc {
                port: 50001,
                timeout: Duration::from_secs(15),
            },
            restart_policy: RestartPolicy::default(),
            essential: true,
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
            restart_policy: RestartPolicy::default(),
            essential: true,
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
        // `embra.loglevel=<spec>`: the brain's `EMBRA_LOG`, an env-filter
        // Targets spec such as "info,kg::traversal=debug" (no spaces: kernel
        // arguments are space-split). Absent, the brain's INFO default.
        if let Some(spec) = self.cmdline.log_level() {
            info!("Brain log filter from cmdline (embra.loglevel): {}", spec);
            brain_env.push(("EMBRA_LOG".to_string(), spec.to_string()));
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
            restart_policy: RestartPolicy::default(),
            essential: true,
        });

        // 5. Operator UI — depends on embra-brain.
        // The brain is single-conversation (one shared SessionManager /
        // in_turn gate), so exactly one console→brain client may run.
        // Web mode (the last word of `embra.web` and `embra.tui` on the
        // kernel line): embra-web, which spawns the sole PTY-hosted
        // embra-console internally. Otherwise the serial embra-console TUI.
        if self.cmdline.web_console() {
            // The web console's gate: a token for this boot, in embra-web's
            // environment (never an argument: `ps` shows those) and in the
            // URL the banner prints. Without a token there is no gate, and
            // an open console is worse than none: the error stays on the
            // serial console, and `EMBRA_TUI=1` still boots the TUI.
            let token = match boot_token() {
                Ok(t) => t,
                Err(e) => {
                    error!("Web mode: no access token could be minted ({e}); embra-web is NOT started. Boot with EMBRA_TUI=1 for the serial console.");
                    return;
                }
            };
            self.web_token = Some(token.clone());
            info!("Web mode (embra.web=1): registering embra-web; serial console skipped");
            self.add_service(ServiceDef {
                name: "embra-web".to_string(),
                binary: "/usr/bin/embra-web".to_string(),
                args: vec![
                    "--port".to_string(), "3345".to_string(),
                    "--apid-addr".to_string(), "http://127.0.0.1:50000".to_string(),
                    "--trust-addr".to_string(), "http://127.0.0.1:50001".to_string(),
                    "--console-bin".to_string(), "/usr/bin/embra-console".to_string(),
                ],
                env: vec![("EMBRA_WEB_TOKEN".to_string(), token.clone())],
                // embra-web only binds :3345 *after* it has fetched its
                // serving cert from embra-trustd, so a TCP-connect on 3345
                // is a real readiness signal (port open ⇒ cert acquired +
                // HTTPS listener up). HealthCheck::Grpc is just a TCP
                // connect (no gRPC spoken), which is what we want here.
                // Generous timeout covers trustd warm-up + cert gen.
                health_check: HealthCheck::Grpc {
                    port: 3345,
                    timeout: std::time::Duration::from_secs(45),
                },
                restart_policy: RestartPolicy::default(),
                essential: true,
            });
            // 6. The display console — depends on embra-web: the web
            // console rendered on the machine's display by WPE WebKit
            // through cog's DRM platform, input through libinput over
            // udev, which embrad starts first (`udevd`, seeded with the
            // input devices once it runs). `display_console` decides: on
            // where a display exists unless `embra.kiosk=0`. Neither
            // service is essential: a display console that cannot start
            // is logged and the boot continues; the web console stays at
            // its URLs.
            if embra_boot::cmdline::display_console(true, self.cmdline.kiosk(), display_present) {
                if !kiosk_available {
                    info!("A display is present, but this image carries no display console ({KIOSK_BIN} absent)");
                } else {
                    info!("Display console: registering udevd and embra-kiosk after embra-web (embra.kiosk=0 boots without them)");
                    self.kiosk = true;
                    self.add_service(ServiceDef {
                        name: "udevd".to_string(),
                        binary: UDEVD_BIN.to_string(),
                        args: vec![],
                        env: vec![],
                        health_check: HealthCheck::ProcessAlive,
                        restart_policy: RestartPolicy::default(),
                        essential: false,
                    });
                    let scale = self.cmdline.kiosk_scale().unwrap_or_else(|| device_scale(display_width_px()));
                    self.add_service(ServiceDef {
                        name: "embra-kiosk".to_string(),
                        binary: KIOSK_BIN.to_string(),
                        args: kiosk_args(scale),
                        env: kiosk_env(&token, self.cmdline.keyboard_layout()),
                        health_check: HealthCheck::ProcessAlive,
                        restart_policy: RestartPolicy::default(),
                        essential: false,
                    });
                }
            }
        } else {
            // The terminal size from the kernel line (`embra.cols`,
            // `embra.rows`, run-qemu.sh's host terminal), 80×24 without it.
            let (term_cols, term_rows) = self.cmdline.terminal_size();
            info!("Terminal size from cmdline: {}x{}", term_cols, term_rows);
            // `embra.graphics=<mode>`: the serial console's image protocol
            // (`EMBRA_TUI_GRAPHICS`: halfblocks, sixel, kitty, iterm2, auto,
            // off); absent, the console's halfblocks default. The web PTY
            // console is embra-web's own child and not governed by this.
            let mut console_env: Vec<(String, String)> = Vec::new();
            if let Some(mode) = self.cmdline.graphics() {
                info!("Serial console graphics from cmdline (embra.graphics): {}", mode);
                console_env.push(("EMBRA_TUI_GRAPHICS".to_string(), mode.to_string()));
            }
            self.add_service(ServiceDef {
                name: "embra-console".to_string(),
                binary: "/usr/bin/embra-console".to_string(),
                args: vec![
                    "--apid-addr".to_string(), "http://127.0.0.1:50000".to_string(),
                    "--device".to_string(), "/dev/ttyS0".to_string(),
                    "--columns".to_string(), term_cols.to_string(),
                    "--rows".to_string(), term_rows.to_string(),
                ],
                env: console_env,
                health_check: HealthCheck::ProcessAlive,
                restart_policy: RestartPolicy::default(),
                essential: true,
            });
        }
    }

    fn add_service(&mut self, def: ServiceDef) {
        self.services.push(ServiceState {
            def,
            child: None,
            pid: None,
            started_at: None,
            restart_count: 0,
            status: ServiceStatus::Stopped,
        });
    }

    /// Start all services in registration order.
    /// After embra-trustd starts, verifies the soul. HALTs if verification fails.
    pub async fn start_all(&mut self) -> Result<()> {
        for i in 0..self.services.len() {
            let name = self.services[i].def.name.clone();
            let essential = self.services[i].def.essential;
            info!("Starting service: {}", name);

            if let Err(e) = self.start_service(i).await {
                if essential {
                    return Err(e);
                }
                // The display console and its udevd: the boot goes on, and
                // the operator at the display learns why it stays dark.
                warn!("{name} did not start ({e}); not essential, the boot continues");
                embra_boot::console::BootConsole::open()
                    .line(&format!("[embrad] {name} did not start; the web console stays at the URLs above"));
                continue;
            }
            if name == "udevd" {
                seed_udev().await;
            }

            // After spawning the operator UI, redirect embrad's output to
            // a log file: serial console needs ttyS0 clean for the TUI;
            // web mode wants ttyS0 quiet (operator is on the browser). The
            // stdio of the boot is kept first, for the serial console's
            // restarts (#29).
            if (name == "embra-console" || name == "embra-web") && std::process::id() == 1 {
                self.boot_stdio = keep_boot_stdio();
                if name == "embra-web" {
                    // embra-web is healthy now (TCP :3345 open ⇒ cert
                    // acquired + HTTPS serving). Print a clear readiness
                    // line to the serial console *before* the redirect —
                    // web mode goes quiet after this and the operator
                    // works from the browser.
                    use std::fmt::Write as _;
                    let mut b = String::new();
                    let _ = writeln!(b);
                    let _ = writeln!(b,
                        "\x1b[1;32m  \u{2713} embraOS boot complete \u{2014} web console is ready\x1b[0m");
                    let lease = crate::net_lease::read();
                    let urls = crate::net_lease::console_urls(self.web_token.as_deref().unwrap_or(""), &lease);
                    let _ = writeln!(b,
                        "    \x1b[1;36m{}\x1b[0m  \x1b[2m(QEMU hostfwd; accept the embraOS-CA cert)\x1b[0m", urls[0]);
                    if let Some(leased) = urls.get(1) {
                        // A lease of this machine's own: a bridged network, a
                        // real host. QEMU's user network leases 10.0.2.15,
                        // which no line names (unreachable from outside).
                        let _ = writeln!(b,
                            "    \x1b[1;36m{leased}\x1b[0m  \x1b[2m(the address DHCP leased to this machine, {}/{})\x1b[0m", lease.ip, lease.prefix);
                    }
                    let _ = writeln!(b,
                        "    \x1b[2mThis boot's access token is in the URL; the URL is the credential, and it is shown here only.\x1b[0m");
                    if self.kiosk {
                        let _ = writeln!(b,
                            "    \x1b[2mThe display shows this console too (embra.kiosk=0 boots without it).\x1b[0m");
                    }
                    if let Ok(release) = embra_boot::Release::read() {
                        let slot = match self.cmdline.slot() {
                            Ok(Some(s)) if self.cmdline.trial() => format!(", slot {s} (trial boot, confirmed once every service is up)"),
                            Ok(Some(s)) => format!(", slot {s}"),
                            _ => String::new(),
                        };
                        let _ = writeln!(b, "    \x1b[2membraOS {} ({}, {}){slot}\x1b[0m", release.version, release.build_id, release.engine);
                    }
                    let _ = writeln!(b);
                    // Both consoles: the serial line and the display.
                    crate::console::emit_bytes(b.as_bytes());
                }
                info!("Redirecting embrad output to log file");
                if let Ok(log) = std::fs::File::create("/embra/ephemeral/embrad.log") {
                    use std::os::unix::io::AsRawFd;
                    let fd = log.as_raw_fd();
                    unsafe {
                        libc::dup2(fd, 1); // stdout
                        libc::dup2(fd, 2); // stderr
                    }
                    std::mem::forget(log);
                }
                // The boot phase ends here: the display got its copy of
                // every line so far, and the log file gets the rest.
                crate::console::end_boot_phase();
            }

            // After embra-trustd is up, verify the soul
            if name == "embra-trustd" {
                self.verify_soul().await?;
            }
        }
        Ok(())
    }

    async fn start_service(&mut self, index: usize) -> Result<()> {
        let boot_stdio = self
            .boot_stdio
            .as_ref()
            .and_then(|(i, o)| Some((i.try_clone().ok()?, o.try_clone().ok()?)));
        let svc = &mut self.services[index];
        svc.status = ServiceStatus::Starting;

        let mut cmd = Command::new(&svc.def.binary);
        cmd.args(&svc.def.args);
        for (key, val) in &svc.def.env {
            cmd.env(key, val);
        }
        // embra-console gets stdin/stdout for the TUI (serial console):
        // the stdio of the boot, kept before the log redirect, once that
        // has happened — a console started again later would otherwise
        // inherit the log file and draw its screen into it (#29) — and
        // embrad's own before it. stderr goes to a log file to prevent
        // embrad log bleed-through. A log file that cannot be opened (or
        // cloned for the second stream) sends that stream nowhere; nothing
        // here can panic PID 1.
        if svc.def.name == "embra-console" {
            match boot_stdio {
                Some((stdin, stdout)) => {
                    cmd.stdin(Stdio::from(stdin));
                    cmd.stdout(Stdio::from(stdout));
                }
                None => {
                    cmd.stdin(Stdio::inherit());
                    cmd.stdout(Stdio::inherit());
                }
            }
            let log_path = "/embra/ephemeral/embra-console.log";
            cmd.stderr(match std::fs::File::create(log_path) {
                Ok(f) => Stdio::from(f),
                Err(_) => Stdio::null(),
            });
        } else {
            let log_path = format!("/embra/ephemeral/{}.log", svc.def.name);
            let (out, err) = match std::fs::File::create(&log_path) {
                Ok(f) => match f.try_clone() {
                    Ok(f2) => (Stdio::from(f), Stdio::from(f2)),
                    Err(_) => (Stdio::from(f), Stdio::null()),
                },
                Err(_) => (Stdio::null(), Stdio::null()),
            };
            cmd.stdout(out);
            cmd.stderr(err);
        }
        if svc.def.name == "embra-kiosk" {
            prepare_kiosk_dirs();
            // The display's VT is parked while the kiosk draws: fbcon off
            // the framebuffer, the keyboard out of the VT layer (no
            // Ctrl+Alt+Fn, SysRq or Ctrl+Alt+Del). Given back when it stops.
            if let Err(e) = embra_boot::vt::take_graphics() {
                warn!("the display's VT could not be parked ({e}); the kernel console may draw over the display console");
            }
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
        if svc.def.name == "embra-kiosk" {
            KIOSK_PID.store(pid.unwrap_or(0), Ordering::Relaxed);
        }

        info!("Spawned {} (pid={:?})", svc.def.name, pid);

        // Give the process a moment to start (or crash)
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Check if it already exited (crash on startup)
        if let Some(ref mut child) = svc.child {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let name = svc.def.name.clone();
                    let msg = fold_refusal(format!("{} exited immediately with status: {:?}", name, status), &name, refusal_of(&name).as_deref());
                    // stdout+stderr were redirected to the log file — read its tail
                    // onto both consoles before the halt.
                    crate::console::emit(&format!("[embrad] DIAG: {}", msg));
                    dump_log_tail(&name, 30);
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
            let e = anyhow::anyhow!(fold_refusal(e.to_string(), &name, refusal_of(&name).as_deref()));
            crate::console::emit(&format!("[embrad] DIAG: {} health check failed: {}", name, e));

            dump_log_tail(&name, 20);
            // Also check if process is still alive
            if let Some(ref mut child) = self.services[index].child {
                match child.try_wait() {
                    Ok(Some(status)) => crate::console::emit(&format!("[embrad] DIAG: {} process exited with: {:?}", name, status)),
                    Ok(None) => crate::console::emit(&format!("[embrad] DIAG: {} process is still running but not responding on health endpoint", name)),
                    Err(e2) => crate::console::emit(&format!("[embrad] DIAG: {} could not check process status: {}", name, e2)),
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
                                    if is_http_200(&response) {
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

        match soul_decision(result.valid, &result.error, &result.computed_hash, &result.stored_hash) {
            SoulDecision::Pass => {
                info!("Soul verification PASSED (hash={})", result.computed_hash);
                Ok(())
            }
            SoulDecision::FirstRun => {
                // First run — no soul exists yet. This is expected.
                warn!("No soul found — this appears to be a first run. Continuing boot for Learning Mode.");
                Ok(())
            }
            SoulDecision::Halt(reason) => {
                error!("Soul verification FAILED: {}", reason);
                // Soul exists but verification failed — this is a HALT condition
                error!("HALTING: Soul integrity violation. This system cannot be trusted.");
                halt_system(&format!("Soul verification failed: {}", reason))
            }
        }
    }

    /// Stop all services in reverse dependency order: SIGTERM, a grace
    /// period, SIGKILL (`stop_one`).
    pub async fn stop_all(&mut self) {
        info!("Stopping all services");
        for i in (0..self.services.len()).rev() {
            let name = self.services[i].def.name.clone();
            info!("Stopping {}", name);
            if let Some(ref mut child) = self.services[i].child {
                stop_one(child, &name, STOP_GRACE).await;
            }
            self.services[i].status = ServiceStatus::Stopped;
            self.services[i].child = None;
            if name == "embra-kiosk" {
                release_display();
            }
        }
    }

    /// The pids of the supervised children that are running: what the
    /// orphan reaper (`reconcile::reap_orphans`) leaves for tokio's `Child`
    /// to collect.
    pub fn child_pids(&self) -> Vec<u32> {
        self.services.iter().filter_map(|s| s.child.as_ref().and_then(|c| c.id())).collect()
    }

    /// Check if a service is still running. Returns true if alive.
    ///
    /// Any exit — clean or not — puts the service in `Failed` so the
    /// reconcile loop restarts it: every supervised service is meant to
    /// run for the life of the boot (a clean exit is still an outage).
    /// A service that has run stably for `stable_after` since its last
    /// restart gets its restart budget back.
    pub async fn check_service(&mut self, index: usize) -> bool {
        if let Some(ref mut child) = self.services[index].child {
            match child.try_wait() {
                Ok(Some(status)) => {
                    // Process exited
                    let name = &self.services[index].def.name;
                    let detail = if status.success() {
                        info!("{} exited cleanly (status 0) — restarting", name);
                        "Exited cleanly (status 0)".to_string()
                    } else {
                        warn!("{} exited with status: {:?}", name, status);
                        format!("Exited: {:?}", status)
                    };
                    self.services[index].status = ServiceStatus::Failed(detail);
                    self.services[index].child = None;
                    false
                }
                Ok(None) => {
                    // Still running — a restarted service that has stayed
                    // up for the stability window earns its budget back.
                    let svc = &mut self.services[index];
                    if let Some(started_at) = svc.started_at
                        && restart_budget_reset_due(
                            svc.restart_count,
                            started_at.elapsed(),
                            svc.def.restart_policy.stable_after,
                        )
                    {
                        info!(
                            "{} stable for {:?} — restart budget reset (was {} of {})",
                            svc.def.name,
                            svc.def.restart_policy.stable_after,
                            svc.restart_count,
                            svc.def.restart_policy.max_restarts
                        );
                        svc.restart_count = 0;
                    }
                    true
                }
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
            if svc.def.name == "embra-kiosk" {
                // The display gets its text console back and says why it
                // changed; the web console is unaffected.
                release_display();
                embra_boot::console::BootConsole::open()
                    .line("[embrad] the display console stopped and is not restarted; the web console stays at the URLs above");
            }
            bail!("{} permanently failed", svc.def.name);
        }

        let backoff = backoff_for(&svc.def.restart_policy, svc.restart_count);
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

    /// The arguments a registered service is started with, for a test.
    #[cfg(test)]
    pub(crate) fn service_args(&self, name: &str) -> Option<&[String]> {
        self.services.iter().find(|s| s.def.name == name).map(|s| s.def.args.as_slice())
    }

    pub fn service_status(&self, index: usize) -> &ServiceStatus {
        &self.services[index].status
    }
}

/// How long a service has to exit on SIGTERM before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// Stops one child: SIGTERM, `grace` to exit on it, SIGKILL after. A
/// service that handles SIGTERM closes its files first — WardSONDB its
/// database — instead of being cut off, which `Child::kill` alone did
/// (#29). The exit status, when one was collected.
pub(crate) async fn stop_one(child: &mut tokio::process::Child, name: &str, grace: Duration) -> Option<std::process::ExitStatus> {
    if let Some(pid) = child.id()
        && let Err(e) = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGTERM)
    {
        warn!("{name}: SIGTERM not sent: {e}");
    }
    match tokio::time::timeout(grace, child.wait()).await {
        Ok(Ok(status)) => Some(status),
        Ok(Err(e)) => {
            warn!("{name}: wait failed: {e}");
            None
        }
        Err(_) => {
            warn!("{name} did not exit on SIGTERM within {grace:?}; killing it");
            let _ = child.kill().await;
            child.try_wait().ok().flatten()
        }
    }
}

/// The last `lines` of a service's log on both consoles, for the
/// diagnosis before a halt (#41): the service's stdout and stderr were
/// redirected to the file, so this is the one place its tail is seen.
fn dump_log_tail(name: &str, lines: usize) {
    let log_path = format!("/embra/ephemeral/{}.log", name);
    match std::fs::read_to_string(&log_path) {
        Ok(log_content) if !log_content.is_empty() => {
            crate::console::emit(&format!("[embrad] DIAG: {} log tail:", name));
            for line in log_content.lines().rev().take(lines).collect::<Vec<_>>().into_iter().rev() {
                crate::console::emit(&format!("  {}", line));
            }
        }
        Ok(_) => {
            crate::console::emit(&format!("[embrad] DIAG: {} log file is empty — service produced no output", name));
        }
        Err(log_err) => {
            crate::console::emit(&format!("[embrad] DIAG: {} could not read log file {}: {}", name, log_path, log_err));
        }
    }
}

/// Whether an HTTP response begins with a 200 status line. The substring
/// `200` anywhere in a response (a `Content-Length: 200`, a body) is not a
/// health (#29).
pub(crate) fn is_http_200(response: &str) -> bool {
    let line = response.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    matches!(parts.next(), Some(version) if version.starts_with("HTTP/")) && parts.next() == Some("200")
}

/// The stdio PID 1 has at boot, duplicated so the log redirect that follows
/// leaves these handles on the kernel console: the serial console's
/// restarts use them (#29). `None` when a duplicate could not be made; the
/// console then inherits as before.
/// The display console's launcher: cog on its DRM platform.
const KIOSK_BIN: &str = "/usr/bin/cog";
/// udev, for libinput's device enumeration under the display console.
const UDEVD_BIN: &str = "/sbin/udevd";
/// The display console's writable home, on the ephemeral tmpfs (the root
/// is read-only; WebKit and fontconfig write caches).
const KIOSK_HOME: &str = "/embra/ephemeral/kiosk";
/// XDG_RUNTIME_DIR for the display console (0700).
const XDG_RUNTIME_DIR: &str = "/run/user/0";

/// A display: any `card*` under /dev/dri.
fn display_present() -> bool {
    std::fs::read_dir("/dev/dri")
        .map(|d| d.flatten().any(|e| e.file_name().to_string_lossy().starts_with("card")))
        .unwrap_or(false)
}

/// The width of the display's first mode, from sysfs: a connector's `modes`
/// file lists `<width>x<height>` lines (`/sys/class/drm/card0-Virtual-1/modes`).
/// 0 when no connector names one.
fn display_width_px() -> u32 {
    let Ok(dir) = std::fs::read_dir("/sys/class/drm") else { return 0 };
    let mut names: Vec<_> = dir.flatten().map(|e| e.path()).collect();
    names.sort();
    for path in names {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if !name.starts_with("card") || !name.contains('-') {
            continue;
        }
        if let Ok(modes) = std::fs::read_to_string(path.join("modes"))
            && let Some(first) = modes.lines().next()
            && let Some((w, _)) = first.split_once('x')
            && let Ok(width) = w.trim().parse::<u32>()
        {
            return width;
        }
    }
    0
}

/// The device scale for a display's width: 1 below 2560 pixels, 2 from there.
fn device_scale(width_px: u32) -> u8 {
    if width_px >= 2560 { 2 } else { 1 }
}

/// The display console's arguments: cog's DRM platform, the console's
/// background until the first paint, TLS errors ignored on the loopback
/// (the certificate is this machine's own, 127.0.0.1 among its names), a
/// crashed web process restarted rather than an error page nobody can
/// dismiss, the device scale.
fn kiosk_args(scale: u8) -> Vec<String> {
    vec![
        "--platform=drm".to_string(),
        "--bg-color=#0c0907".to_string(),
        "--ignore-tls-errors".to_string(),
        "--webprocess-failure=restart".to_string(),
        format!("--device-scale={scale}"),
    ]
}

/// The display console's environment: the console URL with this boot's
/// token in `COG_URL` (never an argument: `ps` shows those; `?kiosk=1` tells
/// the page it is on the display), Mesa's software path on the card
/// (kms_swrast through GBM; never `LIBGL_ALWAYS_SOFTWARE`, which would send
/// the web process down a path the nested display does not serve), the
/// writable homes on the ephemeral tmpfs, the keyboard layout.
fn kiosk_env(token: &str, layout: Option<&str>) -> Vec<(String, String)> {
    vec![
        ("COG_URL".to_string(), format!("https://127.0.0.1:3345/embraOS?token={token}&kiosk=1")),
        ("MESA_LOADER_DRIVER_OVERRIDE".to_string(), "kms_swrast".to_string()),
        ("GBM_ALWAYS_SOFTWARE".to_string(), "1".to_string()),
        ("XDG_RUNTIME_DIR".to_string(), XDG_RUNTIME_DIR.to_string()),
        ("HOME".to_string(), KIOSK_HOME.to_string()),
        ("XDG_CACHE_HOME".to_string(), format!("{KIOSK_HOME}/cache")),
        ("XDG_DATA_HOME".to_string(), format!("{KIOSK_HOME}/data")),
        ("XDG_CONFIG_HOME".to_string(), format!("{KIOSK_HOME}/config")),
        ("XKB_DEFAULT_LAYOUT".to_string(), layout.unwrap_or("us").to_string()),
    ]
}

/// The display console's directories, before each spawn (a restart finds
/// them too): XDG_RUNTIME_DIR 0700, the homes under the ephemeral tmpfs.
/// Best effort; the launcher's own log says what it misses.
fn prepare_kiosk_dirs() {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::create_dir_all(XDG_RUNTIME_DIR) {
        warn!("{XDG_RUNTIME_DIR}: {e}");
    }
    let _ = std::fs::set_permissions(XDG_RUNTIME_DIR, std::fs::Permissions::from_mode(0o700));
    for sub in ["cache", "data", "config"] {
        if let Err(e) = std::fs::create_dir_all(format!("{KIOSK_HOME}/{sub}")) {
            warn!("{KIOSK_HOME}/{sub}: {e}");
        }
    }
}

/// udev, once it runs, is told about the input devices devtmpfs created
/// before it (`udevadm trigger`, the input subsystem alone: no network
/// rule may rename eth0), and waited for; libinput enumerates what udev has
/// tagged. Best effort, ten seconds at most each.
async fn seed_udev() {
    let Some(udevadm) = ["/sbin/udevadm", "/bin/udevadm", "/usr/bin/udevadm"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
    else {
        warn!("udevadm not found; the display console sees the input devices udev tags on its own");
        return;
    };
    for args in [
        &["trigger", "--action=add", "--subsystem-match=input"][..],
        &["settle", "--timeout=5"][..],
    ] {
        let run = Command::new(udevadm)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match tokio::time::timeout(Duration::from_secs(10), run).await {
            Ok(Ok(status)) if status.success() => {}
            Ok(Ok(status)) => warn!("udevadm {} exited with {status}", args[0]),
            Ok(Err(e)) => warn!("udevadm {} could not be run: {e}", args[0]),
            Err(_) => warn!("udevadm {} took longer than 10 s", args[0]),
        }
    }
    info!("udev seeded with the input devices");
}

fn keep_boot_stdio() -> Option<(std::fs::File, std::fs::File)> {
    use std::os::unix::io::FromRawFd;
    // SAFETY: dup(2) of the process's own fds 0 and 1 gives descriptors this
    // process owns alone, each wrapped once; a failed dup is -1 and is not
    // wrapped.
    let stdin = unsafe { libc::dup(0) };
    if stdin < 0 {
        return None;
    }
    let stdout = unsafe { libc::dup(1) };
    if stdout < 0 {
        unsafe { libc::close(stdin) };
        return None;
    }
    Some(unsafe { (std::fs::File::from_raw_fd(stdin), std::fs::File::from_raw_fd(stdout)) })
}

/// The environment every service inherits, set before the tokio runtime is
/// built: `TZ` from STATE (the config wizard writes it) and the operator CA
/// trust (`ca_bundle`). One thread runs at this point, which is what makes
/// the `set_var` calls sound (#29); nothing after the runtime exists touches
/// the environment.
pub fn prepare_environment() {
    let tz = std::fs::read_to_string("/embra/state/timezone")
        .unwrap_or_default()
        .trim()
        .to_string();
    if !tz.is_empty() {
        // SAFETY: one thread runs — `main` calls this before it builds the
        // runtime — so no other thread reads the environment meanwhile.
        unsafe { std::env::set_var("TZ", &tz) };
        info!("System timezone set: TZ={}", tz);
    }
    // Merge operator CA certs (STATE drop-in) with the stock bundle and
    // export GIT_SSL_CAINFO/SSL_CERT_FILE for all child processes.
    crate::ca_bundle::setup_operator_ca_trust();
}

/// Where a service that refuses to start leaves its one-line reason
/// (`/embra/ephemeral/<name>.refusal`): the brain on a knowledge graph
/// written by a newer release than itself. Folded into the halt reason, so
/// the operator reads why on STATE and on the console.
pub(crate) fn refusal_path(name: &str) -> String {
    format!("/embra/ephemeral/{name}.refusal")
}

pub(crate) fn refusal_of(name: &str) -> Option<String> {
    std::fs::read_to_string(refusal_path(name)).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

/// The failure message with the service's own reason appended, when it left one.
pub(crate) fn fold_refusal(message: String, name: &str, refusal: Option<&str>) -> String {
    match refusal {
        Some(r) => format!("{message}; {name} refused to start: {r}"),
        None => message,
    }
}

/// What PID 1 keeps of the environment the kernel handed it: the three
/// variables the kernel sets itself. The rest is whatever `KEY=value` the
/// boot line carried (the kernel passes every undotted one to init), and
/// embrad reads `EMBRA_PROVIDER`, the API keys and the bearers from its
/// environment: scrubbed, the boot line is a channel for `embra.*` knobs
/// and nothing else.
pub(crate) fn kept_at_boot(name: &str) -> bool {
    matches!(name, "PATH" | "HOME" | "TERM")
}

/// Drop every inherited variable but the kept ones; the names (never the
/// values) of what was dropped go to the log. PID 1 only, before the
/// runtime: one thread runs, which is what makes `remove_var` sound.
pub fn scrub_boot_environment() {
    let dropped: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| !kept_at_boot(k))
        .collect();
    for name in &dropped {
        // SAFETY: one thread runs — `main` calls this before it builds the
        // runtime — so no other thread reads the environment meanwhile.
        unsafe { std::env::remove_var(name) };
    }
    if !dropped.is_empty() {
        info!("boot environment scrubbed: {} variable(s) dropped ({})", dropped.len(), dropped.join(", "));
    }
}

/// Where a halt leaves its reason, on STATE: a soul verification failure, a
/// fatal boot error, a panic in PID 1. The intelligence reads it with
/// `file_read`; the operator by loop-mounting STATE.
pub(crate) const HALT_REASON_PATH: &str = "/embra/state/halt_reason";

/// The display console's pid while it runs (0 otherwise): a halt kills it
/// and gives the display's VT back before the reason is written there.
static KIOSK_PID: AtomicU32 = AtomicU32::new(0);

/// The display console stopped: the VT's text console and keyboard are back
/// (`embra_boot::vt`), so the display shows text again. Idempotent; a
/// failure is logged (a machine without a VT).
pub(crate) fn release_display() {
    KIOSK_PID.store(0, Ordering::Relaxed);
    if let Err(e) = embra_boot::vt::release() {
        info!("the display's VT was not given back: {e}");
    }
}

/// Before a halt reason goes to the display: the kiosk, if any, is killed
/// (SIGKILL, up to half a second for it to go) and the VT released, so the
/// text is drawn rather than hidden behind the kiosk's last frame.
#[cfg(target_os = "linux")]
fn clear_the_display_for_the_reason() {
    let pid = KIOSK_PID.swap(0, Ordering::Relaxed);
    if pid > 1 {
        // SAFETY: a signal to the pid this process recorded when it spawned
        // the kiosk; a stale pid is a kill(2) failure, nothing more.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            let mut status = 0;
            // SAFETY: a non-blocking wait on the child this process owns.
            let r = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
            if r != 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let _ = embra_boot::vt::release();
}

/// Halt the machine with `reason` written to STATE. Every fatal path of
/// PID 1 ends here (`main.rs`: the panic hook, a fatal `run()` error, the
/// soul gate); PID 1 never exits.
#[cfg(target_os = "linux")]
pub(crate) fn halt_system(reason: &str) -> ! {
    error!("SYSTEM HALT: {}", reason);
    if std::process::id() != 1 {
        // Development: embrad is not PID 1, so the machine is not the OS
        // and is not ours to halt (#16). The reason goes where the
        // non-Linux arm puts it, and the process exits.
        error!("not PID 1 — exiting instead of halting the host");
        let _ = std::fs::write("/tmp/embra-halt-reason", reason);
        std::process::exit(1);
    }
    // Write reason to STATE partition for post-mortem
    let _ = std::fs::write(HALT_REASON_PATH, reason);
    // Sync filesystems
    unsafe { libc::sync(); }
    clear_the_display_for_the_reason();
    // After the log redirect the line above went to the log file: the
    // reason goes to both consoles by hand (during the boot phase the
    // tracing line reached them already).
    if !crate::console::in_boot_phase() {
        embra_boot::console::BootConsole::open().line(&format!("[embrad] SYSTEM HALT: {reason}"));
    }
    // Let the consoles drain the line before the halt.
    std::thread::sleep(Duration::from_secs(2));
    // Halt
    unsafe { libc::reboot(libc::LINUX_REBOOT_CMD_HALT); }
    // If reboot() fails, loop forever
    loop { std::thread::sleep(Duration::from_secs(3600)); }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn halt_system(reason: &str) -> ! {
    error!("SYSTEM HALT (dev mode, would halt on Linux): {}", reason);
    let _ = std::fs::write("/tmp/embra-halt-reason", reason);
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
pub(crate) fn reboot_system() -> ! {
    info!("Rebooting system");
    unsafe { libc::sync(); }
    unsafe { libc::reboot(libc::LINUX_REBOOT_CMD_RESTART); }
    loop { std::thread::sleep(Duration::from_secs(3600)); }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn reboot_system() -> ! {
    info!("Reboot requested (dev mode, would reboot on Linux)");
    std::process::exit(0);
}

/// Whether trustd's report says there is no soul yet, as on a first boot.
///
/// trustd has no code for that case: its report is text, and these are the
/// words looked for (`embra-trustd/src/soul.rs`, `NO_SOUL_ERROR`). Every
/// other report is a failed verification and halts the system.
/// The sentence embra-trustd reports for a soul document that does not
/// exist while no hash is stored (`NO_SOUL_ERROR` in
/// `embra-trustd/src/soul.rs`, pinned there as this text): a first boot.
/// Reword one and the other with it.
const NO_SOUL_REPORT: &str = "Soul document not found — no soul exists (first run or data loss)";

/// Whether trustd's report is the first-boot sentence. The comparison is
/// exact: a database error that happens to say "not found", a missing
/// hash, a document without its soul field — none of them reads as a first
/// boot, so none re-seals a machine that was sealed (#52).
fn is_first_run(reason: &str) -> bool {
    reason.trim() == NO_SOUL_REPORT
}

/// What embrad does with trustd's verdict (`verify_soul`).
#[derive(Debug, PartialEq)]
enum SoulDecision {
    /// The soul as stored matches its hash: the boot continues.
    Pass,
    /// trustd's first-boot sentence, exactly: Learning Mode.
    FirstRun,
    /// Anything else: the machine halts with this reason (the mismatch
    /// spelled out when trustd gave no words).
    Halt(String),
}

fn soul_decision(valid: bool, error: &str, computed: &str, stored: &str) -> SoulDecision {
    if valid {
        return SoulDecision::Pass;
    }
    let reason = if error.is_empty() {
        format!("Soul hash mismatch: computed={computed}, stored={stored}")
    } else {
        error.to_string()
    };
    if is_first_run(&reason) {
        SoulDecision::FirstRun
    } else {
        SoulDecision::Halt(reason)
    }
}

/// 16 bytes of `/dev/urandom` as 32 lowercase hex characters: the web
/// console's token for this boot. A failed read is an error, never a
/// weaker source.
fn boot_token() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(hex_of(&bytes))
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}


#[cfg(test)]
pub(crate) mod test_support {
    /// The tests that spawn children, and the orphan-reaper guard, run one
    /// at a time. The harness runs tests on parallel threads of one process,
    /// and `reconcile::reap_orphans` with an empty supervised list reaps
    /// every exited child of that process: a child of another test taken
    /// that way leaves its `wait` with no child to collect (`stop_one` then
    /// answers `None`), and a zombie of another test counts in the reaper
    /// guard's sweep. Seen as CI job 1794 under the runner's load.
    pub(crate) static CHILDREN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
}

#[cfg(test)]
mod refusal_tests {
    use super::{fold_refusal, refusal_path};

    #[test]
    fn a_refusal_file_is_folded_into_the_halt_reason() {
        assert_eq!(refusal_path("embra-brain"), "/embra/ephemeral/embra-brain.refusal");
        assert_eq!(
            fold_refusal("embra-brain exited immediately with status: ExitStatus(1)".into(), "embra-brain", Some("the knowledge graph is at schema 15")),
            "embra-brain exited immediately with status: ExitStatus(1); embra-brain refused to start: the knowledge graph is at schema 15"
        );
        assert_eq!(fold_refusal("x".into(), "embra-brain", None), "x");
    }
}

#[cfg(test)]
mod boot_environment_tests {
    use super::kept_at_boot;

    #[test]
    fn pid_one_keeps_path_home_and_term_from_the_boot_line_and_drops_the_rest() {
        for kept in ["PATH", "HOME", "TERM"] {
            assert!(kept_at_boot(kept), "{kept} is the kernel's own");
        }
        for dropped in ["EMBRA_PROVIDER", "ANTHROPIC_API_KEY", "GEMINI_API_KEY", "EMBRA_OLLAMA_BEARER", "EMBRA_CA_DIR", "EMBRA_WEB_TOKEN", "RUST_LOG", "embra"] {
            assert!(!kept_at_boot(dropped), "{dropped} never reaches a service from the boot line");
        }
    }
}

#[cfg(test)]
mod web_token_tests {
    use super::{boot_token, hex_of};

    #[test]
    fn a_boot_token_is_thirty_two_lowercase_hex_characters() {
        assert_eq!(hex_of(&[0u8; 16]).len(), 32);
        assert_eq!(hex_of(&[0xde, 0xad, 0x00, 0x0f]), "dead000f");
        let t = boot_token().expect("/dev/urandom");
        assert_eq!(t.len(), 32);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(t, boot_token().unwrap());
    }

    #[test]
    fn the_console_url_carries_the_token() {
        // The banner's first line is the console through QEMU's port forward
        // with this boot's token; the lease decides whether a second follows
        // (`net_lease::console_urls`).
        let urls = crate::net_lease::console_urls("0123456789abcdef0123456789abcdef", &crate::net_lease::Lease::static_fallback());
        assert_eq!(urls, vec!["https://localhost:3345/embraOS?token=0123456789abcdef0123456789abcdef".to_string()]);
    }
}

#[cfg(test)]
mod first_run_tests {
    use super::is_first_run;

    #[test]
    fn only_a_missing_soul_reads_as_a_first_run() {
        // The text of embra-trustd's `NO_SOUL_ERROR`.
        assert!(is_first_run(
            "Soul document not found — no soul exists (first run or data loss)"
        ));
        // Every other report trustd can make: a failed verification, a halt.
        for failed in [
            "Hash mismatch",
            "Soul hash mismatch: computed=aa, stored=bb",
            "No stored soul hash at /embra/state/soul.sha256 while a soul document exists — restore STATE, or remove the soul with seed-state.sh --wipe data to start over",
            "Stored soul hash is empty",
            // A database whose own message says "not found", and the
            // sentence with anything around it: neither is a first boot.
            "WardSONDB error: collection not found",
            "The stored soul document lacks its soul field; the seal cannot be checked",
            "Soul document not found — no soul exists (first run or data loss) at /x",
            "Failed to read stored soul hash",
            "Failed to connect to WardSONDB",
            "Failed to parse WardSONDB response",
            "Soul document is null",
            "Expected hash aa but computed bb",
            // trustd's `SEALED_HASH_WITHOUT_SOUL_ERROR`: the document is gone
            // while a hash is stored — tamper or data loss, never a first run.
            "Soul document missing but a sealed hash is stored at /embra/state/soul.sha256 — restore DATA, or remove the hash to start over",
        ] {
            assert!(!is_first_run(failed), "{failed}");
        }
    }

    /// The three-way decision the soul gate makes, including the mismatch
    /// trustd reports with no words of its own (#52).
    #[test]
    fn the_soul_gate_passes_continues_a_first_boot_and_halts_everything_else() {
        use super::{soul_decision, SoulDecision};
        assert_eq!(soul_decision(true, "", "aa", "aa"), SoulDecision::Pass);
        assert_eq!(
            soul_decision(false, "Soul document not found — no soul exists (first run or data loss)", "", ""),
            SoulDecision::FirstRun
        );
        assert_eq!(
            soul_decision(false, "", "aa", "bb"),
            SoulDecision::Halt("Soul hash mismatch: computed=aa, stored=bb".to_string())
        );
        for failed in [
            "Hash mismatch",
            "WardSONDB error: collection not found",
            "Soul document missing but a sealed hash is stored at /embra/state/soul.sha256 — restore DATA, or remove the hash to start over",
        ] {
            assert_eq!(soul_decision(false, failed, "aa", "bb"), SoulDecision::Halt(failed.to_string()), "{failed}");
        }
    }
}

#[cfg(test)]
mod restart_budget_tests {
    use super::*;

    #[test]
    fn default_policy_is_a_burst_limit_that_cannot_self_reset_from_max_backoff() {
        let p = RestartPolicy::default();
        assert_eq!(p.max_restarts, 10);
        assert_eq!(p.backoff_base, Duration::from_secs(1));
        assert_eq!(p.backoff_max, Duration::from_secs(30));
        assert_eq!(p.stable_after, Duration::from_secs(60));
        // A crash loop pacing itself at the maximum backoff must never run
        // long enough to reset its own budget.
        assert!(p.stable_after > p.backoff_max);
    }

    #[test]
    fn budget_resets_only_after_a_restarted_service_runs_stably() {
        let window = Duration::from_secs(60);
        // Never restarted → nothing to reset, however long it has run.
        assert!(!restart_budget_reset_due(0, Duration::from_secs(3600), window));
        // Restarted and still inside the window → keep counting.
        assert!(!restart_budget_reset_due(3, Duration::from_secs(59), window));
        // Restarted and stable for the window → reset.
        assert!(restart_budget_reset_due(3, window, window));
        assert!(restart_budget_reset_due(9, Duration::from_secs(600), window));
    }

    /// A service that used its restart budget is marked `Halted` and is not
    /// started again: the budget bail precedes the spawn, so the test needs
    /// no binary (#52).
    #[tokio::test]
    async fn a_service_past_its_restart_budget_is_halted_and_not_started() {
        let mut sup = Supervisor::new();
        sup.services.push(ServiceState {
            def: ServiceDef {
                name: "probe".into(),
                binary: String::new(),
                args: vec![],
                env: vec![],
                health_check: HealthCheck::Http { url: "http://127.0.0.1:1/".into(), timeout: Duration::from_millis(10) },
                restart_policy: RestartPolicy { max_restarts: 2, ..RestartPolicy::default() },
                essential: true,
            },
            child: None,
            pid: None,
            started_at: None,
            restart_count: 2,
            status: ServiceStatus::Failed("exited".into()),
        });
        let err = sup.restart_service(0).await.unwrap_err();
        assert!(err.to_string().contains("permanently failed"), "{err}");
        assert_eq!(*sup.service_status(0), ServiceStatus::Halted);
        assert_eq!(sup.services[0].restart_count, 2, "no attempt was counted");
        // Halted stays halted: a second call is a no-op.
        assert!(sup.restart_service(0).await.is_ok());
        assert_eq!(*sup.service_status(0), ServiceStatus::Halted);
    }

    #[test]
    fn backoff_doubles_from_base_and_caps_at_max() {
        let p = RestartPolicy::default();
        let secs: Vec<u64> = (0..7).map(|n| backoff_for(&p, n).as_secs()).collect();
        assert_eq!(secs, vec![1, 2, 4, 8, 16, 30, 30]);
        // Absurd counts never overflow past the cap.
        assert_eq!(backoff_for(&p, 40), Duration::from_secs(30));
    }
}

#[cfg(test)]
mod stop_and_health_tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use tokio::io::AsyncBufReadExt;

    /// A health is a 200 status line, not the digits `200` anywhere (#29).
    #[test]
    fn a_health_is_a_200_status_line_and_nothing_else() {
        assert!(is_http_200("HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok"));
        assert!(is_http_200("HTTP/1.1 200 \r\n\r\n"));
        assert!(!is_http_200("HTTP/1.1 404 Not Found\r\nContent-Length: 200\r\n\r\n"));
        assert!(!is_http_200("HTTP/1.1 500 Internal Server Error\r\n\r\n{\"code\":200}"));
        assert!(!is_http_200("200 OK"));
        assert!(!is_http_200(""));
    }

    /// A stop is SIGTERM first; a service that exits on it is left its own
    /// status, one that ignores it is killed after the grace (#29).
    #[tokio::test]
    async fn a_stop_is_sigterm_first_and_sigkill_after_the_grace() {
        let _children = test_support::CHILDREN.lock().await;
        let mut polite = Command::new("sleep").arg("30").kill_on_drop(true).spawn().unwrap();
        let status = stop_one(&mut polite, "polite", Duration::from_secs(5)).await.unwrap();
        assert_eq!(status.signal(), Some(15), "ended by SIGTERM: {status:?}");
        // The shell itself stays the process (a `wait`, not an exec of the
        // last command), so the ignored SIGTERM is its own to ignore. It
        // says when its `trap` line is behind it, so the signal lands after
        // that however loaded the host is.
        let mut stubborn = Command::new("sh")
            .args(["-c", "trap '' TERM; echo ready; sleep 30 & wait"])
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut ready = tokio::io::BufReader::new(stubborn.stdout.take().unwrap()).lines();
        assert_eq!(ready.next_line().await.unwrap().as_deref(), Some("ready"));
        let status = stop_one(&mut stubborn, "stubborn", Duration::from_millis(300)).await.unwrap();
        assert_eq!(status.signal(), Some(9), "killed after the grace: {status:?}");
    }

    /// The serial console is started on the stdio kept from the boot: a
    /// console started after the log redirect draws on it, not on what
    /// embrad's stdout has become (#29). The stand-in console exits at
    /// once, which `start_service` reports as an error; what it wrote is
    /// the point.
    #[tokio::test]
    async fn a_restarted_console_draws_on_the_kept_boot_stdio() {
        let _children = test_support::CHILDREN.lock().await;
        let dir = std::env::temp_dir().join(format!("embrad-console-stdio-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let screen = dir.join("screen");
        let out = std::fs::File::create(&screen).unwrap();
        let input = std::fs::File::open("/dev/null").unwrap();
        let mut sup = Supervisor::new();
        sup.boot_stdio = Some((input, out));
        sup.services.push(ServiceState {
            def: ServiceDef {
                name: "embra-console".into(),
                binary: "/bin/sh".into(),
                args: vec!["-c".into(), "echo drawn".into()],
                env: vec![],
                health_check: HealthCheck::ProcessAlive,
                restart_policy: RestartPolicy::default(),
                essential: true,
            },
            child: None,
            pid: None,
            started_at: None,
            restart_count: 0,
            status: ServiceStatus::Stopped,
        });
        let _ = sup.start_service(0).await;
        assert_eq!(std::fs::read_to_string(&screen).unwrap(), "drawn\n");
        assert!(keep_boot_stdio().is_some(), "the boot's stdio can be kept");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod shutdown_during_restart_tests {
    use super::*;

    /// A shutdown signal during a restart's backoff ends the loop at once
    /// (#63): the service here is failed with a 30 s backoff ahead of it,
    /// and the loop returns well within that when the signal fires.
    #[tokio::test]
    async fn a_shutdown_signal_is_answered_during_a_restart_backoff() {
        let mut sup = Supervisor::new();
        sup.services.push(ServiceState {
            def: ServiceDef {
                name: "probe".into(),
                binary: String::new(),
                args: vec![],
                env: vec![],
                health_check: HealthCheck::ProcessAlive,
                restart_policy: RestartPolicy {
                    backoff_base: Duration::from_secs(30),
                    backoff_max: Duration::from_secs(30),
                    ..RestartPolicy::default()
                },
                essential: true,
            },
            child: None,
            pid: None,
            started_at: None,
            restart_count: 0,
            status: ServiceStatus::Failed("exited".into()),
        });
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = tx.send(());
        });
        let started = Instant::now();
        crate::reconcile::run_until(&mut sup, async move { rx.await.ok(); "the test" }, None).await;
        assert!(started.elapsed() < Duration::from_secs(5), "the backoff held the signal: {:?}", started.elapsed());
        assert_eq!(sup.services[0].restart_count, 1, "the restart had begun");
    }
}

#[cfg(test)]
mod launch_argument_tests {
    use super::*;

    /// The database is told to listen on loopback alone (#32): every
    /// in-guest client reaches it at `127.0.0.1:8090`, the QEMU scripts
    /// forward no 8090, and a bridged VM or bare metal keeps the one
    /// unauthenticated store off the wire. The pair rides beside the port.
    #[test]
    fn the_database_is_told_to_listen_on_loopback_alone() {
        let mut supervisor = Supervisor::new();
        supervisor.register_services();
        let args = supervisor.service_args("wardsondb").expect("the database is registered");
        let pair = args.windows(2).find(|w| w[0] == "--bind").expect("--bind is passed");
        assert_eq!(pair[1], "127.0.0.1");
        let port = args.windows(2).find(|w| w[0] == "--port").expect("--port is passed");
        assert_eq!(port[1], "8090");
    }

    /// The certificate-minting service listens to the machine alone as
    /// well: every client dials loopback (the round-2 verification of
    /// 2026-10-31, GitHub #10).
    #[test]
    fn the_trust_service_is_told_to_listen_on_loopback_alone() {
        let mut supervisor = Supervisor::new();
        supervisor.register_services();
        let args = supervisor.service_args("embra-trustd").expect("trustd is registered");
        let pair = args.windows(2).find(|w| w[0] == "--bind").expect("--bind is passed");
        assert_eq!(pair[1], "127.0.0.1");
        let port = args.windows(2).find(|w| w[0] == "--port").expect("--port is passed");
        assert_eq!(port[1], "50001");
    }
}

#[cfg(test)]
mod display_console_tests {
    use super::*;

    fn registered(cmdline: &str, display_present: bool, kiosk_available: bool) -> Supervisor {
        let mut sup = Supervisor::with_cmdline(Cmdline::parse(cmdline));
        sup.register_services_with(display_present, kiosk_available);
        sup
    }

    fn names(sup: &Supervisor) -> Vec<String> {
        sup.services.iter().map(|s| s.def.name.clone()).collect()
    }

    /// Under the web console, with a display and the launcher in the image,
    /// udevd and the kiosk follow embra-web, both inessential, and nothing
    /// else is inessential; without a display, with the knob off, in an
    /// image without the launcher, or on the serial TUI there is neither.
    #[test]
    fn the_display_console_is_registered_after_the_web_console_and_only_with_a_display() {
        let sup = registered("embra.web=1", true, true);
        let n = names(&sup);
        let web = n.iter().position(|x| x == "embra-web").expect("embra-web registered");
        assert_eq!(&n[web..], ["embra-web", "udevd", "embra-kiosk"]);
        assert!(sup.services[..=web].iter().all(|s| s.def.essential), "everything up to embra-web is essential");
        assert!(sup.services[web + 1..].iter().all(|s| !s.def.essential), "the display console is not");
        assert!(sup.kiosk);
        for (cmdline, display, launcher, why) in [
            ("embra.web=1", false, true, "no display"),
            ("embra.web=1 embra.kiosk=0", true, true, "the knob off"),
            ("embra.web=1", true, false, "no launcher in the image"),
            ("embra.tui=1", true, true, "the serial TUI"),
        ] {
            let sup = registered(cmdline, display, launcher);
            let n = names(&sup);
            assert!(!n.iter().any(|x| x == "udevd" || x == "embra-kiosk"), "{why}: {n:?}");
            assert!(!sup.kiosk, "{why}");
        }
    }

    /// The token reaches the kiosk through COG_URL with the kiosk flag, never
    /// through an argument; the web token's own variable and the Mesa switch
    /// that would break the nested display are absent; the software path is
    /// named.
    #[test]
    fn the_display_console_takes_the_console_url_from_its_environment_and_its_argv_carries_no_token() {
        let sup = registered("embra.web=1 embra.kbd=de", true, true);
        let token = sup.web_token.clone().expect("a token in web mode");
        let kiosk = sup.services.iter().find(|s| s.def.name == "embra-kiosk").expect("the kiosk");
        let env = |k: &str| kiosk.def.env.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        let url = env("COG_URL").expect("COG_URL");
        assert!(url.starts_with("https://127.0.0.1:3345/embraOS?token="), "{url}");
        assert!(url.contains(&format!("token={token}")) && url.ends_with("&kiosk=1"), "{url}");
        assert!(kiosk.def.args.iter().all(|a| !a.contains(&token)), "no token in argv: {:?}", kiosk.def.args);
        assert!(kiosk.def.args.contains(&"--platform=drm".to_string()));
        assert_eq!(env("EMBRA_WEB_TOKEN"), None);
        assert_eq!(env("LIBGL_ALWAYS_SOFTWARE"), None);
        assert_eq!(env("MESA_LOADER_DRIVER_OVERRIDE").as_deref(), Some("kms_swrast"));
        assert_eq!(env("GBM_ALWAYS_SOFTWARE").as_deref(), Some("1"));
        assert_eq!(env("XKB_DEFAULT_LAYOUT").as_deref(), Some("de"));
        assert_eq!(env("XDG_RUNTIME_DIR").as_deref(), Some("/run/user/0"));
    }

    #[test]
    fn the_device_scale_is_two_from_2560_pixels() {
        assert_eq!(device_scale(0), 1);
        assert_eq!(device_scale(1024), 1);
        assert_eq!(device_scale(1920), 1);
        assert_eq!(device_scale(2559), 1);
        assert_eq!(device_scale(2560), 2);
        assert_eq!(device_scale(3840), 2);
    }

    /// An inessential service that exits at once is logged and the boot
    /// goes on; an essential one that does is the boot's error.
    #[tokio::test]
    async fn a_failing_display_console_never_halts_the_boot() {
        let _children = test_support::CHILDREN.lock().await;
        let def = |name: &str, script: &str, essential: bool| ServiceDef {
            name: name.into(),
            binary: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![],
            health_check: HealthCheck::ProcessAlive,
            restart_policy: RestartPolicy::default(),
            essential,
        };
        let mut sup = Supervisor::new();
        sup.add_service(def("stays", "sleep 30", true));
        sup.add_service(def("embra-kiosk", "exit 1", false));
        assert!(sup.start_all().await.is_ok(), "the boot continues past the inessential failure");
        assert!(matches!(sup.services[1].status, ServiceStatus::Failed(_)));
        assert_eq!(sup.services[0].status, ServiceStatus::Running);
        sup.stop_all().await;

        let mut sup = Supervisor::new();
        sup.add_service(def("essential", "exit 1", true));
        assert!(sup.start_all().await.is_err(), "an essential failure is the boot's error");
    }
}
