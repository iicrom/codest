//! The chat composer: editable multi-line text with a char-index cursor.
//!
//! This is a much smaller descendant of `codex-rs/tui/src/bottom_pane/textarea.rs`.
//! It keeps the pieces the TUI actually needs for basic chat: insertion,
//! deletion, logical-line movement, word deletion, and cursor-to-visual-row
//! mapping. Vim mode, selections, images, and paste-burst detection are out of
//! scope for the first pass.

use crate::tui::wrap::cursor_position;
use crate::tui::wrap::wrap_chars;

#[derive(Debug, Default, Clone)]
pub struct Composer {
    chars: Vec<char>,
    cursor: usize,
}

impl Composer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.chars = text.into().chars().collect();
        self.cursor = self.chars.len();
    }

    pub fn take(&mut self) -> String {
        let text = self.text();
        self.clear();
        text
    }

    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn is_single_line(&self) -> bool {
        !self.chars.contains(&'\n')
    }

    pub fn len(&self) -> usize {
        self.chars.len()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn insert_char(&mut self, c: char) {
        if c == '\r' {
            return;
        }
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    /// Insert pasted text, normalising CRLF and expanding tabs so wrapping and
    /// cursor mapping stay consistent with typed input.
    pub fn insert_str(&mut self, text: &str) {
        let normalized = text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', "    ");
        let new_chars: Vec<char> = normalized.chars().collect();
        if new_chars.is_empty() {
            return;
        }
        let count = new_chars.len();
        self.chars.splice(self.cursor..self.cursor, new_chars);
        self.cursor += count;
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    /// Delete the word before the cursor, including leading whitespace.
    pub fn delete_word_backward(&mut self) {
        let start = self.word_start();
        if start < self.cursor {
            self.chars.drain(start..self.cursor);
            self.cursor = start;
        }
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.chars.len());
    }

    /// Move up one logical line. Returns false when already on the first line,
    /// which the app uses to trigger prompt-history navigation.
    pub fn move_up(&mut self) -> bool {
        let start = self.line_start(self.cursor);
        if start == 0 {
            return false;
        }
        let previous_end = start - 1;
        let previous_start = self.line_start(previous_end);
        let column = self.cursor - start;
        self.cursor = (previous_start + column).min(previous_end);
        true
    }

    /// Move down one logical line. Returns false when already on the last line.
    pub fn move_down(&mut self) -> bool {
        let end = self.line_end(self.cursor);
        if end >= self.chars.len() {
            return false;
        }
        let next_start = end + 1;
        let next_end = self.line_end(next_start);
        let column = self.cursor - self.line_start(self.cursor);
        self.cursor = (next_start + column).min(next_end);
        true
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start(self.cursor);
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end(self.cursor);
    }

    /// Delete from the start of the current line to the cursor (Ctrl+U).
    pub fn kill_to_start(&mut self) {
        let start = self.line_start(self.cursor);
        if start < self.cursor {
            self.chars.drain(start..self.cursor);
            self.cursor = start;
        }
    }

    /// Delete from the cursor to the end of the current line (Ctrl+K).
    pub fn kill_to_end(&mut self) {
        let end = self.line_end(self.cursor);
        if self.cursor < end {
            self.chars.drain(self.cursor..end);
        }
    }

    /// Visible rows for the given text width.
    pub fn visual_rows(&self, width: usize) -> Vec<String> {
        let width = width.max(1);
        let rows = wrap_chars(&self.chars, width);
        rows.into_iter()
            .map(|(start, end)| self.chars[start..end].iter().collect())
            .collect()
    }

    /// Cursor position in visual `(row, column)` space.
    pub fn cursor_visual(&self, width: usize) -> (usize, usize) {
        let width = width.max(1);
        let rows = wrap_chars(&self.chars, width);
        cursor_position(&self.chars, &rows, self.cursor)
    }

    fn line_start(&self, index: usize) -> usize {
        let mut start = index.min(self.chars.len());
        while start > 0 && self.chars[start - 1] != '\n' {
            start -= 1;
        }
        start
    }

    fn line_end(&self, index: usize) -> usize {
        let mut end = index.min(self.chars.len());
        while end < self.chars.len() && self.chars[end] != '\n' {
            end += 1;
        }
        end
    }

    fn word_start(&self) -> usize {
        let mut index = self.cursor;
        while index > 0 && self.chars[index - 1].is_whitespace() {
            index -= 1;
        }
        while index > 0 && !self.chars[index - 1].is_whitespace() {
            index -= 1;
        }
        index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_and_deletes() {
        let mut composer = Composer::new();
        composer.insert_str("hello");
        assert_eq!(composer.text(), "hello");
        composer.backspace();
        assert_eq!(composer.text(), "hell");
        composer.move_left();
        composer.delete();
        assert_eq!(composer.text(), "hel");
    }

    #[test]
    fn normalises_pasted_text() {
        let mut composer = Composer::new();
        composer.insert_str("a\r\nb\tc");
        assert_eq!(composer.text(), "a\nb    c");
    }

    #[test]
    fn moves_between_logical_lines() {
        let mut composer = Composer::new();
        composer.set_text("abc\ndef");
        composer.cursor = 1;
        assert!(composer.move_down());
        assert_eq!(composer.cursor, 5); // "d" + column 1
        assert!(!composer.move_down());
        assert!(composer.move_up());
        assert_eq!(composer.cursor, 1);
        assert!(!composer.move_up());
    }

    #[test]
    fn home_and_end_are_line_scoped() {
        let mut composer = Composer::new();
        composer.set_text("abc\ndef");
        composer.cursor = 5;
        composer.home();
        assert_eq!(composer.cursor, 4);
        composer.end();
        assert_eq!(composer.cursor, 7);
    }

    #[test]
    fn kills_to_line_bounds() {
        let mut composer = Composer::new();
        composer.set_text("abc def");
        composer.cursor = 4;
        composer.kill_to_start();
        assert_eq!(composer.text(), "def");

        composer.cursor = 1;
        composer.kill_to_end();
        assert_eq!(composer.text(), "d");
    }

    #[test]
    fn deletes_previous_word() {
        let mut composer = Composer::new();
        composer.set_text("hello big world");
        composer.delete_word_backward();
        assert_eq!(composer.text(), "hello big ");
    }

    #[test]
    fn tracks_cursor_in_wrapped_rows() {
        let mut composer = Composer::new();
        composer.set_text("aaaa bbbb");
        assert_eq!(composer.visual_rows(4), vec!["aaaa", "bbbb"]);
        assert_eq!(composer.cursor_visual(4), (1, 4));
        composer.cursor = 3;
        assert_eq!(composer.cursor_visual(4), (0, 3));
    }

    #[test]
    fn handles_cjk_width() {
        let mut composer = Composer::new();
        composer.set_text("你好世界");
        assert_eq!(composer.visual_rows(4), vec!["你好", "世界"]);
        assert_eq!(composer.cursor_visual(4), (1, 4));
    }
}
