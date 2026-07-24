//! The Learning-Mode import seam: directory scanning and the
//! Selector/Confirm dialogue offered after Phase 1 (UserConfiguration),
//! before the Phase-2 kickoff.
//!
//! The dialogue rides the learning loop's existing channels: SetupPrompt
//! frames go out on the loop's `tx`; replies arrive as plain UserMessages
//! on `incoming` (there is no SetupResponse proto type — the console and
//! the chat-mobile SetupOverlay both send the selected option string as a
//! normal message). Confirm prompts MUST carry explicit options — the
//! console builds its selector only when `options` is non-empty — with
//! the safe option first (index 0 is the pre-selected default).

use std::collections::BTreeMap;
use std::path::PathBuf;

use embra_common::proto::brain::*;
use tokio::sync::{mpsc, watch};
use tokio_stream::StreamExt;
use tonic::{Status, Streaming};
use tracing::{info, warn};

use crate::config::SystemConfig;
use crate::db::WardsonDbClient;
use crate::learning::{LearningPhase, LearningState};

use super::format::{parse_import, IdentityGraph};
use super::ORIGIN_IMPORT;

/// Operator-provisioned import directory (survives on STATE; seeded via
/// `seed-state.sh --import-dir`). Wins filename collisions vs the bake.
pub const STATE_IMPORT_DIR: &str = "/embra/state/imported-intelligence";
/// Read-only examples baked into the rootfs from the repo's
/// `Imported_Intelligence/` folder at image build time.
pub const ROOTFS_IMPORT_DIR: &str = "/usr/share/embra/imported-intelligence";
/// Dev-mode override: when set (non-empty), the ONLY directory scanned.
pub const IMPORT_DIR_ENV: &str = "EMBRA_IMPORT_DIR";

/// Defensive parse cap — candidate files are ~50 KB; anything huge is
/// reported, not parsed.
const IMPORT_FILE_MAX_BYTES: u64 = 4 * 1024 * 1024;

pub struct ImportCandidate {
    pub file_name: String,
    pub graph: IdentityGraph,
}

pub enum ImportOutcome {
    /// No valid candidate files — the loop proceeds conversationally with
    /// no dialogue shown (invalid files, if any, were reported).
    NoCandidates,
    /// The operator chose (or defaulted) to build conversationally.
    Conversational,
    /// A graph was sealed and projected; the loop jumps to Phase 4.
    Imported {
        renamed_to: Option<String>,
        summary_line: String,
    },
    /// Another stream sealed the soul while we waited for input — the
    /// caller announces Operational on its own stream and returns.
    SealedByOtherStream,
    /// The client went away mid-dialogue. Nothing was written (the offer
    /// flag is deliberately loop-local, so the next learning loop
    /// re-offers).
    Disconnected,
}

/// Scan the import directories. Returns valid candidates (deterministic
/// filename order) and per-file issue reports for the invalid ones.
pub fn scan_import_dirs() -> (Vec<ImportCandidate>, Vec<String>) {
    let dirs: Vec<PathBuf> = match std::env::var(IMPORT_DIR_ENV) {
        Ok(dir) if !dir.trim().is_empty() => vec![PathBuf::from(dir.trim())],
        // Rootfs first, STATE second: same-name STATE files overwrite in
        // the map → STATE wins.
        _ => vec![
            PathBuf::from(ROOTFS_IMPORT_DIR),
            PathBuf::from(STATE_IMPORT_DIR),
        ],
    };

    let mut by_name: BTreeMap<String, PathBuf> = BTreeMap::new();
    for dir in &dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.ends_with(".graph.json") || !path.is_file() {
                continue;
            }
            by_name.insert(name.to_string(), path);
        }
    }

    let mut candidates = Vec::new();
    let mut issues = Vec::new();
    for (file_name, path) in by_name {
        match std::fs::metadata(&path) {
            Ok(m) if m.len() > IMPORT_FILE_MAX_BYTES => {
                issues.push(format!(
                    "{file_name}: file too large ({} bytes; cap {})",
                    m.len(),
                    IMPORT_FILE_MAX_BYTES
                ));
                continue;
            }
            Err(e) => {
                issues.push(format!("{file_name}: unreadable: {e}"));
                continue;
            }
            _ => {}
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => {
                issues.push(format!("{file_name}: unreadable: {e}"));
                continue;
            }
        };
        match parse_import(&raw) {
            Ok(graph) => candidates.push(ImportCandidate { file_name, graph }),
            Err(errors) => issues.push(format!(
                "{file_name}: invalid — {}",
                errors.join("; ")
            )),
        }
    }
    (candidates, issues)
}

/// Selector option label for a candidate.
fn candidate_label(c: &ImportCandidate) -> String {
    format!(
        "Import: {} — {} nodes, {} edges ({})",
        c.graph.display_name(),
        c.graph.nodes.len(),
        c.graph.edges.len(),
        c.file_name
    )
}

const BUILD_CONVERSATIONALLY: &str = "Build identity conversationally";
const CONFIRM_NO: &str = "No — choose again";
const CONFIRM_YES: &str = "Yes — seal permanently";

/// Match an operator reply against the selector options. Accepts the
/// exact option label, the bare display name, or the bare file name
/// (all trimmed, case-insensitive).
fn match_candidate<'a>(
    reply: &str,
    candidates: &'a [ImportCandidate],
) -> Option<&'a ImportCandidate> {
    let wanted = reply.trim().to_lowercase();
    candidates.iter().find(|c| {
        candidate_label(c).to_lowercase() == wanted
            || c.graph.display_name().to_lowercase() == wanted
            || c.file_name.to_lowercase() == wanted
    })
}

fn is_conversational_reply(reply: &str) -> bool {
    let r = reply.trim().to_lowercase();
    r.is_empty()
        || r == BUILD_CONVERSATIONALLY.to_lowercase()
        || r.starts_with("build")
}

fn confirm_accepted(reply: &str) -> bool {
    let r = reply.trim().to_lowercase();
    r == CONFIRM_YES.to_lowercase() || r == "yes" || r == "y"
}

/// The operator-facing pre-confirm summary block.
fn summary_message(c: &ImportCandidate) -> String {
    let s = c.graph.summary();
    let mut out = format!(
        "Import candidate: {} ({})\n  {} nodes, {} edges\n  Node types:\n",
        s.name, c.file_name, s.node_count, s.edge_count
    );
    for (t, n) in &s.type_histogram {
        out.push_str(&format!("    {t}: {n}\n"));
    }
    if let Some(self_node) = c.graph.self_node() {
        out.push_str(&format!("  Self: {}\n", self_node.text));
    }
    out
}

enum Wait {
    Line(String),
    Sealed,
    Disconnected,
}

/// Wait for the next operator line, mirroring the learning loop's
/// select-against-the-onboarding-stage pattern (another stream sealing
/// the soul must unblock us). Non-UserMessage frames are ignored.
async fn wait_for_user_line(
    incoming: &mut Streaming<ConversationRequest>,
    stage_rx: &mut watch::Receiver<i32>,
    db: &WardsonDbClient,
) -> anyhow::Result<Wait> {
    loop {
        let next = tokio::select! {
            msg = incoming.next() => msg,
            _ = stage_rx.changed() => {
                if crate::learning::is_soul_sealed(db).await.unwrap_or(false) {
                    return Ok(Wait::Sealed);
                }
                continue;
            }
        };
        match next {
            Some(Ok(req)) => {
                if let Some(conversation_request::RequestType::UserMessage(um)) =
                    req.request_type
                {
                    return Ok(Wait::Line(um.content));
                }
                // Ignore non-UserMessage during the dialogue.
            }
            Some(Err(e)) => {
                warn!("Stream error during import dialogue: {e}");
                return Err(e.into());
            }
            None => return Ok(Wait::Disconnected),
        }
    }
}

async fn send_system(
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    content: String,
    msg_type: SystemMessageType,
) {
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::System(
                SystemMessage {
                    content,
                    msg_type: msg_type as i32,
                },
            )),
        }))
        .await;
}

async fn send_selector(
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    prompt: String,
    options: Vec<String>,
    default_value: String,
) {
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::Setup(SetupPrompt {
                field_type: SetupFieldType::Selector as i32,
                prompt,
                options,
                default_value,
            })),
        }))
        .await;
}

/// The import offer. Called once per learning loop when the phase reaches
/// `IdentityFormation`; every terminal path leaves `state` consistent for
/// the caller (only `Imported` mutates it).
pub async fn offer_import(
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    incoming: &mut Streaming<ConversationRequest>,
    stage_rx: &mut watch::Receiver<i32>,
    db: &WardsonDbClient,
    config: &SystemConfig,
    state: &mut LearningState,
) -> anyhow::Result<ImportOutcome> {
    let (candidates, issues) = scan_import_dirs();
    if !issues.is_empty() {
        send_system(
            tx,
            format!(
                "Some import files were skipped as invalid:\n  {}",
                issues.join("\n  ")
            ),
            SystemMessageType::Warning,
        )
        .await;
    }
    if candidates.is_empty() {
        return Ok(ImportOutcome::NoCandidates);
    }

    let mut options = vec![BUILD_CONVERSATIONALLY.to_string()];
    options.extend(candidates.iter().map(candidate_label));

    loop {
        send_selector(
            tx,
            "A pre-built intelligence can be imported instead of building \
             identity and soul conversationally. Importing seals the chosen \
             graph as this instance's identity."
                .to_string(),
            options.clone(),
            BUILD_CONVERSATIONALLY.to_string(),
        )
        .await;

        let reply = match wait_for_user_line(incoming, stage_rx, db).await? {
            Wait::Line(l) => l,
            Wait::Sealed => return Ok(ImportOutcome::SealedByOtherStream),
            Wait::Disconnected => return Ok(ImportOutcome::Disconnected),
        };

        if is_conversational_reply(&reply) {
            return Ok(ImportOutcome::Conversational);
        }
        let Some(candidate) = match_candidate(&reply, &candidates) else {
            send_system(
                tx,
                "Unrecognized selection — continuing conversationally. \
                 (Restart learning to see the import offer again.)"
                    .to_string(),
                SystemMessageType::Info,
            )
            .await;
            return Ok(ImportOutcome::Conversational);
        };

        send_system(tx, summary_message(candidate), SystemMessageType::Info).await;
        send_selector(
            tx,
            format!(
                "Seal '{}' permanently as this instance's identity and soul? \
                 The soul seal is IRREVERSIBLE.",
                candidate.graph.display_name()
            ),
            vec![CONFIRM_NO.to_string(), CONFIRM_YES.to_string()],
            CONFIRM_NO.to_string(),
        )
        .await;

        let confirm = match wait_for_user_line(incoming, stage_rx, db).await? {
            Wait::Line(l) => l,
            Wait::Sealed => return Ok(ImportOutcome::SealedByOtherStream),
            Wait::Disconnected => return Ok(ImportOutcome::Disconnected),
        };
        if !confirm_accepted(&confirm) {
            send_system(
                tx,
                "Not confirmed — returning to selection.".to_string(),
                SystemMessageType::Info,
            )
            .await;
            continue;
        }

        // --- Confirmed: canonicalize → seal → project → name sync. ---
        let proposed_name = candidate.graph.display_name();
        let canonical_name =
            match crate::config::validate_intelligence_name(&proposed_name) {
                Ok(n) => n,
                Err(e) => {
                    warn!(
                        target: "identity_graph",
                        name = %proposed_name,
                        error = %e,
                        "imported display name failed validation; keeping the current config name"
                    );
                    config.name.clone()
                }
            };
        let canonical = candidate.graph.canonicalize(&canonical_name);

        if let Err(e) = crate::learning::seal_soul(db, &canonical).await {
            // Nothing irreversible happened — sealing itself failed.
            send_system(
                tx,
                format!("Import failed at the seal step: {e}. Nothing was written — choose again."),
                SystemMessageType::Error,
            )
            .await;
            continue;
        }
        info!(
            target: "identity_graph",
            name = %canonical_name,
            nodes = candidate.graph.nodes.len(),
            edges = candidate.graph.edges.len(),
            file = %candidate.file_name,
            "identity graph imported and sealed"
        );

        // Post-seal: projection + memory.user graph transition —
        // best-effort, boot-reconciled.
        super::project::complete_graph_transition(db, &candidate.graph, ORIGIN_IMPORT)
            .await;

        let renamed_to =
            crate::learning::sync_name_to_config(db, config, &canonical_name).await;

        state.soul = Some(canonical);
        state.phase = LearningPhase::InitialToolset;

        let summary_line = format!(
            "Imported '{}' — {} nodes, {} edges sealed from {}.",
            canonical_name,
            candidate.graph.nodes.len(),
            candidate.graph.edges.len(),
            candidate.file_name
        );
        return Ok(ImportOutcome::Imported {
            renamed_to,
            summary_line,
        });
    }
}

#[cfg(test)]
mod import_flow_tests {
    use super::*;
    use crate::identity_graph::format::{GraphEdge, GraphNode};

    fn candidate(name: Option<&str>, file: &str) -> ImportCandidate {
        ImportCandidate {
            file_name: file.to_string(),
            graph: IdentityGraph {
                name: name.map(|n| n.to_string()),
                nodes: vec![GraphNode {
                    id: "meridian".into(),
                    node_type: "self".into(),
                    text: "Meridian, a wayfinding intelligence.".into(),
                }],
                edges: vec![GraphEdge {
                    src: "meridian".into(),
                    dst: "meridian".into(),
                    relation: "reflects".into(),
                }],
            },
        }
    }

    #[test]
    fn candidate_labels_and_matching() {
        let c = candidate(None, "Meridian_IDENTITY-SOUL.graph.json");
        let label = candidate_label(&c);
        assert_eq!(
            label,
            "Import: Meridian — 1 nodes, 1 edges (Meridian_IDENTITY-SOUL.graph.json)"
        );
        let list = [c];
        assert!(match_candidate(&label, &list).is_some());
        assert!(match_candidate(" meridian ", &list).is_some());
        assert!(match_candidate("Meridian_IDENTITY-SOUL.graph.json", &list).is_some());
        assert!(match_candidate("something else", &list).is_none());
    }

    #[test]
    fn conversational_and_confirm_matching() {
        assert!(is_conversational_reply(""));
        assert!(is_conversational_reply("  "));
        assert!(is_conversational_reply(BUILD_CONVERSATIONALLY));
        assert!(is_conversational_reply("build it with me"));
        assert!(!is_conversational_reply("meridian"));

        assert!(confirm_accepted(CONFIRM_YES));
        assert!(confirm_accepted("yes"));
        assert!(confirm_accepted(" Y "));
        assert!(!confirm_accepted(CONFIRM_NO));
        assert!(!confirm_accepted("maybe"));
        assert!(!confirm_accepted(""));
    }

    #[test]
    fn summary_message_lists_histogram_and_self() {
        let c = candidate(Some("Custom"), "x.graph.json");
        let s = summary_message(&c);
        assert!(s.contains("Import candidate: Custom (x.graph.json)"));
        assert!(s.contains("1 nodes, 1 edges"));
        assert!(s.contains("self: 1"));
        assert!(s.contains("Self: Meridian, a wayfinding intelligence."));
    }

    #[test]
    fn confirm_options_lead_with_the_safe_choice() {
        // The console pre-selects index 0 — the safe option must be first.
        assert_eq!(CONFIRM_NO, "No — choose again");
        assert!(CONFIRM_YES.contains("seal permanently"));
    }
}
