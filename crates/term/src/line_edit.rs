//! Single line text editor with emacs-style shortcuts.

use crate::{Key, char_width};

#[derive(Default)]
pub struct LineEdit {
    chars: Vec<char>,
    cursor: usize,
}

impl LineEdit {
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn set(&mut self, text: &str) {
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
    }

    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }

    fn insert(&mut self, text: &str) {
        for c in text.chars() {
            let c = if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c };
            if c.is_control() {
                continue;
            }
            self.chars.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    fn word_left(&self) -> usize {
        let mut i = self.cursor;
        while i > 0 && self.chars[i - 1] == ' ' {
            i -= 1;
        }
        while i > 0 && self.chars[i - 1] != ' ' {
            i -= 1;
        }
        i
    }

    fn word_right(&self) -> usize {
        let mut i = self.cursor;
        while i < self.chars.len() && self.chars[i] == ' ' {
            i += 1;
        }
        while i < self.chars.len() && self.chars[i] != ' ' {
            i += 1;
        }
        i
    }

    /// Applies an editing key. Returns true if the text changed.
    pub fn handle(&mut self, key: &Key) -> bool {
        let before = self.chars.len();
        match key {
            Key::Char(c) => self.insert(&c.to_string()),
            Key::Paste(text) => self.insert(text),
            Key::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.chars.remove(self.cursor);
            }
            Key::Delete | Key::Ctrl('d') if self.cursor < self.chars.len() => {
                self.chars.remove(self.cursor);
            }
            Key::Left | Key::Ctrl('b') => self.cursor = self.cursor.saturating_sub(1),
            Key::Right | Key::Ctrl('f') => self.cursor = (self.cursor + 1).min(self.chars.len()),
            Key::Home | Key::Ctrl('a') => self.cursor = 0,
            Key::End | Key::Ctrl('e') => self.cursor = self.chars.len(),
            Key::Alt('b') => self.cursor = self.word_left(),
            Key::Alt('f') => self.cursor = self.word_right(),
            Key::Ctrl('w') | Key::Alt('\x7f') => {
                let start = self.word_left();
                self.chars.drain(start..self.cursor);
                self.cursor = start;
            }
            Key::Ctrl('u') => {
                self.chars.drain(..self.cursor);
                self.cursor = 0;
            }
            Key::Ctrl('k') => self.chars.truncate(self.cursor),
            _ => {}
        }
        self.chars.len() != before
    }

    /// Returns the visible slice for a field of `width` columns and the
    /// cursor column inside it.
    pub fn view(&self, width: usize) -> (String, usize) {
        let width = width.max(1);
        let col = |range: &[char]| range.iter().map(|&c| char_width(c)).sum::<usize>();
        let mut start = 0;
        while col(&self.chars[start..self.cursor]) >= width {
            start += 1;
        }
        let mut shown = String::new();
        let mut used = 0;
        for &c in &self.chars[start..] {
            let w = char_width(c);
            if used + w > width {
                break;
            }
            shown.push(c);
            used += w;
        }
        (shown, col(&self.chars[start..self.cursor]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing() {
        let mut e = LineEdit::default();
        for c in "hello world".chars() {
            e.handle(&Key::Char(c));
        }
        e.handle(&Key::Ctrl('w'));
        assert_eq!(e.text(), "hello ");
        e.handle(&Key::Home);
        e.handle(&Key::Delete);
        assert_eq!(e.text(), "ello ");
        e.handle(&Key::Paste("a\nb".into()));
        assert_eq!(e.text(), "a bello ");
    }

    #[test]
    fn view_scrolls() {
        let mut e = LineEdit::default();
        e.set("abcdefghij");
        let (shown, cursor) = e.view(5);
        assert_eq!(shown, "ghij");
        assert_eq!(cursor, 4);
    }
}
