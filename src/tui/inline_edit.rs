//! A one-line inline editor for the list screens.
//!
//! `/settings` opens one for a text or number row; any screen that needs to
//! collect a short string in place can use it. The editor wraps a
//! [`Composer`] (so movement and kills are the same ones the chat input has)
//! and owns the keyboard while it is open: printable keys edit, `Enter`
//! confirms, `Esc` and `Ctrl+C` drop the edit without touching anything.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::tui::composer::Composer;

/// The glyph drawn after the text as a caret.
pub const CARET: &str = "▌";

/// What `Enter` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The box was confirmed; the text is what the user typed (untrimmed —
    /// the caller decides whether whitespace matters).
    Confirmed(String),
    /// `Esc`/`Ctrl+C`: the edit is dropped and the caller keeps its old value.
    Cancelled,
}

/// A one-line text box, seeded with the current value.
pub struct LineEditor {
    composer: Composer,
}

impl LineEditor {
    /// An empty box.
    pub fn empty() -> Self {
        Self {
            composer: Composer::new(),
        }
    }

    /// A box seeded with `text`, cursor at the end. Seeding with the stored
    /// value matters: typing over a friendly default would otherwise pin it.
    pub fn with_text(text: impl Into<String>) -> Self {
        let mut composer = Composer::new();
        composer.set_text(text);
        Self { composer }
    }

    /// The text as it currently stands, for rendering.
    pub fn text(&self) -> String {
        self.composer.text()
    }

    /// The text with the caret glyph appended, ready for one row.
    pub fn display(&self) -> String {
        format!("{}{CARET}", self.composer.text())
    }

    /// Insert pasted text. The box is one line, so newlines and tabs become
    /// spaces: a stored value carrying `\n` would be unrepresentable in a row
    /// and unreadable in a TOML string without escapes.
    pub fn insert_paste(&mut self, text: &str) {
        let flattened: String = text
            .chars()
            .map(|c| {
                if c == '\n' || c == '\r' || c == '\t' {
                    ' '
                } else {
                    c
                }
            })
            .collect();
        self.composer.insert_str(&flattened);
    }

    /// Feed one key. `None` means the edit continues; `Some(outcome)` means
    /// it ended and the caller should apply it.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Outcome> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter if !ctrl && !alt => Some(Outcome::Confirmed(self.composer.text())),
            KeyCode::Esc => Some(Outcome::Cancelled),
            KeyCode::Char('c') if ctrl => Some(Outcome::Cancelled),
            KeyCode::Char(c) if !ctrl && !alt => {
                self.composer.insert_char(c);
                None
            }
            KeyCode::Backspace => {
                self.composer.backspace();
                None
            }
            KeyCode::Delete => {
                self.composer.delete();
                None
            }
            KeyCode::Left => {
                self.composer.move_left();
                None
            }
            KeyCode::Right => {
                self.composer.move_right();
                None
            }
            KeyCode::Home => {
                self.composer.home();
                None
            }
            KeyCode::End => {
                self.composer.end();
                None
            }
            KeyCode::Char('a') if ctrl => {
                self.composer.home();
                None
            }
            KeyCode::Char('e') if ctrl => {
                self.composer.end();
                None
            }
            KeyCode::Char('u') if ctrl => {
                self.composer.kill_to_start();
                None
            }
            KeyCode::Char('k') if ctrl => {
                self.composer.kill_to_end();
                None
            }
            KeyCode::Char('w') if ctrl => {
                self.composer.delete_word_backward();
                None
            }
            // Anything else (arrows that would leave the row, function keys,
            // Alt chords) is swallowed rather than leaking to the screen.
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn typing_edits_and_enter_confirms() {
        let mut editor = LineEditor::empty();
        for c in "250".chars() {
            assert_eq!(editor.handle_key(key(KeyCode::Char(c))), None);
        }
        assert_eq!(editor.display(), "250▌");
        assert_eq!(
            editor.handle_key(key(KeyCode::Enter)),
            Some(Outcome::Confirmed("250".to_string()))
        );
    }

    #[test]
    fn escape_and_ctrl_c_drop_the_edit() {
        let mut editor = LineEditor::with_text("stored");
        editor.handle_key(key(KeyCode::Char('x')));
        assert_eq!(
            editor.handle_key(key(KeyCode::Esc)),
            Some(Outcome::Cancelled)
        );

        let mut editor = LineEditor::with_text("stored");
        assert_eq!(
            editor.handle_key(ctrl(KeyCode::Char('c'))),
            Some(Outcome::Cancelled)
        );
    }

    #[test]
    fn the_editor_starts_at_the_end_of_the_seeded_text() {
        let mut editor = LineEditor::with_text("abc");
        editor.handle_key(key(KeyCode::Char('d')));
        assert_eq!(editor.text(), "abcd");
        editor.handle_key(key(KeyCode::Backspace));
        assert_eq!(editor.text(), "abc");
    }

    #[test]
    fn the_composer_kills_are_available() {
        let mut editor = LineEditor::with_text("hello world");
        editor.handle_key(ctrl(KeyCode::Char('w')));
        assert_eq!(editor.text(), "hello ");
        editor.handle_key(ctrl(KeyCode::Char('u')));
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn a_stray_key_does_not_end_the_edit() {
        let mut editor = LineEditor::with_text("abc");
        assert_eq!(editor.handle_key(key(KeyCode::Up)), None);
        assert_eq!(editor.handle_key(key(KeyCode::Tab)), None);
        assert_eq!(editor.text(), "abc");
    }
}
