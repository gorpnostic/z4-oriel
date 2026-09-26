//! notes: the notes app. Markdown notes as .md files in the notes folder (config::notes_dir: notes_folder, else
//! <data dir>/oriel/notes), edited in place and saved a second after you stop typing. ctrl+e flips to a rendered
//! preview, ctrl+n makes a note, ctrl+d deletes one (after a y), ctrl+up/down open the next note in the list,
//! ctrl+f finds notes by title or text. The sidebar lists every note, newest first; a note's title is its first
//! line.
//!
//! The folder is plain .md files that other programs (an editor, a coding agent, chat's /note) may change too, so
//! the pane watches it (or, where a folder can't be watched, looks every second and a half while it's on screen):
//! notes added outside show up, an outside change to the open note is loaded if you have no unsaved edits, and if
//! you do, nothing is written until you choose (r takes theirs, ctrl+s keeps yours). Every save also checks first.
//!
//! open_in can seed an empty notes folder by copying .md files from another folder (never moving them).

mod editor;
mod md;

use super::files::clock;
use crate::pane::{Cx, Pane, Waker};
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

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

/// How often the disk is checked for outside changes while the pane is on screen, when the folder can't be
/// watched.
const DISK_CHECK: Duration = Duration::from_millis(1500);

/// A file's (modified time, size), to tell whether someone else wrote it. None = it isn't there.
fn stamp(p: &Path) -> Option<(SystemTime, u64)> {
    let m = std::fs::metadata(p).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// ctrl+f: the sidebar shows only the notes whose title (right away) or text (from a background scan) matches.
struct Find {
    query: String,
    /// the highlighted match (an index into `found()`)
    sel: usize,
    /// ids of notes whose text contains the query, from the last scan that finished for this query
    content: Vec<String>,
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
    /// (open note, its place in the list) when the sidebar last scrolled it into view: the view only follows the
    /// note when that changes, so the wheel scrolls freely.
    side_follow: Option<(String, usize)>,
    /// The open note's file as we last read or wrote it, to spot changes made outside oriel.
    disk: Option<(SystemTime, u64)>,
    /// The open note changed on disk while you had unsaved edits: nothing is written until you pick a side.
    conflict: bool,
    /// The folder's modified time when the list was last read (adding, removing or renaming a note changes it).
    dir_stamp: Option<SystemTime>,
    last_check: Instant,
    /// Watches the folder for other programs' changes (from the first render); None = couldn't, so poll instead.
    watch: Option<notify::RecommendedWatcher>,
    watch_tried: bool,
    /// Set by the watcher when something in the folder changed: the next poll looks at the disk.
    disk_changed: Arc<AtomicBool>,
    find: Option<Find>,
    /// Where the background text scan leaves (its number, matching ids); only the newest scan may write.
    find_slot: Arc<Mutex<Option<(u64, Vec<String>)>>>,
    find_gen: Arc<AtomicU64>,
    /// Opens the new folder when notes_folder changes (settings › folders). Off for tests' own folders.
    follow: bool,
}

impl Notes {
    pub fn new(cfg: &crate::config::Config) -> Self {
        // tests (the app's own snapshot test opens every app) never touch the real notes or nest's
        #[cfg(test)]
        {
            let _ = cfg;
            Self::open_in(std::path::absolute("target/test-scratch/notes-app").unwrap_or_default(), None)
        }
        #[cfg(not(test))]
        {
            let mut n = Self::open_in(crate::config::notes_dir(cfg), None);
            n.follow = true;
            n
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
            side_follow: None,
            disk: None,
            conflict: false,
            dir_stamp: None,
            last_check: Instant::now(),
            watch: None,
            watch_tried: false,
            disk_changed: Arc::new(AtomicBool::new(false)),
            find: None,
            find_slot: Arc::new(Mutex::new(None)),
            find_gen: Arc::new(AtomicU64::new(0)),
            follow: false,
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
        self.dir_stamp = std::fs::metadata(&self.dir).and_then(|m| m.modified()).ok();
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
        if self.conflict {
            return; // the open note changed on disk under unsaved edits: that question comes first
        }
        let path = self.path(id);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                self.crlf = text.contains("\r\n");
                self.ed.set_text(&text);
                self.cur = Some(id.to_string());
                self.disk = stamp(&path);
                self.conflict = false;
                self.dirty_at = None;
                self.saved_at = None;
                self.pscroll = 0;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("can't open that note: {e}")),
        }
    }

    /// Load the open note again from disk (it changed outside oriel), keeping the cursor where it was.
    fn reload(&mut self) {
        let Some(id) = self.cur.clone() else { return };
        let path = self.path(&id);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let (row, col, scroll) = (self.ed.row, self.ed.col, self.ed.scroll);
                self.crlf = text.contains("\r\n");
                self.ed.set_text(&text);
                self.ed.row = row.min(self.ed.lines.len() - 1);
                self.ed.col = col.min(self.ed.lines[self.ed.row].chars().count());
                self.ed.scroll = scroll;
                self.disk = stamp(&path);
                self.conflict = false;
                self.dirty_at = None;
                self.error = None;
                self.refresh(); // its title may have changed
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.let_go(),
            Err(e) => self.error = Some(format!("can't reload that note: {e}")),
        }
    }

    /// The open note was deleted (or renamed) outside oriel and you've chosen theirs: drop it, open the newest.
    fn let_go(&mut self) {
        self.cur = None;
        self.dirty_at = None;
        self.conflict = false;
        self.refresh();
        self.open_first();
    }

    /// Open the newest note, or a fresh one if the folder is empty.
    fn open_first(&mut self) {
        match self.list.first().map(|m| m.id.clone()) {
            Some(id) => self.open(&id),
            None => {
                let id = self.create("# new note\n\n");
                self.open(&id);
            }
        }
    }

    fn save(&mut self) {
        self.write(false);
    }

    /// Write the open note if it has unsaved edits. Unless `force` (ctrl+s on "keep mine"), it first checks that
    /// nobody else wrote the file since we read it; if they did, it writes nothing and asks instead.
    fn write(&mut self, force: bool) {
        let (Some(id), Some(_)) = (self.cur.clone(), self.dirty_at) else { return };
        let path = self.path(&id);
        if !force && (self.conflict || stamp(&path) != self.disk) {
            self.conflict = true;
            return;
        }
        let mut text = self.ed.text();
        if self.crlf {
            text = text.replace('\n', "\r\n");
        }
        let tmp = path.with_extension("md.tmp");
        let dir_before = std::fs::metadata(&self.dir).and_then(|m| m.modified()).ok();
        match std::fs::write(&tmp, &text).and_then(|_| std::fs::rename(&tmp, &path)) {
            Ok(()) => {
                self.disk = stamp(&path);
                if dir_before == self.dir_stamp {
                    // our own write touched the folder: not a reason to re-read every note
                    self.dir_stamp = std::fs::metadata(&self.dir).and_then(|m| m.modified()).ok();
                }
                self.conflict = false;
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
        if self.conflict {
            return;
        }
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
        self.open_first();
    }

    fn cur_index(&self) -> Option<usize> {
        let id = self.cur.as_ref()?;
        self.list.iter().position(|m| &m.id == id)
    }

    /// Open the note `d` places down (+1) or up (-1) the list from the open one.
    fn step_note(&mut self, d: isize) {
        let Some(i) = self.cur_index() else { return };
        let Some(m) = i.checked_add_signed(d).and_then(|j| self.list.get(j)) else { return };
        let id = m.id.clone();
        self.open(&id);
    }

    /// Look at the disk: re-read the list if the folder changed, and pick up an outside change to the open note
    /// (loaded if you have no unsaved edits, else a question).
    fn check_disk(&mut self) {
        let dir = std::fs::metadata(&self.dir).and_then(|m| m.modified()).ok();
        if dir != self.dir_stamp {
            self.refresh();
        }
        let Some(id) = self.cur.clone() else { return };
        let now = stamp(&self.path(&id));
        if now == self.disk || self.conflict {
            return;
        }
        match (now, self.dirty_at) {
            // no unsaved edits: theirs wins (and a note deleted outside is let go)
            (Some(_), None) => self.reload(),
            (None, None) => self.let_go(),
            // unsaved edits: ask
            _ => self.conflict = true,
        }
    }

    /// Watch the notes folder: a change wakes the pane, which then looks (no polling, no redraws while idle).
    fn watch_dir(&self, waker: Waker) -> Option<notify::RecommendedWatcher> {
        use notify::{RecursiveMode, Watcher};
        let changed = self.disk_changed.clone();
        let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            // a save is a burst of events: wake once, the poll clears the flag
            if res.is_ok() && !changed.swap(true, Ordering::SeqCst) {
                waker.wake();
            }
        })
        .ok()?;
        w.watch(&self.dir, RecursiveMode::NonRecursive).ok()?;
        Some(w)
    }

    /// The notes the finder shows, as indices into the list (all of them before anything is typed).
    fn found(&self) -> Vec<usize> {
        let Some(f) = &self.find else { return (0..self.list.len()).collect() };
        let q = f.query.trim().to_lowercase();
        (0..self.list.len()).filter(|&i| q.is_empty() || self.list[i].title.to_lowercase().contains(&q) || f.content.contains(&self.list[i].id)).collect()
    }

    /// Scan every note's text for the finder's query on a background thread (titles match without it).
    fn scan_find(&mut self, cx: &Cx) {
        let Some(f) = &mut self.find else { return };
        f.sel = 0;
        f.content.clear();
        let q = f.query.trim().to_lowercase();
        if q.is_empty() {
            self.find_gen.fetch_add(1, Ordering::SeqCst); // nothing to scan for: older scans are stale
            return;
        }
        let g = self.find_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let (dir, slot, newest, waker) = (self.dir.clone(), self.find_slot.clone(), self.find_gen.clone(), cx.waker());
        std::thread::spawn(move || {
            let hits = Self::md_files(&dir)
                .into_iter()
                .take_while(|_| newest.load(Ordering::SeqCst) == g) // typed on: stop reading for the old query
                .filter(|p| std::fs::read_to_string(p).is_ok_and(|t| t.to_lowercase().contains(&q)))
                .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
                .collect();
            // a slower scan for an older query must not overwrite a newer one (checked under the slot's lock)
            let mut slot = slot.lock().unwrap();
            if newest.load(Ordering::SeqCst) == g {
                *slot = Some((g, hits));
                drop(slot);
                waker.wake();
            }
        });
    }

    fn find_key(&mut self, key: KeyEvent, cx: &mut Cx) {
        let found = self.found();
        let Some(f) = &mut self.find else { return };
        match key.code {
            KeyCode::Esc => self.find = None,
            KeyCode::Enter => {
                let pick = found.get(f.sel).map(|&i| self.list[i].id.clone());
                self.find = None;
                if let Some(id) = pick.filter(|id| Some(id) != self.cur.as_ref()) {
                    self.open(&id);
                }
            }
            KeyCode::Up => f.sel = f.sel.saturating_sub(1),
            KeyCode::Down => f.sel = (f.sel + 1).min(found.len().saturating_sub(1)),
            KeyCode::Backspace => {
                f.query.pop();
                self.scan_find(cx);
            }
            _ => {
                if let Some(c) = ui::typed_char(&key) {
                    f.query.push(c);
                    self.scan_find(cx);
                }
            }
        }
    }

    fn width(&self) -> usize {
        (self.text_rect.width as usize).max(10)
    }

    fn page(&self) -> usize {
        (self.text_rect.height as usize).saturating_sub(1).max(1)
    }

    fn edit_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL) && ui::typed_char(&key).is_none(); // AltGr types
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
        if self.conflict && self.dirty_at.is_some() {
            // it changed on disk under your edits ("theirs or mine?"): theirs stays in the file, yours becomes a note
            // of its own
            let text = self.ed.text();
            self.create(&text);
        }
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
        match (self.dirty_at, &self.watch) {
            (Some(_), _) => Some(Duration::from_millis(250)), // the autosave second
            (None, None) => Some(DISK_CHECK),                 // no watcher: look at the disk now and then
            (None, Some(_)) => None,                          // the watcher wakes us
        }
    }

    /// A new notes folder (settings › folders) opens here at once; what you were typing is saved first, where it
    /// was.
    fn config_changed(&mut self, cfg: &crate::config::Config) {
        let dir = crate::config::notes_dir(cfg);
        if self.follow && dir != self.dir {
            let previewing = self.previewing;
            *self = Self::open_in(dir, None); // the old one saves as it's dropped
            self.follow = true;
            self.previewing = previewing;
        }
    }

    fn poll(&mut self, _cx: &mut Cx) {
        if let Some((g, hits)) = self.find_slot.lock().unwrap().take() {
            if let Some(f) = self.find.as_mut().filter(|_| g == self.find_gen.load(Ordering::SeqCst)) {
                f.content = hits;
            }
        }
        if self.disk_changed.swap(false, Ordering::SeqCst) || self.last_check.elapsed() >= DISK_CHECK {
            self.last_check = Instant::now();
            self.check_disk();
        }
        if self.dirty_at.map(|t| t.elapsed() >= Duration::from_secs(1)).unwrap_or(false) {
            self.save();
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        if !self.watch_tried {
            self.watch_tried = true;
            self.watch = self.watch_dir(cx.waker());
        }
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
        } else if self.conflict {
            let gone = self.cur.as_ref().is_some_and(|id| !self.path(id).exists());
            let (what, theirs) = if gone { ("was deleted outside oriel", " let it go · ") } else { ("changed on disk", " reload theirs · ") };
            let line = Line::from(vec![
                Span::styled(format!(" this note {what} while you were editing it  "), Style::default().fg(t.danger).add_modifier(Modifier::BOLD)),
                Span::styled("r", Style::default().fg(t.fg).add_modifier(Modifier::BOLD)),
                Span::styled(theirs, ui::muted(t)),
                Span::styled("ctrl+s", Style::default().fg(t.fg).add_modifier(Modifier::BOLD)),
                Span::styled(" keep mine", ui::muted(t)),
            ]);
            f.render_widget(Paragraph::new(line), Rect { y: area.bottom().saturating_sub(1), height: 1, ..area });
            Rect { height: area.height.saturating_sub(1), ..area }
        } else if let Some(e) = &self.error {
            f.render_widget(Paragraph::new(Span::styled(format!(" {e}"), Style::default().fg(t.danger))), Rect { y: area.bottom().saturating_sub(1), height: 1, ..area });
            Rect { height: area.height.saturating_sub(1), ..area }
        } else {
            // finding: the query and the count here too, so it works with the sidebar hidden
            let found = self.find.as_ref().map(|fd| match fd.query.is_empty() {
                true => "type a title or text".to_string(),
                false => format!("\"{}\" · {} found", fd.query, self.found().len()),
            });
            let hints: Vec<(&str, &str)> = if let Some(found) = &found {
                vec![("find", found.as_str()), ("↑↓", "pick"), ("enter", "open"), ("esc", "close")]
            } else if self.previewing {
                vec![("rendered", ""), ("ctrl+e", "edit"), ("ctrl+d", "delete"), ("ctrl+n", "new note"), ("ctrl+f", "find"), ("ctrl+↑↓", "other notes")]
            } else {
                vec![("autosaves", ""), ("ctrl+e", "edit/preview"), ("ctrl+d", "delete"), ("ctrl+n", "new note"), ("ctrl+f", "find"), ("ctrl+↑↓", "other notes")]
            };
            ui::hint_line(f, area, &hints, t)
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
        if cx.focused && !self.confirm_delete && !self.conflict && self.find.is_none() {
            let (v, x) = self.ed.cursor_visual(&segs);
            if v >= self.ed.scroll && v < self.ed.scroll + h {
                let x = (x as u16).min(r.width.saturating_sub(1));
                f.set_cursor_position(Position { x: r.x + x, y: r.y + (v - self.ed.scroll) as u16 });
            }
        }
    }

    fn key(&mut self, key: KeyEvent, cx: &mut Cx) -> bool {
        let typed = ui::typed_char(&key).is_some(); // AltGr chars (ctrl+alt on Windows) type
        if key.modifiers.contains(KeyModifiers::ALT) && !typed {
            return false;
        }
        if self.confirm_delete {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.delete_current(cx),
                _ => self.confirm_delete = false, // esc, or anything else, keeps the note
            }
            return true;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL) && !typed;
        if self.conflict {
            // theirs or mine? nothing else happens until you pick (so nothing is lost either way)
            match key.code {
                KeyCode::Char('r') | KeyCode::Char('R') if !ctrl => self.reload(),
                KeyCode::Char('s') if ctrl => self.write(true),
                _ => {}
            }
            return true;
        }
        if self.find.is_some() {
            self.find_key(key, cx);
            return true;
        }
        if ctrl {
            match key.code {
                KeyCode::Up | KeyCode::PageUp => {
                    self.step_note(-1);
                    return true;
                }
                KeyCode::Down | KeyCode::PageDown => {
                    self.step_note(1);
                    return true;
                }
                KeyCode::Char('f') => {
                    self.find = Some(Find { query: String::new(), sel: 0, content: vec![] });
                    return true;
                }
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

    fn paste(&mut self, text: &str, cx: &mut Cx) {
        if let Some(f) = &mut self.find {
            f.query.push_str(&text.replace(['\r', '\n'], " "));
            self.scan_find(cx);
            return;
        }
        if self.previewing || self.confirm_delete || self.conflict {
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
        if let Some(fd) = &self.find {
            // the finder's box sits in the gap above the list
            let q = format!("{}{}▏", ui::lead("search"), fd.query);
            f.render_widget(Paragraph::new(Span::styled(ui::fit(&q, area.width as usize), ui::accent(t))), Rect { y: area.y + 1, height: 1, ..area });
        }
        let list_area = Rect { y: area.y + 2, height: area.height.saturating_sub(2), ..area };
        let h = list_area.height as usize;
        if h == 0 {
            return;
        }
        let cur = self.cur_index();
        let rows = self.found();
        let (off, hl) = match &self.find {
            // finding: keep the highlighted match in view
            Some(fd) => (fd.sel.min(rows.len().saturating_sub(1)).saturating_sub(h - 1), rows.get(fd.sel).copied()),
            None => {
                // follow the open note only when it (or its place in the list) changes, so the wheel isn't undone
                let now = self.cur.clone().zip(cur);
                if now != self.side_follow {
                    if let Some(c) = cur {
                        if c < self.side_scroll {
                            self.side_scroll = c;
                        } else if c >= self.side_scroll + h {
                            self.side_scroll = c + 1 - h;
                        }
                    }
                    self.side_follow = now;
                }
                self.side_scroll = self.side_scroll.min(self.list.len().saturating_sub(h));
                (self.side_scroll, None)
            }
        };
        if rows.is_empty() && self.find.is_some() {
            f.render_widget(Paragraph::new(Span::styled("no note matches", ui::muted(t))), Rect { height: 1, ..list_area });
        }
        for (row, &i) in rows.iter().skip(off).take(h).enumerate() {
            let m = &self.list[i];
            let rr = Rect { y: list_area.y + row as u16, height: 1, ..list_area };
            let st = if hl == Some(i) {
                Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
            } else if cur == Some(i) {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            f.render_widget(Paragraph::new(Span::styled(ui::fit(&m.title, rr.width as usize), st)), rr);
            self.side_hits.push((rr, Some(i)));
        }
    }

    fn side_mouse(&mut self, ev: MouseEvent, area: Rect, _cx: &mut Cx) {
        let pos = Position { x: ev.column, y: ev.row };
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(&(_, hit)) = self.side_hits.iter().find(|(r, _)| r.contains(pos)) else { return };
                if self.conflict {
                    return; // pick theirs or yours first: switching now would drop your edits
                }
                self.confirm_delete = false;
                self.find = None;
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
        assert!(p.dirty_at.is_none() && p.tick_every() != Some(Duration::from_millis(250)), "clean: no autosave ticks");
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

    /// Write a note file "outside oriel" with a modified time `secs` from now (so it never ties with ours).
    fn outside(path: &Path, text: &str, secs: u64) {
        std::fs::write(path, text).unwrap();
        let t = SystemTime::now() + Duration::from_secs(secs);
        std::fs::File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
    }

    fn cx_for<'a>(k: &'a Kit, acts: &'a mut Vec<crate::pane::Action>) -> Cx<'a> {
        Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: acts, focused: true, time: 1.0 }
    }

    #[test]
    fn notes_sidebar_scrolls_and_keys_switch_notes() {
        let dir = scratch("notes-many");
        // 30 notes, "note 29" newest; one with a word only in its text
        for i in 0..30 {
            let body = if i == 7 { "the kumquat recipe" } else { "nothing much" };
            let path = dir.join(format!("n{i:02}.md"));
            std::fs::write(&path, format!("# note {i:02}\n\n{body}\n")).unwrap();
            let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + i * 60);
            std::fs::File::options().write(true).open(&path).unwrap().set_modified(t).unwrap();
        }
        let mut k = Kit::new();
        let mut p = Notes::open_in(dir, None);
        assert_eq!(p.title(), "note 29");
        let area = Rect::new(0, 0, 30, 12); // 10 rows of notes under "new note"
        k.render_side(&mut p, 30, 12);
        // the wheel scrolls the list and it stays scrolled (it used to snap back to the open note every frame)
        let wheel = MouseEvent { kind: MouseEventKind::ScrollDown, column: 3, row: 5, modifiers: KeyModifiers::NONE };
        let mut acts = vec![];
        for _ in 0..8 {
            p.side_mouse(wheel, area, &mut cx_for(&k, &mut acts));
        }
        let side = k.render_side(&mut p, 30, 12);
        assert_eq!(p.side_scroll, 8);
        assert!(side.contains("note 21") && side.contains("note 12") && !side.contains("note 29"), "{side}");
        let side = k.render_side(&mut p, 30, 12);
        assert!(side.contains("note 12"), "still scrolled on the next frame:\n{side}");
        // ctrl+down / ctrl+up open the next / previous note; the list follows the note that opened
        k.key_mod(&mut p, KeyCode::Down, KeyModifiers::CONTROL);
        assert_eq!(p.title(), "note 28");
        k.render_side(&mut p, 30, 12);
        assert_eq!(p.side_scroll, 1, "the newly opened note is scrolled into view");
        k.key_mod(&mut p, KeyCode::Up, KeyModifiers::CONTROL);
        k.key_mod(&mut p, KeyCode::Up, KeyModifiers::CONTROL); // already the first: stays
        assert_eq!(p.title(), "note 29");
        // ctrl+f finds by title right away...
        k.key_mod(&mut p, KeyCode::Char('f'), KeyModifiers::CONTROL);
        k.typ(&mut p, "note 0");
        let side = k.render_side(&mut p, 30, 14);
        assert!(side.contains("note 09") && side.contains("note 00") && !side.contains("note 10"), "{side}");
        assert!(k.render(&mut p, 120, 20).contains("find \"note 0\" · 10 found"), "the hint line says it too");
        let _ = snap(&mut k, &mut p, 120, 24, "target/snap/notes-find.html");
        assert_eq!(p.found().len(), 10);
        k.key(&mut p, KeyCode::Down);
        k.key(&mut p, KeyCode::Enter);
        assert_eq!(p.title(), "note 08");
        assert!(p.find.is_none());
        // ...and by text, from a background scan
        k.key_mod(&mut p, KeyCode::Char('f'), KeyModifiers::CONTROL);
        k.typ(&mut p, "kumquat");
        assert!(p.found().is_empty(), "no title has it");
        for _ in 0..20 {
            k.wait_wake(&mut p, 50);
            if !p.found().is_empty() {
                break;
            }
        }
        assert_eq!(p.found().len(), 1);
        k.key(&mut p, KeyCode::Enter);
        assert_eq!(p.title(), "note 07");
        // typing still goes to the note afterwards
        k.typ(&mut p, "x");
        assert!(p.dirty_at.is_some());
    }

    #[test]
    fn notes_pick_up_outside_changes() {
        let dir = scratch("notes-outside");
        let a = dir.join("a.md");
        std::fs::write(&a, "# a\n\nline one\nline two").unwrap();
        let mut k = Kit::new();
        let mut p = Notes::open_in(dir.clone(), None);
        let check = |k: &mut Kit, p: &mut Notes| {
            p.last_check = Instant::now() - DISK_CHECK;
            k.poll(p);
        };
        // an agent edits the open note while it's clean: it's loaded, the cursor stays on its row
        k.key(&mut p, KeyCode::Up);
        let row = p.ed.row;
        outside(&a, "# a\n\nline one, edited outside\nline two\nline three", 5);
        check(&mut k, &mut p);
        assert_eq!(p.ed.lines[2], "line one, edited outside");
        assert_eq!(p.ed.row, row);
        assert!(p.dirty_at.is_none() && !p.conflict);
        // a note made by another program shows up in the sidebar
        std::fs::write(dir.join("b.md"), "# from outside\n").unwrap();
        check(&mut k, &mut p);
        assert!(k.render_side(&mut p, 30, 10).contains("from outside"));

        // it changes while you have unsaved edits: nothing is overwritten, you're asked
        k.typ(&mut p, "!");
        outside(&a, "# a\n\ntheirs", 10);
        check(&mut k, &mut p);
        assert!(p.conflict);
        let s = k.render(&mut p, 120, 20);
        assert!(s.contains("changed on disk") && s.contains("r reload theirs") && s.contains("ctrl+s keep mine"), "{s}");
        let _ = snap(&mut k, &mut p, 150, 20, "target/snap/notes-conflict.html");
        p.dirty_at = Some(Instant::now() - Duration::from_secs(5));
        k.poll(&mut p); // autosave is held back
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "# a\n\ntheirs");
        k.typ(&mut p, "zz"); // and other keys wait for the answer
        assert!(!p.ed.text().contains("zz"));
        // r takes theirs
        k.key(&mut p, KeyCode::Char('r'));
        assert!(!p.conflict && p.dirty_at.is_none());
        assert_eq!(p.ed.text(), "# a\n\ntheirs");

        // ctrl+s keeps yours
        k.typ(&mut p, " + mine");
        outside(&a, "# a\n\ntheirs again", 15);
        check(&mut k, &mut p);
        assert!(p.conflict);
        k.key_mod(&mut p, KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(!p.conflict);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "# a\n\ntheirs + mine");

        // saving right after an outside change (before the check runs) asks instead of overwriting
        outside(&a, "# a\n\nsneaky", 20);
        k.typ(&mut p, "?");
        k.key_mod(&mut p, KeyCode::Down, KeyModifiers::CONTROL); // switching notes saves first: same question
        assert!(p.conflict);
        assert_eq!(p.cur.as_deref(), Some("a"), "stays on the note with the unsaved edit");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "# a\n\nsneaky");
        // closing oriel right then loses neither side: yours becomes a note of its own
        drop(p);
        let texts: Vec<String> = Notes::md_files(&dir).iter().map(|f| std::fs::read_to_string(f).unwrap()).collect();
        assert!(texts.iter().any(|t| t == "# a\n\nsneaky"));
        assert!(texts.iter().any(|t| t == "# a\n\ntheirs + mine?"), "{texts:?}");
    }

    #[test]
    fn notes_watcher_wakes_on_outside_changes() {
        let dir = scratch("notes-watch");
        let a = dir.join("a.md");
        std::fs::write(&a, "# a\n\nold").unwrap();
        let mut k = Kit::new();
        let mut p = Notes::open_in(dir.clone(), None);
        k.render(&mut p, 80, 10); // starts the watcher
        assert!(p.watch.is_some() || !cfg!(windows), "a local folder can always be watched on Windows");
        if p.watch.is_none() {
            return; // this folder can't be watched here: the polling fallback is covered by the other tests
        }
        assert_eq!(p.tick_every(), None, "idle: no ticks, the watcher wakes it");
        outside(&a, "# a\n\nnew from an agent", 5);
        let t0 = Instant::now();
        while p.ed.text() != "# a\n\nnew from an agent" && t0.elapsed() < Duration::from_secs(5) {
            k.wait_wake(&mut p, 50);
        }
        assert_eq!(p.ed.text(), "# a\n\nnew from an agent");
    }

    #[test]
    fn notes_deleted_outside() {
        let dir = scratch("notes-gone");
        let (a, b) = (dir.join("a.md"), dir.join("b.md"));
        std::fs::write(&b, "# b").unwrap();
        outside(&a, "# a", 5);
        let mut k = Kit::new();
        let mut p = Notes::open_in(dir.clone(), None);
        assert_eq!(p.cur.as_deref(), Some("a"));
        let check = |k: &mut Kit, p: &mut Notes| {
            p.last_check = Instant::now() - DISK_CHECK;
            k.poll(p);
        };
        // deleted while clean: let go, the next note opens (nothing is written back)
        std::fs::remove_file(&a).unwrap();
        check(&mut k, &mut p);
        assert_eq!(p.cur.as_deref(), Some("b"));
        assert!(!a.exists() && !p.conflict);
        // deleted under unsaved edits: asked, and the line says what happened
        k.key_mod(&mut p, KeyCode::End, KeyModifiers::CONTROL);
        k.typ(&mut p, "!");
        std::fs::remove_file(&b).unwrap();
        check(&mut k, &mut p);
        assert!(p.conflict);
        let s = k.render(&mut p, 150, 12);
        assert!(s.contains("was deleted outside oriel") && s.contains("r let it go"), "{s}");
        // ctrl+s keeps yours: it's written back
        k.key_mod(&mut p, KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(!p.conflict);
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "# b!");
        // r lets it go: a fresh note replaces an empty folder
        k.typ(&mut p, "?");
        std::fs::remove_file(&b).unwrap();
        check(&mut k, &mut p);
        assert!(p.conflict);
        k.key(&mut p, KeyCode::Char('r'));
        assert!(!p.conflict && p.dirty_at.is_none() && !b.exists());
        assert_eq!(p.title(), "new note");
    }

    #[test]
    fn notes_types_altgr_chars() {
        let dir = scratch("notes-altgr");
        std::fs::write(dir.join("a.md"), "x").unwrap();
        let mut k = Kit::new();
        let mut p = Notes::open_in(dir, None);
        for c in ['@', '{', '\\', '|', '~'] {
            k.key_mod(&mut p, KeyCode::Char(c), KeyModifiers::CONTROL | KeyModifiers::ALT);
        }
        assert_eq!(p.ed.text(), "x@{\\|~");
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

    /// A note written into the folder from outside (chat's /note, a sync tool) shows up on the next poll,
    /// without touching the note that's open.
    #[test]
    fn notes_sees_notes_added_from_outside() {
        let dir = scratch("notes-added");
        std::fs::write(dir.join("mine.md"), "# mine\n").unwrap();
        let mut k = Kit::new();
        let mut p = Notes::open_in(dir.clone(), None);
        assert_eq!(p.list.len(), 1);
        k.typ(&mut p, "!");
        std::thread::sleep(Duration::from_millis(30));
        std::fs::write(dir.join("from chat.md"), "# a saved reply\n\ntext\n").unwrap();
        k.poll(&mut p);
        assert_eq!(p.list.len(), 1, "not while you're typing");
        p.dirty_at = Some(Instant::now() - Duration::from_secs(2));
        p.last_check = Instant::now() - DISK_CHECK; // the watcher (or the next look at the disk) would say so
        k.poll(&mut p); // saves, then sees the new file
        assert!(p.list.iter().any(|m| m.title == "a saved reply"), "{:?}", p.list);
        assert_eq!(p.title(), "mine", "the open note stays open");
        assert!(std::fs::read_to_string(dir.join("mine.md")).unwrap().contains('!'), "and its edit was saved");
        assert!(k.render_side(&mut p, 30, 10).contains("a saved reply"));
    }

    /// A new notes folder from settings opens right away, after the note being typed is saved in the old one.
    #[test]
    fn notes_follow_a_new_notes_folder() {
        let (a, b) = (scratch("notes-follow-a"), scratch("notes-follow-b"));
        std::fs::write(a.join("old.md"), "# old\n").unwrap();
        std::fs::write(b.join("synced.md"), "# synced\n").unwrap();
        let mut k = Kit::new();
        let mut p = Notes::open_in(a.clone(), None);
        p.follow = true;
        k.typ(&mut p, "!");
        k.config.notes_folder = b.to_string_lossy().to_string();
        p.config_changed(&k.config);
        assert_eq!(p.dir, b);
        assert_eq!(p.title(), "synced");
        assert!(std::fs::read_to_string(a.join("old.md")).unwrap().contains('!'), "the edit was saved where it was typed");
        // the same folder again: nothing happens
        k.typ(&mut p, "?");
        p.config_changed(&k.config);
        assert!(p.dirty_at.is_some(), "still the same open note");
    }
}
