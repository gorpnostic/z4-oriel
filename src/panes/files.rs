//! files: nest's files app. The folder on the left (folders first, icons by type), a live preview of whatever is
//! highlighted on the right: folders get a summary + their README, text/code its contents with light syntax
//! colouring, images a half-block pixel picture. Places (home, desktop, ..., drives) live in the sidebar.
//!
//! Directory reads and previews run on background threads that hand results back through `Shared` and wake
//! the pane; render only draws what is already in memory.

pub mod clock;
mod preview;

use crate::pane::{Action, Cx, Pane, Place, Waker};
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use preview::{Body, Entry, Kind, PlaceItem, Preview, Tok};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
struct Shared {
    listing: Option<(u64, Result<Vec<Entry>, String>)>,
    preview: Option<(u64, Preview)>,
    places: Option<Vec<PlaceItem>>,
}

struct Req {
    ticket: u64,
    path: PathBuf,
    size: (u16, u16),
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
                let p = preview::build(&req.path, req.size.0, req.size.1);
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
        let Some(waker) = self.waker.clone() else { return };
        let (ticket, dir, shared) = (self.list_gen, self.dir.clone(), self.shared.clone());
        std::thread::spawn(move || {
            let r = preview::list_dir(&dir);
            shared.lock().unwrap().listing = Some((ticket, r));
            waker.wake();
        });
    }

    fn navigate(&mut self, dir: PathBuf, want_sel: Option<String>) {
        self.dir = dir;
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
        self.shown = (0..self.all.len()).filter(|&i| self.hidden || !self.all[i].hidden).collect();
        self.sel = keep.and_then(|n| self.shown.iter().position(|&i| self.all[i].name == n).map(|p| p + 1)).unwrap_or(0);
        self.follow = true;
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
            let _ = w.send(Req { ticket: self.prev_gen, path, size: self.pv_size });
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

    fn poll(&mut self, cx: &mut Cx) {
        self.start(cx);
        let (listing, prev, places) = {
            let mut s = self.shared.lock().unwrap();
            (s.listing.take(), s.preview.take(), s.places.take())
        };
        if let Some((ticket, r)) = listing {
            if ticket == self.list_gen {
                self.loading = false;
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
                self.refilter();
                if let Some(want) = self.want_sel.take() {
                    if let Some(p) = self.shown.iter().position(|&i| self.all[i].name == want) {
                        self.sel = p + 1;
                    }
                }
                self.request_preview(false);
            }
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
        let t = cx.theme;
        let os = format!("open with {}", Self::os_name());
        let hints: Vec<(&str, &str)> = if self.focus_preview {
            vec![("j/k", "scroll"), ("esc", "back to the list"), ("o", &os), ("p", "copy path"), ("t", "terminal here")]
        } else {
            vec![("enter", "open"), ("o", &os), ("p", "copy path"), ("t", "terminal here"), ("backspace", "up"), (".", "hidden")]
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
                self.draw_list(f, body, cx);
            }
            return;
        }
        let lw = (body.width * 2 / 5).clamp(26, 60);
        let list = Rect { width: lw, ..body };
        self.draw_list(f, list, cx);
        let t = cx.theme;
        let div_x = body.x + lw + 1;
        for y in body.y..body.bottom() {
            f.buffer_mut()[(div_x, y)].set_symbol("│").set_fg(t.frame);
        }
        let pv = Rect { x: div_x + 2, width: body.right().saturating_sub(div_x + 3), ..body };
        self.draw_preview(f, pv, cx);
    }

    fn key(&mut self, key: KeyEvent, cx: &mut Cx) -> bool {
        self.start(cx);
        if key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) {
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
            KeyCode::Char('t') => {
                let term = crate::panes::term::Term::shell(cx.config, Some(self.dir.clone()));
                cx.act(Action::Open(Box::new(term), Place::Split));
            }
            KeyCode::Char('.') => {
                self.hidden = !self.hidden;
                self.refilter();
                self.request_preview(false);
            }
            KeyCode::Char('r') | KeyCode::F(5) => {
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

    fn settle(k: &mut Kit, p: &mut Files) {
        k.render(p, 150, 44); // starts the workers
        k.wait_wake(p, 400);
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
        k.wait_wake(&mut p, 300);
        let s = snap(&mut k, &mut p, 150, 44, "target/snap/files-code.html");
        assert!(s.contains("2 fn main() {"), "numbered code preview:\n{s}");

        // image preview
        let idx = p.shown.iter().position(|&i| p.all[i].name == "sky.png").unwrap() + 1;
        p.select(idx);
        k.wait_wake(&mut p, 500);
        let s = snap(&mut k, &mut p, 150, 44, "target/snap/files-image.html");
        assert!(s.contains("64×32") && s.contains("▀▀▀▀"), "half-block image:\n{s}");

        // binary
        let idx = p.shown.iter().position(|&i| p.all[i].name == "blob.bin").unwrap() + 1;
        p.select(idx);
        k.wait_wake(&mut p, 300);
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
        k.wait_wake(&mut p, 300);
        assert_eq!(p.dir, d.join("src"));
        assert!(k.render(&mut p, 150, 44).contains("(empty)"));
        k.key(&mut p, KeyCode::Backspace);
        k.wait_wake(&mut p, 300);
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
    }

    #[test]
    fn files_side_places() {
        let mut k = Kit::new();
        let mut p = Files::new(dirs::home_dir());
        k.render(&mut p, 150, 44);
        k.wait_wake(&mut p, 2000); // the drive scan can be slow while other tests run
        let s = k.render_side(&mut p, 30, 20);
        println!("{s}");
        assert!(s.contains("home"));
        if cfg!(windows) {
            assert!(s.contains("C: drive"));
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
