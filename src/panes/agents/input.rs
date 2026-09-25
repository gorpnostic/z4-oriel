//! A small text field (single- or multi-line) for the new-task form, comments and the repo path.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Debug, Default)]
pub struct Input {
    pub text: String,
    /// Cursor, in chars.
    pub cur: usize,
    pub multi: bool,
}

impl Input {
    pub fn new(text: &str, multi: bool) -> Input {
        Input { text: text.to_string(), cur: text.chars().count(), multi }
    }

    fn byte(&self, ch: usize) -> usize {
        self.text.char_indices().nth(ch).map(|(b, _)| b).unwrap_or(self.text.len())
    }

    pub fn insert(&mut self, s: &str) {
        let s: String = if self.multi { s.replace("\r\n", "\n").replace('\r', "\n") } else { s.replace(['\r', '\n'], " ") };
        let s: String = s.chars().filter(|c| *c == '\n' || !c.is_control()).collect::<String>().replace('\t', "    ");
        let b = self.byte(self.cur);
        self.text.insert_str(b, &s);
        self.cur += s.chars().count();
    }

    /// (line index, column) of the cursor, and the lines.
    fn pos(&self) -> (usize, usize) {
        let before: String = self.text.chars().take(self.cur).collect();
        let line = before.matches('\n').count();
        let col = before.rsplit('\n').next().unwrap_or("").chars().count();
        (line, col)
    }

    fn line_start(&self, line: usize) -> usize {
        let mut n = 0;
        for (i, l) in self.text.split('\n').enumerate() {
            if i == line {
                return n;
            }
            n += l.chars().count() + 1;
        }
        n
    }

    /// Handle an editing key. Returns false for keys it doesn't use (the form handles tab, esc, enter...).
    pub fn key(&mut self, k: KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let len = self.text.chars().count();
        match k.code {
            KeyCode::Char('u') if ctrl => {
                self.text.clear();
                self.cur = 0;
            }
            KeyCode::Char('w') if ctrl => {
                // delete the word before the cursor
                let chars: Vec<char> = self.text.chars().collect();
                let mut i = self.cur;
                while i > 0 && chars[i - 1] == ' ' {
                    i -= 1;
                }
                while i > 0 && chars[i - 1] != ' ' && chars[i - 1] != '\n' {
                    i -= 1;
                }
                let (a, b) = (self.byte(i), self.byte(self.cur));
                self.text.replace_range(a..b, "");
                self.cur = i;
            }
            KeyCode::Char(c) if !ctrl => self.insert(&c.to_string()),
            KeyCode::Enter if self.multi => self.insert("\n"),
            KeyCode::Backspace if self.cur > 0 => {
                let (a, b) = (self.byte(self.cur - 1), self.byte(self.cur));
                self.text.replace_range(a..b, "");
                self.cur -= 1;
            }
            KeyCode::Backspace => {}
            KeyCode::Delete if self.cur < len => {
                let (a, b) = (self.byte(self.cur), self.byte(self.cur + 1));
                self.text.replace_range(a..b, "");
            }
            KeyCode::Delete => {}
            KeyCode::Left => self.cur = self.cur.saturating_sub(1),
            KeyCode::Right => self.cur = (self.cur + 1).min(len),
            KeyCode::Home => {
                let (l, _) = self.pos();
                self.cur = self.line_start(l);
            }
            KeyCode::End => {
                let (l, _) = self.pos();
                self.cur = self.line_start(l) + self.text.split('\n').nth(l).map(|s| s.chars().count()).unwrap_or(0);
            }
            KeyCode::Up | KeyCode::Down if self.multi => {
                let (l, c) = self.pos();
                let lines: Vec<&str> = self.text.split('\n').collect();
                let to = if k.code == KeyCode::Up { l.checked_sub(1) } else { (l + 1 < lines.len()).then_some(l + 1) };
                match to {
                    Some(t) => self.cur = self.line_start(t) + c.min(lines[t].chars().count()),
                    None => return false, // past the first/last line: let the form move between fields
                }
            }
            _ => return false,
        }
        true
    }

    /// Wrap to `w` columns: rows of text, plus the cursor's (row, col) in them.
    pub fn wrapped(&self, w: usize) -> (Vec<String>, (usize, usize)) {
        let w = w.max(2);
        let mut rows = vec![];
        let mut cursor = (0, 0);
        let mut idx = 0; // char index into text
        for line in self.text.split('\n') {
            let mut row = String::new();
            let mut used = 0;
            for ch in line.chars() {
                let cw = ch.width().unwrap_or(0);
                if used + cw > w {
                    rows.push(std::mem::take(&mut row));
                    used = 0;
                }
                if idx == self.cur {
                    cursor = (rows.len(), used);
                }
                row.push(ch);
                used += cw;
                idx += 1;
            }
            if idx == self.cur {
                cursor = if used >= w { (rows.len() + 1, 0) } else { (rows.len(), used) };
            }
            rows.push(row);
            idx += 1; // the newline
        }
        (rows, cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn agents_input_edit_and_wrap() {
        let mut i = Input::new("", true);
        i.insert("hello");
        i.key(k(KeyCode::Enter));
        i.insert("world");
        assert_eq!(i.text, "hello\nworld");
        i.key(k(KeyCode::Up));
        assert_eq!(i.cur, 5);
        i.key(k(KeyCode::Backspace));
        assert_eq!(i.text, "hell\nworld");
        let (rows, cur) = Input::new("abcdefgh", false).wrapped(3);
        assert_eq!(rows, vec!["abc", "def", "gh"]);
        assert_eq!(cur, (2, 2));
        let mut s = Input::new("a b", false);
        s.insert("\nc");
        assert_eq!(s.text, "a b c");
    }
}
