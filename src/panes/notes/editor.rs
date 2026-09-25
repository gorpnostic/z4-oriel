//! A small soft-wrapping text editor: a Vec of lines, a cursor (line, char index), word-wrapped layout for
//! drawing and for mapping clicks / up / down through wrapped rows, and snapshot undo.

use std::time::Instant;
use unicode_width::UnicodeWidthChar;

/// One visual row: chars [start, end) of logical line `row`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seg {
    pub row: usize,
    pub start: usize,
    pub end: usize,
    /// Last visual row of its logical line.
    pub last: bool,
}

#[derive(Clone)]
struct Snap {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Insert,
    Delete,
    Other,
}

pub struct Editor {
    pub lines: Vec<String>,
    pub row: usize,
    /// Char index into lines[row].
    pub col: usize,
    /// Column the cursor wants to be in when moving up/down through shorter rows.
    goal_x: Option<usize>,
    /// First visual row shown.
    pub scroll: usize,
    undo: Vec<Snap>,
    redo: Vec<Snap>,
    last_edit: Option<(EditKind, Instant)>,
}

fn cw(c: char) -> usize {
    c.width().unwrap_or(0)
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

fn byte_at(s: &str, col: usize) -> usize {
    s.char_indices().nth(col).map(|(b, _)| b).unwrap_or(s.len())
}

/// Word-wrap one line into char ranges no wider than `w` columns (a long word is cut hard; a space that
/// overflows hangs off the end so the next row doesn't start with it).
pub fn wrap_line(line: &str, w: usize) -> Vec<(usize, usize)> {
    let w = w.max(1);
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut segs = vec![];
    let mut start = 0;
    while start < n {
        let (mut col, mut i, mut brk) = (0, start, None);
        while i < n {
            let c = cw(chars[i]);
            if col + c > w && i > start {
                break;
            }
            if chars[i] == ' ' {
                brk = Some(i + 1);
            }
            col += c;
            i += 1;
        }
        if i >= n {
            segs.push((start, n));
            break;
        }
        let end = if chars[i] == ' ' {
            i + 1
        } else {
            match brk {
                Some(b) if b > start => b,
                _ => i,
            }
        };
        segs.push((start, end));
        start = end;
    }
    if segs.is_empty() {
        segs.push((0, 0));
    }
    segs
}

impl Editor {
    pub fn new(text: &str) -> Editor {
        let mut e = Editor { lines: vec![], row: 0, col: 0, goal_x: None, scroll: 0, undo: vec![], redo: vec![], last_edit: None };
        e.set_text(text);
        e
    }

    pub fn set_text(&mut self, text: &str) {
        let t = text.replace("\r\n", "\n").replace('\r', "\n");
        self.lines = t.split('\n').map(|l| l.replace('\t', "    ")).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.undo.clear();
        self.redo.clear();
        self.last_edit = None;
        self.scroll = 0;
        self.doc_end();
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn line_len(&self) -> usize {
        char_len(&self.lines[self.row])
    }

    // ------------------------------------------------------------------ layout

    pub fn layout(&self, w: usize) -> Vec<Seg> {
        let mut out = Vec::with_capacity(self.lines.len());
        for (row, l) in self.lines.iter().enumerate() {
            let segs = wrap_line(l, w);
            let k = segs.len();
            for (i, (start, end)) in segs.into_iter().enumerate() {
                out.push(Seg { row, start, end, last: i + 1 == k });
            }
        }
        out
    }

    /// (visual row, x column) of the cursor.
    pub fn cursor_visual(&self, segs: &[Seg]) -> (usize, usize) {
        for (v, s) in segs.iter().enumerate() {
            if s.row == self.row && self.col >= s.start && (self.col < s.end || s.last) {
                let x = self.lines[s.row].chars().skip(s.start).take(self.col - s.start).map(cw).sum();
                return (v, x);
            }
        }
        (0, 0)
    }

    /// The char index in visual row `seg` closest to column `x`.
    fn col_at(&self, seg: &Seg, x: usize) -> usize {
        let mut used = 0;
        let mut col = seg.start;
        for c in self.lines[seg.row].chars().skip(seg.start).take(seg.end - seg.start) {
            let w = cw(c);
            if used + w > x {
                break;
            }
            used += w;
            col += 1;
        }
        if !seg.last && col >= seg.end {
            col = seg.end.saturating_sub(1).max(seg.start);
        }
        col
    }

    /// Keep the cursor's row within [scroll, scroll + h).
    pub fn follow(&mut self, segs: &[Seg], h: usize) {
        let (v, _) = self.cursor_visual(segs);
        if v < self.scroll {
            self.scroll = v;
        } else if h > 0 && v >= self.scroll + h {
            self.scroll = v + 1 - h;
        }
    }

    pub fn clamp_scroll(&mut self, segs: &[Seg], h: usize) {
        self.scroll = self.scroll.min(segs.len().saturating_sub(h.max(1)));
    }

    // ------------------------------------------------------------------ undo

    fn snapshot(&mut self, kind: EditKind) {
        let fresh = match self.last_edit {
            Some((k, at)) => k != kind || kind == EditKind::Other || at.elapsed().as_millis() > 1000,
            None => true,
        };
        if fresh {
            self.undo.push(Snap { lines: self.lines.clone(), row: self.row, col: self.col });
            if self.undo.len() > 200 {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.last_edit = Some((kind, Instant::now()));
        self.goal_x = None;
    }

    pub fn undo(&mut self) -> bool {
        let Some(s) = self.undo.pop() else { return false };
        self.redo.push(Snap { lines: std::mem::replace(&mut self.lines, s.lines), row: self.row, col: self.col });
        (self.row, self.col) = (s.row, s.col);
        self.last_edit = None;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(s) = self.redo.pop() else { return false };
        self.undo.push(Snap { lines: std::mem::replace(&mut self.lines, s.lines), row: self.row, col: self.col });
        (self.row, self.col) = (s.row, s.col);
        self.last_edit = None;
        true
    }

    // ------------------------------------------------------------------ editing

    pub fn insert_char(&mut self, c: char) {
        self.snapshot(if c == ' ' { EditKind::Other } else { EditKind::Insert });
        let b = byte_at(&self.lines[self.row], self.col);
        self.lines[self.row].insert(b, c);
        self.col += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        self.snapshot(EditKind::Other);
        let s = s.replace("\r\n", "\n").replace('\r', "\n").replace('\t', "    ");
        let s: String = s.chars().filter(|c| *c == '\n' || !c.is_control()).collect();
        let b = byte_at(&self.lines[self.row], self.col);
        let tail = self.lines[self.row].split_off(b);
        let mut parts = s.split('\n');
        if let Some(first) = parts.next() {
            self.lines[self.row].push_str(first);
        }
        for p in parts {
            self.row += 1;
            self.lines.insert(self.row, p.to_string());
        }
        self.col = self.line_len();
        self.lines[self.row].push_str(&tail);
    }

    /// Enter. Continues a "- " / "* " / "1. " list; Enter on an empty bullet ends the list.
    pub fn newline(&mut self) {
        self.snapshot(EditKind::Other);
        let line = self.lines[self.row].clone();
        let indent: String = line.chars().take_while(|c| *c == ' ').collect();
        let rest = &line[indent.len()..];
        // (marker as written, marker for the next item)
        let mut bullet: Option<(String, String)> = None;
        for b in ["- [ ] ", "- [x] ", "- ", "* ", "+ "] {
            if rest.starts_with(b) {
                let next = if b.starts_with("- [") { "- [ ] ".to_string() } else { b.to_string() };
                bullet = Some((b.to_string(), next));
                break;
            }
        }
        if bullet.is_none() {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() && rest[digits.len()..].starts_with(". ") {
                let n: u64 = digits.parse().unwrap_or(0);
                bullet = Some((format!("{digits}. "), format!("{}. ", n + 1)));
            }
        }
        let at_end = self.col >= char_len(&line);
        if let Some((mark, _)) = &bullet {
            if at_end && rest.trim_end() == mark.trim_end() {
                // Enter on an empty item ends the list instead of adding another
                self.lines[self.row] = indent;
                self.col = self.line_len();
                return;
            }
        }
        let bullet = bullet.map(|b| b.1);
        let b = byte_at(&self.lines[self.row], self.col);
        let tail = self.lines[self.row].split_off(b);
        let prefix = match bullet {
            Some(bl) if at_end => format!("{indent}{bl}"),
            _ => indent,
        };
        self.row += 1;
        self.col = char_len(&prefix);
        self.lines.insert(self.row, format!("{prefix}{tail}"));
    }

    pub fn backspace(&mut self) {
        if self.col == 0 && self.row == 0 {
            return;
        }
        self.snapshot(EditKind::Delete);
        if self.col > 0 {
            let l = &mut self.lines[self.row];
            let b = byte_at(l, self.col - 1);
            l.remove(b);
            self.col -= 1;
        } else {
            let cur = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.line_len();
            self.lines[self.row].push_str(&cur);
        }
    }

    pub fn delete(&mut self) {
        if self.col >= self.line_len() && self.row + 1 >= self.lines.len() {
            return;
        }
        self.snapshot(EditKind::Delete);
        if self.col < self.line_len() {
            let l = &mut self.lines[self.row];
            let b = byte_at(l, self.col);
            l.remove(b);
        } else {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    pub fn delete_word_left(&mut self) {
        if self.col == 0 {
            return self.backspace();
        }
        let from = self.col;
        self.word_left();
        let to = self.col;
        self.snapshot(EditKind::Other);
        let l = &mut self.lines[self.row];
        let (a, b) = (byte_at(l, to), byte_at(l, from));
        l.replace_range(a..b, "");
    }

    // ------------------------------------------------------------------ movement

    pub fn left(&mut self) {
        self.goal_x = None;
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.line_len();
        }
    }

    pub fn right(&mut self) {
        self.goal_x = None;
        if self.col < self.line_len() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    fn is_word(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }

    pub fn word_left(&mut self) {
        self.goal_x = None;
        if self.col == 0 {
            return self.left();
        }
        let chars: Vec<char> = self.lines[self.row].chars().collect();
        let mut i = self.col;
        while i > 0 && !Self::is_word(chars[i - 1]) {
            i -= 1;
        }
        while i > 0 && Self::is_word(chars[i - 1]) {
            i -= 1;
        }
        self.col = i;
    }

    pub fn word_right(&mut self) {
        self.goal_x = None;
        let chars: Vec<char> = self.lines[self.row].chars().collect();
        if self.col >= chars.len() {
            return self.right();
        }
        let mut i = self.col;
        while i < chars.len() && !Self::is_word(chars[i]) {
            i += 1;
        }
        while i < chars.len() && Self::is_word(chars[i]) {
            i += 1;
        }
        self.col = i;
    }

    /// Home: start of the visual row; again goes to the start of the logical line.
    pub fn home(&mut self, segs: &[Seg]) {
        self.goal_x = None;
        let (v, _) = self.cursor_visual(segs);
        let s = segs.get(v).copied();
        self.col = match s {
            Some(s) if self.col != s.start => s.start,
            _ => 0,
        };
    }

    pub fn end(&mut self, segs: &[Seg]) {
        self.goal_x = None;
        let (v, _) = self.cursor_visual(segs);
        self.col = match segs.get(v) {
            Some(s) if !s.last && self.col + 1 < s.end => s.end - 1,
            _ => self.line_len(),
        };
    }

    pub fn doc_start(&mut self) {
        (self.row, self.col, self.goal_x) = (0, 0, None);
    }

    pub fn doc_end(&mut self) {
        self.row = self.lines.len() - 1;
        self.col = self.line_len();
        self.goal_x = None;
    }

    /// Move `dy` visual rows (negative = up), keeping the goal column.
    pub fn vmove(&mut self, segs: &[Seg], dy: isize) {
        let (v, x) = self.cursor_visual(segs);
        let goal = *self.goal_x.get_or_insert(x);
        let target = v as isize + dy;
        if target < 0 {
            self.doc_start();
            return;
        }
        if target as usize >= segs.len() {
            self.doc_end();
            return;
        }
        let s = segs[target as usize];
        self.row = s.row;
        self.col = self.col_at(&s, goal);
        self.goal_x = Some(goal);
    }

    /// Put the cursor at visual row `v`, column `x` (a mouse click).
    pub fn click(&mut self, segs: &[Seg], v: usize, x: usize) {
        self.goal_x = None;
        match segs.get(v) {
            Some(s) => {
                self.row = s.row;
                self.col = self.col_at(s, x);
            }
            None => self.doc_end(),
        }
        self.last_edit = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_editor_wrap_and_move() {
        assert_eq!(wrap_line("hello world foo", 11), vec![(0, 12), (12, 15)]);
        assert_eq!(wrap_line("abcdefghij", 4), vec![(0, 4), (4, 8), (8, 10)]);
        assert_eq!(wrap_line("", 10), vec![(0, 0)]);
        let mut e = Editor::new("one two three four\nsecond");
        let segs = e.layout(9);
        assert_eq!(segs.len(), 4); // "one two ", "three ", "four", "second"
        e.doc_start();
        e.vmove(&segs, 1);
        assert_eq!((e.row, e.col), (0, 8));
        e.vmove(&segs, 2);
        assert_eq!((e.row, e.col), (1, 0));
        e.word_left(); // at a line start: back to the end of the previous line
        assert_eq!((e.row, e.col), (0, 18));
        e.doc_start();
        e.word_right();
        assert_eq!(e.col, 3);
        e.word_right();
        assert_eq!(e.col, 7);
        e.click(&segs, 1, 2);
        assert_eq!((e.row, e.col), (0, 10));
    }

    #[test]
    fn notes_editor_edit_and_undo() {
        let mut e = Editor::new("");
        for c in "- milk".chars() {
            e.insert_char(c);
        }
        e.newline();
        assert_eq!(e.text(), "- milk\n- ");
        e.newline(); // empty bullet ends the list
        assert_eq!(e.text(), "- milk\n");
        e.insert_str("a\r\nb");
        assert_eq!(e.text(), "- milk\na\nb");
        e.backspace();
        e.backspace();
        assert_eq!(e.text(), "- milk\na");
        e.undo();
        assert_eq!(e.text(), "- milk\na\nb");
        e.redo();
        assert_eq!(e.text(), "- milk\na");
        e.doc_start();
        e.delete();
        assert_eq!(e.text(), " milk\na");
        let mut e = Editor::new("1. first");
        e.newline();
        assert_eq!(e.text(), "1. first\n2. ");
    }
}
