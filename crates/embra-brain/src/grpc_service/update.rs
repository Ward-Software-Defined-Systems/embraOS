//! The operator's system commands: `/reboot` now; `/update` and `/backup`
//! land with their waves. None of them is a tool: an OS action is the
//! operator's decision, typed as a command, and the tool manifest the model
//! sees does not move.

use embra_common::proto::brain::{
    ConversationRequest, ConversationResponse, SystemMessage, SystemMessageType, conversation_request, conversation_response,
};
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
    if update_running() {
        return RebootDecision::Refused("An update is being staged; /update status shows it. Reboot once it says staged.".to_string());
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
/// The web console's route for a backup (`embra-web/src/backups.rs`): a GET
/// downloads one, a PUT uploads one.
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

const BACKUP_USAGE: &str = "Usage: /backup create [label] | list | restore <name> | delete <name> | upload | status\n  A snapshot or a restore runs at the next boot, before the database starts (a copy of the live database would be inconsistent): /backup create schedules the snapshot and the boot continues into the session; /backup restore asks for its confirmation, then schedules it and the machine reboots after it. The boot menu's Backup and restore entries do the same from the console. /backup upload says how a backup downloaded earlier, from this machine or another, comes back through the web console.";

/// What `/backup upload` answers. The upload itself is the web console's
/// (`PUT /api/backups/<name>`): the brain holds neither the file nor the
/// console's token, and never prints the console's URL.
pub(super) fn upload_instructions() -> String {
    format!(
        "A backup comes back through the web console, from a browser on another machine: in the sidebar, /backup with the action upload, then the .tar.gz that was downloaded. The machine checks the archive while it arrives; a file that is not a backup of embraOS, a damaged one or one cut short is refused, and nothing of it is kept. Taken in, it is listed by /backup list as uploaded and restored like any other: /backup restore <name> and /reboot now, which are answered during setup too, or the boot menu's Restore entry. Its name is the file's without .tar.gz, or the one typed in the form. Without a browser, the same route takes the file from the command line of another machine: curl -T <file>.tar.gz 'https://<this machine>:3345{BACKUP_ROUTE}<name>?token=<the console's token>', with --cacert <this machine's CA certificate>, or -k to accept the console's certificate unseen: it is from this machine's own CA."
    )
}

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
        "upload" => send(upload_instructions(), SystemMessageType::Info).await,
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

/// A restore that waited for its confirmation and got a command instead: it
/// is said, and no longer waits. The confirmation is a plain message, and a
/// command is not it.
pub(super) async fn cancel_pending_restore(
    pending_restore: &Arc<Mutex<Option<PendingBackupRestore>>>,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
) {
    let cancelled = pending_restore.lock().await.take();
    if let Some(pending) = cancelled {
        let _ = tx
            .send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::System(SystemMessage {
                    content: format!("Restore of '{}' not scheduled (a command arrived instead of the confirmation).", pending.name),
                    msg_type: SystemMessageType::Info as i32,
                })),
            }))
            .await;
    }
}

// ── Commands before a session: the wizard, Learning Mode ────────────────

/// What a command is to the stages that run before any session.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum SetupCommand {
    Backup,
    Reboot,
    /// Any other: answered once the setup is complete.
    Later,
}

pub(super) fn setup_command(command: &str) -> SetupCommand {
    match command.trim() {
        "/backup" => SetupCommand::Backup,
        "/reboot" => SetupCommand::Reboot,
        _ => SetupCommand::Later,
    }
}

/// The one line any other command gets before a session. It says nothing of
/// the command but that it was not run: the word may be one no command has.
pub(super) fn later_notice(command: &str) -> String {
    let shown: String = command.trim().chars().take(40).collect();
    format!("Not run: {shown}. Commands are answered once setup is complete (the configuration, then Learning Mode); until then /backup and /reboot are.")
}

/// What the stages before a session do with a client's frame: the
/// configuration wizard, Learning Mode and the identity import's dialogue.
/// Each reads the stream itself and wants the operator's messages alone, and
/// a command sent then used to be dropped without a word. A machine that is
/// installed again is restored from exactly there (a backup uploaded through
/// the web console, `/backup restore`, `/reboot now`), so `/backup` and
/// `/reboot` are answered as in a session, the confirmation of a restore is
/// taken, and any other command gets one line.
#[derive(Clone)]
pub(crate) struct SetupCommands {
    tx: mpsc::Sender<Result<ConversationResponse, Status>>,
    pending_restore: Arc<Mutex<Option<PendingBackupRestore>>>,
}

impl SetupCommands {
    pub(super) fn new(tx: &mpsc::Sender<Result<ConversationResponse, Status>>, pending_restore: &Arc<Mutex<Option<PendingBackupRestore>>>) -> Self {
        SetupCommands { tx: tx.clone(), pending_restore: Arc::clone(pending_restore) }
    }

    /// The client's stream, which the door answers on: a stage that is
    /// handed the door speaks to the client through the same one.
    pub(super) fn stream(&self) -> &mpsc::Sender<Result<ConversationResponse, Status>> {
        &self.tx
    }

    /// One frame. `Some(text)` is the operator's message for the stage: an
    /// answer to the wizard, a line of the learning conversation. `None`:
    /// the frame was taken here, or it is one the stages never read (an
    /// attach).
    pub(crate) async fn take(&self, req: ConversationRequest) -> Option<String> {
        match req.request_type {
            Some(conversation_request::RequestType::UserMessage(um)) => {
                // A pending `/backup restore`: this message is its answer,
                // the exact confirmation or anything else, and never the
                // stage's (the wizard would store it as a name, Learning
                // Mode would send it to the model).
                if let Some(pending) = self.pending_restore.lock().await.take() {
                    handle_restore_answer(&pending, &um.content, &self.tx).await;
                    return None;
                }
                Some(um.content)
            }
            Some(conversation_request::RequestType::SlashCommand(cmd)) => {
                cancel_pending_restore(&self.pending_restore, &self.tx).await;
                match setup_command(&cmd.command) {
                    SetupCommand::Backup => handle_backup_command(&cmd.args, &self.tx, &self.pending_restore).await,
                    SetupCommand::Reboot => handle_reboot_command(&cmd.args, &self.tx).await,
                    SetupCommand::Later => {
                        let _ = self
                            .tx
                            .send(Ok(ConversationResponse {
                                response_type: Some(conversation_response::ResponseType::System(SystemMessage {
                                    content: later_notice(&cmd.command),
                                    msg_type: SystemMessageType::Info as i32,
                                })),
                            }))
                            .await;
                    }
                }
                None
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod backup_tests {
    use super::*;
    use embra_common::proto::brain::{SessionAttach, SlashCommand, UserMessage};

    fn message(text: &str) -> ConversationRequest {
        ConversationRequest {
            request_type: Some(conversation_request::RequestType::UserMessage(UserMessage {
                content: text.to_string(),
                timestamp: None,
                attachment_ids: Vec::new(),
                file_paths: Vec::new(),
            })),
        }
    }

    fn command(command: &str, args: &str) -> ConversationRequest {
        ConversationRequest {
            request_type: Some(conversation_request::RequestType::SlashCommand(SlashCommand { command: command.to_string(), args: args.to_string() })),
        }
    }

    /// The system lines a stream was sent, in order.
    fn said(rx: &mut mpsc::Receiver<Result<ConversationResponse, Status>>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(Ok(frame)) = rx.try_recv() {
            if let Some(conversation_response::ResponseType::System(m)) = frame.response_type {
                out.push(m.content);
            }
        }
        out
    }

    /// Before a session the operator's messages go to the stage, `/backup`
    /// and `/reboot` are answered, and any other command gets its one line:
    /// none is dropped without a word.
    #[tokio::test]
    async fn before_a_session_backup_and_reboot_are_answered_and_any_other_command_is_told_to_wait() {
        let (tx, mut rx) = mpsc::channel(16);
        let pending = Arc::new(Mutex::new(None));
        let door = SetupCommands::new(&tx, &pending);

        assert_eq!(door.take(message("Embra")).await.as_deref(), Some("Embra"), "an answer is the stage's");
        assert!(said(&mut rx).is_empty(), "and nothing is said about it");

        assert_eq!(door.take(command("/backup", "help")).await, None);
        assert_eq!(said(&mut rx), vec![BACKUP_USAGE.to_string()], "/backup is answered as in a session");
        assert_eq!(door.take(command("/backup", "upload")).await, None);
        assert_eq!(said(&mut rx), vec![upload_instructions()]);

        // On a development host PID 1 is not embrad: the answer is the
        // command's own refusal, which proves the command ran.
        assert_eq!(door.take(command("/reboot", "")).await, None);
        let reboot = said(&mut rx);
        assert_eq!(reboot.len(), 1);
        assert!(reboot[0].contains("PID 1 is not embrad") || reboot[0].contains("Reboot embraOS?"), "{reboot:?}");

        for other in ["/status", "/sessions", "/provider", "/frobnicate"] {
            assert_eq!(door.take(command(other, "now")).await, None);
            let line = said(&mut rx);
            assert_eq!(line, vec![later_notice(other)], "{other}");
            assert!(line[0].starts_with(&format!("Not run: {other}.")) && line[0].contains("/backup and /reboot are"), "{line:?}");
        }

        // A frame the stages never read is still not theirs, and says nothing.
        let attach = ConversationRequest { request_type: Some(conversation_request::RequestType::SessionAttach(SessionAttach::default())) };
        assert_eq!(door.take(attach).await, None);
        assert_eq!(door.take(ConversationRequest { request_type: None }).await, None);
        assert!(said(&mut rx).is_empty());
    }

    /// A restore asked for before a session waits for its confirmation
    /// there: the next message is its answer and never the stage's, and a
    /// command in its place cancels it, as in a session.
    #[tokio::test]
    async fn before_a_session_the_next_message_answers_a_pending_restore_and_a_command_cancels_it() {
        let (tx, mut rx) = mpsc::channel(16);
        let pending = Arc::new(Mutex::new(None));
        let door = SetupCommands::new(&tx, &pending);

        *pending.lock().await = Some(PendingBackupRestore { name: "2026-11-12_101200_manual".into() });
        assert_eq!(door.take(message("Embra")).await, None, "the wizard never gets it as an answer");
        assert_eq!(said(&mut rx), vec!["Restore of \"2026-11-12_101200_manual\" not scheduled.".to_string()]);
        assert!(pending.lock().await.is_none());
        assert_eq!(door.take(message("Embra")).await.as_deref(), Some("Embra"), "the next message is the stage's again");

        *pending.lock().await = Some(PendingBackupRestore { name: "2026-11-12_101200_manual".into() });
        assert_eq!(door.take(command("/status", "")).await, None);
        assert_eq!(
            said(&mut rx),
            vec![
                "Restore of '2026-11-12_101200_manual' not scheduled (a command arrived instead of the confirmation).".to_string(),
                later_notice("/status"),
            ]
        );
        assert!(pending.lock().await.is_none());

        assert_eq!(setup_command("/backup"), SetupCommand::Backup);
        assert_eq!(setup_command(" /reboot "), SetupCommand::Reboot);
        assert_eq!(setup_command("/backups"), SetupCommand::Later);
        assert_eq!(setup_command(""), SetupCommand::Later);
        assert!(later_notice(&"/x".repeat(100)).len() < 220, "a long word is cut");
    }

    /// The three stages before a session read the stream themselves. Each
    /// hands every frame to `SetupCommands::take`, so a stage added later
    /// that reads the stream its own way, and drops commands again, fails
    /// here.
    #[test]
    fn every_stage_before_a_session_takes_its_frames_through_the_one_door() {
        let stages = [
            ("grpc_service/mod.rs (the configuration wizard)", include_str!("mod.rs")),
            ("grpc_service/learning_loop.rs (Learning Mode)", include_str!("learning_loop.rs")),
            ("identity_graph/import_flow.rs (the import dialogue)", include_str!("../identity_graph/import_flow.rs")),
        ];
        for (stage, source) in stages {
            assert!(source.contains(".take(req).await"), "{stage} no longer takes its frames through SetupCommands");
            assert!(!source.contains("Ignore non-UserMessage"), "{stage} drops a frame without a word");
        }
        // The stream is read in these three files and in the session's loop, and nowhere else.
        let readers: Vec<&str> = [
            ("mod.rs", include_str!("mod.rs")),
            ("learning_loop.rs", include_str!("learning_loop.rs")),
            ("turn.rs", include_str!("turn.rs")),
            ("attach.rs", include_str!("attach.rs")),
            ("slash.rs", include_str!("slash.rs")),
            ("provider_setup.rs", include_str!("provider_setup.rs")),
        ]
        .iter()
        .filter(|(_, source)| source.contains("incoming.next()"))
        .map(|(name, _)| *name)
        .collect();
        assert_eq!(readers, ["mod.rs", "learning_loop.rs"], "a new reader of the client's stream: it takes its frames through SetupCommands before a session");
    }

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

    /// The upload is the web console's; the command says how, names the
    /// route the console serves and nothing that only the console knows.
    #[test]
    fn the_upload_command_says_how_and_prints_no_address_or_token_of_its_own() {
        let said = upload_instructions();
        for expected in ["the action upload", "/api/backups/<name>", "/backup restore <name>", "is refused", "token=<the console's token>", "https://<this machine>:3345"] {
            assert!(said.contains(expected), "missing {expected:?}: {said}");
        }
        assert!(BACKUP_USAGE.contains("| upload |") && BACKUP_USAGE.contains("/backup upload says how"));
        // The route the console serves, as embra-web names it.
        let web = std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../embra-web/src/server.rs")).expect("embra-web's routes");
        assert!(web.contains("\"/api/backups/{name}\"") && web.contains("put(crate::backups::api_backup_put)"), "embra-web takes a PUT on the backups route");
    }
}

// ── Updates: /update ────────────────────────────────────────────────────

use std::sync::atomic::{AtomicBool, Ordering};

/// One `apply` at a time, process-wide (the `REBUILD_BATCH` pattern of the
/// Guardian). `/reboot` refuses while it is set.
static UPDATE_RUNNING: AtomicBool = AtomicBool::new(false);

pub(super) fn update_running() -> bool {
    UPDATE_RUNNING.load(Ordering::SeqCst)
}

struct ApplyGuard;

impl Drop for ApplyGuard {
    fn drop(&mut self) {
        UPDATE_RUNNING.store(false, Ordering::SeqCst);
    }
}

/// The operator's update source override on STATE (`/update source`).
pub(super) const UPDATE_SOURCE_FILE: &str = "/embra/state/update-source";

/// What `/update source <value>` does; pure.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum SourceAction {
    Show,
    Reset,
    Set(String),
    Refused(String),
}

pub(super) fn source_action(arg: &str) -> SourceAction {
    let v = arg.trim().trim_end_matches('/');
    if v.is_empty() {
        return SourceAction::Show;
    }
    if v == "reset" {
        return SourceAction::Reset;
    }
    if v.starts_with("https://") && v.len() > "https://".len() {
        return SourceAction::Set(v.to_string());
    }
    if let Some(rest) = v.strip_prefix("http://") {
        let host = rest.split(['/', ':']).next().unwrap_or("");
        let private = host == "localhost" || host.parse::<std::net::Ipv4Addr>().map(|ip| ip.is_private() || ip.is_loopback()).unwrap_or(false);
        if private {
            return SourceAction::Set(v.to_string());
        }
        return SourceAction::Refused(format!("http:// is for a private address only, not {host}; use https://"));
    }
    SourceAction::Refused(format!("not a source URL: {v} (https://…, or http:// to a private address, or reset)"))
}

const UPDATE_USAGE: &str = "Usage: /update [--tag <v…>] | apply [--tag <v…>] [--allow-downgrade] | status | rollback | source [<url>|reset]\n  /update checks the release published at the source against this one (the manifest's signature is verified first); apply writes the release into the inactive slot, verifies it and stages it for the next boot — /reboot now boots it, and GRUB falls back to this slot if it does not come up; --tag names one release of a GitHub releases source in place of the latest; --allow-downgrade applies a release older than this one, or another build of this version, which apply refuses without it; status shows both slots; rollback makes the other slot the default again; source shows, sets or resets the operator's own source (the default is the GitHub releases of the public mirror).";

/// What `/update` and `/update apply` hand on to the updater beyond the
/// command itself.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct UpdateOptions {
    /// One release of a GitHub releases source, by its tag, in place of the
    /// latest.
    pub(super) tag: Option<String>,
    /// Apply what the updater refuses otherwise: a release older than the
    /// running one, or another build of the running version.
    pub(super) allow_downgrade: bool,
}

/// A release tag as `--tag` takes it: `v`, then letters, digits, `.`, `_`
/// and `-`, 64 characters at most. The updater checks a tag itself
/// (`source::resolve`); this keeps anything else off its command line.
fn valid_release_tag(tag: &str) -> bool {
    tag.len() <= 64
        && !tag.contains("..")
        && tag
            .strip_prefix('v')
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')))
}

/// The options after `/update` (a check) or `/update apply`; pure. Only
/// the two the updater takes are accepted, each in its own form, and
/// `--allow-downgrade` belongs to an apply alone.
pub(super) fn update_options(rest: &str, apply: bool) -> Result<UpdateOptions, String> {
    let mut opts = UpdateOptions::default();
    let mut words = rest.split_whitespace();
    while let Some(word) = words.next() {
        match word {
            "--tag" => {
                let tag = words.next().ok_or("--tag needs a release tag, as in --tag v0.24.0-phase2")?;
                if !valid_release_tag(tag) {
                    return Err(format!("not a release tag: {tag} (v, then letters, digits, . _ -)"));
                }
                if opts.tag.replace(tag.to_string()).is_some() {
                    return Err("--tag is given twice".to_string());
                }
            }
            "--allow-downgrade" if apply => opts.allow_downgrade = true,
            "--allow-downgrade" => return Err("--allow-downgrade belongs to /update apply; a check needs none".to_string()),
            other => return Err(format!("/update does not take {other:?} here\n{UPDATE_USAGE}")),
        }
    }
    Ok(opts)
}

/// The updater's arguments for a check or an apply with these options.
pub(super) fn updater_args(apply: bool, opts: &UpdateOptions) -> Vec<String> {
    let mut args: Vec<String> = if apply { vec!["apply".into(), "--yes".into()] } else { vec!["check".into()] };
    if let Some(tag) = &opts.tag {
        args.push("--tag".into());
        args.push(tag.clone());
    }
    if opts.allow_downgrade {
        args.push("--allow-downgrade".into());
    }
    args
}

/// The suffix that makes a source a GitHub releases one, the base the
/// updater rewrites for a tag (`source::resolve` in embra-update).
const GITHUB_LATEST_SUFFIX: &str = "/releases/latest/download";

/// Whether `--tag` selects anything at the source in use. The updater
/// rewrites a GitHub `releases/latest/download` base to the tag's own
/// release and reads every other base as it is, so on the operator's own
/// mirror a tag would be dropped without a word and the mirror's release
/// checked or applied in its place. `source_file` is the text of
/// `UPDATE_SOURCE_FILE`, `None` when there is none (the default source,
/// which is GitHub's).
pub(super) fn tag_selects_a_release(source_file: Option<&str>) -> bool {
    match source_file.map(|t| t.trim().trim_end_matches('/')).filter(|t| !t.is_empty()) {
        Some(base) => base.ends_with(GITHUB_LATEST_SUFFIX),
        None => true,
    }
}

/// `update_options`, and the tag refused where it would select nothing.
fn options_for_this_source(rest: &str, apply: bool) -> Result<UpdateOptions, String> {
    let opts = update_options(rest, apply)?;
    if opts.tag.is_some() && !tag_selects_a_release(std::fs::read_to_string(UPDATE_SOURCE_FILE).ok().as_deref()) {
        return Err("--tag names one release of a GitHub releases source, and the source set here is your own, which serves the one release it holds (/update source shows it; /update source reset returns to the default).".to_string());
    }
    Ok(opts)
}

pub(super) async fn handle_update_command(args: &str, tx: &mpsc::Sender<Result<ConversationResponse, Status>>) {
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
    // `/update --tag v…`: a check's options with no word before them.
    let (sub, rest) = if sub.starts_with("--") { ("check", args.trim()) } else { (sub, rest) };
    match sub {
        "" | "check" => {
            let opts = match options_for_this_source(rest, false) {
                Ok(o) => o,
                Err(why) => {
                    send(why, SystemMessageType::Error).await;
                    return;
                }
            };
            let argv = updater_args(false, &opts);
            let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
            match run_updater(&argv).await {
                Ok(run) => {
                    let mut text = run.stdout.trim().to_string();
                    match run.code {
                        Some(0) => {
                            let apply = match &opts.tag {
                                Some(tag) => format!("/update apply --tag {tag}"),
                                None => "/update apply".to_string(),
                            };
                            text.push_str(&format!("\nRun {apply} to stage it into the inactive slot."));
                            send(text, SystemMessageType::Info).await;
                        }
                        Some(3) => send(text, SystemMessageType::Info).await,
                        _ => {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(&run.reason());
                            send(text, SystemMessageType::Error).await;
                        }
                    }
                }
                Err(e) => send(format!("The update check could not run: {e}"), SystemMessageType::Error).await,
            }
        }
        "apply" => {
            let opts = match options_for_this_source(rest, true) {
                Ok(o) => o,
                Err(why) => {
                    send(why, SystemMessageType::Error).await;
                    return;
                }
            };
            if UPDATE_RUNNING.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
                send("An update is being staged already — /update status shows it.".to_string(), SystemMessageType::Info).await;
                return;
            }
            send("Staging the update: the manifest is checked, then the image is written into the inactive slot and verified; the lines below follow the updater. The session stays usable meanwhile.".to_string(), SystemMessageType::Info).await;
            let tx = tx.clone();
            tokio::spawn(async move {
                let _guard = ApplyGuard;
                let forward = |content: String, kind: SystemMessageType| {
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
                let mut cmd = crate::tools::subprocess(UPDATER);
                cmd.args(updater_args(true, &opts))
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());
                let mut child = match cmd.spawn() {
                    Ok(c) => c,
                    Err(e) => {
                        forward(format!("The updater could not be started: {e}"), SystemMessageType::Error).await;
                        return;
                    }
                };
                let stdout = child.stdout.take();
                let stderr = child.stderr.take();
                let reader = async {
                    if let Some(out) = stdout {
                        use tokio::io::AsyncBufReadExt as _;
                        let mut lines = tokio::io::BufReader::new(out).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            if !line.trim().is_empty() {
                                forward(line, SystemMessageType::Info).await;
                            }
                        }
                    }
                };
                let errs = async {
                    let mut text = String::new();
                    if let Some(mut err) = stderr {
                        use tokio::io::AsyncReadExt as _;
                        let _ = err.read_to_string(&mut text).await;
                    }
                    text
                };
                let ((), stderr_text) = tokio::join!(reader, errs);
                let reason = stderr_text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no reason given").trim().to_string();
                match child.wait().await {
                    Ok(status) if status.code() == Some(0) => {
                        forward("Staged. /reboot now boots the staged release; if it does not come up, GRUB falls back to this one.".to_string(), SystemMessageType::Info).await
                    }
                    Ok(status) if status.code() == Some(3) => forward(reason, SystemMessageType::Info).await,
                    Ok(_) => forward(format!("The update was not staged: {reason}"), SystemMessageType::Error).await,
                    Err(e) => forward(format!("The updater could not be waited for: {e}"), SystemMessageType::Error).await,
                }
            });
        }
        "status" => match run_updater(&["status"]).await {
            Ok(run) if run.code == Some(0) => send(run.stdout.trim().to_string(), SystemMessageType::Info).await,
            Ok(run) => send(run.reason(), SystemMessageType::Error).await,
            Err(e) => send(format!("The status could not be read: {e}"), SystemMessageType::Error).await,
        },
        "rollback" => match run_updater(&["rollback"]).await {
            Ok(run) if run.code == Some(0) => send(format!("{}\n/reboot now when ready.", run.stdout.trim()), SystemMessageType::Info).await,
            Ok(run) => send(run.reason(), SystemMessageType::Error).await,
            Err(e) => send(format!("The rollback could not run: {e}"), SystemMessageType::Error).await,
        },
        "source" => match source_action(rest) {
            SourceAction::Show => {
                let text = match std::fs::read_to_string(UPDATE_SOURCE_FILE) {
                    Ok(t) if !t.trim().is_empty() => format!("Update source: {} (the operator's own; /update source reset returns to the default).", t.trim()),
                    _ => "Update source: the default, the GitHub releases of the public mirror (https://github.com/Ward-Software-Defined-Systems/embraOS/releases/latest/download). /update source <url> sets your own.".to_string(),
                };
                send(text, SystemMessageType::Info).await;
            }
            SourceAction::Reset => match tokio::fs::remove_file(UPDATE_SOURCE_FILE).await {
                Ok(()) => send("Update source reset to the default.".to_string(), SystemMessageType::Info).await,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => send("The default source was in use already.".to_string(), SystemMessageType::Info).await,
                Err(e) => send(format!("The source could not be reset: {e}"), SystemMessageType::Error).await,
            },
            SourceAction::Set(url) => {
                let tmp = format!("{UPDATE_SOURCE_FILE}.tmp");
                match tokio::fs::write(&tmp, format!("{url}\n")).await.and_then(|()| std::fs::rename(&tmp, UPDATE_SOURCE_FILE)) {
                    Ok(()) => send(format!("Update source set: {url}. /update checks it."), SystemMessageType::Info).await,
                    Err(e) => send(format!("The source could not be stored: {e}"), SystemMessageType::Error).await,
                }
            }
            SourceAction::Refused(why) => send(why, SystemMessageType::Error).await,
        },
        _ => send(UPDATE_USAGE.to_string(), SystemMessageType::Error).await,
    }
}

#[cfg(test)]
mod update_tests {
    use super::*;

    #[test]
    fn a_source_is_shown_set_or_reset_and_only_https_or_a_private_http_is_stored() {
        assert_eq!(source_action(""), SourceAction::Show);
        assert_eq!(source_action("reset"), SourceAction::Reset);
        assert_eq!(source_action("https://mirror.example/embraos/"), SourceAction::Set("https://mirror.example/embraos".into()));
        assert_eq!(source_action("http://10.0.0.5:8080/u"), SourceAction::Set("http://10.0.0.5:8080/u".into()));
        assert!(matches!(source_action("http://example.com/u"), SourceAction::Refused(m) if m.contains("private")));
        assert!(matches!(source_action("ftp://x"), SourceAction::Refused(_)));
        assert!(matches!(source_action("https://"), SourceAction::Refused(_)));
    }

    /// `--tag` and `--allow-downgrade` were on the updater's command line
    /// and no command passed them. Nothing but the two reaches it, each in
    /// its own form.
    #[test]
    fn a_check_takes_a_tag_and_an_apply_a_tag_and_the_downgrade_switch() {
        assert_eq!(update_options("", false), Ok(UpdateOptions::default()));
        assert_eq!(updater_args(false, &UpdateOptions::default()), ["check"]);
        assert_eq!(updater_args(true, &UpdateOptions::default()), ["apply", "--yes"]);

        let tagged = update_options("--tag v0.24.0-phase2", false).unwrap();
        assert_eq!(tagged.tag.as_deref(), Some("v0.24.0-phase2"));
        assert_eq!(updater_args(false, &tagged), ["check", "--tag", "v0.24.0-phase2"]);

        let both = update_options("--allow-downgrade  --tag v0.23.0-phase1", true).unwrap();
        assert_eq!(both, UpdateOptions { tag: Some("v0.23.0-phase1".into()), allow_downgrade: true });
        assert_eq!(updater_args(true, &both), ["apply", "--yes", "--tag", "v0.23.0-phase1", "--allow-downgrade"]);
        assert_eq!(updater_args(true, &update_options("--allow-downgrade", true).unwrap()), ["apply", "--yes", "--allow-downgrade"]);

        // A check stages nothing, so the switch is refused there by name.
        assert!(update_options("--allow-downgrade", false).unwrap_err().contains("belongs to /update apply"));
        // A tag is one word of the tag grammar, given once.
        assert!(update_options("--tag", false).unwrap_err().contains("needs a release tag"));
        assert!(update_options("--tag v1 --tag v2", true).unwrap_err().contains("twice"));
        for bad in ["0.24.0", "v", "v../x", "v1/2", "v1;reboot", "--source", "v1..2"] {
            assert!(update_options(&format!("--tag {bad}"), true).is_err(), "{bad}");
        }
        assert!(!valid_release_tag(&format!("v{}", "1".repeat(64))), "64 characters at most");
        // No other flag of the updater is reachable: not its source, not its
        // console, not a second command.
        for other in ["--source https://x.example", "--console", "--yes", "now", "rollback", "--tag=v1"] {
            assert!(update_options(other, true).unwrap_err().contains("does not take"), "{other}");
        }
    }

    /// A tag selects a release at a GitHub releases source and nowhere
    /// else: the updater reads any other base as it is, so the command
    /// refuses a tag there instead of checking another release than the
    /// one named.
    #[test]
    fn a_tag_selects_a_release_at_a_github_source_alone() {
        assert!(tag_selects_a_release(None), "the default source");
        assert!(tag_selects_a_release(Some("\n")), "an empty file is the default");
        assert!(tag_selects_a_release(Some("https://github.com/someone/embraOS/releases/latest/download\n")));
        assert!(tag_selects_a_release(Some("https://github.com/someone/embraOS/releases/latest/download/")));
        assert!(!tag_selects_a_release(Some("https://gitlab.ops.wsds/api/v4/projects/2/packages/generic/embraos/latest\n")));
        assert!(!tag_selects_a_release(Some("http://10.0.0.5:8080/updates")));
        // The suffix is the updater's own: `source::resolve` strips it for a tag.
        let updater = std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../embra-update/src/source.rs")).expect("the updater's source rule");
        assert!(updater.contains(&format!("strip_suffix(\"{GITHUB_LATEST_SUFFIX}\")")), "the updater rewrites another base for a tag now");
    }

    #[test]
    fn a_second_apply_is_refused_while_one_is_staging() {
        assert!(!update_running());
        assert!(UPDATE_RUNNING.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok());
        assert!(update_running());
        assert!(UPDATE_RUNNING.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err(), "the second apply is refused");
        {
            let _guard = ApplyGuard;
        }
        assert!(!update_running(), "the guard releases the flag");
    }
}
