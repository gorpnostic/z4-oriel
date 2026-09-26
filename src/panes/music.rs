//! music: nest's music app. Your audio-player library (or a folder scan), playlists and "most played" in the
//! sidebar, a now-playing column with the cover in half-block pixels, a live spectrum and synced lyrics, and the
//! track table with search. Playback runs on its own thread (engine.rs) and keeps going in the background.

mod cover;
mod engine;
mod library;

use crate::pane::{Cx, Pane, Waker};
use crate::theme::Theme;
use crate::ui;
use cover::Cover;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use engine::{Cmd, Engine, Repeat};
use library::{Lib, Lyrics, State, Track};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

/// Width of the now-playing column (cover is 36 wide, plus a little air).
const NP_W: u16 = 38;
const COVER_W: u32 = 36;
const COVER_ROWS: u32 = 18;
const BANDS: usize = 18;
const SPARK: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// nest's music icons that ui.rs doesn't carry: (nerd glyph, plain fallback).
fn glyph(name: &str) -> &'static str {
    let nerd = ui::NERD.load(std::sync::atomic::Ordering::Relaxed);
    let (n, p) = match name {
        "library" => ("\u{F0331}", ""),
        "most" => ("\u{F0238}", ""),
        "playlist" => ("\u{F0CB8}", "-"),
        "note" => ("\u{F0387}", "~"),
        "repeat1" => ("\u{F0458}", "rep1"),
        "repeat0" => ("\u{F0457}", "rep"),
        other => return ui::icon(other),
    };
    if nerd { n } else { p }
}

/// Glyph + space, or nothing (ui::lead for glyphs ui.rs doesn't know).
fn lead(g: &str) -> String {
    if g.is_empty() { String::new() } else { format!("{g} ") }
}

fn fmt(sec: f64) -> String {
    let s = sec.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

fn wake(slot: &Arc<Mutex<Option<Waker>>>) {
    if let Some(w) = slot.lock().unwrap().as_ref() {
        w.wake();
    }
}

/// The library as the loader thread publishes it.
#[derive(Default)]
struct Load {
    lib: Option<Lib>,
    status: String,
    done: bool,
    version: u64,
    /// Which scan may publish: a rescan (R) bumps it, and an older scan still running stops.
    run: u64,
}

enum LyricState {
    Off,
    Looking,
    Have(Lyrics),
}

#[derive(Clone, Copy, PartialEq)]
enum Hit {
    Row(usize),
    Search,
    Shuffle,
    Prev,
    Toggle,
    Next,
    Repeat,
    Progress,
    Volume,
}

/// What the renderer needs from the engine, copied out so the lock is held for microseconds.
struct Snap {
    track: Option<Track>,
    playing: bool,
    pos: f64,
    dur: f64,
    volume: f32,
    shuffle: bool,
    repeat: Repeat,
    error: Option<String>,
}

pub struct Music {
    lib: Lib,
    by_id: HashMap<String, usize>,
    load: Arc<Mutex<Load>>,
    load_gen: u64,
    status: String,
    loaded: bool,
    state: Arc<Mutex<State>>,
    engine: Engine,
    waker: Arc<Mutex<Option<Waker>>>,
    has_waker: bool,
    /// "library", "most", or a playlist id.
    view: String,
    query: String,
    searching: bool,
    /// Indices into lib.tracks for the current view + search.
    rows: Vec<usize>,
    sel: usize,
    scroll: usize,
    seen: u64,
    cover: Option<Cover>,
    cover_for: String,
    cover_done: bool,
    cover_slot: Arc<Mutex<Option<(String, Option<Cover>)>>>,
    lyrics: LyricState,
    lyrics_for: String,
    lyrics_slot: Arc<Mutex<Option<(String, Option<Lyrics>)>>>,
    levels: Vec<f32>,
    hits: Vec<(Rect, Hit)>,
    side_hits: Vec<(Rect, String)>,
    table_h: usize,
    last_click: Option<(usize, Instant)>,
    /// Play/pause (F12, /play) came before the library was in: start the last song played once it is.
    play_when_loaded: bool,
    /// Starts a library load (spawn_loader; tests swap in one that doesn't read the disk).
    scan: fn(Vec<String>, Arc<Mutex<Load>>, Arc<Mutex<Option<Waker>>>),
}

impl Music {
    pub fn new(cfg: &crate::config::Config) -> Self {
        let state = Arc::new(Mutex::new(library::load_state()));
        let waker = Arc::new(Mutex::new(None));
        let engine = Engine::new(state.clone(), waker.clone(), true);
        let load = Arc::new(Mutex::new(Load::default()));
        spawn_loader(cfg.music.folders.clone(), load.clone(), waker.clone());
        Self::build(state, waker, engine, load)
    }

    fn build(state: Arc<Mutex<State>>, waker: Arc<Mutex<Option<Waker>>>, engine: Engine, load: Arc<Mutex<Load>>) -> Self {
        Music {
            lib: Lib::default(),
            by_id: HashMap::new(),
            load,
            load_gen: 0,
            status: "loading your library…".into(),
            loaded: false,
            state,
            engine,
            waker,
            has_waker: false,
            view: "library".into(),
            query: String::new(),
            searching: false,
            rows: vec![],
            sel: 0,
            scroll: 0,
            seen: 0,
            cover: None,
            cover_for: String::new(),
            cover_done: false,
            cover_slot: Arc::new(Mutex::new(None)),
            lyrics: LyricState::Off,
            lyrics_for: String::new(),
            lyrics_slot: Arc::new(Mutex::new(None)),
            levels: vec![0.0; BANDS],
            hits: vec![],
            side_hits: vec![],
            table_h: 20,
            last_click: None,
            play_when_loaded: false,
            scan: spawn_loader,
        }
    }

    fn plays(&self, t: &Track) -> u64 {
        t.plays + self.state.lock().unwrap().plays.get(&t.id).copied().unwrap_or(0)
    }

    fn view_name(&self) -> String {
        match self.view.as_str() {
            "library" => "library".into(),
            "most" => "most played".into(),
            id => self.lib.playlists.iter().find(|p| p.id == id).map(|p| p.name.clone()).unwrap_or_else(|| "library".into()),
        }
    }

    /// Recompute the rows for the view + search, keeping the cursor on the same song when it's still there.
    fn refill(&mut self) {
        let keep = self.rows.get(self.sel).map(|&i| self.lib.tracks[i].id.clone());
        let mut rows: Vec<usize> = match self.view.as_str() {
            "library" => (0..self.lib.tracks.len()).collect(),
            "most" => {
                let st = self.state.lock().unwrap();
                let p = |t: &Track| t.plays + st.plays.get(&t.id).copied().unwrap_or(0);
                let mut v: Vec<usize> = (0..self.lib.tracks.len()).filter(|&i| p(&self.lib.tracks[i]) > 0).collect();
                v.sort_by_key(|&i| std::cmp::Reverse(p(&self.lib.tracks[i])));
                v
            }
            id => match self.lib.playlists.iter().find(|p| p.id == id) {
                Some(p) => p.ids.iter().filter_map(|i| self.by_id.get(i).copied()).collect(),
                None => {
                    self.view = "library".into();
                    (0..self.lib.tracks.len()).collect()
                }
            },
        };
        let q = self.query.trim().to_lowercase();
        if !q.is_empty() {
            rows.retain(|&i| {
                let t = &self.lib.tracks[i];
                format!("{} {} {}", t.title, t.artist, t.album).to_lowercase().contains(&q)
            });
        }
        self.rows = rows;
        self.sel = keep.and_then(|id| self.rows.iter().position(|&i| self.lib.tracks[i].id == id)).unwrap_or(0);
        self.sel = self.sel.min(self.rows.len().saturating_sub(1));
    }

    fn set_view(&mut self, v: &str) {
        if self.view != v {
            self.view = v.to_string();
            self.rows.clear();
            self.refill();
            self.sel = 0;
            self.scroll = 0;
        }
    }

    fn views(&self) -> Vec<String> {
        let mut v = vec!["library".to_string(), "most".to_string()];
        v.extend(self.lib.playlists.iter().map(|p| p.id.clone()));
        v
    }

    fn snap(&self) -> Snap {
        let s = self.engine.shared.lock().unwrap();
        Snap { track: s.track.clone(), playing: s.playing, pos: s.pos, dur: s.dur, volume: s.volume, shuffle: s.shuffle, repeat: s.repeat, error: s.error.clone() }
    }

    /// Play the view from the selected song. Songs oriel can't decode (.opus) stay out of the queue; picking one
    /// says why instead (the returned message, for a toast).
    fn play_sel(&mut self) -> Option<String> {
        let &sel = self.rows.get(self.sel)?;
        let t = &self.lib.tracks[sel];
        if let Some(why) = library::unplayable(&t.path) {
            return Some(format!("can't play {}: {why}", library::clean_title(t).0));
        }
        let mut start = 0;
        let mut tracks: Vec<Track> = vec![];
        for (n, &i) in self.rows.iter().enumerate() {
            let t = &self.lib.tracks[i];
            if library::unplayable(&t.path).is_none() {
                if n < self.sel {
                    start += 1;
                }
                tracks.push(t.clone());
            }
        }
        self.engine.send(Cmd::PlayList(tracks, start));
        None
    }

    fn toggle(&mut self) -> Option<String> {
        if self.engine.shared.lock().unwrap().track.is_none() {
            if self.rows.is_empty() && !self.loaded {
                self.play_when_loaded = true; // F12 before the library is in: play once it is (sync)
                return None;
            }
            return self.play_sel();
        }
        self.engine.send(Cmd::Toggle);
        None
    }

    fn volume(&mut self, d: f32) {
        let mut s = self.engine.shared.lock().unwrap();
        s.volume = ((s.volume + d) * 100.0).round().clamp(0.0, 100.0) / 100.0;
        drop(s);
        self.engine.send(Cmd::Persist);
    }

    fn set_volume(&mut self, v: f32) {
        self.engine.shared.lock().unwrap().volume = (v * 20.0).round().clamp(0.0, 20.0) / 20.0;
        self.engine.send(Cmd::Persist);
    }

    fn shuffle(&mut self) {
        let mut s = self.engine.shared.lock().unwrap();
        s.shuffle = !s.shuffle;
        drop(s);
        self.engine.send(Cmd::Persist);
    }

    fn repeat(&mut self) {
        let mut s = self.engine.shared.lock().unwrap();
        s.repeat = s.repeat.next();
        drop(s);
        self.engine.send(Cmd::Persist);
    }

    fn move_sel(&mut self, d: isize) {
        if self.rows.is_empty() {
            return;
        }
        let n = self.rows.len() as isize;
        self.sel = (self.sel as isize + d).clamp(0, n - 1) as usize;
    }

    /// Pull in whatever the background threads produced. Memory only, cheap: safe from render.
    fn sync(&mut self, cx: &Cx) {
        if !self.has_waker {
            *self.waker.lock().unwrap() = Some(cx.waker());
            self.has_waker = true;
        }
        // library
        let fresh = {
            let mut l = self.load.lock().unwrap();
            if l.version != self.load_gen {
                self.load_gen = l.version;
                self.status = l.status.clone();
                self.loaded = l.done;
                l.lib.take()
            } else {
                None
            }
        };
        if let Some(lib) = fresh {
            self.by_id = lib.tracks.iter().enumerate().map(|(i, t)| (t.id.clone(), i)).collect();
            self.lib = lib;
            self.refill();
            if self.play_when_loaded && !self.rows.is_empty() {
                // asked to play before the songs were in: pick up where you left off
                self.play_when_loaded = false;
                let last = self.state.lock().unwrap().last.clone();
                if let Some(n) = last.and_then(|id| self.rows.iter().position(|&i| self.lib.tracks[i].id == id)) {
                    self.sel = n;
                }
                self.play_sel();
            }
        }
        if self.loaded && self.rows.is_empty() {
            self.play_when_loaded = false; // nothing to play
        }
        // track change -> cover + lyrics
        let (changed, track, playing) = {
            let s = self.engine.shared.lock().unwrap();
            (s.changed, s.track.clone(), s.playing)
        };
        if changed != self.seen {
            self.seen = changed;
            if self.view == "most" {
                self.refill(); // play counts moved
            }
        }
        if let Some(t) = &track {
            if self.cover_for != t.id {
                self.cover_for = t.id.clone();
                self.cover_done = false;
                self.cover = None;
                let (t2, slot, w) = (t.clone(), self.cover_slot.clone(), self.waker.clone());
                std::thread::spawn(move || {
                    let c = cover::load(&t2, COVER_W, COVER_ROWS);
                    *slot.lock().unwrap() = Some((t2.id.clone(), c));
                    wake(&w);
                });
            }
            if self.lyrics_for != t.id {
                self.lyrics_for = t.id.clone();
                self.lyrics = LyricState::Looking;
                let (t2, slot, w) = (t.clone(), self.lyrics_slot.clone(), self.waker.clone());
                std::thread::spawn(move || {
                    let l = library::lyrics(&t2, true);
                    *slot.lock().unwrap() = Some((t2.id.clone(), l));
                    wake(&w);
                });
            }
        }
        if let Some((id, c)) = self.cover_slot.lock().unwrap().take() {
            if id == self.cover_for {
                self.cover = c;
                self.cover_done = true;
            }
        }
        if let Some((id, l)) = self.lyrics_slot.lock().unwrap().take() {
            if id == self.lyrics_for {
                self.lyrics = match l {
                    Some(l) => LyricState::Have(l),
                    None => LyricState::Off, // offline: say nothing rather than "no lyrics"
                };
            }
        }
        // spectrum
        let new = if playing {
            let sc = self.engine.scope.lock().unwrap();
            engine::spectrum(&sc.buf, sc.rate, BANDS)
        } else {
            vec![0.0; BANDS]
        };
        for (lv, n) in self.levels.iter_mut().zip(new) {
            *lv = n.max(*lv * 0.72); // quick attack, soft fall
        }
    }

    // ------------------------------------------------------------ drawing
    fn draw_np(&mut self, f: &mut Frame, r: Rect, s: &Snap, t: &Theme) {
        let Some(tr) = &s.track else {
            let y = r.y + r.height.saturating_sub(2) / 2;
            let lines = vec![
                Line::from(Span::styled("nothing playing", Style::default().add_modifier(Modifier::BOLD))),
                Line::from(Span::styled("pick a song and press enter", ui::muted(t))),
            ];
            f.render_widget(Paragraph::new(lines), Rect { y, height: 2.min(r.height), ..r });
            return;
        };
        let acc = self.cover.as_ref().and_then(|c| c.accent).unwrap_or(t.accent);
        let info_h: u16 = 5 + s.error.is_some() as u16;
        let spec_h: u16 = if r.height >= info_h + 10 { 5 } else { 0 };
        let spare = r.height.saturating_sub(info_h + 1 + spec_h + 1 + 3);
        let cover_rows = (spare as u32).min(COVER_ROWS) as u16;
        let cover_rows = if cover_rows < 6 { 0 } else { cover_rows };
        let mut y = r.y;
        // cover
        if cover_rows > 0 {
            let cw = cover_rows * 2; // square: 2 pixel rows per cell
            let cx0 = r.x + (r.width.min(COVER_W as u16).saturating_sub(cw)) / 2;
            match &self.cover {
                Some(c) => {
                    let buf = f.buffer_mut();
                    // sample the 36x36 bitmap down to cw x cover_rows*2 by nearest pixel
                    for row in 0..cover_rows {
                        for col in 0..cw {
                            let sx = (col as u32 * c.w as u32 / cw as u32) as usize;
                            let ty = (row as u32 * 2 * c.rows as u32 * 2 / (cover_rows as u32 * 2)) as usize;
                            let by = ((row as u32 * 2 + 1) * c.rows as u32 * 2 / (cover_rows as u32 * 2)) as usize;
                            let top = c.px[ty * c.w as usize + sx];
                            let bot = c.px[by * c.w as usize + sx];
                            let pos = Position { x: cx0 + col, y: y + row };
                            if let Some(cell) = buf.cell_mut(pos) {
                                cell.set_symbol("▀").set_fg(Color::Rgb(top[0], top[1], top[2])).set_bg(Color::Rgb(bot[0], bot[1], bot[2]));
                            }
                        }
                    }
                }
                None if self.cover_done => {
                    let msg = format!("{}no cover", lead(glyph("note")));
                    let p = Paragraph::new(Span::styled(msg, ui::muted(t))).centered();
                    f.render_widget(p, Rect { x: cx0, y: y + cover_rows / 2, width: cw, height: 1 });
                }
                None => {}
            }
            y += cover_rows + 1;
        }
        // title / artist
        let (title, artist) = library::clean_title(tr);
        let title = if title.is_empty() { tr.title.clone() } else { title };
        let w = r.width as usize;
        f.render_widget(Paragraph::new(Span::styled(ui::fit(&title, w), Style::default().fg(acc).add_modifier(Modifier::BOLD))), Rect { y, height: 1, ..r });
        f.render_widget(Paragraph::new(Span::styled(ui::fit(&artist, w), ui::muted(t))), Rect { y: y + 1, height: 1, ..r });
        y += 3;
        // progress
        let bar_w: u16 = 24;
        let frac = if s.dur > 0.0 { (s.pos / s.dur).clamp(0.0, 1.0) } else { 0.0 };
        let filled = (frac * bar_w as f64) as usize;
        let spans = vec![
            Span::styled(format!("{:>5} ", fmt(s.pos)), ui::muted(t)),
            Span::styled("━".repeat(filled), Style::default().fg(acc)),
            Span::styled("●", Style::default().fg(acc).add_modifier(Modifier::BOLD)),
            Span::styled("─".repeat(bar_w as usize - filled), Style::default().fg(t.frame)),
            Span::styled(format!(" {}", if s.dur > 0.0 { fmt(s.dur) } else { "–:––".into() }), ui::muted(t)),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), Rect { y, height: 1, ..r });
        self.hits.push((Rect { x: r.x + 6, y, width: bar_w + 1, height: 1 }, Hit::Progress));
        y += 1;
        // controls
        let on = Style::default().fg(acc).add_modifier(Modifier::BOLD);
        let off = Style::default().fg(t.muted);
        let rep = match s.repeat {
            Repeat::Off => glyph("repeat0"),
            Repeat::One => glyph("repeat1"),
            Repeat::All => glyph("repeat"),
        };
        let vol = (s.volume * 10.0 + 0.5) as usize;
        let parts: Vec<(String, Style, Option<Hit>)> = vec![
            (format!("  {}  ", glyph("shuffle")), if s.shuffle { on } else { off }, Some(Hit::Shuffle)),
            (format!(" {}  ", glyph("prev")), Style::default(), Some(Hit::Prev)),
            (format!(" {} ", glyph(if s.playing { "pause" } else { "play" })), Style::default().fg(acc).add_modifier(Modifier::BOLD | Modifier::REVERSED), Some(Hit::Toggle)),
            (format!("  {} ", glyph("next")), Style::default(), Some(Hit::Next)),
            (format!("  {} ", rep), if s.repeat == Repeat::Off { off } else { on }, Some(Hit::Repeat)),
            (format!("   {} ", glyph("volume")), ui::muted(t), None),
            ("▮".repeat(vol.min(10)), Style::default().fg(acc), Some(Hit::Volume)),
            ("▯".repeat(10 - vol.min(10)), off, Some(Hit::Volume)),
        ];
        let mut x = r.x;
        let mut spans = vec![];
        let mut vol_x = None;
        for (text, st, hit) in parts {
            let tw = text.width() as u16;
            match hit {
                Some(Hit::Volume) => {
                    vol_x.get_or_insert(x);
                }
                Some(h) => self.hits.push((Rect { x, y, width: tw, height: 1 }, h)),
                None => {}
            }
            x += tw;
            spans.push(Span::styled(text, st));
        }
        if let Some(vx) = vol_x {
            self.hits.push((Rect { x: vx, y, width: 10, height: 1 }, Hit::Volume));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), Rect { y, height: 1, ..r });
        y += 1;
        if let Some(e) = &s.error {
            f.render_widget(Paragraph::new(Span::styled(ui::fit(e, w), Style::default().fg(t.danger))), Rect { y, height: 1, ..r });
            y += 1;
        }
        y += 1;
        // spectrum
        if spec_h > 0 && y + spec_h <= r.bottom() {
            for row in 0..spec_h {
                let level = (spec_h - row) as f32; // this row covers (level-1, level]
                let mut line = String::new();
                for lv in &self.levels {
                    let h = lv * spec_h as f32;
                    let ch = if h >= level {
                        "█"
                    } else if h > level - 1.0 {
                        SPARK[((h - (level - 1.0)) * 8.0) as usize]
                    } else {
                        " "
                    };
                    line.push_str(ch);
                    line.push(' ');
                }
                let st = if row < 2 { Style::default().fg(t.shine) } else { Style::default().fg(acc) };
                let st = if acc != t.accent { Style::default().fg(acc) } else { st };
                f.render_widget(Paragraph::new(Span::styled(line, st)), Rect { y: y + row, height: 1, ..r });
            }
            y += spec_h + 1;
        }
        // lyrics
        if y >= r.bottom() {
            return;
        }
        let lr = Rect { y, height: r.bottom() - y, ..r };
        match &self.lyrics {
            LyricState::Looking => f.render_widget(Paragraph::new(Span::styled("looking for lyrics…", ui::muted(t))), Rect { height: 1, ..lr }),
            LyricState::Have(l) if l.is_empty() => f.render_widget(Paragraph::new(Span::styled("no synced lyrics", ui::muted(t))), Rect { height: 1, ..lr }),
            LyricState::Have(l) => {
                let now = s.pos + 0.3;
                let idx = l.iter().rposition(|(ts, _)| *ts <= now).map(|i| i as isize).unwrap_or(-1);
                let first = (idx - 2).max(0) as usize;
                let lines: Vec<Line> = (first..l.len())
                    .take(lr.height as usize)
                    .map(|i| {
                        let text = if l[i].1.is_empty() { glyph("note").to_string() } else { l[i].1.clone() };
                        if i as isize == idx {
                            Line::from(Span::styled(format!("› {}", ui::fit(&text, w.saturating_sub(2))), Style::default().fg(acc).add_modifier(Modifier::BOLD)))
                        } else if (i as isize) < idx {
                            Line::from(Span::styled(format!("  {}", ui::fit(&text, w.saturating_sub(2))), ui::muted(t)))
                        } else {
                            Line::from(Span::raw(format!("  {}", ui::fit(&text, w.saturating_sub(2)))))
                        }
                    })
                    .collect();
                f.render_widget(Paragraph::new(lines), lr);
            }
            LyricState::Off => {}
        }
    }

    /// Two-line now-playing strip for narrow panes (no room for the column).
    fn draw_strip(&mut self, f: &mut Frame, r: Rect, s: &Snap, t: &Theme) {
        let Some(tr) = &s.track else {
            f.render_widget(Paragraph::new(Line::from(vec![
                Span::styled("nothing playing  ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled("pick a song and press enter", ui::muted(t)),
            ])), Rect { height: 1, ..r });
            return;
        };
        let (title, artist) = library::clean_title(tr);
        let head = format!("{}{title}", lead(glyph(if s.playing { "play" } else { "pause" })));
        let line = Line::from(vec![
            Span::styled(ui::fit(&head, r.width as usize / 2 + 8), Style::default().fg(t.accent).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {artist}"), ui::muted(t)),
        ]);
        f.render_widget(Paragraph::new(line), Rect { height: 1, ..r });
        let bar_w = r.width.saturating_sub(14) as usize;
        let frac = if s.dur > 0.0 { (s.pos / s.dur).clamp(0.0, 1.0) } else { 0.0 };
        let filled = (frac * bar_w as f64) as usize;
        let spans = vec![
            Span::styled(format!("{:>5} ", fmt(s.pos)), ui::muted(t)),
            Span::styled("━".repeat(filled), Style::default().fg(t.accent)),
            Span::styled("─".repeat(bar_w - filled), Style::default().fg(t.frame)),
            Span::styled(format!(" {}", fmt(s.dur)), ui::muted(t)),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), Rect { y: r.y + 1, height: 1, ..r });
        self.hits.push((Rect { x: r.x + 6, y: r.y + 1, width: bar_w as u16, height: 1 }, Hit::Progress));
    }

    fn draw_search(&mut self, f: &mut Frame, r: Rect, t: &Theme) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if self.searching { t.accent } else { t.frame }));
        let inner = block.inner(r);
        f.render_widget(block, r);
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
        let line = if self.query.is_empty() && !self.searching {
            Line::from(Span::styled("search songs, artists…   (/)", ui::muted(t)))
        } else {
            let q = self.query.clone();
            let w = inner.width.saturating_sub(1) as usize;
            let shown = if q.width() > w { q.chars().rev().take(w).collect::<Vec<_>>().into_iter().rev().collect() } else { q };
            let mut v = vec![Span::raw(shown)];
            if self.searching {
                v.push(Span::styled(" ", Style::default().add_modifier(Modifier::REVERSED)));
            }
            Line::from(v)
        };
        f.render_widget(Paragraph::new(line), inner);
        self.hits.push((r, Hit::Search));
    }

    fn draw_table(&mut self, f: &mut Frame, r: Rect, s: &Snap, t: &Theme) {
        if r.height < 2 {
            return;
        }
        let most = self.view == "most";
        let bar = 1u16; // scrollbar column
        let w = r.width.saturating_sub(bar + 1);
        let (mark_w, num_w, time_w, plays_w) = (2u16, 5u16, 6u16, if most { 6u16 } else { 0 });
        let artist_w = (w / 4).clamp(10, 30);
        let title_w = w.saturating_sub(mark_w + num_w + time_w + plays_w + artist_w + 2);
        let cols = [mark_w, num_w, title_w + 2, artist_w + 1, time_w, plays_w];
        let cell = |text: &str, cw: u16| -> String {
            let cw = cw as usize;
            let s = ui::fit(text, cw.saturating_sub(1).max(1));
            let pad = cw.saturating_sub(s.width());
            format!("{s}{}", " ".repeat(pad))
        };
        let head_st = Style::default().fg(t.fg).add_modifier(Modifier::BOLD);
        let mut head = vec![Span::raw(cell("", cols[0])), Span::styled(cell("#", cols[1]), head_st), Span::styled(cell("title", cols[2]), head_st), Span::styled(cell("artist", cols[3]), head_st), Span::styled(cell("time", cols[4]), head_st)];
        if most {
            head.push(Span::styled(cell("plays", cols[5]), head_st));
        }
        f.render_widget(Paragraph::new(Line::from(head)), Rect { height: 1, ..r });
        let body = Rect { y: r.y + 1, height: r.height - 1, ..r };
        let h = body.height as usize;
        self.table_h = h;
        if self.rows.is_empty() {
            let (a, b) = if !self.loaded && self.lib.tracks.is_empty() {
                (self.status.clone(), String::new())
            } else if !self.query.is_empty() {
                (format!("nothing matches “{}”", self.query), "esc clears the search".into())
            } else if self.lib.tracks.is_empty() {
                (format!("no songs found{}", if self.lib.source.is_empty() { String::new() } else { format!(" in {}", self.lib.source) }), "add folders under [music] in config.toml, then R rescans".into())
            } else if most {
                ("nothing played yet".into(), String::new())
            } else {
                ("this playlist is empty".into(), String::new())
            };
            let lines = vec![Line::from(Span::styled(a, ui::muted(t))), Line::from(Span::styled(b, ui::muted(t)))];
            f.render_widget(Paragraph::new(lines), Rect { x: body.x + mark_w, width: body.width.saturating_sub(mark_w), y: body.y + 1, height: 2.min(body.height.saturating_sub(1)) });
            return;
        }
        // keep the cursor in view
        if self.sel < self.scroll {
            self.scroll = self.sel;
        } else if self.sel >= self.scroll + h {
            self.scroll = self.sel + 1 - h;
        }
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(h));
        let cur = s.track.as_ref().map(|t| t.id.as_str());
        for (n, &ti) in self.rows.iter().enumerate().skip(self.scroll).take(h) {
            let tr = &self.lib.tracks[ti];
            let y = body.y + (n - self.scroll) as u16;
            let playing = cur == Some(tr.id.as_str());
            let is_sel = n == self.sel;
            let base = if is_sel { Style::default().bg(t.frame).add_modifier(Modifier::BOLD) } else { Style::default() };
            let fg = |c: Color| base.fg(c);
            let cant = library::unplayable(&tr.path);
            let (title_st, meta_st, artist_st) = if playing {
                (fg(t.accent).add_modifier(Modifier::BOLD), fg(t.accent), fg(t.accent))
            } else if cant.is_some() {
                (fg(t.muted), fg(t.muted), fg(t.muted))
            } else {
                (fg(t.fg), fg(t.muted), fg(t.fg))
            };
            let title = match cant {
                Some(why) => format!("{} · {why}", tr.title),
                None => tr.title.clone(),
            };
            let mark = if playing { glyph(if s.playing { "play" } else { "pause" }) } else { "" };
            let mut spans = vec![
                Span::styled(cell(mark, cols[0]), fg(t.accent)),
                Span::styled(cell(&(n + 1).to_string(), cols[1]), meta_st),
                Span::styled(cell(&title, cols[2]), title_st),
                Span::styled(cell(&tr.artist, cols[3]), artist_st),
                Span::styled(cell(&if tr.duration > 0.0 { fmt(tr.duration) } else { String::new() }, cols[4]), meta_st),
            ];
            if most {
                spans.push(Span::styled(cell(&self.plays(tr).to_string(), cols[5]), meta_st));
            }
            let row = Rect { y, height: 1, width: r.width.saturating_sub(bar + 1), ..body };
            // fill the whole row so the cursor bar spans the table width
            f.render_widget(Paragraph::new(Line::from(spans)).style(base), row);
            self.hits.push((row, Hit::Row(n)));
        }
        // scrollbar
        let total = self.rows.len();
        if total > h {
            let x = r.right() - 1;
            let thumb = ((h * h) / total).max(1);
            let top = self.scroll * (h - thumb) / (total - h).max(1);
            for i in 0..h {
                let (sym, st) = if i >= top && i < top + thumb { ("┃", Style::default().fg(t.muted)) } else { ("│", Style::default().fg(t.frame)) };
                f.render_widget(Paragraph::new(Span::styled(sym, st)), Rect { x, y: body.y + i as u16, width: 1, height: 1 });
            }
        }
    }

    /// A click on something; returns a message to show, if any.
    fn click(&mut self, hit: Hit, rect: Rect, col: u16) -> Option<String> {
        match hit {
            Hit::Row(n) => {
                let again = self.last_click.map(|(m, at)| m == n && at.elapsed() < Duration::from_millis(500)).unwrap_or(false);
                if self.sel == n && (again || self.last_click.map(|c| c.0 == n).unwrap_or(false)) {
                    self.last_click = None;
                    return self.play_sel();
                } else {
                    self.sel = n;
                    self.last_click = Some((n, Instant::now()));
                }
                self.searching = false;
            }
            Hit::Search => self.searching = true,
            Hit::Shuffle => self.shuffle(),
            Hit::Prev => self.engine.send(Cmd::Prev),
            Hit::Toggle => return self.toggle(),
            Hit::Next => self.engine.send(Cmd::Next),
            Hit::Repeat => self.repeat(),
            Hit::Progress => {
                let dur = self.engine.shared.lock().unwrap().dur;
                let frac = (col.saturating_sub(rect.x)) as f64 / rect.width.max(1) as f64;
                if dur > 0.0 {
                    self.engine.send(Cmd::Seek(frac * dur));
                }
            }
            Hit::Volume => {
                let v = (col.saturating_sub(rect.x) + 1) as f32 / rect.width.max(1) as f32;
                self.set_volume(v);
            }
        }
        None
    }
}

/// Load the library off the UI thread: audio-player's library.json when it's there, else a folder scan (names
/// first, tags filled in as they're read; tags read before come from the cache, so only new or changed files are
/// probed). Calling it again (R) supersedes a scan still running.
fn spawn_loader(folders: Vec<String>, slot: Arc<Mutex<Load>>, waker: Arc<Mutex<Option<Waker>>>) {
    let run = {
        let mut l = slot.lock().unwrap();
        l.run += 1;
        l.run
    };
    std::thread::Builder::new()
        .name("oriel-music-scan".into())
        .spawn(move || {
            // false once a newer scan has started: this one stops
            let publish = |lib: Option<Lib>, status: String, done: bool| -> bool {
                let mut l = slot.lock().unwrap();
                if l.run != run {
                    return false;
                }
                if lib.is_some() {
                    l.lib = lib;
                }
                l.status = status;
                l.done = done;
                l.version += 1;
                drop(l);
                wake(&waker);
                true
            };
            let mut note = String::new();
            if let Some(dir) = library::app_dir() {
                match library::load_app(&dir) {
                    Ok(lib) => {
                        publish(Some(lib), String::new(), true);
                        return;
                    }
                    Err(e) => note = e,
                }
            }
            let roots = library::scan_roots(&folders);
            let source = roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(", ");
            if roots.is_empty() {
                publish(Some(Lib::default()), note, true);
                return;
            }
            if !publish(None, format!("scanning {source}…"), false) {
                return;
            }
            let files = library::find_audio(&roots);
            let mut tracks: Vec<Track> = files.iter().map(|p| library::bare_track(p)).collect();
            let tags_file = library::tags_path();
            let mut cache = library::load_tags(&tags_file);
            let todo = cache.apply(&mut tracks);
            let n = todo.len();
            let lib = |tracks: &Vec<Track>| Some(Lib { tracks: tracks.clone(), playlists: vec![], source: source.clone() });
            if n > 0 && !publish(lib(&tracks), format!("reading tags… 0/{n}"), false) {
                return;
            }
            for (k, &i) in todo.iter().enumerate() {
                library::read_tags(&mut tracks[i]);
                cache.put(&tracks[i]);
                if k % 50 == 49 && k + 1 < n && !publish(lib(&tracks), format!("reading tags… {}/{n}", k + 1), false) {
                    return;
                }
            }
            let before = cache.files.len();
            cache.keep_only(&tracks);
            if n > 0 || cache.files.len() != before {
                library::save_tags(&tags_file, &cache);
            }
            publish(lib(&tracks), note, true);
        })
        .ok();
}

impl Pane for Music {
    fn title(&self) -> String {
        format!("music · {}", self.view_name())
    }
    fn icon(&self) -> &'static str {
        "music"
    }
    fn subtitle(&self) -> Option<String> {
        if !self.loaded && self.lib.tracks.is_empty() {
            return Some("loading…".into());
        }
        let n = self.rows.len();
        let mut s = format!("{n} song{}", if n == 1 { "" } else { "s" });
        if !self.query.is_empty() {
            s.push_str(&format!(" matching “{}”", self.query));
        }
        if !self.loaded {
            s.push_str(&format!(" · {}", self.status));
        }
        Some(s)
    }
    fn badge(&self) -> Option<String> {
        let s = self.engine.shared.lock().unwrap();
        let t = s.track.as_ref()?;
        let (title, _) = library::clean_title(t);
        let title = if title.is_empty() { t.title.clone() } else { title };
        Some(format!("{} {title}", if s.playing { "▶" } else { "‖" }))
    }
    fn tick_every(&self) -> Option<Duration> {
        // progress bar, spectrum and lyrics move only while playing; otherwise wakes are enough
        if self.engine.shared.lock().unwrap().playing { Some(Duration::from_millis(100)) } else { None }
    }
    fn poll(&mut self, cx: &mut Cx) {
        self.sync(cx);
    }

    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        self.sync(cx);
        self.hits.clear();
        let t = cx.theme;
        let hints: &[(&str, &str)] = if self.searching {
            &[("type", "to filter"), ("↑/↓", "move"), ("enter", "done"), ("esc", "clear")]
        } else {
            &[("space", "play/pause"), ("←/→", "seek"), ("n/p", "next/prev"), ("+/-", "volume"), ("s", "shuffle"), ("r", "repeat"), ("/", "search"), ("enter", "play"), ("R", "rescan")]
        };
        // drop hints from the end until the line fits (narrow panes)
        let mut n = hints.len();
        while n > 1 && hints[..n].iter().map(|(k, w)| k.width() + w.width() + 4).sum::<usize>() > area.width as usize {
            n -= 1;
        }
        let hints = &hints[..n];
        let body = ui::hint_line(f, area, hints, t);
        let body = Rect { y: body.y + 1, height: body.height.saturating_sub(2), ..body }; // a row of air top + bottom
        let s = self.snap();
        let wide = body.width >= 100 && body.height >= 16;
        let list = if wide {
            let np = Rect { x: body.x + 2, width: NP_W, ..body };
            self.draw_np(f, np, &s, t);
            Rect { x: np.right() + 3, width: body.right().saturating_sub(np.right() + 4), ..body }
        } else {
            let strip = Rect { x: body.x + 1, width: body.width.saturating_sub(2), height: 2.min(body.height), ..body };
            self.draw_strip(f, strip, &s, t);
            Rect { x: body.x + 1, width: body.width.saturating_sub(2), y: body.y + 3, height: body.height.saturating_sub(3) }
        };
        if list.height >= 5 {
            self.draw_search(f, Rect { height: 3, ..list }, t);
            self.draw_table(f, Rect { y: list.y + 3, height: list.height - 3, ..list }, &s, t);
        }
    }

    fn key(&mut self, key: KeyEvent, cx: &mut Cx) -> bool {
        let typed = ui::typed_char(&key); // AltGr chars included
        if key.modifiers.intersects(KeyModifiers::ALT) && typed.is_none() {
            return false;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL) && typed.is_none();
        // F12 sends a space to this pane from anywhere: when we aren't the focused pane it's always play/pause
        if self.searching && cx.focused {
            match key.code {
                KeyCode::Esc => {
                    self.query.clear();
                    self.searching = false;
                    self.refill();
                }
                KeyCode::Enter => {
                    self.searching = false;
                }
                KeyCode::Backspace => {
                    self.query.pop();
                    self.refill();
                }
                KeyCode::Char('u') if ctrl => {
                    self.query.clear();
                    self.refill();
                }
                KeyCode::Char(c) if typed.is_some() => {
                    self.query.push(c);
                    self.refill();
                    self.scroll = 0;
                }
                KeyCode::Down => self.move_sel(1),
                KeyCode::Up => self.move_sel(-1),
                _ => return ctrl == false && !matches!(key.code, KeyCode::F(_) | KeyCode::Tab | KeyCode::BackTab),
            }
            return true;
        }
        if ctrl {
            return false;
        }
        let page = self.table_h.max(1) as isize;
        match key.code {
            KeyCode::Char(' ') | KeyCode::Enter => {
                let msg = if key.code == KeyCode::Enter { self.play_sel() } else { self.toggle() };
                if let Some(m) = msg {
                    cx.notify(m);
                }
            }
            KeyCode::Left => self.engine.send(Cmd::Nudge(-5.0)),
            KeyCode::Right => self.engine.send(Cmd::Nudge(5.0)),
            KeyCode::Char('n') => self.engine.send(Cmd::Next),
            KeyCode::Char('p') => self.engine.send(Cmd::Prev),
            KeyCode::Char('+') | KeyCode::Char('=') => self.volume(0.05),
            KeyCode::Char('-') | KeyCode::Char('_') => self.volume(-0.05),
            KeyCode::Char('s') => self.shuffle(),
            KeyCode::Char('r') => self.repeat(),
            KeyCode::Char('R') => {
                // the folders again (new songs, the config's folders now); tags already read are remembered
                self.loaded = false;
                self.status = "rescanning…".into();
                (self.scan)(cx.config.music.folders.clone(), self.load.clone(), self.waker.clone());
                cx.notify(format!("{}rescanning your music", ui::lead("music")));
            }
            KeyCode::Char('/') => self.searching = true,
            KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
            KeyCode::PageDown => self.move_sel(page),
            KeyCode::PageUp => self.move_sel(-page),
            KeyCode::Home | KeyCode::Char('g') => self.sel = 0,
            KeyCode::End | KeyCode::Char('G') => self.sel = self.rows.len().saturating_sub(1),
            KeyCode::Tab | KeyCode::BackTab => {
                let v = self.views();
                let i = v.iter().position(|x| *x == self.view).unwrap_or(0);
                let n = v.len();
                let j = if key.code == KeyCode::Tab { (i + 1) % n } else { (i + n - 1) % n };
                self.set_view(&v[j].clone());
            }
            KeyCode::Esc if !self.query.is_empty() => {
                self.query.clear();
                self.refill();
            }
            _ => return false,
        }
        true
    }

    fn mouse(&mut self, ev: MouseEvent, _area: Rect, cx: &mut Cx) {
        let pos = Position { x: ev.column, y: ev.row };
        match ev.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let over_vol = self.hits.iter().any(|(r, h)| *h == Hit::Volume && r.contains(pos));
                let down = ev.kind == MouseEventKind::ScrollDown;
                if over_vol {
                    self.volume(if down { -0.05 } else { 0.05 });
                } else {
                    let max = self.rows.len().saturating_sub(self.table_h);
                    self.scroll = if down { (self.scroll + 3).min(max) } else { self.scroll.saturating_sub(3) };
                    // drag the cursor along so the keep-in-view logic doesn't undo the scroll
                    let h = self.table_h.max(1);
                    self.sel = self.sel.clamp(self.scroll, (self.scroll + h - 1).min(self.rows.len().saturating_sub(1)));
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((r, h)) = self.hits.iter().find(|(r, _)| r.contains(pos)).copied() {
                    if let Some(m) = self.click(h, r, ev.column) {
                        cx.notify(m);
                    }
                } else {
                    self.searching = false;
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ sidebar section
    fn side(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        self.sync(cx);
        let t = cx.theme;
        self.side_hits.clear();
        let mut y = area.y;
        let mut row = |f: &mut Frame, y: &mut u16, icon: &str, label: &str, right: String, id: Option<&str>| {
            if *y >= area.bottom() {
                return;
            }
            let r = Rect { y: *y, height: 1, ..area };
            let on = id.map(|i| i == self.view).unwrap_or(false);
            let st = if on { Style::default().fg(t.accent).add_modifier(Modifier::BOLD) } else { Style::default() };
            let rw = right.width() as u16;
            let left_w = r.width.saturating_sub(rw + 1) as usize;
            let text = ui::fit(&format!("{}{label}", lead(icon)), left_w);
            let pad = left_w.saturating_sub(text.width());
            let spans = vec![
                Span::styled(format!("{text}{}", " ".repeat(pad)), st),
                Span::styled(format!(" {right}"), if on { st } else { ui::muted(t) }),
            ];
            f.render_widget(Paragraph::new(Line::from(spans)), r);
            if let Some(i) = id {
                self.side_hits.push((r, i.to_string()));
            }
            *y += 1;
        };
        row(f, &mut y, glyph("library"), "library", self.lib.tracks.len().to_string(), Some("library"));
        row(f, &mut y, glyph("most"), "most played", String::new(), Some("most"));
        if !self.lib.playlists.is_empty() {
            y += 1;
            if y < area.bottom() {
                f.render_widget(Paragraph::new(Span::styled("── playlists", ui::muted(t))), Rect { y, height: 1, ..area });
                y += 1;
            }
            let pls: Vec<(String, String, usize)> = self.lib.playlists.iter().map(|p| (p.id.clone(), p.name.clone(), p.ids.iter().filter(|i| self.by_id.contains_key(*i)).count())).collect();
            for (id, name, n) in pls {
                row(f, &mut y, glyph("playlist"), &name, n.to_string(), Some(&id));
            }
        }
    }

    fn side_mouse(&mut self, ev: MouseEvent, _area: Rect, _cx: &mut Cx) {
        if let MouseEventKind::Down(MouseButton::Left) = ev.kind {
            let pos = Position { x: ev.column, y: ev.row };
            if let Some((_, id)) = self.side_hits.iter().find(|(r, _)| r.contains(pos)).cloned() {
                self.set_view(&id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Kit;

    /// A Music with a fixed library and no disk writes, for layout tests.
    fn fake(k: &Kit, n: usize) -> Music {
        let state = Arc::new(Mutex::new(State::default()));
        let waker = Arc::new(Mutex::new(None));
        let engine = Engine::new(state.clone(), waker.clone(), false);
        let tracks: Vec<Track> = (0..n)
            .map(|i| Track { id: format!("t{i}"), path: format!("/nope/{i}.mp3").into(), title: format!("Artist {i} - Song number {i} (Official Video)"), artist: format!("Artist {i}"), duration: 100.0 + i as f64 * 7.0, plays: (i % 4) as u64, ..Default::default() })
            .collect();
        let playlists = vec![library::Playlist { id: "p1".into(), name: "Vibe Coding".into(), ids: vec!["t1".into(), "t3".into(), "t5".into()] }];
        let load = Arc::new(Mutex::new(Load { lib: Some(Lib { tracks, playlists, source: "test".into() }), status: String::new(), done: true, version: 1, run: 1 }));
        let _ = k;
        let mut m = Music::build(state, waker, engine, load);
        m.scan = fake_scan;
        m
    }

    /// A "rescan" that finds five songs, without touching the disk.
    fn fake_scan(_folders: Vec<String>, slot: Arc<Mutex<Load>>, waker: Arc<Mutex<Option<Waker>>>) {
        let mut l = slot.lock().unwrap();
        l.run += 1;
        let tracks = (0..5).map(|i| Track { id: format!("t{i}"), path: format!("/nope/{i}.mp3").into(), title: format!("Rescanned {i}"), ..Default::default() }).collect();
        l.lib = Some(Lib { tracks, playlists: vec![], source: "test".into() });
        (l.status, l.done) = (String::new(), true);
        l.version += 1;
        drop(l);
        wake(&waker);
    }

    #[test]
    fn music_layout_search_views() {
        let mut k = Kit::new();
        let mut p = fake(&k, 30);
        let s = k.render(&mut p, 150, 44);
        assert!(s.contains("nothing playing"), "{s}");
        assert!(s.contains("pick a song and press enter"));
        assert!(s.contains("search songs, artists"));
        assert!(s.contains("Song number 0"));
        assert_eq!(p.subtitle().as_deref(), Some("30 songs"));
        // search
        k.key(&mut p, KeyCode::Char('/'));
        k.typ(&mut p, "number 2");
        let s = k.render(&mut p, 150, 44);
        assert!(s.contains("Song number 2") && s.contains("Song number 21") && !s.contains("Song number 3 "), "{s}");
        assert_eq!(p.rows.len(), 11); // 2, 20-29
        k.key(&mut p, KeyCode::Esc);
        assert_eq!(p.rows.len(), 30);
        // the cursor stays on the song it was on (Song number 2) when the search clears
        assert_eq!(p.sel, 2);
        k.key(&mut p, KeyCode::Char('j'));
        k.key(&mut p, KeyCode::Down);
        assert_eq!(p.sel, 4);
        // views via the sidebar
        let side = k.render_side(&mut p, 32, 12);
        assert!(side.contains("library") && side.contains("30") && side.contains("most played") && side.contains("Vibe Coding"), "{side}");
        k.key(&mut p, KeyCode::Tab);
        assert_eq!(p.view, "most");
        assert!(p.title().contains("most played"));
        // most played is sorted by plays, zero-play songs left out
        assert!(p.rows.iter().all(|&i| p.lib.tracks[i].plays > 0));
        assert_eq!(p.lib.tracks[p.rows[0]].plays, 3);
        k.key(&mut p, KeyCode::Tab);
        assert_eq!(p.view, "p1");
        assert_eq!(p.rows.len(), 3);
        let _ = k.render_html(&mut p, 150, 44, "target/snap/music-playlist.html");
        // narrow pane still works
        let s = k.render(&mut p, 70, 20);
        assert!(s.contains("nothing playing"));
    }

    #[test]
    fn music_opus_rows_are_shown_but_never_queued() {
        let mut k = Kit::new();
        let mut p = fake(&k, 4);
        k.render(&mut p, 150, 30); // loads the library
        // song 1 is an .opus file (yt-dlp's default): listed, muted, marked, and left out of the queue
        p.lib.tracks[1].path = "/nope/1.opus".into();
        let s = k.render_html(&mut p, 150, 30, "target/snap/music-opus.html");
        assert!(s.contains("no opus decoder"), "{s}");
        k.key(&mut p, KeyCode::Char('j'));
        k.key(&mut p, KeyCode::Enter);
        assert!(k.notices().iter().any(|n| n.contains("no opus decoder")), "{:?}", k.notices());
        // play from song 0: the queue holds the other three; none of them exist, so each is tried once and the
        // error stays up (never touching the audio device: they fail before it would open)
        k.key(&mut p, KeyCode::Char('k'));
        k.key(&mut p, KeyCode::Enter);
        for _ in 0..40 {
            k.wait_wake(&mut p, 50);
            if p.snap().error.is_some() {
                break;
            }
        }
        let sh = p.engine.shared.lock().unwrap();
        assert_eq!(sh.queue.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["t0", "t2", "t3"]);
        assert!(!sh.playing && sh.error.as_deref().is_some_and(|e| e.starts_with("can't open")), "{:?}", sh.error);
        assert_eq!(sh.index, 2, "tried to the end of the list, then stopped");
    }

    /// F12 / /play before the library has loaded: remembered, and the last song played starts once it's in.
    #[test]
    fn music_play_before_the_library_is_in() {
        let mut k = Kit::new();
        let state = Arc::new(Mutex::new(State { last: Some("t2".into()), ..State::default() }));
        let waker = Arc::new(Mutex::new(None));
        let engine = Engine::new(state.clone(), waker.clone(), false);
        let load = Arc::new(Mutex::new(Load::default()));
        let mut p = Music::build(state, waker, engine, load.clone());
        // not focused: F12 from another app (App::key sends it as AppKey("music", ' '))
        let mut cx_actions = vec![];
        let mut cx = crate::pane::Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: &mut cx_actions, focused: false, time: 1.0 };
        p.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &mut cx);
        assert!(p.play_when_loaded);
        // the library arrives (none of these files exist, so the engine reports it can't open the song)
        let tracks: Vec<Track> = (0..4).map(|i| Track { id: format!("t{i}"), path: format!("/nope/{i}.mp3").into(), title: format!("song {i}"), ..Default::default() }).collect();
        *load.lock().unwrap() = Load { lib: Some(Lib { tracks, playlists: vec![], source: "test".into() }), status: String::new(), done: true, version: 1, run: 1 };
        k.render(&mut p, 150, 30);
        assert!(!p.play_when_loaded);
        assert_eq!(p.sel, 2, "the last song played is picked");
        for _ in 0..40 {
            k.wait_wake(&mut p, 50);
            if p.snap().error.is_some() {
                break;
            }
        }
        let sh = p.engine.shared.lock().unwrap();
        assert_eq!(sh.queue.first().map(|t| t.id.as_str()), Some("t0"), "the whole view is queued");
        assert!(sh.track.is_some(), "it tried to play");
    }

    #[test]
    fn music_rescan_key() {
        let mut k = Kit::new();
        let mut p = fake(&k, 3);
        k.render(&mut p, 150, 30);
        let run = p.load.lock().unwrap().run;
        k.key(&mut p, KeyCode::Char('R'));
        assert!(!p.loaded && p.load.lock().unwrap().run == run + 1, "a new scan, which an older one can't publish over");
        assert!(k.notices().iter().any(|n| n.contains("rescanning")));
        let s = k.render(&mut p, 150, 30);
        assert!(p.loaded && p.lib.tracks.len() == 5 && s.contains("Rescanned 4"), "{s}");
        // the empty state says how to get songs in, and that R looks again
        let mut p = fake(&k, 0);
        let s = k.render(&mut p, 150, 30);
        assert!(s.contains("no songs found") && s.contains("R rescans"), "{s}");
    }

    #[test]
    fn music_types_altgr_chars_in_search() {
        let mut k = Kit::new();
        let mut p = fake(&k, 3);
        k.key(&mut p, KeyCode::Char('/'));
        k.typ(&mut p, "ac");
        k.key_mod(&mut p, KeyCode::Char('\\'), KeyModifiers::CONTROL | KeyModifiers::ALT);
        k.key_mod(&mut p, KeyCode::Char('{'), KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
        assert_eq!(p.query, "ac\\{");
    }

    #[test]
    fn music_snapshot_real_library() {
        // whatever this machine has: the audio-player library on Windows, else a folder scan
        let mut k = Kit::new();
        let mut p = Music::new(&k.config);
        for _ in 0..50 {
            k.render(&mut p, 150, 44);
            if p.loaded {
                break;
            }
            k.wait_wake(&mut p, 100);
        }
        let s = k.render_html(&mut p, 150, 44, "target/snap/music-main.html");
        println!("{s}");
        let side = k.render_side(&mut p, 32, 14);
        println!("{side}");
        assert!(p.loaded);
    }

    /// Plays the first song for about a second at low volume, then stops. Skips when there's no music or no
    /// audio device (CI, headless Linux).
    #[test]
    #[ignore] // plays real audio out loud: cargo test music_plays_a_song -- --ignored
    fn music_plays_a_song() {
        let mut k = Kit::new();
        let state = Arc::new(Mutex::new(State { volume: 0.12, ..State::default() }));
        let waker = Arc::new(Mutex::new(None));
        let engine = Engine::new(state.clone(), waker.clone(), false);
        let load = Arc::new(Mutex::new(Load::default()));
        spawn_loader(k.config.music.folders.clone(), load.clone(), waker.clone());
        let mut p = Music::build(state.clone(), waker, engine, load);
        for _ in 0..50 {
            k.render(&mut p, 150, 44);
            if p.loaded {
                break;
            }
            k.wait_wake(&mut p, 100);
        }
        if p.rows.is_empty() {
            eprintln!("no music on this machine; skipping playback test");
            return;
        }
        k.key(&mut p, KeyCode::Enter);
        k.wait_wake(&mut p, 1500);
        let s = p.snap();
        if let Some(e) = &s.error {
            if e.starts_with("audio device") {
                eprintln!("{e}; skipping");
                return;
            }
        }
        assert!(s.playing, "not playing: {:?}", s.error);
        assert!(s.pos > 0.3, "position didn't move: {}", s.pos);
        assert!(p.badge().unwrap().starts_with('▶'));
        assert_eq!(state.lock().unwrap().plays.values().sum::<u64>(), 0, "a second of it isn't a play yet");
        // seek + pause
        k.key(&mut p, KeyCode::Right);
        k.wait_wake(&mut p, 300);
        assert!(p.snap().pos >= 5.0, "seek: {}", p.snap().pos);
        // into the song a bit, so the snapshot has lyrics and a spectrum
        p.engine.send(Cmd::Seek(58.0));
        k.wait_wake(&mut p, 700);
        let _ = k.render_html(&mut p, 150, 44, "target/snap/music-playing.html");
        let _ = k.render_html(&mut p, 90, 30, "target/snap/music-narrow.html");
        k.key(&mut p, KeyCode::Char(' '));
        k.wait_wake(&mut p, 200);
        assert!(!p.snap().playing);
        assert!(p.badge().unwrap().starts_with('‖'));
        p.engine.send(Cmd::Stop);
        k.wait_wake(&mut p, 200);
        assert!(p.snap().track.is_none());
    }
}
