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
    /// What oriel opens on: "last" (your tabs, splits and the app and chat you were in, as you left them), an app
    /// ("ai", "music", "terminal"...) or "home".
    pub startup: String,
    /// true = plain-text icons (for terminals without a Nerd Font)
    pub plain_icons: bool,
    /// A desktop notification for alerts while the terminal isn't the window you're in ([alerts] picks which).
    pub desktop_notifications: bool,
    /// The sidebar shows at start (alt s and /sidebar hide it, and that's remembered).
    pub sidebar: bool,
    /// Look for a newer oriel once a day at start.
    pub update_check: bool,
    /// Where notes live (plain .md files); empty = oriel's data folder.
    pub notes_folder: String,
    pub ai: AiConfig,
    pub music: MusicConfig,
    /// The agents app's lead mode: which agent orchestrates, and its caps.
    pub lead: LeadConfig,
    /// Workers a lead can hand tasks to. Empty = one per installed agent (claude, codex, kimi).
    pub roster: Vec<RosterEntry>,
    pub alerts: AlertsConfig,
    pub calendar: CalendarConfig,
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
    /// ("cargo test", "npm run build"…). "" = detect (Cargo → cargo check, go.mod → go build ./..., package.json
    /// with a build script → install + build), "none" = off.
    pub gate: String,
    /// A repo's own gate, keyed by its top folder (`[lead.gates]`): wins over `gate` for runs in that repo.
    pub gates: std::collections::BTreeMap<String, String>,
    pub gate_timeout_s: u32,
    /// Seconds between starting two workers on the same model, so the later one reuses the first one's cache.
    pub stagger_s: u32,
    /// A worker that shows no sign of life for this long is treated as hung.
    pub hung_after_s: u32,
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
    /// What coding agents may do in new chats: "ask", "edits" (default), "auto", "plan" (read-only) or "bypass"
    /// (anything). Set in settings or with /perms default <mode>; shift+tab and /perms change only one chat.
    pub perms: String,
    /// How hard coding agents think (/effort): "low" … "max", or "ultracode" (max plus Claude Code's multi-agent
    /// mode). Empty = the agent's own default.
    #[serde(default)]
    pub effort: String,
    /// The model picked for each AI with /model or in settings (e.g. claude = "opus"); new chats use it.
    /// Unset or "" = the AI's own default.
    #[serde(default)]
    pub models: std::collections::BTreeMap<String, String>,
    pub ollama_url: String,
    /// Any OpenAI-compatible endpoint (OpenAI, OpenRouter, LM Studio, llama.cpp server...).
    pub openai_url: String,
    /// Keys can also come from OPENAI_API_KEY / ANTHROPIC_API_KEY.
    pub openai_key: String,
    pub anthropic_key: String,
    /// The API AIs' built-in default models. Older configs set them here: they're read, moved into `models` on
    /// load and never written back, so each AI's model lives in one place.
    #[serde(skip_serializing)]
    pub ollama_model: String,
    #[serde(skip_serializing)]
    pub openai_model: String,
    #[serde(skip_serializing)]
    pub anthropic_model: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct MusicConfig {
    /// Folders to scan for audio; empty = the OS music folder.
    pub folders: Vec<String>,
    /// "auto" = the audio-player app's library when it's there (Windows), else the folders; "folders" = always
    /// the folders.
    pub source: String,
}

/// The event center: the alerts app, its toasts and desktop notifications.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AlertsConfig {
    /// Which kinds also make a desktop notification when you're in another window (with desktop_notifications
    /// on): agent_done, needs_you, approval, build_failed, calendar, download, memory, usage, update.
    pub desktop: Vec<String>,
    /// Warn when memory is this full (%); it can warn again once it's 7 points under.
    pub memory_pct: u32,
    /// Warn when an AI's plan window is this used (%): the short windows, and the weekly one.
    pub usage_pct: u32,
    pub weekly_usage_pct: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct CalendarConfig {
    /// A reminder this many minutes before a plan with a time (0 = only when it starts).
    pub remind_before_min: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            theme: String::new(),
            shell: String::new(),
            prefix: "ctrl+space".into(),
            startup: "last".into(),
            plain_icons: false,
            desktop_notifications: true,
            sidebar: true,
            update_check: true,
            notes_folder: String::new(),
            ai: AiConfig::default(),
            music: MusicConfig::default(),
            lead: LeadConfig::default(),
            roster: vec![],
            alerts: AlertsConfig::default(),
            calendar: CalendarConfig::default(),
        }
    }
}

impl Default for LeadConfig {
    fn default() -> Self {
        LeadConfig {
            agent: String::new(),
            model: String::new(),
            budget_usd: 2.0,
            run_budget_usd: 8.0,
            max_parallel: 3,
            protocol: String::new(),
            gate: String::new(),
            gates: Default::default(),
            gate_timeout_s: 900,
            stagger_s: 5,
            hung_after_s: 300,
        }
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
            openai_url: "https://api.openai.com/v1".into(),
            openai_key: String::new(),
            anthropic_key: String::new(),
            ollama_model: "llama3.2".into(),
            openai_model: "gpt-4o-mini".into(),
            anthropic_model: "claude-sonnet-5".into(),
        }
    }
}

impl Default for MusicConfig {
    fn default() -> Self {
        MusicConfig { folders: vec![], source: "auto".into() }
    }
}

impl Default for AlertsConfig {
    fn default() -> Self {
        // the ones that want you; "finished" stays inside oriel (several agents would pop one toast each)
        AlertsConfig { desktop: ["needs_you", "approval", "build_failed", "calendar"].map(String::from).to_vec(), memory_pct: 92, usage_pct: 85, weekly_usage_pct: 90 }
    }
}

impl Default for CalendarConfig {
    fn default() -> Self {
        CalendarConfig { remind_before_min: 10 }
    }
}

pub fn dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("oriel")
}

/// ORIEL_DATA_DIR, when it says something: a separate profile (demos, screenshots, testing). An empty one is unset,
/// not the current folder.
fn profile() -> Option<PathBuf> {
    std::env::var_os("ORIEL_DATA_DIR").filter(|p| !p.is_empty()).map(PathBuf::from)
}

/// Where apps keep their data (notes, chats, state).
pub fn data_dir() -> PathBuf {
    // a profile keeps chats, notes, memory and the first-run marker
    let d = profile().unwrap_or_else(|| dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("oriel"));
    let _ = std::fs::create_dir_all(&d);
    d
}

/// config.toml: a profile has its own (its first-run setup saves there, not over your main config).
pub fn path() -> PathBuf {
    profile().unwrap_or_else(dir).join("config.toml")
}

/// The config to run with: config.toml; the defaults if it's missing; if it doesn't parse, every setting in it that
/// does (`load_checked` says what's wrong).
pub fn load() -> Config {
    match std::fs::read_to_string(path()) {
        Ok(s) => from_text(&s),
        Err(_) => defaults(),
    }
}

/// config.toml's text as the config to run with: all of it, or when a value is wrong (a hand edit like
/// `max_parallel = "3"`), the rest of it. Text that isn't TOML at all gives the defaults.
pub fn from_text(s: &str) -> Config {
    with_theme(migrate(parse(s).ok().or_else(|| salvage(s)).unwrap_or_default()))
}

/// Every setting in `s` that fits, dropping the ones that don't: top-level values, and values inside a section
/// ([ai], [lead]...) one by one. None = not TOML.
fn salvage(s: &str) -> Option<Config> {
    let all: toml::Table = s.parse().ok()?;
    let fits = |t: &toml::Table| toml::Value::Table(t.clone()).try_into::<Config>().is_ok();
    let mut good = toml::Table::new();
    for (k, v) in all {
        let mut with = good.clone();
        with.insert(k.clone(), v.clone());
        if fits(&with) {
            good = with;
        } else if let toml::Value::Table(section) = v {
            let mut kept = toml::Table::new();
            for (k2, v2) in section {
                let mut sec = kept.clone();
                sec.insert(k2, v2);
                let mut with = good.clone();
                with.insert(k.clone(), toml::Value::Table(sec.clone()));
                if fits(&with) {
                    kept = sec;
                }
            }
            good.insert(k, toml::Value::Table(kept));
        }
    }
    toml::Value::Table(good).try_into().ok()
}

/// What a missing config.toml means: the defaults, theme filled in.
pub fn defaults() -> Config {
    with_theme(Config::default())
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
    Ok(with_theme(migrate(c)))
}

/// Older configs: `ai.ollama_model` / `openai_model` / `anthropic_model` become that AI's entry in `ai.models`
/// (unless it has one), so each AI's model lives in one place. The old fields go back to the built-in defaults
/// and aren't written again.
fn migrate(mut c: Config) -> Config {
    let d = AiConfig::default();
    let ai = &mut c.ai;
    for (id, old, def) in [("ollama", &mut ai.ollama_model, d.ollama_model), ("openai", &mut ai.openai_model, d.openai_model), ("anthropic", &mut ai.anthropic_model, d.anthropic_model)] {
        let v = std::mem::replace(old, def.clone());
        if !v.trim().is_empty() && v != def && ai.models.get(id).is_none_or(|m| m.trim().is_empty()) {
            ai.models.insert(id.into(), v.trim().to_string());
        }
    }
    c
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

thread_local! {
    /// Why config.toml isn't being saved (it doesn't parse: the message says which line), while that lasts. The
    /// app sets it; the settings app shows it as a banner. The UI is one thread; thread-local keeps tests apart.
    static BROKEN: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

pub fn broken() -> Option<String> {
    BROKEN.with(|b| b.borrow().clone())
}

pub fn set_broken(e: Option<String>) {
    BROKEN.with(|b| *b.borrow_mut() = e);
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

/// The merge gate for runs in `repo`: its own (`[lead.gates]`), else the global `gate`.
pub fn gate_for(l: &LeadConfig, repo: &Path) -> String {
    let want = path_key(repo);
    l.gates.iter().find(|(k, _)| path_key(Path::new(k.as_str())) == want).map(|(_, v)| v.clone()).unwrap_or_else(|| l.gate.clone())
}

/// A folder as a comparable key: forward slashes, no trailing one, case-folded on Windows.
pub fn path_key(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/');
    if cfg!(windows) { s.to_lowercase() } else { s.to_string() }
}

/// Keys apps use with ctrl, so the prefix can't be one of them: (letter, what needs it).
pub const CTRL_TAKEN: &[(char, &str)] = &[
    ('s', "save in the agents forms"),
    ('x', "chat's send-now (ctrl+x s)"),
    ('o', "chat's expand-everything"),
    ('n', "new chat and new note"),
    ('e', "notes' edit/preview switch"),
    ('d', "delete a chat or a note"),
    ('z', "undo in notes"),
    ('y', "redo in notes"),
    ('r', "regenerate in chat"),
    ('c', "copy, and stopping a program in a terminal"),
    ('u', "clearing the input box"),
    ('v', "pasting an image"),
    ('h', "backspace in most terminals"),
    ('i', "tab (terminals send the same key)"),
    ('j', "enter in some terminals"),
    ('m', "enter (terminals send the same key)"),
];

/// Can `spec` be the prefix? Only ctrl+<letter> or ctrl+space: a bare key would eat every one you type, and alt
/// keys are oriel's own bindings. `strict` also refuses a ctrl key an app needs, naming it.
pub fn check_prefix(spec: &str, strict: bool) -> Result<(), String> {
    let s = spec.trim().to_lowercase();
    let bad = || format!("'{}' isn't ctrl+<letter> or ctrl+space", spec.trim());
    let Some(key) = s.strip_prefix("ctrl+") else { return Err(bad()) };
    if key == "space" {
        return Ok(());
    }
    let mut cs = key.chars();
    match (cs.next(), cs.next()) {
        (Some(c), None) if c.is_ascii_lowercase() => match CTRL_TAKEN.iter().find(|t| t.0 == c) {
            Some((_, what)) if strict => Err(format!("ctrl+{c} is {what}")),
            _ => Ok(()),
        },
        _ => Err(bad()),
    }
}

/// The prefix in effect: the config's when it's usable, else ctrl+space.
pub fn prefix(c: &Config) -> String {
    if check_prefix(&c.prefix, false).is_ok() { c.prefix.trim().to_lowercase() } else { "ctrl+space".into() }
}

/// A command line as program + args; the program may be "quoted" (a path with spaces).
pub fn split_command(s: &str) -> (String, Vec<String>) {
    let s = s.trim();
    if let Some((prog, rest)) = s.strip_prefix('"').and_then(|r| r.split_once('"')) {
        return (prog.to_string(), rest.split_whitespace().map(String::from).collect());
    }
    let mut parts = s.split_whitespace().map(String::from);
    let prog = parts.next().unwrap_or_default();
    (prog, parts.collect())
}

pub fn default_shell(c: &Config) -> (String, Vec<String>) {
    if !c.shell.trim().is_empty() {
        return split_command(&c.shell);
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

    /// ollama_model / openai_model / anthropic_model move into ai.models on load (an existing entry wins) and
    /// aren't written back; new settings get their defaults in an old file.
    #[test]
    fn config_old_model_fields_move_into_models() {
        let p = scratch("migrate");
        std::fs::write(&p, "[ai]\nollama_model = \"mistral\"\nopenai_model = \"gpt-4o-mini\"\nanthropic_model = \"claude-opus-5-5\"\n\n[ai.models]\nanthropic = \"claude-haiku-4-5\"\n").unwrap();
        let c = load_from(&p).unwrap();
        assert_eq!(c.ai.models.get("ollama").map(String::as_str), Some("mistral"));
        assert_eq!(c.ai.models.get("openai"), None, "the built-in default isn't copied");
        assert_eq!(c.ai.models.get("anthropic").map(String::as_str), Some("claude-haiku-4-5"), "the models entry wins");
        assert_eq!(c.ai.ollama_model, "llama3.2", "the old field is the built-in fallback again");
        assert!(c.sidebar && c.update_check && c.music.source == "auto" && c.lead.hung_after_s == 300 && c.calendar.remind_before_min == 10);
        let (_, saved) = update_at(&p, &c, |c| c.theme = "ocean".into());
        assert!(saved.is_ok());
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(!text.contains("ollama_model") && text.contains("ollama = \"mistral\""), "{text}");
        assert_eq!(load_from(&p).unwrap().ai.models.get("ollama").map(String::as_str), Some("mistral"));
    }

    #[test]
    fn config_prefix_gates_and_shell() {
        assert!(check_prefix("ctrl+space", true).is_ok() && check_prefix("Ctrl+B", true).is_ok());
        assert!(check_prefix("b", false).is_err(), "a bare key would eat every b");
        assert!(check_prefix("alt+b", false).is_err());
        assert!(check_prefix("ctrl+s", false).is_ok());
        assert!(check_prefix("ctrl+s", true).unwrap_err().contains("save"), "names the clash");
        assert_eq!(prefix(&Config { prefix: "b".into(), ..Config::default() }), "ctrl+space");
        assert_eq!(prefix(&Config { prefix: "ctrl+a".into(), ..Config::default() }), "ctrl+a");
        // a repo's own gate wins, matched whatever the slashes (and case on Windows)
        let mut l = LeadConfig { gate: "cargo test".into(), ..LeadConfig::default() };
        l.gates.insert("/w/web-app".into(), "npm run build".into());
        assert_eq!(gate_for(&l, Path::new("/w/web-app/")), "npm run build");
        assert_eq!(gate_for(&l, Path::new("/w/other")), "cargo test");
        // a quoted program path with spaces
        assert_eq!(split_command("\"C:/Program Files/Git/bin/bash.exe\" --login -i"), ("C:/Program Files/Git/bin/bash.exe".into(), vec!["--login".to_string(), "-i".into()]));
        assert_eq!(split_command("pwsh.exe -NoLogo"), ("pwsh.exe".into(), vec!["-NoLogo".to_string()]));
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
