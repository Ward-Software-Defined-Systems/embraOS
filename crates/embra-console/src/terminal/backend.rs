//! The console never asks its terminal a question.
//!
//! ratatui 0.30's `Terminal::clear()` saves the cursor and puts it back, and
//! it learns where the cursor is by ASKING the terminal: crossterm writes
//! `ESC[6n` and waits two seconds for the answer on stdin. ratatui 0.29
//! cleared without asking.
//!
//! The console has nobody to ask. On the web console's PTY the terminal is
//! a browser's xterm.js, and the console is started before any browser
//! attaches; one that attaches as an observer may not write at all. On the
//! serial line there is a terminal only while an operator's is connected.
//! An unanswered query is an error, `clear()` returns it, and the console
//! never draws: the browser shows an empty pane.
//!
//! [`QuietBackend`] answers from memory — the position last set through it
//! — and hands everything else to the backend it wraps. Where the cursor is
//! put back to does not matter to the console: every `clear()` is followed
//! by a draw, and a draw places or hides the cursor itself.
//!
//! At the next ratatui upgrade: 0.31 gives `Backend` a
//! `save_cursor_position` / `restore_cursor_position` pair whose defaults
//! go through `get_cursor_position`, so the wrapper keeps holding as it is.
//! Do not forward either of them to a backend that asks.
//!
//! The wrapper also keeps an unchanged frame silent. ratatui ends every
//! `draw` with the colour and attribute resets and hides or places the
//! cursor again, whether or not a cell changed: 21 bytes every 200 ms on
//! an idle screen. On the web console each of those writes had xterm.js
//! repaint and, on the display console, the whole screen composited in
//! software. A draw of no cells is not forwarded, and the cursor's
//! visibility and position go out only when they differ from what the
//! terminal was last told; a `clear()` forgets both, so the repaint that
//! follows an attach says everything again.

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

pub struct QuietBackend<B> {
    inner: B,
    /// Where the cursor was last put through this backend.
    cursor: Position,
    /// What the terminal was last told about the cursor's visibility, or
    /// nothing since the last clear.
    shown: Option<bool>,
    /// The terminal's cursor is known to stand at `cursor`: nothing drew or
    /// cleared since it was placed there.
    placed: bool,
}

impl<B> QuietBackend<B> {
    pub fn new(inner: B) -> Self {
        Self { inner, cursor: Position::ORIGIN, shown: None, placed: false }
    }
}

impl<B: Backend> Backend for QuietBackend<B> {
    type Error = B::Error;

    /// A frame with no changed cell is not forwarded: the wrapped backend
    /// would still write its resets.
    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut content = content.peekable();
        if content.peek().is_none() {
            return Ok(());
        }
        self.placed = false;
        self.inner.draw(content)
    }

    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        self.placed = false;
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        if self.shown == Some(false) {
            return Ok(());
        }
        self.shown = Some(false);
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        if self.shown == Some(true) {
            return Ok(());
        }
        self.shown = Some(true);
        self.inner.show_cursor()
    }

    /// From memory. NEVER forwarded: the wrapped backend would ask the
    /// terminal.
    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        let position = position.into();
        if self.placed && self.cursor == position {
            return Ok(());
        }
        self.cursor = position;
        self.placed = true;
        self.inner.set_cursor_position(position)
    }

    /// A cleared screen is told everything again: a browser that attached
    /// meanwhile started from nothing.
    fn clear(&mut self) -> Result<(), Self::Error> {
        self.shown = None;
        self.placed = false;
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.placed = false;
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::widgets::Paragraph;
    use ratatui::{Terminal, TerminalOptions, Viewport};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A terminal that records what it is told and cannot be asked: a
    /// cursor query is the error the real one gives when nobody answers.
    #[derive(Clone, Default)]
    struct Unanswering {
        log: Rc<RefCell<Vec<String>>>,
    }

    impl Unanswering {
        fn note(&self, what: impl Into<String>) {
            self.log.borrow_mut().push(what.into());
        }
        fn asked(&self) -> usize {
            self.log.borrow().iter().filter(|l| l.as_str() == "ASKED").count()
        }
    }

    impl Backend for Unanswering {
        type Error = std::io::Error;

        fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            self.note(format!("draw {}", content.count()));
            Ok(())
        }
        fn hide_cursor(&mut self) -> Result<(), Self::Error> {
            self.note("hide");
            Ok(())
        }
        fn show_cursor(&mut self) -> Result<(), Self::Error> {
            self.note("show");
            Ok(())
        }
        fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
            self.note("ASKED");
            Err(std::io::Error::other(
                "The cursor position could not be read within a normal duration",
            ))
        }
        fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
            let p = position.into();
            self.note(format!("cursor {},{}", p.x, p.y));
            Ok(())
        }
        fn clear(&mut self) -> Result<(), Self::Error> {
            self.note("clear");
            Ok(())
        }
        fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
            self.note(format!("clear {clear_type:?}"));
            Ok(())
        }
        fn size(&self) -> Result<Size, Self::Error> {
            Ok(Size::new(80, 24))
        }
        fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
            Ok(WindowSize { columns_rows: Size::new(80, 24), pixels: Size::new(800, 480) })
        }
        fn flush(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    fn viewports() -> [(&'static str, Viewport); 2] {
        [
            ("web (fullscreen)", Viewport::Fullscreen),
            ("serial (fixed)", Viewport::Fixed(Rect::new(0, 0, 80, 24))),
        ]
    }

    /// Why the wrapper exists. If this stops failing, the library stopped
    /// asking; the wrapper can then go, and so can this test.
    #[test]
    fn the_library_asks_the_terminal_on_every_clear() {
        for (name, viewport) in viewports() {
            let terminal = Unanswering::default();
            let mut screen =
                Terminal::with_options(terminal.clone(), TerminalOptions { viewport }).unwrap();
            let err = screen.clear().expect_err("clear() needs the cursor position");
            assert!(err.to_string().contains("cursor position"), "{name}: {err}");
            assert_eq!(terminal.asked(), 1, "{name}");
        }
    }

    #[test]
    fn behind_the_wrapper_nothing_is_asked() {
        for (name, viewport) in viewports() {
            let terminal = Unanswering::default();
            let mut screen = Terminal::with_options(
                QuietBackend::new(terminal.clone()),
                TerminalOptions { viewport },
            )
            .unwrap();
            // The console's startup, a draw, the Resize arm, a real resize.
            screen.clear().unwrap();
            screen.clear().unwrap();
            screen.draw(|f| f.render_widget(Paragraph::new("first frame"), f.area())).unwrap();
            screen.clear().unwrap();
            screen.draw(|f| f.render_widget(Paragraph::new("repainted"), f.area())).unwrap();
            screen.resize(Rect::new(0, 0, 60, 20)).unwrap();
            screen.draw(|f| f.render_widget(Paragraph::new("resized"), f.area())).unwrap();

            assert_eq!(terminal.asked(), 0, "{name}: {:?}", terminal.log.borrow());
            // Everything else reached the terminal: it was cleared and
            // drawn on.
            let log = terminal.log.borrow();
            assert!(log.iter().any(|l| l.starts_with("clear")), "{name}: {log:?}");
            // (A fixed viewport that shrinks is cleared cell by cell, which
            // is one more draw.)
            assert!(log.iter().filter(|l| l.starts_with("draw")).count() >= 3, "{name}: {log:?}");
        }
    }

    /// The idle console: the same frame drawn again writes nothing at all,
    /// neither cells nor resets nor the cursor.
    #[test]
    fn an_unchanged_frame_writes_nothing() {
        for (name, viewport) in viewports() {
            let terminal = Unanswering::default();
            let mut screen = Terminal::with_options(
                QuietBackend::new(terminal.clone()),
                TerminalOptions { viewport },
            )
            .unwrap();
            let frame = |f: &mut ratatui::Frame| {
                f.render_widget(Paragraph::new("Enter a name"), f.area());
                f.set_cursor_position(Position::new(7, 3));
            };
            screen.draw(frame).unwrap();
            let after_first = terminal.log.borrow().len();
            assert!(terminal.log.borrow().iter().any(|l| l == "show"), "{name}: {:?}", terminal.log.borrow());
            assert!(terminal.log.borrow().iter().any(|l| l == "cursor 7,3"), "{name}: {:?}", terminal.log.borrow());
            for _ in 0..5 {
                screen.draw(frame).unwrap();
            }
            assert_eq!(terminal.log.borrow().len(), after_first, "{name}: {:?}", terminal.log.borrow());
            // A frame without a cursor hides it once, then stays silent.
            let no_cursor = |f: &mut ratatui::Frame| f.render_widget(Paragraph::new("Enter a name"), f.area());
            screen.draw(no_cursor).unwrap();
            screen.draw(no_cursor).unwrap();
            let log = terminal.log.borrow();
            assert_eq!(log.iter().filter(|l| l.as_str() == "hide").count(), 1, "{name}: {log:?}");
        }
    }

    /// After a clear (the attach path, a resize) the cursor's state is said
    /// again, since a fresh terminal knows nothing.
    #[test]
    fn a_clear_resends_the_cursor_state() {
        let terminal = Unanswering::default();
        let mut screen = Terminal::with_options(
            QuietBackend::new(terminal.clone()),
            TerminalOptions { viewport: Viewport::Fullscreen },
        )
        .unwrap();
        let frame = |f: &mut ratatui::Frame| {
            f.render_widget(Paragraph::new("x"), f.area());
            f.set_cursor_position(Position::new(2, 1));
        };
        screen.draw(frame).unwrap();
        screen.draw(frame).unwrap();
        screen.clear().unwrap();
        let before = terminal.log.borrow().len();
        screen.draw(frame).unwrap();
        {
            let log = terminal.log.borrow();
            let tail: Vec<&String> = log.iter().skip(before).collect();
            assert!(tail.iter().any(|l| l.as_str() == "cursor 2,1"), "{tail:?}");
        }
        // A frame without a cursor hides it once; the library itself says
        // hide only on the transition, so the wrapper has nothing to
        // repeat after a clear (the console's screens all place a cursor).
        let no_cursor = |f: &mut ratatui::Frame| f.render_widget(Paragraph::new("x"), f.area());
        screen.draw(no_cursor).unwrap();
        screen.draw(no_cursor).unwrap();
        let hides = terminal.log.borrow().iter().filter(|l| l.as_str() == "hide").count();
        assert_eq!(hides, 1, "{:?}", terminal.log.borrow());
    }

    #[test]
    fn the_answer_is_the_position_last_set() {
        let terminal = Unanswering::default();
        let mut backend = QuietBackend::new(terminal.clone());
        assert_eq!(backend.get_cursor_position().unwrap(), Position::ORIGIN);
        backend.set_cursor_position(Position::new(7, 3)).unwrap();
        assert_eq!(backend.get_cursor_position().unwrap(), Position::new(7, 3));
        // The move itself was passed on.
        assert_eq!(*terminal.log.borrow(), ["cursor 7,3"]);
    }
}
