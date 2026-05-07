//! Live-reasoning buffer + UTF-8 cursor helpers.

/// Hard cap on the live reasoning buffer. The buffer is transient
/// (cleared at turn boundaries) — the cap exists to prevent a
/// pathological reasoning stream from growing console memory unbounded.
pub const MAX_LIVE_REASONING_BYTES: usize = 64 * 1024;

/// Append a reasoning shard to the live buffer, hard-capping at
/// `MAX_LIVE_REASONING_BYTES`. When the cap would be exceeded we drop
/// the oldest content (UTF-8 boundary safe via `char_indices`).
pub fn append_live_reasoning(buffer: &mut String, shard: &str) {
    if shard.is_empty() {
        return;
    }
    if shard.len() >= MAX_LIVE_REASONING_BYTES {
        // Single shard already exceeds cap — keep only its tail.
        let tail_start = shard.len() - MAX_LIVE_REASONING_BYTES;
        let safe_start = shard
            .char_indices()
            .find(|(i, _)| *i >= tail_start)
            .map(|(i, _)| i)
            .unwrap_or(shard.len());
        buffer.clear();
        buffer.push_str(&shard[safe_start..]);
        return;
    }
    let needed = buffer.len() + shard.len();
    if needed > MAX_LIVE_REASONING_BYTES {
        // Drop oldest bytes to make room. Walk forward to a UTF-8
        // boundary >= drop_amount so we never split a char.
        let drop_amount = needed - MAX_LIVE_REASONING_BYTES;
        let safe_drop = buffer
            .char_indices()
            .find(|(i, _)| *i >= drop_amount)
            .map(|(i, _)| i)
            .unwrap_or(buffer.len());
        buffer.replace_range(..safe_drop, "");
    }
    buffer.push_str(shard);
}

/// Convert a char index to a byte index in a string.
/// `char_pos` is a character offset; `String::insert`/`remove` need byte offsets.
pub fn char_to_byte_pos(s: &str, char_pos: usize) -> usize {
    s.char_indices()
        .nth(char_pos)
        .map(|(byte_idx, _)| byte_idx)
        .unwrap_or(s.len())
}

/// Return the number of characters in a string (not bytes).
pub fn char_count(s: &str) -> usize {
    s.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_live_reasoning_caps_at_64_kib_with_tail_keep() {
        // Pathological single-shard input larger than the cap should
        // truncate to the tail.
        let mut buf = String::new();
        let big = "a".repeat(64 * 1024 + 100);
        append_live_reasoning(&mut buf, &big);
        assert_eq!(buf.len(), 64 * 1024);
        // Tail of the original input is what remains.
        assert!(buf.ends_with(&"a".repeat(10)));
    }

    #[test]
    fn append_live_reasoning_drops_oldest_to_fit_within_cap() {
        let mut buf = "x".repeat(64 * 1024 - 5);
        append_live_reasoning(&mut buf, "yyyyyyyyyy"); // 10 bytes
        assert_eq!(buf.len(), 64 * 1024 - 5 + 10 - 5);
        // Note: 5 bytes dropped from the front; the new shard's full
        // 10 bytes are appended.
        assert!(buf.ends_with("yyyyyyyyyy"));
    }

    #[test]
    fn append_live_reasoning_handles_utf8_drop_safely() {
        // Multi-byte chars at the drop boundary must not be split.
        let mut buf = "é".repeat(32 * 1024); // 2 bytes per é = 64 KiB exactly
        let original_byte_len = buf.len();
        append_live_reasoning(&mut buf, "abc");
        assert!(buf.len() <= 64 * 1024);
        assert!(buf.ends_with("abc"));
        // Ensure no panic from splitting a multi-byte char — buffer
        // remains valid UTF-8 (which String guarantees structurally
        // anyway, but the test crashes if `replace_range` were called
        // on a non-boundary).
        assert!(std::str::from_utf8(buf.as_bytes()).is_ok());
        assert!(buf.len() < original_byte_len + 3 + 1); // grew by ≤ shard+1
    }

    #[test]
    fn empty_shard_is_no_op() {
        let mut buf = "kept".to_string();
        append_live_reasoning(&mut buf, "");
        assert_eq!(buf, "kept");
    }

    #[test]
    fn char_to_byte_pos_maps_ascii() {
        assert_eq!(char_to_byte_pos("hello", 0), 0);
        assert_eq!(char_to_byte_pos("hello", 3), 3);
        assert_eq!(char_to_byte_pos("hello", 5), 5);
        assert_eq!(char_to_byte_pos("hello", 99), 5);
    }

    #[test]
    fn char_to_byte_pos_maps_multibyte() {
        // "éàü" = 3 chars, 6 bytes
        assert_eq!(char_to_byte_pos("éàü", 0), 0);
        assert_eq!(char_to_byte_pos("éàü", 1), 2);
        assert_eq!(char_to_byte_pos("éàü", 2), 4);
        assert_eq!(char_to_byte_pos("éàü", 3), 6);
    }

    #[test]
    fn char_count_counts_chars_not_bytes() {
        assert_eq!(char_count(""), 0);
        assert_eq!(char_count("hello"), 5);
        assert_eq!(char_count("éàü"), 3);
    }
}
