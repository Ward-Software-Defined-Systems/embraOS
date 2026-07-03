//! Character-wrap layout for the input box — the single source of truth for
//! how input text wraps, how tall the box is, and where the cursor sits.
//!
//! The input box used to RENDER via ratatui's `Paragraph::wrap` (word-boundary
//! wrapping) while computing its height and cursor position with pure
//! character arithmetic (`w / width`, `w % width`). Word-wrap pushes more text
//! onto continuation rows than character math assumes, so on any soft-wrapped
//! line the hardware cursor landed *inside* the rendered text (and the box
//! height could be provisioned wrong). One packing function now feeds all
//! three consumers, so they cannot disagree by construction. Character wrap is
//! also what the conversation pane already does (`window_to_visible_rows`) and
//! what terminal input conventionally does (readline/shell prompts).

use unicode_width::UnicodeWidthChar;

/// The packed layout of the input buffer at a given inner width.
pub struct InputLayout {
    /// Visual rows: char-packed lines (explicit `'\n'` respected, soft wrap
    /// by display width). Never empty for a non-empty computation — but may
    /// be `vec![""]` for an empty buffer.
    pub rows: Vec<String>,
    /// 0-based visual row of the cursor. May equal `rows.len()` when the
    /// cursor sits just past a row that exactly fills the width (the
    /// "phantom row" convention, preserved from the legacy math — the cursor
    /// wraps to column 0 of a not-yet-rendered row).
    pub cursor_row: usize,
    /// 0-based display-width column of the cursor within its row.
    pub cursor_col: u16,
}

impl InputLayout {
    /// Rows the box must show to contain both the text and the cursor
    /// (the phantom row counts when the cursor is on it). Minimum 1.
    pub fn content_rows(&self) -> usize {
        self.rows.len().max(self.cursor_row + 1).max(1)
    }
}

/// Pack `input` into visual rows of at most `inner_width` display cells and
/// locate the cursor (a char index into `input`) in the same packing.
///
/// Packing rules:
/// - explicit `'\n'` ends the current row (a trailing `'\n'` yields an empty
///   final row);
/// - soft wrap is greedy by display width (`UnicodeWidthChar`): a char that
///   would overflow the row starts the next row whole — a width-2 char never
///   splits, and zero-width chars (combiners) never trigger a wrap;
/// - `cursor_pos` values past the end of the buffer clamp to end-of-text.
pub fn layout_input(input: &str, cursor_pos: usize, inner_width: usize) -> InputLayout {
    if inner_width == 0 {
        // Degenerate box — nothing renderable; keep the cursor pinned.
        return InputLayout { rows: vec![String::new()], cursor_row: 0, cursor_col: 0 };
    }

    let mut rows: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut col: usize = 0; // display cells used in `current`
    let mut cursor_row = 0usize;
    let mut cursor_col = 0u16;

    // Record the cursor at the position *before* consuming char `i`,
    // normalizing the exactly-full-row case to (next row, col 0) — the
    // phantom-row convention.
    let mut place_cursor = |completed_rows: usize, col: usize| {
        if col >= inner_width {
            cursor_row = completed_rows + 1;
            cursor_col = 0;
        } else {
            cursor_row = completed_rows;
            cursor_col = col as u16;
        }
    };

    for (i, ch) in input.chars().enumerate() {
        if i == cursor_pos {
            place_cursor(rows.len(), col);
        }
        if ch == '\n' {
            rows.push(std::mem::take(&mut current));
            col = 0;
            continue;
        }
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w > 0 && col + w > inner_width && !current.is_empty() {
            // Soft wrap: this char starts the next row whole.
            rows.push(std::mem::take(&mut current));
            col = 0;
        }
        current.push(ch);
        col += w;
    }
    // Cursor at (or clamped to) end of text.
    if cursor_pos >= input.chars().count() {
        place_cursor(rows.len(), col);
    }
    rows.push(current);

    InputLayout { rows, cursor_row, cursor_col }
}

/// First visible row so the cursor row stays inside a viewport of
/// `visible_rows`. 0 when everything fits (or the viewport is degenerate).
pub fn scroll_offset(cursor_row: usize, visible_rows: usize) -> usize {
    if visible_rows == 0 {
        return 0;
    }
    (cursor_row + 1).saturating_sub(visible_rows)
}

#[cfg(test)]
mod tests {
    use super::{layout_input, scroll_offset};

    #[test]
    fn no_wrap_short_line_cursor_inline() {
        let l = layout_input("hello", 5, 20);
        assert_eq!(l.rows, vec!["hello"]);
        assert_eq!((l.cursor_row, l.cursor_col), (0, 5));
        assert_eq!(l.content_rows(), 1);
    }

    #[test]
    fn soft_wrap_spaces_cursor_lands_after_last_char() {
        // The screenshot case: a wrapping line with spaces. Under the old
        // word-wrap render + char-math cursor, the cursor sat 2-3 cells
        // inside the text. With one packing, it must land exactly after the
        // final char of the final row.
        let text = "aaaa bbbb cccc";
        let l = layout_input(text, text.chars().count(), 10);
        assert_eq!(l.rows, vec!["aaaa bbbb ", "cccc"]);
        assert_eq!((l.cursor_row, l.cursor_col), (1, 4));
    }

    #[test]
    fn exact_width_multiple_puts_cursor_on_phantom_row() {
        let l = layout_input("abcde", 5, 5);
        assert_eq!(l.rows, vec!["abcde"]);
        assert_eq!((l.cursor_row, l.cursor_col), (1, 0));
        assert_eq!(l.content_rows(), 2); // box must include the phantom row
    }

    #[test]
    fn explicit_newlines_mix_with_soft_wrap() {
        let text = "ab\ncdefgh";
        let l = layout_input(text, text.chars().count(), 5);
        assert_eq!(l.rows, vec!["ab", "cdefg", "h"]);
        assert_eq!((l.cursor_row, l.cursor_col), (2, 1));
    }

    #[test]
    fn trailing_newline_yields_empty_final_row() {
        let l = layout_input("ab\n", 3, 10);
        assert_eq!(l.rows, vec!["ab", ""]);
        assert_eq!((l.cursor_row, l.cursor_col), (1, 0));
    }

    #[test]
    fn wide_char_wraps_to_next_row_at_boundary() {
        // '日' is width 2; only 1 cell remains on the first row, so it must
        // move whole to the next row (never split).
        let text = "abc日";
        let l = layout_input(text, text.chars().count(), 4);
        assert_eq!(l.rows, vec!["abc", "日"]);
        assert_eq!((l.cursor_row, l.cursor_col), (1, 2));
    }

    #[test]
    fn cursor_mid_text_row_and_col() {
        let l = layout_input("abcdefgh", 6, 5);
        assert_eq!(l.rows, vec!["abcde", "fgh"]);
        assert_eq!((l.cursor_row, l.cursor_col), (1, 1));
    }

    #[test]
    fn content_rows_includes_phantom_row() {
        // Cursor beyond the last packed row (exactly-full row) must still be
        // given a row to live on.
        let l = layout_input("abcdeabcde", 10, 5);
        assert_eq!(l.rows.len(), 2);
        assert_eq!((l.cursor_row, l.cursor_col), (2, 0));
        assert_eq!(l.content_rows(), 3);
    }

    #[test]
    fn scroll_offset_keeps_cursor_row_visible() {
        assert_eq!(scroll_offset(9, 8), 2);
        assert_eq!(scroll_offset(7, 8), 0);
        assert_eq!(scroll_offset(8, 8), 1);
        assert_eq!(scroll_offset(3, 0), 0); // degenerate viewport guard
    }

    #[test]
    fn zero_inner_width_is_defensive() {
        let l = layout_input("anything", 3, 0);
        assert_eq!(l.rows, vec![""]);
        assert_eq!((l.cursor_row, l.cursor_col), (0, 0));
        assert_eq!(l.content_rows(), 1);
    }
}
