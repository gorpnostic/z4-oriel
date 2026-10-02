//! files: nest's files app. The folder on the left (folders first, icons by type), a live preview of whatever is
//! highlighted on the right: folders get a summary + their README, text/code its contents with light syntax
//! colouring, images a half-block pixel picture. Places (home, desktop, ..., drives) live in the sidebar.
//!
//! `/` filters the folder as you type; n / R / d make, rename and bin files (asking first); c / x / e / t open
//! Claude Code, Codex, your editor or a shell here, beside it. The folder is watched, so files something else
//! writes (an agent, a download) show up by themselves.
//!
//! Directory reads and previews run on background threads that hand results back through `Shared` and wake
//! the pane; render only draws what is already in memory.

pub mod clock;
pub(crate) mod preview;

use crate::pane::{Action, Cx, Pane, Place, Waker};
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use preview::{Body, Entry, Kind, PlaceItem, Preview, Tok};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
struct Shared {
    listing: Option<(u64, Result<Vec<Entry>, String>)>,
    preview: Option<(u64, Preview)>,
    places: Option<Vec<PlaceItem>>,
    /// A background file operation (the recycle bin) finished: what to say.
    op: Option<Result<String, String>>,
}

/// A name being typed in the little dialog.
enum Ask {
    /// a new file, or a folder when it ends in /
    New(String),
    /// (the old name, the new one so far)
    Rename(String, String),
}

/// What the files app opens beside itself, in the folder it shows.
pub(crate) enum Launch {
    Shell,
    Agent(&'static str),
    Edit(PathBuf),
}

/// The folder is read again at most this often while it keeps changing (a build writing hundreds of files).
const REFRESH_GAP: Duration = Duration::from_millis(500);

struct Req {
    ticket: u64,
    path: PathBuf,
    size: (u16, u16),
    /// `.` is on: a folder's preview lists hidden entries too
    hidden: bool,
}

pub struct Files {
    dir: PathBuf,
    hidden: bool,
    all: Vec<Entry>,
    /// Indexes into `all` that pass the hidden filter.
    shown: Vec<usize>,
    /// 0 = the folder itself (top row), n = shown[n - 1].
    sel: usize,
    scroll: usize,
    follow: bool,
    loading: bool,
    error: Option<String>,
    want_sel: Option<String>,
    preview: Option<Preview>,
    pscroll: usize,
    focus_preview: bool,
    places: Vec<PlaceItem>,
    shared: Arc<Mutex<Shared>>,
    worker: Option<Sender<Req>>,
    waker: Option<Waker>,
    list_gen: u64,
    prev_gen: u64,
    prev_path: Option<PathBuf>,
    /// Preview area size (cols, rows) from the last render, for sizing image previews.
    pv_size: (u16, u16),
    list_rect: Rect,
    pv_rect: Rect,
    side_hits: Vec<(Rect, usize)>,
    last_click: Option<(usize, Instant)>,
    /// The `/` filter: Some while one is set (even empty), `filtering` while you type into it.
    filter: Option<String>,
    filtering: bool,
    ask: Option<Ask>,
    /// "move <path> to the recycle bin?", waiting for y.
    confirm: Option<PathBuf>,
    /// Makes the panes it opens (tests swap in one that starts nothing).
    launcher: fn(&Launch, &Path, &crate::config::Config) -> Result<Box<dyn Pane>, String>,
    /// Moves a file to the recycle bin (tests swap in one that just deletes it in the scratch folder).
    trasher: fn(&Path) -> Result<(), String>,
    /// Set by the folder watcher when something changed; the next poll reads the folder again.
    changed: Arc<AtomicBool>,
    watch: Option<(PathBuf, notify::RecommendedWatcher)>,
    last_load: Instant,
    /// To notice the pane coming back into view (it reads the folder again then).
    was_focused: bool,
    last_render: Instant,
}

// Glyphs the shared icon table doesn't have (Nerd Font Material Design), with plain fallbacks.
fn glyph(name: &str) -> &'static str {
    let nerd = ui::NERD.load(std::sync::atomic::Ordering::Relaxed);
    let (g, plain) = match name {
        "folder" => ("\u{F024B}", "/"),
        "folder_open" => ("\u{F0770}", "/"),
        "desktop" => ("\u{F0379}", "#"),
        "downloads" => ("\u{F01DA}", "v"),
        "documents" => return ui::icon("doc"),
        "home" => return ui::icon("home"),
        "code" => return ui::icon("code"),
        "drive" => return ui::icon("storage"),
        _ => return ui::icon("file"),
    };
    if nerd { g } else { plain }
}

fn kind_glyph(k: Kind) -> &'static str {
    match k {
        Kind::Folder => glyph("folder"),
        Kind::Code => ui::icon("code"),
        Kind::Doc => ui::icon("doc"),
        Kind::Image => ui::icon("image"),
        Kind::Audio => ui::icon("audio"),
        Kind::Video => ui::icon("video"),
        Kind::Archive => ui::icon("archive"),
        Kind::File => ui::icon("file"),
    }
}

/// Path for display: home folder as "~".
fn pretty(p: &Path) -> String {
    let s = p.to_string_lossy().to_string();
    if let Some(h) = dirs::home_dir() {
        let hs = h.to_string_lossy().to_string();
        if !hs.is_empty() && (s == hs || s.starts_with(&format!("{hs}{}", std::path::MAIN_SEPARATOR))) {
            return format!("~{}", &s[hs.len()..]);
        }
    }
    s
}

impl Files {
    pub fn new(dir: Option<PathBuf>) -> Self {
        let dir = dir
            .filter(|d| d.is_dir())
            .or_else(|| std::env::current_dir().ok())
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        let dir = std::path::absolute(&dir).unwrap_or(dir);
        Files {
            dir,
            hidden: false,
            all: vec![],
            shown: vec![],
            sel: 0,
            scroll: 0,
            follow: true,
            loading: true,
            error: None,
            want_sel: None,
            preview: None,
            pscroll: 0,
            focus_preview: false,
            places: vec![],
            shared: Arc::default(),
            worker: None,
            waker: None,
            list_gen: 0,
            prev_gen: 0,
            prev_path: None,
            pv_size: (60, 30),
            list_rect: Rect::default(),
            pv_rect: Rect::default(),
            side_hits: vec![],
            last_click: None,
            filter: None,
            filtering: false,
            ask: None,
            confirm: None,
            launcher: launch,
            trasher: preview::to_trash,
            changed: Arc::default(),
            watch: None,
            last_load: Instant::now(),
            was_focused: true,
            last_render: Instant::now(),
        }
    }

    /// First contact with a Cx: start the preview worker, read the folder, find the places.
    fn start(&mut self, cx: &Cx) {
        if self.waker.is_some() {
            return;
        }
        let waker = cx.waker();
        self.waker = Some(waker.clone());
        let (tx, rx) = channel::<Req>();
        let shared = self.shared.clone();
        let w = waker.clone();
        std::thread::spawn(move || {
            while let Ok(mut req) = rx.recv() {
                while let Ok(newer) = rx.try_recv() {
                    req = newer; // scrolled past: only the latest one matters
                }
                let p = preview::build(&req.path, req.size.0, req.size.1, req.hidden);
                shared.lock().unwrap().preview = Some((req.ticket, p));
                w.wake();
            }
        });
        self.worker = Some(tx);
        let shared = self.shared.clone();
        std::thread::spawn(move || {
            let mut p = preview::places();
            shared.lock().unwrap().places = Some(p.clone());
            waker.wake();
            p.extend(preview::drives());
            shared.lock().unwrap().places = Some(p);
            waker.wake();
        });
        self.load();
    }

    fn load(&mut self) {
        self.list_gen += 1;
        self.loading = true;
        self.last_load = Instant::now();
        let Some(waker) = self.waker.clone() else { return };
        self.watch_dir(&waker);
        let (ticket, dir, shared) = (self.list_gen, self.dir.clone(), self.shared.clone());
        std::thread::spawn(move || {
            let r = preview::list_dir(&dir);
            offer(&mut shared.lock().unwrap().listing, ticket, r);
            waker.wake();
        });
    }

    /// Watch the folder on screen (not its subfolders): a change wakes the pane, which reads it again.
    fn watch_dir(&mut self, waker: &Waker) {
        use notify::event::{EventKind, MetadataKind, ModifyKind};
        use notify::{RecursiveMode, Watcher};
        if self.watch.as_ref().is_some_and(|(d, _)| *d == self.dir) {
            return;
        }
        self.watch = None;
        let (changed, w) = (self.changed.clone(), waker.clone());
        let made = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            // reads (our own previews) aren't changes; a burst of writes wakes us once, the poll clears the flag
            let change = res.is_ok_and(|e| !e.kind.is_access() && !matches!(e.kind, EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime))));
            if change && !changed.swap(true, Ordering::SeqCst) {
                w.wake();
            }
        });
        if let Ok(mut watcher) = made {
            if watcher.watch(&self.dir, RecursiveMode::NonRecursive).is_ok() {
                self.watch = Some((self.dir.clone(), watcher));
            }
        }
    }

    /// Read the folder again, keeping the selection: something changed on disk, or the pane is back in view.
    /// (The entry to keep is taken when the new listing lands, not now: you may move on while it's read.)
    fn refresh(&mut self) {
        if self.waker.is_none() {
            return;
        }
        self.changed.store(false, Ordering::SeqCst);
        self.load();
    }

    fn navigate(&mut self, dir: PathBuf, want_sel: Option<String>) {
        self.dir = dir;
        self.filter = None;
        self.filtering = false;
        self.all.clear();
        self.shown.clear();
        self.error = None;
        self.sel = 0;
        self.scroll = 0;
        self.follow = true;
        self.focus_preview = false;
        self.want_sel = want_sel;
        self.load();
        self.request_preview(false);
    }

    fn up(&mut self) {
        let Some(parent) = self.dir.parent().map(Path::to_path_buf) else { return };
        let name = self.dir.file_name().map(|n| n.to_string_lossy().to_string());
        self.navigate(parent, name);
    }

    fn refilter(&mut self) {
        let keep = self.sel_entry().map(|e| e.name.clone());
        let q = self.filter.as_deref().unwrap_or("").to_lowercase();
        self.shown = (0..self.all.len()).filter(|&i| (self.hidden || !self.all[i].hidden) && (q.is_empty() || self.all[i].name.to_lowercase().contains(&q))).collect();
        self.sel = keep.and_then(|n| self.shown.iter().position(|&i| self.all[i].name == n).map(|p| p + 1)).unwrap_or(0);
        self.follow = true;
    }

    /// The filter text changed: the list narrows and the cursor goes to the top match (enter opens it).
    fn filter_changed(&mut self) {
        self.refilter();
        self.sel = if self.shown.is_empty() { 0 } else { 1 };
        self.scroll = 0;
        self.request_preview(false);
    }

    fn start_filter(&mut self) {
        self.focus_preview = false;
        self.filtering = true;
        self.filter.get_or_insert_with(String::new);
    }

    /// Keys while typing into the filter. Arrows move, enter opens, esc drops the filter.
    fn filter_key(&mut self, key: KeyEvent, typed: Option<char>) -> bool {
        match key.code {
            KeyCode::Esc => {
                self.filter = None;
                self.filtering = false;
                self.refilter();
                self.request_preview(false);
            }
            KeyCode::Enter => {
                self.filtering = false;
                if self.sel > 0 {
                    self.open_sel();
                }
            }
            KeyCode::Backspace => {
                match self.filter.as_mut().map(|f| f.pop()) {
                    Some(Some(_)) => self.filter_changed(),
                    _ => {
                        // backspace on an empty filter: done filtering
                        self.filter = None;
                        self.filtering = false;
                        self.refilter();
                    }
                }
            }
            KeyCode::Down => self.select(self.sel + 1),
            KeyCode::Up => self.select(self.sel.saturating_sub(1)),
            KeyCode::PageDown | KeyCode::PageUp | KeyCode::Home | KeyCode::End => return false,
            _ => match typed {
                Some(c) => {
                    self.filter.get_or_insert_with(String::new).push(c);
                    self.filter_changed();
                }
                None => return false,
            },
        }
        true
    }

    fn rows(&self) -> usize {
        self.shown.len() + 1
    }

    fn sel_entry(&self) -> Option<&Entry> {
        if self.sel == 0 { None } else { self.shown.get(self.sel - 1).map(|&i| &self.all[i]) }
    }

    fn sel_path(&self) -> PathBuf {
        match self.sel_entry() {
            Some(e) => self.dir.join(&e.name),
            None => self.dir.clone(),
        }
    }

    fn request_preview(&mut self, force: bool) {
        let path = self.sel_path();
        if !force && self.prev_path.as_ref() == Some(&path) {
            return;
        }
        self.prev_path = Some(path.clone());
        self.prev_gen += 1;
        self.pscroll = 0;
        if let Some(w) = &self.worker {
            let _ = w.send(Req { ticket: self.prev_gen, path, size: self.pv_size, hidden: self.hidden });
        }
    }

    fn select(&mut self, n: usize) {
        self.sel = n.min(self.rows() - 1);
        self.follow = true;
        self.request_preview(false);
    }

    fn open_sel(&mut self) {
        match self.sel_entry() {
            Some(e) if e.is_dir => {
                let d = self.dir.join(&e.name);
                self.navigate(d, None);
            }
            _ => {
                // a file (or the folder row): look at its preview
                self.focus_preview = true;
            }
        }
    }

    fn counts(&self) -> (usize, usize) {
        let d = self.shown.iter().filter(|&&i| self.all[i].is_dir).count();
        (d, self.shown.len() - d)
    }

    fn os_name() -> &'static str {
        if cfg!(windows) { "windows" } else { "system" }
    }

    // ------------------------------------------------------------------ drawing

    fn draw_list(&mut self, f: &mut Frame, r: Rect, cx: &Cx) {
        let t = cx.theme;
        self.list_rect = r;
        let h = r.height as usize;
        if h == 0 {
            return;
        }
        if self.follow {
            if self.sel < self.scroll {
                self.scroll = self.sel;
            } else if self.sel >= self.scroll + h {
                self.scroll = self.sel + 1 - h;
            }
            self.follow = false;
        }
        self.scroll = self.scroll.min(self.rows().saturating_sub(h));
        let sel_bg = if self.focus_preview || !cx.focused { t.frame } else { t.user };
        let w = r.width as usize;
        let show_size = w >= 38;
        for row in 0..h {
            let n = self.scroll + row;
            let y = r.y + row as u16;
            let rr = Rect { y, height: 1, ..r };
            let on = n == self.sel;
            let mut spans: Vec<Span> = vec![];
            let mut right = String::new();
            let hl_w: usize;
            if n == 0 {
                let name = self.dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| self.dir.to_string_lossy().to_string());
                spans.push(Span::styled(format!("{} ", glyph("folder_open")), Style::default().fg(t.accent)));
                hl_w = UnicodeWidthStr::width(name.as_str());
                spans.push(Span::styled(name, Style::default().add_modifier(Modifier::BOLD)));
            } else if let Some(&i) = self.shown.get(n - 1) {
                let e = &self.all[i];
                let last = n == self.shown.len();
                spans.push(Span::styled(if last { "└── " } else { "├── " }, Style::default().fg(t.frame)));
                let icol = if e.is_dir { t.accent } else { t.muted };
                spans.push(Span::styled(format!("{} ", kind_glyph(e.kind)), Style::default().fg(icol)));
                let avail = w.saturating_sub(7 + if show_size && !e.is_dir { 10 } else { 0 });
                let name = ui::fit(&e.name, avail);
                hl_w = UnicodeWidthStr::width(name.as_str());
                if e.is_dir {
                    spans.push(Span::styled(name, Style::default().add_modifier(Modifier::BOLD)));
                } else {
                    let dot = name.rfind('.').filter(|&d| d > 0 && !name.ends_with('…'));
                    match dot {
                        Some(d) => {
                            spans.push(Span::raw(name[..d].to_string()));
                            spans.push(Span::styled(name[d..].to_string(), Style::default().fg(t.muted).add_modifier(Modifier::ITALIC)));
                        }
                        None => spans.push(Span::raw(name)),
                    }
                    if show_size {
                        right = preview::human(e.size);
                    }
                }
                if e.hidden {
                    for s in spans.iter_mut().skip(2) {
                        s.style = s.style.add_modifier(Modifier::DIM);
                    }
                }
            } else if n == 1 && row < h {
                let msg = if self.loading {
                    Span::styled("    loading…", ui::muted(t))
                } else if let Some(e) = &self.error {
                    Span::styled(format!("    {e}"), Style::default().fg(t.danger))
                } else if self.filter.as_deref().is_some_and(|q| !q.is_empty()) {
                    Span::styled("    nothing matches · esc clears the filter", ui::muted(t))
                } else {
                    Span::styled("    (empty)", ui::muted(t))
                };
                f.render_widget(Paragraph::new(Line::from(msg)), rr);
                continue;
            } else {
                continue;
            }
            f.render_widget(Paragraph::new(Line::from(spans)), rr);
            if !right.is_empty() {
                let rw = right.len() as u16 + 1;
                f.render_widget(Paragraph::new(Span::styled(right, ui::muted(t))), Rect { x: r.right().saturating_sub(rw), width: rw, ..rr });
            }
            if on {
                // highlight just the name, like nest's tree cursor
                let x0 = if n == 0 { r.x + 2 } else { r.x + 6 };
                let x1 = (x0 + hl_w as u16 + 1).min(r.right());
                let buf = f.buffer_mut();
                for x in x0.saturating_sub(1)..x1 {
                    buf[(x, y)].set_bg(sel_bg);
                    if x >= x0 {
                        buf[(x, y)].set_fg(t.fg); // the dim extension stays readable on the bar
                    }
                }
            }
        }
    }

    fn tok_style(&self, tk: Tok, cx: &Cx) -> Style {
        let t = cx.theme;
        match tk {
            Tok::Plain => Style::default(),
            Tok::Kw => Style::default().fg(t.accent),
            Tok::Str => Style::default().fg(t.inline),
            Tok::Com => Style::default().fg(t.muted).add_modifier(Modifier::ITALIC),
            Tok::Num => Style::default().fg(t.good),
            Tok::Func => Style::default().fg(t.shine),
            Tok::Head => Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
            Tok::Bold => Style::default().add_modifier(Modifier::BOLD),
        }
    }

    fn draw_preview(&mut self, f: &mut Frame, r: Rect, cx: &mut Cx) {
        let t = cx.theme;
        self.pv_rect = r;
        let size = (r.width.saturating_sub(1), r.height.saturating_sub(4));
        if size != self.pv_size {
            self.pv_size = size;
            if matches!(self.preview.as_ref().map(|p| &p.body), Some(Body::Image { .. })) {
                self.request_preview(true);
            }
        }
        let Some(p) = &self.preview else {
            f.render_widget(Paragraph::new(Span::styled("…", ui::muted(t))), r);
            return;
        };
        let mut lines: Vec<Line> = vec![
            Line::from(Span::styled(p.name.clone(), ui::bold_accent(t))),
            Line::from(Span::styled(p.meta.clone(), ui::muted(t))),
            Line::raw(""),
        ];
        let head = lines.len() as u16;
        let ts = |tk: Tok| self.tok_style(tk, cx);
        let tl = |l: &Vec<(Tok, String)>| Line::from(l.iter().map(|(tk, s)| Span::styled(s.clone(), ts(*tk))).collect::<Vec<_>>());
        let body_h = r.height.saturating_sub(head) as usize;
        let mut wrap = true;
        let mut pixels: Option<&Vec<Vec<([u8; 3], [u8; 3])>>> = None;
        let max_scroll;
        match &p.body {
            Body::Folder { readme: Some((name, rl)), .. } => {
                lines.push(Line::from(Span::styled(name.clone(), ui::bold_accent(t))));
                max_scroll = rl.len().saturating_sub(1);
                let s = self.pscroll.min(max_scroll);
                lines.extend(rl.iter().skip(s).take(body_h).map(tl));
            }
            Body::Folder { names, .. } => {
                max_scroll = names.len().saturating_sub(1);
                for (is_dir, n) in names.iter().skip(self.pscroll.min(max_scroll)).take(body_h) {
                    lines.push(if *is_dir {
                        Line::from(Span::styled(format!("{} {n}", glyph("folder")), Style::default().fg(t.accent)))
                    } else {
                        Line::from(format!("  {n}"))
                    });
                }
            }
            Body::Text { lines: tls, numbered, more } => {
                max_scroll = tls.len().saturating_sub(1);
                let s = self.pscroll.min(max_scroll);
                if *numbered {
                    wrap = false;
                    let nw = (tls.len() + 1).to_string().len();
                    for (k, l) in tls.iter().enumerate().skip(s).take(body_h) {
                        let mut spans = vec![Span::styled(format!("{:>nw$} ", k + 1), Style::default().fg(t.muted).add_modifier(Modifier::DIM))];
                        spans.extend(l.iter().map(|(tk, s)| Span::styled(s.clone(), ts(*tk))));
                        lines.push(Line::from(spans));
                    }
                } else {
                    lines.extend(tls.iter().skip(s).take(body_h).map(tl));
                }
                if *more && s + body_h >= tls.len() {
                    lines.push(Line::from(Span::styled("… (first 400 lines)", ui::muted(t))));
                }
            }
            Body::Image { w, h, rows } => {
                max_scroll = 0;
                lines.push(Line::from(Span::styled(format!("{w}×{h}"), ui::muted(t))));
                pixels = Some(rows);
            }
            Body::Binary => {
                max_scroll = 0;
                lines.push(Line::from(Span::styled(format!("binary file · o opens it with {}", Self::os_name()), ui::muted(t))));
            }
            Body::Error(e) => {
                max_scroll = 0;
                lines.push(Line::from(Span::styled(e.clone(), Style::default().fg(t.danger))));
            }
        }
        let n_lines = lines.len() as u16;
        let mut para = Paragraph::new(lines);
        if wrap {
            para = para.wrap(Wrap { trim: false });
        }
        f.render_widget(para, r);
        if let Some(rows) = pixels {
            let y0 = r.y + n_lines;
            let buf = f.buffer_mut();
            for (dy, row) in rows.iter().enumerate() {
                let y = y0 + dy as u16;
                if y >= r.bottom() {
                    break;
                }
                for (dx, (top, bot)) in row.iter().enumerate() {
                    let x = r.x + dx as u16;
                    if x >= r.right() {
                        break;
                    }
                    let c = &mut buf[(x, y)];
                    c.set_symbol("▀");
                    c.set_fg(Color::Rgb(top[0], top[1], top[2]));
                    c.set_bg(Color::Rgb(bot[0], bot[1], bot[2]));
                }
            }
        }
        self.pscroll = self.pscroll.min(max_scroll);
    }
}

impl Pane for Files {
    fn title(&self) -> String {
        pretty(&self.dir)
    }
    fn subtitle(&self) -> Option<String> {
        if self.loading && self.all.is_empty() {
            return Some("reading…".into());
        }
        if let Some(q) = self.filter.as_deref().filter(|q| !q.is_empty()) {
            let total = self.all.iter().filter(|e| self.hidden || !e.hidden).count();
            return Some(format!("{} of {total} match “{q}”", self.shown.len()));
        }
        let (d, fcount) = self.counts();
        let mut s = format!("{d} {} · {fcount} {}", if d == 1 { "folder" } else { "folders" }, if fcount == 1 { "file" } else { "files" });
        if self.hidden {
            s.push_str(" · hidden shown");
        }
        Some(s)
    }
    fn icon(&self) -> &'static str {
        "files"
    }
    fn cwd(&self) -> Option<PathBuf> {
        Some(self.dir.clone())
    }
    fn reopen(&self) -> Option<&'static str> {
        Some("files")
    }

    fn poll(&mut self, cx: &mut Cx) {
        self.start(cx);
        let (listing, prev, places, op) = {
            let mut s = self.shared.lock().unwrap();
            (s.listing.take(), s.preview.take(), s.places.take(), s.op.take())
        };
        if let Some((ticket, r)) = listing {
            if ticket == self.list_gen {
                self.loading = false;
                let was = self.sel_entry().map(|e| (e.name.clone(), e.size));
                let old_sel = self.sel;
                match r {
                    Ok(all) => {
                        self.all = all;
                        self.error = None;
                    }
                    Err(e) => {
                        self.all.clear();
                        self.error = Some(e);
                    }
                }
                self.shown.clear(); // its indices were into the old list
                self.refilter();
                // what asked for this listing picked a name (the folder we came up from, a new file), else the
                // cursor stays on whatever it's on now
                let want = self.want_sel.take().or_else(|| was.as_ref().map(|w| w.0.clone()));
                match want.and_then(|w| self.shown.iter().position(|&i| self.all[i].name == w)) {
                    Some(p) => self.sel = p + 1,
                    // the one that was picked is gone (deleted, renamed): the cursor stays where it was
                    None if was.is_some() => self.sel = old_sel.min(self.rows() - 1),
                    None => {}
                }
                // the file on show changed on disk (an agent writing it): show it again
                let now = self.sel_entry().map(|e| (e.name.clone(), e.size));
                let grew = matches!((&was, &now), (Some(a), Some(b)) if a.0 == b.0 && a.1 != b.1);
                self.request_preview(grew);
            }
        }
        if let Some(r) = op {
            cx.notify(r.unwrap_or_else(|e| e));
            self.refresh();
        }
        // the watcher saw a change: read the folder again (at most every REFRESH_GAP; a tick comes back for it)
        if self.changed.load(Ordering::SeqCst) && !self.loading && self.last_load.elapsed() >= REFRESH_GAP {
            self.refresh();
        }
        if let Some((ticket, p)) = prev {
            if ticket == self.prev_gen {
                self.preview = Some(p);
            }
        }
        if let Some(p) = places {
            self.places = p;
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        self.start(cx);
        // back in view (from another tab or pane) with no watcher on the folder (some network drives): read it
        // again, something may have changed meanwhile. A watcher already caught every change, hidden or not.
        let back = cx.focused && (!self.was_focused || self.last_render.elapsed() > Duration::from_secs(2));
        (self.was_focused, self.last_render) = (cx.focused, Instant::now());
        if back && self.watch.is_none() && !self.loading && self.last_load.elapsed() > Duration::from_secs(1) {
            self.refresh();
        }
        let t = cx.theme;
        let os = format!("open with {}", Self::os_name());
        let bin = format!("yes, move it to {}", preview::bin_name());
        let hints: Vec<(&str, &str)> = if self.confirm.is_some() {
            vec![("y", &bin), ("esc", "no")]
        } else if let Some(a) = &self.ask {
            vec![("enter", if matches!(a, Ask::New(_)) { "create" } else { "rename" }), ("esc", "cancel")]
        } else if self.filtering {
            vec![("type", "to filter"), ("↑↓", "move"), ("enter", "open"), ("esc", "clear")]
        } else if self.focus_preview {
            vec![("j/k", "scroll"), ("esc", "back to the list"), ("e", "edit"), ("o", &os), ("p", "copy path"), ("t", "terminal here")]
        } else {
            vec![("enter", "open"), ("/", "filter"), ("e", "edit"), ("n", "new"), ("R", "rename"), ("d", "delete"), ("c/x", "claude/codex here"), ("t", "terminal here"), ("a", "ask chat"), ("o", &os), ("p", "copy path"), (".", "hidden"), ("backspace", "up")]
        };
        let body = ui::hint_line(f, area, &hints, t);
        let body = Rect { height: body.height.saturating_sub(1), ..body }; // a gap above the hints, like nest
        if body.width < 50 {
            // narrow: one or the other
            if self.focus_preview {
                self.list_rect = Rect::default();
                self.draw_preview(f, body, cx);
            } else {
                self.pv_rect = Rect::default();
                let list = self.draw_filter(f, body, cx);
                self.draw_list(f, list, cx);
            }
            self.draw_dialogs(f, area, cx);
            return;
        }
        let lw = (body.width * 2 / 5).clamp(26, 60);
        let list = self.draw_filter(f, Rect { width: lw, ..body }, cx);
        self.draw_list(f, list, cx);
        let t = cx.theme;
        let div_x = body.x + lw + 1;
        for y in body.y..body.bottom() {
            f.buffer_mut()[(div_x, y)].set_symbol("│").set_fg(t.frame);
        }
        let pv = Rect { x: div_x + 2, width: body.right().saturating_sub(div_x + 3), ..body };
        self.draw_preview(f, pv, cx);
        self.draw_dialogs(f, area, cx);
    }

    fn tick_every(&self) -> Option<Duration> {
        // the watcher saw a change too soon after the last read: come back for it
        if self.changed.load(Ordering::SeqCst) { Some(REFRESH_GAP) } else { None }
    }

    fn key(&mut self, key: KeyEvent, cx: &mut Cx) -> bool {
        self.start(cx);
        let typed = ui::typed_char(&key); // AltGr chars like ~ count
        if self.confirm.is_some() {
            // storage's rule: y does it, any other key leaves it
            if matches!(typed, Some('y') | Some('Y')) {
                self.trash_confirmed();
            } else {
                self.confirm = None;
            }
            return true;
        }
        if self.ask.is_some() {
            self.ask_key(key, typed, cx);
            return true;
        }
        if self.filtering && self.filter_key(key, typed) {
            return true;
        }
        if key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) && typed.is_none() {
            return false;
        }
        let page = (self.list_rect.height.max(2) - 1) as usize;
        if self.focus_preview {
            let ppage = (self.pv_rect.height.max(6) - 4) as usize;
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => self.pscroll += 1,
                KeyCode::Char('k') | KeyCode::Up => self.pscroll = self.pscroll.saturating_sub(1),
                KeyCode::PageDown | KeyCode::Char(' ') => self.pscroll += ppage,
                KeyCode::PageUp => self.pscroll = self.pscroll.saturating_sub(ppage),
                KeyCode::Home | KeyCode::Char('g') => self.pscroll = 0,
                KeyCode::End | KeyCode::Char('G') => self.pscroll = usize::MAX / 2,
                KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('h') | KeyCode::Left | KeyCode::Enter | KeyCode::Char('q') => {
                    self.focus_preview = false
                }
                _ => return self.common_key(key, cx),
            }
            return true;
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.select(self.sel + 1),
            KeyCode::Char('k') | KeyCode::Up => self.select(self.sel.saturating_sub(1)),
            KeyCode::PageDown => self.select(self.sel + page),
            KeyCode::PageUp => self.select(self.sel.saturating_sub(page)),
            KeyCode::Home | KeyCode::Char('g') => self.select(0),
            KeyCode::End | KeyCode::Char('G') => self.select(usize::MAX / 2),
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => self.open_sel(),
            KeyCode::Backspace | KeyCode::Char('h') | KeyCode::Left => self.up(),
            KeyCode::Esc if self.filter.is_some() => {
                self.filter = None;
                self.refilter();
                self.request_preview(false);
            }
            _ => return self.common_key(key, cx),
        }
        true
    }

    fn mouse(&mut self, ev: MouseEvent, _area: Rect, _cx: &mut Cx) {
        let pos = Position { x: ev.column, y: ev.row };
        let in_list = self.list_rect.contains(pos);
        let in_pv = self.pv_rect.contains(pos);
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) if in_list => {
                let n = self.scroll + (ev.row - self.list_rect.y) as usize;
                if n >= self.rows() {
                    return;
                }
                self.focus_preview = false;
                let double = matches!(self.last_click, Some((m, at)) if m == n && at.elapsed().as_millis() < 450);
                self.select(n);
                if double {
                    self.last_click = None;
                    self.open_sel();
                } else {
                    self.last_click = Some((n, Instant::now()));
                }
            }
            MouseEventKind::Down(MouseButton::Left) if in_pv => self.focus_preview = true,
            MouseEventKind::ScrollDown if in_list => {
                self.scroll = (self.scroll + 3).min(self.rows().saturating_sub(self.list_rect.height as usize));
            }
            MouseEventKind::ScrollUp if in_list => self.scroll = self.scroll.saturating_sub(3),
            MouseEventKind::ScrollDown if in_pv => self.pscroll += 3,
            MouseEventKind::ScrollUp if in_pv => self.pscroll = self.pscroll.saturating_sub(3),
            _ => {}
        }
    }

    fn side(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        self.side_hits.clear();
        // the place the current folder lives in (longest matching prefix) is highlighted
        let cur = self
            .places
            .iter()
            .enumerate()
            .filter(|(_, p)| self.dir.starts_with(&p.path))
            .max_by_key(|(_, p)| p.path.components().count())
            .map(|(i, _)| i);
        for (i, p) in self.places.iter().enumerate() {
            let y = area.y + i as u16;
            if y >= area.bottom() {
                break;
            }
            let r = Rect { y, height: 1, ..area };
            let g = glyph(p.kind);
            let label = if g.is_empty() { p.label.clone() } else { format!("{g} {}", p.label) };
            ui::side_row(f, r, "", &label, "", cur == Some(i), t);
            self.side_hits.push((r, i));
        }
        if self.places.is_empty() {
            f.render_widget(Paragraph::new(Span::styled("…", ui::muted(t))), Rect { height: 1, ..area });
        }
    }

    fn side_mouse(&mut self, ev: MouseEvent, _area: Rect, cx: &mut Cx) {
        self.start(cx);
        if let MouseEventKind::Down(MouseButton::Left) = ev.kind {
            let pos = Position { x: ev.column, y: ev.row };
            if let Some(&(_, i)) = self.side_hits.iter().find(|(r, _)| r.contains(pos)) {
                if let Some(p) = self.places.get(i) {
                    let path = p.path.clone();
                    self.navigate(path, None);
                }
            }
        }
    }
}

impl Files {
    /// Keys that work the same whether the list or the preview has focus.
    fn common_key(&mut self, key: KeyEvent, cx: &mut Cx) -> bool {
        match key.code {
            KeyCode::Char('o') => {
                let p = self.sel_path();
                match preview::open_external(&p) {
                    Ok(()) => cx.notify(format!("{}opening {}", ui::lead("files"), pretty(&p))),
                    Err(e) => cx.notify(format!("can't open: {e}")),
                }
            }
            KeyCode::Char('p') => {
                let p = self.sel_path().to_string_lossy().to_string();
                match preview::copy_to_clipboard(&p) {
                    Ok(()) => cx.notify(format!("copied {p}")),
                    Err(e) => cx.notify(format!("can't copy: {e}")),
                }
            }
            KeyCode::Char('t') => self.launch(Launch::Shell, cx),
            // ask chat about it: the path goes into chat's box (Claude Code and Codex read the file themselves)
            KeyCode::Char('a') => {
                let p = self.sel_path().to_string_lossy().to_string();
                cx.act(Action::AppPaste("ai", format!("{} ", crate::clip::paste_form(&[p]))));
            }
            KeyCode::Char('c') => self.launch(Launch::Agent("claude"), cx),
            KeyCode::Char('x') => self.launch(Launch::Agent("codex"), cx),
            KeyCode::Char('e') => match self.sel_entry() {
                Some(e) if !e.is_dir => {
                    let path = self.dir.join(&e.name);
                    self.launch(Launch::Edit(path), cx);
                }
                _ => cx.notify("pick a file to edit (e opens it in your editor beside this)"),
            },
            KeyCode::Char('/') => self.start_filter(),
            KeyCode::Char('n') => self.ask = Some(Ask::New(String::new())),
            KeyCode::Char('R') | KeyCode::F(2) => match self.sel_entry() {
                Some(e) => self.ask = Some(Ask::Rename(e.name.clone(), e.name.clone())),
                None => cx.notify("pick a file or folder to rename"),
            },
            KeyCode::Char('d') | KeyCode::Delete => match self.sel_entry() {
                Some(e) => self.confirm = Some(self.dir.join(&e.name)),
                None => cx.notify("pick a file or folder to delete (the top row is this folder itself)"),
            },
            KeyCode::Char('.') => {
                self.hidden = !self.hidden;
                self.refilter();
                self.request_preview(true); // a folder's preview lists what the filter lets through
            }
            // (not F5: that's the system app everywhere)
            KeyCode::Char('r') => {
                self.want_sel = self.sel_entry().map(|e| e.name.clone());
                self.load();
                self.request_preview(true);
            }
            KeyCode::Char('~') => {
                if let Some(h) = dirs::home_dir() {
                    self.navigate(h, None);
                }
            }
            _ => return false,
        }
        true
    }

    /// Open a shell, an agent or the editor beside this pane, in this folder.
    fn launch(&mut self, what: Launch, cx: &mut Cx) {
        match (self.launcher)(&what, &self.dir, cx.config) {
            Ok(p) => cx.act(Action::Open(p, Place::Split)),
            Err(e) => cx.notify(e),
        }
    }

    /// Keys in the name dialog (new / rename).
    fn ask_key(&mut self, key: KeyEvent, typed: Option<char>, cx: &mut Cx) {
        let Some(Ask::New(buf) | Ask::Rename(_, buf)) = &mut self.ask else { return };
        match key.code {
            KeyCode::Esc => self.ask = None,
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Enter => {
                if let Some(a) = self.ask.take() {
                    match self.do_ask(a) {
                        Ok(msg) if msg.is_empty() => {}
                        Ok(msg) | Err(msg) => cx.notify(msg),
                    }
                }
            }
            _ => {
                if let Some(c) = typed {
                    buf.push(c);
                }
            }
        }
    }

    /// Make the file or folder, or rename; then read the folder with the result selected.
    fn do_ask(&mut self, a: Ask) -> Result<String, String> {
        match a {
            Ask::New(name) => {
                let folder = name.trim_end().ends_with(['/', '\\']);
                let rel = name.trim().trim_end_matches(['/', '\\']);
                let first = checked_name(rel, true)?;
                let path = self.dir.join(rel);
                if path.exists() {
                    return Err(format!("{rel} is already there"));
                }
                let made = if folder {
                    std::fs::create_dir_all(&path)
                } else {
                    path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::File::create_new(&path).map(|_| ()))
                };
                made.map_err(|e| format!("couldn't make {rel}: {e}"))?;
                self.want_sel = Some(first);
                self.load();
                Ok(format!("made {}", pretty(&path)))
            }
            Ask::Rename(old, new) => {
                let new = new.trim().to_string();
                if new == old {
                    return Ok(String::new());
                }
                checked_name(&new, false)?;
                let (from, to) = (self.dir.join(&old), self.dir.join(&new));
                // a different file of that name is there: never rename over it ("Readme" → "README" is the same
                // file on Windows, but two files on Linux)
                if to.exists() && !same_file(&from, &to) {
                    return Err(format!("{new} is already there"));
                }
                std::fs::rename(&from, &to).map_err(|e| format!("couldn't rename {old}: {e}"))?;
                self.want_sel = Some(new.clone());
                self.load();
                Ok(format!("renamed {old} → {new}"))
            }
        }
    }

    /// y on "move it to the recycle bin?": off to a background thread (the shell call can take a moment).
    fn trash_confirmed(&mut self) {
        let Some(path) = self.confirm.take() else { return };
        let (trash, shared, waker) = (self.trasher, self.shared.clone(), self.waker.clone());
        std::thread::spawn(move || {
            let bin = preview::bin_name();
            let r = match trash(&path) {
                Ok(()) => Ok(format!("{} is in {bin}", pretty(&path))),
                Err(e) => Err(format!("couldn't move {} to {bin}: {e}", pretty(&path))),
            };
            shared.lock().unwrap().op = Some(r);
            if let Some(w) = waker {
                w.wake();
            }
        });
    }

    /// The filter box, above the list while a filter is set (music's search box). Returns the rect left for the list.
    fn draw_filter(&mut self, f: &mut Frame, r: Rect, cx: &Cx) -> Rect {
        let Some(q) = &self.filter else { return r };
        if r.height < 6 {
            return r;
        }
        let t = cx.theme;
        let bx = Rect { height: 3, ..r };
        let block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).border_style(Style::default().fg(if self.filtering { t.accent } else { t.frame }));
        let inner = block.inner(bx);
        f.render_widget(block, bx);
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
        let mut v = vec![Span::raw(q.clone())];
        if self.filtering {
            v.push(Span::styled(" ", Style::default().add_modifier(Modifier::REVERSED)));
        }
        if q.is_empty() {
            v.push(Span::styled(" filter the names in this folder", ui::muted(t)));
        }
        f.render_widget(Paragraph::new(Line::from(v)), inner);
        Rect { y: r.y + 3, height: r.height - 3, ..r }
    }

    /// The name dialog (new / rename) and the recycle-bin question, over the pane. Both name the full path.
    fn draw_dialogs(&self, f: &mut Frame, area: Rect, cx: &Cx) {
        let t = cx.theme;
        if let Some(path) = &self.confirm {
            let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let what = if path.is_dir() { "folder" } else { "file" };
            let inner = ui::popup(f, area, dialog_w(path), 7, "are you sure?", t);
            let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
            let iw = inner.width as usize;
            let lines = vec![
                Line::from(Span::styled(ui::fit(&format!("move the {what} {name} to {}?", preview::bin_name()), iw), Style::default().fg(t.danger).add_modifier(Modifier::BOLD))),
                Line::raw(""),
                Line::from(Span::styled(fit_tail(&pretty(path), iw), ui::muted(t))),
                Line::raw(""),
                Line::from([ui::key_hint("y", "yes", t), ui::key_hint("esc", "no", t)].concat()),
            ];
            f.render_widget(Paragraph::new(lines), inner);
        }
        if let Some(a) = &self.ask {
            let (title, label, text, verb) = match a {
                Ask::New(s) => ("new", "a name (end it with / for a folder)".to_string(), s, "create"),
                Ask::Rename(old, s) => ("rename", format!("rename {old} to"), s, "rename"),
            };
            let target = self.dir.join(text.trim().trim_end_matches(['/', '\\']));
            let inner = ui::popup(f, area, dialog_w(&target), 8, &format!("{}{title}", ui::lead("files")), t);
            let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
            let iw = inner.width as usize;
            let lines = vec![
                Line::from(Span::styled(ui::fit(&label, iw), ui::muted(t))),
                Line::from(vec![Span::styled("› ", ui::bold_accent(t)), Span::raw(text.clone()), Span::styled("▏", ui::accent(t))]),
                Line::raw(""),
                Line::from(Span::styled(fit_tail(&pretty(&target), iw), ui::muted(t))),
                Line::raw(""),
                Line::from([ui::key_hint("enter", verb, t), ui::key_hint("esc", "cancel", t)].concat()),
            ];
            f.render_widget(Paragraph::new(lines), inner);
        }
    }
}

/// Hand a background result to the pane through its slot, unless a newer one is already waiting there: two folder
/// reads can be in flight (the watcher's and an r), and an older one finishing last must not knock out the newer
/// result before the pane has taken it (the pane drops stale tickets, so it would wait on "loading…" for good).
fn offer<T>(slot: &mut Option<(u64, T)>, ticket: u64, v: T) {
    if slot.as_ref().is_none_or(|(t, _)| *t < ticket) {
        *slot = Some((ticket, v));
    }
}

/// A dialog wide enough for the path it names (the popup keeps it on screen).
fn dialog_w(path: &Path) -> u16 {
    (UnicodeWidthStr::width(pretty(path).as_str()) as u16 + 6).max(72)
}

/// A path cut to `w` columns from the left, so the name at its end stays: "…\src\main.rs".
fn fit_tail(s: &str, w: usize) -> String {
    if UnicodeWidthStr::width(s) <= w {
        return s.to_string();
    }
    let mut tail: Vec<char> = vec![];
    let mut used = 1; // the …
    for c in s.chars().rev() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw > w {
            break;
        }
        used += cw;
        tail.push(c);
    }
    std::iter::once('…').chain(tail.into_iter().rev()).collect()
}

/// Whether two paths are one file (a rename that only changes case on a case-insensitive disk).
fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        matches!((std::fs::metadata(a), std::fs::metadata(b)), (Ok(x), Ok(y)) if x.dev() == y.dev() && x.ino() == y.ino())
    }
    #[cfg(not(unix))]
    {
        // Windows folders are case-insensitive: the same name in another case is the same file
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    }
}

/// A name typed for a new file (`nested`: may hold subfolders, "src/new.rs") or a rename: it has to stay inside
/// this folder. Returns its first part (what to select afterwards).
fn checked_name(name: &str, nested: bool) -> Result<String, String> {
    use std::path::Component;
    if name.is_empty() {
        return Err("type a name first".into());
    }
    let p = Path::new(name);
    if !nested && p.components().count() != 1 {
        return Err("just a name: a rename stays in this folder".into());
    }
    let mut parts = p.components();
    let first = match parts.next() {
        Some(Component::Normal(n)) => n.to_string_lossy().to_string(),
        _ => return Err(format!("{name}: the name has to stay inside this folder")),
    };
    if parts.any(|c| !matches!(c, Component::Normal(_))) {
        return Err(format!("{name}: the name has to stay inside this folder"));
    }
    Ok(first)
}

/// The real launcher: a shell, Claude Code / Codex, or your editor, working in `dir`.
fn launch(what: &Launch, dir: &Path, cfg: &crate::config::Config) -> Result<Box<dyn Pane>, String> {
    let here = Some(dir.to_path_buf());
    match what {
        Launch::Shell => Ok(Box::new(crate::panes::term::Term::shell(cfg, here))),
        Launch::Agent(name) => crate::panes::open_agent(name, here).ok_or_else(|| format!("{name} isn't installed · your AIs (F3) installs it")),
        Launch::Edit(file) => {
            let configured = ["VISUAL", "EDITOR"].iter().filter_map(|v| std::env::var(v).ok()).find(|v| !v.trim().is_empty());
            let (prog, args, title) = editor_command(file, configured.as_deref(), crate::config::which);
            Ok(Box::new(crate::panes::term::Term::new(&title, "notes", &prog, args, here)))
        }
    }
}

/// How to open `file` in an editor: $VISUAL / $EDITOR ("code --wait" works), else a terminal editor this system
/// has (Microsoft Edit on Windows, nano elsewhere), else notepad / vi. `find` looks a program up on PATH.
/// Returns (program, args, pane title).
fn editor_command(file: &Path, configured: Option<&str>, find: impl Fn(&str) -> Option<PathBuf>) -> (String, Vec<String>, String) {
    let mut words: Vec<String> = configured.map(|c| c.split_whitespace().map(String::from).collect()).unwrap_or_default();
    if words.is_empty() {
        let (first, fallback) = if cfg!(windows) { ("edit", "notepad") } else { ("nano", "vi") };
        words.push(if find(first).is_some() { first } else { fallback }.to_string());
    }
    let prog = words.remove(0);
    let stem = Path::new(&prog).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| prog.clone());
    let name = file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let mut args = words;
    args.push(file.to_string_lossy().to_string());
    // .cmd shims (code.cmd from npm or VS Code) need cmd.exe to run them
    match find(&prog).map(|p| p.to_string_lossy().to_string()) {
        Some(p) if cfg!(windows) && (p.ends_with(".cmd") || p.ends_with(".bat")) => ("cmd.exe".into(), [vec!["/c".into(), p], args].concat(), format!("{stem} · {name}")),
        _ => (prog, args, format!("{stem} · {name}")),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::testkit::Kit;
    use ratatui::{Terminal, backend::TestBackend};

    /// A scratch folder under target/ (never the user's files), wiped at the start of each test that uses it.
    pub fn scratch(name: &str) -> PathBuf {
        let d = std::path::absolute(PathBuf::from("target/test-scratch").join(name)).unwrap();
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Render the pane the way the app does: sidebar frame with the pane's side section on the left, the framed
    /// pane on the right. Saved as an HTML snapshot (turn it into a PNG with tools\snap.ps1).
    pub fn snap(k: &mut Kit, p: &mut dyn Pane, w: u16, h: u16, path: &str) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut actions = vec![];
        let theme = k.theme.clone();
        term.draw(|f| {
            let side_w = 32;
            let side = Rect::new(0, 0, side_w, h);
            let inner = ui::frame(f, side, &format!("{}oriel", ui::lead("window")), Some(&p.title_hint()), false, &theme);
            let inner = Rect { x: inner.x + 1, width: inner.width - 2, y: inner.y + 1, height: inner.height - 1 };
            for (i, (icon, label, key)) in [("ai", "ai", "F1"), ("music", "music", "F2"), ("system", "system", "F3"), ("files", "files", "F4"), ("notes", "notes", "F5"), ("storage", "storage", "F6")].iter().enumerate() {
                ui::side_row(f, Rect { y: inner.y + i as u16, height: 1, ..inner }, icon, label, key, p.icon() == *icon, &theme);
            }
            ui::rule(f, Rect { y: inner.y + 7, height: 1, ..inner }, &theme);
            let sr = Rect { y: inner.y + 9, height: inner.height - 9, ..inner };
            let mut cx = Cx { id: 1, theme: &theme, config: &k.config, tx: &k.tx, actions: &mut actions, focused: false, time: 1.0 };
            p.side(f, sr, &mut cx);
            let main = Rect::new(side_w, 0, w - side_w, h);
            let mi = ui::frame(f, main, &format!("{}{}", ui::lead(p.icon()), p.title()), p.subtitle().as_deref(), true, &theme);
            let mut cx = Cx { id: 1, theme: &theme, config: &k.config, tx: &k.tx, actions: &mut actions, focused: true, time: 1.0 };
            p.render(f, mi, &mut cx);
        })
        .unwrap();
        crate::testkit::save_html(term.backend().buffer(), path);
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect();
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    trait TitleHint {
        fn title_hint(&self) -> String;
    }
    impl<T: Pane + ?Sized> TitleHint for T {
        fn title_hint(&self) -> String {
            if self.icon() == "files" { "files".into() } else { "notes".into() }
        }
    }

    fn fixture(name: &str) -> PathBuf {
        let d = scratch(name);
        for sub in ["assets", "docs", "src", "tests"] {
            std::fs::create_dir_all(d.join(sub)).unwrap();
        }
        std::fs::create_dir_all(d.join(".cache")).unwrap();
        std::fs::write(d.join("README.md"), "# demo project\n\n**demo** is a small example app. Everything lives in `src`.\n\n- docs: how it works\n- tests: run them with `cargo test`\n\n## Run it\n\n```powershell\ncargo run\n```\n").unwrap();
        std::fs::write(d.join("main.rs"), "// entry point\nfn main() {\n    let answer = 42;\n    println!(\"hello {answer}\");\n}\n").unwrap();
        std::fs::write(d.join("blob.bin"), [0u8, 1, 2, 3, 0, 255]).unwrap();
        std::fs::write(d.join(".secret"), "shh").unwrap();
        let img = image::RgbImage::from_fn(64, 32, |x, y| image::Rgb([(x * 4) as u8, (y * 8) as u8, 160]));
        img.save(d.join("sky.png")).unwrap();
        d
    }

    /// Start the workers, then wait (as long as it takes, up to 10 s) until the folder is listed, its preview is in
    /// and the places are found: a fixed wait failed whenever the machine was busy.
    fn settle(k: &mut Kit, p: &mut Files) {
        k.render(p, 150, 44); // starts the workers
        assert!(wait_for(k, p, |p| !p.loading && p.preview.is_some() && !p.places.is_empty()), "the folder never settled");
    }

    /// The preview on show is of `name`.
    fn previewing(p: &Files, name: &str) -> bool {
        p.preview.as_ref().is_some_and(|pv| pv.name == name)
    }

    #[test]
    fn files_lists_and_previews() {
        let d = fixture("files-list");
        let mut k = Kit::new();
        let mut p = Files::new(Some(d.clone()));
        settle(&mut k, &mut p);
        let s = snap(&mut k, &mut p, 150, 44, "target/snap/files-main.html");
        println!("{s}");
        assert!(s.contains("assets"), "folders listed");
        assert!(s.contains("README.md"));
        assert!(!s.contains(".secret"), "dotfiles hidden by default");
        assert!(s.contains("4 folders · 5 files") || s.contains("folders ·"), "folder summary");
        assert!(s.contains("demo is a small example app") || s.contains("**demo** is a small example app"), "README previewed");
        assert!(s.contains("home"), "places in the sidebar");

        // move to main.rs: code preview with line numbers
        let idx = p.shown.iter().position(|&i| p.all[i].name == "main.rs").unwrap() + 1;
        for _ in 0..idx {
            k.key(&mut p, KeyCode::Char('j'));
        }
        assert!(wait_for(&mut k, &mut p, |p| previewing(p, "main.rs")));
        let s = snap(&mut k, &mut p, 150, 44, "target/snap/files-code.html");
        assert!(s.contains("2 fn main() {"), "numbered code preview:\n{s}");

        // image preview
        let idx = p.shown.iter().position(|&i| p.all[i].name == "sky.png").unwrap() + 1;
        p.select(idx);
        assert!(wait_for(&mut k, &mut p, |p| previewing(p, "sky.png")));
        let s = snap(&mut k, &mut p, 150, 44, "target/snap/files-image.html");
        assert!(s.contains("64×32") && s.contains("▀▀▀▀"), "half-block image:\n{s}");

        // binary
        let idx = p.shown.iter().position(|&i| p.all[i].name == "blob.bin").unwrap() + 1;
        p.select(idx);
        assert!(wait_for(&mut k, &mut p, |p| previewing(p, "blob.bin")));
        assert!(k.render(&mut p, 150, 44).contains("binary file"));
    }

    #[test]
    fn files_navigation_and_hidden() {
        let d = fixture("files-nav");
        let mut k = Kit::new();
        let mut p = Files::new(Some(d.clone()));
        settle(&mut k, &mut p);
        // hidden toggle
        k.key(&mut p, KeyCode::Char('.'));
        assert!(k.render(&mut p, 150, 44).contains(".secret"));
        k.key(&mut p, KeyCode::Char('.'));
        assert!(!k.render(&mut p, 150, 44).contains(".secret"));
        // into "src", then back up: the folder we came from stays selected
        let idx = p.shown.iter().position(|&i| p.all[i].name == "src").unwrap() + 1;
        p.select(idx);
        k.key(&mut p, KeyCode::Enter);
        assert!(wait_for(&mut k, &mut p, |p| !p.loading), "src is listed");
        assert_eq!(p.dir, d.join("src"));
        assert!(k.render(&mut p, 150, 44).contains("(empty)"));
        k.key(&mut p, KeyCode::Backspace);
        assert!(wait_for(&mut k, &mut p, |p| !p.loading), "back up, listed");
        assert_eq!(p.dir, d);
        assert_eq!(p.sel_entry().map(|e| e.name.as_str()), Some("src"));
        // enter on a file focuses the preview; esc gives focus back
        let idx = p.shown.iter().position(|&i| p.all[i].name == "main.rs").unwrap() + 1;
        p.select(idx);
        k.key(&mut p, KeyCode::Enter);
        assert!(p.focus_preview);
        k.key(&mut p, KeyCode::Esc);
        assert!(!p.focus_preview);
        // mouse: click a row selects it
        let r = p.list_rect;
        let ev = MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: r.x + 8, row: r.y + 1, modifiers: KeyModifiers::NONE };
        k.mouse(&mut p, ev, r);
        assert_eq!(p.sel, 1);
        // r refreshes; F5 (and shift+F5) are left to the app: F5 is the system app from everywhere
        assert!(k.key(&mut p, KeyCode::Char('r')));
        assert!(!k.key(&mut p, KeyCode::F(5)) && !k.key_mod(&mut p, KeyCode::F(5), KeyModifiers::SHIFT));
    }

    #[test]
    fn files_ask_chat_and_folder() {
        let d = fixture("files-ask");
        let mut k = Kit::new();
        let mut p = Files::new(Some(d.clone()));
        settle(&mut k, &mut p);
        assert_eq!((p.cwd(), p.reopen()), (Some(d.clone()), Some("files")), "a terminal from here starts in this folder");
        let idx = p.shown.iter().position(|&i| p.all[i].name == "main.rs").unwrap() + 1;
        p.select(idx);
        assert!(k.key(&mut p, KeyCode::Char('a')));
        let want = format!("{} ", crate::clip::paste_form(&[d.join("main.rs").to_string_lossy().to_string()]));
        assert!(k.actions.iter().any(|a| matches!(a, Action::AppPaste("ai", t) if *t == want)), "the path goes to chat");
    }

    #[test]
    fn files_side_places() {
        let mut k = Kit::new();
        let mut p = Files::new(dirs::home_dir());
        k.render(&mut p, 150, 44);
        // the drive scan runs as a second step and can be slow while the rest of the suite runs: wait for it
        let mut s = String::new();
        for _ in 0..40 {
            k.wait_wake(&mut p, 250);
            s = k.render_side(&mut p, 30, 20);
            if !cfg!(windows) || s.contains("C: drive") {
                break;
            }
        }
        println!("{s}");
        assert!(s.contains("home"));
        if cfg!(windows) {
            assert!(s.contains("C: drive"));
        }
    }

    fn names(p: &Files) -> Vec<String> {
        p.shown.iter().map(|&i| p.all[i].name.clone()).collect()
    }

    fn pv_text(p: &Files) -> String {
        match p.preview.as_ref().map(|x| &x.body) {
            Some(Body::Text { lines, .. }) => lines.iter().flat_map(|l| l.iter().map(|(_, s)| s.as_str())).collect(),
            _ => String::new(),
        }
    }

    /// Poll on each wake, as the app does, until `ok` holds (10 s at most). It returns as soon as it does, so a
    /// busy machine makes the test slower, never wrong — a fixed wait would fail when the listing takes longer.
    fn wait_for(k: &mut Kit, p: &mut Files, ok: impl Fn(&Files) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ok(p) {
            if Instant::now() >= deadline {
                return false;
            }
            k.wait_wake(p, 20);
            k.poll(p);
        }
        true
    }

    #[test]
    fn files_filter_as_you_type() {
        let d = fixture("files-filter");
        let mut k = Kit::new();
        let mut p = Files::new(Some(d.clone()));
        settle(&mut k, &mut p);
        let all = names(&p).len();
        k.key(&mut p, KeyCode::Char('/'));
        k.typ(&mut p, "MA"); // any case
        assert_eq!(names(&p), ["main.rs"]);
        assert_eq!(p.sel_entry().map(|e| e.name.as_str()), Some("main.rs"), "the top match is picked");
        let s = snap(&mut k, &mut p, 150, 30, "target/snap/files-filter.html");
        assert!(s.contains("│ MA") && s.contains(&format!("1 of {all} match “MA”")), "{s}");
        // letters that are keys elsewhere (c, x, d, n...) type into the filter
        k.key(&mut p, KeyCode::Backspace);
        k.key(&mut p, KeyCode::Backspace);
        k.typ(&mut p, "dx");
        assert!(names(&p).is_empty() && p.ask.is_none() && p.confirm.is_none());
        assert!(k.render(&mut p, 150, 30).contains("nothing matches"));
        // esc drops the filter
        k.key(&mut p, KeyCode::Esc);
        assert_eq!(names(&p).len(), all);
        assert!(p.filter.is_none() && !p.filtering);
        // enter opens the top match: a folder, and the filter is gone in there
        k.key(&mut p, KeyCode::Char('/'));
        k.typ(&mut p, "sr");
        k.key(&mut p, KeyCode::Enter);
        k.wait_wake(&mut p, 300);
        assert_eq!(p.dir, d.join("src"));
        assert!(p.filter.is_none());
    }

    thread_local! {
        static LAUNCHED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(vec![]) };
    }

    /// Stands in for the real launcher: records what it was asked to start, starts nothing.
    fn fake_launch(what: &Launch, dir: &Path, _cfg: &crate::config::Config) -> Result<Box<dyn Pane>, String> {
        let what = match what {
            Launch::Shell => "shell".to_string(),
            Launch::Agent("codex") => return Err("codex isn't installed · your AIs (F3) installs it".into()),
            Launch::Agent(a) => a.to_string(),
            Launch::Edit(f) => format!("edit {}", f.file_name().unwrap().to_string_lossy()),
        };
        LAUNCHED.with(|l| l.borrow_mut().push(format!("{what} in {}", dir.file_name().unwrap().to_string_lossy())));
        Ok(Box::new(crate::panes::home::Home::new()))
    }

    #[test]
    fn files_opens_agents_editor_and_shell_here() {
        let d = fixture("files-launch");
        let mut k = Kit::new();
        let mut p = Files::new(Some(d.clone()));
        p.launcher = fake_launch;
        settle(&mut k, &mut p);
        k.key(&mut p, KeyCode::Char('c'));
        k.key(&mut p, KeyCode::Char('t'));
        k.key(&mut p, KeyCode::Char('e')); // the folder row: nothing to edit
        let idx = p.shown.iter().position(|&i| p.all[i].name == "main.rs").unwrap() + 1;
        p.select(idx);
        k.key(&mut p, KeyCode::Char('e'));
        k.key(&mut p, KeyCode::Char('x')); // not installed: says so
        let got = LAUNCHED.with(|l| l.borrow().clone());
        assert_eq!(got, ["claude in files-launch", "shell in files-launch", "edit main.rs in files-launch"]);
        let opened = k.actions.iter().filter(|a| matches!(a, Action::Open(_, Place::Split))).count();
        assert_eq!(opened, 3, "each opens beside the files pane");
        let notes = k.notices();
        assert!(notes.iter().any(|n| n.starts_with("pick a file to edit")) && notes.iter().any(|n| n.contains("codex isn't installed")), "{notes:?}");
    }

    #[test]
    fn files_editor_command() {
        let f = Path::new("notes.md");
        let none = |_: &str| None;
        // $EDITOR with arguments
        let (prog, args, title) = editor_command(f, Some("hx --vsplit"), none);
        assert_eq!((prog.as_str(), args, title.as_str()), ("hx", vec!["--vsplit".to_string(), "notes.md".into()], "hx · notes.md"));
        // nothing set: the system's terminal editor if it has one, else the fallback
        let (prog, _, _) = editor_command(f, None, none);
        assert_eq!(prog, if cfg!(windows) { "notepad" } else { "vi" });
        let has_edit = |p: &str| (p == "edit" || p == "nano").then(|| PathBuf::from(p));
        let (prog, _, _) = editor_command(f, None, has_edit);
        assert_eq!(prog, if cfg!(windows) { "edit" } else { "nano" });
        if cfg!(windows) {
            // a .cmd shim (VS Code's code.cmd) runs through cmd.exe
            let shim = |p: &str| (p == "code").then(|| PathBuf::from(r"C:\Tools\code.cmd"));
            let (prog, args, _) = editor_command(f, Some("code --wait"), shim);
            assert_eq!(prog, "cmd.exe");
            assert_eq!(args, ["/c", r"C:\Tools\code.cmd", "--wait", "notes.md"]);
        }
    }

    /// Two folder reads in flight, the newer finishing first: the older one arriving after it doesn't replace it
    /// (the pane drops the older ticket, so the newer listing would be lost and "loading…" stay up for good).
    #[test]
    fn files_late_old_listing_doesnt_replace_a_newer_one() {
        let mut slot: Option<(u64, &str)> = None;
        offer(&mut slot, 2, "newer");
        offer(&mut slot, 1, "older, finished last");
        assert_eq!(slot, Some((2, "newer")));
        offer(&mut slot, 3, "newest");
        assert_eq!(slot, Some((3, "newest")));
        // once the pane has taken it, a late older one may sit in the empty slot: the pane compares tickets and drops it
        slot = None;
        offer(&mut slot, 1, "older");
        assert_eq!(slot, Some((1, "older")));
    }

    /// Stands in for the recycle bin: the scratch file is simply removed (tests never touch the real bin).
    fn fake_trash(p: &Path) -> Result<(), String> {
        if p.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) }.map_err(|e| e.to_string())
    }

    #[test]
    fn files_new_rename_delete() {
        let d = fixture("files-ops");
        let mut k = Kit::new();
        let mut p = Files::new(Some(d.clone()));
        p.trasher = fake_trash;
        settle(&mut k, &mut p);
        // n: a new file, then a folder (ending in /); the dialog names the full path
        k.key(&mut p, KeyCode::Char('n'));
        k.typ(&mut p, "todo.txt");
        let s = snap(&mut k, &mut p, 150, 30, "target/snap/files-new.html");
        assert!(s.contains("end it with / for a folder") && s.contains("todo.txt"), "{s}");
        k.key(&mut p, KeyCode::Enter);
        assert!(d.join("todo.txt").is_file());
        wait_for(&mut k, &mut p, |p| !p.loading && p.all.iter().any(|e| e.name == "todo.txt"));
        assert_eq!(p.sel_entry().map(|e| e.name.as_str()), Some("todo.txt"), "the new file is picked");
        k.key(&mut p, KeyCode::Char('n'));
        k.typ(&mut p, "drafts/");
        k.key(&mut p, KeyCode::Enter);
        assert!(d.join("drafts").is_dir());
        assert!(wait_for(&mut k, &mut p, |p| !p.loading && p.all.iter().any(|e| e.name == "drafts")));
        // a name that leaves the folder is refused
        k.key(&mut p, KeyCode::Char('n'));
        k.typ(&mut p, "../escape.txt");
        k.key(&mut p, KeyCode::Enter);
        assert!(!d.parent().unwrap().join("escape.txt").exists());
        assert!(k.notices().iter().any(|n| n.contains("has to stay inside this folder")), "{:?}", k.notices());
        // R renames the picked one (the box starts with its name)
        let idx = p.shown.iter().position(|&i| p.all[i].name == "todo.txt").unwrap() + 1;
        p.select(idx);
        k.key(&mut p, KeyCode::Char('R'));
        for _ in 0.."todo.txt".len() {
            k.key(&mut p, KeyCode::Backspace);
        }
        k.typ(&mut p, "done.txt");
        k.key(&mut p, KeyCode::Enter);
        assert!(d.join("done.txt").is_file() && !d.join("todo.txt").exists());
        wait_for(&mut k, &mut p, |p| !p.loading && p.all.iter().any(|e| e.name == "done.txt"));
        assert_eq!(p.sel_entry().map(|e| e.name.as_str()), Some("done.txt"));
        // d asks first, naming the path; anything but y keeps it
        k.key(&mut p, KeyCode::Char('d'));
        let s = snap(&mut k, &mut p, 150, 30, "target/snap/files-delete.html");
        assert!(s.contains("are you sure?") && s.contains("move the file done.txt to") && s.contains("files-ops") && s.contains("done.txt"), "{s}");
        assert_eq!(fit_tail("C:/a/long/path/to/done.txt", 12), "…to/done.txt");
        k.key(&mut p, KeyCode::Char('n'));
        assert!(p.confirm.is_none() && d.join("done.txt").exists());
        // y moves it (off the UI thread), then the list is read again and the cursor stays in place
        let at = p.sel;
        k.key(&mut p, KeyCode::Char('d'));
        k.key(&mut p, KeyCode::Char('y'));
        // as long as it takes (a busy machine), until it's said and the folder has been read again
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(10) && !(k.notices().iter().any(|n| n.contains("done.txt is in")) && !p.loading && !names(&p).iter().any(|n| n == "done.txt")) {
            k.wait_wake(&mut p, 20);
            k.poll(&mut p);
        }
        assert!(!d.join("done.txt").exists());
        assert!(!names(&p).iter().any(|n| n == "done.txt"), "{:?}", names(&p));
        assert!(k.notices().iter().any(|n| n.contains("done.txt is in")), "{:?}", k.notices());
        assert_eq!(p.sel, at.min(p.rows() - 1));
        // the folder row itself can't be deleted
        p.select(0);
        k.key(&mut p, KeyCode::Char('d'));
        assert!(p.confirm.is_none());
        // a rename never lands on another file, even one whose name only differs in case (two files on Linux);
        // changing just the case of a name is fine
        std::fs::write(d.join("keep.txt"), "mine").unwrap();
        std::fs::write(d.join("case.txt"), "x").unwrap();
        assert!(p.do_ask(Ask::Rename("case.txt".into(), "keep.txt".into())).is_err());
        assert!(p.do_ask(Ask::Rename("case.txt".into(), "Case.txt".into())).is_ok());
        let on_disk: Vec<String> = std::fs::read_dir(&d).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        assert!(on_disk.iter().any(|n| n == "Case.txt"), "{on_disk:?}");
        assert_eq!(std::fs::read_to_string(d.join("keep.txt")).unwrap(), "mine");
        #[cfg(unix)]
        {
            std::fs::write(d.join("KEEP.txt"), "theirs").unwrap();
            assert!(p.do_ask(Ask::Rename("keep.txt".into(), "KEEP.txt".into())).is_err());
            assert_eq!(std::fs::read_to_string(d.join("KEEP.txt")).unwrap(), "theirs");
        }
    }

    /// The real recycle bin: moves a scratch file there (look for "oriel-trash-test.txt" in it afterwards).
    #[test]
    #[ignore] // touches the real recycle bin / trash: cargo test files_real_trash -- --ignored
    fn files_real_trash() {
        let d = scratch("files-real-trash");
        let f = d.join("oriel-trash-test.txt");
        std::fs::write(&f, "delete me").unwrap();
        preview::to_trash(&f).unwrap();
        assert!(!f.exists());
    }

    #[test]
    fn files_sees_changes_on_disk() {
        let d = fixture("files-watch");
        let mut k = Kit::new();
        let mut p = Files::new(Some(d.clone()));
        settle(&mut k, &mut p);
        assert!(p.watch.is_some() || !cfg!(windows), "a local folder can be watched");
        let idx = p.shown.iter().position(|&i| p.all[i].name == "main.rs").unwrap() + 1;
        p.select(idx);
        k.wait_wake(&mut p, 300);
        let until = |k: &mut Kit, p: &mut Files, ok: &dyn Fn(&Files) -> bool| {
            for _ in 0..80 {
                if ok(p) {
                    return true;
                }
                k.wait_wake(p, 50);
                k.poll(p); // what the ticks do
            }
            ok(p)
        };
        if p.watch.is_some() {
            // something else writes files here (an agent, a download): they show up without r
            std::fs::write(d.join("fresh.txt"), "new").unwrap();
            assert!(until(&mut k, &mut p, &|p| names(p).iter().any(|n| n == "fresh.txt")), "{:?}", names(&p));
            assert_eq!(p.sel_entry().map(|e| e.name.as_str()), Some("main.rs"), "the cursor stays put");
            // the file on show grows: its preview follows
            std::fs::write(d.join("main.rs"), "fn main() {}\n// a line an agent added\n").unwrap();
            assert!(until(&mut k, &mut p, &|p| pv_text(p).contains("a line an agent added")), "{}", pv_text(&p));
            // the picked file vanishes: the cursor stays on its row instead of jumping to the top
            let at = p.sel;
            std::fs::remove_file(d.join("main.rs")).unwrap();
            assert!(until(&mut k, &mut p, &|p| !names(p).iter().any(|n| n == "main.rs")));
            assert_eq!(p.sel, at.min(p.rows() - 1));
            // a re-read that lands after you've moved on keeps you where you are now, not where you were
            p.refresh();
            p.select(1);
            let moved = p.sel_entry().map(|e| e.name.clone());
            assert!(until(&mut k, &mut p, &|p| !p.loading));
            assert_eq!(p.sel_entry().map(|e| e.name.clone()), moved);
        }
        // with a watcher, coming back to the pane doesn't read it again (the watcher already caught any change)
        if p.watch.is_some() {
            let reads = p.list_gen;
            p.was_focused = false;
            p.last_load = Instant::now() - Duration::from_secs(5);
            k.render(&mut p, 150, 44);
            assert_eq!(p.list_gen, reads);
        }
        // no watcher (some network drives): coming back to the pane reads it again
        p.watch = None;
        p.last_load = Instant::now() - Duration::from_secs(5);
        std::fs::write(d.join("later.txt"), "x").unwrap();
        p.was_focused = false;
        k.render(&mut p, 150, 44);
        assert!(until(&mut k, &mut p, &|p| names(p).iter().any(|n| n == "later.txt")), "{:?}", names(&p));
    }

    /// The new boxes and lines (filter, name dialog, bin question, notes' conflict line and finder, help with no
    /// hits) survive panes squeezed down to nothing.
    #[test]
    fn tools_tiny_panes_dont_panic() {
        let sizes = [(1, 1), (2, 2), (5, 3), (12, 4), (30, 6), (49, 8), (50, 5), (60, 7)];
        let d = fixture("files-tiny");
        let mut k = Kit::new();
        let mut p = Files::new(Some(d.clone()));
        settle(&mut k, &mut p);
        k.key(&mut p, KeyCode::Char('/'));
        k.typ(&mut p, "ma");
        for (w, h) in sizes {
            k.render(&mut p, w, h);
        }
        k.key(&mut p, KeyCode::Esc);
        p.select(1);
        k.key(&mut p, KeyCode::Char('d'));
        for (w, h) in sizes {
            k.render(&mut p, w, h);
        }
        k.key(&mut p, KeyCode::Esc);
        k.key(&mut p, KeyCode::Char('n'));
        k.typ(&mut p, "a/very/long/name/for/a/new/file.txt");
        for (w, h) in sizes {
            k.render(&mut p, w, h);
        }
        k.key(&mut p, KeyCode::Esc);

        let nd = scratch("notes-tiny");
        std::fs::write(nd.join("a.md"), "# a\n\ntext").unwrap();
        let mut n = crate::panes::notes::Notes::open_in(nd, None);
        k.key_mod(&mut n, KeyCode::Char('f'), KeyModifiers::CONTROL);
        k.typ(&mut n, "zz");
        for (w, h) in sizes {
            k.render(&mut n, w, h);
            k.render_side(&mut n, w, h);
        }

        let mut hp = crate::panes::help::Help::new();
        k.key(&mut hp, KeyCode::Char('/'));
        k.typ(&mut hp, "zzzz");
        for (w, h) in sizes {
            k.render(&mut hp, w, h);
            k.render_side(&mut hp, w, h);
        }
        k.key(&mut hp, KeyCode::Esc);
        k.key(&mut hp, KeyCode::Char('/'));
        k.typ(&mut hp, "rollback");
        for (w, h) in sizes {
            k.render(&mut hp, w, h);
        }
    }

    #[test]
    fn files_highlight() {
        let l = preview::highlight(&["let s = \"hi\"; // note".into()], "rs");
        assert!(l[0].iter().any(|(t, s)| *t == Tok::Kw && s == "let"));
        assert!(l[0].iter().any(|(t, s)| *t == Tok::Str && s == "\"hi\""));
        assert!(l[0].iter().any(|(t, s)| *t == Tok::Com && s == "// note"));
        let l = preview::highlight(&["fn f<'a>(x: &'a str) -> char { 'x' }".into()], "rs");
        assert!(l[0].iter().any(|(t, s)| *t == Tok::Str && s == "'x'"), "{l:?}");
        let l = preview::highlight(&["const a = 'js string';".into()], "js");
        assert!(l[0].iter().any(|(t, s)| *t == Tok::Str && s == "'js string'"));
    }
}
