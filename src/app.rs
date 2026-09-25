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
    widgets::{Paragraph, Wrap},
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
    ("storage", "storage", "storage", "F8"),
    ("terminal", "term", "terminal", "F9"),
];
/// Where each sidebar section starts: (index into SIDEBAR, heading).
const SECTIONS: &[(usize, &str)] = &[(0, "ai"), (3, "tools")];

#[derive(Clone, Copy)]
enum SideHit {
    App(&'static str),
    Tab(usize),
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
    SplitRight,
    SplitDown,
    Close,
    Zoom,
    NewTab,
    Icons,
    Help,
    Quit,
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
    help: bool,
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
            help: false,
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
            _watcher: None,
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
        app._watcher = watch_omarchy(tx);
        if !cfg!(test) && !crate::onboard::done_before() {
            app.onboard = Some(crate::onboard::Onboard::new(&app.theme.name, &app.config));
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
        self.tabs.push(Tab { app: None, name: None, root: Node::Leaf(id), focus: id, zoom: false });
        self.cur = self.tabs.len() - 1;
    }

    /// Show an app's tab, creating it the first time (like nest's F1-F6).
    fn goto_app(&mut self, name: &'static str) {
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
                // last pane in the tab: drop the tab
                self.tabs.remove(ti);
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

    fn set_theme(&mut self, name: &str, save: bool) {
        self.theme = theme::get(name);
        if save {
            self.config.theme = name.to_string();
            config::save(&self.config);
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
            self.track_agents();
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
        if self.theme.animated {
            d = d.min(Duration::from_millis(125));
        }
        if let Some((_, t)) = &self.notice {
            d = d.min(Duration::from_secs(4).saturating_sub(t.elapsed()) + Duration::from_millis(10));
        }
        let mut ids = self.visible();
        ids.extend(self.panes.iter().filter(|(_, p)| p.is_terminal()).map(|(id, _)| *id));
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
                notes.push(format!("{} finished · {where_}", p.title()));
            }
            if a == Activity::Blocked && prev != Some(Activity::Blocked) && !seen {
                notes.push(format!("{} needs you · {where_}", p.title()));
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
        if let Some(n) = notes.pop() {
            self.notify(n);
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
                Action::SetTheme(t) => {
                    if theme::names().iter().any(|n| *n == t) {
                        self.set_theme(&t, true);
                        self.notify(format!("{}theme: {t}", ui::lead("theme")));
                    } else {
                        self.notify(format!("no theme called {t} — /theme lists them"));
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
                    let here = self.cur;
                    self.goto_app(a); // opens it if it isn't yet
                    self.cur = here;
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
                Action::ToggleSidebar => self.sidebar = !self.sidebar,
                Action::ToggleIcons => self.run_cmd(Cmd::Icons),
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
            Event::Input(_) => {}
            Event::Wake(id) => {
                self.with_pane(id, |p, cx| p.poll(cx));
            }
            Event::ThemeFilesChanged => {
                if self.theme.name == "omarchy" {
                    std::thread::sleep(Duration::from_millis(150)); // let omarchy finish swapping files
                    self.set_theme("omarchy", false);
                    self.notify(format!("{}omarchy theme updated", ui::lead("theme")));
                }
            }
            Event::Tick => {
                let mut ids = self.visible();
                ids.extend(self.panes.iter().filter(|(id, p)| p.is_terminal() && !ids.contains(id)).map(|(id, _)| *id).collect::<Vec<_>>());
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
        if self.help {
            self.help = false;
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
            if (1..=SIDEBAR.len() as u8).contains(&n) && k.modifiers.is_empty() {
                self.goto_app(SIDEBAR[n as usize - 1].0);
                return;
            }
            if n == 12 {
                // play/pause from anywhere, if the music app is open
                if let Some(id) = self.tabs.iter().find(|t| t.app == Some("music")).map(|t| t.focus) {
                    self.with_pane(id, |p, cx| p.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), cx));
                }
                return;
            }
        }
        let id = self.focused();
        let used = self.with_pane(id, |p, cx| p.key(k, cx)).unwrap_or(false);
        if !used && !self.panes.get(&id).map(|p| p.is_terminal()).unwrap_or(false) {
            // unused keys in app panes
            if let KeyCode::Char('?') = k.code {
                self.help = true;
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
            KeyCode::Char('?') => self.help = true,
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
            Cmd::Close => self.close(from),
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
            Cmd::Help => self.help = true,
            Cmd::Tour => self.start_tour(),
            Cmd::Quit => self.quit = true,
        }
    }

    // ------------------------------------------------------------------ palette
    fn open_palette(&mut self) {
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
        items.push((format!("{}close pane", ui::lead("close")), Cmd::Close));
        items.push((format!("{}new tab", ui::lead("tab")), Cmd::NewTab));
        items.push((format!("{}rename this tab", ui::lead("tab")), Cmd::Rename));
        items.push((format!("{}close this tab", ui::lead("close")), Cmd::CloseTab(self.cur)));
        for t in theme::names() {
            items.push((format!("{}theme {t}", ui::lead("theme")), Cmd::Theme(t)));
        }
        items.push(("toggle nerd font icons".into(), Cmd::Icons));
        items.push(("key bindings".into(), Cmd::Help));
        items.push((format!("{}take the tour", ui::lead("window")), Cmd::Tour));
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
        if let MouseEventKind::Down(MouseButton::Left) = m.kind {
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
        // pane under the cursor
        let Some(&(id, _)) = self.outer.iter().find(|(_, r)| r.contains(pos)) else { return };
        let inner = self.inner.iter().find(|(i, _)| *i == id).map(|x| x.1).unwrap_or_default();
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
        self.outer = rects.clone();
        self.inner.clear();
        let time = self.start.elapsed().as_secs_f64();
        for (id, r) in rects {
            let Some(p) = self.panes.get(&id) else { continue };
            let title = format!("{}{}", ui::lead(p.icon()), p.title());
            let sub = p.subtitle();
            let inner = ui::frame(f, r, &title, sub.as_deref(), id == focus, &t);
            self.inner.push((id, inner));
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
        if self.help {
            draw_help(f, area, &t, &self.config);
        }
        if let Some(ob) = &mut self.onboard {
            ob.draw(f, area, &t, self.start.elapsed().as_secs_f64());
        }
    }

    /// The sidebar: the apps in sections (F1-F9), your own tabs, then the current app's own section.
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
        let mut y = inner.y;
        for (idx, &(name, icon, label, key)) in SIDEBAR.iter().enumerate() {
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
                ui::side_row(f, Rect { x: r.x + 2, width: r.width.saturating_sub(2), ..r }, icon, &title, &right, i == self.cur, t);
            }
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

fn draw_help(f: &mut Frame, area: Rect, t: &Theme, c: &Config) {
    let inner = ui::popup(f, area, 70, 24, "key bindings", t);
    let pre = c.prefix.clone();
    let rows: Vec<(String, &str)> = vec![
        ("alt ←↑↓→".into(), "move between panes"),
        ("alt shift ←↑↓→".into(), "resize the pane"),
        ("alt n · alt enter".into(), "new terminal (splits the pane)"),
        ("alt p".into(), "palette: open apps, themes, everything"),
        ("F1-F3".into(), "ai: chat · agents · your AIs"),
        ("F4-F9".into(), "tools: music · system · files · notes · storage · terminal"),
        ("F12".into(), "play / pause music from anywhere"),
        ("alt 1-9 · alt t".into(), "go to one of your tabs · new tab"),
        ("alt s".into(), "hide / show the sidebar"),
        ("alt z · alt w".into(), "zoom pane · close pane"),
        ("right-click".into(), "menu: split, zoom, rename, close"),
        ("double-click a tab".into(), "rename it (or prefix then ,)"),
        (format!("{pre} then"), ""),
        ("  | or v  ·  - ".into(), "split right · split down"),
        ("  , · &".into(), "rename tab · close tab"),
        ("  h j k l".into(), "move  (H J K L resize)"),
        ("  x · z · c · n/p".into(), "close · zoom · new tab · next/prev tab"),
        ("  a m s f e g".into(), "open ai · music · system · files · notes · storage"),
        ("  t · space · q".into(), "themes · palette · quit"),
        (format!("  {pre} again"), "send the prefix key to the program"),
        ("mouse".into(), "click to focus · drag a divider to resize · wheel scrolls"),
    ];
    let lines: Vec<Line> = rows
        .into_iter()
        .map(|(k, v)| Line::from(vec![Span::styled(format!("{k:<22}"), ui::bold_accent(t)), Span::raw(v.to_string())]))
        .chain([Line::raw(""), Line::styled("any key closes this", ui::muted(t))])
        .collect();
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
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
        cfg.theme = "oriel".into();
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
        assert!(shot(&mut app, "tour2").contains("back to chat"));
        key(&mut app, KeyCode::F(1));
        let p = app.probe();
        let out = app.onboard.as_mut().unwrap().check(&p);
        app.onboard_out(out);
        assert!(shot(&mut app, "tour3").contains("the palette"));
        // end it
        key(&mut app, KeyCode::F(11));
        assert!(app.onboard.is_none());
        let _ = rx;
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
}
