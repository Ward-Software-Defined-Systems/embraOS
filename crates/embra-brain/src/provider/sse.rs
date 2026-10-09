//! Line framing for server-sent event streams, shared by the three
//! provider parsers.
//!
//! A response body arrives in chunks cut wherever the network cut them. A
//! boundary can fall inside a line, and inside a character. Decoding each
//! chunk on its own turns the two halves of a multi-byte character into
//! replacement characters: in the text the operator reads, and in the
//! arguments of a tool call, where it is silent corruption of what gets
//! written. [`LineBuffer`] keeps BYTES until a line is complete and decodes
//! the line. A newline byte is never part of a multi-byte sequence, so a
//! cut there is always safe.

/// Read bytes are dropped from the front once this many have piled up.
const COMPACT_AT: usize = 64 * 1024;

/// Chunks in, complete lines out.
#[derive(Default)]
pub(crate) struct LineBuffer {
    pending: Vec<u8>,
    /// Where the unread part of `pending` starts.
    start: usize,
    /// No newline lies between `start` and here.
    scanned: usize,
}

impl LineBuffer {
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        if self.start == self.pending.len() {
            self.pending.clear();
            self.start = 0;
            self.scanned = 0;
        } else if self.start >= COMPACT_AT {
            self.pending.drain(..self.start);
            self.scanned -= self.start;
            self.start = 0;
        }
        self.pending.extend_from_slice(chunk);
    }

    /// The next complete line, without its `\n` and without any `\r`
    /// before it. `None` until a line is complete; an unterminated tail
    /// stays for the next chunk. Bytes that are not UTF-8 decode to the
    /// replacement character, within their own line.
    pub(crate) fn next_line(&mut self) -> Option<String> {
        let found = self.pending[self.scanned..]
            .iter()
            .position(|&b| b == b'\n');
        let Some(offset) = found else {
            self.scanned = self.pending.len();
            return None;
        };
        let newline = self.scanned + offset;
        let mut line = &self.pending[self.start..newline];
        while let [head @ .., b'\r'] = line {
            line = head;
        }
        let line = String::from_utf8_lossy(line).into_owned();
        self.start = newline + 1;
        self.scanned = self.start;
        Some(line)
    }
}

/// A server's message in an error frame is cut here before it is shown.
pub(crate) const ERROR_MESSAGE_MAX: usize = 1024;

/// The message of an in-stream error frame, or `None` for anything else.
///
/// An OpenAI-compatible server can answer inside an open 200 stream with
/// one frame `{"error": {"message": "…"}}` and close: LM Studio does so for
/// a prompt over its context length. Gemini's error shape is the same
/// object under `error`. No chunk of either wire carries a top-level
/// `error` key, so a frame with one is the server's refusal and never a
/// chunk: the parsers read it before their typed parse and fail the call
/// with the message. The message is `error.message` when that is a
/// string, `error` itself when it is one, otherwise the `error` value as
/// JSON; a null `error` is no error. Cut at [`ERROR_MESSAGE_MAX`].
pub(crate) fn in_stream_error(frame: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    let error = frame.as_object()?.get("error")?;
    let message = match error {
        Value::Null => return None,
        Value::String(s) => s.clone(),
        Value::Object(fields) => match fields.get("message") {
            Some(Value::String(s)) => s.clone(),
            _ => error.to_string(),
        },
        other => other.to_string(),
    };
    Some(crate::tools::sessions::truncate_str(&message, ERROR_MESSAGE_MAX).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_frame_gives_its_message_and_a_chunk_gives_none() {
        use serde_json::json;
        // LM Studio, 2026-10-08, verbatim.
        let lm_studio = json!({"error": {"message":
            "Input does not fit in context length. The input has 359277 tokens, \
             but the context length only supports 262144 tokens."}});
        assert_eq!(
            in_stream_error(&lm_studio).as_deref(),
            Some(
                "Input does not fit in context length. The input has 359277 tokens, \
                 but the context length only supports 262144 tokens."
            )
        );
        assert_eq!(in_stream_error(&json!({"error": "slot busy"})).as_deref(), Some("slot busy"));
        // No message string: the error value itself, as JSON.
        assert_eq!(
            in_stream_error(&json!({"error": {"code": 503, "type": "server_error"}})).as_deref(),
            Some(r#"{"code":503,"type":"server_error"}"#)
        );
        // A null error is no error; a chunk of either wire has no `error`.
        assert_eq!(in_stream_error(&json!({"error": null})), None);
        assert_eq!(in_stream_error(&json!({"choices": [], "usage": {"total_tokens": 9}})), None);
        assert_eq!(in_stream_error(&json!({"candidates": [{"content": {"parts": []}}]})), None);
        assert_eq!(in_stream_error(&json!("error")), None);
        assert_eq!(in_stream_error(&json!(null)), None);
        // Cut at the cap, on a character boundary: the e-acute straddles it.
        let long = format!("{}\u{e9}", "x".repeat(ERROR_MESSAGE_MAX - 1));
        assert_eq!(
            in_stream_error(&json!({"error": long})).as_deref(),
            Some("x".repeat(ERROR_MESSAGE_MAX - 1).as_str())
        );
    }

    fn lines_of(chunks: &[&[u8]]) -> Vec<String> {
        let mut buf = LineBuffer::default();
        let mut out = Vec::new();
        for chunk in chunks {
            buf.push(chunk);
            while let Some(line) = buf.next_line() {
                out.push(line);
            }
        }
        out
    }

    #[test]
    fn lines_come_out_whole_however_the_chunks_were_cut() {
        let body = b"data: one\n\ndata: two\r\n: keep-alive\ndata: three\n";
        let want = ["data: one", "", "data: two", ": keep-alive", "data: three"];
        assert_eq!(lines_of(&[body]), want);
        // Every possible single cut, and one byte at a time.
        for cut in 0..=body.len() {
            assert_eq!(lines_of(&[&body[..cut], &body[cut..]]), want, "cut at {cut}");
        }
        let bytes: Vec<&[u8]> = body.chunks(1).collect();
        assert_eq!(lines_of(&bytes), want);
    }

    #[test]
    fn a_character_cut_in_two_is_still_one_character() {
        // 2, 3 and 4 bytes: e-acute, an em dash, a CJK character, an emoji.
        let text = "data: caf\u{e9} \u{2014} \u{6f22} \u{1f600} end";
        let body = format!("{text}\n").into_bytes();
        for cut in 0..=body.len() {
            let got = lines_of(&[&body[..cut], &body[cut..]]);
            assert_eq!(got, [text], "cut at byte {cut}");
            assert!(!got[0].contains('\u{fffd}'), "cut at byte {cut}");
        }
        // Decoding the chunks one by one is what breaks it: the old way.
        let cut = body.iter().position(|&b| b == 0xc3).unwrap() + 1;
        let old = format!(
            "{}{}",
            String::from_utf8_lossy(&body[..cut]),
            String::from_utf8_lossy(&body[cut..])
        );
        assert!(old.contains('\u{fffd}'));
    }

    #[test]
    fn an_unterminated_tail_waits_for_its_newline() {
        let mut buf = LineBuffer::default();
        buf.push(b"data: par");
        assert_eq!(buf.next_line(), None);
        assert_eq!(buf.next_line(), None);
        buf.push(b"tial");
        assert_eq!(buf.next_line(), None);
        buf.push(b"\ndata: next");
        assert_eq!(buf.next_line().as_deref(), Some("data: partial"));
        assert_eq!(buf.next_line(), None);
        buf.push(b"\n");
        assert_eq!(buf.next_line().as_deref(), Some("data: next"));
        assert_eq!(buf.next_line(), None);
    }

    #[test]
    fn bytes_that_are_not_utf8_stay_in_their_line() {
        let got = lines_of(&[b"data: \xff\xfe bad\ndata: good\n"]);
        assert_eq!(got, ["data: \u{fffd}\u{fffd} bad", "data: good"]);
    }

    #[test]
    fn a_long_stream_is_compacted_without_losing_a_byte() {
        let mut buf = LineBuffer::default();
        let mut got = 0usize;
        let mut carry = Vec::new();
        for i in 0..20_000usize {
            // Lines of 40 bytes pushed in uneven pieces, so the buffer is
            // never empty at a push and compaction has to happen.
            carry.extend_from_slice(format!("data: line {i:06} {}\n", "x".repeat(21)).as_bytes());
            let take = carry.len().min(7 + i % 13);
            let piece: Vec<u8> = carry.drain(..take).collect();
            buf.push(&piece);
            while let Some(line) = buf.next_line() {
                assert_eq!(line, format!("data: line {got:06} {}", "x".repeat(21)));
                got += 1;
            }
            assert!(buf.pending.len() < COMPACT_AT + 4096, "buffer grows without bound");
        }
        buf.push(&carry);
        while buf.next_line().is_some() {
            got += 1;
        }
        assert_eq!(got, 20_000);
    }
}
