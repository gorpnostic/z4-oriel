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
pub mod storage;
mod stub;
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
    ("music", 'm', "music", "music"),
    ("system", 's', "system", "system"),
    ("files", 'f', "files", "files"),
    ("notes", 'n', "notes", "notes"),
    ("storage", 'g', "storage", "storage"),
];

/// CLI agents that run as a terminal pane when installed: (app name, program, title).
const AGENTS: &[(&str, &str, &str)] = &[("claude", "claude", "claude code"), ("codex", "codex", "codex")];

pub fn available(name: &str) -> bool {
    match AGENTS.iter().position(|a| a.0 == name) {
        Some(i) => FOUND.lock().unwrap_or_else(|e| e.into_inner()).get(i, std::time::Instant::now(), |i| which(AGENTS[i].1).is_some()),
        None => true,
    }
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
        "notes" => Box::new(notes::Notes::new()),
        "calendar" => Box::new(calendar::Calendar::new()),
        "storage" => Box::new(storage::Storage::new()),
        "agents" => Box::new(agents::Agents::new(cfg)),
        "ais" => Box::new(ais::Ais::new(cfg)),
        "home" => Box::new(home::Home::new()),
        "help" => Box::new(help::Help::new().prefix(&cfg.prefix)),
        "themes" => Box::new(themes::Themes::new()),
        "alerts" => Box::new(alerts::Alerts::new()),
        "updates" => Box::new(updates::Updates::new()),
        _ => {
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
            Box::new(t.reopen_as(app))
        }
    })
}
