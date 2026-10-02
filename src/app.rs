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
    ("settings", "cog", "settings", "alt ,"),
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
    /// "+N more": the tab list scrolls on
    MoreTabs,
}

#[derive(Clone)]
enum Cmd {
    App(&'static str),
    /// one of your tabs, by id
    Tab(u64),
    Rename,
    CloseTab(u64),
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
    NextTab,
    PrevTab,
    /// back to the tab you were on before
    Back,
    /// your current tab one place up (-1) or down (1) the list
    MoveTab(i32),
    Sidebar,
    /// alt j / alt 0
    Jump,
    NextAgent,
    Icons,
    Config,
    Help,
    Quit,
    /// the clipboard's text into the focused pane (the right-click menu)
    Paste,
    /// a selection: to the clipboard, or into chat as a code block
    Copy(String),
    AskChat(String),
    /// text for chat's box (the palette's "send this note to chat")
    ToChat(String),
}

/// A palette row: icon, what it does, its key (right-aligned, muted), and its group (headings when the query is
/// empty).
struct Item {
    icon: &'static str,
    label: String,
    key: String,
    cmd: Cmd,
    group: &'static str,
}

/// A toast: what kind of alert it is (None = a plain notice), the text, when, and the pane a click on it goes to.
struct Notice {
    kind: Option<crate::alerts::Kind>,
    text: String,
    at: Instant,
    pane: Option<PaneId>,
}

/// How long a toast stays up, and how many stack at once.
const TOAST_FOR: Duration = Duration::from_secs(4);
const TOASTS: usize = 3;

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
    items: Vec<Item>,
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

/// oriel's window has the focus (the terminal reports it): panes check it to tell you about things while you're
/// in another window. Tests get one per thread, so parallel tests that flip it never see each other's.
#[cfg(not(test))]
static TERM_FOCUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
#[cfg(test)]
thread_local! {
    static TERM_FOCUSED: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

pub fn term_focused() -> bool {
    #[cfg(not(test))]
    return TERM_FOCUSED.load(std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    return TERM_FOCUSED.with(|f| f.get());
}

pub(crate) fn set_term_focused(on: bool) {
    #[cfg(not(test))]
    TERM_FOCUSED.store(on, std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    TERM_FOCUSED.with(|f| f.set(on));
}

pub struct App {
    panes: HashMap<PaneId, Box<dyn Pane>>,
    tabs: Vec<Tab>,
    cur: usize,
    next_id: PaneId,
    theme: Theme,
    config: Config,
    tx: Sender<Event>,
    /// toasts, newest last (at most TOASTS), and where each was drawn (by its `at`, which a toast arriving since
    /// can't shift the way it shifts indices; a click goes to its pane)
    notices: Vec<Notice>,
    toast_hits: Vec<(Rect, Instant)>,
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
    /// its text as last drawn (for the right-click menu's copy / ask chat)
    sel_text: String,
    /// copy the selection after the next draw (the text comes from the drawn frame)
    copy_pending: bool,
    /// the tab you were on before this one (by id; alt 0, prefix ;, an app's own F-key again), and the one
    /// seen last, to notice a change
    prev_tab: Option<u64>,
    seen_tab: u64,
    /// your tabs in the sidebar scroll: the first shown, the tab it last scrolled to show, and their rect (wheel)
    tab_scroll: usize,
    tab_scrolled_for: u64,
    tabs_rect: Rect,
    /// the window title as last set ("oriel · 1 needs you")
    title: String,
    /// the session as last written, and when it was checked
    saved: String,
    saved_at: Instant,
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
    /// the focused frame's "← 2 need you" as last drawn (a click opens the alerts app)
    need_hit: Option<Rect>,
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
            notices: vec![],
            toast_hits: vec![],
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
            sel_text: String::new(),
            copy_pending: false,
            prev_tab: None,
            seen_tab: 0,
            tab_scroll: 0,
            tab_scrolled_for: 0,
            tabs_rect: Rect::default(),
            title: String::new(),
            saved: String::new(),
            saved_at: Instant::now(),
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
            need_hit: None,
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
        app._config_watcher = watch_config(tx.clone());
        if !cfg!(test) {
            crate::alerts::open();
            let t2 = tx.clone();
            crate::alerts::watch(alert_th, move |k, s| {
                let _ = t2.send(Event::Alert(k, s));
            });
            // keep the search index (and so the archive of every AI session) current in the background
            crate::recall::start_background();
            // once a day: is there a newer oriel? (in the background; quiet if offline; settings › tools turns it off)
            if app.config.update_check {
                std::thread::spawn(move || {
                    if let Some(r) = crate::update::check(false).ok().and_then(|rs| crate::update::available(&rs)) {
                        let _ = tx.send(Event::UpdateAvailable(r.version));
                    }
                });
            }
            if let Some((from, to, back)) = crate::update::just_updated() {
                if back || crate::update::newer(&from, &to) {
                    // a rollback: the version you left stays quiet until a newer one is out
                    crate::alerts::update_state(|s| s.skipped = Some(from.clone()));
                    app.notify(format!("{}rolled back to {to} (from {from}) · {from} won't be offered again", ui::lead("package")));
                } else {
                    app.notify(format!("{}updated {from} → {to} · alt p → updates for what's new", ui::lead("package")));
                }
            }
        }
        if !cfg!(test) && !crate::onboard::done_before() {
            app.onboard = Some(crate::onboard::Onboard::new(&app.theme.name, &app.config));
        }
        let startup = start.unwrap_or_else(|| app.config.startup.clone());
        if !cfg!(test) && startup_needs_calendar(&startup) {
            app.goto_app("calendar");
        }
        if startup == "last" {
            // where you left off; the first time (or with nothing to bring back), chat
            let restored = crate::session::load().is_some_and(|s| app.restore(s));
            if !restored {
                app.goto_app("ai");
            }
        } else if let Some(a) = SIDEBAR.iter().find(|a| a.0 == startup) {
            app.goto_app(a.0);
        } else if startup == "search" {
            app.goto_app("search"); // `oriel search`: the same tab alt r goes to, not a second one
        } else {
            let first = panes::open(&startup, &app.config);
            if first.is_none() {
                let why = if panes::known(&startup) { "isn't installed" } else { "isn't an app oriel has" };
                app.notify(format!("{startup} {why}: the home screen instead"));
            }
            app.new_tab(first.unwrap_or_else(|| Box::new(panes::home::Home::new())));
        }
        app.seen_tab = app.tabs[app.cur].id;
        app
    }

    /// config.toml didn't parse at start (main.rs): say so. Settings still change for this session, but nothing
    /// is written over the file until it's fixed (it's re-read as soon as it is).
    pub fn config_error(&mut self, e: String) {
        self.notify(format!("⚠ {e} · running on defaults, and not saving over it"));
        config::set_broken(Some(e)); // nothing is written until it parses; settings shows why
    }

    // ------------------------------------------------------------------ where you left off
    /// What to bring back next time: your tabs (each pane as what to reopen and where), the tab or app you're in,
    /// the sidebar, and the chat and note open in their apps.
    fn session(&self) -> crate::session::Session {
        use crate::session::{Session, Tab as STab};
        let resume = |app: &str| self.tabs.iter().find(|t| t.app == Some(app)).and_then(|t| self.panes.get(&t.focus)).and_then(|p| p.resume_id());
        let mut s = Session { app: self.tabs[self.cur].app.map(String::from), sidebar: self.sidebar, chat: resume("ai"), note: resume("notes"), ..Default::default() };
        for i in self.user_tabs() {
            let t = &self.tabs[i];
            let mut kept = vec![];
            // a tab with nothing to bring back (an orchestrator's worker, an install) is left out
            let Some(root) = self.save_node(&t.root, &mut kept) else { continue };
            if i == self.cur {
                s.tab = Some(s.tabs.len());
            }
            s.tabs.push(STab { name: t.name.clone(), root, focus: kept.iter().position(|&id| id == t.focus).unwrap_or(0) });
        }
        s
    }

    /// A split tree as saved: panes that can't come back drop out (their sibling takes their place).
    fn save_node(&self, n: &Node, kept: &mut Vec<PaneId>) -> Option<crate::session::Node> {
        use crate::session::Node as SNode;
        match n {
            Node::Leaf(id) => {
                let p = self.panes.get(id)?;
                let kind = p.reopen().filter(|_| !self.tags.values().any(|t| t == id))?;
                kept.push(*id);
                Some(SNode::Leaf { kind: kind.to_string(), cwd: p.cwd().map(|d| d.display().to_string()), id: p.resume_id() })
            }
            Node::Split { dir, ratio, a, b } => match (self.save_node(a, kept), self.save_node(b, kept)) {
                (Some(a), Some(b)) => Some(SNode::Split { right: *dir == Dir::Right, ratio: *ratio, a: Box::new(a), b: Box::new(b) }),
                (one, other) => one.or(other),
            },
        }
    }

    /// Reopen a saved tree's panes: terminals get a fresh shell in their folder, claude/codex start again there,
    /// a chat reopens its chat. Panes that can't open (an agent that's been uninstalled) drop out.
    fn load_node(&mut self, n: &crate::session::Node, opened: &mut Vec<PaneId>) -> Option<Node> {
        use crate::session::Node as SNode;
        match n {
            SNode::Leaf { kind, cwd, id } => {
                let mut p = panes::open_in(kind, &self.config, cwd.as_ref().map(std::path::PathBuf::from))?;
                if let Some(id) = id {
                    p.resume(id);
                }
                let pid = self.add(p);
                opened.push(pid);
                Some(Node::Leaf(pid))
            }
            SNode::Split { right, ratio, a, b } => match (self.load_node(a, opened), self.load_node(b, opened)) {
                (Some(a), Some(b)) => Some(Node::Split { dir: if *right { Dir::Right } else { Dir::Down }, ratio: ratio.clamp(0.1, 0.9), a: Box::new(a), b: Box::new(b) }),
                (one, other) => one.or(other),
            },
        }
    }

    /// Bring a saved session back. False if there was nothing to put you in (a tab opened for other reasons, like
    /// the calendar for a coming reminder, doesn't count).
    fn restore(&mut self, s: crate::session::Session) -> bool {
        self.sidebar = s.sidebar;
        let mut placed = false;
        // the chat and note you had open, in their apps
        for (app, id) in [("ai", &s.chat), ("notes", &s.note)] {
            if let Some(id) = id {
                self.goto_app(app);
                let f = self.focused();
                if let Some(p) = self.panes.get_mut(&f) {
                    p.resume(id);
                }
                placed = true;
            }
        }
        let mut mine = vec![];
        for (n, t) in s.tabs.iter().enumerate() {
            let mut opened = vec![];
            let Some(root) = self.load_node(&t.root, &mut opened) else { continue };
            let focus = opened.get(t.focus).copied().unwrap_or(opened[0]);
            let tid = self.tab_id();
            self.tabs.push(Tab { id: tid, app: None, name: t.name.clone(), root, focus, zoom: false });
            mine.push((n, self.tabs.len() - 1));
        }
        // the app or tab you were in
        let app = s.app.as_deref().and_then(|a| SIDEBAR.iter().map(|x| x.0).chain(["updates"]).find(|x| *x == a));
        if let Some(a) = app {
            self.goto_app(a);
        } else if let Some(&(_, i)) = mine.iter().find(|(n, _)| Some(*n) == s.tab).or(mine.first()) {
            self.cur = i;
        } else {
            return placed;
        }
        true
    }

    /// Write the session if it changed (checked every couple of seconds while oriel runs, and at quit).
    fn autosave(&mut self, now: bool) {
        if !now && self.saved_at.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.saved_at = Instant::now();
        let Ok(json) = serde_json::to_string_pretty(&self.session()) else { return };
        if json != self.saved {
            crate::session::save(&json);
            self.saved = json;
        }
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

    /// Replay the tour (palette "take the tour", `oriel --tour`). Straight to the tour: the setup pages are for
    /// the first run, and walking through them again would only risk resetting what you've set since.
    pub fn start_tour(&mut self) {
        let probe = self.probe();
        self.onboard = Some(crate::onboard::Onboard::tour(&self.theme.name, &probe, &config::prefix(&self.config)));
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
            if let Some(took) = t.root.remove(id) {
                // focus goes to what took its place, not wherever the tree starts
                if t.focus == id {
                    t.focus = took;
                }
                t.zoom = false;
            } else {
                // last pane in the tab: drop the tab (one before yours going mustn't move you to another; yours
                // going takes you back to the tab you were on before it)
                let back = if ti == self.cur { self.prev_tab.filter(|&p| p != self.tabs[ti].id) } else { None };
                self.tabs.remove(ti);
                if ti < self.cur {
                    self.cur -= 1;
                }
                if self.tabs.is_empty() {
                    self.goto_app("ai");
                }
                self.cur = back.and_then(|b| self.tab_index(b)).unwrap_or(self.cur).min(self.tabs.len() - 1);
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
        self.toast(None, s.into(), None);
    }

    /// Put up a toast: newest on top of the stack, the same text again moves up instead of stacking twice.
    fn toast(&mut self, kind: Option<crate::alerts::Kind>, text: String, pane: Option<PaneId>) {
        self.notices.retain(|n| n.text != text);
        self.notices.push(Notice { kind, text, at: Instant::now(), pane });
        let n = self.notices.len();
        self.notices.drain(..n.saturating_sub(TOASTS));
    }

    /// The newest toast's text ("" = none).
    #[cfg(test)]
    fn last_notice(&self) -> String {
        self.notices.last().map(|n| n.text.clone()).unwrap_or_default()
    }

    /// Into the event center: a toast now, a line in alerts, and a desktop notification if you're elsewhere.
    fn raise(&mut self, kind: crate::alerts::Kind, text: String, app: Option<&str>, pane: Option<PaneId>) {
        use crate::alerts::Kind;
        // the thing it's about is right in front of you: no need to keep it
        let in_view = self.term_focused && pane.is_some_and(|p| self.visible().contains(&p));
        crate::alerts::push(crate::alerts::Alert { at: crate::alerts::now(), kind, text: text.clone(), app: app.map(String::from), read: in_view, pane });
        // something waiting on you says how to get there (a click on the toast does too)
        let hint = if matches!(kind, Kind::NeedsYou | Kind::Approval) && !in_view { " · alt j" } else { "" };
        self.toast(Some(kind), format!("{text}{hint}"), pane);
        if !self.term_focused && kind.loud(&self.config) {
            crate::alerts::desktop("oriel", &text);
        }
    }

    // ------------------------------------------------------------------ getting to what needs you
    /// After anything that can switch tabs: remember the one you left, for "back".
    fn note_tab(&mut self) {
        let Some(t) = self.tabs.get(self.cur) else { return };
        if t.id != self.seen_tab {
            if self.tab_index(self.seen_tab).is_some() {
                self.prev_tab = Some(self.seen_tab);
            }
            self.seen_tab = t.id;
        }
    }

    /// Back to the tab you were on before (prefix ;, alt 0 with nothing waiting, an app's own F-key again).
    fn go_back(&mut self) -> bool {
        match self.prev_tab.and_then(|t| self.tab_index(t)) {
            Some(i) => {
                self.cur = i;
                true
            }
            None => false,
        }
    }

    /// The next tab after yours (round the list, not yours) whose dot is `want`, and the pane in it that has it.
    fn next_dot(&self, want: Dot) -> Option<(usize, PaneId)> {
        let n = self.tabs.len();
        (1..n).map(|k| (self.cur + k) % n).find_map(|i| {
            if self.tab_dot(i) != want {
                return None;
            }
            let mut l = vec![];
            self.tabs[i].root.leaves(&mut l);
            let pane = l.iter().copied().find(|id| self.pane_dot(*id) == want).unwrap_or(self.tabs[i].focus);
            Some((i, pane))
        })
    }

    fn focus_pane(&mut self, i: usize, pane: PaneId) {
        self.cur = i;
        let t = &mut self.tabs[i];
        t.focus = pane;
        t.zoom = false;
        // you're there now: its alerts are read
        let mut l = vec![];
        t.root.leaves(&mut l);
        crate::alerts::mark_read_if(|a| a.pane.is_some_and(|p| l.contains(&p)));
    }

    /// Everything waiting on you right now, gathered from the panes' own state before every draw: the alerts
    /// app's "open now" list and the focused frame's "← 2 need you". A pane at a red dot that lists nothing
    /// of its own (a coding agent in a terminal, found by its screen) gets a plain row.
    fn gather_open(&mut self) {
        use crate::alerts::{Kind, Open};
        let tag_of: HashMap<PaneId, &String> = self.tags.iter().map(|(t, id)| (*id, t)).collect();
        // each row with the tag of the tab it came from, if that tab was opened tagged
        let mut rows: Vec<(Open, Option<String>)> = vec![];
        for (i, tab) in self.tabs.iter().enumerate() {
            let mut l = vec![];
            tab.root.leaves(&mut l);
            for id in l {
                let Some(p) = self.panes.get(&id) else { continue };
                let mut mine = p.open_now();
                if mine.is_empty() && p.activity() == Some(Activity::Blocked) {
                    mine.push(Open::new(Kind::NeedsYou, "blocked", format!("{} is waiting for you", p.title())));
                }
                for mut o in mine {
                    o.pane = id;
                    o.from = self.tab_label(i);
                    o.app = tab.app;
                    rows.push((o, tag_of.get(&id).map(|t| t.to_string())));
                }
            }
        }
        // a task's terminal tab and the agents app would say the same thing twice: keep the agents' row (it knows
        // the task), with what the terminal's screen shows added to its peek
        let claimed: Vec<String> = rows.iter().filter_map(|(o, _)| o.tag.clone()).collect();
        let mut screens: HashMap<String, Vec<String>> = HashMap::new();
        let mut out = vec![];
        for (o, own) in rows {
            match own {
                Some(t) if o.tag.is_none() && claimed.contains(&t) => {
                    screens.insert(t, o.detail);
                }
                _ => out.push(o),
            }
        }
        for o in &mut out {
            if let Some(d) = o.tag.as_ref().and_then(|t| screens.remove(t)).filter(|d| !d.is_empty()) {
                o.detail.push(String::new());
                o.detail.extend(d);
            }
        }
        crate::alerts::set_open(out);
    }

    /// How many open items need you in panes you can't see right now.
    fn need_elsewhere(&self) -> usize {
        let visible = self.visible();
        crate::alerts::with_open(|l| l.iter().filter(|o| o.needs_you() && !visible.contains(&o.pane)).count())
    }

    /// alt j: to whatever needs you. The newest unread "needs you" / "asking you" alert whose pane is still open,
    /// else the first open item waiting on you out of sight (a chat's question), else the next tab with a red
    /// dot, then one with a green dot. Each is marked read, so pressing it again goes on to the next.
    fn jump(&mut self) {
        use crate::alerts::Kind;
        let hit = crate::alerts::with(|l| {
            l.iter().rev().filter(|a| !a.read && matches!(a.kind, Kind::NeedsYou | Kind::Approval)).find_map(|a| a.pane.filter(|p| self.panes.contains_key(p)))
        });
        self.gather_open();
        let visible = self.visible();
        let open = crate::alerts::with_open(|l| l.iter().find(|o| o.needs_you() && !visible.contains(&o.pane)).map(|o| o.pane));
        let to = hit
            .or(open)
            .and_then(|p| self.tabs.iter().position(|t| t.root.contains(p)).map(|i| (i, p)))
            .or_else(|| self.next_dot(Dot::Blocked))
            .or_else(|| self.next_dot(Dot::Done));
        match to {
            Some((i, p)) => self.focus_pane(i, p),
            None => self.notify("nothing else needs you right now"),
        }
    }

    /// alt 0: the next tab whose dot says it needs you, then one that finished while you were away (round the
    /// list); with none, back to the tab you were on.
    fn next_agent(&mut self) {
        match self.next_dot(Dot::Blocked).or_else(|| self.next_dot(Dot::Done)) {
            Some((i, p)) => self.focus_pane(i, p),
            None => {
                if !self.go_back() {
                    self.notify("no agent needs you · no tab to go back to");
                }
            }
        }
    }

    /// The window title: how many agents need you and how many are working, so a glance at the terminal's tab
    /// from another window says whether to come back. A chat's question counts too (the open-now list, gathered
    /// fresh: the title changes between draws).
    fn title_text(&mut self) -> String {
        self.gather_open();
        let (mut need, mut work) = (0, 0);
        for p in self.panes.values() {
            match p.activity() {
                Some(Activity::Blocked) => need += 1,
                Some(Activity::Working) => work += 1,
                _ => {}
            }
        }
        need = need.max(crate::alerts::with_open(|l| l.iter().filter(|o| o.needs_you()).count()));
        let mut s = String::from("oriel");
        if need > 0 {
            s.push_str(&format!(" · {need} needs you"));
        }
        if work > 0 {
            s.push_str(&format!(" · {work} working"));
        }
        s
    }

    fn set_title(&mut self) {
        let t = self.title_text();
        if t != self.title {
            let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::SetTitle(&t));
            self.title = t;
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
            self.reload_theme_if_due();
            self.track_agents();
            if self.config_reload_at.is_some_and(|t| Instant::now() >= t) {
                self.reload_config();
            }
            if !moves_only {
                // (it asks every pane what's open: not for the mouse passing over, which can't change that)
                self.set_title();
            }
            self.autosave(false);
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
        self.autosave(true);
        Ok(())
    }

    /// What a mouse move can change on screen: the × under it, the menu row, the hovered pane's own highlight;
    /// plus a pane closing or a toast, in case the same batch brought one.
    fn look(&self) -> (Option<Rect>, Option<usize>, usize, usize, usize) {
        let pos = self.hover;
        let side_x = self.side_hits.iter().filter(|(_, h)| matches!(h, SideHit::CloseTab(_))).map(|x| x.0);
        let hot = self.pane_close.iter().map(|x| x.0).chain(side_x).chain(self.need_hit).find(|r| r.contains(pos));
        let pane = self.outer.iter().find(|(_, r)| r.contains(pos)).and_then(|(id, _)| self.panes.get(id)).map(|p| p.hover()).unwrap_or(0);
        (hot, self.ctx.as_ref().map(|m| m.sel), pane, self.panes.len(), self.notices.len())
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
        for n in &self.notices {
            d = d.min(TOAST_FOR.saturating_sub(n.at.elapsed()) + Duration::from_millis(10));
        }
        if let Some((_, t)) = &self.confirm {
            d = d.min(CONFIRM_FOR.saturating_sub(t.elapsed()) + Duration::from_millis(10));
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
        l.iter().map(|id| self.pane_dot(*id)).max().unwrap_or(Dot::None)
    }

    fn pane_dot(&self, id: PaneId) -> Dot {
        match self.panes.get(&id).and_then(|p| p.activity()) {
            Some(Activity::Blocked) => Dot::Blocked,
            Some(Activity::Working) => Dot::Working,
            Some(Activity::Idle) if self.done.contains(&id) => Dot::Done,
            Some(Activity::Idle) => Dot::Idle,
            None => Dot::None,
        }
    }

    /// A dot's glyph and colour (the working one spins).
    fn dot_glyph(dot: Dot, time: f64, t: &Theme) -> (&'static str, ratatui::style::Color) {
        match dot {
            Dot::Blocked => ("●", t.danger),
            Dot::Working => (["◐", "◓", "◑", "◒"][(time * 6.0) as usize % 4], t.accent),
            Dot::Done => ("●", t.good),
            Dot::Idle => ("○", t.muted),
            Dot::None => (" ", t.muted),
        }
    }

    fn tab_label(&self, i: usize) -> String {
        let t = &self.tabs[i];
        if let Some(n) = &t.name {
            return n.clone();
        }
        if let Some(a) = t.app {
            // as the sidebar names it ("chat", "your AIs"), not the internal id
            return SIDEBAR.iter().find(|x| x.0 == a).map(|x| x.2).unwrap_or(a).to_string();
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
        self.sidebar = true; // the field is drawn in the tab's row (or a popup when that row isn't drawn)
        self.tab_scrolled_for = 0; // (no tab has id 0) the tab list scrolls back to the tab being renamed
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
        self.notices.retain(|n| n.at.elapsed() <= TOAST_FOR);
        // a close/quit question nobody answered: gone (left in place, its deadline would wake the loop every 10 ms)
        if self.confirm.is_some_and(|c| c.1.elapsed() >= CONFIRM_FOR) {
            self.confirm = None;
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
                Action::Respond(pane, key, reply) => {
                    if reply == crate::alerts::Reply::Go {
                        match self.tabs.iter().position(|t| t.root.contains(pane)) {
                            Some(i) => self.focus_pane(i, pane),
                            None => {
                                self.notify("that pane is closed");
                                continue;
                            }
                        }
                    }
                    let ok = self.with_pane(pane, |p, cx| p.respond(&key, reply, cx)).unwrap_or(false);
                    if !ok && reply != crate::alerts::Reply::Go {
                        self.notify("that one has changed since · enter goes to it");
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
                Action::Palette(q) => self.palette_with(&q),
                Action::GotoApp(a) => self.goto_app(a),
                Action::AppKey(a, c) => {
                    // stay where we are, by id: opening the app inserts its tab (app tabs sort in ahead of your own),
                    // which can shift yours along
                    let here = self.tabs[self.cur].id;
                    self.goto_app(a); // opens it if it isn't yet
                    self.cur = self.tab_index(here).unwrap_or(self.cur);
                    if let Some(id) = self.tabs.iter().find(|t| t.app == Some(a)).map(|t| t.focus) {
                        self.with_pane(id, |p, cx| p.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), cx));
                    }
                }
                Action::AppPaste(a, text) => {
                    self.goto_app(a); // opens it if it isn't yet
                    if let Some(id) = self.tabs.iter().find(|t| t.app == Some(a)).map(|t| t.focus) {
                        self.with_pane(id, |p, cx| p.paste(&text, cx));
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
                Action::ToggleSidebar => self.toggle_sidebar(),
                Action::ToggleIcons => self.run_cmd(Cmd::Icons),
                Action::Tour => self.start_tour(),
                Action::Quit => self.request_quit(),
            }
        }
        self.note_tab();
    }

    /// The terminal window got or lost the focus: the app's own copy (desktop notifications) and the one panes read.
    fn focus_changed(&mut self, on: bool) {
        self.term_focused = on;
        set_term_focused(on);
    }

    fn handle(&mut self, ev: Event) {
        if let (Some(_), Event::Input(e)) = (&self.log, &ev) {
            self.log_line(&format!("{e:?}"));
        }
        // typing or clicking here means you're here, even if the terminal missed telling us (FocusGained)
        if let Event::Input(CEvent::Key(_) | CEvent::Mouse(MouseEvent { kind: MouseEventKind::Down(_), .. })) = &ev {
            self.focus_changed(true);
        }
        match ev {
            Event::Input(CEvent::Key(k)) if k.kind != KeyEventKind::Release => self.key(k),
            Event::Input(CEvent::Mouse(m)) => self.mouse(m),
            Event::Input(CEvent::Paste(s)) => self.paste(&s),
            // (before the catch-all below, or they never arrive: raise() then thinks you're always looking)
            Event::Input(CEvent::FocusGained) => self.focus_changed(true),
            Event::Input(CEvent::FocusLost) => self.focus_changed(false),
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
            // a moment later, once the writer is done (the run loop re-reads it then)
            Event::ConfigFileChanged => self.config_reload_at = Some(Instant::now() + Duration::from_millis(150)),
            Event::ThemeFilesChanged => {
                // not now: the watchers already wait for a burst to go quiet (omarchy swaps several files, editors
                // write in two steps), and the reload itself waits a moment more on the run loop, never with a sleep
                // on the UI thread, so it can tell oriel's own saves from an editor's (reload_theme_if_due)
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
        self.note_tab();
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
        self.note_tab();
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
            // the app's own key again: back to where you came from (F10 opens help and closes it)
            if self.tabs[self.cur].app != Some(a.0) || !self.go_back() {
                self.goto_app(a.0);
            }
            return true;
        }
        if n == 12 {
            // play/pause from anywhere
            if let Some(id) = self.tabs.iter().find(|t| t.app == Some("music")).map(|t| t.focus) {
                self.with_pane(id, |p, cx| p.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), cx));
                return true;
            }
            // music isn't open yet: from an app pane, open it in the background and start it (like /play); in a
            // terminal the program there gets its F12 instead
            let from = self.focused();
            if !self.panes.get(&from).is_some_and(|p| p.is_terminal()) {
                self.apply(from, vec![Action::AppKey("music", ' ')]);
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
            Self::palette_requery(p);
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
                's' => self.toggle_sidebar(),
                'p' => self.open_palette(),
                // search every AI session (a shell's own history search stays on ctrl+r)
                'r' => self.goto_app("search"),
                'z' => self.run_cmd(Cmd::Zoom),
                'w' => self.run_cmd(Cmd::Close),
                't' => self.run_cmd(Cmd::NewTab),
                ',' => self.goto_app("settings"),
                'j' => self.jump(),
                '0' => self.next_agent(),
                _ => return false,
            },
            _ => return false,
        }
        true
    }

    fn prefix_cmd(&mut self, k: KeyEvent) {
        // prefix then an alt key: the program gets it as is (Claude Code's own alt p / alt t, which oriel's alt
        // keys would take otherwise)
        if k.modifiers.contains(KeyModifiers::ALT) {
            let id = self.focused();
            self.pane_key(id, k);
            return;
        }
        match k.code {
            KeyCode::Char('|') | KeyCode::Char('\\') | KeyCode::Char('%') | KeyCode::Char('v') => self.run_cmd(Cmd::SplitRight),
            KeyCode::Char(',') => self.run_cmd(Cmd::Rename),
            KeyCode::Char('&') => self.run_cmd(Cmd::CloseTab(self.tabs[self.cur].id)),
            KeyCode::Char('-') | KeyCode::Char('"') | KeyCode::Char('_') => self.run_cmd(Cmd::SplitDown),
            KeyCode::Char('x') | KeyCode::Char('w') => self.run_cmd(Cmd::Close),
            KeyCode::Char('z') => self.run_cmd(Cmd::Zoom),
            KeyCode::Char('c') => self.run_cmd(Cmd::NewTab),
            KeyCode::Char('n') => self.run_cmd(Cmd::NextTab),
            KeyCode::Char('p') => self.run_cmd(Cmd::PrevTab),
            KeyCode::Char(';') => self.run_cmd(Cmd::Back),
            KeyCode::Char('<') => self.run_cmd(Cmd::MoveTab(-1)),
            KeyCode::Char('>') => self.run_cmd(Cmd::MoveTab(1)),
            // your own tabs, like alt 1-9 (not the app tabs ahead of them)
            KeyCode::Char(c @ '1'..='9') => {
                let i = c as usize - '1' as usize;
                if let Some(&t) = self.user_tabs().get(i) {
                    self.cur = t;
                }
            }
            KeyCode::Char('C') => self.run_cmd(Cmd::Open("claude", Place::Split)),
            KeyCode::Char('X') => self.run_cmd(Cmd::Open("codex", Place::Split)),
            KeyCode::Char('h') | KeyCode::Left => self.move_focus(-1, 0),
            KeyCode::Char('l') | KeyCode::Right => self.move_focus(1, 0),
            KeyCode::Char('k') | KeyCode::Up => self.move_focus(0, -1),
            KeyCode::Char('j') | KeyCode::Down => self.move_focus(0, 1),
            KeyCode::Char('H') => self.resize(Dir::Right, -0.05),
            KeyCode::Char('L') => self.resize(Dir::Right, 0.05),
            KeyCode::Char('K') => self.resize(Dir::Down, -0.05),
            KeyCode::Char('J') => self.resize(Dir::Down, 0.05),
            KeyCode::Char('t') => self.palette_with("theme "),
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
        // zoomed: look at where the panes would be, then unzoom onto the neighbour (like tmux)
        let t = &self.tabs[self.cur];
        let rects = if t.zoom {
            let mut r = vec![];
            t.root.rects(self.body, &mut r);
            r
        } else {
            self.outer.clone()
        };
        if let Some(to) = neighbor(&rects, from, dx, dy) {
            let t = self.tab();
            t.focus = to;
            t.zoom = false;
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
            Cmd::Tab(t) => {
                if let Some(i) = self.tab_index(t) {
                    self.cur = i;
                }
            }
            Cmd::Rename => self.start_rename(self.cur),
            Cmd::CloseTab(t) => self.request_close_tab(t),
            // the apps that keep one copy (music, calendar, agents, your AIs) are gone to, not opened again
            Cmd::Open(name, _) if panes::SINGLE.contains(&name) => self.goto_app(name),
            // a terminal (or an agent, or files) opened from a pane starts in that pane's folder
            Cmd::Open(name, place) => match panes::open_in(name, &self.config, self.panes.get(&from).and_then(|p| p.cwd())) {
                Some(p) => self.open(p, place, from),
                None => self.notify(format!("{name} isn't installed")),
            },
            Cmd::NextTab => self.cur = (self.cur + 1) % self.tabs.len(),
            Cmd::PrevTab => self.cur = (self.cur + self.tabs.len() - 1) % self.tabs.len(),
            Cmd::Back => {
                if !self.go_back() {
                    self.notify("no tab to go back to");
                }
            }
            Cmd::MoveTab(d) => {
                // among your own tabs (the app tabs keep their places)
                let user = self.user_tabs();
                if let Some(k) = user.iter().position(|&i| i == self.cur) {
                    if let Some(&j) = user.get((k as i32 + d).max(0) as usize).filter(|&&j| j != self.cur) {
                        self.tabs.swap(self.cur, j);
                        self.cur = j;
                    }
                }
            }
            Cmd::Sidebar => self.toggle_sidebar(),
            Cmd::Jump => self.jump(),
            Cmd::NextAgent => self.next_agent(),
            Cmd::Config => {
                let p = config::path();
                if !p.exists() {
                    self.notify(format!("no config.toml yet (every setting is at its default): it goes in {}", p.display()));
                } else {
                    let res = if cfg!(test) { Ok(()) } else { panes::files::preview::open_external(&p) };
                    match res {
                        Ok(()) => self.notify(format!("opening {}", p.display())),
                        Err(e) => self.notify(format!("can't open {}: {e}", p.display())),
                    }
                }
            }
            Cmd::Paste => {
                // the clipboard is read off the UI thread; it arrives as a normal paste
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let got = if cfg!(test) { None } else { crate::clip::grab_text() }; // tests never read the clipboard
                    if let Some(s) = got.filter(|s| !s.is_empty()) {
                        let _ = tx.send(Event::Input(CEvent::Paste(s)));
                    }
                });
            }
            Cmd::Copy(text) => {
                crate::clip::copy(&text);
                let n = text.chars().count();
                self.notify(format!("copied {n} character{}", if n == 1 { "" } else { "s" }));
            }
            Cmd::AskChat(text) => self.apply(from, vec![Action::AppPaste("ai", format!("```\n{}\n```\n\n", text.trim_end()))]),
            Cmd::ToChat(text) => self.apply(from, vec![Action::AppPaste("ai", text)]),
            Cmd::Theme(t) => {
                // the toast first, so a "not saved" from set_theme is the one you see
                self.notify(format!("{}theme: {t}", ui::lead("theme")));
                self.set_theme(&t, true);
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
                self.notify(if n { "nerd font icons · remembered" } else { "plain icons (no nerd font) · remembered" });
                self.set_icons(n);
            }
            Cmd::Help => self.open_help(),
            Cmd::Tour => self.start_tour(),
            Cmd::Setting(id) => {
                panes::settings::jump(&id);
                self.goto_app("settings");
            }
            Cmd::Quit => self.request_quit(),
        }
    }

    /// Can you close panes of the current tab? Not the one pane of an app tab (the frame shows no × there).
    fn closable(&self) -> bool {
        let t = &self.tabs[self.cur];
        t.app.is_none() || matches!(t.root, Node::Split { .. })
    }

    // ------------------------------------------------------------------ palette
    /// The prefix key then `k`, as the palette and hints say it ("ctrl+space |").
    fn pk(&self, k: &str) -> String {
        format!("{} {k}", config::prefix(&self.config))
    }

    fn open_palette(&mut self) {
        panes::refresh_available(); // installed claude/codex a moment ago? the list below sees it
        let mut items: Vec<Item> = vec![];
        let mut add = |group: &'static str, icon: &'static str, label: String, key: String, cmd: Cmd| items.push(Item { icon, label, key, cmd, group });
        for &(name, icon, label, key) in SIDEBAR {
            add("apps", icon, format!("go to {label}"), key.to_string(), Cmd::App(name));
        }
        // not in the sidebar: it comes up over whatever you're doing
        add("apps", "history", "search chats: every AI session on this computer".into(), "alt r".into(), Cmd::App("search"));
        // your tabs, so alt p reaches the ones past alt 9 too
        let time = self.start.elapsed().as_secs_f64();
        for (n, i) in self.user_tabs().into_iter().enumerate() {
            let t = &self.tabs[i];
            let mut l = vec![];
            t.root.leaves(&mut l);
            let mut key = vec![];
            let dot = self.tab_dot(i);
            if !matches!(dot, Dot::None | Dot::Idle) {
                key.push(Self::dot_glyph(dot, time, &self.theme).0.to_string());
            }
            if l.len() > 1 {
                key.push(format!("{} panes", l.len()));
            }
            if n < 9 {
                key.push(format!("alt {}", n + 1));
            }
            let icon = self.panes.get(&t.focus).map(|p| p.icon()).unwrap_or("term");
            add("tabs", icon, format!("tab: {}", self.tab_label(i)), key.join(" · "), Cmd::Tab(t.id));
        }
        add("tabs", "tab", "back to the last tab".into(), self.pk(";"), Cmd::Back);
        add("tabs", "tab", "next tab".into(), self.pk("n"), Cmd::NextTab);
        add("tabs", "tab", "previous tab".into(), self.pk("p"), Cmd::PrevTab);
        add("tabs", "bell", "jump to what needs you".into(), "alt j".into(), Cmd::Jump);
        add("tabs", "robot", "next agent that needs you or finished".into(), "alt 0".into(), Cmd::NextAgent);
        add("tabs", "tab", "new tab".into(), "alt t".into(), Cmd::NewTab);
        if self.tabs[self.cur].app.is_none() {
            add("tabs", "tab", "rename this tab".into(), self.pk(","), Cmd::Rename);
            add("tabs", "tab", "move this tab up".into(), self.pk("<"), Cmd::MoveTab(-1));
            add("tabs", "tab", "move this tab down".into(), self.pk(">"), Cmd::MoveTab(1));
            add("tabs", "close", "close this tab".into(), self.pk("&"), Cmd::CloseTab(self.tabs[self.cur].id));
        }
        // what the pane you're in can hand to chat ("send this note to chat")
        if let Some((what, text)) = self.panes.get(&self.focused()).and_then(|p| p.for_chat()) {
            add("panes", "ai", format!("send {what} to chat"), String::new(), Cmd::ToChat(text));
        }
        // (the apps that keep one copy have their "go to" above instead)
        let extra = || APPS.iter().filter(|a| !panes::SINGLE.contains(&a.0) && available(a.0));
        for &(name, key, icon, label) in extra() {
            // the prefix letters that open it beside this
            let k = match name {
                "terminal" => "alt n".to_string(),
                "claude" => self.pk("C"),
                "codex" => self.pk("X"),
                "notes" => self.pk("e"),
                _ => self.pk(&key.to_string()),
            };
            add("panes", icon, format!("split: open {label} beside this"), k, Cmd::Open(name, Place::Split));
        }
        for &(name, _, icon, label) in extra() {
            add("panes", icon, format!("open {label} in a new tab"), String::new(), Cmd::Open(name, Place::Tab));
        }
        add("panes", "split", "split right".into(), self.pk("|"), Cmd::SplitRight);
        add("panes", "split", "split down".into(), self.pk("-"), Cmd::SplitDown);
        add("panes", "window", "zoom pane".into(), "alt z".into(), Cmd::Zoom);
        if self.closable() {
            add("panes", "close", "close pane".into(), "alt w".into(), Cmd::Close);
        }
        add("panes", "window", "toggle sidebar".into(), "alt s".into(), Cmd::Sidebar);
        // the themes before the editor: typing "theme" is usually picking one
        for t in theme::names() {
            let yours = if theme::is_custom(&t) { "yours" } else { "" };
            add("themes", "theme", format!("theme {t}"), yours.into(), Cmd::Theme(t));
        }
        add("themes", "theme", "theme editor: make your own".into(), String::new(), Cmd::App("themes"));
        let upd = match &self.update_ready {
            Some(v) => format!("update oriel to {v}: what's new"),
            None => "updates: what's new, check, roll back".to_string(),
        };
        add("oriel", "package", upd, String::new(), Cmd::App("updates"));
        add("oriel", "doc", "open config.toml".into(), String::new(), Cmd::Config);
        add("oriel", "theme", "toggle nerd font icons".into(), String::new(), Cmd::Icons);
        add("oriel", "search", "help: every key, command and how-to".into(), "F10".into(), Cmd::Help);
        add("oriel", "window", "take the tour".into(), String::new(), Cmd::Tour);
        // every setting, by name: opens settings on its row
        for (label, id) in panes::settings::palette_items(&self.config) {
            add("settings", "cog", format!("setting: {label}"), String::new(), Cmd::Setting(id));
        }
        add("oriel", "quit", "quit oriel".into(), self.pk("q"), Cmd::Quit);
        self.palette = Some(Palette { query: String::new(), sel: 0, items, theme_before: self.theme.name.clone(), rect: Rect::default(), rows: vec![] });
        self.palette_seen += 1;
    }

    /// How well `q` matches `s`: 4 = s starts with it, 3 = a word in s does, 2 = it's in s somewhere, 1 = its
    /// letters are in s in order (spaces ignored); None = no match.
    fn score(s: &str, q: &str) -> Option<u8> {
        let s = s.to_lowercase();
        if q.is_empty() {
            return Some(0);
        }
        if s.starts_with(q) {
            return Some(4);
        }
        let mut best = None;
        for (at, _) in s.match_indices(q) {
            let word = s[..at].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
            best = best.max(Some(if word { 3 } else { 2 }));
        }
        if best.is_some() {
            return best;
        }
        let mut it = s.chars().filter(|c| !c.is_whitespace());
        q.chars().filter(|c| !c.is_whitespace()).all(|c| it.any(|x| x == c)).then_some(1)
    }

    /// The items that match the query, best first (ties keep the list's order).
    fn palette_matches(p: &Palette) -> Vec<usize> {
        let q = p.query.trim().to_lowercase();
        let mut m: Vec<(u8, usize)> = (0..p.items.len())
            .filter_map(|i| {
                let it = &p.items[i];
                // the key counts too ("alt z" finds zoom), a notch below a label that starts with it; only as a
                // run of letters, since "ctrl+space |" holds most of the alphabet scattered
                let s = Self::score(&it.label, &q).max(Self::score(&it.key, &q).filter(|&s| s >= 2).map(|s| s.min(3)))?;
                Some((s, i))
            })
            .collect();
        m.sort_by_key(|&(s, _)| std::cmp::Reverse(s));
        m.into_iter().map(|(_, i)| i).collect()
    }

    /// The query changed: back to the best match, except that asking for themes starts on the one you have, so
    /// the preview begins from it (prefix t and /theme open the palette on "theme ").
    fn palette_requery(p: &mut Palette) {
        p.sel = 0;
        if p.query.trim_start().to_lowercase().starts_with("theme") {
            let m = Self::palette_matches(p);
            let q = p.query.trim().to_lowercase();
            let top = m.first().and_then(|&i| Self::score(&p.items[i].label, &q));
            if let Some(at) = m.iter().position(|&i| matches!(&p.items[i].cmd, Cmd::Theme(t) if *t == p.theme_before)) {
                if Self::score(&p.items[m[at]].label, &q) == top {
                    p.sel = at;
                }
            }
        }
    }

    /// Open the palette with this already typed.
    fn palette_with(&mut self, q: &str) {
        self.open_palette();
        if let Some(p) = &mut self.palette {
            p.query = q.to_string();
            Self::palette_requery(p);
        }
        self.palette_preview();
    }

    fn palette_key(&mut self, k: KeyEvent) {
        let Some(p) = self.palette.as_mut() else { return };
        let m = Self::palette_matches(p);
        match k.code {
            KeyCode::Esc => return self.palette_close(),
            KeyCode::Enter => return self.palette_run(),
            KeyCode::Down | KeyCode::Tab => p.sel = (p.sel + 1).min(m.len().saturating_sub(1)),
            KeyCode::Up | KeyCode::BackTab => p.sel = p.sel.saturating_sub(1),
            KeyCode::PageDown => p.sel = (p.sel + 10).min(m.len().saturating_sub(1)),
            KeyCode::PageUp => p.sel = p.sel.saturating_sub(10),
            KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => p.sel = (p.sel + 1).min(m.len().saturating_sub(1)),
            KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => p.sel = p.sel.saturating_sub(1),
            KeyCode::Backspace => {
                p.query.pop();
                Self::palette_requery(p);
            }
            KeyCode::Char(c) => {
                p.query.push(c);
                Self::palette_requery(p);
            }
            _ => {}
        }
        self.palette_preview();
    }

    /// Live preview while a theme is highlighted (the theme from before comes back otherwise).
    fn palette_preview(&mut self) {
        let Some(p) = &self.palette else { return };
        let m = Self::palette_matches(p);
        let preview = match m.get(p.sel).map(|&i| &p.items[i].cmd) {
            Some(Cmd::Theme(t)) => t.clone(),
            _ => p.theme_before.clone(),
        };
        if preview != self.theme.name {
            self.set_theme(&preview, false);
        }
    }

    /// Run the palette's pick (enter, or a click on its row).
    fn palette_run(&mut self) {
        // a click can land on a row without the pick moving there first: a theme previewed on the way stays only if
        // the pick is that theme
        self.palette_preview();
        let Some(p) = self.palette.take() else { return };
        let m = Self::palette_matches(&p);
        if let Some(c) = m.get(p.sel).map(|&i| p.items[i].cmd.clone()) {
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
        self.note_tab();
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
        // a click on a toast goes to what it's about (and puts it away)
        if let MouseEventKind::Down(MouseButton::Left) = m.kind {
            if let Some(&(_, at)) = self.toast_hits.iter().find(|(r, _)| r.contains(pos)) {
                let pane = self.notices.iter().position(|n| n.at == at).map(|i| self.notices.remove(i)).and_then(|n| n.pane);
                self.toast_hits.clear();
                if let Some(p) = pane {
                    match self.tabs.iter().position(|t| t.root.contains(p)) {
                        Some(i) => self.focus_pane(i, p),
                        None => self.notify("that pane is closed"),
                    }
                }
                return;
            }
        }
        // the wheel over your tabs in the sidebar scrolls them
        if matches!(m.kind, MouseEventKind::ScrollDown | MouseEventKind::ScrollUp) && self.tabs_rect.contains(pos) {
            let n = self.user_tabs().len();
            self.tab_scroll = if m.kind == MouseEventKind::ScrollDown { (self.tab_scroll + 1).min(n.saturating_sub(1)) } else { self.tab_scroll.saturating_sub(1) };
            return;
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
                    let mut items = vec![];
                    // text selected here: copy it, or take it to chat
                    if self.sel.is_some_and(|s| s.active && s.pane == id) && !self.sel_text.trim().is_empty() {
                        if self.tabs[self.cur].app != Some("ai") {
                            items.push((format!("{}ask chat about this", ui::lead("ai")), Cmd::AskChat(self.sel_text.clone())));
                        }
                        items.push((format!("{}copy", ui::lead("file")), Cmd::Copy(self.sel_text.clone())));
                    }
                    items.push((format!("{}paste", ui::lead("doc")), Cmd::Paste));
                    items.extend([
                        (format!("{}split right", ui::lead("split")), Cmd::SplitRight),
                        (format!("{}split down", ui::lead("split")), Cmd::SplitDown),
                        (format!("{}{}", ui::lead("window"), if self.tabs[self.cur].zoom { "unzoom" } else { "zoom" }), Cmd::Zoom),
                    ]);
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
            // "← 2 need you" on the frame: the alerts app lists them
            if self.need_hit.is_some_and(|r| r.contains(pos)) {
                self.goto_app("alerts");
                return;
            }
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
                    SideHit::MoreTabs => {
                        // on a page; round to the top after the last
                        let n = self.user_tabs().len();
                        let page = (self.tabs_rect.height as usize).saturating_sub(1).max(1);
                        self.tab_scroll = if self.tab_scroll + page >= n { 0 } else { self.tab_scroll + page };
                    }
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
        // what's open right now, for the bell, the frames and the alerts app
        self.gather_open();
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
        self.tabs_rect = Rect::default();
        if side_w > 0 {
            self.draw_sidebar(f, side, &t);
        } else if self.body.height >= 8 && self.body.width >= 20 {
            // no sidebar (hidden, or no room for it): the tabs as one row across the top
            self.draw_tab_strip(f, Rect { height: 1, ..self.body }, &t);
            self.body = Rect { y: self.body.y + 1, height: self.body.height - 1, ..self.body };
        }

        // what's waiting on you elsewhere: said on the focused frame (not in alerts, which lists it all)
        let need = if self.tabs[self.cur].app == Some("alerts") { 0 } else { self.need_elsewhere() };
        self.need_hit = None;
        let tab = &self.tabs[self.cur];
        let mut rects = vec![];
        if tab.zoom {
            rects.push((tab.focus, self.body));
        } else {
            tab.root.rects(self.body, &mut rects);
        }
        let (focus, zoom) = (tab.focus, tab.zoom);
        // a pane can be closed from its frame if it's one of several, or if the tab is one you made
        let closable = rects.len() > 1 || tab.app.is_none();
        self.outer = rects.clone();
        self.inner.clear();
        self.pane_close.clear();
        let time = self.start.elapsed().as_secs_f64();
        for (id, r) in rects {
            let Some(p) = self.panes.get(&id) else { continue };
            let title = format!("{}{}", ui::lead(p.icon()), p.title());
            let mut sub = p.subtitle();
            if zoom {
                // the other panes are only hidden: say so
                sub = Some(match sub {
                    Some(s) => format!("{s} · zoomed"),
                    None => "zoomed".into(),
                });
            }
            let inner = ui::frame(f, r, &title, sub.as_deref(), id == focus, &t);
            self.inner.push((id, inner));
            if need > 0 && id == focus {
                // bottom-left of the frame, clear of the subtitle on the right
                let label = format!(" ← {need} need{} you ", if need == 1 { "s" } else { "" });
                let (lw, sw) = (label.chars().count() as u16, sub.as_deref().map(|s| unicode_width::UnicodeWidthStr::width(s) as u16 + 2).unwrap_or(0));
                if r.height > 2 && r.width > lw + sw + 6 {
                    let b = Rect { x: r.x + 2, y: r.bottom() - 1, width: lw, height: 1 };
                    let st = Style::default().fg(t.danger).add_modifier(Modifier::BOLD);
                    f.render_widget(Paragraph::new(Span::styled(label, if b.contains(self.hover) { st.add_modifier(Modifier::REVERSED) } else { st })), b);
                    self.need_hit = Some(b);
                }
            }
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
                self.sel_text = text.trim_end_matches('\n').to_string();
                if self.copy_pending {
                    self.copy_pending = false;
                    if !self.sel_text.trim().is_empty() {
                        crate::clip::copy(&self.sel_text);
                        let n = self.sel_text.chars().count();
                        self.notify(format!("copied {n} character{}", if n == 1 { "" } else { "s" }));
                    }
                }
            } else {
                self.sel = None;
            }
        }
        self.draw_toast(f, &t);
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
        // renaming a tab whose sidebar row (where the field lives) isn't drawn (no sidebar, no room, scrolled out
        // of view): a small box instead
        let in_sidebar = |id: u64| side_w > 0 && self.side_hits.iter().any(|(_, h)| matches!(h, SideHit::Tab(t) if *t == id));
        if let Some((_, text)) = self.renaming.as_ref().filter(|(id, _)| !in_sidebar(*id)) {
            let inner = ui::popup(f, area, 48, 4, &format!("{}rename tab", ui::lead("tab")), &t);
            let line = Line::from(vec![Span::styled(format!(" {text}▏"), Style::default().fg(t.accent).add_modifier(Modifier::BOLD))]);
            f.render_widget(Paragraph::new(vec![line, Line::styled(" enter ok · esc", ui::muted(&t))]), inner);
        }
        if let Some(ob) = &mut self.onboard {
            ob.draw(f, area, &t, self.start.elapsed().as_secs_f64());
        }
    }

    /// With no sidebar: every tab in one row across the top. An app tab is its icon (the current one also its
    /// name); your tabs are icon, name and status dot. Click one to go there.
    fn draw_tab_strip(&mut self, f: &mut Frame, r: Rect, t: &Theme) {
        let time = self.start.elapsed().as_secs_f64();
        let cells: Vec<(String, String, Dot, bool, SideHit)> = (0..self.tabs.len())
            .map(|i| {
                let tb = &self.tabs[i];
                let on = i == self.cur;
                match tb.app {
                    Some(a) => {
                        let (icon, label) = SIDEBAR.iter().find(|x| x.0 == a).map(|x| (x.1, x.2)).unwrap_or(("window", a));
                        let icon = if ui::icon(icon).is_empty() { "window" } else { icon };
                        (ui::lead(icon), if on || ui::icon(icon).is_empty() { label.to_string() } else { String::new() }, Dot::None, on, SideHit::App(a))
                    }
                    None => {
                        let icon = self.panes.get(&tb.focus).map(|p| p.icon()).unwrap_or("term");
                        (ui::lead(icon), self.tab_label(i), self.tab_dot(i), on, SideHit::Tab(tb.id))
                    }
                }
            })
            .collect();
        // names get shorter until the row fits
        let width = |max: usize| -> usize {
            cells.iter().map(|(icon, name, dot, _, _)| {
                let n = unicode_width::UnicodeWidthStr::width(ui::fit(name, max).as_str());
                2 + unicode_width::UnicodeWidthStr::width(icon.as_str()) + n + if matches!(dot, Dot::None | Dot::Idle) { 0 } else { 2 }
            }).sum()
        };
        let mut max = 18;
        while max > 3 && width(max) > r.width as usize {
            max -= 1;
        }
        let mut x = r.x;
        for (icon, name, dot, on, hit) in cells {
            let name = ui::fit(&name, max);
            let st = if on { Style::default().fg(t.accent).add_modifier(Modifier::BOLD | Modifier::REVERSED) } else { Style::default().fg(t.muted) };
            let mut spans = vec![Span::styled(format!(" {icon}{name}"), st)];
            if !matches!(dot, Dot::None | Dot::Idle) {
                let (g, c) = Self::dot_glyph(dot, time, t);
                spans.push(Span::styled(format!(" {g}"), if on { st.fg(c) } else { Style::default().fg(c) }));
            }
            spans.push(Span::styled(" ", st));
            let w = spans.iter().map(|s| s.width()).sum::<usize>() as u16;
            if x >= r.right() {
                break;
            }
            let cell = Rect { x, y: r.y, width: w.min(r.right() - x), height: 1 };
            f.render_widget(Paragraph::new(Line::from(spans)), cell);
            self.side_hits.push((cell, hit));
            x += w;
        }
    }

    /// The sidebar: the apps in sections (F1-F10), your own tabs, then the current app's own section. When
    /// rows are short, the app list folds up so your tabs keep room.
    fn draw_sidebar(&mut self, f: &mut Frame, area: Rect, t: &Theme) {
        let time = self.start.elapsed().as_secs_f64();
        let cur_app = self.tabs[self.cur].app;
        let unread = crate::alerts::unread();
        let waiting = crate::alerts::with_open(|l| l.iter().filter(|o| o.needs_you()).count());
        let extra = self.update_ready.is_some() as u16;
        let foot_n = (SIDEBAR.len() - FOOTER) as u16 + extra;
        // the footer (alerts, themes, help) needs room; without it, the new-alerts count goes in the title
        let footer = area.height.saturating_sub(3) > foot_n + 12;
        let mut title = format!("{}oriel", ui::lead("window"));
        if !footer && unread > 0 {
            title.push_str(&format!(" · {}{unread}", ui::lead("bell")));
        }
        // where you are, as the sidebar names it
        let sub = cur_app.map(|a| SIDEBAR.iter().find(|x| x.0 == a).map(|x| x.2).unwrap_or(a)).unwrap_or("tabs");
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
        if area.width > cw + 4 + unicode_width::UnicodeWidthStr::width(title.as_str()) as u16 + 4 {
            f.render_widget(Paragraph::new(Span::styled(clock, ui::muted(t))), Rect { x: area.right() - cw - 2, y: area.y, width: cw, height: 1 });
        }
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), y: inner.y + 1, height: inner.height.saturating_sub(1) };
        // ---- the footer: alerts, themes and help, pinned to the bottom
        let inner = if footer {
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
                let (unread, waiting) = if name == "alerts" { (unread, waiting) } else { (0, 0) };
                if unread > 0 || waiting > 0 {
                    // the bell lights up with how many are waiting on you (or new), and the key that goes to them
                    let st = Style::default().fg(if waiting > 0 { t.danger } else { t.accent }).add_modifier(Modifier::BOLD);
                    let n = if waiting > 0 { format!("{waiting} need{} you", if waiting == 1 { "s" } else { "" }) } else { format!("{unread} new") };
                    // the key too, if it fits beside the whole label
                    let with_key = format!("{n} · alt j");
                    let n = if r.width as usize > label.len() + 3 + with_key.chars().count() { with_key } else { n };
                    let w = (r.width as usize).saturating_sub(n.chars().count() + 1);
                    let left = ui::fit(&format!("{}{label}", ui::lead(icon)), w);
                    let pad = w.saturating_sub(unicode_width::UnicodeWidthStr::width(left.as_str())) + 1;
                    f.render_widget(Paragraph::new(Line::from(vec![Span::styled(left, st), Span::raw(" ".repeat(pad)), Span::styled(n, st)])), r);
                } else {
                    ui::side_row(f, r, icon, label, key, cur_app == Some(name), t);
                }
                self.side_hits.push((r, SideHit::App(name)));
            }
            Rect { height: inner.height - foot_n - 1, ..inner }
        } else {
            inner
        };
        // ---- the apps: with their section headings when there's room, without them when it's tight, and at
        // the tightest the tools fold into one row of icons (their F-keys still work)
        let user = self.user_tabs();
        let section = cur_app.is_some();
        // (three of your tabs before the tools fold: with the footer's settings row, that's what keeps every app's
        // name listed at Windows Terminal's default 120x30)
        let want = 2 + user.len().min(3) as u16 + 1 + if section { 6 } else { 0 };
        let tools = SECTIONS[1].0;
        let (headings, icons) = if inner.height >= 13 + want {
            (true, false)
        } else if inner.height >= 10 + want {
            (false, false)
        } else {
            (false, true)
        };
        let mut y = inner.y;
        for (idx, &(name, icon, label, key)) in SIDEBAR[..FOOTER].iter().enumerate() {
            if y >= inner.bottom() {
                break;
            }
            if icons && idx >= tools {
                // one row: each tool's icon, the current one lit
                let mut x = inner.x;
                for &(name, icon, label, _) in &SIDEBAR[tools..FOOTER] {
                    let g = if ui::icon(icon).is_empty() { label.chars().next().map(String::from).unwrap_or_default() } else { ui::icon(icon).to_string() };
                    let w = unicode_width::UnicodeWidthStr::width(g.as_str()) as u16 + 2;
                    if x + w > inner.right() {
                        break;
                    }
                    let on = cur_app == Some(name);
                    let st = if on { Style::default().fg(t.accent).add_modifier(Modifier::BOLD | Modifier::REVERSED) } else { Style::default().fg(t.muted) };
                    let r = Rect { x, y, width: w, height: 1 };
                    f.render_widget(Paragraph::new(Span::styled(format!(" {g} "), st)), r);
                    self.side_hits.push((r, SideHit::App(name)));
                    x += w;
                }
                y += 1;
                break;
            }
            if let Some((_, heading)) = SECTIONS.iter().find(|(at, _)| headings && *at == idx) {
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
        // ---- your own tabs: they scroll when there are more than fit, with "+N more" at the end
        y += 1;
        if y < inner.bottom() {
            ui::rule(f, Rect { y, height: 1, ..inner }, t);
            y += 1;
        }
        let left = inner.bottom().saturating_sub(y);
        // leave the current app's section a few rows, unless your tabs need them more
        let reserve = if section { 6.min(left.saturating_sub(3)) } else { 0 };
        let room = left.saturating_sub(reserve) as usize;
        let fits = user.is_empty() || user.len() < room; // every tab, plus "new tab"
        let shown = if fits { user.len() } else { room.saturating_sub(1) };
        // keep the tab you're on in view (once, when you arrive: the wheel can scroll away from it)
        let cur_id = self.tabs[self.cur].id;
        if let Some(k) = user.iter().position(|&i| i == self.cur).filter(|_| self.tab_scrolled_for != cur_id) {
            if k < self.tab_scroll {
                self.tab_scroll = k;
            } else if shown > 0 && k >= self.tab_scroll + shown {
                self.tab_scroll = k + 1 - shown;
            }
            self.tab_scrolled_for = cur_id;
        }
        self.tab_scroll = if fits { 0 } else { self.tab_scroll.min(user.len().saturating_sub(shown)) };
        self.tabs_rect = Rect { y, height: (room as u16).min(inner.bottom().saturating_sub(y)), ..inner };
        for (n, &i) in user.iter().enumerate().skip(self.tab_scroll).take(shown) {
            let tb = &self.tabs[i];
            let icon = self.panes.get(&tb.focus).map(|p| p.icon()).unwrap_or("term");
            let title = self.tab_label(i);
            let panes = { let mut l = vec![]; tb.root.leaves(&mut l); l.len() };
            // alt only reaches 1-9
            let alt = if n < 9 { format!("alt {}", n + 1) } else { String::new() };
            let right = if panes > 1 { format!("{panes} panes  {alt}").trim_end().to_string() } else { alt };
            let r = Rect { y, height: 1, ..inner };
            let xr = Rect { x: r.right().saturating_sub(2), width: 2, ..r };
            let (glyph, color) = Self::dot_glyph(self.tab_dot(i), time, t);
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
        if !fits && y < inner.bottom() {
            let hidden = user.len() - shown;
            let r = Rect { y, height: 1, ..inner };
            ui::side_row(f, Rect { x: r.x + 2, width: r.width.saturating_sub(2), ..r }, "", &format!("+{hidden} more"), "wheel", false, t);
            self.side_hits.push((r, SideHit::MoreTabs));
            y += 1;
        } else if fits && y < inner.bottom() {
            let r = Rect { y, height: 1, ..inner };
            ui::side_row(f, r, "tab", "new tab", "alt t", false, t);
            self.side_hits.push((r, SideHit::NewTab));
            y += 1;
        }
        // ---- the current app's own section
        if section {
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

    /// Toasts, stacked at the top right of the panes under the frame's title (never over a composer or a hint
    /// line): the prefix keys, a close question, then notices newest first. An alert's toast is in its kind's
    /// colour and titled with it ("finished", "failed"); a click on one goes to its pane.
    fn draw_toast(&mut self, f: &mut Frame, t: &Theme) {
        self.toast_hits.clear();
        let area = self.body;
        // (title, colour, lines of (glyph, text), the notice it is)
        let mut boxes: Vec<(String, ratatui::style::Color, Vec<(String, String)>, Option<Instant>)> = vec![];
        if self.prefix_armed {
            let lines = vec![
                ("".into(), "| - split · hjkl move · x close · z zoom · c new tab · 1-9 n p ; tabs".into()),
                ("".into(), "a m s f g e C X open an app beside this · t theme · ? help · alt keys go through".into()),
            ];
            boxes.push(("prefix".into(), t.accent, lines, None));
        }
        if let Some(q) = self.confirm_text() {
            boxes.push(("careful".into(), t.danger, vec![("".into(), q)], None));
        }
        for n in self.notices.iter().rev() {
            match n.kind {
                Some(k) => {
                    let (glyph, name) = k.label();
                    boxes.push((name.to_string(), panes::alerts::color(k, t), vec![(glyph.to_string(), n.text.clone())], Some(n.at)));
                }
                None => boxes.push(("oriel".into(), t.accent, vec![("".into(), n.text.clone())], Some(n.at))),
            }
        }
        let mut y = area.y + 1;
        let max_w = area.width.saturating_sub(4);
        for (title, color, lines, notice) in boxes {
            let text_w = lines.iter().map(|(g, s)| unicode_width::UnicodeWidthStr::width(s.as_str()) + if g.is_empty() { 0 } else { unicode_width::UnicodeWidthStr::width(g.as_str()) + 1 }).max().unwrap_or(0);
            let title_w = unicode_width::UnicodeWidthStr::width(title.as_str()) + 4;
            // one column of padding each side, inside the border
            let w = ((text_w.max(title_w) + 4) as u16).min(max_w);
            let h = lines.len() as u16 + 2;
            if w < 8 || y + h > area.bottom() {
                break;
            }
            let r = Rect { x: area.right().saturating_sub(w + 2), y, width: w, height: h };
            f.render_widget(ratatui::widgets::Clear, r);
            let block = ratatui::widgets::Block::default()
                .borders(ratatui::widgets::Borders::ALL)
                .border_type(ratatui::widgets::BorderType::Rounded)
                .border_style(Style::default().fg(color))
                .title(Line::from(Span::styled(format!(" {title} "), Style::default().fg(color).add_modifier(Modifier::BOLD))));
            let inner = block.inner(r);
            f.render_widget(block, r);
            let room = inner.width.saturating_sub(2) as usize;
            let rows: Vec<Line> = lines
                .iter()
                .map(|(g, s)| {
                    if g.is_empty() {
                        let st = if notice.is_some() && title != "oriel" { Style::default().fg(t.fg) } else { Style::default().fg(color) };
                        Line::from(Span::styled(format!(" {}", ui::fit(s, room)), st))
                    } else {
                        let room = room.saturating_sub(unicode_width::UnicodeWidthStr::width(g.as_str()) + 1);
                        Line::from(vec![Span::styled(format!(" {g} "), Style::default().fg(color).add_modifier(Modifier::BOLD)), Span::styled(ui::fit(s, room), Style::default().fg(t.fg))])
                    }
                })
                .collect();
            f.render_widget(Paragraph::new(rows), inner);
            if let Some(at) = notice {
                self.toast_hits.push((r, at));
            }
            y += h;
        }
    }

    fn draw_palette(f: &mut Frame, area: Rect, p: &mut Palette, t: &Theme) {
        let m = Self::palette_matches(p);
        // a centred box, with how many match in its bottom border
        let w = 76.min(area.width.saturating_sub(4));
        let h = 22.min(area.height.saturating_sub(2));
        let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + area.height.saturating_sub(h) / 3, width: w, height: h };
        f.render_widget(ratatui::widgets::Clear, r);
        let count = if m.is_empty() { "0".to_string() } else { format!("{}/{}", p.sel + 1, m.len()) };
        let inner = ui::frame(f, r, &format!("{}palette", ui::lead("search")), Some(&count), true, t);
        p.rect = r;
        p.rows.clear();
        let q = Line::from(vec![Span::styled("› ", ui::bold_accent(t)), Span::raw(p.query.clone()), Span::styled("▏", ui::accent(t))]);
        f.render_widget(Paragraph::new(q), Rect { height: 1, ..inner });
        if inner.height >= 4 {
            let hint = Rect { y: inner.bottom() - 1, height: 1, ..inner };
            f.render_widget(Paragraph::new(Span::styled(" enter run · esc close · ↑↓ pick · type to search", ui::muted(t))), hint);
        }
        // the rows: with muted group headings while nothing is typed
        enum Row {
            Head(&'static str),
            It(usize),
        }
        let mut disp = vec![];
        let mut group = "";
        for (k, &i) in m.iter().enumerate() {
            if p.query.trim().is_empty() && p.items[i].group != group {
                group = p.items[i].group;
                disp.push(Row::Head(group));
            }
            disp.push(Row::It(k));
        }
        let rows = inner.height.saturating_sub(3) as usize;
        let at = disp.iter().position(|d| matches!(d, Row::It(k) if *k == p.sel)).unwrap_or(0);
        let mut start = at.saturating_sub(rows.saturating_sub(1));
        // the selection's heading stays with it at the top
        if start > 0 && start == at && matches!(disp[start - 1], Row::Head(_)) {
            start -= 1;
        }
        for (row, d) in disp.iter().skip(start).take(rows).enumerate() {
            let r = Rect { y: inner.y + 2 + row as u16, height: 1, ..inner };
            match d {
                Row::Head(g) => {
                    f.render_widget(Paragraph::new(Span::styled(format!("  ── {g}"), ui::muted(t))), r);
                }
                Row::It(k) => {
                    let it = &p.items[m[*k]];
                    let on = *k == p.sel;
                    let style = if on { Style::default().fg(t.accent).add_modifier(Modifier::BOLD) } else { Style::default() };
                    // the key right-aligned in muted text, like the sidebar's rows
                    let kw = unicode_width::UnicodeWidthStr::width(it.key.as_str());
                    let lead = ui::lead(it.icon);
                    let lw = (r.width as usize).saturating_sub(2 + kw + 2 + unicode_width::UnicodeWidthStr::width(lead.as_str()));
                    let label = ui::fit(&it.label, lw);
                    let pad = lw.saturating_sub(unicode_width::UnicodeWidthStr::width(label.as_str())) + 1;
                    let line = Line::from(vec![
                        Span::styled(if on { "▌ " } else { "  " }, ui::accent(t)),
                        Span::styled(lead, if on { style } else { ui::accent(t) }),
                        Span::styled(label, style),
                        Span::raw(" ".repeat(pad)),
                        Span::styled(it.key.clone(), if on { style } else { ui::muted(t) }),
                    ]);
                    f.render_widget(Paragraph::new(line), r);
                    p.rows.push((r, *k));
                }
            }
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
mod qa_keys;

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
        assert!(s.contains("/perms ask") && s.contains("getting started") && s.contains("more"), "{s}");
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
        assert!(app.last_notice().contains("finished"));
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
        assert_eq!(app.config.startup, "ai", "the startup app you picked (one on from \"where you left off\")");
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
        assert_eq!(disk.startup, Config::default().startup, "`oriel music` wasn't saved as the startup app");
        assert_eq!(app.config, disk, "the app's copy is what's on disk");
        // the icons toggle is saved too (the config says plain, the screen shows glyphs: make it glyphs)
        config::save_to(&p, &Config { plain_icons: true, ..disk }).unwrap();
        app.config.plain_icons = true;
        app.set_icons(true);
        assert!(!config::load_from(&p).unwrap().plain_icons, "remembered");
        app.open_palette();
        let item = app.palette.as_ref().unwrap().items.iter().find(|i| i.label.contains("toggle nerd font icons")).map(|i| i.icon).unwrap();
        assert_eq!(item, "theme");
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
        assert!(app.last_notice().contains("line 2"));
        app.run_cmd(Cmd::Theme("ocean".into()));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), broken, "never written over");
        assert!(app.last_notice().contains("not saved"));
        assert_eq!(app.theme.name, "ocean", "it still applies for now");
        // fixed: picked up, and saving works again
        std::fs::write(&p, "theme = \"dracula\"\nprefix = \"ctrl+b\"\n").unwrap();
        app.reload_config();
        assert!(config::broken().is_none() && app.config.prefix == "ctrl+b");
        assert!(app.last_notice().contains("reads fine again"));
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
        let item = app.palette.as_ref().unwrap().items.iter().find(|i| i.label.contains("setting: permissions · edits")).map(|i| i.cmd.clone()).expect("a palette row per setting");
        app.palette = None;
        app.run_cmd(item);
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
        assert!(app.last_notice().contains("claud isn't an app oriel has"));
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
        assert_eq!(app.last_notice(), "copied 8 characters", "'a window' copied");
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
        assert!(app.last_notice().contains("pasted image"));
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
        let tid = app.tab_id();
        app.tabs.push(Tab { id: tid, app: Some("music"), name: None, root: Node::Leaf(id), focus: id, zoom: false });
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
        let items: Vec<String> = app.palette.as_ref().unwrap().items.iter().map(|i| i.label.clone()).collect();
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

    /// alt r opens the search app from anywhere (the palette has it too), and AppPaste lands text in an app's box and
    /// switches to it.
    #[test]
    fn app_search_alt_r_and_app_paste() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut cfg = Config::default();
        cfg.theme = "oriel".into();
        let mut app = App::new(cfg, tx);
        app.new_tab(Box::new(crate::panes::home::Home::new()));
        app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT));
        assert_eq!(app.tabs[app.cur].app, Some("search"));
        let s = snap(&mut app, "search");
        assert!(s.contains("search every AI session") && s.contains("what did we decide about"), "{s}");
        assert!(!app.user_tabs().contains(&app.cur), "it isn't one of your tabs");
        app.goto_app("files");
        app.open_palette();
        if let Some(p) = &mut app.palette {
            p.query = "search chats".into();
        }
        assert!(snap(&mut app, "search-palette").contains("search chats: every AI session"));
        app.palette_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.tabs[app.cur].app, Some("search"), "the palette goes back to the same search");
        assert_eq!(app.tabs.iter().filter(|t| t.app == Some("search")).count(), 1);
        // AppPaste: into the chat's composer, and the chat is where you land
        let from = app.focused();
        app.apply(from, vec![Action::AppPaste("ai", "(from a codex session) use a token bucket".into())]);
        assert_eq!(app.tabs[app.cur].app, Some("ai"));
        assert!(snap(&mut app, "search-paste").contains("use a token bucket"));

        // `oriel search` starts on that same app tab, so alt r doesn't make a second one
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut cfg = Config::default();
        cfg.startup = "search".into();
        let mut app = App::new(cfg, tx);
        assert_eq!((app.tabs[app.cur].app, app.tabs.len()), (Some("search"), 1));
        app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT));
        assert_eq!(app.tabs.len(), 1);
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
        dir: Rc<RefCell<Option<std::path::PathBuf>>>,
        /// what it lists as open now, what it was answered with, and whether it takes the answer
        open: Rc<RefCell<Vec<crate::alerts::Open>>>,
        replies: Rc<RefCell<Vec<(String, crate::alerts::Reply)>>>,
        refuse: Rc<Cell<bool>>,
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
        fn cwd(&self) -> Option<std::path::PathBuf> {
            self.ctl.dir.borrow().clone()
        }
        fn open_now(&self) -> Vec<crate::alerts::Open> {
            self.ctl.open.borrow().clone()
        }
        fn respond(&mut self, key: &str, r: crate::alerts::Reply, _cx: &mut Cx) -> bool {
            self.ctl.replies.borrow_mut().push((key.to_string(), r));
            !self.ctl.refuse.get()
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
        app.last_notice()
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
        assert!(!app.palette.as_ref().unwrap().items.iter().any(|it| it.label.contains("close pane") || it.label.contains("close this tab")));
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
        app.notices.clear();
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
        // a theme previewed with the wheel, then a click on a row that isn't a theme: the theme goes back
        app.open_palette();
        {
            let p = app.palette.as_mut().unwrap();
            let m = App::palette_matches(p);
            p.sel = m.iter().position(|&i| matches!(&p.items[i].cmd, Cmd::Theme(t) if t == "ocean")).unwrap();
        }
        app.palette_preview();
        assert_eq!(app.theme.name, "ocean");
        screen(&mut app, 150, 42);
        let p = app.palette.as_ref().unwrap();
        let m = App::palette_matches(p);
        let (r, _) = *p.rows.iter().find(|(_, k)| matches!(p.items[m[*k]].cmd, Cmd::Sidebar)).expect("toggle sidebar drawn near the themes");
        let side = app.sidebar;
        app.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: r.x + 2, row: r.y, modifiers: KeyModifiers::NONE });
        assert!(app.palette.is_none() && app.sidebar != side, "clicked row ran");
        assert_eq!(app.theme.name, "oriel", "the preview didn't stay on unsaved");
        app.sidebar = side;
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
        // a close/quit question left unanswered goes away, and stops waking the loop
        app.confirm = Some((Doom::Quit, Instant::now().checked_sub(CONFIRM_FOR + Duration::from_secs(1)).unwrap()));
        app.reap();
        assert!(app.confirm.is_none() && app.next_deadline() > Duration::from_secs(1));
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
        // the quiet window is far longer than the gaps, so a busy machine stretching a 5 ms sleep (Windows timers
        // alone round it up to ~16 ms) can't split the burst in two
        let quiet = Duration::from_millis(400);
        let (tx, rx) = std::sync::mpsc::channel();
        let ping = debounced(tx, quiet);
        for _ in 0..8 {
            ping.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(rx.recv_timeout(Duration::from_secs(5)), Ok(Event::ThemeFilesChanged)));
        assert!(rx.recv_timeout(quiet * 2).is_err(), "just the one");
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

    // ------------------------------------------------------------------ stage 2: getting around
    #[test]
    fn app_session_comes_back() {
        let dir = std::path::absolute("target/test-scratch/shell/session-proj").unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let (mut app, _rx) = new_app();
        assert_eq!(app.tabs[app.cur].app, Some("ai"), "nothing saved (a first start): chat");
        // "work": home | files in the project (focused), and under files a pane that can't come back
        app.new_tab(Box::new(crate::panes::home::Home::new()));
        let home = app.focused();
        app.open(Box::new(crate::panes::files::Files::new(Some(dir.clone()))), Place::SplitRight, home);
        let files = app.focused();
        let (p, _) = fake(true, false);
        app.open(p, Place::SplitDown, files);
        app.tabs[app.cur].focus = files;
        app.tabs[app.cur].name = Some("work".into());
        let work = app.tabs[app.cur].id;
        // an orchestrator's worker tab: its app brings those back itself
        let (p, _) = fake(true, false);
        app.apply(home, vec![Action::OpenTagged { pane: p, tag: "w1".into(), name: "w1".into(), focus: false }]);
        // and a terminal in the project
        app.new_tab(Box::new(crate::panes::term::Term::shell(&app.config, Some(dir.clone()))));
        app.cur = app.tab_index(work).unwrap();
        app.sidebar = false;
        let s = app.session();
        assert_eq!((s.tabs.len(), s.app.as_deref(), s.tab, s.sidebar), (2, None, Some(0), false), "{s:?}");
        // through JSON into a fresh start
        let json = serde_json::to_string(&s).unwrap();
        let (mut back, _rx) = new_app();
        assert!(back.restore(serde_json::from_str(&json).unwrap()));
        let mine = back.user_tabs();
        assert_eq!(mine.len(), 2);
        let t = &back.tabs[back.cur];
        assert_eq!(t.name.as_deref(), Some("work"), "back on the tab you were in");
        let mut l = vec![];
        t.root.leaves(&mut l);
        assert!(l.len() == 2 && matches!(t.root, Node::Split { dir: Dir::Right, .. }), "home | files, the lost pane's place taken");
        assert_eq!((back.panes[&t.focus].reopen(), back.panes[&t.focus].cwd()), (Some("files"), Some(dir.clone())), "files, focused, in its folder");
        let term = back.tabs[mine[1]].focus;
        assert_eq!((back.panes[&term].reopen(), back.panes[&term].cwd()), (Some("terminal"), Some(dir.clone())), "a fresh shell in the saved folder");
        assert!(!back.sidebar);
        // an app tab you were in comes back as that app
        back.goto_app("help");
        let (mut again, _rx) = new_app();
        again.restore(back.session());
        assert_eq!((again.tabs[again.cur].app, again.user_tabs().len()), (Some("help"), 2));
        // nothing in it can come back: not restored, even with a tab already open (chat, here; at a real start
        // the calendar for a coming reminder), so the start goes to chat as on a first run
        let (mut none, _rx) = new_app();
        let lost = crate::session::Session { tabs: vec![crate::session::Tab { name: None, root: crate::session::Node::Leaf { kind: "uninstalled-agent".into(), cwd: None, id: None }, focus: 0 }], ..Default::default() };
        assert!(!none.restore(lost));
    }

    #[test]
    fn app_alt_n_zoom_and_close_follow_you() {
        let dir = std::path::absolute("target/test-scratch/shell/alt-n here").unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let (mut app, _rx) = new_app();
        let (p, a) = fake(false, false);
        *a.dir.borrow_mut() = Some(dir.clone());
        app.new_tab(p);
        let first = app.focused();
        screen(&mut app, 150, 42);
        press(&mut app, KeyCode::Char('n'), KeyModifiers::ALT);
        let term = app.focused();
        assert_ne!(term, first);
        assert_eq!(app.panes[&term].cwd(), Some(dir.clone()), "the terminal starts in the folder of the pane you were in");
        // zoomed: the frame says so, and alt ← unzooms onto the neighbour
        press(&mut app, KeyCode::Char('z'), KeyModifiers::ALT);
        assert!(shot(&mut app, 150, 42, "zoomed").contains("zoomed"));
        press(&mut app, KeyCode::Left, KeyModifiers::ALT);
        assert_eq!((app.focused(), app.tabs[app.cur].zoom), (first, false));
        assert!(!screen(&mut app, 150, 42).contains("zoomed"));
        // closing a pane focuses what took its place: first | (term / x), closing term lands on x, not first
        let (p, _) = fake(false, false);
        app.open(p, Place::SplitDown, term);
        let x = app.focused();
        app.tabs[app.cur].focus = term;
        app.close(term);
        assert_eq!(app.focused(), x);
    }

    #[test]
    fn app_getting_to_what_needs_you() {
        let (mut app, _rx) = new_app();
        crate::alerts::clear();
        let (p, a1) = fake(true, false);
        app.new_tab(p);
        let (t1, p1) = (app.tabs[app.cur].id, app.focused());
        let (p, a2) = fake(true, false);
        app.new_tab(p);
        let t2 = app.tabs[app.cur].id;
        app.new_tab(Box::new(crate::panes::home::Home::new()));
        let home = app.tabs[app.cur].id;
        press(&mut app, KeyCode::Char('2'), KeyModifiers::ALT);
        press(&mut app, KeyCode::Char('3'), KeyModifiers::ALT);
        // nothing going on: alt j says so, alt 0 flips between the last two tabs
        press(&mut app, KeyCode::Char('j'), KeyModifiers::ALT);
        assert!(app.last_notice().contains("nothing else needs you"));
        press(&mut app, KeyCode::Char('0'), KeyModifiers::ALT);
        assert_eq!(app.tabs[app.cur].id, t2, "back to where you were");
        press(&mut app, KeyCode::Char('0'), KeyModifiers::ALT);
        assert_eq!(app.tabs[app.cur].id, home, "and back again");
        // an agent gets stuck while you're elsewhere: the toast says how to get there, the title counts it
        a1.act.set(Some(Activity::Working));
        a2.act.set(Some(Activity::Working));
        app.track_agents();
        a1.act.set(Some(Activity::Blocked));
        app.track_agents();
        assert!(app.last_notice().contains("needs you") && app.last_notice().ends_with("· alt j"), "{}", app.last_notice());
        assert_eq!(app.title_text(), "oriel · 1 needs you · 1 working");
        press(&mut app, KeyCode::Char('j'), KeyModifiers::ALT);
        assert_eq!((app.tabs[app.cur].id, app.focused()), (t1, p1), "alt j went to it");
        assert_eq!(crate::alerts::with(|l| l.iter().filter(|x| x.pane == Some(p1) && !x.read).count()), 0, "and marked it read");
        // the other one finishes: alt j again goes on to it (a green dot)
        a2.act.set(Some(Activity::Idle));
        app.track_agents();
        press(&mut app, KeyCode::Char('j'), KeyModifiers::ALT);
        assert_eq!(app.tabs[app.cur].id, t2, "then the one that finished");
        // alt 0 from there: the stuck one (round the list); prefix ; flips back
        press(&mut app, KeyCode::Char('0'), KeyModifiers::ALT);
        assert_eq!(app.tabs[app.cur].id, t1);
        prefix(&mut app);
        press(&mut app, KeyCode::Char(';'), KeyModifiers::NONE);
        assert_eq!(app.tabs[app.cur].id, t2);
        // F10 opens help, F10 again comes back
        press(&mut app, KeyCode::F(10), KeyModifiers::NONE);
        assert_eq!(app.tabs[app.cur].app, Some("help"));
        press(&mut app, KeyCode::F(10), KeyModifiers::NONE);
        assert_eq!(app.tabs[app.cur].id, t2, "F10 again goes back");
        // prefix digits count your tabs, like alt does
        prefix(&mut app);
        press(&mut app, KeyCode::Char('1'), KeyModifiers::NONE);
        assert_eq!(app.tabs[app.cur].id, t1, "ctrl+space 1 = your first tab, not the chat app");
        // prefix then an alt key goes to the program (Claude Code's own alt p), not to oriel's palette
        prefix(&mut app);
        press(&mut app, KeyCode::Char('p'), KeyModifiers::ALT);
        assert!(app.palette.is_none());
        assert_eq!(a1.keys.borrow().last().map(|k| (k.code, k.modifiers)), Some((KeyCode::Char('p'), KeyModifiers::ALT)));
        a1.act.set(None);
        a2.act.set(None);
        assert_eq!(app.title_text(), "oriel");
        // closing the tab you're on takes you back to the one before it
        press(&mut app, KeyCode::Char('3'), KeyModifiers::ALT);
        press(&mut app, KeyCode::Char('2'), KeyModifiers::ALT);
        app.request_close_tab(t2);
        assert_eq!(app.tabs[app.cur].id, home);
    }

    #[test]
    fn app_toasts_stack_top_right_and_click_through() {
        let (mut app, _rx) = new_app();
        let (p, a) = fake(true, false);
        app.new_tab(p);
        let (agent_tab, agent) = (app.tabs[app.cur].id, app.focused());
        app.goto_app("help");
        app.notify("older one");
        app.notify("old one");
        a.act.set(Some(Activity::Working));
        app.track_agents();
        a.act.set(Some(Activity::Idle));
        app.track_agents();
        let mut term = Terminal::new(TestBackend::new(150, 42)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        crate::testkit::save_html(term.backend().buffer(), "target/snap/app-toast-finished.html");
        let (r, _) = app.toast_hits[0];
        assert_eq!(r.y, app.body.y + 1, "under the frame's title, clear of the composer and hints at the bottom");
        assert!(r.right() <= app.body.right() && r.x > app.body.x + app.body.width / 2, "on the right");
        let b = term.backend().buffer();
        assert_eq!(b[(r.x, r.y)].fg, app.theme.good, "a finished agent's colour");
        let row: String = (r.x..r.right()).map(|x| b[(x, r.y)].symbol()).collect();
        assert!(row.contains(" finished "), "titled with the kind: {row}");
        // a click on it goes to that agent, even with a toast in since it was drawn (the oldest went to keep three,
        // so the drawn one's place in the list moved)
        app.notify("late one");
        app.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: r.x + 3, row: r.y + 1, modifiers: KeyModifiers::NONE });
        assert_eq!((app.tabs[app.cur].id, app.focused()), (agent_tab, agent));
        assert!(!app.notices.iter().any(|n| n.text.contains("finished")), "and puts it away");
        app.notices.clear();
        // three at most, newest on top, a failure in its own colour
        app.raise(crate::alerts::Kind::BuildFailed, "build failed · w1".into(), None, None);
        let s = shot(&mut app, 150, 42, "toast-failed");
        assert!(s.contains(" failed ") && s.contains("× build failed · w1"), "{s}");
        for n in ["plain info", "one more", "newest"] {
            app.notify(n);
        }
        let s = shot(&mut app, 150, 42, "toasts");
        assert!(!s.contains("build failed"), "the oldest went: {s}");
        let rows: Vec<&str> = s.lines().collect();
        let y = |needle: &str| rows.iter().position(|l| l.contains(needle)).unwrap();
        assert!(y("newest") < y("one more") && y("one more") < y("plain info"), "{s}");
        assert!(s.contains("│ plain info │"), "a column of padding each side: {s}");
    }

    #[test]
    fn app_palette_ranks_and_shows_keys() {
        let (mut app, _rx) = new_app();
        app.new_tab(Box::new(crate::panes::home::Home::new()));
        app.tabs[app.cur].name = Some("refactor".into());
        app.open_palette();
        let top = |app: &mut App, q: &str| -> (String, String) {
            let p = app.palette.as_mut().unwrap();
            p.query = q.into();
            App::palette_requery(p);
            let m = App::palette_matches(p);
            let it = &p.items[m[p.sel]];
            (it.label.clone(), it.key.clone())
        };
        assert!(top(&mut app, "theme").0.starts_with("theme "), "typing theme picks a theme, not 'go to themes'");
        assert_eq!(top(&mut app, "refac"), ("tab: refactor".to_string(), "alt 1".to_string()), "your tabs, with their key");
        assert_eq!(top(&mut app, "alt z").0, "zoom pane", "keys count too");
        assert_eq!(top(&mut app, "sidebar"), ("toggle sidebar".to_string(), "alt s".to_string()));
        assert_eq!(top(&mut app, "split right").1, "ctrl+space |", "the prefix keys, from the config");
        assert_eq!(top(&mut app, "config").0, "open config.toml");
        assert_eq!(top(&mut app, "split: open chat").0, "split: open chat beside this", "one name for chat");
        // a key matches as a run of letters, not scattered ones ("ctrl+space |" has most of the alphabet)
        let p = app.palette.as_mut().unwrap();
        p.query = "trlspc".into();
        // (a setting whose value is ctrl+space matches by its own text, which is fine: no key matched)
        let by_key: Vec<&String> = App::palette_matches(p).iter().filter(|&&i| !matches!(p.items[i].cmd, Cmd::Setting(_))).map(|&i| &p.items[i].label).collect();
        assert!(by_key.is_empty(), "{by_key:?}");
        app.palette = None;
        // prefix t starts on the theme you have, so the preview moves on from it
        prefix(&mut app);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::NONE);
        let p = app.palette.as_ref().unwrap();
        let m = App::palette_matches(p);
        assert!(matches!(&p.items[m[p.sel]].cmd, Cmd::Theme(t) if t == "oriel"));
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_ne!(app.theme.name, "oriel", "the next one previews");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.theme.name, "oriel");
        // drawn: headings, keys right-aligned, a count and the hints
        app.open_palette();
        let s = shot(&mut app, 150, 42, "palette-keys");
        assert!(s.contains("── apps") && s.contains(&format!("1/{}", App::palette_matches(app.palette.as_ref().unwrap()).len())) && s.contains("enter run · esc close"), "{s}");
        let row = s.lines().find(|l| l.contains("go to chat")).unwrap();
        let after = &row[row.find("go to chat").unwrap() + "go to chat".len()..];
        let k = after.find("F1").unwrap();
        assert!(after[..k].trim().is_empty() && after[k + 2..].starts_with(" │"), "F1 right-aligned: {row}");
    }

    #[test]
    fn app_sidebar_fits_small_windows() {
        let (mut app, _rx) = new_app();
        for i in 0..12 {
            app.new_tab(Box::new(crate::panes::home::Home::new()));
            app.tabs[app.cur].name = Some(format!("task {i}"));
        }
        app.goto_app("ais");
        // Windows Terminal's default size: the app list folds its headings so tabs keep room
        let s = shot(&mut app, 120, 30, "sidebar-120x30");
        assert!(s.contains("your AIs ╯") && !s.contains(" ais ╯"), "the sidebar names the app as its list does: {s}");
        assert!(!s.contains("── tools"), "headings dropped: {s}");
        assert!(s.contains("task 0") && s.contains("+9 more") && s.contains("storage"), "every app still listed: {s}");
        assert!(!s.contains("alt 10"), "alt only reaches 1-9");
        // the wheel scrolls your tabs; "+N more" pages on
        let r = app.tabs_rect;
        app.mouse(MouseEvent { kind: MouseEventKind::ScrollDown, column: r.x + 3, row: r.y, modifiers: KeyModifiers::NONE });
        let s = screen(&mut app, 120, 30);
        assert!(!s.contains("task 0") && s.contains("task 1"), "{s}");
        let (m, _) = app.side_hits.iter().copied().find(|(_, h)| matches!(h, SideHit::MoreTabs)).unwrap();
        app.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: m.x + 3, row: m.y, modifiers: KeyModifiers::NONE });
        assert!(screen(&mut app, 120, 30).contains("task 5"));
        assert_eq!(app.tab_label(app.cur), "your AIs", "an app tab by the sidebar's name (toasts, the open-now list)");
        // renaming a tab scrolled out of view: the list scrolls back to it, and the field is in its row
        let t0 = app.user_tabs()[0];
        app.cur = t0;
        app.tab_scrolled_for = app.tabs[t0].id; // arrived before the wheel scrolled away from it
        assert!(!screen(&mut app, 120, 30).contains("task 0"));
        app.start_rename(t0);
        let s = screen(&mut app, 120, 30);
        assert!(s.contains("task 0▏") && !s.contains("rename tab"), "{s}");
        // no room for any tab row: the field comes up in a box instead of taking keys unseen
        let s = shot(&mut app, 120, 9, "rename-no-room");
        assert!(s.contains("rename tab") && s.contains("task 0▏"), "{s}");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        app.goto_app("ais");
        // shorter still: the tools fold into one row of icons, and the new-alerts count moves to the title
        app.raise(crate::alerts::Kind::Update, "something new".into(), None, None);
        let s = shot(&mut app, 120, 16, "sidebar-120x16");
        assert!(s.lines().next().unwrap().contains("oriel · "), "{s}");
        assert!(!s.contains("storage"), "{s}");
        assert!(app.side_hits.iter().any(|(_, h)| matches!(h, SideHit::App("storage"))), "still clickable");
        // too narrow for the sidebar: the tabs go in a row across the top, and a click on one goes there
        let (mut app, _rx) = new_app();
        for n in ["api", "docs"] {
            app.new_tab(Box::new(crate::panes::home::Home::new()));
            app.tabs[app.cur].name = Some(n.into());
        }
        let s = shot(&mut app, 60, 20, "tab-strip");
        let top = s.lines().next().unwrap();
        assert!(top.contains("api") && top.contains("docs"), "{s}");
        let (r, _) = app.side_hits.iter().copied().find(|(_, h)| matches!(h, SideHit::Tab(_))).unwrap();
        app.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: r.x + 1, row: r.y, modifiers: KeyModifiers::NONE });
        assert_eq!(app.tabs[app.cur].name.as_deref(), Some("api"));
    }

    #[test]
    fn app_send_things_to_chat() {
        let (mut app, _rx) = new_app();
        // a stand-in for the chat app's pane, to see what it's handed
        let chat = app.tabs.iter().find(|t| t.app == Some("ai")).unwrap().focus;
        let (p, c) = fake(false, false);
        app.panes.insert(chat, p);
        app.new_tab(Box::new(crate::panes::home::Home::new()));
        // select the tagline, right-click it: ask chat, copy, paste
        let (col, row) = find(&mut app, 150, 42, "a window onto everything").unwrap();
        let ev = |app: &mut App, kind, x| app.mouse(MouseEvent { kind, column: x, row, modifiers: KeyModifiers::NONE });
        ev(&mut app, MouseEventKind::Down(MouseButton::Left), col);
        ev(&mut app, MouseEventKind::Drag(MouseButton::Left), col + 7);
        ev(&mut app, MouseEventKind::Up(MouseButton::Left), col + 7);
        screen(&mut app, 150, 42);
        ev(&mut app, MouseEventKind::Down(MouseButton::Right), col + 2);
        let items: Vec<String> = app.ctx.as_ref().unwrap().items.iter().map(|x| x.0.clone()).collect();
        assert!(items[0].ends_with("ask chat about this") && items[1].ends_with("copy") && items[2].ends_with("paste"), "{items:?}");
        let s = shot(&mut app, 150, 42, "ctx-selection");
        assert!(s.contains("ask chat about this"));
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.tabs[app.cur].app, Some("ai"), "chat comes up");
        assert_eq!(c.pastes.borrow().last().map(String::as_str), Some("```\na window\n```\n\n"), "as a code block, room for the question after");
        // no selection: just paste (and the usual)
        app.cur = app.user_tabs()[0];
        screen(&mut app, 150, 42);
        ev(&mut app, MouseEventKind::Down(MouseButton::Right), col + 2);
        let items: Vec<String> = app.ctx.as_ref().unwrap().items.iter().map(|x| x.0.clone()).collect();
        assert!(items[0].ends_with("paste") && !items.iter().any(|i| i.contains("ask chat")), "{items:?}");
        app.ctx = None;
        // a note, from the palette
        app.goto_app("notes");
        app.palette_with("send this note");
        let text = app.panes[&app.focused()].for_chat().unwrap().1;
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(c.pastes.borrow().last(), Some(&text));
        assert_eq!(app.tabs[app.cur].app, Some("ai"));
    }

    #[test]
    fn app_tour_says_your_prefix() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut cfg = Config::default();
        cfg.theme = "oriel".into();
        cfg.prefix = "ctrl+b".into();
        let mut app = App::new(cfg, tx);
        app.start_tour();
        let probe = app.probe();
        let step = 8; // "name the tab"
        app.onboard.as_mut().unwrap().stage = crate::onboard::Stage::Tour { step, start: probe };
        let s = screen(&mut app, 150, 42);
        assert!(s.contains("ctrl+b then ,") && !s.contains("ctrl+space"), "{s}");
    }

    /// The open-now list: gathered from every pane before each draw (a red-dot terminal with nothing of its own
    /// gets a plain row, a task's terminal isn't listed twice), counted on the focused frame, answered from the
    /// alerts app through the pane that holds it, and enter goes there.
    #[test]
    fn app_open_now_from_every_pane() {
        use crate::alerts::{Kind, Open, Reply};
        let (mut app, _rx) = new_app();
        crate::alerts::clear();
        // a pane asking a question, and a terminal agent stuck at a prompt, each in a tab of its own
        let (p, asker) = fake(false, false);
        let mut q = Open::new(Kind::Approval, "q:which", "claude asks: which parser?");
        q.options = vec!["nom".into(), "winnow".into()];
        asker.open.borrow_mut().push(q);
        app.new_tab(p);
        let (asker_tab, asker_pane) = (app.tabs[app.cur].id, app.focused());
        let (p, stuck) = fake(true, false);
        stuck.act.set(Some(Activity::Blocked));
        app.new_tab(p);
        let (stuck_tab, stuck_pane) = (app.tabs[app.cur].id, app.focused());
        app.new_tab(Box::new(crate::panes::home::Home::new()));
        let s = shot(&mut app, 150, 42, "need-you-footer");
        assert_eq!(app.title_text(), "oriel · 2 needs you", "the question counts in the window title too");
        let rows = crate::alerts::with_open(|l| l.iter().map(|o| (o.text.clone(), o.pane)).collect::<Vec<_>>());
        assert_eq!(rows, [("claude asks: which parser?".to_string(), asker_pane), ("fake agent is waiting for you".to_string(), stuck_pane)]);
        assert!(s.contains("← 2 need you"), "on the focused frame: {s}");
        assert!(s.lines().any(|l| l.contains("alerts ") && l.contains("2 need you")), "and on the bell, name whole: {s}");
        // alt j goes to the question (no alert was raised for it), then the stuck one
        press(&mut app, KeyCode::Char('j'), KeyModifiers::ALT);
        assert_eq!(app.tabs[app.cur].id, asker_tab);
        let s = screen(&mut app, 150, 42);
        assert!(s.contains("← 1 needs you"), "what you're looking at isn't counted: {s}");
        press(&mut app, KeyCode::Char('j'), KeyModifiers::ALT);
        assert_eq!(app.tabs[app.cur].id, stuck_tab);
        // a click on the count opens the alerts app, which lists them (and doesn't count itself)
        screen(&mut app, 150, 42);
        let r = app.need_hit.expect("drawn");
        app.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: r.x + 2, row: r.y, modifiers: KeyModifiers::NONE });
        assert_eq!(app.tabs[app.cur].app, Some("alerts"));
        let s = shot(&mut app, 150, 42, "alerts-open-now");
        assert!(s.contains("open now") && s.contains("which parser?") && s.contains("fake agent is waiting for you") && !s.contains("← "), "{s}");
        // 2 answers the question through its pane, without leaving alerts
        press(&mut app, KeyCode::Char('2'), KeyModifiers::NONE);
        assert_eq!(asker.replies.borrow().as_slice(), [("q:which".to_string(), Reply::Pick(1))]);
        assert_eq!(app.tabs[app.cur].app, Some("alerts"));
        // the pane says it has changed since: the alerts app says so
        asker.refuse.set(true);
        press(&mut app, KeyCode::Char('1'), KeyModifiers::NONE);
        assert!(notice(&app).contains("changed since"), "{}", notice(&app));
        // enter on the stuck terminal goes to it
        asker.open.borrow_mut().clear();
        screen(&mut app, 150, 42);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!((app.tabs[app.cur].id, app.focused()), (stuck_tab, stuck_pane));
        // a task's terminal tab and the agents' row for that task: listed once, with the screen in the peek
        let (p, task_term) = fake(true, false);
        let mut screen_row = Open::new(Kind::NeedsYou, "term", "claude is waiting for you");
        screen_row.detail = vec!["Do you want to proceed?".into()];
        task_term.open.borrow_mut().push(screen_row);
        app.apply(stuck_pane, vec![Action::OpenTagged { pane: p, tag: "agent-task:t1".into(), name: "fix tests".into(), focus: false }]);
        let mut row = Open::new(Kind::NeedsYou, "task:t1", "fix tests needs you");
        row.detail = vec!["waiting for you in its tab".into()];
        row.tag = Some("agent-task:t1".into());
        asker.open.borrow_mut().push(row);
        screen(&mut app, 150, 42);
        let rows = crate::alerts::with_open(|l| l.iter().map(|o| (o.text.clone(), o.detail.clone())).collect::<Vec<_>>());
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0], ("fix tests needs you".to_string(), vec!["waiting for you in its tab".to_string(), String::new(), "Do you want to proceed?".to_string()]));
    }

    /// The terminal saying oriel's window lost or got the focus reaches the app (and the panes): desktop
    /// notifications depend on it.
    #[test]
    fn app_window_focus() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(Config::default(), tx);
        app.handle(Event::Input(CEvent::FocusLost));
        let lost = (app.term_focused, term_focused());
        app.handle(Event::Input(CEvent::FocusGained));
        assert_eq!(lost, (false, false));
        assert!(app.term_focused && term_focused());
    }
}

#[cfg(test)]
#[path = "qa_sizes_app.rs"]
mod qa_sizes_app;
#[cfg(test)]
#[path = "qa_first_run_app.rs"]
mod qa_first_run_app;
