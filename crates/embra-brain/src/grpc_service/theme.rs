//! `/theme`: the look of the web console.
//!
//! The browser's page, the phone's page and the machine's own display are
//! one page, and one theme is in force for the whole machine
//! (`embra_tools_core::theme`): a name in one small file on STATE. This
//! command and the web console's picker write that file; the web console
//! reads it for every page it serves and every status answer, so nothing
//! here tells it: an open page follows at its next poll.
//!
//! The command is answered before a session too (`update::SetupCommands`):
//! a look can be chosen while the wizard or Learning Mode runs.

use std::path::Path;

use tokio::sync::mpsc;
use tonic::Status;

use embra_common::proto::brain::{conversation_response, ConversationResponse, SystemMessage, SystemMessageType};
use embra_tools_core::theme::{self, THEMES};

/// What follows a change, said with it.
const FOLLOW: &str = "Every open page of the web console and the display console follow within a few seconds. The serial console keeps its terminal's colours.";

fn names() -> String {
    THEMES.iter().map(|t| t.name).collect::<Vec<_>>().join(", ")
}

/// The themes and the one in use.
fn listing(in_use: &theme::Theme) -> String {
    let width = THEMES.iter().map(|t| t.name.len()).max().unwrap_or(0);
    let mut out = format!("Theme: {}.\n", in_use.name);
    for t in THEMES {
        let mut notes = vec![t.note.to_string()];
        if t.name == theme::DEFAULT {
            notes.push("the default".to_string());
        }
        if t.name == in_use.name {
            notes.push("in use".to_string());
        }
        out.push_str(&format!("  {:width$}  {}\n", t.name, notes.join("; ")));
    }
    out.push_str(&format!(
        "/theme <name> sets it for the whole machine: the web console in every browser, the phone's page and the display console. /theme reset goes back to {}. Every theme holds the contrast WCAG 2.1 asks of text at level AA (4.5:1); the two high-contrast ones hold 7:1 (AAA). The web console's sidebar and the phone's command list open a picker that shows each theme in its own colours.",
        theme::DEFAULT
    ));
    out
}

/// What `/theme` answers to `args`, with the machine's theme kept in `file`.
pub(super) fn theme_answer(args: &str, file: &Path) -> (String, SystemMessageType) {
    match args.trim() {
        "" => (listing(theme::read(file)), SystemMessageType::Info),
        "reset" => match theme::reset(file) {
            Ok(()) => (format!("Theme back to the default, {}. {FOLLOW}", theme::DEFAULT), SystemMessageType::Info),
            Err(e) => (format!("The theme could not be reset ({e}); it stays as it is."), SystemMessageType::Error),
        },
        name => match theme::write(file, name) {
            Ok(set) => (format!("Theme set to {}. {FOLLOW}", set.name), SystemMessageType::Info),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
                // What was typed is said back cut short and without control characters.
                let shown: String = name.chars().filter(|c| !c.is_control()).take(40).collect();
                (format!("No theme is called '{shown}'. The themes: {}. /theme alone says what each is.", names()), SystemMessageType::Error)
            }
            Err(e) => (format!("The theme could not be stored ({e}); it stays as it is."), SystemMessageType::Error),
        },
    }
}

pub(super) async fn handle_theme_command(args: &str, tx: &mpsc::Sender<Result<ConversationResponse, Status>>) {
    let (content, kind) = theme_answer(args, Path::new(theme::STATE_FILE));
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::System(SystemMessage { content, msg_type: kind as i32 })),
        }))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("embra-brain-theme-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir.join("theme")
    }

    #[test]
    fn theme_alone_lists_every_theme_and_marks_the_default_and_the_one_in_use() {
        let file = scratch("listing");
        let (said, kind) = theme_answer("", &file);
        assert_eq!(kind, SystemMessageType::Info);
        assert!(said.starts_with(&format!("Theme: {}.\n", theme::DEFAULT)), "{said}");
        for t in THEMES {
            assert!(said.contains(&format!("  {}", t.name)) && said.contains(t.note), "{}: {said}", t.name);
        }
        assert_eq!(said.matches("the default; in use").count(), 1, "{said}");
        assert!(said.contains("/theme <name>") && said.contains("/theme reset") && said.contains("4.5:1") && said.contains("7:1"), "{said}");
        assert!(!file.exists(), "a listing writes nothing");

        let other = THEMES.last().unwrap();
        theme::write(&file, other.name).unwrap();
        let (said, _) = theme_answer("  ", &file);
        assert!(said.starts_with(&format!("Theme: {}.\n", other.name)), "{said}");
        assert!(said.contains(&format!("{}; in use", other.note)), "{said}");
        assert!(said.contains("; the default\n") && !said.contains("the default; in use"), "{said}");
    }

    #[test]
    fn a_name_sets_the_theme_and_reset_goes_back_to_the_default() {
        let file = scratch("set");
        let other = THEMES.last().unwrap();
        assert_ne!(other.name, theme::DEFAULT);
        let (said, kind) = theme_answer(&format!(" {} ", other.name.to_uppercase()), &file);
        assert_eq!(kind, SystemMessageType::Info);
        assert_eq!(said, format!("Theme set to {}. {FOLLOW}", other.name));
        assert_eq!(theme::read(&file).name, other.name);

        let (said, kind) = theme_answer("reset", &file);
        assert_eq!(kind, SystemMessageType::Info);
        assert_eq!(said, format!("Theme back to the default, {}. {FOLLOW}", theme::DEFAULT));
        assert!(!file.exists());
        assert_eq!(theme_answer("reset", &file).1, SystemMessageType::Info, "a second reset finds nothing to do and is no error");
    }

    #[test]
    fn a_word_that_is_no_theme_is_refused_with_the_list_and_changes_nothing() {
        let file = scratch("refused");
        theme::write(&file, theme::DEFAULT).unwrap();
        for typed in ["plaid", "ember dark", "../ember", "--help"] {
            let (said, kind) = theme_answer(typed, &file);
            assert_eq!(kind, SystemMessageType::Error, "{typed}");
            assert_eq!(said, format!("No theme is called '{typed}'. The themes: {}. /theme alone says what each is.", names()));
        }
        // What is said back is cut short and carries no control character.
        let (said, _) = theme_answer(&format!("x\u{1b}[31m{}", "y".repeat(80)), &file);
        assert!(said.starts_with(&format!("No theme is called 'x[31m{}'.", "y".repeat(35))), "{said}");
        assert_eq!(theme::read(&file).name, theme::DEFAULT);
    }

    #[test]
    fn a_theme_that_cannot_be_stored_is_an_error_that_says_so() {
        let file = scratch("unwritable").parent().unwrap().join("no-such-directory").join("theme");
        let (said, kind) = theme_answer(theme::DEFAULT, &file);
        assert_eq!(kind, SystemMessageType::Error);
        assert!(said.starts_with("The theme could not be stored (") && said.ends_with("); it stays as it is."), "{said}");
    }
}
