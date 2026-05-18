//! Brain-side glue for embra-guardian-v1. Owns the WardSONDB-backed
//! manifest, the in-OS build environment, boot reconcile, the
//! `/guardian` operator slash-command, and the `guardian_call` /
//! `guardian_list` meta-tool backends. The sandbox/validator/scaffold
//! themselves live in the decoupled `embra-guardian` crate.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use embra_guardian::build::{self, BuildEnv};
use embra_guardian::store::{ToolDoc, ToolStatus};
use embra_guardian::ValidatedModule;
use embra_tools_core::DispatchError;
use serde_json::Value;
use tracing::{error, info, warn};

use crate::db::WardsonDbClient;

/// Guardian's writable sub-tree of the workspace (DATA, persists reboots).
const GUARDIAN_BASE: &str = "/embra/workspace/.guardian";
/// Prebaked toolchain location (Buildroot package, task #5).
const TOOLCHAIN_BIN: &str = "/opt/rust/bin";
const COLLECTION: &str = "guardian.tools";
/// Brave Search API key, stored host-side on the STATE partition like the
/// other provider credentials (flat 0600 file, same convention as
/// `/embra/state/api_key_anthropic`). NEVER reaches a guest module, the
/// manifest, or the returned envelope — it only ever lives here + in the
/// host-side `BraveSearch` provider.
const BRAVE_KEY_PATH: &str = "/embra/state/api_key_brave";

fn base() -> &'static Path {
    Path::new(GUARDIAN_BASE)
}

fn build_env() -> BuildEnv {
    BuildEnv {
        toolchain_bin: PathBuf::from(TOOLCHAIN_BIN),
        cargo_home: base().join("cargo-home"),
        target_dir: base().join("target"),
    }
}

/// Pinned toolchain version, written into the rootfs by the Buildroot
/// package. Used to detect a toolchain bump (forces re-define/rebuild).
pub fn toolchain_version() -> String {
    let v = std::fs::read_to_string("/opt/rust/RUST_VERSION")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if v.is_empty() {
        "unknown".to_string()
    } else {
        v
    }
}

fn reserved_names() -> Vec<&'static str> {
    crate::tools::registry::all_descriptors()
        .map(|d| d.name)
        .collect()
}

fn artifact_path(name: &str) -> PathBuf {
    base()
        .join("target/wasm32-unknown-unknown/release")
        .join(format!("{name}.wasm"))
}

/// Read the Brave Search API key from STATE. `None` ⇒ not set; the
/// `web_search` capability then degrades to a structured "not configured"
/// envelope rather than failing the call.
fn read_brave_key() -> Option<String> {
    std::fs::read_to_string(BRAVE_KEY_PATH)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ── persistence (one doc per tool, _id == name) ──

async fn upsert(db: &WardsonDbClient, doc: &ToolDoc) -> Result<(), String> {
    let v = doc.to_value();
    // Update-first, write-fallback — same idempotent pattern as
    // tools::registry::write_snapshot (WardSONDB honors the supplied _id).
    match db.update(COLLECTION, &doc.name, &v).await {
        Ok(()) => Ok(()),
        Err(_) => db.write(COLLECTION, &v).await.map(|_| ()).map_err(|e| e.to_string()),
    }
}

async fn load_doc(db: &WardsonDbClient, name: &str) -> Option<ToolDoc> {
    db.read(COLLECTION, name)
        .await
        .ok()
        .and_then(|v| ToolDoc::from_value(&v).ok())
}

async fn all_docs(db: &WardsonDbClient) -> Vec<ToolDoc> {
    db.query(COLLECTION, &serde_json::json!({}))
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|v| ToolDoc::from_value(v).ok())
        .collect()
}

// ── boot reconcile ──

/// Initialize the runtime overlay and load previously-built artifacts for
/// `Ready` tools whose toolchain still matches. Missing/stale/foreign-
/// toolchain tools are logged and left out (re-define rebuilds them) —
/// boot is never blocked on a compile.
pub async fn reconcile_on_boot(db: &WardsonDbClient) -> anyhow::Result<()> {
    let tv = toolchain_version();
    let rt = embra_guardian::overlay::init(tv.clone())
        .map_err(|e| anyhow::anyhow!("guardian overlay init: {e}"))?;
    for sub in ["", "cargo-home", "target", "tools"] {
        let _ = std::fs::create_dir_all(base().join(sub));
    }

    let docs = all_docs(db).await;
    let total = docs.len();
    let mut loaded = 0usize;
    for doc in docs {
        if doc.status != ToolStatus::Ready {
            continue;
        }
        if doc.toolchain_version != tv {
            warn!(
                "guardian: '{}' built with toolchain {} (now {}) — re-define to rebuild",
                doc.name, doc.toolchain_version, tv
            );
            continue;
        }
        match std::fs::read(artifact_path(&doc.name)) {
            Ok(wasm) => match rt.compile_insert(
                &doc.name,
                &doc.description,
                doc.input_schema.clone(),
                doc.caps.clone(),
                &wasm,
            ) {
                Ok(()) => loaded += 1,
                Err(e) => warn!("guardian: compiling '{}' failed: {e}", doc.name),
            },
            Err(_) => warn!(
                "guardian: artifact for '{}' missing — re-define to rebuild",
                doc.name
            ),
        }
    }
    info!("guardian: reconcile loaded {loaded}/{total} ready tool(s)");
    Ok(())
}

// ── /guardian operator slash-command ──

/// Handle `/guardian <subcommand> …`. `define`'s payload is everything
/// after the first whitespace (the console sends `define\n<module>`).
/// Returns a message for the operator (the gRPC arm sends it as a
/// SystemMessage); never feeds a synthetic model turn.
pub async fn handle_guardian_slash(args: &str, db: &Arc<WardsonDbClient>) -> String {
    let (sub, rest) = match args.split_once(char::is_whitespace) {
        Some((a, b)) => (a.trim(), b.trim_start()),
        None => (args.trim(), ""),
    };
    match sub {
        "define" => define(db, rest).await,
        "list" => list_human(db).await,
        "status" => match load_doc(db, rest.trim()).await {
            Some(d) => format!(
                "guardian '{}': {:?} | caps={:?} | toolchain={} | updated={}\n--- build log tail ---\n{}",
                d.name, d.status, d.caps, d.toolchain_version, d.updated_at, d.build_log_tail
            ),
            None => format!("guardian: no such tool '{}'", rest.trim()),
        },
        "show" => match load_doc(db, rest.trim()).await {
            Some(d) => format!("// guardian-tool: {}\n{}", d.name, d.source),
            None => format!("guardian: no such tool '{}'", rest.trim()),
        },
        "delete" => delete(db, rest.trim()).await,
        "key" => key_cmd(rest),
        "" => "Usage: /guardian-define (paste a module) | /guardian list | \
                /guardian status <name> | /guardian show <name> | \
                /guardian delete <name> | /guardian key brave <token>"
            .to_string(),
        other => format!(
            "guardian: unknown subcommand '{other}'. Use list|status|show|delete|key, \
             or /guardian-define to paste a module."
        ),
    }
}

/// `/guardian key <provider> [<token>]`. Sets (or, with no token, reports
/// the presence of) a search-provider credential. The token is written to
/// STATE 0600 like the other provider keys and is **never echoed back** —
/// status replies only ever say SET / NOT set. v1 provider: `brave`.
fn key_cmd(rest: &str) -> String {
    let (provider, token) = match rest.split_once(char::is_whitespace) {
        Some((p, t)) => (p.trim(), t.trim()),
        None => (rest.trim(), ""),
    };
    match provider {
        "brave" => {
            if token.is_empty() {
                return if read_brave_key().is_some() {
                    "guardian: Brave Search API key is SET — web_search-capable \
                     tools can search. Re-run `/guardian key brave <token>` to \
                     replace it."
                        .to_string()
                } else {
                    "guardian: Brave Search API key is NOT set. Set it with \
                     `/guardian key brave <token>` to enable web_search-capable \
                     tools (until then they return a 'not configured' result)."
                        .to_string()
                };
            }
            match crate::config::write_credential_state(BRAVE_KEY_PATH, token) {
                Ok(()) => "guardian: Brave Search API key saved (STATE, 0600). \
                           web_search-capable tools can now search."
                    .to_string(),
                Err(e) => format!("guardian: could not save Brave key — {e}"),
            }
        }
        "" => "Usage: /guardian key brave <token>  (sets the Brave Search API \
               key; omit the token to check status). Brave is the only v1 \
               search provider."
            .to_string(),
        other => {
            format!("guardian: unknown key provider '{other}'. v1 supports: brave.")
        }
    }
}

async fn define(db: &Arc<WardsonDbClient>, source: &str) -> String {
    if source.trim().is_empty() {
        return "guardian: empty module. Use /guardian-define and paste a Rust \
                module (marker + GUARDIAN_* + fn run)."
            .to_string();
    }
    let names = reserved_names();
    let module = match embra_guardian::validate(source, &names) {
        Ok(m) => m,
        Err(e) => return format!("guardian: validation failed — {e}"),
    };
    let tv = toolchain_version();
    let now = chrono::Utc::now().to_rfc3339();
    let doc = ToolDoc::building(
        &module.name,
        &module.description,
        module.input_schema.clone(),
        &module.source,
        module.caps.clone(),
        &tv,
        &now,
    );
    if let Err(e) = upsert(db, &doc).await {
        return format!("guardian: could not persist '{}' — {e}", module.name);
    }
    let name = module.name.clone();
    let caps = module.caps.clone();
    let db2 = db.clone();
    tokio::spawn(async move { build_and_register(db2, module).await });
    format!(
        "guardian: '{name}' validated{}. Building in background — poll with \
         `/guardian status {name}` or the guardian_call status action.",
        if caps.is_empty() {
            String::new()
        } else {
            format!(" (capabilities: {caps:?})")
        }
    )
}

async fn delete(db: &Arc<WardsonDbClient>, name: &str) -> String {
    if name.is_empty() {
        return "Usage: /guardian delete <name>".to_string();
    }
    if let Some(rt) = embra_guardian::runtime() {
        rt.remove(name);
    }
    let _ = db.delete(COLLECTION, name).await;
    let _ = std::fs::remove_dir_all(base().join("tools").join(name));
    let _ = std::fs::remove_file(artifact_path(name));
    format!("guardian: '{name}' deleted (manifest, overlay, project, artifact).")
}

async fn list_human(db: &WardsonDbClient) -> String {
    let docs = all_docs(db).await;
    if docs.is_empty() {
        return "guardian: no dynamic tools defined.".to_string();
    }
    let mut out = format!("=== Guardian dynamic tools ({}) ===\n", docs.len());
    for d in docs {
        out.push_str(&format!(
            "  {} [{:?}] caps={:?} — {}\n",
            d.name, d.status, d.caps, d.description
        ));
    }
    out
}

// ── background build ──

async fn build_and_register(db: Arc<WardsonDbClient>, module: ValidatedModule) {
    let tv = toolchain_version();
    let env = build_env();
    let paths = match embra_guardian::scaffold(base(), &module) {
        Ok(p) => p,
        Err(e) => return mark_failed(&db, &module, &tv, &format!("scaffold: {e}")).await,
    };
    match build::build(&paths, &env, build::DEFAULT_BUILD_TIMEOUT).await {
        Ok(art) => match embra_guardian::runtime() {
            Some(rt) => match rt.compile_insert(
                &module.name,
                &module.description,
                module.input_schema.clone(),
                module.caps.clone(),
                &art.wasm,
            ) {
                Ok(()) => {
                    mark(&db, &module, &tv, ToolStatus::Ready, &art.log_tail).await;
                    info!("guardian: '{}' ready", module.name);
                }
                Err(e) => {
                    mark_failed(&db, &module, &tv, &format!("wasm load: {e}")).await
                }
            },
            None => mark_failed(&db, &module, &tv, "overlay not initialized").await,
        },
        Err(e) => mark_failed(&db, &module, &tv, &format!("{e}")).await,
    }
}

async fn mark(
    db: &WardsonDbClient,
    m: &ValidatedModule,
    tv: &str,
    status: ToolStatus,
    log_tail: &str,
) {
    let now = chrono::Utc::now().to_rfc3339();
    let mut d = ToolDoc::building(
        &m.name,
        &m.description,
        m.input_schema.clone(),
        &m.source,
        m.caps.clone(),
        tv,
        &now,
    );
    d.status = status;
    d.build_log_tail = log_tail.chars().take(8 * 1024).collect();
    d.updated_at = now;
    if let Err(e) = upsert(db, &d).await {
        error!("guardian: failed to persist status for '{}': {e}", m.name);
    }
}

async fn mark_failed(db: &WardsonDbClient, m: &ValidatedModule, tv: &str, why: &str) {
    warn!("guardian: build failed for '{}': {}", m.name, why);
    mark(db, m, tv, ToolStatus::Failed, why).await;
}

// ── meta-tool backends (called from tools/guardian.rs) ──

/// `guardian_list` — machine-readable inventory for the model.
pub async fn list_for_model(db: &WardsonDbClient) -> Result<String, String> {
    let docs = all_docs(db).await;
    let arr: Vec<Value> = docs
        .iter()
        .map(|d| {
            serde_json::json!({
                "name": d.name,
                "description": d.description,
                "capabilities": d.caps,
                "status": format!("{:?}", d.status).to_lowercase(),
                "input_schema": d.input_schema,
            })
        })
        .collect();
    serde_json::to_string(&serde_json::json!({ "tools": arr })).map_err(|e| e.to_string())
}

/// `guardian_call` backend. `action` = `invoke` | `status`.
pub async fn guardian_call(
    db: &WardsonDbClient,
    action: &str,
    tool: &str,
    input: Value,
) -> Result<String, DispatchError> {
    match action {
        "status" => match load_doc(db, tool).await {
            Some(d) => Ok(serde_json::json!({
                "name": d.name,
                "status": format!("{:?}", d.status).to_lowercase(),
                "capabilities": d.caps,
                "toolchain_version": d.toolchain_version,
                "updated_at": d.updated_at,
                "build_log_tail": d.build_log_tail,
            })
            .to_string()),
            None => Err(DispatchError::Handler(format!(
                "guardian: no such tool '{tool}'"
            ))),
        },
        "invoke" => {
            let rt = embra_guardian::runtime().ok_or_else(|| {
                DispatchError::Handler("guardian: runtime not initialized".into())
            })?;
            let compiled = match rt.get(tool) {
                Some(t) => t,
                None => {
                    let status = load_doc(db, tool)
                        .await
                        .map(|d| format!("{:?}", d.status).to_lowercase())
                        .unwrap_or_else(|| "not found".to_string());
                    return Err(DispatchError::Handler(format!(
                        "guardian: tool '{tool}' is not callable (status: {status}). \
                         Use guardian_call action=status for details."
                    )));
                }
            };
            // Build the per-call grant from the tool's declared caps. The
            // validator already KNOWN_CAPS-checked these; we only wire the
            // host-side primitive for each one declared. A declared cap
            // whose backend is unconfigured (no Brave key) degrades to a
            // structured "not configured" envelope inside the guard — it
            // does NOT fail the call.
            let mut caps = embra_guardian::Capabilities::none();
            if compiled
                .caps
                .iter()
                .any(|c| c == embra_guardian::abi::CAP_HTTP_GET)
            {
                match embra_guardian::caps::ReqwestTransport::new() {
                    Ok(tr) => {
                        caps.http = Some(Arc::new(tr));
                        caps.http_policy = embra_guardian::EgressPolicy::default();
                    }
                    Err(e) => {
                        return Err(DispatchError::Handler(format!(
                            "guardian: http capability init failed: {e}"
                        )));
                    }
                }
            }
            if compiled
                .caps
                .iter()
                .any(|c| c == embra_guardian::abi::CAP_WEB_SEARCH)
                && let Some(key) = read_brave_key()
            {
                match embra_guardian::caps::BraveSearch::new(&key) {
                    Ok(bs) => caps.search = Some(Arc::new(bs)),
                    Err(e) => {
                        return Err(DispatchError::Handler(format!(
                            "guardian: web_search capability init failed: {e}"
                        )));
                    }
                }
            }
            let input_str = serde_json::to_string(&input).unwrap_or_else(|_| "{}".into());
            let module = compiled.module.clone();
            let res = tokio::task::spawn_blocking(move || {
                rt.host().call(
                    &module,
                    &input_str,
                    caps,
                    embra_guardian::host::DEFAULT_DEADLINE,
                    embra_guardian::host::DEFAULT_MEMORY_CAP,
                )
            })
            .await
            .map_err(|e| DispatchError::Handler(format!("guardian: task join: {e}")))?;
            res.map_err(|ge| DispatchError::Handler(ge.into_handler_message(tool)))
        }
        other => Err(DispatchError::Handler(format!(
            "guardian: action must be \"invoke\" or \"status\", got \"{other}\""
        ))),
    }
}
