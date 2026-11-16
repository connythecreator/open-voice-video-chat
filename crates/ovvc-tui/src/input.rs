//! A small single-line text input with a visible cursor.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::theme;

#[derive(Default)]
pub struct Input {
    text: String,
    cursor: usize,
}

impl Input {
    pub fn with_text(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.chars().count();
        Self { text, cursor }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    pub fn insert(&mut self, c: char) {
        let byte = self.byte_at(self.cursor);
        self.text.insert(byte, c);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.byte_at(self.cursor - 1);
        let end = self.byte_at(self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.text.chars().count() {
            return;
        }
        let start = self.byte_at(self.cursor);
        let end = self.byte_at(self.cursor + 1);
        self.text.replace_range(start..end, "");
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        let len = self.text.chars().count();
        self.cursor = (self.cursor + 1).min(len);
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.text.chars().count();
    }

    fn byte_at(&self, char_index: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_index)
            .map(|(byte, _)| byte)
            .unwrap_or(self.text.len())
    }

    /// Render the text with a cursor cell at the insertion point.
    pub fn line(&self) -> Line<'static> {
        let palette = theme::palette();
        let chars: Vec<char> = self.text.chars().collect();
        let cursor = self.cursor.min(chars.len());
        let before: String = chars[..cursor].iter().collect();
        let mut spans = vec![Span::raw(before)];

        match chars.get(cursor) {
            Some(c) => {
                spans.push(Span::styled(
                    c.to_string(),
                    Style::new().bg(palette.select_bg).fg(palette.select_fg),
                ));
                let after: String = chars[cursor + 1..].iter().collect();
                spans.push(Span::raw(after));
            }
            None => spans.push(Span::styled("▏", Style::new().fg(palette.accent))),
        }

        Line::from(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::Input;

    fn plain_line(input: &Input) -> String {
        input.line().spans.iter().map(|span| span.content.as_ref()).collect()
    }

    #[test]
    fn with_text_starts_at_the_end_and_clear_resets() {
        let mut input = Input::with_text("hello");
        assert_eq!(input.text(), "hello");
        assert!(!input.is_empty());
        input.right();
        input.insert('!');
        assert_eq!(input.text(), "hello!");
        input.home();
        input.left();
        input.insert('>');
        assert_eq!(input.text(), ">hello!");
        input.clear();
        assert!(input.is_empty());
        assert_eq!(input.take(), "");
    }

    #[test]
    fn editing_at_the_boundaries_is_a_no_op() {
        let mut input = Input::with_text("ab");
        input.end();
        input.delete();
        input.right();
        assert_eq!(input.text(), "ab");
        input.home();
        input.backspace();
        input.left();
        assert_eq!(input.text(), "ab");
    }

    #[test]
    fn the_rendered_line_marks_the_cursor_cell() {
        let mut input = Input::with_text("hé!");
        assert_eq!(plain_line(&input), "hé!▏");
        input.left();
        input.left();
        assert_eq!(plain_line(&input), "hé!");
        input.end();
        assert_eq!(plain_line(&input), "hé!▏");
    }

    #[test]
    fn edits_unicode_by_character() {
        let mut input = Input::default();
        input.insert('h');
        input.insert('é');
        input.insert('!');
        assert_eq!(input.text(), "hé!");

        input.left();
        input.backspace();
        assert_eq!(input.text(), "h!");
        input.home();
        input.delete();
        assert_eq!(input.text(), "!");
        input.end();
        input.insert('?');
        assert_eq!(input.text(), "!?");
        assert_eq!(input.take(), "!?");
        assert!(input.is_empty());
    }
}
