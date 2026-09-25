//! ~/.config/oriel/config.toml (Linux) or %APPDATA%\oriel\config.toml (Windows). Every field is optional.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    /// Theme name; empty = "omarchy" on Omarchy, else "oriel".
    pub theme: String,
    /// Shell for terminal panes; empty = pwsh/powershell on Windows, $SHELL on Linux/macOS.
    pub shell: String,
    /// Prefix key for tmux-style bindings, e.g. "ctrl+space", "ctrl+b", "ctrl+a".
    pub prefix: String,
    /// What oriel opens on: an app ("ai", "music", "terminal"...) or "home".
    pub startup: String,
    pub ai: AiConfig,
    pub music: MusicConfig,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct AiConfig {
    /// AI for new chats: "claude", "codex", "ollama", "openai", "anthropic". Empty = the first one this
    /// computer has.
    pub provider: String,
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
            ai: AiConfig::default(),
            music: MusicConfig::default(),
        }
    }
}

impl Default for AiConfig {
    fn default() -> Self {
        AiConfig {
            provider: String::new(),
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
        c.theme = if crate::theme::omarchy_dir().is_some() { "omarchy".into() } else { "oriel".into() };
    }
    c
}

pub fn save(c: &Config) {
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
