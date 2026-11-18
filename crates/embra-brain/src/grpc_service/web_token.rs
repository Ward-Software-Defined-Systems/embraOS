//! `/web-token`: the web console's access token is made once and kept across
//! boots, and this command asks for a new one. Answered before a session as
//! in one (`update.rs`, `SetupCommands`): a URL that was seen by someone it
//! was not meant for is replaced without waiting for the setup to end.
//!
//! The brain never holds the token and no command shows it: a reply passes
//! the console and is kept with the session. The command leaves a request on
//! STATE (`embra_boot::web_token`); embrad makes the new token, starts the
//! web console and the display console again with it, and prints the new URL
//! where the old one was printed, on the machine's own consoles.

use std::path::Path;

use tokio::sync::mpsc;
use tonic::Status;

use embra_common::proto::brain::{conversation_response, ConversationResponse, SystemMessage, SystemMessageType};

use super::update::{pid_one_is_embrad, PID_ONE_COMM};

/// What `/web-token` does with its argument on this machine.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum WebTokenDecision {
    /// Not embraOS, or a boot without a web console.
    Refused(String),
    /// `/web-token` alone, or a word it does not know.
    Explain(String),
    /// `/web-token rotate`: the request goes to STATE.
    Rotate,
}

/// Where the new URL is read once the token is made again. No reply names
/// the URL itself.
const WHERE_THE_URL_IS: &str =
    "it is printed on the serial console and shown in the display console's menu (This console's URL), and no command shows it";

/// Pure: the argument, the text of `/proc/1/comm`, and whether this boot
/// runs the web console (the kernel line's last word on `embra.web` and
/// `embra.tui`).
pub(super) fn web_token_decision(args: &str, pid_one_comm: Option<&str>, web_console: bool) -> WebTokenDecision {
    if !pid_one_is_embrad(pid_one_comm) {
        return WebTokenDecision::Refused(
            "Not running on embraOS: PID 1 is not embrad, so there is no web console token to make again from here.".to_string(),
        );
    }
    if !web_console {
        return WebTokenDecision::Refused(
            "This boot runs the serial console and no web console (embra.tui), so no access token is in use.".to_string(),
        );
    }
    match args.trim() {
        "rotate" => WebTokenDecision::Rotate,
        "" => WebTokenDecision::Explain(format!(
            "The web console's URL carries an access token. It is made once and stays the same across boots, so a bookmark keeps working. /web-token rotate makes a new one: every browser is signed out, this one included, and the console is opened again from its new URL; {WHERE_THE_URL_IS}."
        )),
        other => WebTokenDecision::Explain(format!(
            "Unknown argument '{other}'. /web-token explains, /web-token rotate makes a new access token."
        )),
    }
}

/// What the operator is told once the request is on STATE.
fn asked_notice() -> String {
    format!(
        "A new access token is being made: within a few seconds every browser is signed out, this one included, and the web console restarts. Every session is kept. Open the console from its new URL; {WHERE_THE_URL_IS}."
    )
}

pub(super) async fn handle_web_token_command(args: &str, tx: &mpsc::Sender<Result<ConversationResponse, Status>>) {
    let comm = std::fs::read_to_string(PID_ONE_COMM).ok();
    let web_console = embra_boot::Cmdline::read().web_console();
    let (content, kind) = match web_token_decision(args, comm.as_deref(), web_console) {
        WebTokenDecision::Refused(msg) => (msg, SystemMessageType::Error),
        WebTokenDecision::Explain(msg) => (msg, SystemMessageType::Info),
        WebTokenDecision::Rotate => match embra_boot::web_token::ask_rotation(Path::new(embra_boot::web_token::DIR)) {
            Ok(()) => (asked_notice(), SystemMessageType::Info),
            Err(e) => (
                format!("The request for a new access token could not be written to STATE ({e}); the token stays as it is."),
                SystemMessageType::Error,
            ),
        },
    };
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::System(SystemMessage { content, msg_type: kind as i32 })),
        }))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_token_is_refused_off_embraos_and_on_a_boot_without_the_web_console() {
        assert!(matches!(web_token_decision("rotate", Some("systemd\n"), true), WebTokenDecision::Refused(m) if m.contains("PID 1 is not embrad")));
        assert!(matches!(web_token_decision("", None, true), WebTokenDecision::Refused(_)));
        assert!(matches!(web_token_decision("rotate", Some("embrad\n"), false), WebTokenDecision::Refused(m) if m.contains("no web console")));
    }

    #[test]
    fn web_token_explains_first_and_asks_on_rotate() {
        assert!(matches!(
            web_token_decision("", Some("embrad\n"), true),
            WebTokenDecision::Explain(m) if m.contains("stays the same across boots") && m.contains("/web-token rotate")
        ));
        assert_eq!(web_token_decision("rotate", Some("embrad\n"), true), WebTokenDecision::Rotate);
        assert_eq!(web_token_decision("  rotate ", Some("embrad"), true), WebTokenDecision::Rotate);
        assert!(matches!(web_token_decision("show", Some("embrad"), true), WebTokenDecision::Explain(m) if m.contains("'show'")));
    }

    /// No reply carries a URL or a token: the text of a command passes the
    /// console and is kept with the session, and neither holds the token.
    /// Every reply that sends the operator to the new URL says where it is.
    #[test]
    fn no_reply_shows_the_url_and_each_says_where_it_is_shown() {
        let mut replies = vec![asked_notice()];
        for args in ["", "rotate", "show"] {
            for (comm, web) in [(Some("embrad"), true), (Some("embrad"), false), (None, true)] {
                match web_token_decision(args, comm, web) {
                    WebTokenDecision::Refused(m) | WebTokenDecision::Explain(m) => replies.push(m),
                    WebTokenDecision::Rotate => {}
                }
            }
        }
        for reply in &replies {
            assert!(!reply.contains("https://") && !reply.contains("token="), "{reply}");
        }
        for reply in [&asked_notice(), &replies[1]] {
            assert!(reply.contains("the serial console") && reply.contains("the display console's menu"), "{reply}");
        }
    }
}
