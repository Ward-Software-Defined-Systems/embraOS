//! What the model sees of a session's history.
//!
//! A session's history is persisted as the operator's words and the
//! assistant's words, one message each. A turn the operator stopped
//! before any text arrived has no words of the assistant's, so
//! `final_assistant_text` (`grpc_service.rs`) writes
//! [`OPERATOR_STOP_MARKER`] in their place: the replay and the session
//! tools show the stop where it happened.
//!
//! Handed to the model in the assistant's slot, that marker reads as a
//! reply of its own. On 2026-10-15, after two stops in a row on short
//! prompts, Gemini 4 Argon answered the next four prompts with the
//! marker itself (six output tokens, no thinking, no tool call): the
//! pattern "short prompt, then `(turn stopped by operator)`" stood in
//! its context twice, and it completed it. So the model never sees the
//! marker as an assistant message: [`fold_stopped_turns`] runs over
//! every message list the brain assembles and folds a no-text stop into
//! the operator's next message as [`OPERATOR_STOP_NOTE`], between the
//! operator's two messages. Those two messages become one, so the roles
//! keep alternating (Gemini requires it), and the fold leaves one text
//! block per run of text (the OpenAI-compat path joins a turn's text
//! blocks with nothing). A stop that cut a reply mid-text keeps the
//! assistant's words and their suffix: those are its words.
//!
//! The stored history does not change; only what the model reads does.

use crate::provider::ir::{ApiMessage, Block};

/// The assistant text persisted for a turn the operator stopped before
/// any text had arrived. `final_assistant_text` writes it; the console
/// and the mobile chat replay it as the assistant's row.
pub const OPERATOR_STOP_MARKER: &str = "(turn stopped by operator)";

/// What the model reads in the operator's next message in place of the
/// marker. "The message above" is the operator's message the stopped
/// turn was answering; the note sits right under it.
pub const OPERATOR_STOP_NOTE: &str =
    "[Your reply to the message above was stopped by the operator before you had written anything.]";

/// Fold every no-text stop into the operator's next message.
///
/// `operator A, marker, operator B` becomes the one operator message
/// `A, note, B`. Two stops in a row fold into one message with two
/// notes. A marker that is the last message, or that does not sit
/// between two operator messages, stays as it is: the roles must keep
/// alternating, and the brain never sends a list that does not end on
/// the operator's own turn.
pub fn fold_stopped_turns(messages: Vec<ApiMessage>) -> Vec<ApiMessage> {
    let mut out: Vec<ApiMessage> = Vec::with_capacity(messages.len());
    for message in messages {
        let content = match message {
            ApiMessage::User { content } => content,
            other => {
                out.push(other);
                continue;
            }
        };
        let n = out.len();
        let foldable = n >= 2
            && is_stop_marker(&out[n - 1])
            && matches!(out[n - 2], ApiMessage::User { .. });
        if !foldable {
            out.push(ApiMessage::User { content });
            continue;
        }
        let marker = out.pop();
        match out.pop() {
            Some(ApiMessage::User { content: before }) => {
                out.push(ApiMessage::User { content: merge(before, content) });
            }
            // `foldable` checked this message; kept total rather than
            // panicking in a turn.
            other => {
                out.extend(other);
                out.extend(marker);
                out.push(ApiMessage::User { content });
            }
        }
    }
    out
}

/// An assistant message that is the marker and nothing else.
fn is_stop_marker(message: &ApiMessage) -> bool {
    match message {
        ApiMessage::Assistant { content } => {
            matches!(content.as_slice(), [Block::Text(text)] if text.trim() == OPERATOR_STOP_MARKER)
        }
        ApiMessage::User { .. } => false,
    }
}

/// `before`, the note, `after`, with adjacent text blocks joined by a
/// blank line: one text block per run; images and their labels stay
/// where they were.
fn merge(before: Vec<Block>, after: Vec<Block>) -> Vec<Block> {
    let note = std::iter::once(Block::Text(OPERATOR_STOP_NOTE.to_string()));
    let mut out: Vec<Block> = Vec::with_capacity(before.len() + after.len() + 1);
    for block in before.into_iter().chain(note).chain(after) {
        if let Block::Text(text) = &block
            && let Some(Block::Text(last)) = out.last_mut()
        {
            last.push_str("\n\n");
            last.push_str(text);
            continue;
        }
        out.push(block);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ir::ImageData;
    use std::sync::Arc;

    fn user(text: &str) -> ApiMessage {
        ApiMessage::user_text(text)
    }

    fn assistant(text: &str) -> ApiMessage {
        ApiMessage::Assistant { content: vec![Block::Text(text.to_string())] }
    }

    fn marker() -> ApiMessage {
        assistant(OPERATOR_STOP_MARKER)
    }

    fn texts(m: &ApiMessage) -> Vec<&str> {
        m.content()
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_stop_without_text_is_told_in_the_operators_next_message() {
        let out = fold_stopped_turns(vec![user("Run a slow tool"), marker(), user("Ok, try again")]);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], ApiMessage::User { .. }));
        let expected = format!("Run a slow tool\n\n{OPERATOR_STOP_NOTE}\n\nOk, try again");
        assert_eq!(texts(&out[0]), vec![expected.as_str()]);
    }

    #[test]
    fn two_stops_in_a_row_fold_into_one_message_with_two_notes() {
        let out = fold_stopped_turns(vec![user("A"), marker(), user("B"), marker(), user("C")]);
        assert_eq!(out.len(), 1);
        let expected = format!("A\n\n{OPERATOR_STOP_NOTE}\n\nB\n\n{OPERATOR_STOP_NOTE}\n\nC");
        assert_eq!(texts(&out[0]), vec![expected.as_str()]);
    }

    #[test]
    fn a_stop_that_cut_a_reply_keeps_the_assistants_words() {
        let partial = "Here is what I found\n\n(response interrupted by operator stop)";
        let out = fold_stopped_turns(vec![user("A"), assistant(partial), user("B")]);
        assert_eq!(out.len(), 3);
        assert_eq!(texts(&out[1]), vec![partial]);
    }

    #[test]
    fn a_marker_with_no_operator_message_after_it_stays() {
        let out = fold_stopped_turns(vec![user("A"), marker()]);
        assert_eq!(out.len(), 2);
        assert!(is_stop_marker(&out[1]));
    }

    #[test]
    fn a_marker_that_does_not_follow_an_operator_message_stays() {
        let out = fold_stopped_turns(vec![marker(), user("A")]);
        assert_eq!(out.len(), 2);
        assert!(is_stop_marker(&out[0]));
        let out = fold_stopped_turns(vec![user("A"), assistant("hi"), marker(), user("B")]);
        assert_eq!(out.len(), 4);
        assert!(is_stop_marker(&out[2]));
    }

    #[test]
    fn the_fold_changes_nothing_without_a_marker() {
        let out = fold_stopped_turns(vec![user("A"), assistant("hi"), user("B")]);
        assert_eq!(out.len(), 3);
        assert_eq!(texts(&out[0]), vec!["A"]);
        assert_eq!(texts(&out[1]), vec!["hi"]);
        assert_eq!(texts(&out[2]), vec!["B"]);
    }

    #[test]
    fn the_merged_turn_keeps_its_images_and_one_text_block_per_run() {
        let img = ImageData {
            media_type: "image/png".into(),
            data_b64: Arc::from("AAAA"),
            width: 1,
            height: 1,
            name: "a.png".into(),
        };
        let out = fold_stopped_turns(vec![
            user("A"),
            marker(),
            ApiMessage::user_with_images(vec![img], "look"),
        ]);
        assert_eq!(out.len(), 1);
        let blocks = out[0].content();
        assert_eq!(blocks.len(), 3, "{blocks:?}");
        let lead = format!("A\n\n{OPERATOR_STOP_NOTE}");
        assert!(matches!(&blocks[0], Block::Text(t) if *t == lead));
        assert!(matches!(blocks[1], Block::Image(_)));
        assert!(matches!(&blocks[2], Block::Text(t) if t == "look"));
        // Never two text blocks side by side: the OpenAI-compat path
        // joins them with nothing.
        assert!(!blocks.windows(2).any(|w| matches!(w, [Block::Text(_), Block::Text(_)])));
    }

    #[test]
    fn the_note_never_reads_as_the_marker() {
        assert!(!OPERATOR_STOP_NOTE.contains(OPERATOR_STOP_MARKER));
        assert!(OPERATOR_STOP_NOTE.starts_with('[') && OPERATOR_STOP_NOTE.ends_with(']'));
    }
}
