//! Brain-side glue for embra-guardian-v1. Owns the WardSONDB-backed
//! manifest, the in-OS build environment, boot reconcile, the
//! `/guardian` operator slash-command, and the `guardian_call` /
//! `guardian_list` meta-tool backends. The sandbox/validator/scaffold
//! themselves live in the decoupled `embra-guardian` crate.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use embra_guardian::build::{self, BuildEnv};
use embra_guardian::store::{ReplicantRecord, ToolDoc, ToolStatus};
use embra_guardian::ValidatedModule;
use embra_tools_core::DispatchError;
use serde_json::Value;
use tracing::{error, info, warn};

use crate::config::SystemConfig;
use crate::db::WardsonDbClient;

mod replicant;

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

/// Whether a tool on record can run right now and, when it cannot, why.
/// A record alone does not say: boot reconcile leaves a `Ready` tool out
/// of the overlay when the image's toolchain has moved on, and the record
/// keeps reading `ready`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Availability {
    /// In the overlay.
    Callable,
    /// `Ready` on record, built by a toolchain this image no longer carries.
    ToolchainMismatch { built_with: String, current: String },
    /// `Ready` on record and the toolchain matches, but the build artifact
    /// is missing or did not load.
    ArtifactNotLoaded,
    /// A proposal the operator has not approved.
    Proposed,
    Building,
    Failed,
}

fn availability(doc: &ToolDoc, current_toolchain: &str, loaded: bool) -> Availability {
    if loaded {
        return Availability::Callable;
    }
    match doc.status {
        ToolStatus::Ready if doc.toolchain_version != current_toolchain => {
            Availability::ToolchainMismatch {
                built_with: doc.toolchain_version.clone(),
                current: current_toolchain.to_string(),
            }
        }
        ToolStatus::Ready => Availability::ArtifactNotLoaded,
        ToolStatus::Proposed => Availability::Proposed,
        ToolStatus::Building => Availability::Building,
        ToolStatus::Failed => Availability::Failed,
    }
}

/// The operator command that brings a built-but-unloaded tool back.
fn remedy(name: &str) -> String {
    format!("paste the module again with /guardian-define to rebuild '{name}'")
}

impl Availability {
    /// Why the tool cannot run and what changes that — one sentence, read
    /// by the operator and by the model alike (the model relays the
    /// command; it cannot run one). `None` when the tool is callable.
    fn note(&self, name: &str) -> Option<String> {
        match self {
            Availability::Callable => None,
            Availability::ToolchainMismatch { built_with, current } => Some(format!(
                "not loaded: built with toolchain {built_with}, this OS image carries \
                 {current}. Operator: {}.",
                remedy(name)
            )),
            Availability::ArtifactNotLoaded => Some(format!(
                "not loaded: the build artifact is missing or did not load. Operator: {}.",
                remedy(name)
            )),
            Availability::Proposed => Some(format!(
                "a proposal awaiting the operator: /guardian approve {name} builds it, \
                 /guardian reject {name} discards it."
            )),
            Availability::Building => Some("build in progress.".to_string()),
            Availability::Failed => Some(format!(
                "the last build failed — /guardian status {name} shows the log. Operator: {}.",
                remedy(name)
            )),
        }
    }
}

/// [`availability`] for a record, against this boot's toolchain and overlay.
fn availability_now(doc: &ToolDoc) -> Availability {
    let loaded = embra_guardian::runtime().is_some_and(|rt| rt.get(&doc.name).is_some());
    availability(doc, &toolchain_version(), loaded)
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
                "guardian: '{}' built with toolchain {} (now {}) — not loaded; {}",
                doc.name,
                doc.toolchain_version,
                tv,
                remedy(&doc.name)
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
                "guardian: artifact for '{}' missing — not loaded; {}",
                doc.name,
                remedy(&doc.name)
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
        "approve" => approve(db, rest.trim()).await,
        "reject" => reject(db, rest.trim()).await,
        "list" => list_human(db).await,
        "status" => match load_doc(db, rest.trim()).await {
            Some(d) => status_human(&d, &availability_now(&d)),
            None => format!("guardian: no such tool '{}'", rest.trim()),
        },
        "show" => match load_doc(db, rest.trim()).await {
            Some(d) => {
                let mut header = String::new();
                if let Some(r) = &d.replicant {
                    header.push_str(&format!(
                        "// replicant check: {} (model {}, judged {})\n",
                        r.verdict, r.model, r.judged_at
                    ));
                    if !r.touched_lines.is_empty() {
                        header.push_str(&format!("//   touched: {}\n", r.touched_lines.join("; ")));
                    }
                    if !r.rationale.is_empty() {
                        header.push_str(&format!("//   rationale: {}\n", r.rationale));
                    }
                }
                format!("{header}// guardian-tool: {}\n{}", d.name, d.source)
            }
            None => format!("guardian: no such tool '{}'", rest.trim()),
        },
        "delete" => delete(db, rest.trim()).await,
        "key" => key_cmd(rest),
        "" => "Usage: /guardian-define (paste a module) | /guardian list | \
                /guardian status <name> | /guardian show <name> | \
                /guardian approve <name> | /guardian reject <name> | \
                /guardian delete <name> | /guardian key brave <token>"
            .to_string(),
        other => format!(
            "guardian: unknown subcommand '{other}'. Use list|status|show|approve|reject|\
             delete|key, or /guardian-define to paste a module."
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
        Err(e) => {
            return format!(
                "guardian: validation failed — {e}\n\nStart from this exact skeleton and adjust:\n{}",
                embra_guardian::GUARDIAN_TEMPLATE
            );
        }
    };

    // The replicant check gates operator-pasted tools too: the soul
    // outranks even the operator, so a `refuse` is not waivable and the
    // module is not compiled. Skipped pre-seal (no sealed soul to evaluate
    // against — setup-time defines are unaffected); fail-closed if the
    // check is configured but cannot run. `escalate`/`allow` proceed — the
    // operator already chose to paste, so they ARE the escalation target.
    let now = chrono::Utc::now().to_rfc3339();
    let cfg = match crate::config::load_config(db).await {
        Ok(c) => c,
        Err(e) => {
            return format!(
                "guardian: could not load config for the replicant check ({e}). '{}' not compiled.",
                module.name
            );
        }
    };
    let record = match run_replicant_check(db, &cfg, &module).await {
        Ok(Some((verdict, model))) => {
            if verdict.is_refuse() {
                let touched = if verdict.touched_lines.is_empty() {
                    String::new()
                } else {
                    format!(" (touched: {})", verdict.touched_lines.join("; "))
                };
                return format!(
                    "guardian: '{}' did not pass the replicant check{} — {}. Not compiled — the \
                     soul outranks even an operator paste.",
                    module.name, touched, verdict.rationale
                );
            }
            Some(replicant_record(&verdict, &model, &now))
        }
        Ok(None) => None, // no sealed soul — nothing to evaluate against
        Err(e) => return format!("guardian: {e}. '{}' not compiled — try again.", module.name),
    };

    let tv = toolchain_version();
    let mut doc = ToolDoc::building(
        &module.name,
        &module.description,
        module.input_schema.clone(),
        &module.source,
        module.caps.clone(),
        &tv,
        &now,
    );
    doc.replicant = record.clone();
    if let Err(e) = upsert(db, &doc).await {
        return format!("guardian: could not persist '{}' — {e}", module.name);
    }
    let name = module.name.clone();
    let caps = module.caps.clone();
    let escalated = record.as_ref().map(|r| r.verdict == "escalate").unwrap_or(false);
    let db2 = db.clone();
    tokio::spawn(async move { build_and_register(db2, module).await });
    let warn = if escalated {
        " The replicant check ESCALATED this as soul-borderline — review it with /guardian show."
    } else {
        ""
    };
    format!(
        "guardian: '{name}' validated{}.{warn} Building in background — poll with \
         `/guardian status {name}` or the guardian_call status action.",
        if caps.is_empty() {
            String::new()
        } else {
            format!(" (capabilities: {caps:?})")
        }
    )
}

/// Fallback API key for the replicant-check provider: the active
/// provider's STATE key file. `build_provider_from_config` prefers the
/// recorded config key and only falls back to this. OpenAI-compat presets
/// carry their bearer in env, so this returns empty for them (unused).
fn provider_state_key(cfg: &SystemConfig) -> String {
    let path = match crate::provider::ProviderKind::from_str(&cfg.api_provider) {
        Some(crate::provider::ProviderKind::Anthropic) => "/embra/state/api_key_anthropic",
        Some(crate::provider::ProviderKind::Gemini) => "/embra/state/api_key_gemini",
        _ => return String::new(),
    };
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Run the replicant check against the sealed soul for a validated
/// module — the shared Gate 2 used by BOTH the brain's `propose` and the
/// operator's `define` (the soul gates both authoring paths). Returns:
/// - `Ok(None)` — no soul is sealed yet (nothing to evaluate against; the
///   caller decides whether that means proceed (operator setup) or refuse
///   (brain must not self-author ungated)).
/// - `Err(msg)` — the check is configured but could not run/complete;
///   callers fail closed (no compile/proposal).
/// - `Ok(Some((verdict, model)))` — a completed judgment + the judging
///   model id (for the audit record).
async fn run_replicant_check(
    db: &WardsonDbClient,
    cfg: &SystemConfig,
    module: &ValidatedModule,
) -> Result<Option<(replicant::ReplicantVerdict, String)>, String> {
    let soul = match crate::learning::load_soul(db).await {
        Ok(Some(s)) => s,
        _ => return Ok(None),
    };
    let provider =
        crate::grpc_service::build_provider_from_config(cfg, &provider_state_key(cfg), None)
            .map_err(|e| format!("replicant check could not run — {e}"))?;
    let model = provider.display_name().to_string();
    let verdict = replicant::evaluate_against_soul(provider.as_ref(), &soul, module)
        .await
        .map_err(|e| format!("replicant check could not complete ({e})"))?;
    Ok(Some((verdict, model)))
}

fn replicant_record(v: &replicant::ReplicantVerdict, model: &str, now: &str) -> ReplicantRecord {
    ReplicantRecord {
        verdict: v.verdict.clone(),
        touched_lines: v.touched_lines.clone(),
        rationale: v.rationale.clone(),
        model: model.to_string(),
        judged_at: now.to_string(),
    }
}

/// Brain-side draft — the backend for the `guardian_propose` meta-tool.
/// Statically validates the module (Gate 1), runs the soul-spec
/// **replicant check** (Gate 2, an independent verdict call), and on a
/// passing verdict persists it as a `Proposed` doc for the operator to
/// approve (Gate 3). A `refuse` verdict — or ANY failure of the check —
/// records nothing and returns an error: fail closed. The brain re-drafts
/// on the returned error. It does NOT build; only `/guardian approve`
/// does.
pub async fn propose(
    db: &WardsonDbClient,
    cfg: &SystemConfig,
    source: &str,
) -> Result<String, DispatchError> {
    if source.trim().is_empty() {
        return Err(DispatchError::Handler(
            "guardian: empty module. Provide a Rust module (marker + GUARDIAN_* + fn run)."
                .to_string(),
        ));
    }
    // Gate 1 — static validation (syn + denylist + contract). On failure,
    // hand back the exact skeleton so the redraft converges in one pass
    // instead of looping against the gate.
    let names = reserved_names();
    let module = embra_guardian::validate(source, &names).map_err(|e| {
        DispatchError::Handler(format!(
            "guardian: validation failed — {e}\n\nStart from this exact skeleton and adjust:\n{}",
            embra_guardian::GUARDIAN_TEMPLATE
        ))
    })?;

    // Don't let a proposal clobber a working (or building) operator tool.
    if let Some(existing) = load_doc(db, &module.name).await
        && matches!(existing.status, ToolStatus::Ready | ToolStatus::Building)
    {
        return Err(DispatchError::Handler(format!(
            "guardian: a tool named '{}' already exists (status: {}). Choose a different name, \
             or ask the operator to /guardian delete it first.",
            module.name,
            format!("{:?}", existing.status).to_lowercase()
        )));
    }

    // Gate 2 — the replicant check (independent soul-verdict model call).
    // No sealed soul fails CLOSED here: the brain must not self-author an
    // ungated tool (unlike operator `define`, which proceeds pre-seal).
    let (verdict, model) = match run_replicant_check(db, cfg, &module).await {
        Ok(Some(vm)) => vm,
        Ok(None) => {
            return Err(DispatchError::Handler(
                "guardian: replicant check could not run — no sealed soul to evaluate against."
                    .to_string(),
            ));
        }
        Err(e) => {
            return Err(DispatchError::Handler(format!(
                "guardian: {e}. No proposal recorded — try again."
            )));
        }
    };

    if verdict.is_refuse() {
        let touched = if verdict.touched_lines.is_empty() {
            String::new()
        } else {
            format!(" (touched: {})", verdict.touched_lines.join("; "))
        };
        return Err(DispatchError::Handler(format!(
            "guardian: '{}' did not pass the replicant check{} — {}. No proposal recorded.",
            module.name, touched, verdict.rationale
        )));
    }

    // allow | escalate → persist as Proposed with the verdict attached.
    let tv = toolchain_version();
    let now = chrono::Utc::now().to_rfc3339();
    let mut doc = ToolDoc::building(
        &module.name,
        &module.description,
        module.input_schema.clone(),
        &module.source,
        module.caps.clone(),
        &tv,
        &now,
    );
    doc.status = ToolStatus::Proposed;
    doc.replicant = Some(replicant_record(&verdict, &model, &now));
    if let Err(e) = upsert(db, &doc).await {
        return Err(DispatchError::Handler(format!(
            "guardian: could not persist proposal '{}' — {e}",
            module.name
        )));
    }

    let caps = if module.caps.is_empty() {
        String::new()
    } else {
        format!(" (capabilities: {:?})", module.caps)
    };
    let name = &module.name;
    if verdict.is_escalate() {
        Ok(format!(
            "guardian: '{name}' is proposed{caps}, but the replicant check ESCALATED it for the \
             operator's judgment — {}. Relay to the operator: review with /guardian show {name}, \
             then /guardian approve {name} to build it (or /guardian reject {name}). It will NOT \
             run until approved.",
            verdict.rationale
        ))
    } else {
        Ok(format!(
            "guardian: '{name}' passed the replicant check and is proposed{caps}. It will NOT run \
             until the operator approves it. Relay to the operator: review with /guardian show \
             {name}, then /guardian approve {name} to build and enable it (or /guardian reject \
             {name})."
        ))
    }
}

/// `/guardian approve <name>` — operator gate. Only a `Proposed` doc is
/// approvable; re-validate the stored source (reserved names may have
/// drifted) and run the existing build pipeline (the second half of
/// `define`). The stored replicant verdict survives via `mark`'s
/// load-merge on completion.
async fn approve(db: &Arc<WardsonDbClient>, name: &str) -> String {
    if name.is_empty() {
        return "Usage: /guardian approve <name>".to_string();
    }
    let doc = match load_doc(db, name).await {
        Some(d) => d,
        None => return format!("guardian: no such proposal '{name}'."),
    };
    match doc.status {
        ToolStatus::Ready => return format!("guardian: '{name}' is already built and ready."),
        ToolStatus::Building => {
            return format!(
                "guardian: '{name}' is already building — poll with /guardian status {name}."
            );
        }
        ToolStatus::Failed => {
            return format!(
                "guardian: '{name}' previously failed to build. /guardian delete it, then have \
                 the intelligence re-propose."
            );
        }
        ToolStatus::Proposed => {}
    }
    let module = match embra_guardian::validate(&doc.source, &reserved_names()) {
        Ok(m) => m,
        Err(e) => {
            return format!(
                "guardian: proposal '{name}' no longer validates ({e}). Not built — have the \
                 intelligence re-propose."
            );
        }
    };
    let mut d = doc.clone();
    d.status = ToolStatus::Building;
    d.updated_at = chrono::Utc::now().to_rfc3339();
    if let Err(e) = upsert(db, &d).await {
        return format!("guardian: could not start build for '{name}' — {e}");
    }
    let caps = if module.caps.is_empty() {
        String::new()
    } else {
        format!(" (capabilities: {:?})", module.caps)
    };
    let db2 = db.clone();
    tokio::spawn(async move { build_and_register(db2, module).await });
    format!(
        "guardian: '{name}' approved{caps} — building in background. Poll with /guardian status \
         {name} or the guardian_call status action."
    )
}

/// `/guardian reject <name>` — discard a pending proposal. Refuses on a
/// built tool (those go through `/guardian delete`).
async fn reject(db: &Arc<WardsonDbClient>, name: &str) -> String {
    if name.is_empty() {
        return "Usage: /guardian reject <name>".to_string();
    }
    match load_doc(db, name).await {
        Some(d) if d.status == ToolStatus::Proposed => {
            let _ = db.delete(COLLECTION, name).await;
            format!("guardian: proposal '{name}' rejected and removed.")
        }
        Some(d) => format!(
            "guardian: '{name}' is not a pending proposal (status: {}). Use /guardian delete to \
             remove a built tool.",
            format!("{:?}", d.status).to_lowercase()
        ),
        None => format!("guardian: no such proposal '{name}'."),
    }
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

/// `/guardian status <name>`: the record, whether the tool can run, the
/// replicant verdict when one is stored, and the build log tail.
fn status_human(d: &ToolDoc, avail: &Availability) -> String {
    let mut out = format!(
        "guardian '{}': {:?} | caps={:?} | toolchain={} | updated={}\n",
        d.name, d.status, d.caps, d.toolchain_version, d.updated_at
    );
    match avail.note(&d.name) {
        Some(note) => out.push_str(&format!("callable: no — {note}\n")),
        None => out.push_str("callable: yes\n"),
    }
    if let Some(r) = &d.replicant {
        out.push_str(&format!(
            "replicant check: {} (model {}, judged {})\n",
            r.verdict, r.model, r.judged_at
        ));
    }
    out.push_str(&format!("--- build log tail ---\n{}", d.build_log_tail));
    out
}

/// One `/guardian list` row.
fn list_row(d: &ToolDoc, avail: &Availability) -> String {
    let state = match avail {
        Availability::Callable
        | Availability::Proposed
        | Availability::Building
        | Availability::Failed => format!("{:?}", d.status),
        Availability::ToolchainMismatch { built_with, current } => format!(
            "{:?}, NOT LOADED: toolchain {built_with}, image carries {current}",
            d.status
        ),
        Availability::ArtifactNotLoaded => {
            format!("{:?}, NOT LOADED: artifact missing", d.status)
        }
    };
    format!("  {} [{state}] caps={:?} — {}\n", d.name, d.caps, d.description)
}

async fn list_human(db: &WardsonDbClient) -> String {
    let docs = all_docs(db).await;
    if docs.is_empty() {
        return "guardian: no dynamic tools defined.".to_string();
    }
    let mut out = format!("=== Guardian dynamic tools ({}) ===\n", docs.len());
    let mut unloaded = 0usize;
    for d in docs {
        let avail = availability_now(&d);
        if matches!(
            avail,
            Availability::ToolchainMismatch { .. } | Availability::ArtifactNotLoaded
        ) {
            unloaded += 1;
        }
        out.push_str(&list_row(&d, &avail));
    }
    if unloaded > 0 {
        out.push_str(&format!(
            "{unloaded} built tool(s) are not loaded and cannot run — /guardian status <name> \
             says why.\n"
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
    // Load-merge so fields not derivable from the module — the replicant
    // verdict and the original created_at — survive proposed→building→
    // ready/failed. Falls back to a fresh doc if none is persisted yet.
    let mut d = load_doc(db, &m.name).await.unwrap_or_else(|| {
        ToolDoc::building(
            &m.name,
            &m.description,
            m.input_schema.clone(),
            &m.source,
            m.caps.clone(),
            tv,
            &now,
        )
    });
    d.status = status;
    d.toolchain_version = tv.to_string();
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

/// Add `callable` (and `note`, when it is false) to a model-facing tool
/// object. `status` stays the recorded build status; `callable` is what
/// decides whether an invoke can succeed.
fn stamp_availability(v: &mut Value, d: &ToolDoc, avail: &Availability) {
    let Some(obj) = v.as_object_mut() else { return };
    obj.insert("callable".into(), Value::Bool(*avail == Availability::Callable));
    if let Some(note) = avail.note(&d.name) {
        obj.insert("note".into(), Value::String(note));
    }
}

/// `guardian_list` — machine-readable inventory for the model.
pub async fn list_for_model(db: &WardsonDbClient) -> Result<String, String> {
    let docs = all_docs(db).await;
    let arr: Vec<Value> = docs
        .iter()
        .map(|d| {
            let mut v = serde_json::json!({
                "name": d.name,
                "description": d.description,
                "capabilities": d.caps,
                "status": format!("{:?}", d.status).to_lowercase(),
                "input_schema": d.input_schema,
            });
            stamp_availability(&mut v, d, &availability_now(d));
            v
        })
        .collect();
    serde_json::to_string(&serde_json::json!({ "tools": arr })).map_err(|e| e.to_string())
}

/// The invoke error for a tool on record that is not in the overlay.
fn not_callable_message(d: &ToolDoc, avail: &Availability) -> String {
    let status = format!("{:?}", d.status).to_lowercase();
    let why = avail.note(&d.name).unwrap_or_default();
    format!(
        "guardian: tool '{}' is not callable (status: {status}) — {why}",
        d.name
    )
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
            Some(d) => {
                let mut v = serde_json::json!({
                    "name": d.name,
                    "status": format!("{:?}", d.status).to_lowercase(),
                    "capabilities": d.caps,
                    "toolchain_version": d.toolchain_version,
                    "updated_at": d.updated_at,
                    "build_log_tail": d.build_log_tail,
                });
                stamp_availability(&mut v, &d, &availability_now(&d));
                Ok(v.to_string())
            }
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
                    return Err(DispatchError::Handler(match load_doc(db, tool).await {
                        Some(d) => not_callable_message(&d, &availability_now(&d)),
                        None => format!(
                            "guardian: tool '{tool}' is not callable (status: not found). \
                             Use guardian_list to see what exists."
                        ),
                    }));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(status: ToolStatus, toolchain: &str) -> ToolDoc {
        let mut d = ToolDoc::building(
            "word_count",
            "Counts words.",
            serde_json::json!({"type": "object", "properties": {}}),
            "// guardian-tool: word_count",
            vec![],
            toolchain,
            "2026-09-27T00:00:00Z",
        );
        d.status = status;
        d
    }

    #[test]
    fn availability_covers_every_recorded_status() {
        let mismatch = Availability::ToolchainMismatch {
            built_with: "1.94.1".into(),
            current: "1.98.1".into(),
        };
        // The case the record hides: `ready`, yet left out at boot.
        assert_eq!(availability(&doc(ToolStatus::Ready, "1.94.1"), "1.98.1", false), mismatch);
        assert_eq!(
            availability(&doc(ToolStatus::Ready, "1.98.1"), "1.98.1", false),
            Availability::ArtifactNotLoaded
        );
        assert_eq!(
            availability(&doc(ToolStatus::Proposed, "1.98.1"), "1.98.1", false),
            Availability::Proposed
        );
        assert_eq!(
            availability(&doc(ToolStatus::Building, "1.98.1"), "1.98.1", false),
            Availability::Building
        );
        assert_eq!(
            availability(&doc(ToolStatus::Failed, "1.98.1"), "1.98.1", false),
            Availability::Failed
        );
    }

    #[test]
    fn a_loaded_tool_is_callable_whatever_the_record_says() {
        // A re-define keeps the previous build in the overlay while the
        // record reads `building`.
        for status in [ToolStatus::Ready, ToolStatus::Building, ToolStatus::Failed] {
            assert_eq!(
                availability(&doc(status, "1.98.1"), "1.98.1", true),
                Availability::Callable
            );
        }
        assert_eq!(Availability::Callable.note("word_count"), None);
    }

    #[test]
    fn toolchain_mismatch_is_named_on_every_surface() {
        let d = doc(ToolStatus::Ready, "1.94.1");
        let avail = availability(&d, "1.98.1", false);

        let status = status_human(&d, &avail);
        assert!(status.contains("callable: no"), "{status}");
        assert!(status.contains("1.94.1") && status.contains("1.98.1"), "{status}");

        let row = list_row(&d, &avail);
        assert!(row.contains("NOT LOADED: toolchain 1.94.1, image carries 1.98.1"), "{row}");

        let err = not_callable_message(&d, &avail);
        assert!(err.contains("(status: ready)"), "{err}");
        assert!(err.contains("built with toolchain 1.94.1"), "{err}");

        let mut v = serde_json::json!({"name": "word_count", "status": "ready"});
        stamp_availability(&mut v, &d, &avail);
        assert_eq!(v["callable"], false);
        assert_eq!(v["status"], "ready");
        assert!(v["note"].as_str().unwrap().contains("1.94.1"), "{v}");
    }

    #[test]
    fn a_callable_tool_gets_no_note() {
        let d = doc(ToolStatus::Ready, "1.98.1");
        let avail = availability(&d, "1.98.1", true);

        assert!(status_human(&d, &avail).contains("callable: yes"));
        assert_eq!(list_row(&d, &avail), "  word_count [Ready] caps=[] — Counts words.\n");

        let mut v = serde_json::json!({"name": "word_count"});
        stamp_availability(&mut v, &d, &avail);
        assert_eq!(v, serde_json::json!({"name": "word_count", "callable": true}));
    }

    #[test]
    fn status_shows_the_stored_replicant_verdict() {
        let mut d = doc(ToolStatus::Proposed, "1.98.1");
        d.replicant = Some(ReplicantRecord {
            verdict: "escalate".into(),
            touched_lines: vec![],
            rationale: "borderline".into(),
            model: "opus-5".into(),
            judged_at: "2026-09-27T00:00:01Z".into(),
        });
        let status = status_human(&d, &availability(&d, "1.98.1", false));
        assert!(
            status.contains("replicant check: escalate (model opus-5, judged 2026-09-27T00:00:01Z)"),
            "{status}"
        );
        assert!(status.contains("/guardian approve word_count"), "{status}");
    }
}
