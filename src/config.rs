//! ~/.config/oriel/config.toml (Linux) or %APPDATA%\oriel\config.toml (Windows). Every field is optional.
//!
//! One writer: the app (App::edit_config, reached from panes by `cx.edit_config(|c| ...)`). It re-reads the file,
//! changes only what was asked and writes it back atomically, so a hand edit or a second window isn't overwritten
//! by a stale copy. A file that doesn't parse is never written over.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Theme name; empty = "omarchy" on Omarchy, else "ultra".
    pub theme: String,
    /// Shell for terminal panes; empty = pwsh/powershell on Windows, $SHELL on Linux/macOS.
    pub shell: String,
    /// Prefix key for tmux-style bindings, e.g. "ctrl+space", "ctrl+b", "ctrl+a".
    pub prefix: String,
    /// What oriel opens on: an app ("ai", "music", "terminal"...) or "home".
    pub startup: String,
    /// true = plain-text icons (for terminals without a Nerd Font)
    pub plain_icons: bool,
    /// A desktop notification for alerts while the terminal isn't the window you're in.
    pub desktop_notifications: bool,
    /// Where notes live (plain .md files); empty = oriel's data folder.
    pub notes_folder: String,
    pub ai: AiConfig,
    pub music: MusicConfig,
    /// The agents app's lead mode: which agent orchestrates, and its caps.
    pub lead: LeadConfig,
    /// Workers a lead can hand tasks to. Empty = one per installed agent (claude, codex, kimi).
    pub roster: Vec<RosterEntry>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct LeadConfig {
    /// Who orchestrates: "claude", "codex", "kimi"… Empty = the last one used, else the first installed.
    pub agent: String,
    /// Empty = the agent's default model.
    pub model: String,
    /// The lead's own spend cap per run in USD (claude --max-budget-usd).
    pub budget_usd: f64,
    /// Cap for a whole run: lead + every worker. The lead can't spawn more work once it's reached.
    pub run_budget_usd: f64,
    /// Headless workers running at once (1-5); more tasks wait in TODO.
    pub max_parallel: u32,
    /// How the lead calls oriel: "" = MCP tools when its CLI can load them, else JSON actions; "mcp"; "text".
    pub protocol: String,
    /// The merge gate, run on every merged candidate before the integration branch moves: a shell command
    /// ("cargo test", "npm run build"…). "" = detect (Cargo → cargo check, go.mod → go build ./...), "none" = off.
    pub gate: String,
    pub gate_timeout_s: u32,
    /// Seconds between starting two workers on the same model, so the later one reuses the first one's cache.
    pub stagger_s: u32,
}

/// One worker in the roster, e.g. `[[roster]] name = "codex" agent = "codex" tier = "mid" good_at = "refactors"`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct RosterEntry {
    pub name: String,
    /// "claude", "codex" or "kimi"
    pub agent: String,
    /// Empty = the agent's default.
    pub model: String,
    /// "cheap", "mid" or "premium": the lead prefers cheap tiers for simple work.
    pub tier: String,
    /// What to hand it, in a few words (the lead reads this).
    pub good_at: String,
    /// claude --max-turns (0 = no cap).
    pub max_turns: u32,
    /// Spend cap per task in USD (claude --max-budget-usd; others are stopped when their cost passes it). 0 = none.
    pub budget_usd: f64,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AiConfig {
    /// AI for new chats: "claude", "codex", "ollama", "openai", "anthropic". Empty = the first one this
    /// computer has.
    pub provider: String,
    /// What coding agents may do in chat: "ask", "edits" (default), "plan" (read-only) or "bypass" (anything).
    /// Set with /perms; applies to every new chat.
    pub perms: String,
    /// How hard coding agents think (/effort): "low" … "max", or "ultracode" (max plus Claude Code's multi-agent
    /// mode). Empty = the agent's own default.
    #[serde(default)]
    pub effort: String,
    /// The model picked for each AI with /model (e.g. claude = "opus"); new chats use it.
    #[serde(default)]
    pub models: std::collections::BTreeMap<String, String>,
    pub ollama_url: String,
    pub ollama_model: String,
    /// Any OpenAI-compatible endpoint (OpenAI, OpenRouter, LM Studio, llama.cpp server...).
    pub openai_url: String,
    pub openai_model: String,
    /// Keys can also come from OPENAI_API_KEY / ANTHROPIC_API_KEY.
    pub openai_key: String,
    pub anthropic_model: String,
    pub anthropic_key: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct MusicConfig {
    /// Folders to scan for audio; empty = the OS music folder.
    pub folders: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            theme: String::new(),
            shell: String::new(),
            prefix: "ctrl+space".into(),
            startup: "ai".into(),
            plain_icons: false,
            desktop_notifications: true,
            notes_folder: String::new(),
            ai: AiConfig::default(),
            music: MusicConfig::default(),
            lead: LeadConfig::default(),
            roster: vec![],
        }
    }
}

impl Default for LeadConfig {
    fn default() -> Self {
        LeadConfig { agent: String::new(), model: String::new(), budget_usd: 2.0, run_budget_usd: 8.0, max_parallel: 3, protocol: String::new(), gate: String::new(), gate_timeout_s: 900, stagger_s: 5 }
    }
}

impl Default for RosterEntry {
    fn default() -> Self {
        RosterEntry { name: String::new(), agent: "claude".into(), model: String::new(), tier: "mid".into(), good_at: String::new(), max_turns: 40, budget_usd: 1.5, enabled: true }
    }
}

impl Default for AiConfig {
    fn default() -> Self {
        AiConfig {
            provider: String::new(),
            perms: "edits".into(),
            effort: String::new(),
            models: Default::default(),
            ollama_url: "http://127.0.0.1:11434".into(),
            ollama_model: "llama3.2".into(),
            openai_url: "https://api.openai.com/v1".into(),
            openai_model: "gpt-4o-mini".into(),
            openai_key: String::new(),
            anthropic_model: "claude-sonnet-5".into(),
            anthropic_key: String::new(),
        }
    }
}

impl Default for MusicConfig {
    fn default() -> Self {
        MusicConfig { folders: vec![] }
    }
}

pub fn dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("oriel")
}

/// Where apps keep their data (notes, chats, state).
pub fn data_dir() -> PathBuf {
    // ORIEL_DATA_DIR: a separate profile (demos, screenshots, testing) — chats, notes and memory live there
    let d = match std::env::var_os("ORIEL_DATA_DIR") {
        Some(p) => PathBuf::from(p),
        None => dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("oriel"),
    };
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn path() -> PathBuf {
    dir().join("config.toml")
}

/// The config to run with: config.toml, or the defaults if it's missing or doesn't parse (`load_checked` says why).
pub fn load() -> Config {
    load_checked().unwrap_or_else(|_| with_theme(Config::default()))
}

/// config.toml; no file = the defaults. Err = it's there but doesn't parse (the message says which line).
pub fn load_checked() -> Result<Config, String> {
    load_from(&path())
}

pub fn load_from(path: &Path) -> Result<Config, String> {
    let c = match std::fs::read_to_string(path) {
        Ok(s) => parse(&s)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => return Err(format!("can't read {}: {e}", path.display())),
    };
    Ok(with_theme(c))
}

/// config.toml's text as a Config. The error is one line: where, and what's wrong ("line 3: invalid string").
fn parse(s: &str) -> Result<Config, String> {
    toml::from_str(s).map_err(|e| {
        let at = e.span().map(|r| format!("line {}: ", s.get(..r.start).unwrap_or(s).matches('\n').count() + 1)).unwrap_or_default();
        format!("config.toml {at}{}", e.message().split_whitespace().collect::<Vec<_>>().join(" "))
    })
}

fn with_theme(mut c: Config) -> Config {
    if c.theme.is_empty() {
        // the animated ultra theme by default; on Omarchy follow the desktop theme instead
        c.theme = if crate::theme::omarchy_dir().is_some() { "omarchy".into() } else { "ultra".into() };
    }
    c
}

/// Write `c` atomically (a temp file renamed over the old one), unless the file there doesn't parse: then nothing
/// is written, so one typo never costs you the rest of the file.
pub fn save_to(path: &Path, c: &Config) -> Result<(), String> {
    if let Ok(s) = std::fs::read_to_string(path) {
        parse(&s).map_err(|e| format!("{e} (fix it and oriel picks it up)"))?;
    }
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let text = toml::to_string_pretty(c).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, path)).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("couldn't save {}: {e}", path.display())
    })
}

/// The one way config.toml changes: re-read it (so edits made elsewhere survive), apply `f`, write it back, and
/// return what's now in effect. If the file doesn't parse, nothing is written: `f` is applied to `current` instead
/// (it holds for this session) and the error comes back with it.
pub fn update_at(path: &Path, current: &Config, f: impl FnOnce(&mut Config)) -> (Config, Result<(), String>) {
    match load_from(path) {
        Ok(mut c) => {
            f(&mut c);
            let saved = save_to(path, &c);
            (c, saved)
        }
        Err(e) => {
            let mut c = current.clone();
            f(&mut c);
            (c, Err(format!("{e} (fix it and oriel picks it up)")))
        }
    }
}

/// Where notes live: `notes_folder` (~ = your home folder), else oriel's data folder. The notes app and chat's
/// /note both use this.
pub fn notes_dir(c: &Config) -> PathBuf {
    let custom = c.notes_folder.trim();
    match custom.strip_prefix('~') {
        _ if custom.is_empty() => data_dir().join("notes"),
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest.trim_start_matches(['/', '\\'])),
        None => PathBuf::from(custom),
    }
}

pub fn default_shell(c: &Config) -> (String, Vec<String>) {
    if !c.shell.trim().is_empty() {
        let mut parts = c.shell.split_whitespace().map(String::from);
        let prog = parts.next().unwrap_or_default();
        return (prog, parts.collect());
    }
    if cfg!(windows) {
        for p in ["pwsh.exe", "powershell.exe"] {
            if which(p).is_some() {
                return (p.into(), vec!["-NoLogo".into()]);
            }
        }
        ("cmd.exe".into(), vec![])
    } else {
        (std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into()), vec![])
    }
}

/// Find a program on PATH (with .exe/.cmd on Windows).
pub fn which(prog: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<&str> = if cfg!(windows) && !prog.contains('.') { vec![".exe", ".cmd", ".bat", ""] } else { vec![""] };
    for dir in std::env::split_paths(&path) {
        for e in &exts {
            let p = dir.join(format!("{prog}{e}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::path::absolute(format!("target/test-scratch/config/{name}")).unwrap();
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("config.toml")
    }

    #[test]
    fn config_update_keeps_what_others_saved() {
        let p = scratch("update");
        // what a chat saved after the app loaded its copy
        std::fs::write(&p, "[ai]\nperms = \"bypass\"\nanthropic_key = \"sk-test\"\n\n[[roster]]\nname = \"kimi\"\nagent = \"kimi\"\n").unwrap();
        let stale = Config::default();
        let (c, saved) = update_at(&p, &stale, |c| c.theme = "ocean".into());
        assert!(saved.is_ok());
        let disk = load_from(&p).unwrap();
        assert_eq!((disk.theme.as_str(), disk.ai.perms.as_str(), disk.ai.anthropic_key.as_str()), ("ocean", "bypass", "sk-test"));
        assert_eq!(disk.roster.len(), 1);
        assert_eq!(c, disk, "what's in effect is what's on disk");
        assert!(!p.with_extension("toml.tmp").exists(), "written through a temp file that's gone");
    }

    #[test]
    fn config_typo_is_reported_and_never_overwritten() {
        let p = scratch("broken");
        let text = "theme = \"ocean\"\nprefix = ctrl+b\n\n[ai]\nanthropic_key = \"sk-keep\"\n";
        std::fs::write(&p, text).unwrap();
        let e = load_from(&p).unwrap_err();
        assert!(e.contains("line 2"), "{e}");
        // a change still applies for this session, but the file is left alone
        let mut current = Config::default();
        current.theme = "dracula".into();
        let (c, saved) = update_at(&p, &current, |c| c.ai.perms = "plan".into());
        assert!(saved.unwrap_err().contains("line 2"));
        assert_eq!((c.theme.as_str(), c.ai.perms.as_str()), ("dracula", "plan"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), text);
        assert!(save_to(&p, &Config::default()).is_err(), "save_to refuses too");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), text);
        // a wrong type is caught the same way
        std::fs::write(&p, "[lead]\nmax_parallel = \"3\"\n").unwrap();
        assert!(load_from(&p).unwrap_err().contains("line 2"));
        // no file at all is just the defaults
        let _ = std::fs::remove_file(&p);
        assert_eq!(load_from(&p).unwrap().prefix, "ctrl+space");
    }

    #[test]
    fn config_notes_dir() {
        let mut c = Config::default();
        assert_eq!(notes_dir(&c), data_dir().join("notes"));
        c.notes_folder = "~/vault".into();
        assert_eq!(notes_dir(&c), dirs::home_dir().unwrap_or_default().join("vault"));
        c.notes_folder = " /x/notes ".into();
        assert_eq!(notes_dir(&c), PathBuf::from("/x/notes"));
    }
}
