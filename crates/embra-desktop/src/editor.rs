//! Structured-input editor overlay state.
//!
//! A GUI-native replacement for the TUI's `/ml` multi-line toggle: a
//! large `text_editor` overlay with syntect syntax highlighting for
//! JSON / Markdown. `text_editor::Content` is stateful, so this lives in
//! app state rather than being a pure function of a `String` (unlike the
//! single-line input box and the menu-prompt modal).

use iced::widget::text_editor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorSyntax {
    Json,
    Markdown,
    Plain,
}

impl EditorSyntax {
    /// syntect token (file-extension-like) passed to `TextEditor::highlight`.
    pub fn token(self) -> &'static str {
        match self {
            EditorSyntax::Json => "json",
            EditorSyntax::Markdown => "md",
            EditorSyntax::Plain => "txt",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            EditorSyntax::Json => "JSON",
            EditorSyntax::Markdown => "Markdown",
            EditorSyntax::Plain => "Plain",
        }
    }

    pub const ALL: &'static [EditorSyntax] =
        &[EditorSyntax::Json, EditorSyntax::Markdown, EditorSyntax::Plain];
}

pub struct EditorState {
    pub content: text_editor::Content,
    pub syntax: EditorSyntax,
}

impl EditorState {
    pub fn new() -> Self {
        Self {
            content: text_editor::Content::new(),
            syntax: EditorSyntax::Json,
        }
    }
}

impl Default for EditorState {
    fn default() -> Self {
        Self::new()
    }
}
