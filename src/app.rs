//! The workspace: tabs, each a tree of split panes; focus, key bindings, the palette, mouse, themes.
//! Event-driven: it sleeps until input, pane output or a tick a visible pane asked for, then redraws once.

use crate::config::{self, Config};
use crate::layout::{Dir, Node, PaneId, neighbor, split_rect};
use crate::pane::{Action, Activity, Cx, Event, Pane, Place};
use crate::panes::{self, APPS, available};
use crate::theme::{self, Theme};
use crate::ui;
use crossterm::event::{Event as CEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

struct Tab {
    /// Stable for the tab's life: menus, renames and key handlers keep this, not an index that shifts as tabs
    /// open and close in the background.
    id: u64,
    /// Some(name) for the pinned app tabs in the sidebar (ai, music, ...); None for tabs you made.
    app: Option<&'static str>,
    /// A name you gave it (herdr style); None = named after its focused pane.
    name: Option<String>,
    root: Node,
    focus: PaneId,
    zoom: bool,
}

/// The sidebar's app list, nest style: (app, icon, label, key).
pub const SIDEBAR: &[(&str, &str, &str, &str)] = &[
    // ── ai
    ("ai", "ai", "chat", "F1"),
    ("agents", "robot", "agents", "F2"),
    ("ais", "gauge", "your AIs", "F3"),
    // ── tools
    ("music", "music", "music", "F4"),
    ("system", "system", "system", "F5"),
    ("files", "files", "files", "F6"),
    ("notes", "notes", "notes", "F7"),
    ("calendar", "calendar", "calendar", ""),
    ("storage", "storage", "storage", "F8"),
    ("terminal", "term", "terminal", "F9"),
    // ── pinned to the bottom of the sidebar
    ("alerts", "bell", "alerts", ""),
    ("themes", "theme", "themes", ""),
    ("help", "search", "help", "F10"),
];
/// SIDEBAR entries from here on sit at the bottom of the sidebar, not in a section.
const FOOTER: usize = 10;
/// Where each sidebar section starts: (index into SIDEBAR, heading).
const SECTIONS: &[(usize, &str)] = &[(0, "ai"), (3, "tools")];

/// Tabs in hits and commands are by Tab::id.
#[derive(Clone, Copy)]
enum SideHit {
    App(&'static str),
    Tab(u64),
    /// the × on one of your tabs
    CloseTab(u64),
    NewTab,
}

#[derive(Clone)]
enum Cmd {
    App(&'static str),
    Rename,
    CloseTab(u64),
    Tour,
    Open(&'static str, Place),
    Theme(String),
    SplitRight,
    SplitDown,
    Close,
    Zoom,
    NewTab,
    Icons,
    Help,
    Quit,
}

/// A mouse selection inside one pane.
#[derive(Clone, Copy)]
struct Sel {
    pane: PaneId,
    area: Rect,
    a: Position,
    b: Position,
    active: bool,
}

impl Sel {
    /// start and end in reading order
    fn ordered(&self) -> (Position, Position) {
        if (self.a.y, self.a.x) <= (self.b.y, self.b.x) { (self.a, self.b) } else { (self.b, self.a) }
    }
    fn contains(&self, x: u16, y: u16) -> bool {
        let (s, e) = self.ordered();
        if y < s.y || y > e.y || x < self.area.x || x >= self.area.right() {
            return false;
        }
        !(y == s.y && x < s.x) && !(y == e.y && x > e.x)
    }
}

/// Right-click menu.
struct CtxMenu {
    x: u16,
    y: u16,
    items: Vec<(String, Cmd)>,
    sel: usize,
    rect: Rect,
}

/// A tab's status dot: the loudest of its panes.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Dot {
    None,
    Idle,
    Done,
    Working,
    Blocked,
}

struct Palette {
    query: String,
    sel: usize,
    items: Vec<(String, Cmd)>,
    theme_before: String,
    /// where it was drawn, and each visible row with its place in the matches (for the mouse)
    rect: Rect,
    rows: Vec<(Rect, usize)>,
}

/// Something that would stop agents mid-work, so it asks first: y (or the same key or click again) does it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Doom {
    Close(PaneId),
    CloseTab(u64),
    Quit,
}

/// How long that question waits for an answer.
const CONFIRM_FOR: Duration = Duration::from_secs(4);
/// How long keys wait behind a ctrl+v clipboard check before they're let through anyway.
const CLIP_WAIT: Duration = Duration::from_secs(5);

pub struct App {
    panes: HashMap<PaneId, Box<dyn Pane>>,
    tabs: Vec<Tab>,
    cur: usize,
    next_id: PaneId,
    theme: Theme,
    config: Config,
    tx: Sender<Event>,
    notice: Option<(String, Instant)>,
    prefix_armed: bool,
    palette: Option<Palette>,
    quit: bool,
    start: Instant,
    last_tick: HashMap<PaneId, Instant>,
    // geometry from the last draw, for the mouse
    inner: Vec<(PaneId, Rect)>,
    outer: Vec<(PaneId, Rect)>,
    side_hits: Vec<(Rect, SideHit)>,
    side_area: Option<(PaneId, Rect)>,
    sidebar: bool,
    body: Rect,
    drag: Option<(Vec<bool>, Dir, Rect)>,
    /// mouse text selection: which pane, its inner rect, anchor and far end (screen cells), dragging yet?
    sel: Option<Sel>,
    /// copy the selection after the next draw (the text comes from the drawn frame)
    copy_pending: bool,
    /// the × buttons drawn on pane frames this frame
    pane_close: Vec<(Rect, PaneId)>,
    /// where the mouse is (for hover highlights)
    hover: Position,
    /// renaming a tab (by id): the text typed so far
    renaming: Option<(u64, String)>,
    next_tab: u64,
    /// close/quit is waiting for a yes because agents are at work (the toast asks)
    confirm: Option<(Doom, Instant)>,
    /// ctrl+v / alt+v is checking the clipboard for an image: keys for that pane wait here, in order
    pending_clip: Option<(PaneId, Instant, Vec<KeyEvent>)>,
    /// ORIEL_LOG: input events and draw times go to this file
    log: Option<std::path::PathBuf>,
    ctx: Option<CtxMenu>,
    /// first-run welcome, setup and tour
    onboard: Option<crate::onboard::Onboard>,
    ctx_seen: usize,
    palette_seen: usize,
    last_click: Option<(Instant, u16, u16)>,
    /// agents' last known activity, and the ones that finished while you weren't looking
    agent_state: HashMap<PaneId, Activity>,
    /// panes opened with Action::OpenTagged: tag -> pane
    tags: HashMap<String, PaneId>,
    done: std::collections::HashSet<PaneId>,
    _watcher: Option<notify::RecommendedWatcher>,
    _theme_watcher: Option<notify::RecommendedWatcher>,
    /// A newer version is out: shown at the bottom of the sidebar.
    update_ready: Option<String>,
    /// Is the terminal the window you're in? (desktop notifications only when it isn't)
    term_focused: bool,
}

impl App {
    pub fn new(config: Config, tx: Sender<Event>) -> App {
        let theme = theme::get(&config.theme);
        ui::NERD.store(std::env::var("ORIEL_PLAIN").is_err() && !config.plain_icons, std::sync::atomic::Ordering::Relaxed);
        let mut app = App {
            panes: HashMap::new(),
            tabs: vec![],
            cur: 0,
            next_id: 1,
            theme,
            config,
            tx: tx.clone(),
            notice: None,
            prefix_armed: false,
            palette: None,
            quit: false,
            start: Instant::now(),
            last_tick: HashMap::new(),
            inner: vec![],
            outer: vec![],
            side_hits: vec![],
            side_area: None,
            sidebar: true,
            body: Rect::default(),
            drag: None,
            sel: None,
            copy_pending: false,
            pane_close: vec![],
            hover: Position { x: u16::MAX, y: u16::MAX },
            _watcher: None,
            _theme_watcher: None,
            update_ready: None,
            term_focused: true,
            renaming: None,
            next_tab: 1,
            confirm: None,
            pending_clip: None,
            log: std::env::var_os("ORIEL_LOG").map(std::path::PathBuf::from),
            ctx: None,
            onboard: None,
            ctx_seen: 0,
            palette_seen: 0,
            last_click: None,
            agent_state: HashMap::new(),
            tags: HashMap::new(),
            done: Default::default(),
        };
        app._watcher = watch_omarchy(tx.clone());
        app._theme_watcher = watch_themes(tx.clone());
        if !cfg!(test) {
            crate::alerts::open();
            let t2 = tx.clone();
            crate::alerts::watch(move |k, s| {
                let _ = t2.send(Event::Alert(k, s));
            });
            // once a day: is there a newer oriel? (in the background; quiet if offline)
            std::thread::spawn(move || {
                if let Some(r) = crate::update::check(false).ok().and_then(|rs| crate::update::available(&rs)) {
                    let _ = tx.send(Event::UpdateAvailable(r.version));
                }
            });
            if let Some((from, to)) = crate::update::just_updated() {
                if crate::update::newer(&from, &to) {
                    // a rollback: the version you left stays quiet until a newer one is out
                    crate::alerts::update_state(|s| s.skipped = Some(from.clone()));
                    app.notify(format!("{}rolled back {from} → {to} · {from} won't be offered again", ui::lead("package")));
                } else {
                    app.notify(format!("{}updated {from} → {to} · alt p → updates for what's new", ui::lead("package")));
                }
            }
        }
        if !cfg!(test) && !crate::onboard::done_before() {
            app.onboard = Some(crate::onboard::Onboard::new(&app.theme.name, &app.config));
        }
        if !cfg!(test) && startup_needs_calendar(&app.config.startup) {
            app.goto_app("calendar");
        }
        let startup = app.config.startup.clone();
        if SIDEBAR.iter().any(|a| a.0 == startup) {
            app.goto_app(SIDEBAR.iter().find(|a| a.0 == startup).unwrap().0);
        } else {
            let first = panes::open(&startup, &app.config).unwrap_or_else(|| Box::new(panes::home::Home::new()));
            app.new_tab(first);
        }
        app
    }

    fn add(&mut self, p: Box<dyn Pane>) -> PaneId {
        let id = self.next_id;
        self.next_id += 1;
        self.panes.insert(id, p);
        id
    }

    fn new_tab(&mut self, p: Box<dyn Pane>) {
        let id = self.add(p);
        let tid = self.tab_id();
        self.tabs.push(Tab { id: tid, app: None, name: None, root: Node::Leaf(id), focus: id, zoom: false });
        self.cur = self.tabs.len() - 1;
    }

    fn tab_id(&mut self) -> u64 {
        self.next_tab += 1;
        self.next_tab - 1
    }

    /// Where the tab with this id is now (tabs open and close in the background, so indices go stale).
    fn tab_index(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    /// Show an app's tab, creating it the first time (like nest's F1-F6).
    fn goto_app(&mut self, name: &'static str) {
        if let (true, Some(t)) = (name == "help", self.tabs.get(self.cur)) {
            let ctx = match t.app {
                Some(a) => a,
                None if self.panes.get(&t.focus).map(|p| p.is_terminal()).unwrap_or(false) => "terminal",
                None => "home",
            };
            if ctx != "help" {
                panes::help::set_context(ctx);
            }
        }
        if let Some(i) = self.tabs.iter().position(|t| t.app == Some(name)) {
            self.cur = i;
            return;
        }
        let Some(p) = panes::open(name, &self.config) else {
            self.notify(format!("{name} isn't available"));
            return;
        };
        let id = self.add(p);
        // app tabs stay in sidebar order, ahead of your own tabs
        let order = |a: Option<&str>| a.and_then(|n| SIDEBAR.iter().position(|s| s.0 == n)).unwrap_or(usize::MAX);
        let me = order(Some(name));
        let at = self.tabs.iter().position(|t| order(t.app) > me).unwrap_or(self.tabs.len());
        let tid = self.tab_id();
        self.tabs.insert(at, Tab { id: tid, app: Some(name), name: None, root: Node::Leaf(id), focus: id, zoom: false });
        self.cur = at;
    }

    /// The help app, opened on the topic for whatever you're looking at (F10, ?, the sidebar, the palette).
    fn open_help(&mut self) {
        self.goto_app("help");
    }

    /// Tabs you made (not the pinned apps), with their index in self.tabs.
    /// Replay the welcome + tour (palette "take the tour", `oriel --tour`).
    pub fn start_tour(&mut self) {
        self.onboard = Some(crate::onboard::Onboard::new(&self.theme.name, &self.config));
    }

    fn probe(&self) -> crate::onboard::Probe {
        let t = &self.tabs[self.cur];
        let mut l = vec![];
        t.root.leaves(&mut l);
        crate::onboard::Probe {
            app: t.app,
            user_tabs: self.user_tabs().len(),
            panes_in_tab: l.len(),
            focus: t.focus,
            palette_open: self.palette.is_some(),
            tab_named: t.app.is_none() && t.name.is_some(),
            ctx_seen: self.ctx_seen,
            palette_seen: self.palette_seen,
        }
    }

    fn onboard_out(&mut self, out: crate::onboard::Out) {
        use crate::onboard::Out;
        match out {
            Out::None => {}
            Out::Theme(name, save) => self.set_theme(&name, save),
            Out::Icons(nerd) => {
                ui::NERD.store(nerd, std::sync::atomic::Ordering::Relaxed);
                self.config.plain_icons = !nerd;
                config::save(&self.config);
            }
            Out::MusicFolder(p) => {
                self.config.music.folders = vec![p];
                config::save(&self.config);
            }
            Out::NotesFolder(p) => {
                self.config.notes_folder = p;
                config::save(&self.config);
            }
            Out::Defaults { startup, provider } => {
                self.config.startup = startup;
                if !provider.is_empty() {
                    self.config.ai.provider = provider;
                }
                config::save(&self.config);
            }
            Out::Finished => {
                self.onboard = None;
                crate::onboard::mark_done();
                self.notify(format!("{}welcome to oriel · ? for keys · alt p for everything", ui::lead("window")));
            }
        }
    }

    fn user_tabs(&self) -> Vec<usize> {
        (0..self.tabs.len()).filter(|&i| self.tabs[i].app.is_none()).collect()
    }

    fn tab(&mut self) -> &mut Tab {
        &mut self.tabs[self.cur]
    }

    fn focused(&self) -> PaneId {
        self.tabs[self.cur].focus
    }

    fn open(&mut self, p: Box<dyn Pane>, place: Place, from: PaneId) {
        match place {
            Place::Tab => self.new_tab(p),
            Place::Replace => {
                let id = self.add(p);
                for t in &mut self.tabs {
                    if t.root.replace(from, id) {
                        if t.focus == from {
                            t.focus = id;
                        }
                    }
                }
                self.panes.remove(&from);
            }
            Place::SplitRight | Place::SplitDown | Place::Split => {
                let dir = match place {
                    Place::SplitRight => Dir::Right,
                    Place::SplitDown => Dir::Down,
                    _ => {
                        // along the longer side (cells are ~2x taller than wide)
                        let r = self.outer.iter().find(|(i, _)| *i == from).map(|x| x.1).unwrap_or(self.body);
                        if r.width as f32 > r.height as f32 * 2.2 { Dir::Right } else { Dir::Down }
                    }
                };
                let id = self.add(p);
                let tab = self.tab();
                if !tab.root.split(from, id, dir) {
                    tab.root = Node::Split { dir, ratio: 0.5, a: Box::new(tab.root.clone()), b: Box::new(Node::Leaf(id)) };
                }
                tab.focus = id;
                tab.zoom = false;
            }
        }
    }

    fn close(&mut self, id: PaneId) {
        self.panes.remove(&id);
        self.last_tick.remove(&id);
        if let Some(ti) = self.tabs.iter().position(|t| t.root.contains(id)) {
            let t = &mut self.tabs[ti];
            if t.root.remove(id) {
                let mut l = vec![];
                t.root.leaves(&mut l);
                if t.focus == id {
                    t.focus = l[0];
                }
                t.zoom = false;
            } else {
                // last pane in the tab: drop the tab (one before yours going mustn't move you to another)
                self.tabs.remove(ti);
                if ti < self.cur {
                    self.cur -= 1;
                }
                if self.tabs.is_empty() {
                    self.goto_app("ai");
                }
                self.cur = self.cur.min(self.tabs.len() - 1);
            }
        }
    }

    /// Close a pane because you asked (alt w, a ×, the menus): an app tab keeps its one pane, and a pane with
    /// agents at work asks first.
    fn request_close(&mut self, id: PaneId) {
        let Some(ti) = self.tabs.iter().position(|t| t.root.contains(id)) else { return };
        if self.tabs[ti].app.is_some() && matches!(self.tabs[ti].root, Node::Leaf(_)) {
            self.notify("app tabs stay open · F-keys switch");
            return;
        }
        if self.confirmed(Doom::Close(id)) {
            self.close(id);
        }
    }

    /// Close one of your tabs (the sidebar ×, middle-click, prefix &, the menus). App tabs stay.
    fn request_close_tab(&mut self, tab: u64) {
        let Some(i) = self.tab_index(tab) else { return };
        if self.tabs[i].app.is_some() {
            self.notify("app tabs stay open · F-keys switch");
            return;
        }
        if self.confirmed(Doom::CloseTab(tab)) {
            self.close_tab(i);
        }
    }

    fn request_quit(&mut self) {
        if self.confirmed(Doom::Quit) {
            self.quit = true;
        }
    }

    /// How many agents `d` would stop mid-work.
    fn doomed(&self, d: Doom) -> usize {
        let ids: Vec<PaneId> = match d {
            Doom::Close(id) => vec![id],
            Doom::CloseTab(t) => {
                let mut l = vec![];
                if let Some(i) = self.tab_index(t) {
                    self.tabs[i].root.leaves(&mut l);
                }
                l
            }
            Doom::Quit => self.panes.keys().copied().collect(),
        };
        ids.iter().filter_map(|id| self.panes.get(id)).map(|p| p.busy()).sum()
    }

    /// Go ahead with `d`? Yes if it stops nothing, or if it's the thing already being asked about (the same key
    /// or click again). Otherwise it asks on the toast, and y does it.
    fn confirmed(&mut self, d: Doom) -> bool {
        if self.doomed(d) == 0 {
            return true;
        }
        if self.confirm.is_some_and(|(c, at)| c == d && at.elapsed() < CONFIRM_FOR) {
            self.confirm = None;
            return true;
        }
        self.confirm = Some((d, Instant::now()));
        false
    }

    fn doom(&mut self, d: Doom) {
        match d {
            Doom::Close(id) => self.close(id),
            Doom::CloseTab(t) => {
                if let Some(i) = self.tab_index(t) {
                    self.close_tab(i);
                }
            }
            Doom::Quit => self.quit = true,
        }
    }

    /// The question on the toast while a close/quit waits for its yes.
    fn confirm_text(&self) -> Option<String> {
        let (d, _) = self.confirm.filter(|c| c.1.elapsed() < CONFIRM_FOR)?;
        let n = self.doomed(d).max(1);
        let agents = if n == 1 { "1 agent".to_string() } else { format!("{n} agents") };
        Some(match d {
            Doom::Close(id) => match self.panes.get(&id).filter(|p| p.is_terminal()) {
                Some(p) => format!("{} is working · close it anyway? y / n", p.title()),
                None => format!("{agents} still working here · close anyway? y / n"),
            },
            Doom::CloseTab(_) => format!("{agents} still working in this tab · close it anyway? y / n"),
            Doom::Quit => format!("{agents} still working · quit anyway? y / n"),
        })
    }

    fn notify(&mut self, s: impl Into<String>) {
        self.notice = Some((s.into(), Instant::now()));
    }

    /// Into the event center: a toast now, a line in alerts, and a desktop notification if you're elsewhere.
    fn raise(&mut self, kind: crate::alerts::Kind, text: String, app: Option<&str>, pane: Option<PaneId>) {
        // the thing it's about is right in front of you: no need to keep it
        let in_view = self.term_focused && pane.is_some_and(|p| self.visible().contains(&p));
        crate::alerts::push(crate::alerts::Alert { at: crate::alerts::now(), kind, text: text.clone(), app: app.map(String::from), read: in_view, pane });
        self.notify(format!("{} {text}", kind.label().0));
        if !self.term_focused && kind.loud() && self.config.desktop_notifications {
            crate::alerts::desktop("oriel", &text);
        }
    }

    fn set_theme(&mut self, name: &str, save: bool) {
        self.theme = theme::get(name);
        if save {
            self.config.theme = name.to_string();
            config::save(&self.config);
        }
    }

    // ------------------------------------------------------------------ loop
    pub fn run(&mut self, term: &mut DefaultTerminal, rx: Receiver<Event>) -> anyhow::Result<()> {
        let mut dirty = true;
        loop {
            if dirty {
                let t0 = Instant::now();
                term.draw(|f| self.draw(f))?;
                if self.log.is_some() {
                    self.log_line(&format!("draw {:.2} ms", t0.elapsed().as_secs_f64() * 1000.0));
                }
            }
            let timeout = self.next_deadline();
            let ev = match rx.recv_timeout(timeout) {
                Ok(e) => e,
                Err(RecvTimeoutError::Timeout) => Event::Tick,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            let before = self.look();
            let mut moves_only = is_move(&ev);
            self.handle(ev);
            // coalesce a burst (a terminal spewing output) into one redraw
            let burst = Instant::now();
            while let Ok(e) = rx.try_recv() {
                moves_only &= is_move(&e);
                self.handle(e);
                if burst.elapsed() > Duration::from_millis(12) {
                    break;
                }
            }
            self.reap();
            self.track_agents();
            if self.onboard.is_some() {
                let probe = self.probe();
                let out = self.onboard.as_mut().unwrap().check(&probe);
                self.onboard_out(out);
            }
            if self.quit {
                break;
            }
            // the mouse just passing over (any-motion tracking reports every cell) changes nothing on screen
            // unless it moved onto or off something that lights up
            dirty = !moves_only || self.look() != before;
        }
        Ok(())
    }

    /// What a mouse move can change on screen: the × under it, the menu row, the hovered pane's own highlight;
    /// plus a pane closing or a toast, in case the same batch brought one.
    fn look(&self) -> (Option<Rect>, Option<usize>, usize, usize, bool) {
        let pos = self.hover;
        let side_x = self.side_hits.iter().filter(|(_, h)| matches!(h, SideHit::CloseTab(_))).map(|x| x.0);
        let hot = self.pane_close.iter().map(|x| x.0).chain(side_x).find(|r| r.contains(pos));
        let pane = self.outer.iter().find(|(_, r)| r.contains(pos)).and_then(|(id, _)| self.panes.get(id)).map(|p| p.hover()).unwrap_or(0);
        (hot, self.ctx.as_ref().map(|m| m.sel), pane, self.panes.len(), self.notice.is_some())
    }

    fn log_line(&self, s: &str) {
        use std::io::Write;
        let Some(path) = &self.log else { return };
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{:>8.3} {s}", self.start.elapsed().as_secs_f64());
        }
    }

    /// Sleep until the soonest thing that needs a redraw without an event.
    fn next_deadline(&self) -> Duration {
        let mut d = Duration::from_secs(30);
        // an animated theme only animates while you can see it
        if (self.theme.animated && self.term_focused) || self.onboard.as_ref().map(|o| o.animating()).unwrap_or(false) {
            d = d.min(Duration::from_millis(125));
        }
        if let Some((_, t)) = &self.notice {
            d = d.min(Duration::from_secs(4).saturating_sub(t.elapsed()) + Duration::from_millis(10));
        }
        if let Some((_, t)) = &self.confirm {
            d = d.min(CONFIRM_FOR.saturating_sub(t.elapsed()) + Duration::from_millis(10));
        }
        let mut ids = self.visible();
        ids.extend(self.panes.iter().filter(|(_, p)| p.is_terminal() || p.ticks_hidden()).map(|(id, _)| *id));
        for id in ids {
            if let Some(every) = self.panes.get(&id).and_then(|p| p.tick_every()) {
                let last = self.last_tick.get(&id).copied().unwrap_or(self.start);
                d = d.min(every.saturating_sub(last.elapsed()));
            }
        }
        d.max(Duration::from_millis(4))
    }

    fn visible(&self) -> Vec<PaneId> {
        let t = &self.tabs[self.cur];
        if t.zoom {
            return vec![t.focus];
        }
        let mut l = vec![];
        t.root.leaves(&mut l);
        l
    }

    /// herdr's sidebar states: notice agents finishing or getting stuck where you aren't looking: another tab, a
    /// pane hidden by zoom, or anywhere while you're in another window.
    fn track_agents(&mut self) {
        let visible = self.visible();
        let mut notes = vec![];
        for (id, p) in &self.panes {
            let Some(a) = p.activity() else { continue };
            let prev = self.agent_state.insert(*id, a);
            let seen = self.term_focused && visible.contains(id);
            let tab_no = self.tabs.iter().position(|t| t.root.contains(*id));
            let where_ = tab_no.map(|i| self.tab_label(i)).unwrap_or_default();
            // where enter in the alerts app goes back to once this pane is gone: its app tab, or agents for the
            // task tabs the orchestrator opened
            let app = tab_no.and_then(|i| self.tabs[i].app).or_else(|| self.tags.values().any(|t| t == id).then_some("agents"));
            if prev == Some(Activity::Working) && a == Activity::Idle && !seen {
                self.done.insert(*id);
                notes.push((crate::alerts::Kind::AgentDone, format!("{} finished · {where_}", p.title()), *id, app));
            }
            if a == Activity::Blocked && prev != Some(Activity::Blocked) && !seen {
                notes.push((crate::alerts::Kind::NeedsYou, format!("{} needs you · {where_}", p.title()), *id, app));
            }
            if seen {
                self.done.remove(id);
            }
        }
        self.agent_state.retain(|id, _| self.panes.contains_key(id));
        // tell every app pane which tagged panes are still alive (the orchestrator follows its agents this way)
        self.tags.retain(|_, id| self.panes.contains_key(id));
        let live: Vec<(String, Option<Activity>)> = self.tags.iter().map(|(t, id)| (t.clone(), self.panes.get(id).and_then(|p| p.activity()))).collect();
        let app_ids: Vec<PaneId> = self.tabs.iter().filter(|t| t.app.is_some()).map(|t| t.focus).collect();
        for id in app_ids {
            if let Some(p) = self.panes.get_mut(&id) {
                p.tagged_panes(&live);
            }
        }
        self.done.retain(|id| self.panes.contains_key(id));
        for (k, text, id, app) in notes {
            self.raise(k, text, app, Some(id));
        }
    }

    fn tab_dot(&self, i: usize) -> Dot {
        let mut l = vec![];
        self.tabs[i].root.leaves(&mut l);
        l.iter()
            .map(|id| match self.panes.get(id).and_then(|p| p.activity()) {
                Some(Activity::Blocked) => Dot::Blocked,
                Some(Activity::Working) => Dot::Working,
                Some(Activity::Idle) if self.done.contains(id) => Dot::Done,
                Some(Activity::Idle) => Dot::Idle,
                None => Dot::None,
            })
            .max()
            .unwrap_or(Dot::None)
    }

    fn tab_label(&self, i: usize) -> String {
        let t = &self.tabs[i];
        if let Some(n) = &t.name {
            return n.clone();
        }
        if let Some(a) = t.app {
            return a.to_string();
        }
        self.panes.get(&t.focus).map(|p| p.title()).unwrap_or_default()
    }

    fn start_rename(&mut self, i: usize) {
        if self.tabs[i].app.is_some() {
            self.notify("app tabs keep their names — make a new tab (alt t) to name one");
            return;
        }
        let cur = self.tab_label(i);
        self.renaming = Some((self.tabs[i].id, cur));
        self.sidebar = true; // the field is drawn in the tab's row (or a popup when the window is too narrow)
    }

    fn close_tab(&mut self, i: usize) {
        let mut l = vec![];
        self.tabs[i].root.leaves(&mut l);
        for id in l {
            self.close(id);
        }
    }

    fn ctx_open(&mut self, x: u16, y: u16, items: Vec<(String, Cmd)>) {
        self.ctx_seen += 1;
        self.ctx = Some(CtxMenu { x, y, items, sel: 0, rect: Rect::default() });
    }

    fn reap(&mut self) {
        let dead: Vec<PaneId> = self.panes.iter().filter(|(_, p)| !p.alive()).map(|(id, _)| *id).collect();
        for id in dead {
            if let Some((k, s)) = self.panes.get_mut(&id).and_then(|p| p.exit_note()) {
                self.raise(k, s, None, None);
            }
            self.close(id);
        }
        if let Some((_, t)) = &self.notice {
            if t.elapsed() > Duration::from_secs(4) {
                self.notice = None;
            }
        }
    }

    /// Run `f` on pane `id` with a Cx, then apply whatever actions it asked for.
    fn with_pane<R>(&mut self, id: PaneId, f: impl FnOnce(&mut dyn Pane, &mut Cx) -> R) -> Option<R> {
        let mut actions = vec![];
        let focused = self.focused() == id;
        let time = self.start.elapsed().as_secs_f64();
        let r = {
            let p = self.panes.get_mut(&id)?;
            let mut cx = Cx { id, theme: &self.theme, config: &self.config, tx: &self.tx, actions: &mut actions, focused, time };
            f(p.as_mut(), &mut cx)
        };
        self.apply(id, actions);
        Some(r)
    }

    fn apply(&mut self, from: PaneId, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Open(p, place) => self.open(p, place, from),
                Action::Close => self.close(from),
                Action::Notify(s) => self.notify(s),
                Action::Alert(k, s) => {
                    let app = self.tabs.iter().find(|t| t.root.contains(from)).and_then(|t| t.app);
                    self.raise(k, s, app, Some(from));
                }
                Action::FocusPane(id) => {
                    if let Some(i) = self.tabs.iter().position(|t| t.root.contains(id)) {
                        self.cur = i;
                        self.tabs[i].focus = id;
                    } else {
                        self.notify("that pane is closed");
                    }
                }
                Action::FocusPaneOr(id, app) => {
                    if let Some(i) = self.tabs.iter().position(|t| t.root.contains(id)) {
                        self.cur = i;
                        self.tabs[i].focus = id;
                    } else {
                        self.goto_app(app);
                    }
                }
                Action::SetTheme(t) => {
                    if theme::names().iter().any(|n| *n == t) {
                        self.set_theme(&t, true);
                        self.notify(format!("{}theme: {t}", ui::lead("theme")));
                    } else {
                        self.notify(format!("no theme called {t} — /theme lists them"));
                    }
                }
                Action::ApplyTheme(t) => {
                    self.set_theme(&t, true);
                    if let Some(p) = theme::problems(&t).into_iter().next() {
                        self.notify(format!("⚠ {p}"));
                    }
                }
                Action::Palette(q) => {
                    self.open_palette();
                    if let Some(p) = &mut self.palette {
                        p.query = q;
                    }
                }
                Action::GotoApp(a) => self.goto_app(a),
                Action::AppKey(a, c) => {
                    // by id: opening the app inserts its tab, which can shift yours along
                    let here = self.tabs[self.cur].id;
                    self.goto_app(a); // opens it if it isn't yet
                    self.cur = self.tab_index(here).unwrap_or(self.cur);
                    if let Some(id) = self.tabs.iter().find(|t| t.app == Some(a)).map(|t| t.focus) {
                        self.with_pane(id, |p, cx| p.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), cx));
                    }
                }
                Action::OpenTagged { pane, tag, name, focus } => {
                    let here = self.tabs[self.cur].id;
                    self.new_tab(pane);
                    let id = self.tabs[self.cur].focus;
                    self.tabs[self.cur].name = Some(name);
                    self.tags.insert(tag, id);
                    if !focus {
                        self.cur = self.tab_index(here).unwrap_or(self.cur);
                    }
                }
                Action::FocusTag(tag) => {
                    if let Some(&id) = self.tags.get(&tag) {
                        if let Some(i) = self.tabs.iter().position(|t| t.root.contains(id)) {
                            self.cur = i;
                            self.tabs[i].focus = id;
                        }
                    }
                }
                Action::CloseTag(tag) => {
                    if let Some(id) = self.tags.remove(&tag) {
                        self.close(id);
                    }
                }
                Action::ToggleSidebar => self.sidebar = !self.sidebar,
                Action::ToggleIcons => self.run_cmd(Cmd::Icons),
                Action::Quit => self.request_quit(),
            }
        }
    }

    fn handle(&mut self, ev: Event) {
        if let (Some(_), Event::Input(e)) = (&self.log, &ev) {
            self.log_line(&format!("{e:?}"));
        }
        // typing or clicking here means you're here, even if the terminal missed telling us (FocusGained)
        if let Event::Input(CEvent::Key(_) | CEvent::Mouse(MouseEvent { kind: MouseEventKind::Down(_), .. })) = &ev {
            self.term_focused = true;
        }
        match ev {
            Event::Input(CEvent::Key(k)) if k.kind != KeyEventKind::Release => self.key(k),
            Event::Input(CEvent::Mouse(m)) => self.mouse(m),
            Event::Input(CEvent::Paste(s)) => self.paste(&s),
            Event::Input(CEvent::FocusGained) => self.term_focused = true,
            Event::Input(CEvent::FocusLost) => self.term_focused = false,
            Event::Input(_) => {}
            Event::Wake(id) => {
                self.with_pane(id, |p, cx| p.poll(cx));
            }
            Event::Clipboard(id, got, key) => {
                match got {
                    Some(paths) => {
                        let text = crate::clip::paste_form(&paths);
                        self.with_pane(id, |p, cx| p.paste(&text, cx));
                        let what = if paths.len() == 1 && paths[0].ends_with(".png") && paths[0].contains("paste-") { "image".to_string() } else { format!("{} file{}", paths.len(), if paths.len() == 1 { "" } else { "s" }) };
                        self.notify(format!("pasted {what} as a path · Claude Code and Codex attach it"));
                    }
                    // nothing image-like: the program gets its own key (so e.g. Claude's own alt+v still works)
                    None => {
                        self.with_pane(id, |p, cx| p.key(key, cx));
                    }
                }
                // then whatever was typed while the clipboard was being checked, in order
                self.flush_clip();
            }
            Event::UpdateAvailable(v) => {
                let st = crate::alerts::state();
                // rolled back from it: stay quiet (the updates app still offers it)
                if st.skipped.as_deref() != Some(v.as_str()) {
                    // said once per version, not once per start
                    if st.announced.as_deref() != Some(v.as_str()) {
                        self.raise(crate::alerts::Kind::Update, format!("oriel {v} is out: click it at the bottom of the sidebar, or alt p → updates"), Some("updates"), None);
                        crate::alerts::update_state(|s| s.announced = Some(v.clone()));
                    }
                    self.update_ready = Some(v);
                }
            }
            Event::Alert(k, s) => {
                let app = match k {
                    crate::alerts::Kind::Memory => Some("system"),
                    crate::alerts::Kind::Usage => Some("ais"),
                    _ => None,
                };
                self.raise(k, s, app, None);
            }
            // (the watchers only send this once the files have gone quiet: omarchy swaps several, editors write
            // in two steps)
            Event::ThemeFilesChanged => {
                if self.theme.name == "omarchy" {
                    self.set_theme("omarchy", false);
                    self.notify(format!("{}omarchy theme updated", ui::lead("theme")));
                } else if theme::is_custom(&self.theme.name) {
                    // your theme file changed (the themes app, or you, in an editor): recolour
                    let name = self.theme.name.clone();
                    self.set_theme(&name, false);
                    if let Some(p) = theme::problems(&name).into_iter().next() {
                        self.notify(format!("⚠ {p}"));
                    }
                }
            }
            Event::Tick => {
                let mut ids = self.visible();
                ids.extend(self.panes.iter().filter(|(id, p)| (p.is_terminal() || p.ticks_hidden()) && !ids.contains(id)).map(|(id, _)| *id).collect::<Vec<_>>());
                for id in ids {
                    let due = match self.panes.get(&id).and_then(|p| p.tick_every()) {
                        Some(every) => self.last_tick.get(&id).map(|t| t.elapsed() >= every).unwrap_or(true),
                        None => false,
                    };
                    if due {
                        self.last_tick.insert(id, Instant::now());
                        self.with_pane(id, |p, cx| p.poll(cx));
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------ keys
    fn is_prefix(&self, k: &KeyEvent) -> bool {
        let spec = self.config.prefix.to_lowercase();
        let (need_ctrl, key) = match spec.strip_prefix("ctrl+") {
            Some(rest) => (true, rest.to_string()),
            None => (false, spec.clone()),
        };
        if need_ctrl != k.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        match (key.as_str(), k.code) {
            ("space", KeyCode::Char(' ')) => true,
            // some terminals report ctrl+space as ctrl+@ / NUL
            ("space", KeyCode::Char('@')) => true,
            (s, KeyCode::Char(c)) if s.chars().count() == 1 => s.starts_with(c.to_ascii_lowercase()),
            _ => false,
        }
    }

    fn key(&mut self, k: KeyEvent) {
        // a close/quit is asking because agents are at work: y does it, n or esc keeps them going. Anything else
        // carries on as normal and drops the question, unless it's the same close/quit again (that's a yes too).
        if let Some((d, at)) = self.confirm {
            if at.elapsed() >= CONFIRM_FOR {
                self.confirm = None;
            } else if k.kind == KeyEventKind::Repeat {
                return; // holding the key down isn't asking twice
            } else if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
                match k.code {
                    KeyCode::Char('y' | 'Y') => {
                        self.confirm = None;
                        self.doom(d);
                        return;
                    }
                    KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                        self.confirm = None;
                        self.notify("ok · they keep working");
                        return;
                    }
                    _ => {}
                }
            }
        }
        let asked = self.confirm.map(|c| c.1);
        self.key_inner(k);
        if self.confirm.map(|c| c.1) == asked && !self.prefix_armed {
            self.confirm = None;
        }
    }

    fn key_inner(&mut self, k: KeyEvent) {
        self.sel = None;
        if self.pending_clip.as_ref().is_some_and(|c| c.1.elapsed() > CLIP_WAIT) {
            self.flush_clip(); // the clipboard check never came back: don't hold the keys hostage
        }
        let modal = self.onboard.as_ref().is_some_and(|o| o.is_modal());
        if !modal && self.palette.is_none() && self.renaming.is_none() && self.ctx.is_none() && !self.prefix_armed && k.kind != KeyEventKind::Repeat {
            let v = matches!(k.code, KeyCode::Char('v') | KeyCode::Char('V'));
            let alt = k.modifiers.contains(KeyModifiers::ALT) && !k.modifiers.contains(KeyModifiers::CONTROL);
            let ctrl = k.modifiers.contains(KeyModifiers::CONTROL) && !k.modifiers.contains(KeyModifiers::ALT);
            let id = self.focused();
            // alt+v anywhere; ctrl+v only where an image path helps (chat, agents, a coding agent in a terminal):
            // vim's visual-block, a shell's quoted-insert and the rest get their ctrl+v straight away
            let images = self.panes.get(&id).is_some_and(|p| p.wants_images());
            if v && (alt || (ctrl && images && !self.is_prefix(&k))) && self.pending_clip.is_none() {
                // paste an image: windows terminal keeps ctrl+v for text, so alt+v is the reliable one there
                let tx = self.tx.clone();
                self.pending_clip = Some((id, Instant::now(), vec![]));
                std::thread::spawn(move || {
                    let got = if cfg!(test) { None } else { crate::clip::grab_image() }; // tests never read the clipboard
                    let _ = tx.send(Event::Clipboard(id, got, k));
                });
                return;
            }
        }
        if self.onboard.is_some() {
            let probe = self.probe();
            let (used, out) = self.onboard.as_mut().unwrap().key(k, &probe);
            self.onboard_out(out);
            if used {
                return;
            }
        }
        if let Some((i, mut text)) = self.renaming.take() {
            match k.code {
                KeyCode::Enter => {
                    let t = text.trim().to_string();
                    if let Some(ti) = self.tab_index(i) {
                        self.tabs[ti].name = if t.is_empty() { None } else { Some(t) };
                    }
                }
                KeyCode::Esc => {}
                KeyCode::Backspace => {
                    text.pop();
                    self.renaming = Some((i, text));
                }
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                    if text.chars().count() < 40 {
                        text.push(c);
                    }
                    self.renaming = Some((i, text));
                }
                _ => self.renaming = Some((i, text)),
            }
            return;
        }
        if let Some(m) = &mut self.ctx {
            match k.code {
                KeyCode::Up => m.sel = m.sel.saturating_sub(1),
                KeyCode::Down | KeyCode::Tab => m.sel = (m.sel + 1).min(m.items.len().saturating_sub(1)),
                KeyCode::Enter => {
                    let cmd = m.items.get(m.sel).map(|x| x.1.clone());
                    self.ctx = None;
                    if let Some(c) = cmd {
                        self.run_cmd(c);
                    }
                }
                _ => self.ctx = None,
            }
            return;
        }
        if self.palette.is_some() {
            self.palette_key(k);
            return;
        }
        if self.prefix_armed {
            self.prefix_armed = false;
            if self.is_prefix(&k) {
                // prefix twice: send it through (so ctrl+b ctrl+b types ctrl+b in the shell)
                let id = self.focused();
                self.pane_key(id, k);
                return;
            }
            self.prefix_cmd(k);
            return;
        }
        if self.is_prefix(&k) {
            self.prefix_armed = true;
            return;
        }
        if k.modifiers.contains(KeyModifiers::ALT) && self.alt_cmd(k) {
            return;
        }
        let id = self.focused();
        // bare F-keys switch apps, except in a full-screen program in a terminal (htop's F10, mc's F5...)
        if matches!(k.code, KeyCode::F(_)) && !self.panes.get(&id).is_some_and(|p| p.wants_fkeys()) && self.fkey_app(k) {
            return;
        }
        let Some(used) = self.pane_key(id, k) else { return };
        if !used && !self.panes.get(&id).map(|p| p.is_terminal()).unwrap_or(false) {
            // unused keys in app panes
            if let KeyCode::Char('?') = k.code {
                self.open_help();
            }
        }
    }

    /// A key for a pane, unless a clipboard check for it is still out: then it waits its turn behind the paste,
    /// so `ctrl+v j j` doesn't arrive as `j j ctrl+v`. None = it waits (or the pane is gone).
    fn pane_key(&mut self, id: PaneId, k: KeyEvent) -> Option<bool> {
        if let Some((pid, _, keys)) = &mut self.pending_clip {
            if *pid == id {
                keys.push(k);
                return None;
            }
        }
        self.with_pane(id, |p, cx| p.key(k, cx))
    }

    fn flush_clip(&mut self) {
        if let Some((id, _, keys)) = self.pending_clip.take() {
            for k in keys {
                self.with_pane(id, |p, cx| p.key(k, cx));
            }
        }
    }

    /// Bare F1-F10 → that app, F12 → play/pause music. False if it's neither, so the pane gets the key.
    fn fkey_app(&mut self, k: KeyEvent) -> bool {
        let KeyCode::F(n) = k.code else { return false };
        if !k.modifiers.is_empty() {
            return false;
        }
        if let Some(a) = SIDEBAR.iter().find(|a| a.3 == format!("F{n}")) {
            self.goto_app(a.0);
            return true;
        }
        if n == 12 {
            // play/pause from anywhere, if the music app is open
            if let Some(id) = self.tabs.iter().find(|t| t.app == Some("music")).map(|t| t.focus) {
                self.with_pane(id, |p, cx| p.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), cx));
                return true;
            }
        }
        false
    }

    /// Pasted text goes to whatever has the keyboard: a setup screen's folder field, the palette, a tab name
    /// being typed. Only then the focused pane.
    fn paste(&mut self, s: &str) {
        if let Some(ob) = self.onboard.as_mut().filter(|o| o.is_modal()) {
            ob.paste(s);
            return;
        }
        if let Some(p) = &mut self.palette {
            p.query.extend(s.chars().filter(|c| !c.is_control()));
            p.sel = 0;
            self.palette_preview();
            return;
        }
        if let Some((_, text)) = &mut self.renaming {
            let room = 40usize.saturating_sub(text.chars().count());
            text.extend(s.chars().filter(|c| !c.is_control()).take(room));
            return;
        }
        let id = self.focused();
        self.with_pane(id, |p, cx| p.paste(s, cx));
    }

    /// Direct Alt bindings. Chosen to stay clear of the shell's own Alt+b/f/d/. word keys.
    fn alt_cmd(&mut self, k: KeyEvent) -> bool {
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        match k.code {
            KeyCode::Left if shift => self.resize(Dir::Right, -0.05),
            KeyCode::Right if shift => self.resize(Dir::Right, 0.05),
            KeyCode::Up if shift => self.resize(Dir::Down, -0.05),
            KeyCode::Down if shift => self.resize(Dir::Down, 0.05),
            KeyCode::Left => self.move_focus(-1, 0),
            KeyCode::Right => self.move_focus(1, 0),
            KeyCode::Up => self.move_focus(0, -1),
            KeyCode::Down => self.move_focus(0, 1),
            KeyCode::Enter => self.run_cmd(Cmd::Open("terminal", Place::Split)),
            KeyCode::Char(c) => match c.to_ascii_lowercase() {
                // alt+n too: Windows Terminal keeps alt+enter for fullscreen
                'n' => self.run_cmd(Cmd::Open("terminal", Place::Split)),
                '1'..='9' => {
                    let i = c as usize - '1' as usize;
                    if let Some(&t) = self.user_tabs().get(i) {
                        self.cur = t;
                    }
                }
                's' => self.sidebar = !self.sidebar,
                'p' => self.open_palette(),
                'z' => self.run_cmd(Cmd::Zoom),
                'w' => self.run_cmd(Cmd::Close),
                't' => self.run_cmd(Cmd::NewTab),
                '[' | ',' if false => {}
                _ => return false,
            },
            _ => return false,
        }
        true
    }

    fn prefix_cmd(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Char('|') | KeyCode::Char('\\') | KeyCode::Char('%') | KeyCode::Char('v') => self.run_cmd(Cmd::SplitRight),
            KeyCode::Char(',') => self.run_cmd(Cmd::Rename),
            KeyCode::Char('&') => self.run_cmd(Cmd::CloseTab(self.tabs[self.cur].id)),
            KeyCode::Char('-') | KeyCode::Char('"') | KeyCode::Char('_') => self.run_cmd(Cmd::SplitDown),
            KeyCode::Char('x') | KeyCode::Char('w') => self.run_cmd(Cmd::Close),
            KeyCode::Char('z') => self.run_cmd(Cmd::Zoom),
            KeyCode::Char('c') => self.run_cmd(Cmd::NewTab),
            KeyCode::Char('n') => self.cur = (self.cur + 1) % self.tabs.len(),
            KeyCode::Char('p') => self.cur = (self.cur + self.tabs.len() - 1) % self.tabs.len(),
            KeyCode::Char(c @ '1'..='9') => {
                let i = c as usize - '1' as usize;
                if i < self.tabs.len() {
                    self.cur = i;
                }
            }
            KeyCode::Char('h') | KeyCode::Left => self.move_focus(-1, 0),
            KeyCode::Char('l') | KeyCode::Right => self.move_focus(1, 0),
            KeyCode::Char('k') | KeyCode::Up => self.move_focus(0, -1),
            KeyCode::Char('j') | KeyCode::Down => self.move_focus(0, 1),
            KeyCode::Char('H') => self.resize(Dir::Right, -0.05),
            KeyCode::Char('L') => self.resize(Dir::Right, 0.05),
            KeyCode::Char('K') => self.resize(Dir::Down, -0.05),
            KeyCode::Char('J') => self.resize(Dir::Down, 0.05),
            KeyCode::Char('t') => {
                self.open_palette();
                if let Some(p) = &mut self.palette {
                    p.query = "theme ".into();
                }
            }
            KeyCode::Char(':') | KeyCode::Char(' ') => self.open_palette(),
            KeyCode::Char('?') => self.open_help(),
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::F(_) => {
                // an F-key's other meaning: from a full-screen program it switches apps; at a shell prompt it goes
                // to the shell (PSReadLine's F2 and F8)
                let id = self.focused();
                let (term, full) = self.panes.get(&id).map(|p| (p.is_terminal(), p.wants_fkeys())).unwrap_or_default();
                if term && !full {
                    self.pane_key(id, k);
                } else {
                    self.fkey_app(k);
                }
            }
            KeyCode::Char(c) => {
                if let Some(a) = APPS.iter().find(|a| a.1 == c && a.0 != "terminal") {
                    self.run_cmd(Cmd::Open(a.0, Place::Split));
                } else if c == 'e' {
                    self.run_cmd(Cmd::Open("notes", Place::Split));
                }
            }
            _ => {}
        }
    }

    fn move_focus(&mut self, dx: i32, dy: i32) {
        let from = self.focused();
        if let Some(to) = neighbor(&self.outer, from, dx, dy) {
            self.tab().focus = to;
        }
    }

    fn resize(&mut self, dir: Dir, delta: f32) {
        let id = self.focused();
        self.tab().root.resize(id, dir, delta);
    }

    fn run_cmd(&mut self, c: Cmd) {
        let from = self.focused();
        match c {
            Cmd::App(name) => self.goto_app(name),
            Cmd::Rename => self.start_rename(self.cur),
            Cmd::CloseTab(t) => self.request_close_tab(t),
            Cmd::Open(name, place) => match panes::open(name, &self.config) {
                Some(p) => self.open(p, place, from),
                None => self.notify(format!("{name} isn't installed")),
            },
            Cmd::Theme(t) => {
                self.set_theme(&t, true);
                self.notify(format!("{}theme: {t}", ui::lead("theme")));
            }
            Cmd::SplitRight => self.run_cmd(Cmd::Open("terminal", Place::SplitRight)),
            Cmd::SplitDown => self.run_cmd(Cmd::Open("terminal", Place::SplitDown)),
            Cmd::Close => self.request_close(from),
            Cmd::Zoom => {
                let t = self.tab();
                t.zoom = !t.zoom;
            }
            Cmd::NewTab => self.new_tab(Box::new(panes::home::Home::new())),
            Cmd::Icons => {
                let n = !ui::NERD.load(std::sync::atomic::Ordering::Relaxed);
                ui::NERD.store(n, std::sync::atomic::Ordering::Relaxed);
                self.notify(if n { "nerd font icons" } else { "plain icons (no nerd font)" });
            }
            Cmd::Help => self.open_help(),
            Cmd::Tour => self.start_tour(),
            Cmd::Quit => self.request_quit(),
        }
    }

    /// Can you close panes of the current tab? Not the one pane of an app tab (the frame shows no × there).
    fn closable(&self) -> bool {
        let t = &self.tabs[self.cur];
        t.app.is_none() || matches!(t.root, Node::Split { .. })
    }

    // ------------------------------------------------------------------ palette
    fn open_palette(&mut self) {
        panes::refresh_available(); // installed claude/codex a moment ago? the list below sees it
        let mut items: Vec<(String, Cmd)> = vec![];
        for &(name, icon, label, key) in SIDEBAR {
            items.push((format!("{}go to {label}  {key}", ui::lead(icon)), Cmd::App(name)));
        }
        for &(name, _, icon, label) in APPS {
            if available(name) {
                items.push((format!("{}split: open {label} beside this", ui::lead(icon)), Cmd::Open(name, Place::Split)));
            }
        }
        for &(name, _, icon, label) in APPS {
            if available(name) {
                items.push((format!("{}open {label} in a new tab", ui::lead(icon)), Cmd::Open(name, Place::Tab)));
            }
        }
        items.push((format!("{}split right", ui::lead("split")), Cmd::SplitRight));
        items.push((format!("{}split down", ui::lead("split")), Cmd::SplitDown));
        items.push((format!("{}zoom pane", ui::lead("window")), Cmd::Zoom));
        if self.closable() {
            items.push((format!("{}close pane", ui::lead("close")), Cmd::Close));
        }
        items.push((format!("{}new tab", ui::lead("tab")), Cmd::NewTab));
        if self.tabs[self.cur].app.is_none() {
            items.push((format!("{}rename this tab", ui::lead("tab")), Cmd::Rename));
            items.push((format!("{}close this tab", ui::lead("close")), Cmd::CloseTab(self.tabs[self.cur].id)));
        }
        items.push((format!("{}theme editor: make your own", ui::lead("theme")), Cmd::App("themes")));
        let upd = match &self.update_ready {
            Some(v) => format!("{}update oriel to {v}: what's new", ui::lead("package")),
            None => format!("{}updates: what's new, check, roll back", ui::lead("package")),
        };
        items.push((upd, Cmd::App("updates")));
        for t in theme::names() {
            let yours = if theme::is_custom(&t) { "  · yours" } else { "" };
            items.push((format!("{}theme {t}{yours}", ui::lead("theme")), Cmd::Theme(t)));
        }
        items.push(("toggle nerd font icons".into(), Cmd::Icons));
        items.push((format!("{}help: every key, command and how-to  F10", ui::lead("search")), Cmd::Help));
        items.push((format!("{}take the tour", ui::lead("window")), Cmd::Tour));
        items.push((format!("{}quit oriel", ui::lead("quit")), Cmd::Quit));
        self.palette = Some(Palette { query: String::new(), sel: 0, items, theme_before: self.theme.name.clone(), rect: Rect::default(), rows: vec![] });
        self.palette_seen += 1;
    }

    fn palette_matches(p: &Palette) -> Vec<usize> {
        let q: Vec<char> = p.query.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
        (0..p.items.len())
            .filter(|&i| {
                let mut it = p.items[i].0.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect::<Vec<_>>().into_iter();
                q.iter().all(|c| it.any(|x| x == *c))
            })
            .collect()
    }

    fn palette_key(&mut self, k: KeyEvent) {
        let Some(p) = self.palette.as_mut() else { return };
        let m = Self::palette_matches(p);
        match k.code {
            KeyCode::Esc => return self.palette_close(),
            KeyCode::Enter => return self.palette_run(),
            KeyCode::Down | KeyCode::Tab => p.sel = (p.sel + 1).min(m.len().saturating_sub(1)),
            KeyCode::Up | KeyCode::BackTab => p.sel = p.sel.saturating_sub(1),
            KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => p.sel = (p.sel + 1).min(m.len().saturating_sub(1)),
            KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => p.sel = p.sel.saturating_sub(1),
            KeyCode::Backspace => {
                p.query.pop();
                p.sel = 0;
            }
            KeyCode::Char(c) => {
                p.query.push(c);
                p.sel = 0;
            }
            _ => {}
        }
        self.palette_preview();
    }

    /// Live preview while a theme is highlighted (the theme from before comes back otherwise).
    fn palette_preview(&mut self) {
        let Some(p) = &self.palette else { return };
        let m = Self::palette_matches(p);
        let preview = match m.get(p.sel).map(|&i| &p.items[i].1) {
            Some(Cmd::Theme(t)) => t.clone(),
            _ => p.theme_before.clone(),
        };
        if preview != self.theme.name {
            self.set_theme(&preview, false);
        }
    }

    /// Run the palette's pick (enter, or a click on its row).
    fn palette_run(&mut self) {
        let Some(p) = self.palette.take() else { return };
        let m = Self::palette_matches(&p);
        if let Some(c) = m.get(p.sel).map(|&i| p.items[i].1.clone()) {
            self.run_cmd(c);
        }
    }

    fn palette_close(&mut self) {
        if let Some(p) = self.palette.take() {
            self.set_theme(&p.theme_before, false);
        }
    }

    /// The mouse while the palette is up: it's modal, like the keyboard. A click on a row runs it, a click
    /// outside closes it, the wheel moves the pick.
    fn palette_mouse(&mut self, m: MouseEvent) {
        let pos = Position { x: m.column, y: m.row };
        let Some(p) = &mut self.palette else { return };
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(&(_, row)) = p.rows.iter().find(|(r, _)| r.contains(pos)) {
                    p.sel = row;
                    self.palette_run();
                } else if !p.rect.contains(pos) {
                    self.palette_close();
                }
            }
            MouseEventKind::Down(_) if !p.rect.contains(pos) => self.palette_close(),
            MouseEventKind::ScrollDown => {
                let n = Self::palette_matches(p).len();
                p.sel = (p.sel + 1).min(n.saturating_sub(1));
                self.palette_preview();
            }
            MouseEventKind::ScrollUp => {
                p.sel = p.sel.saturating_sub(1);
                self.palette_preview();
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------ mouse
    fn mouse(&mut self, m: MouseEvent) {
        // a click elsewhere drops a close/quit question; the same × again answers yes
        let asked = self.confirm.map(|c| c.1);
        self.mouse_inner(m);
        if matches!(m.kind, MouseEventKind::Down(_)) && self.confirm.map(|c| c.1) == asked {
            self.confirm = None;
        }
    }

    fn mouse_inner(&mut self, m: MouseEvent) {
        if self.onboard.is_some() {
            let probe = self.probe();
            let (used, out) = self.onboard.as_mut().unwrap().mouse(m, &probe);
            self.onboard_out(out);
            if used {
                return;
            }
        }
        let pos = Position { x: m.column, y: m.row };
        self.hover = pos;
        if self.palette.is_some() {
            return self.palette_mouse(m);
        }
        // an open right-click menu takes the mouse first
        if let Some(menu) = &mut self.ctx {
            match m.kind {
                MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                    if menu.rect.contains(pos) {
                        menu.sel = (m.row.saturating_sub(menu.rect.y + 1) as usize).min(menu.items.len().saturating_sub(1));
                    }
                    return;
                }
                MouseEventKind::Down(_) => {
                    let cmd = if menu.rect.contains(pos) && m.row > menu.rect.y && m.row < menu.rect.bottom() - 1 {
                        menu.items.get((m.row - menu.rect.y - 1) as usize).map(|x| x.1.clone())
                    } else {
                        None
                    };
                    self.ctx = None;
                    if let Some(c) = cmd {
                        self.run_cmd(c);
                    }
                    return;
                }
                _ => return,
            }
        }
        if let MouseEventKind::Down(MouseButton::Right) = m.kind {
            if let Some(&(_, SideHit::Tab(t))) = self.side_hits.iter().find(|(r, _)| r.contains(pos)) {
                let Some(i) = self.tab_index(t) else { return };
                self.cur = i;
                self.ctx_open(m.column, m.row, vec![
                    (format!("{}rename", ui::lead("tab")), Cmd::Rename),
                    (format!("{}split right", ui::lead("split")), Cmd::SplitRight),
                    (format!("{}split down", ui::lead("split")), Cmd::SplitDown),
                    (format!("{}close tab", ui::lead("close")), Cmd::CloseTab(t)),
                ]);
                return;
            }
            if let Some(&(id, _)) = self.outer.iter().find(|(_, r)| r.contains(pos)) {
                let wants = self.panes.get(&id).map(|p| p.wants_mouse()).unwrap_or(false);
                if !wants || m.modifiers.contains(KeyModifiers::SHIFT) {
                    self.tab().focus = id;
                    let mut items = vec![
                        (format!("{}split right", ui::lead("split")), Cmd::SplitRight),
                        (format!("{}split down", ui::lead("split")), Cmd::SplitDown),
                        (format!("{}{}", ui::lead("window"), if self.tabs[self.cur].zoom { "unzoom" } else { "zoom" }), Cmd::Zoom),
                    ];
                    if self.tabs[self.cur].app.is_none() {
                        items.push((format!("{}rename tab", ui::lead("tab")), Cmd::Rename));
                    }
                    items.push((format!("{}new tab", ui::lead("tab")), Cmd::NewTab));
                    if self.closable() {
                        items.push((format!("{}close pane", ui::lead("close")), Cmd::Close));
                    }
                    self.ctx_open(m.column, m.row, items);
                    return;
                }
            }
        }
        // middle-click a tab in the sidebar: close it
        if let MouseEventKind::Down(MouseButton::Middle) = m.kind {
            if let Some(&(_, SideHit::Tab(t) | SideHit::CloseTab(t))) = self.side_hits.iter().find(|(r, _)| r.contains(pos)) {
                self.request_close_tab(t);
                return;
            }
        }
        if let MouseEventKind::Down(MouseButton::Left) = m.kind {
            // the × on a pane's frame
            if let Some(&(_, id)) = self.pane_close.iter().find(|(r, _)| r.contains(pos)) {
                self.request_close(id);
                return;
            }
            if let Some(&(_, hit)) = self.side_hits.iter().find(|(r, _)| r.contains(pos)) {
                let double = self.last_click.map(|(t, x, y)| t.elapsed() < Duration::from_millis(400) && x == m.column && y == m.row).unwrap_or(false);
                self.last_click = Some((Instant::now(), m.column, m.row));
                match hit {
                    SideHit::App(a) => self.goto_app(a),
                    SideHit::Tab(t) => {
                        if let Some(i) = self.tab_index(t) {
                            self.cur = i;
                            if double {
                                self.start_rename(i);
                            }
                        }
                    }
                    SideHit::CloseTab(t) => self.request_close_tab(t),
                    SideHit::NewTab => self.new_tab(Box::new(panes::home::Home::new())),
                }
                return;
            }
            // a split divider: start dragging
            let t = &self.tabs[self.cur];
            if !t.zoom {
                let mut out = vec![];
                t.root.borders(self.body, &mut vec![], &mut out);
                for (area, dir, path) in out.into_iter().rev() {
                    let Some(Node::Split { ratio, .. }) = self.tabs[self.cur].root.clone().node_at(&path).cloned() else { continue };
                    let (a, _) = split_rect(area, dir, ratio);
                    let hit = match dir {
                        Dir::Right => (m.column == a.right() || m.column + 1 == a.right()) && m.row >= area.y && m.row < area.bottom(),
                        Dir::Down => (m.row == a.bottom() || m.row + 1 == a.bottom()) && m.column >= area.x && m.column < area.right(),
                    };
                    if hit {
                        self.drag = Some((path, dir, area));
                        return;
                    }
                }
            }
        }
        if let (Some((path, dir, area)), MouseEventKind::Drag(MouseButton::Left)) = (&self.drag, m.kind) {
            let (path, dir, area) = (path.clone(), *dir, *area);
            let r = match dir {
                Dir::Right => (m.column.saturating_sub(area.x) as f32 + 0.5) / area.width.max(1) as f32,
                Dir::Down => (m.row.saturating_sub(area.y) as f32 + 0.5) / area.height.max(1) as f32,
            };
            if let Some(Node::Split { ratio, .. }) = self.tab().root.node_at(&path) {
                *ratio = r.clamp(0.1, 0.9);
            }
            return;
        }
        if let MouseEventKind::Up(_) = m.kind {
            if self.drag.take().is_some() {
                return;
            }
        }
        // the current app's own sidebar section
        if let Some((id, r)) = self.side_area {
            if r.contains(pos) {
                self.with_pane(id, |p, cx| p.side_mouse(m, r, cx));
                return;
            }
        }
        // dragging out a text selection
        if let (Some(sel), MouseEventKind::Drag(MouseButton::Left)) = (&mut self.sel, m.kind) {
            let r = sel.area;
            let b = Position { x: pos.x.clamp(r.x, r.right().saturating_sub(1)), y: pos.y.clamp(r.y, r.bottom().saturating_sub(1)) };
            if b != sel.a || sel.active {
                sel.active = true;
                sel.b = b;
                return;
            }
        }
        if let MouseEventKind::Up(MouseButton::Left) = m.kind {
            if self.sel.map(|s| s.active).unwrap_or(false) {
                self.copy_pending = true; // copied right after the next draw, from what's on screen
                return;
            }
            self.sel = None;
        }
        // pane under the cursor
        let Some(&(id, _)) = self.outer.iter().find(|(_, r)| r.contains(pos)) else { return };
        let inner = self.inner.iter().find(|(i, _)| *i == id).map(|x| x.1).unwrap_or_default();
        // a left press inside a pane anchors a possible selection (programs that use the mouse themselves keep
        // it unless shift is held; windows terminal does its own selection on shift+drag anyway)
        if let MouseEventKind::Down(MouseButton::Left) = m.kind {
            let wants = self.panes.get(&id).map(|p| p.wants_mouse()).unwrap_or(false);
            if inner.contains(pos) && (!wants || m.modifiers.contains(KeyModifiers::SHIFT)) {
                self.sel = Some(Sel { pane: id, area: inner, a: pos, b: pos, active: false });
            } else {
                self.sel = None;
            }
        }
        if let MouseEventKind::Down(_) = m.kind {
            self.tab().focus = id;
        }
        self.with_pane(id, |p, cx| p.mouse(m, inner, cx));
    }

    // ------------------------------------------------------------------ draw
    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let t = self.theme.clone();
        if !matches!(t.bg, ratatui::style::Color::Reset) {
            f.render_widget(ratatui::widgets::Block::default().style(Style::default().bg(t.bg)), area);
        }
        let side_w = if self.sidebar && area.width >= 70 { (area.width / 5).clamp(26, 36) } else { 0 };
        let side = Rect { width: side_w, ..area };
        self.body = Rect { x: area.x + side_w, width: area.width - side_w, ..area };
        self.side_hits.clear();
        self.side_area = None;
        if side_w > 0 {
            self.draw_sidebar(f, side, &t);
        }

        let tab = &self.tabs[self.cur];
        let mut rects = vec![];
        if tab.zoom {
            rects.push((tab.focus, self.body));
        } else {
            tab.root.rects(self.body, &mut rects);
        }
        let focus = tab.focus;
        // a pane can be closed from its frame if it's one of several, or if the tab is one you made
        let closable = rects.len() > 1 || tab.app.is_none();
        self.outer = rects.clone();
        self.inner.clear();
        self.pane_close.clear();
        let time = self.start.elapsed().as_secs_f64();
        for (id, r) in rects {
            let Some(p) = self.panes.get(&id) else { continue };
            let title = format!("{}{}", ui::lead(p.icon()), p.title());
            let sub = p.subtitle();
            let inner = ui::frame(f, r, &title, sub.as_deref(), id == focus, &t);
            self.inner.push((id, inner));
            if closable && r.width > 12 {
                let b = Rect { x: r.right() - 5, y: r.y, width: 3, height: 1 };
                let hot = b.contains(self.hover);
                let style = if hot {
                    Style::default().fg(t.danger).add_modifier(Modifier::BOLD | Modifier::REVERSED)
                } else {
                    Style::default().fg(if id == focus { t.accent } else { t.muted })
                };
                f.render_widget(Paragraph::new(Span::styled(" × ", style)), b);
                self.pane_close.push((b, id));
            }
            let mut actions = vec![];
            if let Some(p) = self.panes.get_mut(&id) {
                let mut cx = Cx { id, theme: &t, config: &self.config, tx: &self.tx, actions: &mut actions, focused: id == focus, time };
                p.render(f, inner, &mut cx);
            }
            if !actions.is_empty() {
                // actions from render are rare (a pane closing itself); apply after the frame
                let tx = self.tx.clone();
                self.apply(id, actions);
                let _ = tx.send(Event::Tick);
            }
        }
        if let Some(sel) = self.sel.filter(|s| s.active) {
            if self.outer.iter().any(|(id, _)| *id == sel.pane) {
                let buf = f.buffer_mut();
                let (s, e) = sel.ordered();
                let mut text = String::new();
                for y in s.y..=e.y.min(sel.area.bottom().saturating_sub(1)) {
                    let mut line = String::new();
                    for x in sel.area.x..sel.area.right() {
                        if !sel.contains(x, y) {
                            continue;
                        }
                        if let Some(c) = buf.cell_mut(Position { x, y }) {
                            line.push_str(c.symbol());
                            let st = c.style().add_modifier(Modifier::REVERSED);
                            c.set_style(st);
                        }
                    }
                    if !text.is_empty() || y > s.y {
                        text.push('\n');
                    }
                    text.push_str(line.trim_end());
                }
                if self.copy_pending {
                    self.copy_pending = false;
                    let text = text.trim_end_matches('\n').to_string();
                    if !text.trim().is_empty() {
                        crate::clip::copy(&text);
                        let n = text.chars().count();
                        self.notice = Some((format!("copied {n} character{}", if n == 1 { "" } else { "s" }), Instant::now()));
                    }
                }
            } else {
                self.sel = None;
            }
        }
        self.draw_toast(f, area, &t);
        if let Some(m) = &mut self.ctx {
            let w = m.items.iter().map(|x| unicode_width::UnicodeWidthStr::width(x.0.as_str())).max().unwrap_or(10) as u16 + 4;
            let h = m.items.len() as u16 + 2;
            let x = m.x.min(area.right().saturating_sub(w));
            let y = if m.y + h > area.bottom() { m.y.saturating_sub(h) } else { m.y };
            let r = Rect { x, y, width: w.min(area.width), height: h.min(area.height) };
            m.rect = r;
            f.render_widget(ratatui::widgets::Clear, r);
            let inner = ui::frame(f, r, "", None, true, &t);
            for (i, (label, _)) in m.items.iter().enumerate() {
                let on = i == m.sel;
                let st = if on { Style::default().fg(t.accent).add_modifier(Modifier::BOLD | Modifier::REVERSED) } else { Style::default() };
                f.render_widget(Paragraph::new(Span::styled(format!(" {:<width$}", label, width = inner.width.saturating_sub(1) as usize), st)), Rect { y: inner.y + i as u16, height: 1, ..inner });
            }
        }
        if let Some(p) = &mut self.palette {
            Self::draw_palette(f, area, p, &t);
        }
        // renaming a tab while the sidebar (where the field lives) is too narrow to draw: a small box instead
        if let (Some((_, text)), 0) = (&self.renaming, side_w) {
            let inner = ui::popup(f, area, 48, 4, &format!("{}rename tab", ui::lead("tab")), &t);
            let line = Line::from(vec![Span::styled(format!(" {text}▏"), Style::default().fg(t.accent).add_modifier(Modifier::BOLD))]);
            f.render_widget(Paragraph::new(vec![line, Line::styled(" enter ok · esc", ui::muted(&t))]), inner);
        }
        if let Some(ob) = &mut self.onboard {
            ob.draw(f, area, &t, self.start.elapsed().as_secs_f64());
        }
    }

    /// The sidebar: the apps in sections (F1-F10), your own tabs, then the current app's own section.
    fn draw_sidebar(&mut self, f: &mut Frame, area: Rect, t: &Theme) {
        let time = self.start.elapsed().as_secs_f64();
        let cur_app = self.tabs[self.cur].app;
        let title = format!("{}oriel", ui::lead("window"));
        let sub = cur_app.unwrap_or("tabs");
        let inner = ui::frame(f, area, &title, Some(sub), false, t);
        if t.animated {
            // rainbow the title over the frame's plain one
            let spans: Vec<Span> = format!(" {title} ").chars().enumerate()
                .map(|(i, c)| Span::styled(c.to_string(), Style::default().fg(theme::rainbow(i, time)).add_modifier(Modifier::BOLD)))
                .collect();
            f.render_widget(Paragraph::new(Line::from(spans)), Rect { x: area.x + 1, y: area.y, width: area.width.saturating_sub(2), height: 1 });
        }
        // clock in the top border, right side
        let clock = format!(" {} ", clock());
        let cw = clock.chars().count() as u16;
        if area.width > cw + 12 {
            f.render_widget(Paragraph::new(Span::styled(clock, ui::muted(t))), Rect { x: area.right() - cw - 2, y: area.y, width: cw, height: 1 });
        }
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), y: inner.y + 1, height: inner.height.saturating_sub(1) };
        // ---- the footer: themes and help, pinned to the bottom
        let extra = self.update_ready.is_some() as u16;
        let foot_n = (SIDEBAR.len() - FOOTER) as u16 + extra;
        let inner = if inner.height > foot_n + 12 {
            let fy = inner.bottom() - foot_n;
            ui::rule(f, Rect { y: fy - 1, height: 1, ..inner }, t);
            if let Some(v) = &self.update_ready {
                let r = Rect { y: fy, height: 1, ..inner };
                f.render_widget(Paragraph::new(Line::from(vec![Span::styled(format!("{}update to {v}", ui::lead("package")), Style::default().fg(t.good).add_modifier(Modifier::BOLD))])), r);
                self.side_hits.push((r, SideHit::App("updates")));
            }
            let fy = fy + extra;
            for (k, &(name, icon, label, key)) in SIDEBAR[FOOTER..].iter().enumerate() {
                let r = Rect { y: fy + k as u16, height: 1, ..inner };
                let unread = if name == "alerts" { crate::alerts::unread() } else { 0 };
                if unread > 0 {
                    // the bell lights up with how many are new
                    let st = Style::default().fg(t.accent).add_modifier(Modifier::BOLD);
                    let n = format!("{unread} new");
                    let w = (r.width as usize).saturating_sub(n.len() + 1);
                    f.render_widget(Paragraph::new(Line::from(vec![Span::styled(ui::fit(&format!("{}{label}", ui::lead(icon)), w), st), Span::raw(" ".repeat(w.saturating_sub(ui::fit(&format!("{}{label}", ui::lead(icon)), w).chars().count()) + 1)), Span::styled(n, st)])), r);
                } else {
                    ui::side_row(f, r, icon, label, key, cur_app == Some(name), t);
                }
                self.side_hits.push((r, SideHit::App(name)));
            }
            Rect { height: inner.height - foot_n - 1, ..inner }
        } else {
            inner
        };
        let mut y = inner.y;
        for (idx, &(name, icon, label, key)) in SIDEBAR[..FOOTER].iter().enumerate() {
            if y >= inner.bottom() {
                break;
            }
            if let Some((_, heading)) = SECTIONS.iter().find(|(at, _)| *at == idx) {
                if idx > 0 {
                    y += 1;
                }
                if y + 1 >= inner.bottom() {
                    break;
                }
                f.render_widget(Paragraph::new(Span::styled(format!("── {heading}"), ui::muted(t))), Rect { y, height: 1, ..inner });
                y += 1;
            }
            let r = Rect { y, height: 1, ..inner };
            let on = cur_app == Some(name);
            let badge = self.tabs.iter().find(|tb| tb.app == Some(name)).and_then(|tb| self.panes.get(&tb.focus)).and_then(|p| p.badge());
            let right = match &badge {
                Some(b) => format!("{}  {key}", ui::fit(b, (inner.width as usize).saturating_sub(label.len() + 8).max(1))),
                None => key.to_string(),
            };
            ui::side_row(f, r, icon, label, &right, on, t);
            self.side_hits.push((r, SideHit::App(name)));
            y += 1;
        }
        // your own tabs
        y += 1;
        if y < inner.bottom() {
            ui::rule(f, Rect { y, height: 1, ..inner }, t);
            y += 1;
        }
        let user = self.user_tabs();
        for (n, &i) in user.iter().enumerate() {
            if y >= inner.bottom() {
                break;
            }
            let tb = &self.tabs[i];
            let icon = self.panes.get(&tb.focus).map(|p| p.icon()).unwrap_or("term");
            let title = self.tab_label(i);
            let panes = { let mut l = vec![]; tb.root.leaves(&mut l); l.len() };
            let right = if panes > 1 { format!("{panes} panes  alt {}", n + 1) } else { format!("alt {}", n + 1) };
            let r = Rect { y, height: 1, ..inner };
            let xr = Rect { x: r.right().saturating_sub(2), width: 2, ..r };
            let dot = self.tab_dot(i);
            let (glyph, color) = match dot {
                Dot::Blocked => ("●", t.danger),
                Dot::Working => (["◐", "◓", "◑", "◒"][(time * 6.0) as usize % 4], t.accent),
                Dot::Done => ("●", t.good),
                Dot::Idle => ("○", t.muted),
                Dot::None => (" ", t.muted),
            };
            if let Some((_, text)) = self.renaming.as_ref().filter(|(rt, _)| *rt == tb.id) {
                let line = Line::from(vec![
                    Span::styled(format!("{glyph} "), Style::default().fg(color)),
                    Span::styled(format!("{}{}", text, "▏"), Style::default().fg(t.accent).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)),
                    Span::styled("  enter ok · esc", ui::muted(t)),
                ]);
                f.render_widget(Paragraph::new(line), r);
            } else {
                f.render_widget(Paragraph::new(Span::styled(glyph, Style::default().fg(color))), Rect { width: 2, ..r });
                ui::side_row(f, Rect { x: r.x + 2, width: r.width.saturating_sub(5), ..r }, icon, &title, &right, i == self.cur, t);
                let hot = xr.contains(self.hover);
                let style = if hot { Style::default().fg(t.danger).add_modifier(Modifier::BOLD) } else { ui::muted(t) };
                f.render_widget(Paragraph::new(Span::styled(" ×", style)), xr);
            }
            // the × first, so a click on it wins over the row
            let tid = tb.id;
            self.side_hits.push((xr, SideHit::CloseTab(tid)));
            self.side_hits.push((r, SideHit::Tab(tid)));
            y += 1;
        }
        if y < inner.bottom() {
            let r = Rect { y, height: 1, ..inner };
            ui::side_row(f, r, "tab", "new tab", "alt t", false, t);
            self.side_hits.push((r, SideHit::NewTab));
            y += 1;
        }
        // the current app's own section
        if let Some(_) = cur_app {
            y += 1;
            if y < inner.bottom() {
                ui::rule(f, Rect { y, height: 1, ..inner }, t);
                y += 1;
            }
            if y + 1 < inner.bottom() {
                let r = Rect { y: y + 1, height: inner.bottom() - y - 1, ..inner };
                let id = self.tabs[self.cur].focus;
                let mut actions = vec![];
                if let Some(p) = self.panes.get_mut(&id) {
                    let mut cx = Cx { id, theme: t, config: &self.config, tx: &self.tx, actions: &mut actions, focused: false, time };
                    p.side(f, r, &mut cx);
                }
                self.side_area = Some((id, r));
                if !actions.is_empty() {
                    self.apply(id, actions);
                }
            }
        }
    }

    /// Prefix hint / notices, bottom-right over everything.
    fn draw_toast(&self, f: &mut Frame, area: Rect, t: &Theme) {
        let (text, title) = if self.prefix_armed {
            (" | - split · hjkl move · x close · z zoom · c tab · t theme · ? help ".to_string(), "prefix")
        } else if let Some(q) = self.confirm_text() {
            (format!(" {q} "), "careful")
        } else if let Some((n, _)) = &self.notice {
            (format!(" {n} "), "oriel")
        } else {
            return;
        };
        let w = (unicode_width::UnicodeWidthStr::width(text.as_str()) as u16 + 10).min(area.width);
        let r = Rect { x: area.right().saturating_sub(w + 1), y: area.bottom().saturating_sub(4), width: w, height: 3.min(area.height) };
        f.render_widget(ratatui::widgets::Clear, r);
        let inner = ui::frame(f, r, title, None, true, t);
        f.render_widget(Paragraph::new(Span::styled(text, ui::accent(t))), inner);
    }

    fn draw_palette(f: &mut Frame, area: Rect, p: &mut Palette, t: &Theme) {
        let m = Self::palette_matches(p);
        let inner = ui::popup(f, area, 64, 18, &format!("{}palette", ui::lead("search")), t);
        p.rect = Rect { x: inner.x.saturating_sub(1), y: inner.y.saturating_sub(1), width: inner.width + 2, height: inner.height + 2 };
        p.rows.clear();
        let q = Line::from(vec![Span::styled("› ", ui::bold_accent(t)), Span::raw(p.query.clone()), Span::styled("▏", ui::accent(t))]);
        f.render_widget(Paragraph::new(q), Rect { height: 1, ..inner });
        let rows = inner.height.saturating_sub(2) as usize;
        let start = p.sel.saturating_sub(rows.saturating_sub(1));
        for (row, &i) in m.iter().skip(start).take(rows).enumerate() {
            let on = start + row == p.sel;
            let style = if on { Style::default().fg(t.accent).add_modifier(Modifier::BOLD) } else { Style::default() };
            let line = Line::from(vec![Span::styled(if on { "▌ " } else { "  " }, ui::accent(t)), Span::styled(p.items[i].0.clone(), style)]);
            let r = Rect { y: inner.y + 2 + row as u16, height: 1, ..inner };
            f.render_widget(Paragraph::new(line), r);
            p.rows.push((r, start + row));
        }
        if m.is_empty() {
            f.render_widget(Paragraph::new(Span::styled("  nothing matches", ui::muted(t))), Rect { y: inner.y + 2, height: 1, ..inner });
        }
    }
}

fn clock() -> String {
    // local time without a date crate: good enough via the OS
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let off = local_offset_secs();
    let s = (now as i64 + off).rem_euclid(86400);
    format!("{:02}:{:02}", s / 3600, (s % 3600) / 60)
}

/// Seconds east of UTC, from the OS. Asked again every hour, so the clock follows a DST change.
pub(crate) fn local_offset_secs() -> i64 {
    use std::sync::Mutex;
    static OFF: Mutex<Option<(Instant, i64)>> = Mutex::new(None);
    let mut off = OFF.lock().unwrap_or_else(|e| e.into_inner());
    match *off {
        Some((at, s)) if at.elapsed() < Duration::from_secs(3600) => s,
        _ => {
            let s = os_offset_secs();
            *off = Some((Instant::now(), s));
            s
        }
    }
}

/// Windows: straight from the time zone settings (a PowerShell for this cost ~230 ms at every start).
#[cfg(windows)]
fn os_offset_secs() -> i64 {
    #[repr(C)]
    struct TimeZoneInformation {
        bias: i32,
        standard_name: [u16; 32],
        standard_date: [u16; 8],
        standard_bias: i32,
        daylight_name: [u16; 32],
        daylight_date: [u16; 8],
        daylight_bias: i32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetTimeZoneInformation(tzi: *mut TimeZoneInformation) -> u32;
    }
    // SAFETY: a plain C struct the call fills in; all-zero is a valid value for it
    let mut tzi: TimeZoneInformation = unsafe { std::mem::zeroed() };
    let bias = match unsafe { GetTimeZoneInformation(&mut tzi) } {
        0 => tzi.bias,                      // no daylight saving here
        1 => tzi.bias + tzi.standard_bias,  // standard time now
        2 => tzi.bias + tzi.daylight_bias,  // daylight time now
        _ => return 0,
    };
    // bias is minutes to add to local time for UTC
    -(bias as i64) * 60
}

#[cfg(not(windows))]
fn os_offset_secs() -> i64 {
    let out = std::process::Command::new("date").arg("+%z").output();
    out.ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let sign = if s.starts_with('-') { -1 } else { 1 };
            let d = s.trim_start_matches(['+', '-']);
            let h: i64 = d.get(0..2)?.parse().ok()?;
            let m: i64 = d.get(2..4)?.parse().ok()?;
            Some(sign * (h * 3600 + m * 60))
        })
        .unwrap_or(0)
}

fn is_move(e: &Event) -> bool {
    matches!(e, Event::Input(CEvent::Mouse(m)) if m.kind == MouseEventKind::Moved)
}

/// The calendar keeps its reminders only while it runs, so open it in the background when a timed plan is coming.
fn startup_needs_calendar(startup: &str) -> bool {
    startup != "calendar" && crate::panes::calendar::has_timed_plans()
}

/// Your theme files: saving one recolours oriel.
fn watch_themes(tx: Sender<Event>) -> Option<notify::RecommendedWatcher> {
    use notify::{RecursiveMode, Watcher};
    if cfg!(test) {
        return None;
    }
    let dir = theme::themes_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let changed = debounced(tx, Duration::from_millis(100)); // editors write in two steps
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok_and(|e| e.kind.is_modify() || e.kind.is_create()) {
            let _ = changed.send(());
        }
    })
    .ok()?;
    w.watch(&dir, RecursiveMode::NonRecursive).ok()?;
    Some(w)
}

fn watch_omarchy(tx: Sender<Event>) -> Option<notify::RecommendedWatcher> {
    use notify::{RecursiveMode, Watcher};
    let dir = theme::omarchy_watch_dir()?;
    let changed = debounced(tx, Duration::from_millis(150)); // omarchy swaps several files
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            let _ = changed.send(());
        }
    })
    .ok()?;
    w.watch(&dir, RecursiveMode::NonRecursive).ok()?;
    Some(w)
}

/// A burst of file events (a key held down in the themes app writes on every repeat) becomes one
/// ThemeFilesChanged, sent once they've been quiet for `quiet` — the waiting happens here, not on the UI thread.
fn debounced(tx: Sender<Event>, quiet: Duration) -> Sender<()> {
    let (ping, pings) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        while pings.recv().is_ok() {
            loop {
                match pings.recv_timeout(quiet) {
                    Ok(()) => continue,
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            if tx.send(Event::ThemeFilesChanged).is_err() {
                return;
            }
        }
    });
    ping
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    /// Whole-screen snapshot: `cargo test app_snapshot -- --nocapture`, then `pwsh tools\snap.ps1 target\snap\app-<name>.html`.
    fn snap(app: &mut App, name: &str) -> String {
        let mut term = Terminal::new(TestBackend::new(160, 48)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        crate::testkit::save_html(term.backend().buffer(), &format!("target/snap/app-{name}.html"));
        let buf = term.backend().buffer();
        (0..buf.area.height).map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n")
    }

    /// The README screenshots (no personal chats): `cargo test docs_screenshots -- --ignored`,
    /// then tools\snap.ps1 on docs\*.html. The agent transcript one lives in panes::chat (docs_agent_screenshot).
    #[test]
    #[ignore]
    fn docs_screenshots() {
        let demo = std::path::Path::new("target/demo");
        let _ = std::fs::remove_dir_all(demo);
        std::fs::create_dir_all(demo.join("chats")).unwrap();
        unsafe { std::env::set_var("ORIEL_DATA_DIR", std::path::absolute(demo).unwrap()) };
        let now = crate::panes::chat::demo_now();
        for (i, (title, ago)) in [("debounce a search box", 60.0), ("plan a 3 day trip to lisbon", 18000.0), ("explain rust lifetimes simply", 100000.0),
            ("regex for a uk postcode", 260000.0), ("fix the flaky login test", 300000.0), ("what should I name my cat", 800000.0)].iter().enumerate() {
            let c = serde_json::json!({"id": format!("d{i}"), "title": title, "created": now - ago, "updated": now - ago, "provider": "claude",
                "messages": [{"role": "user", "content": title}, {"role": "assistant", "content": "…"}]});
            std::fs::write(demo.join("chats").join(format!("d{i}.json")), c.to_string()).unwrap();
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let mut cfg = Config::default();
        cfg.theme = "ultra".into(); // the default look
        cfg.ai.provider = "claude".into();
        let mut app = App::new(cfg, tx);
        // a renamed tab with a finished agent, so the sidebar shows tabs + status dots
        let mut term = Terminal::new(TestBackend::new(150, 42)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        crate::testkit::save_html(term.backend().buffer(), "docs/screenshot-hero.html");
        // system summary with ~45 s of history
        app.goto_app("system");
        for _ in 0..460 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            while let Ok(e) = rx.try_recv() { app.handle(e); }
            app.handle(Event::Tick);
        }
        term.draw(|f| app.draw(f)).unwrap();
        crate::testkit::save_html(term.backend().buffer(), "docs/screenshot-system.html");
        // the app catalog (storage → get apps)
        app.goto_app("storage");
        term.draw(|f| app.draw(f)).unwrap();
        for _ in 0..3 {
            app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        }
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(20) {
            while let Ok(e) = rx.try_recv() { app.handle(e); }
            std::thread::sleep(Duration::from_millis(200));
            term.draw(|f| app.draw(f)).unwrap();
            let b = term.backend().buffer();
            let text: String = (0..b.area.height).flat_map(|y| (0..b.area.width).map(move |x| (x, y))).map(|(x, y)| b[(x, y)].symbol().to_string()).collect();
            if text.contains("✓ installed") { break; }
        }
        crate::testkit::save_html(term.backend().buffer(), "docs/screenshot-apps.html");
    }
    #[test]
    fn app_help_screen() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut cfg = Config::default();
        cfg.theme = "ultra".into();
        let mut app = App::new(cfg, tx);
        let mut term = Terminal::new(TestBackend::new(150, 42)).unwrap();
        let mut shot = |app: &mut App, name: &str| {
            term.draw(|f| app.draw(f)).unwrap();
            crate::testkit::save_html(term.backend().buffer(), &format!("target/snap/app-help-{name}.html"));
            let b = term.backend().buffer();
            (0..b.area.height).map(|y| (0..b.area.width).map(|x| b[(x, y)].symbol().to_string()).collect::<String>() + "
").collect::<String>()
        };
        // F10 from chat opens help on the chat topic; the sidebar lists the topics
        let s = shot(&mut app, "chat0");
        assert!(s.contains("help") && s.contains("F10"), "{s}");
        app.key(KeyEvent::new(KeyCode::F(10), KeyModifiers::NONE));
        let s = shot(&mut app, "chat");
        assert!(s.contains("/perms bypass") && s.contains("getting started") && s.contains("troubleshooting"), "{s}");
        // ? on the home launcher (a tab of your own) opens getting started
        app.new_tab(Box::new(crate::panes::home::Home::new()));
        app.key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        let s = shot(&mut app, "start");
        assert!(s.contains("oriel in one minute"), "{s}");
        // and the palette has it
        app.goto_app("files");
        app.open_palette();
        if let Some(p) = &mut app.palette {
            p.query = "help".into();
        }
        assert!(shot(&mut app, "palette").contains("every key, command and how-to"));
    }

    #[test]
    fn app_rename_menu_agents() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut cfg = Config::default();
        cfg.theme = "oriel".into();
        let mut app = App::new(cfg, tx);
        let key = |app: &mut App, c: KeyCode, m: KeyModifiers| app.key(KeyEvent::new(c, m));
        // a fake agent: shows Claude's "esc to interrupt", then finishes (clears the screen)
        let agent: Box<dyn Pane> = if cfg!(windows) {
            Box::new(crate::panes::term::Term::new("claude", "claude", "pwsh.exe", vec!["-NoProfile".into(), "-Command".into(), "Write-Host '* thinking (esc to interrupt)'; Start-Sleep 3; Clear-Host; Write-Host 'done'; Start-Sleep 60".into()], None))
        } else {
            Box::new(crate::panes::term::Term::new("claude", "claude", "sh", vec!["-c".into(), "echo '* thinking (esc to interrupt)'; sleep 3; clear; echo done; sleep 60".into()], None))
        };
        app.new_tab(agent);
        let agent_tab = app.cur;
        let _ = snap(&mut app, "agent0"); // starts the pty
        // rename it: prefix then ,
        key(&mut app, KeyCode::Char(' '), KeyModifiers::CONTROL);
        key(&mut app, KeyCode::Char(','), KeyModifiers::NONE);
        for _ in 0..10 { key(&mut app, KeyCode::Backspace, KeyModifiers::NONE); }
        for c in "refactor".chars() { key(&mut app, KeyCode::Char(c), KeyModifiers::NONE); }
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.tabs[agent_tab].name.as_deref(), Some("refactor"));
        // watch it work from another tab
        app.goto_app("ai");
        let mut saw_working = false;
        let t0 = Instant::now();
        // (a busy machine can take seconds just to start pwsh: the loop ends as soon as it's done)
        while t0.elapsed() < Duration::from_secs(25) {
            while let Ok(e) = rx.try_recv() { app.handle(e); }
            app.handle(Event::Tick);
            app.track_agents();
            if app.tab_dot(agent_tab) == Dot::Working { saw_working = true; }
            if saw_working && app.tab_dot(agent_tab) == Dot::Done { break; }
            std::thread::sleep(Duration::from_millis(100));
        }
        let s = snap(&mut app, "agents");
        println!("{}", s.lines().take(16).collect::<Vec<_>>().join("\n"));
        assert!(saw_working, "never saw the agent working");
        assert_eq!(app.tab_dot(agent_tab) as u8, Dot::Done as u8, "agent should be done (finished while unseen)");
        assert!(app.notice.as_ref().map(|n| n.0.contains("finished")).unwrap_or(false));
        // right-click menu on the pane
        app.cur = agent_tab;
        let _ = snap(&mut app, "x");
        let (_, r) = app.outer[0];
        app.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Right), column: r.x + 5, row: r.y + 5, modifiers: KeyModifiers::NONE });
        let s = snap(&mut app, "ctx");
        assert!(s.contains("split right") && s.contains("rename tab"));
    }

    #[test]
    fn app_onboarding_flow() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut cfg = Config::default();
        cfg.theme = "ultra".into(); // what a first run looks like
        let mut app = App::new(cfg, tx);
        app.start_tour();
        let key = |app: &mut App, c: KeyCode| app.key(KeyEvent::new(c, KeyModifiers::NONE));
        let mut term = Terminal::new(TestBackend::new(150, 42)).unwrap();
        let mut shot = |app: &mut App, name: &str| -> String {
            term.draw(|f| app.draw(f)).unwrap();
            crate::testkit::save_html(term.backend().buffer(), &format!("target/snap/onboard-{name}.html"));
            let b = term.backend().buffer();
            (0..b.area.height).map(|y| (0..b.area.width).map(|x| b[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
        };
        assert!(shot(&mut app, "welcome").contains("take the tour"));
        key(&mut app, KeyCode::Enter);
        assert!(shot(&mut app, "theme").contains("pick a look"));
        key(&mut app, KeyCode::Down); // live preview
        key(&mut app, KeyCode::Enter);
        assert!(shot(&mut app, "icons").contains("little pictures"));
        key(&mut app, KeyCode::Char('y'));
        assert!(shot(&mut app, "music").contains("your music"));
        key(&mut app, KeyCode::Enter);
        assert!(shot(&mut app, "notes").contains("your notes"));
        key(&mut app, KeyCode::Enter);
        assert!(shot(&mut app, "defaults").contains("open oriel on"));
        key(&mut app, KeyCode::Right);
        key(&mut app, KeyCode::Enter);
        assert!(shot(&mut app, "ais").contains("your AIs"));
        key(&mut app, KeyCode::Enter); // into the tour
        assert!(shot(&mut app, "tour1").contains("switch apps"));
        // doing the thing moves the tour on
        key(&mut app, KeyCode::F(4));
        let p = app.probe();
        let out = app.onboard.as_mut().unwrap().check(&p);
        app.onboard_out(out);
        assert!(shot(&mut app, "tour2").contains("chat with any AI"));
        key(&mut app, KeyCode::F(1));
        let p = app.probe();
        let out = app.onboard.as_mut().unwrap().check(&p);
        app.onboard_out(out);
        assert!(shot(&mut app, "tour3").contains("installs and signs in"));
        // skip ahead to the help step: F10 there opens help (the app's key) and that finishes the step
        for _ in 0..10 {
            key(&mut app, KeyCode::F(10));
        }
        assert!(shot(&mut app, "tour-help").contains("the help screen"));
        key(&mut app, KeyCode::F(10));
        let p = app.probe();
        let out = app.onboard.as_mut().unwrap().check(&p);
        app.onboard_out(out);
        let s = shot(&mut app, "tour-last");
        assert!(s.contains("you're set") && s.contains("help ·"), "{s}");
        // end it
        key(&mut app, KeyCode::F(11));
        assert!(app.onboard.is_none());
        let _ = rx;
    }

    #[test]
    fn app_close_buttons() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut cfg = Config::default();
        cfg.theme = "ultra".into();
        let mut app = App::new(cfg, tx);
        let mut term = Terminal::new(TestBackend::new(150, 42)).unwrap();
        app.goto_app("ais");
        term.draw(|f| app.draw(f)).unwrap();
        assert!(app.pane_close.is_empty(), "a lone app pane has no ×");
        // an install opens a terminal split inside the app's tab
        let ais = app.focused();
        let t: Box<dyn Pane> = Box::new(crate::panes::term::Term::shell(&app.config, None));
        app.apply(ais, vec![Action::Open(t, Place::Split)]);
        term.draw(|f| app.draw(f)).unwrap();
        crate::testkit::save_html(term.backend().buffer(), "target/snap/app-close-split.html");
        assert_eq!(app.pane_close.len(), 2, "both panes of a split get a ×");
        let (b, id) = app.pane_close.iter().copied().find(|(_, id)| *id != ais).unwrap();
        let click = |app: &mut App, x: u16, y: u16, btn: MouseButton| app.mouse(MouseEvent { kind: MouseEventKind::Down(btn), column: x, row: y, modifiers: KeyModifiers::NONE });
        click(&mut app, b.x + 1, b.y, MouseButton::Left);
        assert!(!app.panes.contains_key(&id), "× closed the terminal");
        term.draw(|f| app.draw(f)).unwrap();
        assert!(app.pane_close.is_empty() && app.tabs[app.cur].app == Some("ais"), "back to just your AIs");
        // your own tab: × in the sidebar closes it
        app.new_tab(Box::new(crate::panes::home::Home::new()));
        let n = app.tabs.len();
        term.draw(|f| app.draw(f)).unwrap();
        crate::testkit::save_html(term.backend().buffer(), "target/snap/app-close-tab.html");
        let (xr, _) = app.side_hits.iter().copied().find(|(_, h)| matches!(h, SideHit::CloseTab(_))).expect("× on the tab row");
        click(&mut app, xr.x + 1, xr.y, MouseButton::Left);
        assert_eq!(app.tabs.len(), n - 1, "sidebar × closed the tab");
        // middle-click works too
        app.new_tab(Box::new(crate::panes::home::Home::new()));
        term.draw(|f| app.draw(f)).unwrap();
        let (r, _) = app.side_hits.iter().copied().find(|(_, h)| matches!(h, SideHit::Tab(_))).unwrap();
        click(&mut app, r.x + 3, r.y, MouseButton::Middle);
        assert_eq!(app.tabs.len(), n - 1, "middle-click closed the tab");
    }

    #[test]
    fn app_select_copy_and_image_paste() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut cfg = Config::default();
        cfg.theme = "ultra".into();
        let mut app = App::new(cfg, tx);
        app.new_tab(Box::new(crate::panes::home::Home::new()));
        let mut term = Terminal::new(TestBackend::new(150, 42)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        // find the tagline on screen and drag across it
        let text_at = |term: &Terminal<TestBackend>| -> Vec<String> {
            let b = term.backend().buffer();
            (0..b.area.height).map(|y| (0..b.area.width).map(|x| b[(x, y)].symbol()).collect::<String>()).collect()
        };
        let lines = text_at(&term);
        let (row, line) = lines.iter().enumerate().find(|(_, l)| l.contains("a window onto everything")).unwrap();
        let col = line.find("a window").unwrap();
        let col = line[..col].chars().count() as u16;
        let ev = |app: &mut App, kind, x, y| app.mouse(MouseEvent { kind, column: x, row: y, modifiers: KeyModifiers::NONE });
        ev(&mut app, MouseEventKind::Down(MouseButton::Left), col, row as u16);
        ev(&mut app, MouseEventKind::Drag(MouseButton::Left), col + 7, row as u16);
        ev(&mut app, MouseEventKind::Up(MouseButton::Left), col + 7, row as u16);
        term.draw(|f| app.draw(f)).unwrap();
        crate::testkit::save_html(term.backend().buffer(), "target/snap/app-select.html");
        let b = term.backend().buffer();
        assert!(b[(col, row as u16)].modifier.contains(Modifier::REVERSED), "selection highlighted");
        assert_eq!(app.notice.as_ref().map(|n| n.0.clone()).unwrap_or_default(), "copied 8 characters", "'a window' copied");
        // a key clears it
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(app.sel.is_none());
        // a clipboard image lands in the chat composer as a path
        app.goto_app("ai");
        term.draw(|f| app.draw(f)).unwrap();
        let id = app.focused();
        app.handle(Event::Clipboard(id, Some(vec!["C:/tmp/paste-1.png".into()]), KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT)));
        let s = text_at({ term.draw(|f| app.draw(f)).unwrap(); &term }).join("\n");
        assert!(s.contains("C:/tmp/paste-1.png"), "path pasted into the composer");
        assert!(app.notice.as_ref().unwrap().0.contains("pasted image"));
    }

    #[test]
    fn app_slash_and_palette() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut cfg = Config::default();
        cfg.theme = "oriel".into();
        let mut app = App::new(cfg, tx);
        let _ = snap(&mut app, "k0");
        let key = |app: &mut App, c: KeyCode, m: KeyModifiers| app.key(KeyEvent::new(c, m));
        key(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
        let s = snap(&mut app, "slash");
        println!("{}", s.lines().rev().take(16).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"));
        assert!(s.contains("/model"), "slash menu missing");
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        key(&mut app, KeyCode::Char('p'), KeyModifiers::ALT);
        for c in "theme".chars() {
            key(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        let s = snap(&mut app, "palette");
        assert!(s.contains("theme ocean"), "theme list missing");
    }

    #[test]
    fn app_snapshot() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut cfg = Config::default();
        cfg.theme = "oriel".into();
        let mut app = App::new(cfg, tx);
        println!("{}", snap(&mut app, "ai"));
        for (i, name) in ["music", "system", "files", "notes", "storage"].iter().enumerate() {
            app.goto_app(SIDEBAR[i + 1].0);
            let s = snap(&mut app, name);
            assert!(s.contains("oriel"));
        }
        app.new_tab(Box::new(crate::panes::home::Home::new()));
        println!("{}", snap(&mut app, "home"));
    }

    // ------------------------------------------------------------------ the shell's own behaviour, headless
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    /// A stand-in pane the test drives: an agent's activity, a terminal's full-screen state; records its keys.
    #[derive(Clone, Default)]
    struct Ctl {
        act: Rc<Cell<Option<Activity>>>,
        fkeys: Rc<Cell<bool>>,
        keys: Rc<RefCell<Vec<KeyEvent>>>,
        pastes: Rc<RefCell<Vec<String>>>,
    }
    struct Fake {
        ctl: Ctl,
        term: bool,
        images: bool,
    }
    impl Pane for Fake {
        fn title(&self) -> String {
            "fake agent".into()
        }
        fn render(&mut self, _f: &mut Frame, _area: Rect, _cx: &mut Cx) {}
        fn key(&mut self, k: KeyEvent, _cx: &mut Cx) -> bool {
            self.ctl.keys.borrow_mut().push(k);
            true
        }
        fn paste(&mut self, text: &str, _cx: &mut Cx) {
            self.ctl.pastes.borrow_mut().push(text.to_string());
        }
        fn activity(&self) -> Option<Activity> {
            self.ctl.act.get()
        }
        fn is_terminal(&self) -> bool {
            self.term
        }
        fn wants_fkeys(&self) -> bool {
            self.ctl.fkeys.get()
        }
        fn wants_images(&self) -> bool {
            self.images
        }
    }
    fn fake(term: bool, images: bool) -> (Box<dyn Pane>, Ctl) {
        let ctl = Ctl::default();
        (Box::new(Fake { ctl: ctl.clone(), term, images }), ctl)
    }

    fn new_app() -> (App, Receiver<Event>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut cfg = Config::default();
        cfg.theme = "oriel".into();
        (App::new(cfg, tx), rx)
    }
    fn press(app: &mut App, c: KeyCode, m: KeyModifiers) {
        app.key(KeyEvent::new(c, m));
    }
    fn prefix(app: &mut App) {
        press(app, KeyCode::Char(' '), KeyModifiers::CONTROL);
    }
    fn screen(app: &mut App, w: u16, h: u16) -> String {
        shot(app, w, h, "")
    }
    /// `screen`, also saved as target/snap/app-<name>.html (tools\snap.ps1 makes a PNG of it).
    fn shot(app: &mut App, w: u16, h: u16, name: &str) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        if !name.is_empty() {
            crate::testkit::save_html(term.backend().buffer(), &format!("target/snap/app-{name}.html"));
        }
        let b = term.backend().buffer();
        (0..b.area.height).map(|y| (0..b.area.width).map(|x| b[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }
    fn notice(app: &App) -> String {
        app.notice.as_ref().map(|n| n.0.clone()).unwrap_or_default()
    }

    #[test]
    fn app_tabs_keep_their_app_pane() {
        let (mut app, _rx) = new_app();
        app.goto_app("help");
        let n = app.tabs.len();
        let id = app.focused();
        press(&mut app, KeyCode::Char('w'), KeyModifiers::ALT);
        assert!(app.panes.contains_key(&id) && app.tabs.len() == n, "alt w on a lone app pane does nothing");
        assert!(notice(&app).contains("app tabs stay open"));
        prefix(&mut app);
        press(&mut app, KeyCode::Char('&'), KeyModifiers::NONE);
        assert_eq!(app.tabs.len(), n, "nor does close tab");
        // and the menus don't offer it
        app.open_palette();
        assert!(!app.palette.as_ref().unwrap().items.iter().any(|(l, _)| l.contains("close pane") || l.contains("close this tab")));
        app.palette = None;
        screen(&mut app, 150, 42);
        let (_, r) = app.outer[0];
        app.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Right), column: r.x + 5, row: r.y + 5, modifiers: KeyModifiers::NONE });
        assert!(!app.ctx.as_ref().unwrap().items.iter().any(|(l, _)| l.contains("close pane")));
        // a split in an app tab can still be closed
        app.ctx = None;
        let (p, _) = fake(false, false);
        app.open(p, Place::SplitRight, id);
        let extra = app.focused();
        press(&mut app, KeyCode::Char('w'), KeyModifiers::ALT);
        assert!(!app.panes.contains_key(&extra) && app.panes.contains_key(&id));
    }

    #[test]
    fn app_close_and_quit_ask_while_agents_work() {
        let (mut app, _rx) = new_app();
        app.new_tab(Box::new(crate::panes::home::Home::new()));
        let home = app.focused();
        let (p, a) = fake(true, true);
        app.open(p, Place::SplitRight, home);
        let agent = app.focused();
        // idle: closes at once
        a.act.set(Some(Activity::Idle));
        assert_eq!(app.doomed(Doom::Close(agent)), 0);
        // working: alt w asks, n keeps it
        a.act.set(Some(Activity::Working));
        press(&mut app, KeyCode::Char('w'), KeyModifiers::ALT);
        assert!(app.panes.contains_key(&agent), "not closed yet");
        let s = shot(&mut app, 150, 42, "confirm-close");
        assert!(s.contains("fake agent is working · close it anyway? y / n"), "{s}");
        press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
        assert!(app.panes.contains_key(&agent) && app.confirm.is_none());
        assert!(a.keys.borrow().is_empty(), "the n answered the question, it didn't go to the agent");
        // another key drops the question (and goes where it was going)
        press(&mut app, KeyCode::Char('w'), KeyModifiers::ALT);
        press(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
        assert!(app.confirm.is_none() && app.panes.contains_key(&agent));
        assert_eq!(a.keys.borrow().len(), 1);
        // y closes it
        press(&mut app, KeyCode::Char('w'), KeyModifiers::ALT);
        press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(!app.panes.contains_key(&agent), "y closed it");
        // alt w twice does too
        let (p, a) = fake(true, true);
        a.act.set(Some(Activity::Blocked));
        app.open(p, Place::SplitRight, home);
        let agent = app.focused();
        press(&mut app, KeyCode::Char('w'), KeyModifiers::ALT);
        assert!(app.panes.contains_key(&agent));
        press(&mut app, KeyCode::Char('w'), KeyModifiers::ALT);
        assert!(!app.panes.contains_key(&agent), "the same key again is a yes");
        // quitting with two agents at work: prefix q asks, prefix q again quits
        let (p1, a1) = fake(true, true);
        let (p2, a2) = fake(true, true);
        a1.act.set(Some(Activity::Working));
        a2.act.set(Some(Activity::Working));
        app.new_tab(p1);
        app.new_tab(p2);
        prefix(&mut app);
        press(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(!app.quit);
        assert!(screen(&mut app, 150, 42).contains("2 agents still working · quit anyway? y / n"));
        prefix(&mut app);
        press(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(app.quit, "asked twice");
        // /quit from a chat goes through the same question
        app.quit = false;
        app.apply(home, vec![Action::Quit]);
        assert!(!app.quit && app.confirm.is_some());
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        // closing a tab with an agent at work: middle-click asks, a second one closes it
        let tabs = app.tabs.len();
        screen(&mut app, 150, 42);
        let (r, _) = app.side_hits.iter().copied().filter(|(_, h)| matches!(h, SideHit::Tab(_))).last().unwrap();
        let middle = |app: &mut App| app.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Middle), column: r.x + 3, row: r.y, modifiers: KeyModifiers::NONE });
        middle(&mut app);
        assert_eq!(app.tabs.len(), tabs);
        assert!(app.confirm_text().unwrap().contains("still working in this tab"));
        middle(&mut app);
        assert_eq!(app.tabs.len(), tabs - 1);
        // nothing at work: quit is instant
        a1.act.set(None);
        prefix(&mut app);
        press(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(app.quit);
    }

    #[test]
    fn app_agent_done_while_you_are_elsewhere() {
        let (mut app, _rx) = new_app();
        let (p, a) = fake(true, false);
        app.new_tab(p);
        let agent = app.focused();
        a.act.set(Some(Activity::Working));
        app.track_agents();
        // you alt-tab to the browser; it finishes in the tab you left open
        app.handle(Event::Input(CEvent::FocusLost));
        assert!(!app.term_focused);
        a.act.set(Some(Activity::Idle));
        app.track_agents();
        assert!(app.done.contains(&agent), "a green dot for when you're back");
        assert!(notice(&app).contains("fake agent finished"), "{}", notice(&app));
        assert_eq!(crate::alerts::with(|l| l.iter().filter(|x| x.pane == Some(agent) && !x.read).count()), 1, "kept as unread");
        // back in the terminal: looking at it clears the dot
        app.handle(Event::Input(CEvent::FocusGained));
        app.track_agents();
        assert!(!app.done.contains(&agent));
        // a key typed here means you're here, even if the terminal never sent FocusGained
        app.handle(Event::Input(CEvent::FocusLost));
        app.handle(Event::Input(CEvent::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))));
        assert!(app.term_focused);
        // zoomed onto another pane in the same tab: the hidden agent counts as out of sight too
        let (p2, _) = fake(false, false);
        app.open(p2, Place::SplitRight, agent);
        app.tabs[app.cur].zoom = true;
        a.act.set(Some(Activity::Working));
        app.track_agents();
        a.act.set(Some(Activity::Blocked));
        app.notice = None;
        app.track_agents();
        assert!(notice(&app).contains("needs you"));
    }

    #[test]
    fn app_tabs_stay_put_when_others_close() {
        let (mut app, _rx) = new_app();
        let mut ids = vec![];
        for _ in 0..3 {
            app.new_tab(Box::new(crate::panes::home::Home::new()));
            ids.push(app.focused());
        }
        // watching (and renaming) the second; the first goes away in the background
        let second = app.tabs.iter().position(|t| t.root.contains(ids[1])).unwrap();
        app.cur = second;
        app.start_rename(app.cur);
        app.close(ids[0]);
        assert_eq!(app.focused(), ids[1], "still on the same tab");
        for _ in 0..20 {
            press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        }
        for c in "mine".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let named = |app: &App, id: PaneId| app.tabs.iter().find(|t| t.root.contains(id)).and_then(|t| t.name.clone());
        assert_eq!((named(&app, ids[1]).as_deref(), named(&app, ids[2])), (Some("mine"), None), "the rename went to the tab it started on");
        // the orchestrator's worker tabs: one closing doesn't move you off the one you watch
        let origin = app.focused();
        for t in ["w1", "w2", "w3"] {
            let (p, _) = fake(true, false);
            app.apply(origin, vec![Action::OpenTagged { pane: p, tag: t.into(), name: t.into(), focus: false }]);
        }
        assert_eq!(app.focused(), origin, "opened in the background");
        app.apply(origin, vec![Action::FocusTag("w2".into())]);
        let w2 = app.focused();
        app.apply(origin, vec![Action::CloseTag("w1".into())]);
        assert_eq!(app.focused(), w2);
        // a chat in a tab of yours sends a key to an app that isn't open yet (its tab goes in ahead of yours)
        app.cur = app.tabs.iter().position(|t| t.root.contains(ids[1])).unwrap();
        app.apply(ids[1], vec![Action::AppKey("help", 'j')]);
        assert_eq!(app.focused(), ids[1], "AppKey doesn't switch you");
        assert!(app.tabs.iter().any(|t| t.app == Some("help")));
    }

    /// Where `needle` is on the drawn screen, in cells.
    fn find(app: &mut App, w: u16, h: u16, needle: &str) -> Option<(u16, u16)> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let b = term.backend().buffer();
        for y in 0..h {
            for x in 0..w {
                let mut s = String::new();
                for cx in x..w {
                    s.push_str(b[(cx, y)].symbol());
                    if s.len() >= needle.len() {
                        break;
                    }
                }
                if s.starts_with(needle) {
                    return Some((x, y));
                }
            }
        }
        None
    }

    #[test]
    fn app_paste_and_clicks_go_to_the_modal() {
        let (mut app, _rx) = new_app();
        let (p, pane) = fake(false, false);
        app.new_tab(p);
        // the palette takes a paste (and previews the theme it lands on)
        app.open_palette();
        app.handle(Event::Input(CEvent::Paste("theme ocean".into())));
        assert_eq!(app.palette.as_ref().unwrap().query, "theme ocean");
        assert_eq!(app.theme.name, "ocean", "live preview");
        assert!(pane.pastes.borrow().is_empty());
        // a click outside closes it and puts the theme back
        screen(&mut app, 150, 42);
        app.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 1, row: 1, modifiers: KeyModifiers::NONE });
        assert!(app.palette.is_none() && app.theme.name == "oriel");
        assert!(app.sel.is_none(), "the click didn't reach the sidebar or a pane");
        // the wheel moves the pick, a click on a row runs it
        app.open_palette();
        app.palette.as_mut().unwrap().query = "zoom pane".into();
        screen(&mut app, 150, 42);
        let (r, _) = app.palette.as_ref().unwrap().rows[0];
        let zoom = app.tabs[app.cur].zoom;
        app.mouse(MouseEvent { kind: MouseEventKind::ScrollDown, column: r.x, row: r.y, modifiers: KeyModifiers::NONE });
        assert_eq!(app.palette.as_ref().unwrap().sel, 0, "one match: the wheel stays on it");
        app.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: r.x + 2, row: r.y, modifiers: KeyModifiers::NONE });
        assert!(app.palette.is_none() && app.tabs[app.cur].zoom != zoom, "clicked row ran");
        // renaming takes it (control characters dropped, 40 at most)
        app.start_rename(app.cur);
        app.handle(Event::Input(CEvent::Paste("my\ttab\n".into())));
        assert_eq!(app.renaming.as_ref().unwrap().1, "fake agentmytab");
        // in a window too narrow for the sidebar, the rename field shows in a box
        let s = shot(&mut app, 60, 20, "rename-narrow");
        assert!(s.contains("rename tab") && s.contains("fake agentmytab"), "{s}");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        // setup's folder field takes a pasted path (Explorer's "copy as path" quotes included)
        let dir = std::path::absolute("target/test-scratch/shell/music").unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.mp3"), b"").unwrap();
        app.start_tour();
        let ob = app.onboard.as_mut().unwrap();
        ob.stage = crate::onboard::Stage::Music { input: String::new(), found: None };
        app.handle(Event::Input(CEvent::Paste(format!("\"{}\"", dir.display()))));
        match &app.onboard.as_ref().unwrap().stage {
            crate::onboard::Stage::Music { input, found } => {
                assert_eq!(input, &dir.display().to_string());
                assert_eq!(*found, Some(1));
            }
            _ => panic!("left the music step"),
        }
        assert!(pane.pastes.borrow().is_empty(), "nothing leaked into the pane behind");
        // the tour card owns clicks on its body, not just its buttons
        let probe = app.probe();
        app.onboard.as_mut().unwrap().stage = crate::onboard::Stage::Tour { step: 0, start: probe };
        let (col, row) = find(&mut app, 150, 42, "Everything lives").expect("the tour card");
        app.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: col + 3, row, modifiers: KeyModifiers::NONE });
        assert!(app.sel.is_none(), "no selection started in the pane under the card");
        // with nothing modal up, a paste goes to the pane
        app.onboard = None;
        app.handle(Event::Input(CEvent::Paste("hello".into())));
        assert_eq!(*pane.pastes.borrow(), ["hello"]);
    }

    #[test]
    fn app_tiny_windows_dont_crash() {
        let (mut app, _rx) = new_app();
        let (p, _) = fake(false, false);
        app.new_tab(p);
        // A/(B/(C/D)): split down three times from the new bottom pane, then right
        for _ in 0..3 {
            let (p, _) = fake(false, false);
            let from = app.focused();
            app.open(p, Place::SplitDown, from);
        }
        let (p, _) = fake(false, false);
        let from = app.focused();
        app.open(p, Place::SplitRight, from);
        for (w, h) in [(80, 7), (80, 3), (40, 1), (10, 2), (1, 1), (3, 30)] {
            screen(&mut app, w, h);
        }
        // and with the popups up: the rename box (no room for the sidebar), the palette, a toast
        app.start_rename(app.cur);
        app.open_palette();
        app.notify("a toast");
        for (w, h) in [(60, 7), (20, 3), (5, 2), (1, 1)] {
            screen(&mut app, w, h);
        }
    }

    #[test]
    fn app_ctrl_v_only_where_images_help() {
        let (mut app, rx) = new_app();
        // a plain terminal (vim, a shell): ctrl+v goes straight through, in order
        let (p, vim) = fake(true, false);
        app.new_tab(p);
        let ctrl_v = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL);
        app.key(ctrl_v);
        press(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
        assert!(app.pending_clip.is_none());
        assert_eq!(vim.keys.borrow().iter().map(|k| k.code).collect::<Vec<_>>(), [KeyCode::Char('v'), KeyCode::Char('j')]);
        // a coding agent: ctrl+v checks the clipboard first, and keys typed meanwhile wait behind it
        let (p, claude) = fake(true, true);
        app.new_tab(p);
        let id = app.focused();
        app.key(ctrl_v);
        assert!(app.pending_clip.is_some());
        press(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('k'), KeyModifiers::NONE);
        assert!(claude.keys.borrow().is_empty(), "held until the clipboard answers");
        // (the test clipboard is always empty: the key itself goes through, then the held ones)
        let t0 = Instant::now();
        loop {
            let ev = rx.recv_timeout(Duration::from_secs(5)).expect("the clipboard check answers");
            let done = matches!(ev, Event::Clipboard(i, None, _) if i == id);
            app.handle(ev);
            if done || t0.elapsed() > Duration::from_secs(5) {
                break;
            }
        }
        assert_eq!(claude.keys.borrow().iter().map(|k| k.code).collect::<Vec<_>>(), [KeyCode::Char('v'), KeyCode::Char('j'), KeyCode::Char('k')]);
        assert!(app.pending_clip.is_none());
        // alt+v is still global; a held key doesn't start a check per repeat
        app.goto_app("help");
        let mut rep = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT);
        rep.kind = KeyEventKind::Repeat;
        app.key(rep);
        assert!(app.pending_clip.is_none());
        app.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT));
        assert!(app.pending_clip.is_some());
    }

    #[test]
    fn app_fkeys_reach_full_screen_programs() {
        let (mut app, _rx) = new_app();
        let (p, htop) = fake(true, false);
        app.new_tab(p);
        let tab = app.tabs[app.cur].id;
        let back = |app: &mut App| app.cur = app.tab_index(tab).unwrap();
        // at a prompt, F10 is help...
        press(&mut app, KeyCode::F(10), KeyModifiers::NONE);
        assert_eq!(app.tabs[app.cur].app, Some("help"));
        // ...in htop it's htop's
        back(&mut app);
        htop.fkeys.set(true);
        press(&mut app, KeyCode::F(10), KeyModifiers::NONE);
        assert_eq!(app.tabs[app.cur].id, tab);
        assert_eq!(htop.keys.borrow().last().map(|k| k.code), Some(KeyCode::F(10)));
        // prefix then the F-key switches apps from there
        prefix(&mut app);
        press(&mut app, KeyCode::F(10), KeyModifiers::NONE);
        assert_eq!(app.tabs[app.cur].app, Some("help"));
        // and at a prompt, prefix then F2 sends F2 to the shell (PSReadLine's prediction view)
        back(&mut app);
        htop.fkeys.set(false);
        prefix(&mut app);
        press(&mut app, KeyCode::F(2), KeyModifiers::NONE);
        assert_eq!((app.tabs[app.cur].id, htop.keys.borrow().last().map(|k| k.code)), (tab, Some(KeyCode::F(2))));
        // F12 with no music app open goes to the program
        press(&mut app, KeyCode::F(12), KeyModifiers::NONE);
        assert_eq!(htop.keys.borrow().last().map(|k| k.code), Some(KeyCode::F(12)));
    }

    #[test]
    fn app_update_is_announced_once() {
        let _ = std::fs::remove_file("target/test-scratch/shell/alerts-state.json");
        let (mut app, _rx) = new_app();
        let count = || crate::alerts::with(|l| l.iter().filter(|a| a.kind == crate::alerts::Kind::Update).count());
        app.handle(Event::UpdateAvailable("99.0.0".into()));
        assert_eq!((count(), app.update_ready.as_deref()), (1, Some("99.0.0")));
        // the next start (the check is cached, so it says the same thing): quiet, but the sidebar still offers it
        let (mut app, _rx) = new_app();
        app.handle(Event::UpdateAvailable("99.0.0".into()));
        assert_eq!(count(), 1, "not again");
        assert_eq!(app.update_ready.as_deref(), Some("99.0.0"));
        // after rolling back from it: not offered at all
        crate::alerts::update_state(|s| s.skipped = Some("99.0.0".into()));
        let (mut app, _rx) = new_app();
        app.handle(Event::UpdateAvailable("99.0.0".into()));
        assert!(app.update_ready.is_none() && count() == 1);
        // a newer one than that: said as usual
        app.handle(Event::UpdateAvailable("99.0.1".into()));
        assert_eq!(app.update_ready.as_deref(), Some("99.0.1"));
        assert_eq!(count(), 1, "it replaces the older unread one");
        let _ = std::fs::remove_file("target/test-scratch/shell/alerts-state.json");
    }

    #[test]
    fn app_alert_jumps_back_or_says_why_not() {
        let (mut app, _rx) = new_app();
        let from = app.focused();
        app.apply(from, vec![Action::FocusPane(12345)]);
        assert_eq!(notice(&app), "that pane is closed");
        app.apply(from, vec![Action::FocusPaneOr(12345, "help")]);
        assert_eq!(app.tabs[app.cur].app, Some("help"), "falls back to the alert's app");
        // an orchestrator task tab's agent finishing out of sight: the alert remembers agents
        let (p, a) = fake(true, false);
        app.apply(from, vec![Action::OpenTagged { pane: p, tag: "agent-task:7".into(), name: "task 7".into(), focus: false }]);
        a.act.set(Some(Activity::Working));
        app.track_agents();
        a.act.set(Some(Activity::Idle));
        app.track_agents();
        let got = crate::alerts::with(|l| l.last().map(|x| (x.kind, x.app.clone())));
        assert_eq!(got, Some((crate::alerts::Kind::AgentDone, Some("agents".to_string()))));
    }

    #[test]
    fn app_idle_redraws_less() {
        let (mut app, _rx) = new_app();
        app.theme = theme::get("ultra");
        assert!(app.theme.animated);
        assert_eq!(app.next_deadline(), Duration::from_millis(125), "animated while you look");
        app.term_focused = false;
        assert!(app.next_deadline() > Duration::from_secs(1), "not while you're in another window");
        // a mouse move over nothing that lights up changes nothing on screen
        app.new_tab(Box::new(crate::panes::home::Home::new()));
        let (p, _) = fake(false, false);
        let from = app.focused();
        app.open(p, Place::SplitRight, from);
        screen(&mut app, 150, 42);
        let at = |app: &mut App, x: u16, y: u16| app.mouse(MouseEvent { kind: MouseEventKind::Moved, column: x, row: y, modifiers: KeyModifiers::NONE });
        let (_, r) = *app.outer.last().unwrap();
        at(&mut app, r.x + 4, r.y + 4);
        let before = app.look();
        at(&mut app, r.x + 5, r.y + 6);
        assert_eq!(app.look(), before);
        // onto a pane's ×: that lights up
        let (x, _) = app.pane_close[0];
        at(&mut app, x.x + 1, x.y);
        assert_ne!(app.look(), before);
    }

    #[test]
    fn app_theme_file_bursts_become_one_event() {
        let (tx, rx) = std::sync::mpsc::channel();
        let ping = debounced(tx, Duration::from_millis(40));
        for _ in 0..8 {
            ping.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(rx.recv_timeout(Duration::from_secs(2)), Ok(Event::ThemeFilesChanged)));
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "just the one");
    }

    #[test]
    fn app_local_offset_without_a_process() {
        let t = Instant::now();
        let off = os_offset_secs();
        if cfg!(windows) {
            assert!(t.elapsed() < Duration::from_millis(50), "no process spawned for it");
        }
        assert!(off.abs() <= 14 * 3600 && off % 900 == 0, "{off}");
        assert_eq!(local_offset_secs(), off);
    }
}
