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

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

pub struct QuietBackend<B> {
    inner: B,
    /// Where the cursor was last put through this backend.
    cursor: Position,
}

impl<B> QuietBackend<B> {
    pub fn new(inner: B) -> Self {
        Self { inner, cursor: Position::ORIGIN }
    }
}

impl<B: Backend> Backend for QuietBackend<B> {
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }

    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }

    /// From memory. NEVER forwarded: the wrapped backend would ask the
    /// terminal.
    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        let position = position.into();
        self.cursor = position;
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
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
            Ok(())
        }
        fn show_cursor(&mut self) -> Result<(), Self::Error> {
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
