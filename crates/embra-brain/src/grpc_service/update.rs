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

/// Pure: the argument and the text of `/proc/1/comm`.
pub(super) fn reboot_decision(args: &str, pid_one_comm: Option<&str>) -> RebootDecision {
    if !pid_one_is_embrad(pid_one_comm) {
        return RebootDecision::Refused(
            "Not running on embraOS: PID 1 is not embrad, so there is nothing to reboot from here.".to_string(),
        );
    }
    match args.trim() {
        "now" => RebootDecision::Reboot,
        "" => RebootDecision::Explain(
            "Reboot embraOS? Every session is kept; a turn in flight is stopped, and the console disconnects for about a minute while the machine comes back through the boot menu. Type /reboot now to proceed."
                .to_string(),
        ),
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
    match reboot_decision(args, comm.as_deref()) {
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
        assert!(matches!(reboot_decision("now", Some("systemd\n")), RebootDecision::Refused(_)));
        assert!(matches!(reboot_decision("now", None), RebootDecision::Refused(_)));
        assert!(matches!(reboot_decision("", Some("init")), RebootDecision::Refused(_)));
    }

    #[test]
    fn reboot_explains_first_and_acts_on_now() {
        assert!(matches!(reboot_decision("", Some("embrad\n")), RebootDecision::Explain(m) if m.contains("/reboot now")));
        assert_eq!(reboot_decision("now", Some("embrad\n")), RebootDecision::Reboot);
        assert_eq!(reboot_decision("  now ", Some("embrad")), RebootDecision::Reboot);
        assert!(matches!(reboot_decision("please", Some("embrad")), RebootDecision::Explain(m) if m.contains("please")));
    }
}
