//! The workspace: tabs, each a tree of split panes; focus, key bindings, the palette, mouse, themes.
//! Event-driven: it sleeps until input, pane output or a tick a visible pane asked for, then redraws once.

use crate::config::{self, Config};
use crate::layout::{Dir, Node, PaneId, neighbor, split_rect};
use crate::pane::{Action, Cx, Event, Pane, Place};
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
    root: Node,
    focus: PaneId,
    zoom: bool,
}

/// The sidebar's app list, nest style: (app, icon, label, key).
pub const SIDEBAR: &[(&str, &str, &str, &str)] = &[
    ("ai", "ai", "ai", "F1"),
    ("music", "music", "music", "F2"),
    ("system", "system", "system", "F3"),
    ("files", "files", "files", "F4"),
    ("notes", "notes", "notes", "F5"),
    ("storage", "storage", "storage", "F6"),
    ("terminal", "term", "terminal", "F7"),
];

#[derive(Clone, Copy)]
enum SideHit {
    App(&'static str),
    Tab(usize),
    NewTab,
}

#[derive(Clone)]
enum Cmd {
    App(&'static str),
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
    _watcher: Option<notify::RecommendedWatcher>,
}

impl App {
    pub fn new(config: Config, tx: Sender<Event>) -> App {
        let theme = theme::get(&config.theme);
        ui::NERD.store(std::env::var("ORIEL_PLAIN").is_err(), std::sync::atomic::Ordering::Relaxed);
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
        };
        app._watcher = watch_omarchy(tx);
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
        self.tabs.push(Tab { app: None, root: Node::Leaf(id), focus: id, zoom: false });
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
        self.tabs.insert(at, Tab { app: Some(name), root: Node::Leaf(id), focus: id, zoom: false });
        self.cur = at;
    }

    /// Tabs you made (not the pinned apps), with their index in self.tabs.
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
        for id in self.visible() {
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
                for id in self.visible() {
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
            if (1..=7).contains(&n) && k.modifiers.is_empty() {
                self.goto_app(SIDEBAR[n as usize - 1].0);
                return;
            }
            if n == 8 {
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
            KeyCode::Char('|') | KeyCode::Char('\\') | KeyCode::Char('%') => self.run_cmd(Cmd::SplitRight),
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
        for t in theme::names() {
            items.push((format!("{}theme {t}", ui::lead("theme")), Cmd::Theme(t)));
        }
        items.push(("toggle nerd font icons".into(), Cmd::Icons));
        items.push(("key bindings".into(), Cmd::Help));
        items.push((format!("{}quit oriel", ui::lead("quit")), Cmd::Quit));
        self.palette = Some(Palette { query: String::new(), sel: 0, items, theme_before: self.theme.name.clone() });
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
        let pos = Position { x: m.column, y: m.row };
        if let MouseEventKind::Down(MouseButton::Left) = m.kind {
            if let Some(&(_, hit)) = self.side_hits.iter().find(|(r, _)| r.contains(pos)) {
                match hit {
                    SideHit::App(a) => self.goto_app(a),
                    SideHit::Tab(i) => self.cur = i,
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
        if let Some(p) = &self.palette {
            self.draw_palette(f, area, p, &t);
        }
        if self.help {
            draw_help(f, area, &t, &self.config);
        }
    }

    /// nest's sidebar: the apps (F1-F7), your own tabs, then the current app's own section.
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
        for &(name, icon, label, key) in SIDEBAR {
            if y >= inner.bottom() {
                break;
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
            let (icon, title) = self.panes.get(&tb.focus).map(|p| (p.icon(), p.title())).unwrap_or(("term", String::new()));
            let panes = { let mut l = vec![]; tb.root.leaves(&mut l); l.len() };
            let right = if panes > 1 { format!("{panes} panes  alt {}", n + 1) } else { format!("alt {}", n + 1) };
            let r = Rect { y, height: 1, ..inner };
            ui::side_row(f, r, icon, &title, &right, i == self.cur, t);
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
        ("F1-F7".into(), "ai · music · system · files · notes · storage · terminal"),
        ("F8".into(), "play / pause music from anywhere"),
        ("alt 1-9 · alt t".into(), "go to one of your tabs · new tab"),
        ("alt s".into(), "hide / show the sidebar"),
        ("alt z · alt w".into(), "zoom pane · close pane"),
        (format!("{pre} then"), ""),
        ("  | or \\  ·  - ".into(), "split right · split down"),
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
