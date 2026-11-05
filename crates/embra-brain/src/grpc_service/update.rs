//! The operator's system commands: `/reboot` now; `/update` and `/backup`
//! land with their waves. None of them is a tool: an OS action is the
//! operator's decision, typed as a command, and the tool manifest the model
//! sees does not move.

use embra_common::proto::brain::{ConversationResponse, SystemMessage, SystemMessageType, conversation_response};
use tokio::sync::mpsc;
use tonic::Status;

/// The process PID 1 is, as the kernel names it.
pub(super) const PID_ONE_COMM: &str = "/proc/1/comm";

/// What `/reboot` does with its argument and PID 1's name.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RebootDecision {
    /// Not embraOS: PID 1 is not embrad.
    Refused(String),
    /// `/reboot` alone: say what a reboot does and ask for `/reboot now`.
    Explain(String),
    /// `/reboot now`: SIGTERM to PID 1, which stops the services in order
    /// and reboots through the boot menu.
    Reboot,
}

pub(super) fn pid_one_is_embrad(comm: Option<&str>) -> bool {
    comm.map(str::trim) == Some("embrad")
}

/// Pure: the argument, the text of `/proc/1/comm`, and what a session
/// scheduled for the next boot (named, since the reboot runs it).
pub(super) fn reboot_decision(args: &str, pid_one_comm: Option<&str>, scheduled: Option<&str>) -> RebootDecision {
    if !pid_one_is_embrad(pid_one_comm) {
        return RebootDecision::Refused(
            "Not running on embraOS: PID 1 is not embrad, so there is nothing to reboot from here.".to_string(),
        );
    }
    let job = scheduled.map(|j| format!(" The next boot first runs {j}.")).unwrap_or_default();
    match args.trim() {
        "now" => RebootDecision::Reboot,
        "" => RebootDecision::Explain(format!(
            "Reboot embraOS? Every session is kept; a turn in flight is stopped, and the console disconnects for about a minute while the machine comes back through the boot menu.{job} Type /reboot now to proceed."
        )),
        other => RebootDecision::Explain(format!("Unknown argument '{other}'. /reboot explains, /reboot now reboots.")),
    }
}

pub(super) async fn handle_reboot_command(args: &str, tx: &mpsc::Sender<Result<ConversationResponse, Status>>) {
    let send = |content: String, kind: SystemMessageType| {
        let tx = tx.clone();
        async move {
            let _ = tx
                .send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(SystemMessage {
                        content,
                        msg_type: kind as i32,
                    })),
                }))
                .await;
        }
    };
    let comm = std::fs::read_to_string(PID_ONE_COMM).ok();
    let scheduled = scheduled_job().map(|j| j.describe());
    match reboot_decision(args, comm.as_deref(), scheduled.as_deref()) {
        RebootDecision::Refused(msg) => send(msg, SystemMessageType::Error).await,
        RebootDecision::Explain(msg) => send(msg, SystemMessageType::Info).await,
        RebootDecision::Reboot => {
            send(
                "Rebooting embraOS now. The console disconnects; reconnect in about a minute.".to_string(),
                SystemMessageType::Info,
            )
            .await;
            // The frame leaves before the stack is stopped.
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            // SAFETY: kill(2) with a pid and a signal; no pointer. SIGTERM to
            // embrad (PID 1) ends its reconcile loop, which stops the
            // services in reverse order and reboots.
            let rc = unsafe { libc::kill(1, libc::SIGTERM) };
            if rc != 0 {
                send(
                    format!("The reboot signal could not be sent: {}", std::io::Error::last_os_error()),
                    SystemMessageType::Error,
                )
                .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reboot_is_refused_when_pid_one_is_not_embrad() {
        assert!(matches!(reboot_decision("now", Some("systemd\n"), None), RebootDecision::Refused(_)));
        assert!(matches!(reboot_decision("now", None, None), RebootDecision::Refused(_)));
        assert!(matches!(reboot_decision("", Some("init"), None), RebootDecision::Refused(_)));
    }

    #[test]
    fn reboot_explains_first_and_acts_on_now() {
        assert!(matches!(reboot_decision("", Some("embrad\n"), None), RebootDecision::Explain(m) if m.contains("/reboot now")));
        assert!(matches!(reboot_decision("", Some("embrad\n"), Some("a snapshot \"nightly\" of STATE and DATA")), RebootDecision::Explain(m) if m.contains("first runs a snapshot \"nightly\"")));
        assert_eq!(reboot_decision("now", Some("embrad\n"), None), RebootDecision::Reboot);
        assert_eq!(reboot_decision("  now ", Some("embrad"), None), RebootDecision::Reboot);
        assert!(matches!(reboot_decision("please", Some("embrad"), None), RebootDecision::Explain(m) if m.contains("please")));
    }
}

// ── Backups: /backup ────────────────────────────────────────────────────

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// The OS's updater; the brain runs it as a child for the operator's commands.
pub(super) const UPDATER: &str = "/usr/bin/embra-update";
/// What a session schedules for the next boot (embrad reads and removes it
/// before the services start): a snapshot or a restore. On STATE, where the
/// tools cannot write.
pub(super) const NEXT_BOOT_MARKER: &str = "/embra/state/next-boot";
/// The web console's download route for a backup (`embra-web/src/backups.rs`).
pub(super) const BACKUP_ROUTE: &str = "/api/backups/";
const UPDATER_TIMEOUT: Duration = Duration::from_secs(60);

/// A restore a session asked for, waiting for its exact confirmation; any
/// other message, or any slash command, cancels it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingBackupRestore {
    pub name: String,
}

/// The confirmation a pending restore waits for.
pub(super) fn restore_confirmation(name: &str) -> String {
    format!("restore {name}")
}

/// Whether the operator's message is the confirmation, exactly.
pub(super) fn confirms_restore(pending: &PendingBackupRestore, message: &str) -> bool {
    message.trim() == restore_confirmation(&pending.name)
}

/// A label or a backup name: letters, digits, `_`, `.` and `-`, starting
/// with a letter or digit; the updater's own grammar (`cli.rs`), so a word
/// the brain accepts is one the updater accepts.
pub(super) fn valid_word(s: &str, max: usize) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    s.len() <= max && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// What a session schedules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Scheduled {
    Snapshot { label: String },
    Restore { name: String },
}

impl Scheduled {
    /// The marker's text, `key=value` lines (embrad's `modes::parse_marker`).
    pub(super) fn marker_text(&self) -> String {
        match self {
            Scheduled::Snapshot { label } => format!("mode=snapshot\nlabel={label}\n"),
            Scheduled::Restore { name } => format!("mode=restore\nname={name}\n"),
        }
    }

    /// One phrase for the operator.
    pub(super) fn describe(&self) -> String {
        match self {
            Scheduled::Snapshot { label } => format!("a snapshot \"{label}\" of STATE and DATA"),
            Scheduled::Restore { name } => format!("a restore of \"{name}\""),
        }
    }

    /// The job a marker's text names, if any (the mirror of `marker_text`).
    pub(super) fn parse(text: &str) -> Option<Scheduled> {
        let field = |key: &str| text.lines().find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('='))).map(|v| v.trim().to_string());
        match field("mode").as_deref() {
            Some("snapshot") => Some(Scheduled::Snapshot { label: field("label").filter(|l| !l.is_empty()).unwrap_or_else(|| "scheduled".into()) }),
            Some("restore") => field("name").filter(|n| !n.is_empty()).map(|name| Scheduled::Restore { name }),
            _ => None,
        }
    }
}

/// The job scheduled for the next boot, read from the marker.
pub(super) fn scheduled_job() -> Option<Scheduled> {
    std::fs::read_to_string(NEXT_BOOT_MARKER).ok().and_then(|t| Scheduled::parse(&t))
}

/// Write the marker whole: a temporary file beside it, then a rename.
pub(super) async fn write_marker(job: &Scheduled) -> Result<(), String> {
    let text = job.marker_text();
    let path = std::path::Path::new(NEXT_BOOT_MARKER);
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, text).await.map_err(|e| format!("could not write {}: {e}", tmp.display()))?;
    tokio::fs::rename(&tmp, path).await.map_err(|e| format!("could not place {}: {e}", path.display()))
}

/// What a run of the updater came to.
pub(super) struct UpdaterRun {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl UpdaterRun {
    /// The last non-empty stderr line: the updater's reason.
    pub fn reason(&self) -> String {
        self.stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no reason given").trim().to_string()
    }
}

pub(super) async fn run_updater(args: &[&str]) -> Result<UpdaterRun, String> {
    let mut cmd = crate::tools::subprocess(UPDATER);
    cmd.args(args).stdin(std::process::Stdio::null());
    let out = tokio::time::timeout(UPDATER_TIMEOUT, cmd.output())
        .await
        .map_err(|_| format!("{UPDATER} did not answer within {} s", UPDATER_TIMEOUT.as_secs()))?
        .map_err(|e| format!("{UPDATER} could not be run: {e}"))?;
    Ok(UpdaterRun {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    })
}

/// A `list` line's name and whether the entry is complete (`ok`).
pub(super) fn listed_entry(line: &str) -> Option<(&str, bool)> {
    let mut parts = line.split("  ");
    let name = parts.next()?.trim();
    if name.is_empty() || name.starts_with("no backups") {
        return None;
    }
    let state = parts.next().map(str::trim);
    Some((name, state == Some("ok")))
}

const BACKUP_USAGE: &str = "Usage: /backup create [label] | list | restore <name> | delete <name> | status\n  A snapshot or a restore runs at the next boot, before the database starts (a copy of the live database would be inconsistent): /backup create schedules the snapshot and the boot continues into the session; /backup restore asks for its confirmation, then schedules it and the machine reboots after it. The boot menu's Backup and restore entries do the same from the console.";

pub(super) async fn handle_backup_command(
    args: &str,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    pending_restore: &Arc<Mutex<Option<PendingBackupRestore>>>,
) {
    let send = |content: String, kind: SystemMessageType| {
        let tx = tx.clone();
        async move {
            let _ = tx
                .send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(SystemMessage {
                        content,
                        msg_type: kind as i32,
                    })),
                }))
                .await;
        }
    };
    let (sub, rest) = match args.trim().split_once(char::is_whitespace) {
        Some((s, r)) => (s, r.trim()),
        None => (args.trim(), ""),
    };
    match sub {
        "" | "status" => {
            let scheduled = scheduled_job().map(|j| format!("Scheduled for the next boot: {}. /reboot now when ready.", j.describe()));
            let count = match run_updater(&["list", "--names"]).await {
                Ok(run) if run.code == Some(0) => format!("{} complete backup(s) under /embra/data/backups.", run.stdout.lines().filter(|l| !l.trim().is_empty()).count()),
                Ok(run) => format!("The backups could not be listed: {}", run.reason()),
                Err(e) => format!("The backups could not be listed: {e}"),
            };
            let mut text = count;
            if let Some(s) = scheduled {
                text.push('\n');
                text.push_str(&s);
            }
            if sub.is_empty() {
                text.push('\n');
                text.push_str(BACKUP_USAGE);
            }
            send(text, SystemMessageType::Info).await;
        }
        "help" => send(BACKUP_USAGE.to_string(), SystemMessageType::Info).await,
        "create" => {
            let label = if rest.is_empty() { "manual" } else { rest };
            if !valid_word(label, 32) {
                send(format!("Not a label: {label:?} (letters, digits, _ . -; 32 at most)."), SystemMessageType::Error).await;
                return;
            }
            let replaced = scheduled_job();
            match write_marker(&Scheduled::Snapshot { label: label.to_string() }).await {
                Ok(()) => {
                    let mut msg = format!(
                        "Scheduled: the next boot takes a snapshot \"{label}\" of STATE and DATA before the database starts, then continues into the session. /reboot now when ready."
                    );
                    if let Some(prev) = replaced {
                        msg.push_str(&format!(" (This replaces {} that was scheduled.)", prev.describe()));
                    }
                    send(msg, SystemMessageType::Info).await;
                }
                Err(e) => send(format!("The snapshot could not be scheduled: {e}"), SystemMessageType::Error).await,
            }
        }
        "list" => match run_updater(&["list"]).await {
            Ok(run) if run.code == Some(0) => {
                let mut lines = Vec::new();
                for line in run.stdout.lines().filter(|l| !l.trim().is_empty()) {
                    lines.push(line.to_string());
                    if let Some((name, true)) = listed_entry(line) {
                        lines.push(format!("web: {BACKUP_ROUTE}{name}"));
                    }
                }
                send(lines.join("\n"), SystemMessageType::Info).await;
            }
            Ok(run) => send(format!("The backups could not be listed: {}", run.reason()), SystemMessageType::Error).await,
            Err(e) => send(format!("The backups could not be listed: {e}"), SystemMessageType::Error).await,
        },
        "delete" => {
            if !valid_word(rest, 80) {
                send("Usage: /backup delete <name> (a name from /backup list).".to_string(), SystemMessageType::Error).await;
                return;
            }
            match run_updater(&["delete", rest]).await {
                Ok(run) if run.code == Some(0) => send(run.stdout.trim().to_string(), SystemMessageType::Info).await,
                Ok(run) => send(run.reason(), SystemMessageType::Error).await,
                Err(e) => send(format!("The backup could not be deleted: {e}"), SystemMessageType::Error).await,
            }
        }
        "restore" => {
            if !valid_word(rest, 80) {
                send("Usage: /backup restore <name> (a name from /backup list).".to_string(), SystemMessageType::Error).await;
                return;
            }
            let names: Vec<String> = match run_updater(&["list", "--names"]).await {
                Ok(run) if run.code == Some(0) => run.stdout.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect(),
                Ok(run) => {
                    send(format!("The backups could not be listed: {}", run.reason()), SystemMessageType::Error).await;
                    return;
                }
                Err(e) => {
                    send(format!("The backups could not be listed: {e}"), SystemMessageType::Error).await;
                    return;
                }
            };
            if !names.iter().any(|n| n == rest) {
                send(format!("No complete backup named \"{rest}\" (/backup list shows them)."), SystemMessageType::Error).await;
                return;
            }
            *pending_restore.lock().await = Some(PendingBackupRestore { name: rest.to_string() });
            send(
                format!(
                    "Restore \"{rest}\" at the next boot? It replaces STATE and DATA with the snapshot (the knowledge graph, the sessions, the workspace); what is there now is not kept unless you /backup create first. Type: {}",
                    restore_confirmation(rest)
                ),
                SystemMessageType::Info,
            )
            .await;
        }
        _ => send(BACKUP_USAGE.to_string(), SystemMessageType::Error).await,
    }
}

/// The operator's next message against a pending restore: the exact
/// confirmation schedules it, anything else cancels it.
pub(super) async fn handle_restore_answer(
    pending: &PendingBackupRestore,
    message: &str,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
) {
    let send = |content: String, kind: SystemMessageType| {
        let tx = tx.clone();
        async move {
            let _ = tx
                .send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(SystemMessage {
                        content,
                        msg_type: kind as i32,
                    })),
                }))
                .await;
        }
    };
    if !confirms_restore(pending, message) {
        send(format!("Restore of \"{}\" not scheduled.", pending.name), SystemMessageType::Info).await;
        return;
    }
    match write_marker(&Scheduled::Restore { name: pending.name.clone() }).await {
        Ok(()) => {
            send(
                format!(
                    "Scheduled: the next boot restores \"{}\" before the database starts, then reboots. /reboot now when ready.",
                    pending.name
                ),
                SystemMessageType::Info,
            )
            .await
        }
        Err(e) => send(format!("The restore could not be scheduled: {e}"), SystemMessageType::Error).await,
    }
}

#[cfg(test)]
mod backup_tests {
    use super::*;

    #[test]
    fn a_restore_is_scheduled_only_by_its_exact_confirmation() {
        let pending = PendingBackupRestore { name: "2026-11-05_101200_manual".into() };
        assert!(confirms_restore(&pending, "restore 2026-11-05_101200_manual"));
        assert!(confirms_restore(&pending, "  restore 2026-11-05_101200_manual \n"));
        assert!(!confirms_restore(&pending, "restore"));
        assert!(!confirms_restore(&pending, "yes"));
        assert!(!confirms_restore(&pending, "restore 2026-11-05_101200_other"));
        assert!(!confirms_restore(&pending, "Restore 2026-11-05_101200_manual"));
    }

    #[test]
    fn a_scheduled_job_round_trips_through_the_marker_text_embrad_reads() {
        for job in [Scheduled::Snapshot { label: "nightly".into() }, Scheduled::Restore { name: "2026-11-05_101200_manual".into() }] {
            assert_eq!(Scheduled::parse(&job.marker_text()), Some(job.clone()));
        }
        assert_eq!(Scheduled::parse("mode=snapshot\n"), Some(Scheduled::Snapshot { label: "scheduled".into() }));
        assert_eq!(Scheduled::parse("mode=restore\n"), None);
        assert_eq!(Scheduled::parse("nothing"), None);
        assert_eq!(Scheduled::Snapshot { label: "x".into() }.marker_text(), "mode=snapshot\nlabel=x\n");
        assert!(Scheduled::Restore { name: "n".into() }.describe().contains("\"n\""));
    }

    #[test]
    fn a_list_line_yields_its_name_and_whether_it_is_complete() {
        assert_eq!(listed_entry("2026-11-05_101200_manual  ok  298 MB  manual  0.24.0-phase2  manual"), Some(("2026-11-05_101200_manual", true)));
        assert_eq!(listed_entry("2026-10-30_080000_weekly.partial  incomplete  0 MB"), Some(("2026-10-30_080000_weekly.partial", false)));
        assert_eq!(listed_entry("no backups under /embra/data/backups"), None);
        assert_eq!(listed_entry(""), None);
        assert!(valid_word("nightly", 32));
        assert!(!valid_word("../x", 80));
        assert!(!valid_word("", 80));
    }
}
