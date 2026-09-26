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
    ("settings", "cog", "settings", "alt ,"),
    ("themes", "theme", "themes", ""),
    ("help", "search", "help", "F10"),
];
/// SIDEBAR entries from here on sit at the bottom of the sidebar, not in a section.
const FOOTER: usize = 10;
/// Where each sidebar section starts: (index into SIDEBAR, heading).
const SECTIONS: &[(usize, &str)] = &[(0, "ai"), (3, "tools")];

#[derive(Clone, Copy)]
enum SideHit {
    App(&'static str),
    Tab(usize),
    /// the × on one of your tabs
    CloseTab(usize),
    NewTab,
}

#[derive(Clone)]
enum Cmd {
    App(&'static str),
    Rename,
    CloseTab(usize),
    Tour,
    Open(&'static str, Place),
    Theme(String),
    /// open settings on this row (the palette's "setting: …" entries)
    Setting(String),
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
}

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
    /// The pane showing a theme it hasn't saved (settings' live preview): only while it has the keyboard.
    theme_preview: Option<PaneId>,
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
    /// renaming tab i: the text typed so far
    renaming: Option<(usize, String)>,
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
    _config_watcher: Option<notify::RecommendedWatcher>,
    /// Where the config is read and written (see edit_config); None = memory only (tests).
    cfg_path: Option<std::path::PathBuf>,
    /// config.toml changed on disk: re-read it at this point (editors write in two steps).
    config_reload_at: Option<Instant>,
    /// Reload the theme at this time: its files changed, and a burst of watcher events (one save can be two or
    /// three) settles into one reload.
    theme_reload_at: Option<Instant>,
    /// A newer version is out: shown at the bottom of the sidebar.
    update_ready: Option<String>,
    /// Is the terminal the window you're in? (desktop notifications only when it isn't)
    term_focused: bool,
    /// The memory/usage watcher's thresholds (settings › alerts changes them live).
    alert_th: std::sync::Arc<crate::alerts::Thresholds>,
}

impl App {
    /// Tests: opens on config.startup (main.rs uses with_start).
    #[cfg(test)]
    pub fn new(config: Config, tx: Sender<Event>) -> App {
        App::with_start(config, tx, None)
    }

    /// `start`: the app named on the command line (`oriel music`), opened instead of config.startup this once.
    pub fn with_start(config: Config, tx: Sender<Event>, start: Option<String>) -> App {
        let theme = theme::get(&config.theme);
        ui::NERD.store(std::env::var("ORIEL_PLAIN").is_err() && !config.plain_icons, std::sync::atomic::Ordering::Relaxed);
        let alert_th = crate::alerts::Thresholds::new(&config.alerts);
        let sidebar = config.sidebar;
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
            theme_preview: None,
            quit: false,
            start: Instant::now(),
            last_tick: HashMap::new(),
            inner: vec![],
            outer: vec![],
            side_hits: vec![],
            side_area: None,
            sidebar,
            body: Rect::default(),
            drag: None,
            sel: None,
            copy_pending: false,
            pane_close: vec![],
            hover: Position { x: u16::MAX, y: u16::MAX },
            _watcher: None,
            _theme_watcher: None,
            _config_watcher: None,
            cfg_path: if cfg!(test) { None } else { Some(config::path()) },
            config_reload_at: None,
            update_ready: None,
            theme_reload_at: None,
            term_focused: true,
            alert_th: alert_th.clone(),
            renaming: None,
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
        app._config_watcher = watch_config(tx.clone());
        if !cfg!(test) {
            *crate::alerts::CENTER.lock().unwrap() = crate::alerts::load();
            let t2 = tx.clone();
            crate::alerts::watch(alert_th, move |k, s| {
                let _ = t2.send(Event::Alert(k, s));
            });
            // once a day: is there a newer oriel? (in the background; quiet if offline; settings › tools turns it off)
            if app.config.update_check {
                std::thread::spawn(move || {
                    if let Some(r) = crate::update::check(false).ok().and_then(|rs| crate::update::available(&rs)) {
                        let _ = tx.send(Event::UpdateAvailable(r.version));
                    }
                });
            }
            if let Some((from, to, back)) = crate::update::just_updated() {
                app.notify(if back {
                    format!("{}rolled back to {to} (from {from}) · alt p → updates", ui::lead("package"))
                } else {
                    format!("{}updated {from} → {to} · alt p → updates for what's new", ui::lead("package"))
                });
            }
        }
        if !cfg!(test) && !crate::onboard::done_before() {
            app.onboard = Some(crate::onboard::Onboard::new(&app.theme.name, &app.config));
        }
        let startup = start.unwrap_or_else(|| app.config.startup.clone());
        if !cfg!(test) && startup_needs_calendar(&startup) {
            app.goto_app("calendar");
        }
        if let Some(a) = SIDEBAR.iter().find(|a| a.0 == startup) {
            app.goto_app(a.0);
        } else {
            let first = panes::open(&startup, &app.config);
            if first.is_none() {
                let why = if panes::known(&startup) { "isn't installed" } else { "isn't an app oriel has" };
                app.notify(format!("{startup} {why}: the home screen instead"));
            }
            app.new_tab(first.unwrap_or_else(|| Box::new(panes::home::Home::new())));
        }
        app
    }

    /// config.toml didn't parse at start (main.rs): say so. Settings still change for this session, but nothing
    /// is written over the file until it's fixed (it's re-read as soon as it is).
    pub fn config_error(&mut self, e: String) {
        self.notify(format!("⚠ {e} · running on defaults, and not saving over it"));
        config::set_broken(Some(e)); // nothing is written until it parses; settings shows why
    }

    fn add(&mut self, p: Box<dyn Pane>) -> PaneId {
        let id = self.next_id;
        self.next_id += 1;
        self.panes.insert(id, p);
        id
    }

    fn new_tab(&mut self, p: Box<dyn Pane>) {
        let id = self.add(p);
        self.tabs.push(Tab { app: None, name: None, root: Node::Leaf(id), focus: id, zoom: false });
        self.cur = self.tabs.len() - 1;
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
        self.tabs.insert(at, Tab { app: Some(name), name: None, root: Node::Leaf(id), focus: id, zoom: false });
        self.cur = at;
    }

    /// The help app, opened on the topic for whatever you're looking at (F10, ?, the sidebar, the palette).
    fn open_help(&mut self) {
        self.goto_app("help");
    }

    /// Replay the tour (palette "take the tour", `oriel --tour`). Straight to the tour: the setup pages are for
    /// the first run, and walking through them again would only risk resetting what you've set since.
    pub fn start_tour(&mut self) {
        let probe = self.probe();
        self.onboard = Some(crate::onboard::Onboard::tour(&self.theme.name, &probe));
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
            menu_open: self.ctx.is_some() || self.renaming.is_some(),
            in_terminal: self.panes.get(&t.focus).is_some_and(|p| p.is_terminal()),
        }
    }

    fn onboard_out(&mut self, out: crate::onboard::Out) {
        use crate::onboard::Out;
        match out {
            Out::None => {}
            Out::Theme(name, save) => self.set_theme(&name, save),
            Out::Icons(nerd) => self.set_icons(nerd),
            // the first music folder; any others you added stay
            Out::MusicFolder(p) => self.edit_config(move |c| match c.music.folders.first_mut() {
                Some(f) => *f = p,
                None => c.music.folders.push(p),
            }),
            Out::NotesFolder(p) => self.edit_config(move |c| c.notes_folder = p),
            // empty = left as it was
            Out::Defaults { startup, provider } => self.edit_config(move |c| {
                if !startup.is_empty() {
                    c.startup = startup;
                }
                if !provider.is_empty() {
                    c.ai.provider = provider;
                }
            }),
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
                // last pane in the tab: drop the tab (and stay on the tab you were on, if it was another one)
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

    fn notify(&mut self, s: impl Into<String>) {
        self.notice = Some((s.into(), Instant::now()));
    }

    /// Into the event center: a toast now, a line in alerts, and a desktop notification if you're elsewhere.
    fn raise(&mut self, kind: crate::alerts::Kind, text: String, app: Option<&str>, pane: Option<PaneId>) {
        // the thing it's about is right in front of you: no need to keep it
        let in_view = self.term_focused && pane.is_some_and(|p| self.visible().contains(&p));
        crate::alerts::push(crate::alerts::Alert { at: crate::alerts::now(), kind, text: text.clone(), app: app.map(String::from), read: in_view, pane });
        self.notify(format!("{} {text}", kind.label().0));
        if !self.term_focused && kind.loud(&self.config) {
            crate::alerts::desktop("oriel", &text);
        }
    }

    fn set_theme(&mut self, name: &str, save: bool) {
        self.theme = theme::get(name);
        if save && self.config.theme != name {
            let name = name.to_string();
            self.edit_config(move |c| c.theme = name);
        }
    }

    /// A theme settings is previewing lasts while settings has the keyboard: go to another app (or close it) and
    /// the saved theme is back, the way closing the palette puts it back. Unsaved looks never linger.
    fn end_left_preview(&mut self) {
        let Some(id) = self.theme_preview else { return };
        if self.theme.name == self.config.theme {
            self.theme_preview = None; // kept (saved), or already back
        } else if self.focused() != id {
            self.theme_preview = None;
            self.theme = theme::get(&self.config.theme);
        }
    }

    /// Nerd Font glyphs on or off, now and from the next start (palette, /icons, setup).
    fn set_icons(&mut self, nerd: bool) {
        ui::NERD.store(nerd, std::sync::atomic::Ordering::Relaxed);
        if self.config.plain_icons == nerd {
            self.edit_config(move |c| c.plain_icons = !nerd);
        }
    }

    /// The one writer of config.toml (panes reach it with `cx.edit_config`). It re-reads the file so a hand edit,
    /// another window or a pane's change since start isn't undone by a stale copy, applies `f`, writes it back
    /// and tells every pane. If the file doesn't parse, `f` still holds for this session but nothing is written.
    fn edit_config(&mut self, f: impl FnOnce(&mut Config)) {
        let (c, saved) = match &self.cfg_path {
            Some(p) => config::update_at(p, &self.config, f),
            None => {
                let mut c = self.config.clone();
                f(&mut c);
                (c, Ok(()))
            }
        };
        match saved {
            Ok(()) => config::set_broken(None),
            Err(e) => {
                self.notify(format!("⚠ not saved: {e}"));
                config::set_broken(Some(e));
            }
        }
        self.set_config(c);
    }

    /// A new config is in effect: what the app shows follows it at once (theme, icons, sidebar, the watcher's
    /// thresholds; the prefix is read on every key), and every open pane hears about it.
    fn set_config(&mut self, c: Config) {
        if c.theme != self.config.theme && self.palette.is_none() {
            self.theme = theme::get(&c.theme);
        }
        if c.plain_icons != self.config.plain_icons && std::env::var("ORIEL_PLAIN").is_err() {
            ui::NERD.store(!c.plain_icons, std::sync::atomic::Ordering::Relaxed);
        }
        if c.sidebar != self.config.sidebar {
            self.sidebar = c.sidebar;
        }
        self.alert_th.set(&c.alerts);
        self.config = c;
        for p in self.panes.values_mut() {
            p.config_changed(&self.config);
        }
    }

    /// config.toml changed on disk (a hand edit, another window, or our own save): take it in, look included.
    fn reload_config(&mut self) {
        self.config_reload_at = None;
        let Some(p) = &self.cfg_path else { return };
        match config::load_from(p) {
            Ok(c) => {
                if config::broken().is_some() {
                    config::set_broken(None);
                    self.notify("✓ config.toml reads fine again: settings loaded");
                }
                if c == self.config {
                    return; // our own save coming back
                }
                self.set_config(c);
            }
            Err(e) => {
                if config::broken().as_ref() != Some(&e) {
                    self.notify(format!("⚠ {e} · keeping the settings you had, and not saving over it"));
                }
                config::set_broken(Some(e));
            }
        }
    }

    /// The theme's files changed a moment ago (see theme_reload_at): recolour, unless the change was oriel's own.
    fn reload_theme_if_due(&mut self) {
        let Some(at) = self.theme_reload_at else { return };
        if Instant::now() < at {
            return;
        }
        self.theme_reload_at = None;
        if self.theme.name == "omarchy" {
            self.set_theme("omarchy", false);
            self.notify(format!("{}omarchy theme updated", ui::lead("theme")));
        } else if theme::is_custom(&self.theme.name) && !theme::own_write(&self.theme.name) {
            // your theme file changed in an editor: recolour (the themes app's own saves are applied already)
            let name = self.theme.name.clone();
            self.set_theme(&name, false);
            if let Some(p) = theme::problems(&name).into_iter().next() {
                self.notify(format!("⚠ {p}"));
            }
        }
    }

    // ------------------------------------------------------------------ loop
    pub fn run(&mut self, term: &mut DefaultTerminal, rx: Receiver<Event>) -> anyhow::Result<()> {
        loop {
            term.draw(|f| self.draw(f))?;
            let timeout = self.next_deadline();
            let ev = match rx.recv_timeout(timeout) {
                Ok(e) => e,
                Err(RecvTimeoutError::Timeout) => Event::Tick,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            self.handle(ev);
            // coalesce a burst (a terminal spewing output) into one redraw
            let burst = Instant::now();
            while let Ok(e) = rx.try_recv() {
                self.handle(e);
                if burst.elapsed() > Duration::from_millis(12) {
                    break;
                }
            }
            self.reap();
            self.reload_theme_if_due();
            self.track_agents();
            if self.config_reload_at.is_some_and(|t| Instant::now() >= t) {
                self.reload_config();
            }
            if self.onboard.is_some() {
                let probe = self.probe();
                let out = self.onboard.as_mut().unwrap().check(&probe);
                self.onboard_out(out);
            }
            if self.quit {
                break;
            }
        }
        Ok(())
    }

    /// Sleep until the soonest thing that needs a redraw without an event.
    fn next_deadline(&self) -> Duration {
        let mut d = Duration::from_secs(30);
        if self.theme.animated || self.onboard.as_ref().map(|o| o.animating()).unwrap_or(false) {
            d = d.min(Duration::from_millis(125));
        }
        if let Some((_, t)) = &self.notice {
            d = d.min(Duration::from_secs(4).saturating_sub(t.elapsed()) + Duration::from_millis(10));
        }
        if let Some(t) = self.config_reload_at {
            d = d.min(t.saturating_duration_since(Instant::now()));
        }
        if let Some(at) = self.theme_reload_at {
            d = d.min(at.saturating_duration_since(Instant::now()));
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

    /// herdr's sidebar states: notice agents finishing or getting stuck in tabs you aren't looking at.
    fn track_agents(&mut self) {
        let cur_leaves = {
            let mut l = vec![];
            self.tabs[self.cur].root.leaves(&mut l);
            l
        };
        let mut notes = vec![];
        for (id, p) in &self.panes {
            let Some(a) = p.activity() else { continue };
            let prev = self.agent_state.insert(*id, a);
            let seen = cur_leaves.contains(id);
            let tab_no = self.tabs.iter().position(|t| t.root.contains(*id));
            let where_ = tab_no.map(|i| self.tab_label(i)).unwrap_or_default();
            if prev == Some(Activity::Working) && a == Activity::Idle && !seen {
                self.done.insert(*id);
                notes.push((crate::alerts::Kind::AgentDone, format!("{} finished · {where_}", p.title()), *id));
            }
            if a == Activity::Blocked && prev != Some(Activity::Blocked) && !seen {
                notes.push((crate::alerts::Kind::NeedsYou, format!("{} needs you · {where_}", p.title()), *id));
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
        for (k, text, id) in notes {
            self.raise(k, text, None, Some(id));
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
        self.renaming = Some((i, cur));
        self.sidebar = true;
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
                    }
                }
                Action::SetTheme(t) => {
                    if theme::names().iter().any(|n| *n == t) {
                        self.notify(format!("{}theme: {t}", ui::lead("theme")));
                        self.set_theme(&t, true);
                    } else {
                        self.notify(format!("no theme called {t} — /theme lists them"));
                    }
                }
                Action::ApplyTheme(t) => {
                    // a nudge in the themes app re-applies the file it just wrote (the app shows its problems)
                    let switching = t != self.theme.name;
                    self.set_theme(&t, true);
                    if let Some(p) = theme::problems(&t).into_iter().next().filter(|_| switching) {
                        self.notify(format!("⚠ {p}"));
                    }
                }
                Action::PreviewTheme(t) => {
                    self.set_theme(&t, false);
                    self.theme_preview = Some(from);
                }
                Action::Config(f) => self.edit_config(f),
                Action::Palette(q) => {
                    self.open_palette();
                    if let Some(p) = &mut self.palette {
                        p.query = q;
                    }
                }
                Action::GotoApp(a) => self.goto_app(a),
                Action::AppKey(a, c) => {
                    // stay where we are: an app tab opened now sorts in ahead of your own tabs, so find ours again
                    let here = self.tabs[self.cur].focus;
                    self.goto_app(a); // opens it if it isn't yet
                    self.cur = self.tabs.iter().position(|t| t.root.contains(here)).unwrap_or(self.cur);
                    if let Some(id) = self.tabs.iter().find(|t| t.app == Some(a)).map(|t| t.focus) {
                        self.with_pane(id, |p, cx| p.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), cx));
                    }
                }
                Action::OpenTagged { pane, tag, name, focus } => {
                    let here = self.cur;
                    self.new_tab(pane);
                    let id = self.tabs[self.cur].focus;
                    self.tabs[self.cur].name = Some(name);
                    self.tags.insert(tag, id);
                    if !focus {
                        self.cur = here;
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
                Action::ToggleSidebar => self.toggle_sidebar(),
                Action::ToggleIcons => self.run_cmd(Cmd::Icons),
                Action::Tour => self.start_tour(),
                Action::Quit => self.quit = true,
            }
        }
    }

    fn handle(&mut self, ev: Event) {
        if let (Some(path), Event::Input(e)) = (std::env::var_os("ORIEL_LOG"), &ev) {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(f, "{:>8.3} {:?}", self.start.elapsed().as_secs_f64(), e);
            }
        }
        match ev {
            Event::Input(CEvent::Key(k)) if k.kind != KeyEventKind::Release => self.key(k),
            Event::Input(CEvent::Mouse(m)) => self.mouse(m),
            Event::Input(CEvent::Paste(s)) => {
                let id = self.focused();
                self.with_pane(id, |p, cx| p.paste(&s, cx));
            }
            Event::Input(CEvent::FocusGained) => self.term_focused = true,
            Event::Input(CEvent::FocusLost) => self.term_focused = false,
            Event::Input(_) => {}
            Event::Wake(id) => {
                self.with_pane(id, |p, cx| p.poll(cx));
            }
            Event::Clipboard(id, got, key) => match got {
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
            },
            Event::UpdateAvailable(v) => {
                self.raise(crate::alerts::Kind::Update, format!("oriel {v} is out: click it at the bottom of the sidebar, or alt p → updates"), Some("updates"), None);
                self.update_ready = Some(v);
            }
            Event::Alert(k, s) => {
                let app = match k {
                    crate::alerts::Kind::Memory => Some("system"),
                    crate::alerts::Kind::Usage => Some("ais"),
                    _ => None,
                };
                self.raise(k, s, app, None);
            }
            // a moment later, once the writer is done (the run loop re-reads it then)
            Event::ConfigFileChanged => self.config_reload_at = Some(Instant::now() + Duration::from_millis(150)),
            Event::ThemeFilesChanged => {
                // not now: editors write in steps and omarchy swaps several files, so wait until it's been quiet a
                // moment (was a sleep here, on the UI thread, for every event: holding an arrow in the themes app
                // queued them up)
                let quiet = Duration::from_millis(if self.theme.name == "omarchy" { 150 } else { 80 });
                self.theme_reload_at = Some(Instant::now() + quiet);
            }
            Event::Tick => {
                self.reload_theme_if_due();
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

    /// alt s, /sidebar: hide or show the sidebar, and remember it for the next start.
    fn toggle_sidebar(&mut self) {
        self.sidebar = !self.sidebar;
        let on = self.sidebar;
        if self.config.sidebar != on {
            self.edit_config(move |c| c.sidebar = on);
        }
    }

    // ------------------------------------------------------------------ keys
    fn is_prefix(&self, k: &KeyEvent) -> bool {
        // a bare key or alt+… in the config would break typing: ctrl+space then (settings shows why)
        let spec = config::prefix(&self.config);
        let (need_ctrl, key) = match spec.strip_prefix("ctrl+") {
            Some(rest) => (true, rest.to_string()),
            None => (false, spec.clone()),
        };
        if need_ctrl != k.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        match (key.as_str(), k.code) {
            ("space", KeyCode::Char(' ')) => true,
            // some terminals report ctrl+space as ctrl+@ / NUL (but ctrl+alt+@ is AltGr typing '@')
            ("space", KeyCode::Char('@')) => !k.modifiers.contains(KeyModifiers::ALT),
            (s, KeyCode::Char(c)) if s.chars().count() == 1 => s.starts_with(c.to_ascii_lowercase()),
            _ => false,
        }
    }

    fn key(&mut self, k: KeyEvent) {
        // AltGr arrives as ctrl+alt+char on Windows: make it the plain char before any binding sees it
        let k = ui::strip_altgr(k);
        self.sel = None;
        if self.onboard.is_none() && self.palette.is_none() && self.renaming.is_none() && self.ctx.is_none() && !self.prefix_armed {
            let v = matches!(k.code, KeyCode::Char('v') | KeyCode::Char('V'));
            let alt = k.modifiers.contains(KeyModifiers::ALT) && !k.modifiers.contains(KeyModifiers::CONTROL);
            let ctrl = k.modifiers.contains(KeyModifiers::CONTROL) && !k.modifiers.contains(KeyModifiers::ALT);
            if v && (alt || ctrl) {
                // paste an image: windows terminal keeps ctrl+v for text, so alt+v is the reliable one there
                let id = self.focused();
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let got = crate::clip::grab_image();
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
                    if i < self.tabs.len() {
                        self.tabs[i].name = if t.is_empty() { None } else { Some(t) };
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
                self.with_pane(id, |p, cx| p.key(k, cx));
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
        if let KeyCode::F(n) = k.code {
            if let Some(a) = SIDEBAR.iter().find(|a| a.3 == format!("F{n}")).filter(|_| k.modifiers.is_empty()) {
                self.goto_app(a.0);
                return;
            }
            if n == 12 {
                // play/pause from anywhere: opens the music app in the background if it isn't yet (like /play)
                let from = self.focused();
                self.apply(from, vec![Action::AppKey("music", ' ')]);
                return;
            }
        }
        let id = self.focused();
        let used = self.with_pane(id, |p, cx| p.key(k, cx)).unwrap_or(false);
        if !used && !self.panes.get(&id).map(|p| p.is_terminal()).unwrap_or(false) {
            // unused keys in app panes
            if let KeyCode::Char('?') = k.code {
                self.open_help();
            }
        }
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
                's' => self.toggle_sidebar(),
                'p' => self.open_palette(),
                'z' => self.run_cmd(Cmd::Zoom),
                'w' => self.run_cmd(Cmd::Close),
                't' => self.run_cmd(Cmd::NewTab),
                ',' => self.goto_app("settings"),
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
            KeyCode::Char('&') => self.run_cmd(Cmd::CloseTab(self.cur)),
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
            KeyCode::Char('q') => self.quit = true,
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
            Cmd::CloseTab(i) => {
                if i < self.tabs.len() {
                    self.close_tab(i);
                }
            }
            Cmd::Open(name, _) if panes::SINGLE.contains(&name) => self.goto_app(name),
            Cmd::Open(name, place) => match panes::open(name, &self.config) {
                Some(p) => self.open(p, place, from),
                None => self.notify(format!("{name} isn't installed")),
            },
            Cmd::Theme(t) => {
                // the toast first, so a "not saved" from set_theme is the one you see
                self.notify(format!("{}theme: {t}", ui::lead("theme")));
                self.set_theme(&t, true);
            }
            Cmd::SplitRight => self.run_cmd(Cmd::Open("terminal", Place::SplitRight)),
            Cmd::SplitDown => self.run_cmd(Cmd::Open("terminal", Place::SplitDown)),
            Cmd::Close => self.close(from),
            Cmd::Zoom => {
                let t = self.tab();
                t.zoom = !t.zoom;
            }
            Cmd::NewTab => self.new_tab(Box::new(panes::home::Home::new())),
            Cmd::Icons => {
                let n = !ui::NERD.load(std::sync::atomic::Ordering::Relaxed);
                self.notify(if n { "nerd font icons · remembered" } else { "plain icons (no nerd font) · remembered" });
                self.set_icons(n);
            }
            Cmd::Help => self.open_help(),
            Cmd::Tour => self.start_tour(),
            Cmd::Setting(id) => {
                panes::settings::jump(&id);
                self.goto_app("settings");
            }
            Cmd::Quit => self.quit = true,
        }
    }

    // ------------------------------------------------------------------ palette
    fn open_palette(&mut self) {
        let mut items: Vec<(String, Cmd)> = vec![];
        for &(name, icon, label, key) in SIDEBAR {
            items.push((format!("{}go to {label}  {key}", ui::lead(icon)), Cmd::App(name)));
        }
        // (the apps that keep one copy have their "go to" above instead)
        let extra = || APPS.iter().filter(|a| !panes::SINGLE.contains(&a.0) && available(a.0));
        for &(name, _, icon, label) in extra() {
            items.push((format!("{}split: open {label} beside this", ui::lead(icon)), Cmd::Open(name, Place::Split)));
        }
        for &(name, _, icon, label) in extra() {
            items.push((format!("{}open {label} in a new tab", ui::lead(icon)), Cmd::Open(name, Place::Tab)));
        }
        items.push((format!("{}split right", ui::lead("split")), Cmd::SplitRight));
        items.push((format!("{}split down", ui::lead("split")), Cmd::SplitDown));
        items.push((format!("{}zoom pane", ui::lead("window")), Cmd::Zoom));
        items.push((format!("{}close pane", ui::lead("close")), Cmd::Close));
        items.push((format!("{}new tab", ui::lead("tab")), Cmd::NewTab));
        items.push((format!("{}rename this tab", ui::lead("tab")), Cmd::Rename));
        items.push((format!("{}close this tab", ui::lead("close")), Cmd::CloseTab(self.cur)));
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
        items.push((format!("{}toggle nerd font icons", ui::lead("theme")), Cmd::Icons));
        items.push((format!("{}help: every key, command and how-to  F10", ui::lead("search")), Cmd::Help));
        items.push((format!("{}take the tour", ui::lead("window")), Cmd::Tour));
        // every setting, by name: opens settings on its row
        for (label, id) in panes::settings::palette_items(&self.config) {
            items.push((format!("{}setting: {label}", ui::lead("cog")), Cmd::Setting(id)));
        }
        items.push((format!("{}quit oriel", ui::lead("quit")), Cmd::Quit));
        self.palette = Some(Palette { query: String::new(), sel: 0, items, theme_before: self.theme.name.clone() });
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
            KeyCode::Esc => {
                let before = p.theme_before.clone();
                self.palette = None;
                self.set_theme(&before, false);
                return;
            }
            KeyCode::Enter => {
                let cmd = m.get(p.sel).map(|&i| p.items[i].1.clone());
                self.palette = None;
                if let Some(c) = cmd {
                    self.run_cmd(c);
                }
                return;
            }
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
        // live preview while a theme is highlighted
        let m = Self::palette_matches(p);
        let preview = match m.get(p.sel).map(|&i| &p.items[i].1) {
            Some(Cmd::Theme(t)) => t.clone(),
            _ => p.theme_before.clone(),
        };
        if preview != self.theme.name {
            self.set_theme(&preview, false);
        }
    }

    // ------------------------------------------------------------------ mouse
    fn mouse(&mut self, m: MouseEvent) {
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
            if let Some(&(_, SideHit::Tab(i))) = self.side_hits.iter().find(|(r, _)| r.contains(pos)) {
                self.cur = i;
                self.ctx_open(m.column, m.row, vec![
                    (format!("{}rename", ui::lead("tab")), Cmd::Rename),
                    (format!("{}split right", ui::lead("split")), Cmd::SplitRight),
                    (format!("{}split down", ui::lead("split")), Cmd::SplitDown),
                    (format!("{}close tab", ui::lead("close")), Cmd::CloseTab(i)),
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
                    items.push((format!("{}close pane", ui::lead("close")), Cmd::Close));
                    self.ctx_open(m.column, m.row, items);
                    return;
                }
            }
        }
        // middle-click a tab in the sidebar: close it
        if let MouseEventKind::Down(MouseButton::Middle) = m.kind {
            if let Some(&(_, SideHit::Tab(i) | SideHit::CloseTab(i))) = self.side_hits.iter().find(|(r, _)| r.contains(pos)) {
                self.close_tab(i);
                return;
            }
        }
        if let MouseEventKind::Down(MouseButton::Left) = m.kind {
            // the × on a pane's frame
            if let Some(&(_, id)) = self.pane_close.iter().find(|(r, _)| r.contains(pos)) {
                self.close(id);
                return;
            }
            if let Some(&(_, hit)) = self.side_hits.iter().find(|(r, _)| r.contains(pos)) {
                let double = self.last_click.map(|(t, x, y)| t.elapsed() < Duration::from_millis(400) && x == m.column && y == m.row).unwrap_or(false);
                self.last_click = Some((Instant::now(), m.column, m.row));
                match hit {
                    SideHit::App(a) => self.goto_app(a),
                    SideHit::Tab(i) => {
                        self.cur = i;
                        if double {
                            self.start_rename(i);
                        }
                    }
                    SideHit::CloseTab(i) => self.close_tab(i),
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
        self.end_left_preview();
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
        if let Some(p) = &self.palette {
            self.draw_palette(f, area, p, &t);
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
            if let Some((ri, text)) = self.renaming.as_ref().filter(|(ri, _)| *ri == i) {
                let _ = ri;
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
            self.side_hits.push((xr, SideHit::CloseTab(i)));
            self.side_hits.push((r, SideHit::Tab(i)));
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
            // straight under the rule, like your tabs under theirs (a tall list, like help's topics, needs the row)
            if y < inner.bottom() {
                let r = Rect { y, height: inner.bottom() - y, ..inner };
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
        let (text, key) = if self.prefix_armed {
            (" | - split · hjkl move · x close · z zoom · c tab · t theme · ? help ".to_string(), true)
        } else if let Some((n, _)) = &self.notice {
            (format!(" {n} "), false)
        } else {
            return;
        };
        let w = (unicode_width::UnicodeWidthStr::width(text.as_str()) as u16 + 10).min(area.width);
        let r = Rect { x: area.right().saturating_sub(w + 1), y: area.bottom().saturating_sub(4), width: w, height: 3 };
        f.render_widget(ratatui::widgets::Clear, r);
        let inner = ui::frame(f, r, if key { "prefix" } else { "oriel" }, None, true, t);
        f.render_widget(Paragraph::new(Span::styled(text, ui::accent(t))), inner);
    }

    fn draw_palette(&self, f: &mut Frame, area: Rect, p: &Palette, t: &Theme) {
        let m = Self::palette_matches(p);
        let inner = ui::popup(f, area, 64, 18, &format!("{}palette", ui::lead("search")), t);
        let q = Line::from(vec![Span::styled("› ", ui::bold_accent(t)), Span::raw(p.query.clone()), Span::styled("▏", ui::accent(t))]);
        f.render_widget(Paragraph::new(q), Rect { height: 1, ..inner });
        let rows = inner.height.saturating_sub(2) as usize;
        let start = p.sel.saturating_sub(rows.saturating_sub(1));
        for (row, &i) in m.iter().skip(start).take(rows).enumerate() {
            let on = start + row == p.sel;
            let style = if on { Style::default().fg(t.accent).add_modifier(Modifier::BOLD) } else { Style::default() };
            let line = Line::from(vec![Span::styled(if on { "▌ " } else { "  " }, ui::accent(t)), Span::styled(p.items[i].0.clone(), style)]);
            f.render_widget(Paragraph::new(line), Rect { y: inner.y + 2 + row as u16, height: 1, ..inner });
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

/// Seconds east of UTC, asked once from the OS.
pub(crate) fn local_offset_secs() -> i64 {
    use std::sync::OnceLock;
    static OFF: OnceLock<i64> = OnceLock::new();
    *OFF.get_or_init(|| {
        #[cfg(windows)]
        {
            let out = std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-Command", "[int][TimeZoneInfo]::Local.GetUtcOffset([DateTime]::Now).TotalSeconds"])
                .creation_flags(0x08000000)
                .output();
            out.ok().and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok()).unwrap_or(0)
        }
        #[cfg(not(windows))]
        {
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
    })
}

#[cfg(windows)]
use std::os::windows::process::CommandExt;

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
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok_and(|e| e.kind.is_modify() || e.kind.is_create()) {
            let _ = tx.send(Event::ThemeFilesChanged);
        }
    })
    .ok()?;
    w.watch(&dir, RecursiveMode::NonRecursive).ok()?;
    Some(w)
}

/// config.toml: a hand edit or another oriel window applies here live, instead of being overwritten later.
fn watch_config(tx: Sender<Event>) -> Option<notify::RecommendedWatcher> {
    use notify::{RecursiveMode, Watcher};
    if cfg!(test) {
        return None;
    }
    let dir = config::dir();
    std::fs::create_dir_all(&dir).ok()?;
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        // config.toml itself (a save lands as config.toml.tmp renamed over it), not the themes folder beside it
        if res.is_ok_and(|e| !e.kind.is_access() && e.paths.iter().any(|p| p.file_name().is_some_and(|n| n == "config.toml"))) {
            let _ = tx.send(Event::ConfigFileChanged);
        }
    })
    .ok()?;
    w.watch(&dir, RecursiveMode::NonRecursive).ok()?;
    Some(w)
}

fn watch_omarchy(tx: Sender<Event>) -> Option<notify::RecommendedWatcher> {
    use notify::{RecursiveMode, Watcher};
    let dir = theme::omarchy_watch_dir()?;
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            let _ = tx.send(Event::ThemeFilesChanged);
        }
    })
    .ok()?;
    w.watch(&dir, RecursiveMode::NonRecursive).ok()?;
    Some(w)
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
        while t0.elapsed() < Duration::from_secs(9) {
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
        // the music page counts songs in this folder, not your real one
        let music = std::path::absolute("target/test-scratch/config/onboard-flow-music").unwrap();
        let _ = std::fs::create_dir_all(&music);
        cfg.music.folders = vec![music.to_string_lossy().to_string()];
        let mut app = App::new(cfg, tx);
        app.onboard = Some(crate::onboard::Onboard::new(&app.theme.name, &app.config)); // the first run
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
        assert_eq!(app.config.startup, "agents", "the startup app you picked");
        assert_eq!(app.config.ai.provider, "", "the default AI you didn't touch stays as it was");
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
        assert!(s.contains("you're set") && s.contains("help ·") && s.contains("esc  end tour"), "{s}");
        // end it
        key(&mut app, KeyCode::Esc);
        assert!(app.onboard.is_none());
        let _ = rx;
    }

    /// "take the tour" and `oriel --tour` go straight to the tour (the setup pages would only reset settings);
    /// esc ends it unless the palette has esc, and so does the × on its card.
    #[test]
    fn app_tour_replay() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut cfg = Config::default();
        cfg.theme = "ultra".into();
        cfg.startup = "files".into();
        let mut app = App::new(cfg, tx);
        let mut term = Terminal::new(TestBackend::new(150, 42)).unwrap();
        let mut shot = |app: &mut App| -> Vec<String> {
            term.draw(|f| app.draw(f)).unwrap();
            let b = term.backend().buffer();
            (0..b.area.height).map(|y| (0..b.area.width).map(|x| b[(x, y)].symbol()).collect::<String>()).collect()
        };
        app.run_cmd(Cmd::Tour);
        let s = shot(&mut app).join("\n");
        assert!(s.contains("tour · 1/") && s.contains("switch apps") && !s.contains("pick a look"), "{s}");
        // esc with the palette open closes the palette, not the tour
        app.open_palette();
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.palette.is_none() && app.onboard.is_some());
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.onboard.is_none(), "esc ends the tour");
        assert_eq!(app.config.startup, "files", "nothing was reset");
        // the × on the card
        app.start_tour();
        let lines = shot(&mut app);
        let (row, line) = lines.iter().enumerate().find(|(_, l)| l.contains("tour · 1/")).unwrap();
        let col = line[..line.rfind('×').unwrap()].chars().count() as u16;
        app.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: col, row: row as u16, modifiers: KeyModifiers::NONE });
        assert!(app.onboard.is_none(), "× ends the tour");
    }

    fn scratch_config(name: &str, text: &str) -> std::path::PathBuf {
        let d = std::path::absolute(format!("target/test-scratch/config/{name}")).unwrap();
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("config.toml");
        std::fs::write(&p, text).unwrap();
        p
    }

    /// The app writes config.toml by re-reading it and changing one field: a theme pick can't undo what a chat
    /// saved (/key, /perms), and `oriel music` is never saved as the startup app.
    #[test]
    fn app_config_single_writer() {
        let p = scratch_config("app-writer", "theme = \"oriel\"\n");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut cfg = Config::default();
        cfg.theme = "oriel".into();
        let mut app = App::with_start(cfg, tx, Some("music".into()));
        app.cfg_path = Some(p.clone());
        assert_eq!(app.tabs[app.cur].app, Some("music"), "oriel music opens music");
        // a chat saves a key and a mode (through the app, as chats now do)
        let chat = app.focused();
        app.apply(chat, vec![Action::Config(Box::new(|c| c.ai.anthropic_key = "sk-test".into())), Action::Config(Box::new(|c| c.ai.perms = "bypass".into()))]);
        assert_eq!(app.config.ai.perms, "bypass", "new panes are built from this");
        // meanwhile another window (or a hand edit) adds a roster
        let mut disk = config::load_from(&p).unwrap();
        disk.roster.push(crate::config::RosterEntry { name: "kimi".into(), agent: "kimi".into(), ..Default::default() });
        config::save_to(&p, &disk).unwrap();
        // then a theme pick from the palette
        app.run_cmd(Cmd::Theme("ocean".into()));
        let disk = config::load_from(&p).unwrap();
        assert_eq!(disk.theme, "ocean");
        assert_eq!((disk.ai.anthropic_key.as_str(), disk.ai.perms.as_str()), ("sk-test", "bypass"), "the chat's settings survive");
        assert_eq!(disk.roster.len(), 1, "the other window's roster survives");
        assert_eq!(disk.startup, "ai", "`oriel music` wasn't saved as the startup app");
        assert_eq!(app.config, disk, "the app's copy is what's on disk");
        // the icons toggle is saved too (the config says plain, the screen shows glyphs: make it glyphs)
        config::save_to(&p, &Config { plain_icons: true, ..disk }).unwrap();
        app.config.plain_icons = true;
        app.set_icons(true);
        assert!(!config::load_from(&p).unwrap().plain_icons, "remembered");
        app.open_palette();
        let item = app.palette.as_ref().unwrap().items.iter().find(|i| i.0.contains("toggle nerd font icons")).unwrap().0.clone();
        assert!(item.starts_with(&ui::lead("theme")), "{item}");
    }

    /// config.toml edited by hand: applied live; a typo is reported with its line and nothing is written over it.
    #[test]
    fn app_config_hand_edits() {
        let p = scratch_config("app-hand", "theme = \"oriel\"\n");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut cfg = Config::default();
        cfg.theme = "oriel".into();
        let mut app = App::new(cfg, tx);
        app.cfg_path = Some(p.clone());
        std::fs::write(&p, "theme = \"dracula\"\n[ai]\nperms = \"plan\"\n").unwrap();
        app.handle(Event::ConfigFileChanged);
        assert!(app.config_reload_at.is_some(), "re-read a moment later, once the editor is done");
        app.reload_config();
        assert_eq!((app.theme.name.as_str(), app.config.ai.perms.as_str()), ("dracula", "plan"));
        // a typo: keep what we have, say where it is, don't write over it
        let broken = "theme = \"dracula\"\nprefix = ctrl+b\n[ai]\nperms = \"plan\"\n";
        std::fs::write(&p, broken).unwrap();
        app.reload_config();
        assert!(config::broken().as_deref().is_some_and(|e| e.contains("line 2")));
        assert!(app.notice.as_ref().unwrap().0.contains("line 2"));
        app.run_cmd(Cmd::Theme("ocean".into()));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), broken, "never written over");
        assert!(app.notice.as_ref().unwrap().0.contains("not saved"));
        assert_eq!(app.theme.name, "ocean", "it still applies for now");
        // fixed: picked up, and saving works again
        std::fs::write(&p, "theme = \"dracula\"\nprefix = \"ctrl+b\"\n").unwrap();
        app.reload_config();
        assert!(config::broken().is_none() && app.config.prefix == "ctrl+b");
        assert!(app.notice.as_ref().unwrap().0.contains("reads fine again"));
    }

    /// Settings: alt , opens it, the palette has a row per setting that opens on it, a change applies live (the
    /// sidebar, the prefix), alt s is remembered, and a theme preview isn't saved.
    #[test]
    fn app_settings_app() {
        let p = scratch_config("app-settings", "theme = \"oriel\"\n");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(Config { theme: "oriel".into(), ..Config::default() }, tx);
        app.cfg_path = Some(p.clone());
        let mut term = Terminal::new(TestBackend::new(160, 44)).unwrap();
        app.key(KeyEvent::new(KeyCode::Char(','), KeyModifiers::ALT));
        assert_eq!(app.tabs[app.cur].app, Some("settings"));
        term.draw(|f| app.draw(f)).unwrap();
        crate::testkit::save_html(term.backend().buffer(), "target/snap/app-settings.html");
        let b = term.backend().buffer();
        let s: String = (0..b.area.height).map(|y| (0..b.area.width).map(|x| b[(x, y)].symbol()).collect::<String>() + "\n").collect();
        assert!(s.contains("settings") && s.contains("alt ,") && s.contains("prefix key") && s.contains("providers & keys"), "{s}");
        // the palette: one row per setting, opening on it
        app.goto_app("files");
        app.open_palette();
        let item = app.palette.as_ref().unwrap().items.iter().find(|i| i.0.contains("setting: permissions · edits")).cloned().expect("a palette row per setting");
        app.palette = None;
        app.run_cmd(item.1);
        assert_eq!(app.tabs[app.cur].app, Some("settings"));
        term.draw(|f| app.draw(f)).unwrap();
        let settings = app.focused();
        assert!(app.panes[&settings].title().contains("AI chat"));
        // a change from settings applies at once, and only it is saved
        app.apply(settings, vec![Action::Config(Box::new(|c| c.prefix = "ctrl+b".into())), Action::Config(Box::new(|c| c.sidebar = false))]);
        assert!(!app.sidebar, "the sidebar follows at once");
        assert!(app.is_prefix(&KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)), "so does the prefix");
        assert!(!app.is_prefix(&KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL)));
        assert_eq!(config::load_from(&p).unwrap().prefix, "ctrl+b");
        // alt s is remembered
        app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT));
        assert!(app.sidebar && config::load_from(&p).unwrap().sidebar);
        // a preview shows a theme without saving it
        app.apply(settings, vec![Action::PreviewTheme("ocean".into())]);
        assert_eq!(app.theme.name, "ocean");
        assert_eq!(config::load_from(&p).unwrap().theme, "oriel");
        term.draw(|f| app.draw(f)).unwrap();
        assert_eq!(app.theme.name, "ocean", "still previewing while settings has the keyboard");
        // ... and going to another app mid-preview puts the saved one back (nothing unsaved lingers)
        app.goto_app("files");
        term.draw(|f| app.draw(f)).unwrap();
        assert_eq!((app.theme.name.as_str(), app.theme_preview), ("oriel", None));
        // a bare-key prefix from a hand edit can't eat every b typed: ctrl+space then
        app.config.prefix = "b".into();
        assert!(!app.is_prefix(&KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)));
        assert!(app.is_prefix(&KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL)));
    }

    /// Focus changes reach the app (they used to fall into the catch-all input arm), so desktop notifications know
    /// when the terminal isn't the window you're in.
    #[test]
    fn app_tracks_terminal_focus() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(Config { theme: "oriel".into(), ..Config::default() }, tx);
        app.handle(Event::Input(CEvent::FocusLost));
        assert!(!app.term_focused);
        app.handle(Event::Input(CEvent::FocusGained));
        assert!(app.term_focused);
    }

    /// Setup's music page changes the first folder and keeps the rest; an unknown app on the command line or in
    /// the config says so instead of quietly showing the home screen.
    #[test]
    fn app_setup_folders_and_unknown_startup() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut cfg = Config::default();
        cfg.theme = "oriel".into();
        cfg.music.folders = vec!["A".into(), "B".into()];
        let mut app = App::with_start(cfg, tx, Some("claud".into()));
        assert!(app.notice.as_ref().unwrap().0.contains("claud isn't an app oriel has"));
        app.onboard_out(crate::onboard::Out::MusicFolder("C".into()));
        assert_eq!(app.config.music.folders, vec!["C".to_string(), "B".to_string()]);
        app.config.music.folders.clear();
        app.onboard_out(crate::onboard::Out::MusicFolder("D".into()));
        assert_eq!(app.config.music.folders, vec!["D".to_string()]);
        assert!(panes::known("music") && panes::known("claude") && panes::known("home"));
        assert!(!panes::known("claud") && !panes::known("-v"));
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

    /// F12 works before music has been opened: it opens it in the background (and doesn't switch to it, even from
    /// a tab of your own that the new app tab sorts in ahead of).
    #[test]
    fn app_f12_opens_music_in_the_background() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(Config::default(), tx);
        app.new_tab(Box::new(crate::panes::home::Home::new()));
        let mine = app.focused();
        assert!(!app.tabs.iter().any(|t| t.app == Some("music")));
        // (never drawn after this, so the library never loads and nothing can start playing)
        app.key(KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE));
        assert!(app.tabs.iter().any(|t| t.app == Some("music")), "F12 opened music");
        assert_eq!(app.focused(), mine, "and left you where you were");
    }

    #[test]
    fn app_altgr_at_is_not_the_prefix() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(Config::default(), tx);
        let altgr = KeyEvent::new(KeyCode::Char('@'), KeyModifiers::CONTROL | KeyModifiers::ALT);
        assert!(!app.is_prefix(&altgr), "AltGr+2 types '@' on a German layout");
        assert!(app.is_prefix(&KeyEvent::new(KeyCode::Char('@'), KeyModifiers::CONTROL)), "ctrl+@ is still ctrl+space");
        app.key(altgr);
        assert!(!app.prefix_armed);
    }

    #[test]
    fn app_theme_saves_dont_stall_or_reload() {
        let d = std::path::absolute("target/test-scratch/newer/app-theme").unwrap();
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        theme::TEST_DIR.with(|t| *t.borrow_mut() = Some(d.clone()));
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut cfg = Config::default();
        cfg.theme = "ultra".into();
        let mut app = App::new(cfg, tx);
        // what the themes app does on each nudge: save the file, then apply it
        theme::save_custom("mine", "ultra", &theme::get("ultra")).unwrap();
        let id = app.focused();
        app.apply(id, vec![Action::ApplyTheme("mine".into())]);
        assert_eq!((app.theme.name.as_str(), app.config.theme.as_str()), ("mine", "mine"));
        // the watcher's burst of events: no sleep on the UI thread, one reload once it's quiet
        let t0 = Instant::now();
        for _ in 0..5 {
            app.handle(Event::ThemeFilesChanged);
        }
        assert!(t0.elapsed() < Duration::from_millis(50), "{:?}", t0.elapsed());
        assert!(app.theme_reload_at.is_some() && app.next_deadline() <= Duration::from_millis(80));
        // it was oriel's own save: nothing to reload (a marker on the live theme survives)
        app.theme.accent = ratatui::style::Color::Rgb(1, 2, 3);
        std::thread::sleep(Duration::from_millis(90));
        app.handle(Event::Tick);
        assert!(app.theme_reload_at.is_none());
        assert_eq!(app.theme.accent, ratatui::style::Color::Rgb(1, 2, 3), "no reload for its own write");
        // a save from an editor does recolour
        std::fs::write(d.join("mine.toml"), "base = \"ultra\"\naccent = \"#102030\"\n").unwrap();
        app.handle(Event::ThemeFilesChanged);
        std::thread::sleep(Duration::from_millis(90));
        app.handle(Event::Tick);
        assert_eq!(app.theme.accent, ratatui::style::Color::Rgb(0x10, 0x20, 0x30));
        theme::TEST_DIR.with(|t| *t.borrow_mut() = None);
    }

    #[test]
    fn app_launcher_goes_to_app_tabs() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut cfg = Config::default();
        cfg.theme = "oriel".into();
        let mut app = App::new(cfg, tx);
        // the F4 tab, with a stand-in pane: a real music player would scan the music folder on this machine
        let id = app.add(Box::new(crate::panes::themes::Themes::new()));
        app.tabs.push(Tab { app: Some("music"), name: None, root: Node::Leaf(id), focus: id, zoom: false });
        let music_tabs = |app: &App| app.tabs.iter().filter(|t| t.app == Some("music")).count();
        // m on a new tab's launcher, twice: the F4 tab both times, and the launcher steps aside
        for _ in 0..2 {
            app.new_tab(Box::new(crate::panes::home::Home::new()));
            let mine = app.user_tabs().len();
            app.key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
            assert_eq!(app.tabs[app.cur].app, Some("music"));
            assert_eq!(app.user_tabs().len(), mine - 1, "the launcher's tab closed");
        }
        assert_eq!(music_tabs(&app), 1, "one music player");
        // the palette and ctrl+space m go to it too
        app.run_cmd(Cmd::Open("music", Place::Split));
        app.prefix_cmd(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        assert_eq!(music_tabs(&app), 1);
        app.open_palette();
        let items: Vec<String> = app.palette.as_ref().unwrap().items.iter().map(|i| i.0.clone()).collect();
        app.palette = None;
        assert!(!items.iter().any(|i| i.contains("open music")) && items.iter().any(|i| i.contains("go to music")));
        // closing a tab before the one you're on keeps you on yours
        app.new_tab(Box::new(crate::panes::home::Home::new()));
        let first = app.cur;
        app.new_tab(Box::new(crate::panes::home::Home::new()));
        let mine = app.focused();
        app.close_tab(first);
        assert_eq!(app.focused(), mine);
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
}
