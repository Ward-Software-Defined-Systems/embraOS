//! Service supervisor for embrad.
//!
//! Manages the lifecycle of all embraOS services:
//! - Start in dependency order
//! - Health check polling
//! - Restart on failure with exponential backoff
//! - Graceful shutdown in reverse order

use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tracing::{info, warn, error};

use embra_boot::{Cmdline, KioskMode, KioskPaint};

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
    /// The web console's access token (web mode): the one kept on STATE, or
    /// made and stored by this boot (`embra_boot::web_token`). It is handed
    /// to embra-web in its environment and printed in the boot banner; a
    /// request of the operator's replaces it while the services run
    /// (`rotate_web_token`).
    web_token: Option<String>,
    /// Whether the token is on STATE, so that the next boot's is the same
    /// one: the banner says which.
    web_token_stored: bool,
    /// Where the token is kept: STATE's directory for it (a test passes its
    /// own).
    web_token_dir: PathBuf,
    /// A request for a new token failed in this boot. It is not tried again
    /// every few seconds; the request stays on STATE for the next boot.
    web_token_rotation_failed: bool,
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
    /// The model server of native models (experimental): no service of the
    /// list above, kept by the reconcile loop's tick as the settings on
    /// STATE say (`native_model.rs`).
    model_server: crate::native_model::Keeper,
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
            web_token_stored: false,
            web_token_dir: web_token_dir(),
            web_token_rotation_failed: false,
            boot_stdio: None,
            services: Vec::new(),
            model_server: crate::native_model::Keeper::new(cmdline.native()),
            cmdline,
            kiosk: false,
        }
    }

    /// One tick of the model server's keeper: the reconcile loop calls it
    /// after the pass over the services.
    pub async fn keep_model_server(&mut self) {
        self.model_server.tick().await;
    }

    /// Register all embraOS services in dependency order: `start_all`
    /// starts them in the order they are registered here.
    pub fn register_services(&mut self) {
        self.register_services_with(display_present(), Path::new(KIOSK_BIN).exists(), Path::new(DRM_SYSFS));
    }

    /// `register_services` with the machine's facts passed in: whether a
    /// display exists (`/dev/dri/card*`), whether this image carries the
    /// display console's launcher, and the directory where the kernel lists
    /// the displays (`/sys/class/drm`). The tests pass all three.
    pub fn register_services_with(&mut self, display_present: bool, kiosk_available: bool, drm: &Path) {
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
                // Loopback alone: the console, embra-web and the health
                // check dial 127.0.0.1, and the stream answers without
                // authentication. The REST port stays on every interface.
                "--grpc-bind".to_string(), "127.0.0.1".to_string(),
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
        // A provider without a key gets none (`key_sources`); a preset's
        // bearer is read separately below and threaded via EMBRA_*_BEARER
        // env vars.
        let api_key = match key_sources(&api_provider) {
            None => String::new(),
            Some((per_provider_path, env_key_name)) => std::fs::read_to_string(per_provider_path)
                .or_else(|_| std::fs::read_to_string("/embra/state/api_key"))
                .or_else(|_| std::env::var(env_key_name))
                .unwrap_or_default()
                .trim().to_string(),
        };
        // Loopback alone: embra-apid beside it is the brain's one client.
        let mut brain_args = vec![
            "--bind".to_string(), "127.0.0.1".to_string(),
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
            // The web console's gate: the token kept on STATE, or a new one
            // stored there (a first boot, the operator's request), in
            // embra-web's environment (never an argument: `ps` shows those)
            // and in the URL the banner prints. Without a token there is no
            // gate, and an open console is worse than none: the error stays
            // on the serial console, and `EMBRA_TUI=1` still boots the TUI.
            use embra_boot::web_token::Origin;
            let boot = match embra_boot::web_token::for_boot(&self.web_token_dir) {
                Ok(b) => b,
                Err(e) => {
                    error!("Web mode: no access token could be minted ({e}); embra-web is NOT started. Boot with EMBRA_TUI=1 for the serial console.");
                    return;
                }
            };
            match &boot.origin {
                Origin::Kept => info!("Web console: the access token kept on STATE"),
                Origin::Made => info!("Web console: a new access token, stored on STATE"),
                Origin::NotStored(e) => warn!("Web console: a new access token that could not be stored on STATE ({e}); the next boot makes another"),
            }
            self.web_token_stored = !matches!(boot.origin, Origin::NotStored(_));
            let token = boot.token;
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
                    let wanted = self.cmdline.kiosk_mode();
                    let mut mode = kiosk_video_mode(wanted, &connected_display_modes(drm));
                    if mode.is_none() {
                        // The kernel's list may be a shortened one
                        // (`redetect_displays`): the displays are detected
                        // once more before the display console is left
                        // with the display's own mode, and before the
                        // device scale is taken from that list.
                        if video_option_forces_a_connector(self.cmdline.tokens()) {
                            info!("Display console: a video= option on the kernel line forces a connector; the displays are not detected again");
                        } else {
                            redetect_displays(drm);
                            mode = kiosk_video_mode(wanted, &connected_display_modes(drm));
                            if let Some((w, h)) = mode {
                                info!("Display console: {w}x{h} is in the kernel's list after the displays were detected again");
                            }
                        }
                    }
                    if let (Some(KioskMode::Size(w, h)), None) = (wanted, mode) {
                        warn!("embra.kioskmode={w}x{h}: not a mode every connected display offers; the display console keeps the display's own");
                    }
                    match mode {
                        Some((w, h)) => info!("Display console: asking the display for {w}x{h}"),
                        None => info!("Display console: the display's own video mode"),
                    }
                    let width = mode.map_or_else(|| display_width_px(drm), |(w, _)| w);
                    let scale = self.cmdline.kiosk_scale().unwrap_or_else(|| device_scale(width));
                    let paint = self.cmdline.kiosk_paint();
                    if paint == KioskPaint::Gpu {
                        info!("Display console: the page is drawn by Skia's GPU backend, WebKit's default (embra.kioskpaint=gpu)");
                    }
                    self.add_service(ServiceDef {
                        name: "embra-kiosk".to_string(),
                        binary: KIOSK_BIN.to_string(),
                        args: kiosk_args(scale),
                        env: kiosk_env(&token, self.cmdline.keyboard_layout(), mode, paint),
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
                    // One line an address: the forwarded one always, then
                    // every address this machine holds on a network (a
                    // lease on a bridged network or a real host, an address
                    // the operator set). QEMU's user network leases
                    // 10.0.2.15, which no line names (unreachable from
                    // outside).
                    let leases = embra_boot::net_lease::read();
                    let urls = embra_boot::net_lease::console_urls(self.web_token.as_deref().unwrap_or(""), &leases);
                    for line in banner_url_lines(&urls, leases.is_empty()) {
                        let _ = writeln!(b, "{line}");
                    }
                    // Printed here and nowhere else, except where the display
                    // console runs: its menu shows the URL again (embra-web's
                    // `/api/console-url`, answered to the machine's own browser
                    // alone), the display console covering this copy.
                    let _ = writeln!(b, "    \x1b[2m{}\x1b[0m", banner_token_note(self.web_token_stored, self.kiosk));
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
                    // A clock that is still the image's build time: every
                    // certificate date is checked against it, and this is
                    // where the operator learns why they are refused.
                    for line in crate::clock::banner_lines(embra_boot::ClockStatus::read().as_ref()) {
                        let _ = writeln!(b, "    \x1b[1;33m{line}\x1b[0m");
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
            cmd.stderr(match service_log(Path::new(log_path)) {
                Ok(f) => Stdio::from(f),
                Err(_) => Stdio::null(),
            });
        } else {
            let log_path = format!("/embra/ephemeral/{}.log", svc.def.name);
            let (out, err) = match service_log(Path::new(&log_path)) {
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
        // The model server after the brain, its one client.
        self.model_server.stop().await;
    }

    /// The pids of the supervised children that are running: what the
    /// orphan reaper (`reconcile::reap_orphans`) leaves for tokio's `Child`
    /// to collect.
    pub fn child_pids(&self) -> Vec<u32> {
        self.services
            .iter()
            .filter_map(|s| s.child.as_ref().and_then(|c| c.id()))
            .chain(self.model_server.pids())
            .collect()
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

    /// Whether the operator asked for a new access token (`/web-token
    /// rotate`: the brain leaves the request on STATE) and the loop has it
    /// to make: a web console runs on a token, and no request failed in
    /// this boot.
    pub fn web_token_rotation_asked(&self) -> bool {
        self.web_token.is_some()
            && !self.web_token_rotation_failed
            && embra_boot::web_token::rotation_asked(&self.web_token_dir)
    }

    /// The new token in this supervisor and in the environment of the two
    /// services that carry it; they take it at their next start.
    fn take_web_token(&mut self, token: String) {
        for svc in &mut self.services {
            match svc.def.name.as_str() {
                "embra-web" => set_env(&mut svc.def.env, WEB_TOKEN_ENV, token.clone()),
                "embra-kiosk" => set_env(&mut svc.def.env, KIOSK_URL_ENV, kiosk_url(&token)),
                _ => {}
            }
        }
        self.web_token = Some(token);
        self.web_token_stored = true;
    }

    /// The operator's request for a new access token. A new token is made
    /// and stored, and the request removed; then the display console and
    /// the web console are stopped and started again with it, which ends
    /// every connection the old token opened and signs every browser out
    /// (a browser's cookie is the old token). The new URLs go to both
    /// consoles as the banner's did, and never to a log: the tools read the
    /// logs. A request that cannot be answered is said on the consoles, the
    /// token stays as it is, and the request waits for the next boot.
    pub async fn rotate_web_token(&mut self) {
        let token = match embra_boot::web_token::rotate(&self.web_token_dir) {
            Ok(t) => t,
            Err(e) => {
                self.web_token_rotation_failed = true;
                error!("the web console's access token was not made again: {e}; the token stays as it is, and the request is answered by the next boot");
                embra_boot::console::BootConsole::open()
                    .line(&format!("[embrad] the web console's access token was not made again ({e}); the console stays at its URL, and the next boot makes the new token"));
                return;
            }
        };
        info!("The web console's access token was made again on the operator's request; restarting the web console");
        self.take_web_token(token.clone());
        // The display console is a client of the web console: it goes
        // first and comes back last, on the new URL.
        let kiosk = self.services.iter().position(|s| s.def.name == "embra-kiosk");
        let web = self.services.iter().position(|s| s.def.name == "embra-web");
        let kiosk_ran = kiosk.is_some_and(|i| self.services[i].child.is_some());
        if let Some(i) = kiosk.filter(|_| kiosk_ran) {
            self.stop_service(i).await;
        }
        if let Some(i) = web {
            // A web console that is not running (failed, and waiting for its
            // restart) takes the new token when the loop starts it.
            if self.services[i].child.is_some() {
                self.stop_service(i).await;
                if let Err(e) = self.start_service(i).await {
                    error!("embra-web did not come back with the new access token: {e}");
                    self.services[i].status = ServiceStatus::Failed(e.to_string());
                }
            }
        }
        if let Some(i) = kiosk.filter(|_| kiosk_ran)
            && let Err(e) = self.start_service(i).await
        {
            warn!("embra-kiosk did not come back on the new URL: {e}");
            self.services[i].status = ServiceStatus::Failed(e.to_string());
        }
        let leases = embra_boot::net_lease::read();
        let mut consoles = embra_boot::console::BootConsole::open();
        for line in new_token_lines(&token, &leases, self.kiosk) {
            consoles.line(&line);
        }
    }

    /// Stops one service and leaves it stopped: its caller starts it again.
    async fn stop_service(&mut self, index: usize) {
        let name = self.services[index].def.name.clone();
        info!("Stopping {name}");
        if let Some(ref mut child) = self.services[index].child {
            stop_one(child, &name, STOP_GRACE).await;
        }
        self.services[index].status = ServiceStatus::Stopped;
        self.services[index].child = None;
        if name == "embra-kiosk" {
            release_display();
        }
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
pub(crate) const STOP_GRACE: Duration = Duration::from_secs(5);

/// The providers the brain reaches without an API key: the
/// OpenAI-compatible presets, whose stored names these are. Any other name,
/// and none, takes a key.
pub(crate) const KEYLESS_PROVIDERS: [&str; 3] = ["ollama", "lm_studio", "native"];

/// Where the key of the stored provider comes from: its file on STATE and
/// the environment variable that stands in for it. `None` for a provider
/// that has no key. A name that is no provider, and no name, read as
/// Anthropic, as they do in the brain.
pub(crate) fn key_sources(api_provider: &str) -> Option<(&'static str, &'static str)> {
    if KEYLESS_PROVIDERS.contains(&api_provider) {
        return None;
    }
    Some(match api_provider {
        "gemini" => ("/embra/state/api_key_gemini", "GEMINI_API_KEY"),
        _ => ("/embra/state/api_key_anthropic", "ANTHROPIC_API_KEY"),
    })
}

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
/// The banner's lines for the console's URLs: the forwarded one, then one
/// for every address on a network with how the machine came by it; and a
/// line that says so when it holds no address at all.
fn banner_url_lines(urls: &[embra_boot::net_lease::ConsoleUrl<'_>], no_address: bool) -> Vec<String> {
    use embra_boot::net_lease::Source;
    let mut lines: Vec<String> = urls
        .iter()
        .map(|u| {
            let place = match u.lease {
                None => "QEMU hostfwd; accept the embraOS-CA cert".to_string(),
                Some(l) if l.source == Source::Static => format!("the address set for {}, {}/{}", l.interface, l.ip, l.prefix),
                Some(l) => format!("the address DHCP leased to this machine, {}/{} on {}", l.ip, l.prefix, l.interface),
            };
            format!("    \x1b[1;36m{}\x1b[0m  \x1b[2m({place})\x1b[0m", u.url)
        })
        .collect();
    if no_address {
        lines.push("    \x1b[2m(no address on a network yet; DHCP is still being asked on every card)\x1b[0m".to_string());
    }
    lines
}

/// What both consoles are told after the operator's request for a new
/// access token: what happened, and the console's URLs as the banner prints
/// them, each with where it works.
fn new_token_lines(token: &str, leases: &[embra_boot::net_lease::Lease], kiosk: bool) -> Vec<String> {
    let urls = embra_boot::net_lease::console_urls(token, leases);
    let mut lines = vec![
        "[embrad] The web console has a new access token (/web-token rotate): every browser is signed out. The console is at:".to_string(),
    ];
    lines.extend(banner_url_lines(&urls, leases.is_empty()));
    if kiosk {
        lines.push("    \x1b[2mThe display console is on the new URL, and its menu shows it (the right mouse button).\x1b[0m".to_string());
    }
    lines
}

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

/// Where the kernel lists the displays: a directory per connector
/// (`card0-Virtual-1`) with its `status` and its `modes`.
const DRM_SYSFS: &str = "/sys/class/drm";

/// The connector directories of the kernel's list (`card0-Virtual-1`; never
/// `card0` itself, a render node or `version`), in name order.
fn connector_dirs(drm: &Path) -> Vec<PathBuf> {
    let Ok(dir) = std::fs::read_dir(drm) else { return Vec::new() };
    let mut paths: Vec<_> = dir
        .flatten()
        .map(|e| e.path())
        .filter(|path| {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            name.starts_with("card") && name.contains('-')
        })
        .collect();
    paths.sort();
    paths
}

/// The width of the display's first mode, from sysfs: a connector's `modes`
/// file lists `<width>x<height>` lines (`/sys/class/drm/card0-Virtual-1/modes`).
/// 0 when no connector names one.
fn display_width_px(drm: &Path) -> u32 {
    for path in connector_dirs(drm) {
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

/// The mode the display console asks for when `embra.kioskmode` names none:
/// wide enough for the console's status row on one line, where a virtual
/// display's own default (1280x800 under OVMF, 1024x768 elsewhere) wraps it.
const DEFAULT_KIOSK_MODE: (u32, u32) = (1920, 1080);

/// The mode names of every connected connector, from sysfs
/// (`/sys/class/drm/card0-Virtual-1/status` and `modes`): what the display
/// console's mode is chosen from.
fn connected_display_modes(drm: &Path) -> Vec<Vec<String>> {
    let mut displays = Vec::new();
    for path in connector_dirs(drm) {
        let connected = std::fs::read_to_string(path.join("status")).is_ok_and(|s| s.trim() == "connected");
        let modes: Vec<String> = std::fs::read_to_string(path.join("modes"))
            .map(|m| m.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
            .unwrap_or_default();
        if connected && !modes.is_empty() {
            displays.push(modes);
        }
    }
    displays
}

/// Asks the kernel to detect every display again: `detect` written to a
/// connector's `status` makes the kernel read that connector's modes afresh,
/// up to the largest the device takes (`status_store`, drm_sysfs.c). A
/// connector's `modes` is otherwise the list its last reader left, and the
/// kernel's own console reads it again after every display event (a monitor
/// plugged in, a VM window reporting its size) with its own framebuffer's
/// size as the limit, so every larger mode leaves the list
/// (`drm_fb_helper_hotplug_event`). Under a QEMU window that event arrives a
/// second after the kernel's console appears at 1280x800, and 1920x1080 is
/// gone from the list before it is read here. Best effort: a connector that
/// cannot be written keeps the list it has.
fn redetect_displays(drm: &Path) {
    for path in connector_dirs(drm) {
        // Never created: sysfs has the file, or the connector is gone.
        if let Ok(mut status) = std::fs::OpenOptions::new().write(true).truncate(true).open(path.join("status")) {
            let _ = std::io::Write::write_all(&mut status, b"detect");
        }
    }
}

/// Whether a `video=` option on the kernel line forces a connector on or off
/// (`video=HDMI-A-1:D`, `video=1024x768e`; the letters `e`, `D` and `d` after
/// the connector's name, drm_modes.c). `detect` written to a connector's
/// `status` clears that force, so a line with such an option keeps the
/// displays from being detected again. A firmware-framebuffer option whose
/// value carries one of the letters (`video=efifb:mode=3`) counts too: the
/// cost is the second look, never an operator's word undone.
fn video_option_forces_a_connector(tokens: &[String]) -> bool {
    tokens.iter().filter_map(|t| t.strip_prefix("video=")).any(|value| {
        let spec = value.rsplit_once(':').map_or(value, |(_, spec)| spec);
        spec.split(',').next().unwrap_or("").contains(['e', 'D', 'd'])
    })
}

/// The video mode cog is told to set (`COG_PLATFORM_DRM_VIDEO_MODE`), or
/// `None` for the display's own, which is cog's choice without the variable:
/// the connector's preferred mode. `embra.kioskmode=native` asks for none;
/// a size, or the default without the knob, is asked for only when every
/// connected display lists it, because cog takes the named mode or no mode at
/// all and a display console that names a missing mode does not start. The
/// firmware's framebuffer has one mode, the one GRUB asked the kernel for
/// (`video=efifb:`), and lists that alone.
fn kiosk_video_mode(wanted: Option<KioskMode>, displays: &[Vec<String>]) -> Option<(u32, u32)> {
    let (w, h) = match wanted {
        Some(KioskMode::Native) => return None,
        Some(KioskMode::Size(w, h)) => (w, h),
        None => DEFAULT_KIOSK_MODE,
    };
    let name = format!("{w}x{h}");
    (!displays.is_empty() && displays.iter().all(|modes| modes.contains(&name))).then_some((w, h))
}

/// The device scale for a display's width: 1 below 2560 pixels, 2 from there.
fn device_scale(width_px: u32) -> u8 {
    if width_px >= 2560 { 2 } else { 1 }
}

/// What the display console's launcher does when its renderer (WebKit's web
/// process) dies: it starts another and loads the page it was started with.
/// Not `restart`, which reloads the address the view had reached: the start
/// URL carries the token, the web console exchanges it for a cookie and
/// redirects to the clean address, and that cookie (`SameSite=Strict`) does
/// not travel with the reload a new renderer makes, so the reload is answered
/// with the refusal notice, black letters on the display console's own
/// background. The action is one of this tree's cog patches
/// (`buildroot/patches/cog/0007-…`), and the image build looks for it in the
/// launcher it built (`assert_display_console_launcher`).
const KIOSK_FAILURE_ACTION: &str = "restart-home";

/// The display console's arguments: cog's DRM platform, the console's
/// background until the first paint, TLS errors ignored on the loopback
/// (the certificate is this machine's own, 127.0.0.1 among its names), a
/// renderer that died started again on the console's page rather than an
/// error page nobody can dismiss, the device scale.
fn kiosk_args(scale: u8) -> Vec<String> {
    vec![
        "--platform=drm".to_string(),
        "--bg-color=#0c0907".to_string(),
        "--ignore-tls-errors".to_string(),
        format!("--webprocess-failure={KIOSK_FAILURE_ACTION}"),
        format!("--device-scale={scale}"),
    ]
}

/// The variable cog reads its page's URL from.
const KIOSK_URL_ENV: &str = "COG_URL";
/// The variable embra-web reads its token from (`embra-web`'s `access.rs`).
const WEB_TOKEN_ENV: &str = "EMBRA_WEB_TOKEN";

/// The display console's page: the web console on loopback, with the access
/// token and the flag that tells the page it is on the display.
fn kiosk_url(token: &str) -> String {
    format!("https://127.0.0.1:3345/embraOS?token={token}&kiosk=1")
}

/// A service's variable, replaced or added.
fn set_env(env: &mut Vec<(String, String)>, key: &str, value: String) {
    match env.iter_mut().find(|(k, _)| k == key) {
        Some(pair) => pair.1 = value,
        None => env.push((key.to_string(), value)),
    }
}

/// Where the web console's token is kept. A test build's default is a
/// place nothing can be stored in, so no test touches STATE or leaves a
/// directory behind: its supervisor runs on a token that is not kept, and a
/// test of the keeping passes a directory of its own.
#[cfg(not(test))]
fn web_token_dir() -> PathBuf {
    PathBuf::from(embra_boot::web_token::DIR)
}

#[cfg(test)]
fn web_token_dir() -> PathBuf {
    PathBuf::from("/dev/null/embrad-test-web-token")
}

/// The banner's line under the console's URLs: what the token in them is,
/// and where the URL is shown. A token that could not be stored is named as
/// this boot's: the next boot's URL differs.
fn banner_token_note(stored: bool, kiosk: bool) -> String {
    let token = if stored {
        "The access token is in the URL and stays the same across boots (/web-token rotate makes a new one)"
    } else {
        "This boot's access token is in the URL (it could not be stored on STATE, so the next boot makes another)"
    };
    // Printed here and nowhere else, except where the display console runs:
    // its menu shows the URL again (embra-web's `/api/console-url`, answered
    // to the machine's own browser alone), the display console covering
    // this copy.
    let shown = if kiosk {
        "it is shown here and in the display console's menu (the right mouse button)"
    } else {
        "it is shown here only"
    };
    format!("{token}; the URL is the credential, and {shown}.")
}

/// The display console's environment: the console URL with the access
/// token in `COG_URL` (never an argument: `ps` shows those; `?kiosk=1` tells
/// the page it is on the display), Mesa's software path on the card
/// (kms_swrast through GBM; never `LIBGL_ALWAYS_SOFTWARE`, which would send
/// the web process down a path the nested display does not serve), the
/// writable homes on the ephemeral tmpfs, the keyboard layout, the video
/// mode when the display console asks for one (`kiosk_video_mode`), and
/// how the page is drawn (`KIOSK_CPU_PAINT_ENV`).
fn kiosk_env(token: &str, layout: Option<&str>, mode: Option<(u32, u32)>, paint: KioskPaint) -> Vec<(String, String)> {
    let mut env = vec![
        (KIOSK_URL_ENV.to_string(), kiosk_url(token)),
        ("MESA_LOADER_DRIVER_OVERRIDE".to_string(), "kms_swrast".to_string()),
        ("GBM_ALWAYS_SOFTWARE".to_string(), "1".to_string()),
        ("XDG_RUNTIME_DIR".to_string(), XDG_RUNTIME_DIR.to_string()),
        ("HOME".to_string(), KIOSK_HOME.to_string()),
        ("XDG_CACHE_HOME".to_string(), format!("{KIOSK_HOME}/cache")),
        ("XDG_DATA_HOME".to_string(), format!("{KIOSK_HOME}/data")),
        ("XDG_CONFIG_HOME".to_string(), format!("{KIOSK_HOME}/config")),
        ("XKB_DEFAULT_LAYOUT".to_string(), layout.unwrap_or("us").to_string()),
    ];
    if let Some((w, h)) = mode {
        env.push(("COG_PLATFORM_DRM_VIDEO_MODE".to_string(), format!("{w}x{h}")));
    }
    if paint == KioskPaint::Cpu {
        env.push((KIOSK_CPU_PAINT_ENV.to_string(), "1".to_string()));
    }
    env
}

/// WebKit's switch for Skia's CPU code (`WebProcessGLib.cpp`: any value but
/// `0` turns accelerated buffers off in the web process). WebKit's default
/// is Skia's GPU backend with painting threads of its own, and in this
/// image the GL under it is software: GPU code with no GPU. That backend's
/// resource cache is where a display console's renderer died on a machine
/// (a bounds check in `GrResourceCache::refAndMakeResourceMRU`), so the
/// display console draws with the CPU code; the screen is still composed
/// through GL. `embra.kioskpaint=gpu` leaves the variable out for one boot.
const KIOSK_CPU_PAINT_ENV: &str = "WEBKIT_SKIA_ENABLE_CPU_RENDERING";

/// A service's log for the run that starts now, with the log of the run
/// before it kept beside it as `<name>.log.prev`: a service that is started
/// again (a crash, the display console's launcher after its budget) would
/// otherwise begin by erasing the one account of why. One run back and no
/// further; the ephemeral tmpfs is small. A log that cannot be kept is no
/// reason not to start: the rename is best effort, the create decides.
fn service_log(path: &Path) -> std::io::Result<std::fs::File> {
    let mut kept = path.as_os_str().to_owned();
    kept.push(".prev");
    match std::fs::rename(path, &kept) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!("{}: the log of the run before could not be kept ({e})", path.display()),
    }
    std::fs::File::create(path)
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
/// rule may rename a card), and waited for; libinput enumerates what udev has
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

/// How the machine is stopped once the services are: what a signal asks of
/// PID 1 (`reconcile::run`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    /// Through the firmware and the boot menu again: the operator's
    /// `/reboot now`, and the end of every boot-menu job.
    Restart,
    /// Powered off: the operator's `/reboot halt`. A machine that cannot
    /// power itself off halts (the kernel's own fallback), and is switched
    /// off by hand.
    PowerOff,
}

impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Stop::Restart => "a restart",
            Stop::PowerOff => "a power-off",
        })
    }
}

/// What the operator at a console reads before the power goes: the display
/// has its text back by now (the display console is stopped), and a machine
/// that cannot power itself off keeps the line on its screen.
const POWER_OFF_LINE: &str =
    "[embrad] embraOS has stopped. The machine powers off now; if it stays on, it is safe to switch it off.";

/// The end of PID 1: the disk's filesystems are closed (`settle_the_disks`)
/// and the machine restarts or is powered off. The services are stopped by
/// now (`Supervisor::stop_all`), or never ran (a boot-menu job).
#[cfg(target_os = "linux")]
pub(crate) fn stop_machine(stop: Stop) -> ! {
    info!("Stopping the machine: {stop}");
    if std::process::id() != 1 {
        // Development: embrad is not PID 1, so the machine is not the OS
        // and is not ours to stop, and its processes are not ours to end.
        info!("not PID 1 — exiting instead of stopping the host");
        std::process::exit(0);
    }
    settle_the_disks();
    let command = match stop {
        Stop::Restart => libc::LINUX_REBOOT_CMD_RESTART,
        Stop::PowerOff => {
            embra_boot::console::BootConsole::open().line(POWER_OFF_LINE);
            // Let the consoles drain the line before the power goes.
            std::thread::sleep(Duration::from_millis(500));
            libc::LINUX_REBOOT_CMD_POWER_OFF
        }
    };
    // SAFETY: reboot(2) with a command and no argument; it does not return
    // when it succeeds.
    unsafe { libc::reboot(command); }
    loop { std::thread::sleep(Duration::from_secs(3600)); }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn stop_machine(stop: Stop) -> ! {
    info!("{stop} requested (dev mode, would stop the machine on Linux)");
    std::process::exit(0);
}

/// A restart, for the boot-menu jobs and a scheduled restore, which end in
/// one whatever happened.
pub(crate) fn reboot_system() -> ! {
    stop_machine(Stop::Restart)
}

/// The mounts on the machine's disk, innermost first: the workspace is a
/// bind mount of a directory on DATA (`mount.rs`).
#[cfg(target_os = "linux")]
const DISK_MOUNTS: [&str; 3] = ["/embra/workspace", "/embra/data", "/embra/state"];

/// How long every other process has to exit on SIGTERM before SIGKILL, and
/// how long the kernel has to take the killed ones down.
#[cfg(target_os = "linux")]
const LAST_PROCESSES_GRACE: Duration = Duration::from_secs(2);
#[cfg(target_os = "linux")]
const LAST_PROCESSES_KILLED: Duration = Duration::from_secs(1);

/// Before the machine restarts or powers off, the filesystems on its disk
/// are closed. Every process but this one is ended first (SIGTERM, a grace,
/// SIGKILL): an orphan a tool left behind, a build of the Guardian's, would
/// hold the workspace open. Then the workspace, DATA and STATE are unmounted,
/// or remounted read-only when something still holds one. An ext4 that is
/// neither keeps its journal open and is replayed at the next mount, as
/// after a power cut: `sync` alone left both that way at every restart.
/// Every step is bounded and none is fatal: what cannot be closed has been
/// synced, as before.
#[cfg(target_os = "linux")]
fn settle_the_disks() {
    end_every_other_process();
    // SAFETY: sync(2) takes no arguments and cannot fail.
    unsafe { libc::sync(); }
    for path in DISK_MOUNTS {
        match close_filesystem(Path::new(path)) {
            Closed::Unmounted => info!("{path}: unmounted"),
            Closed::ReadOnly => info!("{path}: busy, remounted read-only"),
            Closed::NotMounted => {}
            Closed::Left(e) => warn!("{path}: neither unmounted nor remounted read-only ({e}); its journal is replayed at the next boot"),
        }
    }
    // SAFETY: as above.
    unsafe { libc::sync(); }
}

/// SIGTERM to every process but PID 1, `LAST_PROCESSES_GRACE` for them to
/// go, SIGKILL to what is left. `kill(-1, …)` from PID 1 reaches every
/// process except PID 1 itself; every one of them is a child of PID 1 by
/// now or becomes one when its parent goes, so "no child left" is "no
/// process left". PID 1 only: anywhere else `-1` is every process the caller
/// may signal.
#[cfg(target_os = "linux")]
fn end_every_other_process() {
    if std::process::id() != 1 {
        return;
    }
    // SAFETY: kill(2) with a pid and a signal; no pointer.
    unsafe { libc::kill(-1, libc::SIGTERM); }
    if !no_child_left_within(LAST_PROCESSES_GRACE) {
        // SAFETY: as above.
        unsafe { libc::kill(-1, libc::SIGKILL); }
        no_child_left_within(LAST_PROCESSES_KILLED);
    }
}

/// Collects exited children until none is left (`true`) or the time is up.
#[cfg(target_os = "linux")]
fn no_child_left_within(limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        let mut status = 0;
        // SAFETY: a non-blocking wait for any child; `status` is a valid
        // pointer for the call.
        let r = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if r > 0 {
            continue;
        }
        if r < 0 {
            // ECHILD: nothing is left to wait for.
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// What became of one mount at the end of a boot.
#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq, Eq)]
enum Closed {
    Unmounted,
    /// Still held by something: read-only, which closes an ext4's journal
    /// as an unmount does.
    ReadOnly,
    NotMounted,
    /// Neither: the filesystem stays as `sync` left it.
    Left(nix::errno::Errno),
}

/// What an unmount's result asks for next.
#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq, Eq)]
enum AfterUnmount {
    Done,
    /// Not a mount point, or no such directory: a boot-menu job that never
    /// had it, a development tree.
    Nothing,
    /// Busy: the read-only remount is tried, and the unmount again after a
    /// moment when that is refused too.
    ReadOnly,
    GiveUp(nix::errno::Errno),
}

#[cfg(target_os = "linux")]
fn after_unmount(result: Result<(), nix::errno::Errno>) -> AfterUnmount {
    use nix::errno::Errno;
    match result {
        Ok(()) => AfterUnmount::Done,
        Err(Errno::EINVAL | Errno::ENOENT) => AfterUnmount::Nothing,
        Err(Errno::EBUSY) => AfterUnmount::ReadOnly,
        Err(e) => AfterUnmount::GiveUp(e),
    }
}

/// Unmounts one filesystem, or remounts it read-only while something holds
/// it; a holder that is on its way out (a process just killed) gets
/// `CLOSE_TRIES` looks, a tenth of a second apart.
#[cfg(target_os = "linux")]
fn close_filesystem(path: &Path) -> Closed {
    use nix::mount::{mount, umount2, MntFlags, MsFlags};
    const CLOSE_TRIES: u32 = 10;
    let mut last = nix::errno::Errno::EBUSY;
    for attempt in 0..CLOSE_TRIES {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(100));
        }
        match after_unmount(umount2(path, MntFlags::empty())) {
            AfterUnmount::Done => return Closed::Unmounted,
            AfterUnmount::Nothing => return Closed::NotMounted,
            AfterUnmount::GiveUp(e) => return Closed::Left(e),
            AfterUnmount::ReadOnly => {
                match mount(None::<&str>, path, None::<&str>, MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY, None::<&str>) {
                    Ok(()) => return Closed::ReadOnly,
                    Err(e) => last = e,
                }
            }
        }
    }
    Closed::Left(last)
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
    use super::*;

    /// A supervisor in web mode over a token directory of the test's own.
    fn web_mode(tag: &str) -> Supervisor {
        let mut sup = Supervisor::with_cmdline(Cmdline::parse("embra.web=1"));
        sup.web_token_dir = std::env::temp_dir().join(format!("embrad-web-token-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sup.web_token_dir);
        sup
    }

    fn env_of(sup: &Supervisor, service: &str, key: &str) -> Option<String> {
        let svc = sup.services.iter().find(|s| s.def.name == service)?;
        svc.def.env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    /// The token is made once and kept: a second boot over the same STATE
    /// registers the web console and the display console with the first
    /// boot's token.
    #[test]
    fn a_second_boot_takes_the_token_the_first_one_stored() {
        let mut first = web_mode("kept");
        let dir = first.web_token_dir.clone();
        first.register_services_with(true, true, Path::new("/nonexistent/embrad-test/drm"));
        let token = first.web_token.clone().expect("a token in web mode");
        assert!(embra_boot::web_token::well_formed(&token));
        assert!(first.web_token_stored);
        assert_eq!(embra_boot::web_token::read(&dir).as_deref(), Some(token.as_str()), "on STATE");
        assert_eq!(env_of(&first, "embra-web", WEB_TOKEN_ENV).as_deref(), Some(token.as_str()));

        let mut second = Supervisor::with_cmdline(Cmdline::parse("embra.web=1"));
        second.web_token_dir = dir.clone();
        second.register_services_with(true, true, Path::new("/nonexistent/embrad-test/drm"));
        assert_eq!(second.web_token.as_deref(), Some(token.as_str()), "the same token at the next boot");
        assert_eq!(env_of(&second, "embra-web", WEB_TOKEN_ENV).as_deref(), Some(token.as_str()));
        assert_eq!(env_of(&second, "embra-kiosk", KIOSK_URL_ENV), Some(kiosk_url(&token)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A request for a new token that the machine stopped before answering
    /// is answered by the next boot: a new token, and the request gone.
    #[test]
    fn a_request_left_on_state_is_answered_by_the_next_boot() {
        let mut first = web_mode("asked");
        let dir = first.web_token_dir.clone();
        first.register_services_with(false, false, Path::new("/nonexistent/embrad-test/drm"));
        let old = first.web_token.clone().unwrap();
        embra_boot::web_token::ask_rotation(&dir).unwrap();

        let mut second = Supervisor::with_cmdline(Cmdline::parse("embra.web=1"));
        second.web_token_dir = dir.clone();
        second.register_services_with(false, false, Path::new("/nonexistent/embrad-test/drm"));
        let new = second.web_token.clone().unwrap();
        assert_ne!(new, old);
        assert!(!embra_boot::web_token::rotation_asked(&dir));
        assert_eq!(embra_boot::web_token::read(&dir).as_deref(), Some(new.as_str()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The loop looks for a request only where a web console runs on a
    /// stored token, and not again in a boot where one failed.
    #[test]
    fn a_request_is_seen_where_a_web_console_runs_and_not_again_after_a_failure() {
        let mut sup = web_mode("seen");
        let dir = sup.web_token_dir.clone();
        embra_boot::web_token::ask_rotation(&dir).unwrap();
        assert!(!sup.web_token_rotation_asked(), "no web console registered: nothing to make again");
        sup.register_services_with(false, false, Path::new("/nonexistent/embrad-test/drm"));
        assert!(!sup.web_token_rotation_asked(), "the registration answered the request");
        embra_boot::web_token::ask_rotation(&dir).unwrap();
        assert!(sup.web_token_rotation_asked());
        sup.web_token_rotation_failed = true;
        assert!(!sup.web_token_rotation_asked(), "one failure a boot");
        std::fs::remove_dir_all(&dir).unwrap();

        // The serial TUI: no web console, no token, no request looked for.
        let mut tui = web_mode("tui");
        tui.cmdline = Cmdline::parse("embra.tui=1");
        tui.register_services_with(false, false, Path::new("/nonexistent/embrad-test/drm"));
        assert!(tui.web_token.is_none());
        embra_boot::web_token::ask_rotation(&tui.web_token_dir).unwrap();
        assert!(!tui.web_token_rotation_asked());
        std::fs::remove_dir_all(&tui.web_token_dir).unwrap();
    }

    /// A new token reaches both services that carry it, and the display
    /// console's other variables stay as they were registered.
    #[test]
    fn a_new_token_replaces_the_old_one_in_both_services_environments() {
        let mut sup = web_mode("env");
        let dir = sup.web_token_dir.clone();
        sup.register_services_with(true, true, Path::new("/nonexistent/embrad-test/drm"));
        let old = sup.web_token.clone().unwrap();
        let kiosk_before = sup.services.iter().find(|s| s.def.name == "embra-kiosk").unwrap().def.env.clone();
        let new = "0123456789abcdef0123456789abcdef".to_string();
        sup.take_web_token(new.clone());
        assert_eq!(sup.web_token.as_deref(), Some(new.as_str()));
        assert_eq!(env_of(&sup, "embra-web", WEB_TOKEN_ENV).as_deref(), Some(new.as_str()));
        assert_eq!(env_of(&sup, "embra-kiosk", KIOSK_URL_ENV), Some(kiosk_url(&new)));
        let kiosk_after = sup.services.iter().find(|s| s.def.name == "embra-kiosk").unwrap().def.env.clone();
        assert_eq!(kiosk_after.len(), kiosk_before.len(), "replaced, not added");
        for (before, after) in kiosk_before.iter().zip(&kiosk_after) {
            assert_eq!(before.0, after.0);
            if before.0 != KIOSK_URL_ENV {
                assert_eq!(before.1, after.1, "{}", before.0);
            }
        }
        for svc in &sup.services {
            for (_, value) in &svc.def.env {
                assert!(!value.contains(&old), "{} still carries the old token", svc.def.name);
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// What the banner says of the token: kept across boots and how to
    /// replace it, or this boot's alone when it could not be stored; and
    /// where the URL is shown.
    #[test]
    fn the_banner_says_whether_the_token_stays_and_where_the_url_is_shown() {
        let kept = banner_token_note(true, false);
        assert!(kept.contains("stays the same across boots") && kept.contains("/web-token rotate"), "{kept}");
        assert!(kept.ends_with("the URL is the credential, and it is shown here only."), "{kept}");
        let unstored = banner_token_note(false, true);
        assert!(unstored.starts_with("This boot's access token") && unstored.contains("the next boot makes another"), "{unstored}");
        assert!(unstored.contains("the display console's menu"), "{unstored}");
    }

    /// The lines an operator at a console reads after a request: what
    /// happened, and the console's URLs as the banner prints them.
    #[test]
    fn the_lines_after_a_new_token_name_what_happened_and_the_new_urls() {
        let token = "0123456789abcdef0123456789abcdef";
        let lines = new_token_lines(token, &[], true);
        assert!(lines[0].contains("a new access token") && lines[0].contains("every browser is signed out"), "{}", lines[0]);
        assert!(lines.iter().any(|l| l.contains("https://localhost:3345/embraOS?token=0123456789abcdef0123456789abcdef")));
        assert!(lines.last().unwrap().contains("The display console is on the new URL"), "{lines:?}");
        assert!(!new_token_lines(token, &[], false).iter().any(|l| l.contains("display console")));
    }

    #[test]
    fn the_console_url_carries_the_token() {
        // The banner's first line is the console through QEMU's port forward
        // with the access token; the records decide what follows
        // (`net_lease::console_urls`).
        let token = "0123456789abcdef0123456789abcdef";
        let urls = embra_boot::net_lease::console_urls(token, &[]);
        assert_eq!(urls.iter().map(|u| u.url.as_str()).collect::<Vec<_>>(), ["https://localhost:3345/embraOS?token=0123456789abcdef0123456789abcdef"]);
        // A machine without an address says so under its one line.
        let lines = super::banner_url_lines(&urls, true);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("https://localhost:3345/embraOS?token=0123456789abcdef0123456789abcdef") && lines[0].contains("QEMU hostfwd"));
        assert!(lines[1].contains("no address on a network yet"));
    }

    #[test]
    fn the_banner_has_a_line_for_every_address_and_says_how_it_came() {
        use embra_boot::net_lease::parse;
        let token = "0123456789abcdef0123456789abcdef";
        let leases = [
            parse("interface=eth0\nip=10.20.0.5\nmask=24\nmetric=102\nsource=static\n").unwrap(),
            parse("interface=eth1\nip=192.168.1.42\nmask=24\nrouter=192.168.1.1\nmetric=103\nsource=dhcp\n").unwrap(),
        ];
        let lines = super::banner_url_lines(&embra_boot::net_lease::console_urls(token, &leases), false);
        assert_eq!(lines.len(), 3);
        assert!(lines[1].contains("https://10.20.0.5:3345/embraOS?token=") && lines[1].contains("the address set for eth0, 10.20.0.5/24"));
        assert!(lines[2].contains("https://192.168.1.42:3345/embraOS?token=") && lines[2].contains("the address DHCP leased to this machine, 192.168.1.42/24 on eth1"));
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
        let stop = crate::reconcile::run_until(&mut sup, async move { rx.await.ok(); ("the test", Stop::Restart) }, None).await;
        assert!(started.elapsed() < Duration::from_secs(5), "the backoff held the signal: {:?}", started.elapsed());
        assert_eq!(sup.services[0].restart_count, 1, "the restart had begun");
        assert_eq!(stop, Stop::Restart);
    }

    /// The loop ends on the signal and hands back how it asked for the
    /// machine to be stopped: `main` restarts on one and powers off on the
    /// other, and nothing else tells the two apart.
    #[tokio::test]
    async fn the_loop_hands_back_how_the_signal_asked_for_the_machine_to_be_stopped() {
        for asked in [Stop::Restart, Stop::PowerOff] {
            let mut sup = Supervisor::new();
            let stop = crate::reconcile::run_until(&mut sup, async move { ("the test", asked) }, None).await;
            assert_eq!(stop, asked);
        }
    }

    /// What an unmount's result asks for at the end of a boot: a busy
    /// filesystem is remounted read-only, one that is not mounted is passed
    /// over, and any other refusal ends the try.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_busy_filesystem_is_remounted_read_only_and_one_that_is_not_mounted_is_passed_over() {
        use nix::errno::Errno;
        assert_eq!(after_unmount(Ok(())), AfterUnmount::Done);
        assert_eq!(after_unmount(Err(Errno::EBUSY)), AfterUnmount::ReadOnly);
        assert_eq!(after_unmount(Err(Errno::EINVAL)), AfterUnmount::Nothing, "not a mount point");
        assert_eq!(after_unmount(Err(Errno::ENOENT)), AfterUnmount::Nothing, "no such directory");
        assert_eq!(after_unmount(Err(Errno::EPERM)), AfterUnmount::GiveUp(Errno::EPERM));
        // The workspace is a bind mount of a directory on DATA: it goes
        // first, or DATA is busy with it.
        assert_eq!(DISK_MOUNTS, ["/embra/workspace", "/embra/data", "/embra/state"]);
    }

    /// Only PID 1 ends every other process: `kill(-1, …)` anywhere else is
    /// every process the caller may signal. The test would not be here to
    /// assert it otherwise.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_last_processes_are_ended_by_pid_one_alone() {
        assert_ne!(std::process::id(), 1);
        end_every_other_process();
    }
}

#[cfg(test)]
mod key_source_tests {
    use super::*;

    /// A native model has no key, as Ollama and LM Studio have none: before
    /// it was named here the brain was handed Anthropic's.
    #[test]
    fn a_provider_without_a_key_is_handed_none() {
        for keyless in ["ollama", "lm_studio", "native"] {
            assert_eq!(key_sources(keyless), None, "{keyless}");
        }
        assert_eq!(key_sources("gemini"), Some(("/embra/state/api_key_gemini", "GEMINI_API_KEY")));
        for anthropic in ["anthropic", "", "nobody"] {
            assert_eq!(key_sources(anthropic), Some(("/embra/state/api_key_anthropic", "ANTHROPIC_API_KEY")), "{anthropic:?}");
        }
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

    /// The gateway's gRPC listens to the machine alone: the console,
    /// embra-web and the health check dial loopback, and the conversation
    /// stream behind it, slash commands included, carries no authentication.
    /// Its REST port is not bound here: three read-only GETs.
    #[test]
    fn the_api_gateways_grpc_is_told_to_listen_on_loopback_alone() {
        let mut supervisor = Supervisor::new();
        supervisor.register_services();
        let args = supervisor.service_args("embra-apid").expect("apid is registered");
        let pair = args.windows(2).find(|w| w[0] == "--grpc-bind").expect("--grpc-bind is passed");
        assert_eq!(pair[1], "127.0.0.1");
        let port = args.windows(2).find(|w| w[0] == "--grpc-port").expect("--grpc-port is passed");
        assert_eq!(port[1], "50000");
        let brain = args.windows(2).find(|w| w[0] == "--brain-addr").expect("--brain-addr is passed");
        assert_eq!(brain[1], "http://127.0.0.1:50002", "the gateway dials the brain on loopback");
    }

    /// The brain listens to the machine alone: embra-apid beside it is its
    /// one client.
    #[test]
    fn the_brain_is_told_to_listen_on_loopback_alone() {
        let mut supervisor = Supervisor::new();
        supervisor.register_services();
        let args = supervisor.service_args("embra-brain").expect("the brain is registered");
        let pair = args.windows(2).find(|w| w[0] == "--bind").expect("--bind is passed");
        assert_eq!(pair[1], "127.0.0.1");
        let port = args.windows(2).find(|w| w[0] == "--port").expect("--port is passed");
        assert_eq!(port[1], "50002");
    }
}

#[cfg(test)]
mod display_console_tests {
    use super::*;

    /// Registered on a machine whose kernel lists no display: the tests
    /// read nothing from the host's own `/sys/class/drm`.
    fn registered(cmdline: &str, display_present: bool, kiosk_available: bool) -> Supervisor {
        registered_on(cmdline, display_present, kiosk_available, Path::new("/nonexistent/embrad-test/drm"))
    }

    fn registered_on(cmdline: &str, display_present: bool, kiosk_available: bool, drm: &Path) -> Supervisor {
        let mut sup = Supervisor::with_cmdline(Cmdline::parse(cmdline));
        sup.register_services_with(display_present, kiosk_available, drm);
        sup
    }

    /// A stand-in for the kernel's `/sys/class/drm`: a directory per
    /// connector with its `status` and its `modes`, beside the entries that
    /// are no connectors (`card0`, a render node, `version`). A plain
    /// directory answers no `detect`: the file keeps what was written.
    fn drm_tree(name: &str, connectors: &[(&str, &str, &[&str])]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("embrad-drm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["card0", "renderD128"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("version"), "drm 1.1.0 20060810\n").unwrap();
        for (connector, status, modes) in connectors {
            let dir = root.join(connector);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("status"), format!("{status}\n")).unwrap();
            std::fs::write(dir.join("modes"), modes.iter().map(|m| format!("{m}\n")).collect::<String>()).unwrap();
        }
        root
    }

    fn status_of(root: &Path, connector: &str) -> String {
        std::fs::read_to_string(root.join(connector).join("status")).unwrap()
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
        // A renderer that died is started again on the page the launcher was given.
        assert!(kiosk.def.args.contains(&"--webprocess-failure=restart-home".to_string()), "{:?}", kiosk.def.args);
        assert_eq!(kiosk.def.args.iter().filter(|a| a.starts_with("--webprocess-failure")).count(), 1);
        assert_eq!(env("EMBRA_WEB_TOKEN"), None);
        assert_eq!(env("LIBGL_ALWAYS_SOFTWARE"), None);
        assert_eq!(env("MESA_LOADER_DRIVER_OVERRIDE").as_deref(), Some("kms_swrast"));
        assert_eq!(env("GBM_ALWAYS_SOFTWARE").as_deref(), Some("1"));
        assert_eq!(env("XKB_DEFAULT_LAYOUT").as_deref(), Some("de"));
        assert_eq!(env("XDG_RUNTIME_DIR").as_deref(), Some("/run/user/0"));
    }

    /// 1920x1080 is asked for where every connected display lists it, and
    /// nothing is asked where one does not: cog takes the named mode or none.
    #[test]
    fn the_display_console_asks_for_1920x1080_only_where_every_connected_display_offers_it() {
        let d = |modes: &[&str]| modes.iter().map(|m| m.to_string()).collect::<Vec<_>>();
        let vm = d(&["1280x800", "1920x1080", "1024x768"]);
        let small = d(&["1024x768", "800x600"]);
        assert_eq!(kiosk_video_mode(None, std::slice::from_ref(&vm)), Some((1920, 1080)));
        assert_eq!(kiosk_video_mode(None, std::slice::from_ref(&small)), None, "a display without it keeps its own");
        assert_eq!(kiosk_video_mode(None, &[vm.clone(), small.clone()]), None, "one display without it is enough");
        assert_eq!(kiosk_video_mode(None, &[]), None, "no connected display read: cog decides");
        // the firmware's framebuffer lists the one mode GRUB asked the kernel for
        assert_eq!(kiosk_video_mode(None, &[d(&["1920x1080"])]), Some((1920, 1080)));
        assert_eq!(kiosk_video_mode(None, &[d(&["1920x1080i"])]), None, "an interlaced mode is another name");
    }

    #[test]
    fn embra_kioskmode_names_another_mode_or_the_displays_own() {
        let vm = vec![vec!["1280x800".to_string(), "1920x1080".to_string(), "1600x900".to_string()]];
        assert_eq!(kiosk_video_mode(Some(KioskMode::Size(1600, 900)), &vm), Some((1600, 900)));
        assert_eq!(kiosk_video_mode(Some(KioskMode::Size(2560, 1440)), &vm), None, "not offered: the display's own, never the default");
        assert_eq!(kiosk_video_mode(Some(KioskMode::Native), &vm), None);
    }

    /// The displays are the kernel's list: the connected connectors that
    /// name a mode, in name order; the width is the first mode's.
    #[test]
    fn the_connected_displays_and_their_modes_are_read_from_the_kernels_list() {
        let root = drm_tree("read", &[
            ("card0-HDMI-A-1", "disconnected", &[]),
            ("card0-Virtual-1", "connected", &["2560x1600", "1920x1080"]),
            ("card1-DP-1", "connected", &[]),
        ]);
        assert_eq!(connected_display_modes(&root), vec![vec!["2560x1600".to_string(), "1920x1080".to_string()]]);
        assert_eq!(display_width_px(&root), 2560);
        assert_eq!(connector_dirs(&root).len(), 3, "card0, the render node and version are no connectors");
        let nowhere = Path::new("/nonexistent/embrad-test/drm");
        assert!(connected_display_modes(nowhere).is_empty());
        assert_eq!(display_width_px(nowhere), 0);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// `detect` goes to every connector's `status` and to nothing else, and
    /// no file is created.
    #[test]
    fn a_detect_is_asked_of_every_connector_and_of_nothing_else() {
        let root = drm_tree("detect", &[
            ("card0-Virtual-1", "connected", &["1280x800"]),
            ("card0-HDMI-A-1", "disconnected", &[]),
        ]);
        std::fs::create_dir_all(root.join("card1-DP-2")).unwrap(); // a connector that went away: no status
        std::fs::write(root.join("card0/status"), "no connector\n").unwrap();
        redetect_displays(&root);
        assert_eq!(status_of(&root, "card0-Virtual-1"), "detect");
        assert_eq!(status_of(&root, "card0-HDMI-A-1"), "detect");
        assert_eq!(std::fs::read_to_string(root.join("card0-Virtual-1/modes")).unwrap(), "1280x800\n");
        assert!(!root.join("card1-DP-2/status").exists(), "no file is created");
        assert_eq!(status_of(&root, "card0"), "no connector\n", "the card itself is not written");
        assert!(!root.join("renderD128/status").exists());
        assert_eq!(std::fs::read_to_string(root.join("version")).unwrap(), "drm 1.1.0 20060810\n");
        redetect_displays(Path::new("/nonexistent/embrad-test/drm")); // nothing to ask
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The displays are detected again only when the mode asked for is not
    /// in the kernel's list (or the display's own mode is asked for, whose
    /// width sets the device scale), and never on a kernel line that forces
    /// a connector.
    #[test]
    fn the_displays_are_detected_again_only_when_the_mode_is_missing_and_no_connector_is_forced() {
        let mode = |sup: &Supervisor| {
            let kiosk = sup.services.iter().find(|s| s.def.name == "embra-kiosk").expect("the kiosk");
            kiosk.def.env.iter().find(|(k, _)| k == "COG_PLATFORM_DRM_VIDEO_MODE").map(|(_, v)| v.clone())
        };
        // listed: asked for, and nothing is written
        let listed = drm_tree("listed", &[("card0-Virtual-1", "connected", &["1280x800", "1920x1080"])]);
        let sup = registered_on("embra.web=1", true, true, &listed);
        assert_eq!(mode(&sup).as_deref(), Some("1920x1080"));
        assert_eq!(status_of(&listed, "card0-Virtual-1"), "connected\n");
        // missing: a detect is asked for (a plain directory answers none, so the mode stays missing)
        let cut = drm_tree("cut", &[("card0-Virtual-1", "connected", &["1280x800", "1024x768"])]);
        let sup = registered_on("embra.web=1", true, true, &cut);
        assert_eq!(mode(&sup), None);
        assert_eq!(status_of(&cut, "card0-Virtual-1"), "detect");
        // missing on a line that forces a connector: left as it is
        let forced = drm_tree("forced", &[("card0-Virtual-1", "connected", &["1280x800", "1024x768"])]);
        let sup = registered_on("embra.web=1 video=HDMI-A-1:D", true, true, &forced);
        assert_eq!(mode(&sup), None);
        assert_eq!(status_of(&forced, "card0-Virtual-1"), "connected\n");
        // the display's own mode by the knob: read afresh for the device scale
        let native = drm_tree("native", &[("card0-Virtual-1", "connected", &["1280x800", "1920x1080"])]);
        let sup = registered_on("embra.web=1 embra.kioskmode=native", true, true, &native);
        assert_eq!(mode(&sup), None);
        assert_eq!(status_of(&native, "card0-Virtual-1"), "detect");
        for root in [listed, cut, forced, native] {
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    /// The letters that force a connector (`e`, `D`, `d`) count after the
    /// connector's name and before the first comma; a name that carries
    /// them (`eDP-1`, `HDMI-A-1`) forces nothing.
    #[test]
    fn a_video_option_that_forces_a_connector_is_told_from_one_that_does_not() {
        let forces = |line: &str| video_option_forces_a_connector(Cmdline::parse(line).tokens());
        for line in [
            "video=HDMI-A-1:D",
            "video=eDP-1:d",
            "video=VGA-1:1024x768e",
            "video=1024x768e",
            "quiet video=DP-1:1920x1080@60e embra.web=1",
            "video=HDMI-A-1:e,rotate=180",
        ] {
            assert!(forces(line), "{line}");
        }
        for line in [
            "",
            "quiet embra.web=1",
            "video=efifb:1920x1080",
            "video=efifb:1920x1080 video=Virtual-1:1920x1080@60",
            "video=eDP-1:1920x1080",
            "video=DP-1:1280x720M@60,rotate=180",
        ] {
            assert!(!forces(line), "{line}");
        }
        // a firmware-framebuffer option with one of the letters: counted, the cautious side
        assert!(forces("video=efifb:mode=3"));
    }

    /// The page is drawn by Skia's CPU code: WebKit is told so in the
    /// display console's environment on every boot, and `embra.kioskpaint=gpu`
    /// leaves WebKit its own default for the one boot that carries it.
    #[test]
    fn the_display_console_draws_with_skias_cpu_code_unless_the_line_asks_for_webkits_default() {
        let paint = |line: &str| {
            let sup = registered(line, true, true);
            let kiosk = sup.services.iter().find(|s| s.def.name == "embra-kiosk").expect("the kiosk");
            kiosk.def.env.iter().find(|(k, _)| k == "WEBKIT_SKIA_ENABLE_CPU_RENDERING").map(|(_, v)| v.clone())
        };
        assert_eq!(paint("embra.web=1").as_deref(), Some("1"), "without the knob");
        assert_eq!(paint("embra.web=1 embra.kioskpaint=cpu").as_deref(), Some("1"));
        assert_eq!(paint("embra.web=1 embra.kioskpaint=gpu"), None, "WebKit's default: the variable is not there at all");
        assert_eq!(paint("embra.web=1 embra.kioskpaint=fast").as_deref(), Some("1"), "a word that is neither asks nothing");
        assert_eq!(paint("embra.web=1 embra.kioskpaint=gpu embra.kioskpaint=cpu").as_deref(), Some("1"), "the last occurrence");
        // WebKit reads any value but "0" as the switch: the value given is "1".
        assert_eq!(KIOSK_CPU_PAINT_ENV, "WEBKIT_SKIA_ENABLE_CPU_RENDERING");
        // The other variables are the same either way.
        let names = |p| kiosk_env("t", None, None, p).into_iter().map(|(k, _)| k).filter(|k| k != KIOSK_CPU_PAINT_ENV).collect::<Vec<_>>();
        assert_eq!(names(KioskPaint::Cpu), names(KioskPaint::Gpu));
    }

    /// The failure action embrad names is one the launcher of this tree
    /// knows: a patch of the series adds it, and the image build looks for
    /// it, and for the two other patches that leave words, in what it built.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_failure_action_is_one_the_patched_launcher_knows_and_the_image_build_looks_for_it() {
        use std::process::Command;
        let tree = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
        let action = kiosk_args(1).into_iter().find_map(|a| a.strip_prefix("--webprocess-failure=").map(str::to_string)).expect("a failure action");
        assert_eq!(action, KIOSK_FAILURE_ACTION);
        // The series: one patch adds the action to the launcher's table, and loads the launcher's page.
        let patches = format!("{tree}/buildroot/patches/cog");
        let series: Vec<String> = std::fs::read_dir(&patches).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".patch")).collect();
        let adding: Vec<&String> = series
            .iter()
            .filter(|n| std::fs::read_to_string(format!("{patches}/{n}")).unwrap().contains(&format!("+        {{\"{action}\", WEBPROCESS_FAIL_RESTART_HOME}},")))
            .collect();
        assert_eq!(adding.len(), 1, "one patch of the series adds the action {action}: {series:?}");
        let patch = std::fs::read_to_string(format!("{patches}/{}", adding[0])).unwrap();
        assert!(patch.contains("+        cog_web_view_connect_web_process_terminated_restart_handler_with_uri(web_view, 5, 1000, s_options.home_uri);"), "the action loads the launcher's own page");
        // The image check, on stand-ins for a built target directory.
        let common = format!("{tree}/scripts/lib/image-common.sh");
        let dir = std::env::temp_dir().join(format!("embrad-kiosk-launcher-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let target = |name: &str, cog: Option<&str>, core: &str, drm: &str| {
            let root = dir.join(name);
            let t = root.join("output/target");
            std::fs::create_dir_all(t.join("usr/bin")).unwrap();
            std::fs::create_dir_all(t.join("usr/lib/cog/modules")).unwrap();
            std::fs::create_dir_all(root.join("output/images")).unwrap();
            if let Some(cog) = cog {
                std::fs::write(t.join("usr/bin/cog"), cog).unwrap();
            }
            std::fs::write(t.join("usr/lib/libcogcore.so.9.2.8"), core).unwrap();
            std::fs::write(t.join("usr/lib/cog/modules/libcogplatform-drm.so"), drm).unwrap();
            std::fs::write(root.join("output/images/embraos.img"), b"the image this build wrote").unwrap();
            root
        };
        let check = |root: &std::path::Path| {
            let out = Command::new("bash")
                .args(["-c", r#"set -euo pipefail; BUILDROOT_DIR="$1"; source "$2"; assert_display_console_launcher "$BUILDROOT_DIR/output/target""#, "_"])
                .arg(root)
                .arg(&common)
                .output()
                .unwrap();
            (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned(), root.join("output/images/embraos.img").exists())
        };
        let (cog, core, drm) = (
            format!("\0error-page\0exit\0restart\0{action}\0"),
            "\0crashed\0exceeded the memory limit\0terminated by the API\0",
            "\0a page flip was refused (%s): the mode was set again\0",
        );
        let (ok, said, err, image) = check(&target("patched", Some(&cog), core, drm));
        assert!(ok && image, "{err}");
        assert!(said.contains(&format!("the failure action {action}")), "{said}");
        // An image without the launcher has nothing to check.
        let (ok, said, err, image) = check(&target("none", None, "", ""));
        assert!(ok && image && said.is_empty(), "{said}{err}");
        for (name, cog, core, drm, why) in [
            ("no-action", "\0error-page\0exit\0restart\0", core, drm, "does not know the failure action embrad starts it with (restart-home"),
            ("no-reason", cog.as_str(), "\0crashed\0", drm, "does not say why a renderer stopped"),
            ("no-mode", cog.as_str(), core, "\0failed to schedule a page flip: %s\0", "does not set the mode again after a refused commit"),
        ] {
            let (ok, _, err, image) = check(&target(name, Some(cog), core, drm));
            assert!(!ok, "{name} passed");
            assert!(err.contains(why), "{name}: {err}");
            assert!(!image, "{name}: the image this build wrote is left");
        }
        // The check is part of the root filesystem's checks.
        let script = std::fs::read_to_string(&common).unwrap();
        assert!(script.contains("    assert_display_console_launcher \"$BUILDROOT_DIR/output/target\"\n"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A service that is started again keeps the log of the run before it,
    /// one run back, and starts its own from nothing.
    #[test]
    fn a_restarted_service_keeps_the_log_of_the_run_before() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("embrad-service-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("embra-kiosk.log");
        let prev = dir.join("embra-kiosk.log.prev");
        // The first start: nothing to keep.
        service_log(&log).unwrap().write_all(b"the first run\n").unwrap();
        assert!(!prev.exists());
        // The second: the first run's log is beside it, the new one is empty.
        let mut second = service_log(&log).unwrap();
        assert_eq!(std::fs::read(&prev).unwrap(), b"the first run\n");
        assert_eq!(std::fs::read(&log).unwrap(), b"");
        second.write_all(b"the second run\n").unwrap();
        // The third: one run back and no further.
        drop(service_log(&log).unwrap());
        assert_eq!(std::fs::read(&prev).unwrap(), b"the second run\n");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        // A directory that is not there fails as before, at the create.
        assert!(service_log(&dir.join("absent/embra-web.log")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// cog is named a mode only when the display console asks for one, and
    /// the scale follows the mode in use.
    #[test]
    fn the_display_console_names_its_video_mode_to_cog_only_when_it_asks_for_one() {
        let env = |mode| kiosk_env("t", None, mode, KioskPaint::Cpu).into_iter().find(|(k, _)| k == "COG_PLATFORM_DRM_VIDEO_MODE").map(|(_, v)| v);
        assert_eq!(env(Some((1920, 1080))).as_deref(), Some("1920x1080"));
        assert_eq!(env(None), None);
        assert_eq!(DEFAULT_KIOSK_MODE, (1920, 1080));
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
