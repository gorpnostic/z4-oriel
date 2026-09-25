//! notes: nest's notes app. Markdown notes as .md files in <data dir>/oriel/notes, edited in place and saved a
//! second after you stop typing. ctrl+e flips to a rendered preview, ctrl+n makes a note, ctrl+d deletes one
//! (after a y). The sidebar lists every note, newest first; a note's title is its first line.
//!
//! open_in can seed an empty notes folder by copying .md files from another folder (never moving them).

mod editor;
mod md;

use super::files::clock;
use crate::pane::{Cx, Pane};
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use editor::Editor;
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const WELCOME: &str = "# welcome to notes\n\nType anything. It saves itself.\n";
/// Written after nest's notes have been imported, so deleting every note later doesn't bring them back.
const IMPORTED: &str = ".imported-from-nest";

#[derive(Clone, Debug)]
struct Meta {
    id: String,
    title: String,
    mtime: std::time::SystemTime,
}

fn title_of(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim_start_matches('#').trim();
    if first.is_empty() { "untitled".into() } else { first.to_string() }
}

pub struct Notes {
    dir: PathBuf,
    list: Vec<Meta>,
    /// id of the open note (file stem).
    cur: Option<String>,
    ed: Editor,
    crlf: bool,
    dirty_at: Option<Instant>,
    saved_at: Option<i64>,
    previewing: bool,
    pscroll: usize,
    confirm_delete: bool,
    error: Option<String>,
    /// Text area from the last render (for clicks, paging, wrapping).
    text_rect: Rect,
    side_hits: Vec<(Rect, Option<usize>)>,
    side_scroll: usize,
}

impl Notes {
    pub fn new() -> Self {
        // tests (the app's own snapshot test opens every app) never touch the real notes or nest's
        #[cfg(test)]
        return Self::open_in(std::path::absolute("target/test-scratch/notes-app").unwrap_or_default(), None);
        #[cfg(not(test))]
        {
            {
                let custom = crate::config::load().notes_folder;
                let dir = if custom.trim().is_empty() { crate::config::data_dir().join("notes") } else { std::path::PathBuf::from(custom.trim()) };
                Self::open_in(dir, None)
            }
        }
    }

    /// Notes kept in `dir`; `import_from` is nest's folder to copy from on first run (tests pass their own).
    pub(crate) fn open_in(dir: PathBuf, import_from: Option<PathBuf>) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        let mut n = Notes {
            dir,
            list: vec![],
            cur: None,
            ed: Editor::new(""),
            crlf: false,
            dirty_at: None,
            saved_at: None,
            previewing: false,
            pscroll: 0,
            confirm_delete: false,
            error: None,
            text_rect: Rect::default(),
            side_hits: vec![],
            side_scroll: 0,
        };
        if let Some(src) = import_from {
            n.import(&src);
        }
        n.refresh();
        match n.list.first().map(|m| m.id.clone()) {
            Some(id) => n.open(&id),
            None => {
                let id = n.create(WELCOME);
                n.open(&id);
            }
        }
        n
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.md"))
    }

    fn md_files(dir: &Path) -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
        rd.flatten().map(|e| e.path()).filter(|p| p.is_file() && p.extension().map(|e| e.eq_ignore_ascii_case("md")).unwrap_or(false)).collect()
    }

    /// Copy (never move) nest's notes in, once, and only into an empty folder.
    fn import(&mut self, src: &Path) {
        if !Self::md_files(&self.dir).is_empty() || self.dir.join(IMPORTED).exists() || !src.is_dir() {
            return;
        }
        let mut copied = 0;
        for p in Self::md_files(src) {
            let Some(name) = p.file_name() else { continue };
            let to = self.dir.join(name);
            if !to.exists() && std::fs::copy(&p, &to).is_ok() {
                // keep nest's modified times so the order in the sidebar survives
                if let Ok(m) = std::fs::metadata(&p).and_then(|m| m.modified()) {
                    let _ = std::fs::File::options().write(true).open(&to).and_then(|f| f.set_modified(m));
                }
                copied += 1;
            }
        }
        let _ = std::fs::write(self.dir.join(IMPORTED), format!("copied {copied} notes from {}\n", src.display()));
    }

    fn refresh(&mut self) {
        let mut list = vec![];
        for p in Self::md_files(&self.dir) {
            let Some(id) = p.file_stem().map(|s| s.to_string_lossy().to_string()) else { continue };
            let first = std::fs::File::open(&p)
                .ok()
                .and_then(|f| {
                    use std::io::BufRead;
                    let mut s = String::new();
                    std::io::BufReader::new(f).read_line(&mut s).ok().map(|_| s)
                })
                .unwrap_or_default();
            let mtime = std::fs::metadata(&p).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            list.push(Meta { id, title: title_of(&first), mtime });
        }
        list.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.id.cmp(&b.id)));
        self.list = list;
    }

    fn create(&mut self, text: &str) -> String {
        let now = clock::now_secs();
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let mut id = format!("{}-{:04x}", clock::compact(now), (nanos ^ (nanos >> 16)) & 0xffff);
        while self.path(&id).exists() {
            id.push('x');
        }
        if let Err(e) = std::fs::write(self.path(&id), text) {
            self.error = Some(format!("can't create a note: {e}"));
        }
        self.refresh();
        id
    }

    fn open(&mut self, id: &str) {
        self.save();
        match std::fs::read_to_string(self.path(id)) {
            Ok(text) => {
                self.crlf = text.contains("\r\n");
                self.ed.set_text(&text);
                self.cur = Some(id.to_string());
                self.dirty_at = None;
                self.saved_at = None;
                self.pscroll = 0;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("can't open that note: {e}")),
        }
    }

    fn save(&mut self) {
        let (Some(id), Some(_)) = (self.cur.clone(), self.dirty_at) else { return };
        let mut text = self.ed.text();
        if self.crlf {
            text = text.replace('\n', "\r\n");
        }
        let path = self.path(&id);
        let tmp = path.with_extension("md.tmp");
        match std::fs::write(&tmp, &text).and_then(|_| std::fs::rename(&tmp, &path)) {
            Ok(()) => {
                self.dirty_at = None;
                self.saved_at = Some(clock::now_secs());
                self.error = None;
                // update this note's entry without re-reading the folder
                let title = title_of(&text);
                let now = std::time::SystemTime::now();
                if let Some(m) = self.list.iter_mut().find(|m| m.id == id) {
                    m.title = title;
                    m.mtime = now;
                } else {
                    self.list.push(Meta { id: id.clone(), title, mtime: now });
                }
                self.list.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.id.cmp(&b.id)));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                self.error = Some(format!("couldn't save: {e}"));
            }
        }
    }

    fn touch(&mut self) {
        self.dirty_at = Some(Instant::now());
    }

    fn new_note(&mut self) {
        self.save();
        let id = self.create("# new note\n\n");
        self.open(&id);
        self.previewing = false;
    }

    fn delete_current(&mut self, cx: &mut Cx) {
        self.confirm_delete = false;
        let Some(id) = self.cur.take() else { return };
        self.dirty_at = None;
        match std::fs::remove_file(self.path(&id)) {
            Ok(()) => cx.notify(format!("{}note deleted", ui::lead("trash"))),
            Err(e) => cx.notify(format!("couldn't delete: {e}")),
        }
        self.refresh();
        match self.list.first().map(|m| m.id.clone()) {
            Some(next) => self.open(&next),
            None => {
                let id = self.create("# new note\n\n");
                self.open(&id);
            }
        }
    }

    fn cur_index(&self) -> Option<usize> {
        let id = self.cur.as_ref()?;
        self.list.iter().position(|m| &m.id == id)
    }

    fn width(&self) -> usize {
        (self.text_rect.width as usize).max(10)
    }

    fn page(&self) -> usize {
        (self.text_rect.height as usize).saturating_sub(1).max(1)
    }

    fn edit_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let segs = self.ed.layout(self.width());
        let mut edited = true;
        match key.code {
            KeyCode::Char('z') if ctrl => edited = self.ed.undo(),
            KeyCode::Char('y') if ctrl => edited = self.ed.redo(),
            KeyCode::Char(_) if ctrl => return false,
            KeyCode::Char(c) => self.ed.insert_char(c),
            KeyCode::Enter => self.ed.newline(),
            KeyCode::Tab => self.ed.insert_str("    "),
            KeyCode::Backspace if ctrl => self.ed.delete_word_left(),
            KeyCode::Backspace => self.ed.backspace(),
            KeyCode::Delete => self.ed.delete(),
            _ => {
                edited = false;
                match key.code {
                    KeyCode::Left if ctrl => self.ed.word_left(),
                    KeyCode::Right if ctrl => self.ed.word_right(),
                    KeyCode::Left => self.ed.left(),
                    KeyCode::Right => self.ed.right(),
                    KeyCode::Up => self.ed.vmove(&segs, -1),
                    KeyCode::Down => self.ed.vmove(&segs, 1),
                    KeyCode::Home if ctrl => self.ed.doc_start(),
                    KeyCode::End if ctrl => self.ed.doc_end(),
                    KeyCode::Home => self.ed.home(&segs),
                    KeyCode::End => self.ed.end(&segs),
                    KeyCode::PageUp => {
                        self.ed.vmove(&segs, -(self.page() as isize));
                        self.ed.scroll = self.ed.scroll.saturating_sub(self.page());
                    }
                    KeyCode::PageDown => {
                        self.ed.vmove(&segs, self.page() as isize);
                        self.ed.scroll += self.page();
                    }
                    _ => return false,
                }
            }
        }
        if edited {
            self.touch();
        }
        let segs = self.ed.layout(self.width());
        let h = self.text_rect.height as usize;
        self.ed.clamp_scroll(&segs, h);
        self.ed.follow(&segs, h);
        true
    }

    fn preview_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.pscroll += 1,
            KeyCode::Char('k') | KeyCode::Up => self.pscroll = self.pscroll.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.pscroll += self.page(),
            KeyCode::PageUp => self.pscroll = self.pscroll.saturating_sub(self.page()),
            KeyCode::Home | KeyCode::Char('g') => self.pscroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.pscroll = usize::MAX / 2,
            KeyCode::Esc => self.previewing = false,
            _ => return false,
        }
        true
    }
}

impl Drop for Notes {
    fn drop(&mut self) {
        self.save(); // closing the pane or quitting inside the autosave second loses nothing
    }
}

impl Pane for Notes {
    fn title(&self) -> String {
        let t = self.cur_index().map(|i| self.list[i].title.clone()).unwrap_or_else(|| title_of(&self.ed.lines[0]));
        ui::fit(&t, 60)
    }
    fn subtitle(&self) -> Option<String> {
        if self.dirty_at.is_some() {
            Some("editing…".into())
        } else {
            self.saved_at.map(|s| format!("saved {}", clock::hms(s)))
        }
    }
    fn icon(&self) -> &'static str {
        "notes"
    }
    fn tick_every(&self) -> Option<Duration> {
        self.dirty_at.map(|_| Duration::from_millis(250))
    }

    fn poll(&mut self, _cx: &mut Cx) {
        if self.dirty_at.map(|t| t.elapsed() >= Duration::from_secs(1)).unwrap_or(false) {
            self.save();
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        // the bottom line: a delete confirmation, an error, or nest's hints
        let body = if self.confirm_delete {
            let title = self.title();
            let line = Line::from(vec![
                Span::styled(format!(" delete \"{}\"?  ", ui::fit(&title, 40)), Style::default().fg(t.danger).add_modifier(Modifier::BOLD)),
                Span::styled("y", Style::default().fg(t.fg).add_modifier(Modifier::BOLD)),
                Span::styled(" delete · ", ui::muted(t)),
                Span::styled("esc", Style::default().fg(t.fg).add_modifier(Modifier::BOLD)),
                Span::styled(" keep it", ui::muted(t)),
            ]);
            f.render_widget(Paragraph::new(line), Rect { y: area.bottom().saturating_sub(1), height: 1, ..area });
            Rect { height: area.height.saturating_sub(1), ..area }
        } else if let Some(e) = &self.error {
            f.render_widget(Paragraph::new(Span::styled(format!(" {e}"), Style::default().fg(t.danger))), Rect { y: area.bottom().saturating_sub(1), height: 1, ..area });
            Rect { height: area.height.saturating_sub(1), ..area }
        } else {
            let hints: &[(&str, &str)] = if self.previewing {
                &[("rendered", ""), ("ctrl+e", "edit"), ("ctrl+d", "delete"), ("ctrl+n", "new note")]
            } else {
                &[("autosaves", ""), ("ctrl+e", "edit/preview"), ("ctrl+d", "delete"), ("ctrl+n", "new note")]
            };
            ui::hint_line(f, area, hints, t)
        };
        // nest: one row of air at the top and above the hints, a column of padding each side
        let r = Rect { x: body.x + 1, y: body.y + 1, width: body.width.saturating_sub(2), height: body.height.saturating_sub(2) };
        self.text_rect = r;
        if r.width == 0 || r.height == 0 {
            return;
        }
        let h = r.height as usize;
        if self.previewing {
            let lines = md::render(&self.ed.text(), t, r.width);
            self.pscroll = self.pscroll.min(lines.len().saturating_sub(1));
            let lines: Vec<Line> = lines.into_iter().skip(self.pscroll).collect();
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), r);
            return;
        }
        let segs = self.ed.layout(r.width as usize);
        self.ed.clamp_scroll(&segs, h);
        let mut out = Vec::with_capacity(h);
        for s in segs.iter().skip(self.ed.scroll).take(h) {
            let line = &self.ed.lines[s.row];
            let text: String = line.chars().skip(s.start).take(s.end - s.start).collect();
            let st = if line.starts_with('#') { Style::default().fg(t.accent).add_modifier(Modifier::BOLD) } else { Style::default() };
            out.push(Line::from(Span::styled(text, st)));
        }
        f.render_widget(Paragraph::new(out), r);
        if cx.focused && !self.confirm_delete {
            let (v, x) = self.ed.cursor_visual(&segs);
            if v >= self.ed.scroll && v < self.ed.scroll + h {
                let x = (x as u16).min(r.width.saturating_sub(1));
                f.set_cursor_position(Position { x: r.x + x, y: r.y + (v - self.ed.scroll) as u16 });
            }
        }
    }

    fn key(&mut self, key: KeyEvent, cx: &mut Cx) -> bool {
        if key.modifiers.contains(KeyModifiers::ALT) {
            return false;
        }
        if self.confirm_delete {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.delete_current(cx),
                _ => self.confirm_delete = false, // esc, or anything else, keeps the note
            }
            return true;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl {
            match key.code {
                KeyCode::Char('e') => {
                    self.save();
                    self.previewing = !self.previewing;
                    self.pscroll = 0;
                    return true;
                }
                KeyCode::Char('n') => {
                    self.new_note();
                    return true;
                }
                KeyCode::Char('d') => {
                    if self.cur.is_some() {
                        self.confirm_delete = true;
                    }
                    return true;
                }
                KeyCode::Char('s') => {
                    if self.dirty_at.is_none() && self.cur.is_some() {
                        self.touch();
                    }
                    self.save();
                    return true;
                }
                _ => {}
            }
        }
        if self.previewing { self.preview_key(key) } else { self.edit_key(key) }
    }

    fn paste(&mut self, text: &str, _cx: &mut Cx) {
        if self.previewing || self.confirm_delete {
            return;
        }
        self.ed.insert_str(text);
        self.touch();
        let segs = self.ed.layout(self.width());
        self.ed.follow(&segs, self.text_rect.height as usize);
    }

    fn mouse(&mut self, ev: MouseEvent, _area: Rect, _cx: &mut Cx) {
        let r = self.text_rect;
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) if !self.previewing => {
                if ev.row < r.y {
                    return;
                }
                let segs = self.ed.layout(self.width());
                let v = self.ed.scroll + (ev.row - r.y).min(r.height.saturating_sub(1)) as usize;
                let x = ev.column.saturating_sub(r.x) as usize;
                self.ed.click(&segs, v, x);
            }
            MouseEventKind::ScrollDown if self.previewing => self.pscroll += 3,
            MouseEventKind::ScrollUp if self.previewing => self.pscroll = self.pscroll.saturating_sub(3),
            MouseEventKind::ScrollDown => {
                let segs = self.ed.layout(self.width());
                self.ed.scroll += 3;
                self.ed.clamp_scroll(&segs, r.height as usize);
            }
            MouseEventKind::ScrollUp => self.ed.scroll = self.ed.scroll.saturating_sub(3),
            _ => {}
        }
    }

    fn side(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        self.side_hits.clear();
        if area.height == 0 {
            return;
        }
        let r = Rect { height: 1, ..area };
        ui::side_row(f, r, "notes", "new note", "ctrl+n", false, t);
        self.side_hits.push((r, None));
        let list_area = Rect { y: area.y + 2, height: area.height.saturating_sub(2), ..area };
        let h = list_area.height as usize;
        if h == 0 {
            return;
        }
        if let Some(c) = self.cur_index() {
            if c < self.side_scroll {
                self.side_scroll = c;
            } else if c >= self.side_scroll + h {
                self.side_scroll = c + 1 - h;
            }
        }
        self.side_scroll = self.side_scroll.min(self.list.len().saturating_sub(h));
        let cur = self.cur_index();
        for (row, (i, m)) in self.list.iter().enumerate().skip(self.side_scroll).take(h).enumerate() {
            let rr = Rect { y: list_area.y + row as u16, height: 1, ..list_area };
            let on = cur == Some(i);
            let st = if on { Style::default().add_modifier(Modifier::BOLD) } else { Style::default() };
            f.render_widget(Paragraph::new(Span::styled(ui::fit(&m.title, rr.width as usize), st)), rr);
            self.side_hits.push((rr, Some(i)));
        }
    }

    fn side_mouse(&mut self, ev: MouseEvent, area: Rect, _cx: &mut Cx) {
        let pos = Position { x: ev.column, y: ev.row };
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(&(_, hit)) = self.side_hits.iter().find(|(r, _)| r.contains(pos)) else { return };
                self.confirm_delete = false;
                match hit {
                    None => self.new_note(),
                    Some(i) => {
                        if let Some(id) = self.list.get(i).map(|m| m.id.clone()) {
                            if Some(&id) != self.cur.as_ref() {
                                self.open(&id);
                            }
                        }
                    }
                }
            }
            MouseEventKind::ScrollDown if area.contains(pos) => self.side_scroll += 1,
            MouseEventKind::ScrollUp => self.side_scroll = self.side_scroll.saturating_sub(1),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panes::files::tests::{scratch, snap};
    use crate::testkit::Kit;

    fn nest_fixture(name: &str) -> PathBuf {
        let src = scratch(name);
        std::fs::write(src.join("ideas.md"), "# ideas\n\nThings to try this week. One per line starting with \"- \".\n\n- Learn a new chord.\n").unwrap();
        std::fs::write(src.join("20260923-184214-edb2.md"), WELCOME).unwrap();
        src
    }

    #[test]
    fn notes_imports_by_copying() {
        let src = nest_fixture("notes-import-src");
        let dir = scratch("notes-import");
        let p = Notes::open_in(dir.clone(), Some(src.clone()));
        assert_eq!(p.list.len(), 2);
        assert!(src.join("ideas.md").exists(), "nest's notes are copied, not moved");
        assert!(dir.join("ideas.md").exists());
        assert!(dir.join(IMPORTED).exists());
        drop(p);
        // delete everything: nest's notes don't come back, a fresh note appears instead
        for f in Notes::md_files(&dir) {
            std::fs::remove_file(f).unwrap();
        }
        let p = Notes::open_in(dir.clone(), Some(src));
        assert_eq!(p.list.len(), 1);
        assert_eq!(p.title(), "welcome to notes");
    }

    #[test]
    fn notes_edit_autosave_preview() {
        let src = nest_fixture("notes-edit-src");
        let dir = scratch("notes-edit");
        let mut k = Kit::new();
        let mut p = Notes::open_in(dir.clone(), Some(src));
        // open the imported note
        let mi = p.list.iter().position(|m| m.id == "ideas").unwrap();
        let id = p.list[mi].id.clone();
        p.open(&id);
        let s = snap(&mut k, &mut p, 150, 44, "target/snap/notes-main.html");
        println!("{s}");
        assert!(s.contains("# ideas") && s.contains("Learn a new chord."));
        assert!(s.contains("new note") && s.contains("ctrl+n"), "sidebar: new note row");
        assert!(s.contains("welcome to notes"), "sidebar: other notes");
        assert!(s.contains("autosaves · ctrl+e edit/preview · ctrl+d delete · ctrl+n new note"));

        // type at the end (the cursor starts there, like nest)
        k.key(&mut p, KeyCode::Enter);
        k.typ(&mut p, "- Likes **fast** tools and `rust`");
        assert!(p.dirty_at.is_some());
        assert_eq!(p.tick_every(), Some(Duration::from_millis(250)));
        assert_eq!(p.subtitle().as_deref(), Some("editing…"));
        p.dirty_at = Some(Instant::now() - Duration::from_secs(2));
        k.poll(&mut p);
        assert!(p.dirty_at.is_none() && p.tick_every().is_none());
        assert!(p.subtitle().unwrap().starts_with("saved "));
        let disk = std::fs::read_to_string(dir.join("ideas.md")).unwrap();
        assert!(disk.ends_with("- Likes **fast** tools and `rust`"), "{disk:?}");

        // ctrl+left/right, home/end
        k.key_mod(&mut p, KeyCode::Left, KeyModifiers::CONTROL);
        assert_eq!(p.ed.col, p.ed.lines[p.ed.row].chars().count() - 5);
        k.key(&mut p, KeyCode::Home);
        assert_eq!(p.ed.col, 0);

        // preview
        k.key_mod(&mut p, KeyCode::Char('e'), KeyModifiers::CONTROL);
        let s = snap(&mut k, &mut p, 150, 44, "target/snap/notes-preview.html");
        assert!(s.contains("• Likes fast tools and rust"), "rendered bullets/inline:\n{s}");
        assert!(!s.contains("# ideas"), "heading marks hidden in preview");
        k.key_mod(&mut p, KeyCode::Char('e'), KeyModifiers::CONTROL);

        // click places the cursor: row 0 of the text area, column 3
        let r = p.text_rect;
        let ev = MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: r.x + 3, row: r.y, modifiers: KeyModifiers::NONE };
        k.mouse(&mut p, ev, r);
        assert_eq!((p.ed.row, p.ed.col), (0, 3));

        // paste
        let mut cx_actions = vec![];
        {
            let mut cx = Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: &mut cx_actions, focused: true, time: 1.0 };
            p.paste("ABC\r\nDEF", &mut cx);
        }
        assert_eq!(p.ed.lines[0], "# iABC");
        assert_eq!(p.ed.lines[1], "DEFdeas");
    }

    #[test]
    fn notes_new_and_delete() {
        let dir = scratch("notes-del");
        let keep = dir.join("keep.md");
        std::fs::write(&keep, "# keep me\n").unwrap();
        let mut k = Kit::new();
        let mut p = Notes::open_in(dir.clone(), None);
        k.key_mod(&mut p, KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert_eq!(p.list.len(), 2);
        assert_eq!(p.title(), "new note");
        let new_path = p.path(p.cur.as_ref().unwrap());
        assert!(new_path.exists());
        // ctrl+d then esc: nothing deleted
        k.key_mod(&mut p, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(k.render(&mut p, 120, 30).contains("delete \"new note\"?"));
        k.key(&mut p, KeyCode::Esc);
        assert!(new_path.exists());
        // ctrl+d then y: only that note goes
        k.key_mod(&mut p, KeyCode::Char('d'), KeyModifiers::CONTROL);
        k.key(&mut p, KeyCode::Char('y'));
        assert!(!new_path.exists());
        assert!(keep.exists());
        assert_eq!(p.title(), "keep me");
        let side = k.render_side(&mut p, 30, 10);
        println!("{side}");
        assert!(side.contains("keep me"));
    }
}
