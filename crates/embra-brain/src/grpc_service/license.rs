//! The licence at a console: `/license`, the question, and the gate of a
//! configured machine's console.
//!
//! The rules are `crate::license`'s. Here is what a console is sent, and
//! where the operator's input waits. `Gate::hold` is the first thing the
//! operational loop does with a request that a console sent
//! (`console_request`): a message is the answer or is not sent on, and a
//! command is told to wait but the few that show the text, stop or leave.
//! An attach goes through, starts no briefing, and the question follows it.
//! What the brain dispatches itself never comes here: a turn a reminder or
//! a job asked for, the turn `/continue` hands over.
//!
//! The first boot's wizard asks the same question before the name
//! (`config::run_config_wizard_with`), and `/license` is answered before a
//! session too (`update::SetupCommands`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc;
use tonic::Status;

use embra_common::proto::brain::{
    ConversationRequest, ConversationResponse, SetupFieldType, SetupPrompt, SystemMessage, SystemMessageType, conversation_request, conversation_response,
};

use crate::license::{self, Answer, License};

type Tx = mpsc::Sender<Result<ConversationResponse, Status>>;

/// What a message that is no answer is told on a configured machine, before
/// the question comes again.
pub(crate) const NOT_SENT_ON: &str = "Your message was not sent on: the embraOS license is not accepted on this machine yet.";

async fn say(tx: &Tx, content: String, kind: SystemMessageType) {
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::System(SystemMessage { content, msg_type: kind as i32 })),
        }))
        .await;
}

/// Where an answer is given: its record names the place, and two sentences
/// differ by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Surface {
    /// The first boot's wizard, before the name.
    FirstBoot,
    /// A console of a configured machine.
    Console,
}

impl Surface {
    fn by(self) -> &'static str {
        match self {
            Surface::FirstBoot => license::AT_THE_WIZARD,
            Surface::Console => license::AT_A_SESSION,
        }
    }

    fn not_accepted(self) -> &'static str {
        match self {
            Surface::FirstBoot => license::NOT_ACCEPTED_AT_FIRST_BOOT,
            Surface::Console => license::NOT_ACCEPTED,
        }
    }

    /// What a line that is no answer is told before the question comes
    /// again. The first boot has no message that could have gone anywhere:
    /// the question alone says it.
    fn no_answer(self) -> Option<&'static str> {
        match self {
            Surface::FirstBoot => None,
            Surface::Console => Some(NOT_SENT_ON),
        }
    }
}

/// What became of an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Taken {
    /// The licence is accepted on this machine: by this answer, or at
    /// another console before it arrived.
    Accepted,
    /// The question is on the console again.
    Asked,
    /// Nothing is accepted and no list of choices is on the console: it is
    /// free for a command.
    Free,
}

/// The question, when the licence is open: a selector whose first option
/// accepts nothing, with no default. `true` when it was asked.
pub(crate) async fn ask(tx: &Tx, license: &License) -> bool {
    let Some((prompt, options)) = license.question() else { return false };
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::Setup(SetupPrompt {
                field_type: SetupFieldType::Selector as i32,
                prompt,
                options,
                default_value: String::new(),
            })),
        }))
        .await;
    true
}

/// The whole text, in messages of a console's length.
async fn show(tx: &Tx, license: &License) {
    for message in license.show() {
        say(tx, message, SystemMessageType::Info).await;
    }
}

/// An answer to the question, from the first boot's wizard or from a
/// console of a configured machine.
///
/// The accepting option's own words are recorded, with where they were
/// given. The option that shows the text is followed by the question. The
/// refusing option accepts nothing and leaves the console free: one that
/// shows a list of choices takes no typed command, and the operator who
/// does not accept needs `/reboot`, `/backup` or the way back to the
/// release before. Any other line brings the question again.
///
/// A second console may still show the question when the first one has
/// answered it: whatever it sends then is told that nothing is left to
/// answer, and counts as accepted.
pub(crate) async fn take_answer(answer: &str, tx: &Tx, license: &License, surface: Surface) -> Taken {
    if !license.open() {
        say(tx, license.settled_words(), SystemMessageType::Info).await;
        return Taken::Accepted;
    }
    match license::answer(answer) {
        Answer::Accept => match license.accept(surface.by()) {
            Ok(accepted) => {
                say(tx, license.accepted_words(&accepted), SystemMessageType::Info).await;
                Taken::Accepted
            }
            Err(why) => {
                say(tx, format!("Not accepted: {why}. Nothing was changed."), SystemMessageType::Error).await;
                Taken::Free
            }
        },
        Answer::Show => {
            show(tx, license).await;
            ask(tx, license).await;
            Taken::Asked
        }
        Answer::Decline => {
            say(tx, surface.not_accepted().to_string(), SystemMessageType::Info).await;
            Taken::Free
        }
        Answer::Other => {
            if let Some(line) = surface.no_answer() {
                say(tx, line.to_string(), SystemMessageType::Info).await;
            }
            ask(tx, license).await;
            Taken::Asked
        }
    }
}

/// `/license`: how the licence stands on this machine and its whole text,
/// and the question after it while the licence is open (`true` then).
pub(super) async fn handle_license_command(args: &str, tx: &Tx, license: &License) -> bool {
    if !args.trim().is_empty() {
        say(tx, "Usage: /license. It shows how the embraOS license stands on this machine, and its whole text.".to_string(), SystemMessageType::Error).await;
        return false;
    }
    show(tx, license).await;
    ask(tx, license).await
}

/// The licence on one console's stream of a configured machine.
#[derive(Debug)]
pub(super) struct Gate {
    license: Arc<License>,
    /// The question is on this console: put there, and not answered from
    /// it. When another console accepts meanwhile, this one still shows its
    /// choices, and the one it sends next is no message for the
    /// intelligence.
    asked: AtomicBool,
}

impl Gate {
    pub(super) fn of(license: Arc<License>) -> Gate {
        Gate { license, asked: AtomicBool::new(false) }
    }

    /// The machine's.
    pub(super) fn machine() -> Gate {
        Gate::of(License::machine())
    }

    /// A machine whose image carries no text of the licence: nothing is
    /// asked there and nothing is held.
    #[cfg(test)]
    pub(super) fn without_a_text() -> Gate {
        Gate::of(Arc::new(License::at("/nonexistent/embra-license-text", "/nonexistent/embra-license-acceptance")))
    }

    /// Whether the licence waits for its answer. An attach starts no
    /// briefing then (`turn.rs`): the question is what a console is shown
    /// first, and a briefing is a turn that may run for minutes before it.
    pub(super) fn open(&self) -> bool {
        self.license.open()
    }

    /// A console's request. `Some` is the request when it goes on to the
    /// loop; `None` when it was taken here.
    ///
    /// While the licence is open a message is an answer to the question
    /// (`take_answer`) and is never sent on, and a command is run when it
    /// is one of `license::ANSWERED_WHILE_OPEN` and told to wait otherwise.
    /// An attach is no input to hold: the session is attached as always,
    /// and the question follows it (`after_attach`).
    pub(super) async fn hold(&self, req: ConversationRequest, tx: &Tx) -> Option<ConversationRequest> {
        if !self.license.open() {
            // Settled at another console while this one shows the question:
            // the choice it sends is told so, once, and is not sent on.
            if self.asked.load(Ordering::Relaxed)
                && let Some(conversation_request::RequestType::UserMessage(message)) = &req.request_type
                && license::answer(&message.content) != Answer::Other
            {
                self.asked.store(false, Ordering::Relaxed);
                say(tx, self.license.settled_words(), SystemMessageType::Info).await;
                return None;
            }
            return Some(req);
        }
        match &req.request_type {
            Some(conversation_request::RequestType::UserMessage(message)) => {
                let taken = take_answer(&message.content, tx, &self.license, Surface::Console).await;
                self.asked.store(taken == Taken::Asked, Ordering::Relaxed);
                None
            }
            Some(conversation_request::RequestType::SlashCommand(command)) => {
                if license::ANSWERED_WHILE_OPEN.contains(&command.command.trim()) {
                    Some(req)
                } else {
                    say(tx, license::command_waits(&command.command), SystemMessageType::Info).await;
                    None
                }
            }
            Some(conversation_request::RequestType::SessionAttach(_)) | None => Some(req),
        }
    }

    /// After a console's attach: the question, while the licence is open.
    pub(super) async fn after_attach(&self, tx: &Tx) {
        if ask(tx, &self.license).await {
            self.asked.store(true, Ordering::Relaxed);
        }
    }

    /// `/license` in a session.
    pub(super) async fn command(&self, args: &str, tx: &Tx) {
        if handle_license_command(args, tx, &self.license).await {
            self.asked.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::license::test_support::machine;
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

    fn attach() -> ConversationRequest {
        ConversationRequest { request_type: Some(conversation_request::RequestType::SessionAttach(SessionAttach { session_name: String::new() })) }
    }

    /// What a stream was sent, in order: a system line as its text, a
    /// question as `?` and its options.
    fn sent(rx: &mut mpsc::Receiver<Result<ConversationResponse, Status>>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(Ok(frame)) = rx.try_recv() {
            match frame.response_type {
                Some(conversation_response::ResponseType::System(m)) => out.push(m.content),
                Some(conversation_response::ResponseType::Setup(p)) => {
                    assert_eq!(p.field_type, SetupFieldType::Selector as i32);
                    assert_eq!(p.default_value, "", "no default");
                    assert!(p.prompt.ends_with("\n\nDo you accept the embraOS Proprietary License?"), "{}", p.prompt);
                    out.push(format!("? {}", p.options.join(" | ")));
                }
                other => panic!("neither a line nor a question: {other:?}"),
            }
        }
        out
    }

    const QUESTION: &str = "? I do not accept | Show the full text | I accept the license";

    /// While the licence is open the operator's input waits. A message that
    /// is no answer is not sent on and brings the question; the refusing
    /// answer is told so and leaves the console free; the showing answer
    /// shows the text and asks again; a command is told to wait but the few
    /// that show the text, stop or leave (`/continue` among them); an
    /// attach passes. The accepting answer is recorded, and then everything
    /// passes.
    #[tokio::test]
    async fn the_operators_input_waits_while_the_license_is_open() {
        let (root, license) = machine("hold", true);
        let license = Arc::new(license);
        let gate = Gate::of(license.clone());
        let (tx, mut rx) = mpsc::channel(64);
        assert!(gate.open());

        // A message that is no answer, and an empty one.
        assert!(gate.hold(message("hello there"), &tx).await.is_none());
        assert_eq!(sent(&mut rx), [NOT_SENT_ON, QUESTION]);
        assert!(gate.hold(message(""), &tx).await.is_none());
        assert_eq!(sent(&mut rx), [NOT_SENT_ON, QUESTION]);
        // The refusing answer: the first option, which Enter sends.
        assert!(gate.hold(message("I do not accept"), &tx).await.is_none());
        assert_eq!(sent(&mut rx), [license::NOT_ACCEPTED], "no list of choices follows: the console is free for a command");
        // The text, then the question again.
        assert!(gate.hold(message("Show the full text"), &tx).await.is_none());
        let shown = sent(&mut rx);
        assert!(shown[0].starts_with("The embraOS Proprietary License, Version ") && shown[0].contains("not accepted on this machine yet"), "{}", shown[0]);
        assert!(shown.len() > 3 && shown[1].starts_with("embraOS PROPRIETARY LICENSE"));
        assert_eq!(shown.last().map(String::as_str), Some(QUESTION));

        // Commands: the few are run, any other is told to wait.
        for answered in ["/license", "/help", "/continue", "/stop", "/reboot", "/update", "/backup"] {
            assert!(gate.hold(command(answered, ""), &tx).await.is_some(), "{answered}");
        }
        assert!(sent(&mut rx).is_empty());
        for waits in ["/provider", "/native", "/model", "/sessions", "/new", "/guardian", "/network", "/frobnicate"] {
            assert!(gate.hold(command(waits, "x"), &tx).await.is_none(), "{waits}");
            assert_eq!(sent(&mut rx), [license::command_waits(waits)], "{waits}");
        }
        // An attach is attached, and the question follows it.
        assert!(gate.hold(attach(), &tx).await.is_some());
        assert!(sent(&mut rx).is_empty());
        gate.after_attach(&tx).await;
        assert_eq!(sent(&mut rx), [QUESTION]);
        // `/license` in a session: the text, and the question after it.
        gate.command("", &tx).await;
        assert_eq!(sent(&mut rx).last().map(String::as_str), Some(QUESTION));
        assert!(license.open(), "nothing was accepted by any of it");
        assert!(!root.join("state").exists());

        // Accepted: recorded as given at a console, and then everything passes.
        assert!(gate.hold(message("I accept the license"), &tx).await.is_none());
        let said = sent(&mut rx);
        assert_eq!(said.len(), 1);
        assert!(said[0].starts_with("Accepted on this machine and recorded: the embraOS Proprietary License, Version "), "{}", said[0]);
        let license::Standing::Accepted(accepted) = license.standing() else { panic!("the acceptance was recorded") };
        assert_eq!(accepted.by, "session");
        assert!(!gate.open());
        assert!(gate.hold(message("hello there"), &tx).await.is_some());
        assert!(gate.hold(message("I do not accept"), &tx).await.is_some(), "words of the operator's, on the console that answered");
        assert!(gate.hold(command("/provider", ""), &tx).await.is_some());
        gate.after_attach(&tx).await;
        assert!(sent(&mut rx).is_empty(), "asked once: nothing more is said of it");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Two consoles show the question, and one accepts. The other still
    /// shows its choices: the one it sends is told that nothing is left to
    /// answer and is no message for the intelligence. That is said once;
    /// what the console sends before and after is the operator's own.
    #[tokio::test]
    async fn a_second_console_that_still_shows_the_question_is_told_it_is_settled() {
        let (root, license) = machine("second-console", true);
        let license = Arc::new(license);
        let (first, second, third) = (Gate::of(license.clone()), Gate::of(license.clone()), Gate::of(license.clone()));
        let (tx, mut rx) = mpsc::channel(64);
        first.after_attach(&tx).await;
        second.after_attach(&tx).await;
        assert_eq!(sent(&mut rx), [QUESTION, QUESTION]);

        assert!(first.hold(message("I accept the license"), &tx).await.is_none());
        sent(&mut rx);
        // A command and a message of the operator's own pass; the stale choice does not.
        assert!(second.hold(command("/status", ""), &tx).await.is_some());
        assert!(second.hold(message("what did I miss?"), &tx).await.is_some());
        assert!(sent(&mut rx).is_empty());
        for stale in ["I do not accept", "Show the full text", "I accept the license"] {
            let gate = Gate::of(license.clone());
            gate.asked.store(true, Ordering::Relaxed);
            assert!(gate.hold(message(stale), &tx).await.is_none(), "{stale}");
            let said = sent(&mut rx);
            assert_eq!(said.len(), 1, "{stale}");
            assert!(said[0].starts_with("The embraOS Proprietary License, Version ") && said[0].ends_with(", at a console): nothing is left to answer here."), "{}", said[0]);
        }
        assert!(second.hold(message("I do not accept"), &tx).await.is_none());
        assert_eq!(sent(&mut rx).len(), 1);
        assert!(second.hold(message("I do not accept"), &tx).await.is_some(), "said once");
        // A console that was never asked sends what it likes.
        assert!(third.hold(message("I accept the license"), &tx).await.is_some());
        assert!(sent(&mut rx).is_empty());
        let license::Standing::Accepted(accepted) = license.standing() else { panic!("accepted") };
        assert_eq!(accepted.by, "session", "the first console's record stands");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// On an image that carries no text of the licence nothing is held and
    /// nothing is asked: there is no text to show.
    #[tokio::test]
    async fn nothing_is_held_where_there_is_no_text_to_show() {
        let (root, license) = machine("hold-no-text", false);
        let (tx, mut rx) = mpsc::channel(8);
        for gate in [Gate::of(Arc::new(license)), Gate::without_a_text()] {
            assert!(!gate.open());
            assert!(gate.hold(message("hello"), &tx).await.is_some());
            assert!(gate.hold(message("I accept the license"), &tx).await.is_some());
            assert!(gate.hold(command("/provider", ""), &tx).await.is_some());
            gate.after_attach(&tx).await;
            assert!(sent(&mut rx).is_empty());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    /// `/license` shows how the licence stands and the whole text, and the
    /// question after it while the licence is open; it takes no argument.
    #[tokio::test]
    async fn the_license_command_shows_the_text_and_asks_while_it_is_open() {
        let (root, license) = machine("command", true);
        let (tx, mut rx) = mpsc::channel(64);
        assert!(handle_license_command("", &tx, &license).await, "the question was asked");
        let said = sent(&mut rx);
        assert!(said[0].contains("not accepted on this machine yet"));
        assert_eq!(said[1..said.len() - 1].join("\n"), crate::license::test_support::REPOSITORY_TEXT.trim_end_matches('\n'));
        assert_eq!(said.last().map(String::as_str), Some(QUESTION));
        assert!(!handle_license_command("accept", &tx, &license).await);
        assert_eq!(sent(&mut rx), ["Usage: /license. It shows how the embraOS license stands on this machine, and its whole text."]);
        assert!(license.open(), "the command accepts nothing");

        assert_eq!(take_answer("I accept the license", &tx, &license, Surface::Console).await, Taken::Accepted);
        sent(&mut rx);
        assert!(!handle_license_command("", &tx, &license).await);
        let said = sent(&mut rx);
        assert!(said[0].contains(": accepted on this machine (20"), "{}", said[0]);
        assert!(!said.last().unwrap().starts_with("? "), "no question once it is accepted");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The first boot waits at the question. The refusing answer accepts
    /// nothing and leaves the console free for a command; any other line
    /// brings the question again; the showing answer shows the text and
    /// asks; the accepting answer is recorded as given at the first boot.
    /// An answer that arrives after another console accepted counts as
    /// accepted and is told so.
    #[tokio::test]
    async fn the_first_boot_waits_at_the_question_until_it_is_accepted() {
        let (root, license) = machine("first-boot", true);
        let (tx, mut rx) = mpsc::channel(64);
        assert_eq!(take_answer("I do not accept", &tx, &license, Surface::FirstBoot).await, Taken::Free);
        assert_eq!(sent(&mut rx), [license::NOT_ACCEPTED_AT_FIRST_BOOT], "no list of choices follows");
        for nothing in ["", "yes", "accept", "I accept"] {
            assert_eq!(take_answer(nothing, &tx, &license, Surface::FirstBoot).await, Taken::Asked, "{nothing:?}");
            assert_eq!(sent(&mut rx), [QUESTION], "{nothing:?}");
        }
        assert_eq!(take_answer("Show the full text", &tx, &license, Surface::FirstBoot).await, Taken::Asked);
        assert_eq!(sent(&mut rx).last().map(String::as_str), Some(QUESTION));
        assert!(license.open());
        assert_eq!(take_answer("I accept the license", &tx, &license, Surface::FirstBoot).await, Taken::Accepted);
        let license::Standing::Accepted(accepted) = license.standing() else { panic!("the acceptance was recorded") };
        assert_eq!(accepted.by, "wizard");
        sent(&mut rx);

        // Another console's wizard, still at the question.
        for late in ["I do not accept", "I accept the license", ""] {
            assert_eq!(take_answer(late, &tx, &license, Surface::FirstBoot).await, Taken::Accepted, "{late:?}");
            let said = sent(&mut rx);
            assert_eq!(said.len(), 1);
            assert!(said[0].ends_with(", at the first boot): nothing is left to answer here."), "{}", said[0]);
        }
        assert_eq!(license.standing(), license::Standing::Accepted(accepted), "the first record stands");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A sentence that names commands names ones that are answered where it
    /// is said: while the licence is open on a configured machine, and
    /// before a session at the first boot.
    #[test]
    fn each_sentence_names_commands_that_are_answered_where_it_is_said() {
        fn commands(sentence: &str) -> Vec<&str> {
            sentence.split(|c: char| c.is_whitespace() || c == ',' || c == ';').filter(|w| w.starts_with('/')).map(|w| w.trim_end_matches(['.', ':'])).collect()
        }
        assert_eq!(commands(license::NOT_ACCEPTED), ["/license", "/reboot", "/update"]);
        for named in commands(license::NOT_ACCEPTED).into_iter().chain(commands(&license::command_waits("/x")).into_iter().skip(1)) {
            assert!(license::ANSWERED_WHILE_OPEN.contains(&named), "{named}");
        }
        assert_eq!(commands(license::NOT_ACCEPTED_AT_FIRST_BOOT), ["/license", "/reboot", "/backup"]);
        for named in commands(license::NOT_ACCEPTED_AT_FIRST_BOOT) {
            assert_ne!(super::super::update::setup_command(named), super::super::update::SetupCommand::Later, "{named}");
        }
        // Every command answered while the licence is open is one the brain
        // runs: an arm of its dispatcher.
        for named in license::ANSWERED_WHILE_OPEN {
            assert!(include_str!("slash.rs").contains(&format!("\n        \"{named}\" => ")), "{named}");
        }
    }
}
