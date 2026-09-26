//! ~/.config/oriel/config.toml (Linux) or %APPDATA%\oriel\config.toml (Windows). Every field is optional.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
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
    /// Where notes live (plain .md files); empty = oriel's data folder.
    pub notes_folder: String,
    pub ai: AiConfig,
    pub music: MusicConfig,
    /// The agents app's lead mode: which agent orchestrates, and its caps.
    pub lead: LeadConfig,
    /// Workers a lead can hand tasks to. Empty = one per installed agent (claude, codex, kimi).
    pub roster: Vec<RosterEntry>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
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

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct AiConfig {
    /// AI for new chats: "claude", "codex", "ollama", "openai", "anthropic". Empty = the first one this
    /// computer has.
    pub provider: String,
    /// What coding agents may do in chat: "ask", "edits" (default), "plan" (read-only) or "bypass" (anything).
    /// Set with /perms; applies to every new chat.
    pub perms: String,
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

#[derive(Serialize, Deserialize, Clone, Debug)]
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

pub fn load() -> Config {
    let mut c: Config = std::fs::read_to_string(path()).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default();
    if c.theme.is_empty() {
        // the animated ultra theme by default; on Omarchy follow the desktop theme instead
        c.theme = if crate::theme::omarchy_dir().is_some() { "omarchy".into() } else { "ultra".into() };
    }
    c
}

pub fn save(c: &Config) {
    if cfg!(test) {
        return; // tests never touch the real config
    }
    let _ = std::fs::create_dir_all(dir());
    if let Ok(s) = toml::to_string_pretty(c) {
        let _ = std::fs::write(path(), s);
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
