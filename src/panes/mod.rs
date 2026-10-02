//! Every kind of pane, and the one factory that makes them by name (used by the launcher, palette and keys).

pub mod agents;
pub mod alerts;
pub mod ais;
pub mod calendar;
pub mod chat;
pub mod files;
pub mod help;
pub mod home;
pub mod music;
pub mod notes;
pub mod settings;
pub mod search;
pub mod storage;
pub mod system;
pub mod term;
pub mod themes;
pub mod updates;

use crate::config::{Config, which};
use crate::pane::Pane;

/// (name, key on the home screen, icon, label). The order is the home screen's order.
pub const APPS: &[(&str, char, &str, &str)] = &[
    ("terminal", 't', "term", "terminal"),
    ("ai", 'a', "ai", "chat"),
    ("claude", 'c', "claude", "claude code"),
    ("codex", 'x', "robot", "codex"),
    ("agents", 'r', "robot", "agents"),
    ("ais", 'i', "gauge", "your AIs"),
    ("music", 'm', "music", "music"),
    ("system", 's', "system", "system"),
    ("files", 'f', "files", "files"),
    ("notes", 'n', "notes", "notes"),
    ("calendar", 'd', "calendar", "calendar"),
    ("storage", 'g', "storage", "storage"),
    ("themes", 'l', "theme", "themes"),
    ("help", '?', "search", "help"),
];

/// Apps that keep a single copy, in their sidebar tab: a second would play over the first (music), fire every
/// reminder twice (calendar) or fight the first over the same state (agents, your AIs). Opening one again goes to
/// its tab instead.
pub const SINGLE: &[&str] = &["music", "calendar", "agents", "ais"];

/// CLI agents that run as a terminal pane when installed: (app name, program, title).
const AGENTS: &[(&str, &str, &str)] = &[("claude", "claude", "claude code"), ("codex", "codex", "codex")];

#[cfg(test)]
thread_local! {
    /// How often `available` ran on this thread: the launcher must not ask on every frame.
    pub static AVAILABLE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub fn available(name: &str) -> bool {
    #[cfg(test)]
    AVAILABLE_CALLS.with(|c| c.set(c.get() + 1));
    match AGENTS.iter().position(|a| a.0 == name) {
        Some(i) => FOUND.lock().unwrap_or_else(|e| e.into_inner()).get(i, std::time::Instant::now(), |i| which(AGENTS[i].1).is_some()),
        None => true,
    }
}

/// Every name `open` takes (`oriel <app>` checks against it). A CLI agent counts even when it isn't installed:
/// the app says so then.
const NAMES: &[&str] = &["terminal", "shell", "ai", "chat", "music", "system", "files", "notes", "calendar", "storage", "agents", "ais", "home", "help", "themes", "alerts", "updates", "settings", "search"];

pub fn known(name: &str) -> bool {
    NAMES.contains(&name) || AGENTS.iter().any(|a| a.0 == name)
}

/// Look for claude/codex on PATH again next time (the palette does this when it opens).
pub fn refresh_available() {
    FOUND.lock().unwrap_or_else(|e| e.into_inner()).at = None;
}

/// Which AGENTS are on PATH, looked up at most every 10 s: a PATH scan costs ~9 ms for the pair on a long PATH,
/// and the home screen asks on every draw, key and mouse move.
static FOUND: std::sync::Mutex<Found> = std::sync::Mutex::new(Found { at: None, have: Vec::new() });

struct Found {
    at: Option<std::time::Instant>,
    have: Vec<bool>,
}

impl Found {
    fn get(&mut self, i: usize, now: std::time::Instant, look: impl Fn(usize) -> bool) -> bool {
        if self.at.is_none_or(|at| now.duration_since(at) > std::time::Duration::from_secs(10)) {
            self.have = (0..AGENTS.len()).map(&look).collect();
            self.at = Some(now);
        }
        self.have.get(i).copied().unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    #[test]
    fn available_is_cached() {
        let looks = Cell::new(0);
        let look = |i: usize| {
            looks.set(looks.get() + 1);
            i == 0
        };
        let mut f = Found { at: None, have: vec![] };
        let t0 = Instant::now();
        assert!(f.get(0, t0, look) && !f.get(1, t0, look));
        for _ in 0..100 {
            f.get(0, t0 + Duration::from_secs(5), look);
        }
        assert_eq!(looks.get(), AGENTS.len(), "one PATH scan for every agent, then the cache");
        f.get(0, t0 + Duration::from_secs(11), look);
        assert_eq!(looks.get(), 2 * AGENTS.len(), "looked again after 10 s");
        f.at = None; // refresh_available
        f.get(1, t0 + Duration::from_secs(12), look);
        assert_eq!(looks.get(), 3 * AGENTS.len());
    }
}

pub fn open(name: &str, cfg: &Config) -> Option<Box<dyn Pane>> {
    open_in(name, cfg, None)
}

/// `open`, starting in `cwd` where that means something: a terminal, Claude Code or Codex runs there, files shows
/// it (alt n from a pane opens the terminal in that pane's folder; a restored session reopens each pane in its own).
pub fn open_in(name: &str, cfg: &Config, cwd: Option<std::path::PathBuf>) -> Option<Box<dyn Pane>> {
    let cwd = cwd.filter(|d| d.is_dir());
    Some(match name {
        "terminal" | "shell" => Box::new(term::Term::shell(cfg, cwd)),
        "ai" | "chat" => Box::new(chat::Chat::new(cfg)),
        "music" => Box::new(music::Music::new(cfg)),
        "system" => Box::new(system::System::new()),
        "files" => Box::new(files::Files::new(cwd)),
        "notes" => Box::new(notes::Notes::new(cfg)),
        "calendar" => Box::new(calendar::Calendar::new()),
        "storage" => Box::new(storage::Storage::new()),
        "agents" => Box::new(agents::Agents::new(cfg)),
        "ais" => Box::new(ais::Ais::new(cfg)),
        "search" => Box::new(search::Search::new(cfg)),
        "home" => Box::new(home::Home::new()),
        "help" => Box::new(help::Help::new().prefix(&crate::config::prefix(cfg))),
        "themes" => Box::new(themes::Themes::new()),
        "alerts" => Box::new(alerts::Alerts::new()),
        "updates" => Box::new(updates::Updates::new()),
        "settings" => Box::new(settings::Settings::new(cfg)),
        _ => return open_agent(name, cwd),
    })
}

/// A CLI agent (claude, codex) in a terminal pane, working in `cwd` (None = the folder oriel runs in). None if
/// it isn't one or isn't installed.
pub fn open_agent(name: &str, cwd: Option<std::path::PathBuf>) -> Option<Box<dyn Pane>> {
    let &(app, prog, title) = AGENTS.iter().find(|a| a.0 == name)?;
    let path = which(prog)?;
    let icon = APPS.iter().find(|a| a.0 == name).map(|a| a.2).unwrap_or("term");
    let icon: &'static str = crate::ui::ICON_NAMES.iter().find(|n| **n == icon).copied().unwrap_or("term");
    // .cmd shims (npm installs on Windows) need cmd.exe to run them
    let p = path.to_string_lossy().to_string();
    let t = if cfg!(windows) && (p.ends_with(".cmd") || p.ends_with(".bat")) {
        term::Term::new(title, icon, "cmd.exe", vec!["/c".into(), p], cwd)
    } else {
        term::Term::new(title, icon, &p, vec![], cwd)
    };
    // it comes back as claude / codex, in its folder, when oriel restarts where you left off
    Some(Box::new(t.reopen_as(app)))
}
