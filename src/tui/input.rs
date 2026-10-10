//! A single-line text input.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Default)]
pub struct TextInput {
    text: String,
    /// Cursor position as a byte offset (always on a char boundary).
    cursor: usize,
}

impl TextInput {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        Self { cursor: text.len(), text }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Text before the cursor, for placing the terminal cursor.
    pub fn before_cursor(&self) -> &str {
        &self.text[..self.cursor]
    }

    /// Apply an editing key. Returns whether the text changed.
    pub fn handle(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('u') if ctrl => {
                self.text.drain(..self.cursor);
                self.cursor = 0;
                true
            }
            KeyCode::Char('w') if ctrl => {
                let before = &self.text[..self.cursor];
                let trimmed = before.trim_end();
                let start = trimmed.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
                self.text.drain(start..self.cursor);
                self.cursor = start;
                true
            }
            KeyCode::Char('a') if ctrl => {
                self.cursor = 0;
                false
            }
            KeyCode::Char('e') if ctrl => {
                self.cursor = self.text.len();
                false
            }
            KeyCode::Char(c) if !ctrl => {
                self.text.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                true
            }
            KeyCode::Backspace => match self.prev_boundary() {
                Some(p) => {
                    self.text.drain(p..self.cursor);
                    self.cursor = p;
                    true
                }
                None => false,
            },
            KeyCode::Delete => match self.next_boundary() {
                Some(n) => {
                    self.text.drain(self.cursor..n);
                    true
                }
                None => false,
            },
            KeyCode::Left => {
                if let Some(p) = self.prev_boundary() {
                    self.cursor = p;
                }
                false
            }
            KeyCode::Right => {
                if let Some(n) = self.next_boundary() {
                    self.cursor = n;
                }
                false
            }
            KeyCode::Home => {
                self.cursor = 0;
                false
            }
            KeyCode::End => {
                self.cursor = self.text.len();
                false
            }
            _ => false,
        }
    }

    fn prev_boundary(&self) -> Option<usize> {
        self.text[..self.cursor].char_indices().next_back().map(|(i, _)| i)
    }

    fn next_boundary(&self) -> Option<usize> {
        self.text[self.cursor..].chars().next().map(|c| self.cursor + c.len_utf8())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn edits_unicode_text() {
        let mut t = TextInput::new("星");
        t.handle(key(KeyCode::Char('x')));
        assert_eq!(t.text(), "星x");
        t.handle(key(KeyCode::Left));
        t.handle(key(KeyCode::Left));
        t.handle(key(KeyCode::Char('a')));
        assert_eq!(t.text(), "a星x");
        t.handle(key(KeyCode::Delete));
        assert_eq!(t.text(), "ax");
        t.handle(key(KeyCode::Backspace));
        assert_eq!(t.text(), "x");
        assert_eq!(t.before_cursor(), "");
        assert!(!t.handle(key(KeyCode::Backspace)));
    }

    #[test]
    fn ctrl_shortcuts() {
        let mut t = TextInput::new("order of the sinking");
        t.handle(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(t.text(), "order of the ");
        t.handle(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(t.text(), "");
    }
}
