//! The settings app: every setting in one place, by section (the sidebar). A change is saved at once through the
//! app, config.toml's one writer, and applies where it can right away (theme, icons, prefix, sidebar, alerts, the
//! agents app, notes, music); the panel beside the list says each setting's key in config.toml, its default and
//! when a change takes effect. The rows themselves are in rows.rs; this draws them and handles the keys.
//!
//! Reach it with alt , · `/settings [what]` in chat · palette → "setting: …" (each opens on its row).

mod draw;
mod rows;
#[cfg(test)]
mod tests;

use crate::config::Config;
use crate::pane::{Action, Cx, Pane, Waker};
use crate::panes::agents::input::Input;
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use rows::{CATS, Ctl, Do, Env, Jump, Row, Unit};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

thread_local! {
    /// Where to open next (palette "setting: …", /settings perms): a row id, a section, or words to find. The UI
    /// is single-threaded; thread-local keeps parallel tests apart.
    static JUMP: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Open settings on this row, section or search the next time it draws (the app switches to it).
pub fn jump(what: &str) {
    JUMP.with(|j| *j.borrow_mut() = Some(what.trim().to_string()));
}

/// Every setting, for the palette: ("default AI · Claude Code", row id).
pub fn palette_items(cfg: &Config) -> Vec<(String, String)> {
    rows::all(&Env::quick())
        .iter()
        .map(|r| {
            let v = if r.is_setting() { shown(r, cfg, None) } else { String::new() };
            (if v.is_empty() { r.label.clone() } else { format!("{} · {}", r.label, ui::fit(&v, 40)) }, r.id.clone())
        })
        .collect()
}

/// What /settings offers after its name: the sections and the settings people look for most.
pub fn topics() -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = CATS.iter().map(|c| (c.0.to_lowercase(), "section".to_string())).collect();
    for (w, what) in [
        ("perms", "what coding agents may do in new chats"),
        ("provider", "the default AI"),
        ("effort", "how hard agents think"),
        ("model", "each AI's model"),
        ("theme", "how oriel looks"),
        ("prefix", "the prefix key"),
        ("shell", "what terminals run"),
        ("gate", "the merge gate"),
        ("notes", "the notes folder"),
        ("music", "music folders"),
    ] {
        v.push((w.into(), what.into()));
    }
    v
}

/// What a setting shows in the list: a switch as on/off, a choice by its label, a key masked, a number with its
/// unit. `pane` adds what only an open settings app knows (your AIs, the theme being previewed).
fn shown(r: &Row, cfg: &Config, pane: Option<&Settings>) -> String {
    match &r.ctl {
        Ctl::Link(_) | Ctl::Act(_) => pane.map(|p| p.link_value(r, cfg)).unwrap_or_default(),
        _ => fmt(r, (r.get)(cfg), pane),
    }
}

/// A setting's value the way the list shows it.
fn fmt(r: &Row, v: String, pane: Option<&Settings>) -> String {
    match &r.ctl {
        Ctl::Toggle => if v == "true" { "on" } else { "off" }.into(),
        Ctl::Choice { opts, .. } => {
            if r.id == "theme" {
                if let Some(p) = pane.filter(|p| p.preview.is_some()) {
                    return format!("{} (preview)", p.theme_now);
                }
            }
            match opts.iter().find(|o| o.0 == v) {
                Some(o) => o.1.clone(),
                None => v,
            }
        }
        Ctl::Number { unit, .. } => with_unit(v.parse().unwrap_or(0.0), *unit),
        Ctl::Text => if v.is_empty() { "default".into() } else { v },
        Ctl::Secret { env } => match (v.is_empty(), std::env::var(env).is_ok_and(|e| !e.is_empty())) {
            (false, _) => mask(&v),
            (true, true) => format!("from ${env}"),
            (true, false) => "not set".into(),
        },
        Ctl::Folder => if v.is_empty() { "oriel's data folder".into() } else { v },
        Ctl::List => match v.lines().count() {
            0 => "none: the OS music folder".into(),
            1 => "1 folder".into(),
            n => format!("{n} folders"),
        },
        Ctl::Key => if crate::config::check_prefix(&v, false).is_ok() { v } else { format!("{v} (unusable)") },
        Ctl::Link(_) | Ctl::Act(_) => v,
    }
}

fn with_unit(n: f64, u: Unit) -> String {
    let int = |n: f64| if n.fract() == 0.0 { format!("{n:.0}") } else { format!("{n}") };
    match u {
        Unit::None => int(n),
        Unit::Usd => format!("${n:.2}"),
        Unit::Pct => format!("{}%", int(n)),
        Unit::Min => format!("{} min", int(n)),
        Unit::Secs if n >= 60.0 && n % 60.0 == 0.0 => format!("{} min", int(n / 60.0)),
        Unit::Secs => format!("{} s", int(n)),
    }
}

/// An API key as `sk-…a1b2`: enough to tell which one it is.
fn mask(k: &str) -> String {
    let k = k.trim();
    let n = k.chars().count();
    if n <= 10 {
        return "•".repeat(n.clamp(4, 10));
    }
    format!("{}…{}", k.chars().take(3).collect::<String>(), k.chars().skip(n - 4).collect::<String>())
}

/// A folder as typed (~ = your home folder), for checking it's there.
fn expand(s: &str) -> std::path::PathBuf {
    let s = s.trim();
    match s.strip_prefix('~') {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest.trim_start_matches(['/', '\\'])),
        None => std::path::PathBuf::from(s),
    }
}

/// Which row (or list item) is selected: by name, so rows appearing later (the repo's gate) don't move it.
#[derive(Clone, PartialEq, Debug)]
enum Sel {
    Row(String),
    Item(String, usize),
    Add(String),
}

/// One line of the list.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Ln {
    /// a section heading (while finding across sections)
    Head(usize),
    Row(usize),
    /// a list row's item (a music folder)
    Item(usize, usize),
    /// "+ add a folder" under a list row
    Add(usize),
}

enum Edit {
    /// typing a value; `item`: Some(None) adds to a list, Some(Some(i)) replaces item i
    Text { id: String, input: Input, item: Option<Option<usize>>, err: Option<String> },
    /// waiting for the new prefix key
    Key { err: Option<String> },
}

/// What background threads found out.
#[derive(Default)]
struct Probe {
    avail: Option<Vec<&'static str>>,
    tags: Vec<String>,
    glance: Option<crate::panes::ais::Glance>,
    /// from the last update check: Some(Some(v)) = v is out, Some(None) = this is the newest
    newer: Option<Option<String>>,
    /// None = still looking; Some(None) = not in a git repo
    repo: Option<Option<(String, Option<String>)>>,
    songs: HashMap<String, Option<usize>>,
    tests: HashMap<&'static str, Result<String, String>>,
}

pub struct Settings {
    env: Env,
    probe: Arc<Mutex<Probe>>,
    /// look things up in the background (off in tests: nothing touches the network or your files)
    live: bool,
    started: bool,
    waker: Option<Waker>,
    /// the AI endpoints and keys the last provider check used
    ai_seen: (String, String, String, String),
    cat: usize,
    sel: Sel,
    scroll: usize,
    filter: String,
    filtering: bool,
    edit: Option<Edit>,
    /// what just happened (true = a problem)
    msg: Option<(String, bool)>,
    /// the theme before a live preview (esc goes back to it)
    preview: Option<String>,
    /// the theme on screen (the preview's)
    theme_now: String,
    /// a row waiting for a second enter (resetting the roster)
    confirm: Option<String>,
    songs_asked: HashSet<String>,
    tests_running: HashSet<&'static str>,
    hits: Vec<(Rect, Sel)>,
    side_hits: Vec<(Rect, usize)>,
}

impl Settings {
    pub fn new(cfg: &Config) -> Settings {
        let a = &cfg.ai;
        Settings {
            env: Env::scan(),
            probe: Arc::default(),
            live: !cfg!(test),
            started: false,
            waker: None,
            ai_seen: (a.ollama_url.clone(), a.openai_url.clone(), a.openai_key.clone(), a.anthropic_key.clone()),
            cat: 0,
            sel: Sel::Row("theme".into()),
            scroll: 0,
            filter: String::new(),
            filtering: false,
            edit: None,
            msg: None,
            preview: None,
            theme_now: cfg.theme.clone(),
            confirm: None,
            songs_asked: HashSet::new(),
            tests_running: HashSet::new(),
            hits: vec![],
            side_hits: vec![],
        }
    }

    // ------------------------------------------------------------------ background

    /// Once, on the first draw: which AIs are set up (TCP probes), what the token saver and your AI tools look
    /// like, and which repo oriel is in.
    fn start(&mut self, cx: &Cx) {
        if self.started {
            return;
        }
        self.started = true;
        self.waker = Some(cx.waker());
        if !self.live {
            return;
        }
        self.probe_ais(&cx.config.ai);
        let (p, w, cfg) = (self.probe.clone(), cx.waker(), cx.config.clone());
        std::thread::spawn(move || {
            let g = crate::panes::ais::glance(&cfg);
            let newer = crate::update::cached().map(|rs| crate::update::available(&rs).map(|r| r.version));
            let mut probe = p.lock().unwrap();
            probe.glance = Some(g);
            probe.newer = newer;
            drop(probe);
            w.wake();
        });
        let (p, w) = (self.probe.clone(), cx.waker());
        std::thread::spawn(move || {
            let here = std::env::current_dir().unwrap_or_default();
            let r = crate::panes::agents::repo_and_gate(&here).map(|(root, gate)| (root.display().to_string(), gate));
            p.lock().unwrap().repo = Some(r);
            w.wake();
        });
    }

    fn probe_ais(&mut self, ai: &crate::config::AiConfig) {
        let (p, w, ai) = (self.probe.clone(), self.waker.clone(), ai.clone());
        std::thread::spawn(move || {
            let found = crate::panes::chat::providers::available(&ai);
            let tags = if found.contains(&"ollama") { ollama_models(&ai.ollama_url).unwrap_or_default() } else { vec![] };
            {
                let mut g = p.lock().unwrap();
                g.avail = Some(found);
                g.tags = tags;
            }
            if let Some(w) = w {
                w.wake();
            }
        });
    }

    /// Take in what the threads found.
    fn pump(&mut self) {
        let g = self.probe.lock().unwrap();
        if g.avail.is_some() {
            self.env.avail = g.avail.clone();
        }
        self.env.ollama_tags = g.tags.clone();
        if let Some(r) = &g.repo {
            self.env.repo = r.clone();
        }
        self.tests_running.retain(|w| !g.tests.contains_key(w));
    }

    /// Song counts for the music folders (a thread each; right away in tests, whose folders are tiny).
    fn count_songs(&mut self, cfg: &Config) {
        for f in &cfg.music.folders {
            if !self.songs_asked.insert(f.clone()) {
                continue;
            }
            let (p, w, f) = (self.probe.clone(), self.waker.clone(), f.clone());
            let job = move || {
                let n = crate::onboard::count_audio(&expand(&f).to_string_lossy());
                p.lock().unwrap().songs.insert(f, n);
                if let Some(w) = w {
                    w.wake();
                }
            };
            if self.live {
                std::thread::spawn(job);
            } else {
                job();
            }
        }
    }

    /// `t` on an address: can oriel reach it? (GET /api/tags for Ollama, /models for OpenAI-compatible)
    fn test(&mut self, which: &'static str, cfg: &Config) {
        if !self.tests_running.insert(which) {
            return;
        }
        self.probe.lock().unwrap().tests.remove(which);
        let (p, w, ai) = (self.probe.clone(), self.waker.clone(), cfg.ai.clone());
        let job = move || {
            let r = match which {
                "ollama" => ollama_models(&ai.ollama_url).map(|m| format!("reachable · {} model{}", m.len(), if m.len() == 1 { "" } else { "s" })),
                _ => {
                    let key = Some(ai.openai_key.trim().to_string()).filter(|k| !k.is_empty()).or_else(|| std::env::var("OPENAI_API_KEY").ok().filter(|k| !k.is_empty()));
                    openai_models(&ai.openai_url, key.as_deref()).map(|n| format!("reachable · {n} model{}", if n == 1 { "" } else { "s" }))
                }
            };
            p.lock().unwrap().tests.insert(which, r);
            if let Some(w) = w {
                w.wake();
            }
        };
        if self.live {
            std::thread::spawn(job);
        } else {
            self.probe.lock().unwrap().tests.insert(which, Err("not tested here (tests stay offline)".into()));
            drop(job);
        }
    }

    // ------------------------------------------------------------------ where you are

    fn take_jump(&mut self) {
        if let Some(q) = JUMP.with(|j| j.borrow_mut().take()) {
            self.go(&q);
        }
    }

    /// Open on a row id ("ai.perms"), a setting's name or key ("perms"), a section ("alerts"), or else find it.
    fn go(&mut self, q: &str) {
        let q = q.trim().to_lowercase();
        self.edit = None;
        self.filtering = false;
        self.filter.clear();
        if q.is_empty() {
            return;
        }
        let rows = rows::all(&self.env);
        // "perms" → [ai] perms; but "model" is in several names, so that one's a search
        let tail: Vec<&Row> = rows.iter().filter(|r| r.id.rsplit('.').next() == Some(q.as_str())).collect();
        let named = rows.iter().filter(|r| r.label.to_lowercase().contains(&q)).count();
        let exact = rows.iter().find(|r| r.id == q).or_else(|| rows.iter().find(|r| r.label.to_lowercase() == q)).or(if tail.len() == 1 && named <= 1 { tail.first().copied() } else { None });
        if let Some(r) = exact {
            (self.cat, self.sel) = (r.cat, Sel::Row(r.id.clone()));
            return;
        }
        let words = |s: &str| s.to_lowercase().split(|c: char| !c.is_alphanumeric()).any(|w| w == q);
        if let Some(i) = CATS.iter().position(|c| c.0.to_lowercase().starts_with(&q) || words(c.0)) {
            self.cat = i;
            self.sel = rows.iter().find(|r| r.cat == i).map(|r| Sel::Row(r.id.clone())).unwrap_or(Sel::Row(String::new()));
            return;
        }
        let hits: Vec<&Row> = rows.iter().filter(|r| matches(r, &q)).collect();
        match hits.as_slice() {
            [r] => (self.cat, self.sel) = (r.cat, Sel::Row(r.id.clone())),
            _ => {
                self.filter = q;
                if let Some(r) = hits.first() {
                    self.sel = Sel::Row(r.id.clone());
                }
            }
        }
    }

    /// The lines of the list: this section's rows (list rows with their items), or every match while finding.
    fn lines(&self, rows: &[Row], cfg: &Config) -> Vec<Ln> {
        let q = self.filter.trim().to_lowercase();
        let mut v = vec![];
        for ci in 0..CATS.len() {
            if q.is_empty() && ci != self.cat {
                continue;
            }
            let mine: Vec<usize> = (0..rows.len()).filter(|&i| rows[i].cat == ci && (q.is_empty() || matches(&rows[i], &q))).collect();
            if mine.is_empty() {
                continue;
            }
            if !q.is_empty() {
                v.push(Ln::Head(ci));
            }
            for i in mine {
                v.push(Ln::Row(i));
                if matches!(rows[i].ctl, Ctl::List) {
                    let n = (rows[i].get)(cfg).lines().count();
                    v.extend((0..n).map(|k| Ln::Item(i, k)));
                    v.push(Ln::Add(i));
                }
            }
        }
        v
    }

    fn key_of(ln: Ln, rows: &[Row]) -> Option<Sel> {
        match ln {
            Ln::Head(_) => None,
            Ln::Row(i) => Some(Sel::Row(rows[i].id.clone())),
            Ln::Item(i, k) => Some(Sel::Item(rows[i].id.clone(), k)),
            Ln::Add(i) => Some(Sel::Add(rows[i].id.clone())),
        }
    }

    /// The selected line (the first one if the selection is gone).
    fn index(&mut self, lines: &[Ln], rows: &[Row]) -> Option<usize> {
        if let Some(i) = lines.iter().position(|l| Self::key_of(*l, rows).as_ref() == Some(&self.sel)) {
            return Some(i);
        }
        // an item that was removed: the one before it, else its list row
        if let Sel::Item(id, k) = self.sel.clone() {
            self.sel = if k > 0 { Sel::Item(id, k - 1) } else { Sel::Row(id) };
            return self.index(lines, rows);
        }
        let i = lines.iter().position(|l| !matches!(l, Ln::Head(_)))?;
        self.sel = Self::key_of(lines[i], rows)?;
        Some(i)
    }

    fn select(&mut self, lines: &[Ln], rows: &[Row], i: usize, cx: &mut Cx) {
        if let Some(k) = lines.get(i).and_then(|l| Self::key_of(*l, rows)) {
            if k != self.sel {
                self.end_preview(cx);
                self.confirm = None;
            }
            self.sel = k;
        }
    }

    /// Move the selection `d` lines (headings skipped).
    fn step(&mut self, lines: &[Ln], rows: &[Row], d: isize, cx: &mut Cx) {
        let Some(cur) = self.index(lines, rows) else { return };
        let picks: Vec<usize> = (0..lines.len()).filter(|&j| !matches!(lines[j], Ln::Head(_))).collect();
        let at = picks.iter().position(|&j| j == cur).unwrap_or(0) as isize;
        let to = (at + d).clamp(0, picks.len() as isize - 1) as usize;
        self.select(lines, rows, picks[to], cx);
    }

    fn set_cat(&mut self, c: usize, cx: &mut Cx) {
        self.end_preview(cx);
        self.cat = c % CATS.len();
        self.filter.clear();
        self.filtering = false;
        self.scroll = 0;
        self.confirm = None;
        let rows = rows::all(&self.env);
        if let Some(r) = rows.iter().find(|r| r.cat == self.cat) {
            self.sel = Sel::Row(r.id.clone());
        }
    }

    /// Leaving the theme row (or esc) mid-preview: back to the saved theme.
    fn end_preview(&mut self, cx: &mut Cx) {
        if let Some(before) = self.preview.take() {
            cx.act(Action::PreviewTheme(before));
        }
    }

    // ------------------------------------------------------------------ changing things

    /// Save a value through the app (it re-reads config.toml, changes this one setting, writes it, and tells
    /// every pane).
    fn commit(&mut self, r: &Row, v: String, cx: &mut Cx) {
        let Some(set) = r.set.clone() else { return };
        let label = r.label.clone();
        let when = r.when;
        cx.edit_config(move |c| set(c, &v));
        self.msg = Some((format!("{label} saved · applies {}", when.applies()), false));
    }

    /// A typed value, checked for what the row needs. Err = what's wrong (the box stays open).
    fn check(r: &Row, text: &str) -> Result<String, String> {
        let t = text.trim();
        match &r.ctl {
            Ctl::Number { min, max, unit, .. } => {
                let n: f64 = t.trim_start_matches('$').trim_end_matches(['%', 's']).trim().parse().map_err(|_| format!("'{t}' isn't a number"))?;
                if !n.is_finite() {
                    return Err(format!("'{t}' isn't a number"));
                }
                let n = n.clamp(*min, *max);
                Ok(if *unit == Unit::Usd { format!("{:.2}", n) } else { format!("{}", n.round()) })
            }
            _ if r.id.ends_with("_url") => {
                if t.starts_with("http://") || t.starts_with("https://") {
                    Ok(t.trim_end_matches('/').to_string())
                } else {
                    Err("an address starts with http:// or https://".into())
                }
            }
            Ctl::Secret { .. } if t.contains(char::is_whitespace) => Err("a key has no spaces in it".into()),
            _ => Ok(t.to_string()),
        }
    }

    /// Open the typing box for a row (or a list item).
    fn start_edit(&mut self, r: &Row, cfg: &Config, item: Option<Option<usize>>) {
        let now = (r.get)(cfg);
        let text = match (&r.ctl, item) {
            (_, Some(Some(i))) => now.lines().nth(i).unwrap_or("").to_string(),
            (_, Some(None)) => String::new(),
            (Ctl::Secret { .. }, _) => String::new(), // a new key replaces the old one; it's never shown
            (Ctl::Number { unit: Unit::Usd, .. }, _) => format!("{:.2}", now.parse::<f64>().unwrap_or(0.0)),
            (Ctl::Number { .. }, _) => with_unit(now.parse().unwrap_or(0.0), Unit::None),
            _ => now,
        };
        self.edit = Some(Edit::Text { id: r.id.clone(), input: Input::new(&text, false), item, err: None });
    }

    fn edit_key(&mut self, k: KeyEvent, rows: &[Row], cx: &mut Cx) -> bool {
        let Some(edit) = &mut self.edit else { return false };
        match edit {
            Edit::Key { err } => {
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                let spec = match k.code {
                    KeyCode::Esc => {
                        self.edit = None;
                        return true;
                    }
                    KeyCode::Char(' ') | KeyCode::Char('@') if ctrl => "ctrl+space".to_string(),
                    KeyCode::Char(c) if ctrl && c.is_ascii_alphabetic() => format!("ctrl+{}", c.to_ascii_lowercase()),
                    _ => {
                        *err = Some("that isn't ctrl+<letter> or ctrl+space: press one of those".into());
                        return true;
                    }
                };
                match crate::config::check_prefix(&spec, true) {
                    Ok(()) => {
                        self.edit = None;
                        if let Some(r) = rows.iter().find(|r| r.id == "prefix") {
                            self.commit(r, spec.clone(), cx);
                            self.msg = Some((format!("prefix key: {spec} · works now"), false));
                        }
                    }
                    Err(e) => *err = Some(format!("{e}: pick another")),
                }
                true
            }
            Edit::Text { id, input, item, err } => {
                let Some(r) = rows.iter().find(|r| r.id == *id) else {
                    self.edit = None;
                    return true;
                };
                match k.code {
                    KeyCode::Esc => self.edit = None,
                    KeyCode::Enter => {
                        let item = *item;
                        let text = input.text.clone();
                        if let Some(it) = item {
                            // a folder in a list: it has to be there
                            let t = text.trim().to_string();
                            if t.is_empty() {
                                self.edit = None;
                                return true;
                            }
                            if !expand(&t).is_dir() {
                                *err = Some("that folder doesn't exist".into());
                                return true;
                            }
                            let mut list: Vec<String> = (r.get)(cx.config).lines().map(String::from).collect();
                            match it {
                                Some(i) if i < list.len() => list[i] = t,
                                _ if list.contains(&t) => {
                                    *err = Some("it's on the list already".into());
                                    return true;
                                }
                                _ => list.push(t),
                            }
                            self.edit = None;
                            self.commit(r, list.join("\n"), cx);
                            return true;
                        }
                        match Self::check(r, &text) {
                            Ok(v) => {
                                self.edit = None;
                                let secret = matches!(r.ctl, Ctl::Secret { .. });
                                self.commit(r, v, cx);
                                if secret {
                                    self.msg = Some((format!("{} saved · it's in config.toml as plain text", r.label), false));
                                }
                            }
                            Err(e) => *err = Some(e),
                        }
                    }
                    KeyCode::Tab if matches!(r.ctl, Ctl::Folder | Ctl::List) => {
                        // complete a folder, ~ included
                        let t = input.text.clone();
                        let done = match t.strip_prefix('~') {
                            Some(_) => crate::onboard::complete_dir(&expand(&t).to_string_lossy()),
                            None => crate::onboard::complete_dir(&t),
                        };
                        *input = Input::new(&done, false);
                    }
                    _ => {
                        *err = None;
                        input.key(k);
                    }
                }
                true
            }
        }
    }

    /// Links go somewhere; actions do something.
    fn follow(&mut self, j: Jump, cx: &mut Cx) {
        match j {
            Jump::App(a) => cx.act(Action::GotoApp(a)),
            Jump::AppKey(a, c) => {
                cx.act(Action::GotoApp(a));
                cx.act(Action::AppKey(a, c));
            }
            Jump::Tour => cx.act(Action::Tour),
        }
    }

    fn run(&mut self, d: Do, r: &Row, cx: &mut Cx) {
        match d {
            Do::ResetRoster => {
                if cx.config.roster.is_empty() {
                    self.msg = Some(("the roster is already the defaults from what's installed".into(), false));
                } else if self.confirm.as_deref() == Some(&r.id) {
                    self.confirm = None;
                    cx.edit_config(|c| c.roster.clear());
                    self.msg = Some(("roster reset: one worker per agent installed here".into(), false));
                } else {
                    self.confirm = Some(r.id.clone());
                    let n = cx.config.roster.len();
                    self.msg = Some((format!("enter again to remove your {n} worker{} and use the defaults", if n == 1 { "" } else { "s" }), true));
                }
            }
            Do::TestNotify => {
                crate::alerts::desktop("oriel", "a test notification: desktop notifications work");
                self.msg = Some((
                    if cx.config.desktop_notifications { "sent: a notification should show now".into() } else { "sent (desktop notifications are off, so alerts won't send one)".to_string() },
                    false,
                ));
            }
            Do::ConfigFile => self.open_config(),
        }
    }

    fn open_config(&mut self) {
        let p = crate::config::path();
        self.msg = Some(if !p.exists() {
            ("there's no config.toml yet: change any setting and it's made".into(), false)
        } else {
            match crate::panes::themes::open_in_editor(&p) {
                Ok(()) => ("opened config.toml: save it and oriel picks it up".into(), false),
                Err(e) => (format!("couldn't open it: {e}"), true),
            }
        });
    }

    /// The live state a link row shows.
    fn link_value(&self, r: &Row, cfg: &Config) -> String {
        let g = self.probe.lock().unwrap();
        match r.id.as_str() {
            "themes" => self.theme_now.clone(),
            "roster.edit" if cfg.roster.is_empty() => "defaults from what's installed".into(),
            "roster.edit" => format!("{} worker{}: {}", cfg.roster.len(), if cfg.roster.len() == 1 { "" } else { "s" }, cfg.roster.iter().map(|w| w.name.as_str()).collect::<Vec<_>>().join(", ")),
            "roster.reset" if cfg.roster.is_empty() => "in use".into(),
            "tools.saver" => g.glance.as_ref().map(|x| x.saver.clone()).unwrap_or_else(|| if self.live { "…".into() } else { String::new() }),
            "tools.ais" => g.glance.as_ref().map(|x| x.tools.clone()).unwrap_or_else(|| if self.live { "…".into() } else { String::new() }),
            "tools.updates" => match &g.newer {
                Some(Some(v)) => format!("oriel {} · {v} is out", crate::update::VERSION),
                Some(None) => format!("oriel {}, the newest", crate::update::VERSION),
                None => format!("oriel {}", crate::update::VERSION),
            },
            "tools.config" => crate::config::path().display().to_string(),
            _ => String::new(),
        }
    }

}

/// Does a row match what you're looking for (its name, key, id, description or section)?
fn matches(r: &Row, q: &str) -> bool {
    [r.label.as_str(), r.key.as_str(), r.id.as_str(), r.desc.as_str(), CATS[r.cat].0].iter().any(|s| s.to_lowercase().contains(q))
}

/// Models installed in Ollama (GET /api/tags).
fn ollama_models(url: &str) -> Result<Vec<String>, String> {
    let body = get(&format!("{}/api/tags", url.trim_end_matches('/')), None)?;
    parse_tags(&body).ok_or_else(|| "it answered, but not like Ollama".into())
}

/// How many models an OpenAI-compatible server lists (GET /models).
fn openai_models(url: &str, key: Option<&str>) -> Result<usize, String> {
    let body = get(&format!("{}/models", url.trim_end_matches('/')), key)?;
    parse_models(&body).ok_or_else(|| "it answered, but not with a model list".into())
}

fn get(url: &str, key: Option<&str>) -> Result<String, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(4))).build().into();
    let mut req = agent.get(url);
    if let Some(k) = key {
        req = req.header("Authorization", &format!("Bearer {k}"));
    }
    match req.call() {
        Ok(r) => r.into_body().read_to_string().map_err(|e| e.to_string()),
        Err(ureq::Error::StatusCode(401 | 403)) => Err("it refused the key (401/403)".into()),
        Err(ureq::Error::StatusCode(c)) => Err(format!("it answered {c}")),
        Err(e) => Err(format!("can't reach it: {e}")),
    }
}

fn parse_tags(body: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    Some(v.get("models")?.as_array()?.iter().filter_map(|m| m["name"].as_str().map(String::from)).collect())
}

fn parse_models(body: &str) -> Option<usize> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    Some(v.get("data")?.as_array()?.len())
}

impl Pane for Settings {
    fn title(&self) -> String {
        if self.filter.is_empty() { format!("settings · {}", CATS[self.cat].0) } else { format!("settings · find “{}”", self.filter) }
    }
    fn icon(&self) -> &'static str {
        "cog"
    }

    /// The AI addresses or keys changed: look again at which AIs are set up.
    fn config_changed(&mut self, cfg: &Config) {
        let a = &cfg.ai;
        let now = (a.ollama_url.clone(), a.openai_url.clone(), a.openai_key.clone(), a.anthropic_key.clone());
        if now != self.ai_seen {
            self.ai_seen = now;
            if self.live && self.started {
                self.probe_ais(a);
            }
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        self.draw(f, area, cx);
    }

    fn key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        let rows = rows::all(&self.env);
        if self.edit.is_some() {
            return self.edit_key(k, &rows, cx);
        }
        if self.filtering {
            match k.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.filtering = false;
                }
                KeyCode::Enter | KeyCode::Down | KeyCode::Up => self.filtering = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char(c) if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => self.filter.push(c),
                _ => return false,
            }
            return true;
        }
        let cfg = cx.config.clone();
        let lines = self.lines(&rows, &cfg);
        let idx = self.index(&lines, &rows);
        let ln = idx.map(|i| lines[i]);
        if !matches!(k.code, KeyCode::Enter) {
            self.confirm = None;
        }
        if !matches!(k.code, KeyCode::Char('t')) && self.msg.as_ref().is_some_and(|m| !m.1) {
            self.msg = None;
        }
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => self.step(&lines, &rows, -1, cx),
            KeyCode::Down | KeyCode::Char('j') => self.step(&lines, &rows, 1, cx),
            KeyCode::PageUp => self.step(&lines, &rows, -10, cx),
            KeyCode::PageDown => self.step(&lines, &rows, 10, cx),
            KeyCode::Home => self.step(&lines, &rows, -(lines.len() as isize), cx),
            KeyCode::End => self.step(&lines, &rows, lines.len() as isize, cx),
            KeyCode::Tab => self.set_cat(self.cat + 1, cx),
            KeyCode::BackTab => self.set_cat(self.cat + CATS.len() - 1, cx),
            KeyCode::Char('/') => {
                self.end_preview(cx);
                self.filtering = true;
                self.filter.clear();
            }
            KeyCode::Char('o') => self.open_config(),
            KeyCode::Esc => {
                if self.preview.is_some() {
                    self.end_preview(cx);
                    self.msg = Some(("back to the theme you had".into(), false));
                } else if !self.filter.is_empty() {
                    self.filter.clear();
                } else if self.msg.take().is_none() {
                    return false;
                }
            }
            _ => {
                let Some(ln) = ln else { return false };
                return self.act(k, ln, &rows, &cfg, cx);
            }
        }
        true
    }

    fn mouse(&mut self, ev: MouseEvent, _area: Rect, cx: &mut Cx) {
        let rows = rows::all(&self.env);
        let cfg = cx.config.clone();
        let lines = self.lines(&rows, &cfg);
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let pos = Position { x: ev.column, y: ev.row };
                if let Some((_, k)) = self.hits.iter().find(|(r, _)| r.contains(pos)).cloned() {
                    if k != self.sel {
                        self.end_preview(cx);
                    }
                    self.sel = k;
                }
            }
            MouseEventKind::ScrollDown => self.step(&lines, &rows, 1, cx),
            MouseEventKind::ScrollUp => self.step(&lines, &rows, -1, cx),
            _ => {}
        }
    }

    /// The sections, each with how many of its settings you've changed.
    fn side(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        self.take_jump(); // the sidebar is drawn before the pane
        let t = cx.theme;
        let rows = rows::all(&self.env);
        self.side_hits.clear();
        let mut y = area.y;
        for (i, (name, icon)) in CATS.iter().enumerate() {
            if y >= area.bottom() {
                break;
            }
            let n = rows.iter().filter(|r| r.cat == i && r.is_setting() && (r.get)(cx.config) != rows::all_default(r)).count();
            let r = Rect { y, height: 1, ..area };
            ui::side_row(f, r, icon, name, &if n > 0 { format!("● {n}") } else { String::new() }, i == self.cat && self.filter.is_empty(), t);
            self.side_hits.push((r, i));
            y += 1;
        }
    }

    fn side_mouse(&mut self, ev: MouseEvent, _area: Rect, cx: &mut Cx) {
        if let MouseEventKind::Down(MouseButton::Left) = ev.kind {
            let pos = Position { x: ev.column, y: ev.row };
            if let Some(&(_, i)) = self.side_hits.iter().find(|(r, _)| r.contains(pos)) {
                self.set_cat(i, cx);
            }
        }
    }
}

impl Settings {
    /// A key on the selected line that isn't moving around.
    fn act(&mut self, k: KeyEvent, ln: Ln, rows: &[Row], cfg: &Config, cx: &mut Cx) -> bool {
        let (Ln::Row(i) | Ln::Item(i, _) | Ln::Add(i)) = ln else { return false };
        let r = &rows[i];
        let val = (r.get)(cfg);
        let code = match k.code {
            KeyCode::Char('h') => KeyCode::Left,
            KeyCode::Char('l') => KeyCode::Right,
            c => c,
        };
        // a list's items and its "add" line
        match (ln, code) {
            (Ln::Item(_, n), KeyCode::Char('x') | KeyCode::Delete) => {
                let mut list: Vec<&str> = val.lines().collect();
                if n < list.len() {
                    let gone = list.remove(n);
                    let text = list.join("\n");
                    self.commit(r, text, cx);
                    self.msg = Some((format!("removed {gone}"), false));
                }
                return true;
            }
            (Ln::Item(_, n), KeyCode::Char(c @ ('J' | 'K'))) => {
                let mut list: Vec<&str> = val.lines().collect();
                let to = if c == 'J' { n + 1 } else { n.wrapping_sub(1) };
                if to < list.len() {
                    list.swap(n, to);
                    let text = list.join("\n");
                    self.commit(r, text, cx);
                    self.sel = Sel::Item(r.id.clone(), to);
                }
                return true;
            }
            (Ln::Item(_, n), KeyCode::Enter) => {
                self.start_edit(r, cfg, Some(Some(n)));
                return true;
            }
            (Ln::Item(..) | Ln::Add(_), KeyCode::Char('a')) | (Ln::Add(_), KeyCode::Enter) => {
                self.sel = Sel::Add(r.id.clone());
                self.start_edit(r, cfg, Some(None));
                return true;
            }
            (Ln::Item(..) | Ln::Add(_), _) => return false,
            _ => {}
        }
        if code == KeyCode::Char('r') {
            let def = rows::all_default(r);
            if r.is_setting() && val != def {
                self.end_preview(cx);
                self.commit(r, def, cx);
                self.msg = Some((format!("{} is back to its default", r.label), false));
            }
            return r.is_setting();
        }
        if code == KeyCode::Char('t') && r.id.ends_with("_url") {
            self.test(if r.id == "ai.ollama_url" { "ollama" } else { "openai" }, cfg);
            return true;
        }
        if code == KeyCode::Char('c') && r.id == "tools.config" {
            crate::clip::copy(&crate::config::path().display().to_string());
            self.msg = Some(("copied where config.toml is".into(), false));
            return true;
        }
        match &r.ctl {
            Ctl::Toggle if matches!(code, KeyCode::Char(' ') | KeyCode::Enter | KeyCode::Left | KeyCode::Right) => {
                self.commit(r, if val == "true" { "false".into() } else { "true".into() }, cx);
            }
            Ctl::Choice { opts, .. } if matches!(code, KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')) && !opts.is_empty() => {
                let now = if r.id == "theme" { self.theme_now.clone() } else { val.clone() };
                let at = opts.iter().position(|o| o.0 == now);
                let n = opts.len();
                let next = match (at, code) {
                    (Some(a), KeyCode::Left) => (a + n - 1) % n,
                    (Some(a), _) => (a + 1) % n,
                    (None, _) => 0,
                };
                let v = opts[next].0.clone();
                if r.id == "theme" {
                    // shown, not saved yet: enter keeps it, esc (or leaving the row) goes back
                    self.preview.get_or_insert(val.clone());
                    self.theme_now = v.clone();
                    cx.act(Action::PreviewTheme(v));
                } else {
                    self.commit(r, v, cx);
                }
            }
            Ctl::Choice { .. } if code == KeyCode::Enter && r.id == "theme" => match self.preview.take() {
                Some(_) => {
                    let v = self.theme_now.clone();
                    self.commit(r, v.clone(), cx);
                    self.msg = Some((format!("theme: {v} · saved"), false));
                }
                // the palette's theme picker (live preview, search by name)
                None => cx.act(Action::Palette("theme ".into())),
            },
            Ctl::Choice { custom: true, .. } | Ctl::Number { .. } | Ctl::Text | Ctl::Folder | Ctl::Secret { .. } if code == KeyCode::Enter => self.start_edit(r, cfg, None),
            Ctl::Choice { .. } if code == KeyCode::Enter => {
                // no typing: enter moves to the next choice
                return self.act(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), ln, rows, cfg, cx);
            }
            Ctl::Number { min, max, step, unit } if matches!(code, KeyCode::Left | KeyCode::Right) => {
                let n: f64 = val.parse().unwrap_or(*min);
                let d = if code == KeyCode::Left { -step } else { *step };
                let v = ((n + d) / step).round() * step;
                let v = v.clamp(*min, *max);
                self.commit(r, if *unit == Unit::Usd { format!("{v:.2}") } else { format!("{v}") }, cx);
            }
            Ctl::Secret { .. } if matches!(code, KeyCode::Char('x') | KeyCode::Delete) => {
                if !val.is_empty() {
                    self.commit(r, String::new(), cx);
                    self.msg = Some((format!("{} cleared", r.label), false));
                }
            }
            Ctl::List if matches!(code, KeyCode::Enter | KeyCode::Char('a')) => {
                self.sel = Sel::Add(r.id.clone());
                self.start_edit(r, cfg, Some(None));
            }
            Ctl::Key if code == KeyCode::Enter => self.edit = Some(Edit::Key { err: None }),
            Ctl::Link(j) if code == KeyCode::Enter => {
                let j = *j;
                self.follow(j, cx);
            }
            Ctl::Act(d) if code == KeyCode::Enter => {
                let d = *d;
                self.run(d, r, cx);
            }
            _ => return false,
        }
        true
    }
}
