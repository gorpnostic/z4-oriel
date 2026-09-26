//! Every setting the settings app shows: its section, name, where it lives in config.toml, its control, when a
//! change applies, and how it's read and written. Values travel as text ("true"/"false" for a switch, numbers as
//! typed, a list one item per line), so a row's default is simply what it reads from the default config.

use crate::config::Config;
use crate::panes::chat::providers::PROVIDERS;
use std::sync::Arc;

/// The sections, in the sidebar's order: (name, icon).
pub(super) const CATS: &[(&str, &str)] = &[
    ("general", "window"),
    ("AI chat", "ai"),
    ("providers & keys", "cloud"),
    ("lead mode", "robot"),
    ("roster", "you"),
    ("alerts", "bell"),
    ("folders", "files"),
    ("tools", "gauge"),
];

/// When a change takes effect.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum When {
    Now,
    NewChats,
    NewTerminals,
    NextStart,
    NextMessage,
    NextRun,
    NextMerge,
    /// a link or an action: nothing to apply
    Never,
}

impl When {
    pub fn applies(self) -> &'static str {
        match self {
            When::Now => "now",
            When::NewChats => "to new chats (and empty open ones)",
            When::NewTerminals => "to new terminals",
            When::NextStart => "from the next start",
            When::NextMessage => "from the next message",
            When::NextRun => "from the next lead run",
            When::NextMerge => "from the next merge",
            When::Never => "",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Unit {
    None,
    Secs,
    Pct,
    Min,
    Usd,
}

/// Where a link row goes.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Jump {
    App(&'static str),
    /// that app, then this key in it (the agents' roster, a view of your AIs)
    AppKey(&'static str, char),
    Tour,
}

/// What an action row does.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Do {
    ResetRoster,
    TestNotify,
    ConfigFile,
}

pub(super) enum Ctl {
    Toggle,
    /// ← → over (value, label); `custom`: enter types a value of your own
    Choice { opts: Vec<(String, String)>, custom: bool },
    Number { min: f64, max: f64, step: f64, unit: Unit },
    Text,
    /// typed out of sight, shown masked; `env` can hold it instead of the file
    Secret { env: &'static str },
    Folder,
    /// folders, one per line: a adds, x removes, J/K reorder
    List,
    /// the prefix: enter, then press it
    Key,
    Link(Jump),
    Act(Do),
}

type Get = Box<dyn Fn(&Config) -> String>;
pub(super) type Set = Arc<dyn Fn(&mut Config, &str) + Send + Sync>;

pub(super) struct Row {
    /// stable name ("ai.perms"): the palette, /settings and tests use it
    pub id: String,
    pub cat: usize,
    pub label: String,
    /// where it lives in config.toml, e.g. "[ai] perms"
    pub key: String,
    pub desc: String,
    pub when: When,
    pub ctl: Ctl,
    pub get: Get,
    pub set: Option<Set>,
}

impl Row {
    fn new(cat: usize, id: impl Into<String>, label: impl Into<String>, key: impl Into<String>, desc: impl Into<String>, when: When, ctl: Ctl) -> Row {
        Row { id: id.into(), cat, label: label.into(), key: key.into(), desc: desc.into(), when, ctl, get: Box::new(|_| String::new()), set: None }
    }
    fn io(mut self, get: impl Fn(&Config) -> String + 'static, set: impl Fn(&mut Config, &str) + Send + Sync + 'static) -> Row {
        self.get = Box::new(get);
        self.set = Some(Arc::new(set));
        self
    }
    /// A setting (not a link or an action).
    pub fn is_setting(&self) -> bool {
        self.set.is_some()
    }
}

/// What this machine has, for the choices (looked up once per settings pane; the AIs and the repo arrive later
/// from background threads).
#[derive(Clone, Debug, Default)]
pub(super) struct Env {
    pub themes: Vec<String>,
    /// what oriel can open on: (app, label)
    pub starts: Vec<(String, String)>,
    /// shells found on PATH: (the shell setting, label); the first is "" = the default
    pub shells: Vec<(String, String)>,
    /// lead agents installed here
    pub kinds: Vec<&'static str>,
    /// the AIs chat can use (None = still looking)
    pub avail: Option<Vec<&'static str>>,
    /// models installed in Ollama
    pub ollama_tags: Vec<String>,
    /// the git repo oriel started in: (its top folder, the gate detected for it)
    pub repo: Option<(String, Option<String>)>,
}

impl Env {
    pub fn scan() -> Env {
        let mut starts: Vec<(String, String)> = crate::app::SIDEBAR.iter().map(|s| (s.0.to_string(), s.2.to_string())).collect();
        starts.push(("home".into(), "home screen".into()));
        starts.push(("updates".into(), "updates".into()));
        for (app, label) in [("claude", "claude code"), ("codex", "codex")] {
            if crate::panes::available(app) {
                starts.push((app.into(), label.into()));
            }
        }
        Env { themes: crate::theme::names(), starts, shells: shells(), kinds: crate::panes::agents::installed_kinds(), ..Env::default() }
    }
}

/// The shell used when none is set, as a command line ("pwsh.exe -NoLogo").
pub(super) fn default_shell() -> String {
    let (p, a) = crate::config::default_shell(&Config::default());
    std::iter::once(p).chain(a).collect::<Vec<_>>().join(" ")
}

/// What a row reads from the default config: its default ("r" puts it back; ● marks a row that differs).
pub(super) fn all_default(r: &Row) -> String {
    static DEFAULTS: std::sync::OnceLock<Config> = std::sync::OnceLock::new();
    (r.get)(DEFAULTS.get_or_init(crate::config::defaults))
}

/// The default, then every shell found here: (the shell setting, label).
fn shells() -> Vec<(String, String)> {
    let mut v = vec![(String::new(), format!("default ({})", default_shell()))];
    let has = |p: &str| crate::config::which(p).is_some();
    if cfg!(windows) {
        for (prog, value, label) in [("pwsh", "pwsh.exe -NoLogo", "PowerShell 7"), ("powershell", "powershell.exe -NoLogo", "Windows PowerShell"), ("cmd", "cmd.exe", "cmd")] {
            if has(prog) {
                v.push((value.into(), label.into()));
            }
        }
        // Git for Windows' bash (never System32's bash.exe, which is WSL's launcher)
        let from_git = crate::config::which("git").and_then(|g| Some(g.parent()?.parent()?.join("bin").join("bash.exe")));
        if let Some(b) = [Some(std::path::PathBuf::from(r"C:\Program Files\Git\bin\bash.exe")), from_git].into_iter().flatten().find(|p| p.is_file()) {
            v.push((format!("\"{}\" --login -i", b.display()), "Git bash".into()));
        }
        for (prog, value, label) in [("wsl", "wsl.exe", "WSL"), ("nu", "nu", "nushell")] {
            if has(prog) {
                v.push((value.into(), label.into()));
            }
        }
    } else {
        for (prog, label) in [("bash", "bash"), ("zsh", "zsh"), ("fish", "fish"), ("nu", "nushell"), ("sh", "sh")] {
            if let Some(p) = crate::config::which(prog) {
                v.push((p.display().to_string(), label.into()));
            }
        }
    }
    v
}

fn b(v: bool) -> String {
    v.to_string()
}

fn opts(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
}

fn num<T: std::str::FromStr + Default>(v: &str) -> T {
    v.trim().parse().unwrap_or_default()
}

/// The alert kinds, as the desktop toggles name them: (short, when it happens).
fn kind_label(k: crate::alerts::Kind) -> (&'static str, &'static str) {
    use crate::alerts::Kind::*;
    match k {
        AgentDone => ("agent finished", "an agent finished while you were elsewhere"),
        NeedsYou => ("agent needs you", "an agent is stuck on a question or a prompt"),
        Approval => ("approvals", "something waits for your yes"),
        BuildFailed => ("failed checks", "a build or a merge check failed"),
        Calendar => ("calendar", "a plan is about to start"),
        Download => ("installs", "an install finished"),
        Memory => ("memory", "memory is nearly full"),
        Usage => ("AI usage", "an AI's usage nears its limit"),
        Update => ("updates", "a new oriel is out"),
    }
}

/// Every row, in order: each section's rows together.
pub(super) fn all(env: &Env) -> Vec<Row> {
    let mut v = vec![];
    // ---------------------------------------------------------------- general
    let c = 0;
    v.push(
        Row::new(c, "theme", "theme", "theme", "how oriel looks · ← → shows each one", When::Now, Ctl::Choice { opts: env.themes.iter().map(|t| (t.clone(), t.clone())).collect(), custom: false })
            .io(|c| c.theme.clone(), |c, v| c.theme = v.into()),
    );
    v.push(Row::new(c, "themes", "edit colours…", "", "the theme editor: make your own", When::Never, Ctl::Link(Jump::App("themes"))));
    v.push(Row::new(c, "plain_icons", "plain icons", "plain_icons", "letters instead of Nerd Font icons (for fonts without them)", When::Now, Ctl::Toggle).io(|c| b(c.plain_icons), |c, v| c.plain_icons = v == "true"));
    v.push(Row::new(c, "sidebar", "sidebar at start", "sidebar", "alt s hides and shows it, and that's remembered", When::Now, Ctl::Toggle).io(|c| b(c.sidebar), |c, v| c.sidebar = v == "true"));
    v.push(Row::new(c, "startup", "open on", "startup", "the app oriel starts in", When::NextStart, Ctl::Choice { opts: env.starts.clone(), custom: false }).io(|c| c.startup.clone(), |c, v| c.startup = v.into()));
    v.push(Row::new(c, "prefix", "prefix key", "prefix", "tmux-style keys: the prefix, then | - x z c…", When::Now, Ctl::Key).io(|c| c.prefix.clone(), |c, v| c.prefix = v.into()));
    v.push(Row::new(c, "shell", "shell", "shell", "what terminal panes run", When::NewTerminals, Ctl::Choice { opts: env.shells.clone(), custom: true }).io(|c| c.shell.clone(), |c, v| c.shell = v.into()));
    // ---------------------------------------------------------------- AI chat
    let c = 1;
    let mut ais = vec![(String::new(), "the first one found".to_string())];
    for (id, label, _) in PROVIDERS {
        let mark = match &env.avail {
            Some(av) if av.contains(id) => " ✓",
            Some(_) => " · not set up",
            None => "",
        };
        ais.push((id.to_string(), format!("{label}{mark}")));
    }
    v.push(Row::new(c, "ai.provider", "default AI", "[ai] provider", "the AI new chats talk to", When::NewChats, Ctl::Choice { opts: ais, custom: false }).io(|c| c.ai.provider.clone(), |c, v| c.ai.provider = v.into()));
    let perms: Vec<(String, String)> = crate::panes::chat::PERMS.iter().map(|(k, _)| (k.to_string(), k.to_string())).collect();
    v.push(
        Row::new(c, "ai.perms", "permissions", "[ai] perms", "what coding agents may do: where new chats start", When::NewChats, Ctl::Choice { opts: perms, custom: false })
            .io(|c| crate::panes::chat::norm_perms(&c.ai.perms).map(String::from).unwrap_or_else(|| c.ai.perms.clone()), |c, v| c.ai.perms = v.into()),
    );
    let mut efforts = vec![(String::new(), "default".to_string())];
    efforts.extend(crate::panes::chat::EFFORTS.iter().filter(|(k, _)| *k != "default").map(|(k, _)| (k.to_string(), k.to_string())));
    v.push(Row::new(c, "ai.effort", "effort", "[ai] effort", "how hard coding agents think", When::NewChats, Ctl::Choice { opts: efforts, custom: false }).io(|c| c.ai.effort.clone(), |c, v| c.ai.effort = v.into()));
    for (id, label, _) in PROVIDERS {
        let mut models = vec![(String::new(), "default".to_string())];
        if *id == "ollama" {
            models.extend(env.ollama_tags.iter().map(|t| (t.clone(), t.clone())));
        } else {
            models.extend(crate::panes::chat::known_models(id).iter().filter(|(m, _)| *m != "default").map(|(m, _)| (m.to_string(), m.to_string())));
        }
        let (g, s) = (id.to_string(), id.to_string());
        v.push(
            Row::new(c, format!("ai.models.{id}"), format!("{label} model"), format!("[ai.models] {id}"), format!("the model new {label} chats use"), When::NewChats, Ctl::Choice { opts: models, custom: true }).io(
                move |c| c.ai.models.get(&g).cloned().unwrap_or_default(),
                move |c, v| {
                    if v.trim().is_empty() {
                        c.ai.models.remove(&s);
                    } else {
                        c.ai.models.insert(s.clone(), v.trim().into());
                    }
                },
            ),
        );
    }
    // ---------------------------------------------------------------- providers & keys
    let c = 2;
    v.push(Row::new(c, "ai.ollama_url", "Ollama address", "[ai] ollama_url", "where Ollama listens · t tests it", When::NextMessage, Ctl::Text).io(|c| c.ai.ollama_url.clone(), |c, v| c.ai.ollama_url = v.trim().into()));
    let presets = opts(&[("https://api.openai.com/v1", "OpenAI"), ("https://openrouter.ai/api/v1", "OpenRouter"), ("http://127.0.0.1:1234/v1", "LM Studio"), ("http://127.0.0.1:8080/v1", "llama.cpp")]);
    v.push(
        Row::new(c, "ai.openai_url", "OpenAI-compatible URL", "[ai] openai_url", "OpenAI, OpenRouter, LM Studio, llama.cpp… · t tests it", When::NextMessage, Ctl::Choice { opts: presets, custom: true })
            .io(|c| c.ai.openai_url.clone(), |c, v| c.ai.openai_url = v.trim().into()),
    );
    v.push(Row::new(c, "ai.openai_key", "OpenAI key", "[ai] openai_key", "for the OpenAI-compatible AI · x clears it", When::NextMessage, Ctl::Secret { env: "OPENAI_API_KEY" }).io(|c| c.ai.openai_key.clone(), |c, v| c.ai.openai_key = v.trim().into()));
    v.push(
        Row::new(c, "ai.anthropic_key", "Anthropic key", "[ai] anthropic_key", "for the Anthropic API · x clears it", When::NextMessage, Ctl::Secret { env: "ANTHROPIC_API_KEY" })
            .io(|c| c.ai.anthropic_key.clone(), |c, v| c.ai.anthropic_key = v.trim().into()),
    );
    // ---------------------------------------------------------------- lead mode
    let c = 3;
    let mut kinds = vec![(String::new(), "the last one used".to_string())];
    kinds.extend(env.kinds.iter().map(|k| (k.to_string(), k.to_string())));
    v.push(Row::new(c, "lead.agent", "lead AI", "[lead] agent", "who plans the run and hands out the work", When::NextRun, Ctl::Choice { opts: kinds, custom: false }).io(|c| c.lead.agent.clone(), |c, v| c.lead.agent = v.into()));
    v.push(Row::new(c, "lead.model", "lead model", "[lead] model", "empty = the lead AI's own default", When::NextRun, Ctl::Text).io(|c| c.lead.model.clone(), |c, v| c.lead.model = v.trim().into()));
    v.push(
        Row::new(c, "lead.max_parallel", "workers at once", "[lead] max_parallel", "more tasks wait their turn", When::NextRun, Ctl::Number { min: 1.0, max: 5.0, step: 1.0, unit: Unit::None })
            .io(|c| c.lead.max_parallel.to_string(), |c, v| c.lead.max_parallel = num(v)),
    );
    v.push(
        Row::new(c, "lead.run_budget_usd", "run budget", "[lead] run_budget_usd", "a whole run's cap: the lead and every worker", When::NextRun, Ctl::Number { min: 0.5, max: 1000.0, step: 0.5, unit: Unit::Usd })
            .io(|c| c.lead.run_budget_usd.to_string(), |c, v| c.lead.run_budget_usd = num(v)),
    );
    v.push(
        Row::new(c, "lead.budget_usd", "lead's own budget", "[lead] budget_usd", "what the lead itself may spend in a run", When::NextRun, Ctl::Number { min: 0.5, max: 1000.0, step: 0.5, unit: Unit::Usd })
            .io(|c| c.lead.budget_usd.to_string(), |c, v| c.lead.budget_usd = num(v)),
    );
    v.push(
        Row::new(c, "lead.protocol", "protocol", "[lead] protocol", "how the lead talks to oriel", When::NextRun, Ctl::Choice { opts: opts(&[("", "auto"), ("mcp", "MCP tools"), ("text", "text")]), custom: false })
            .io(|c| c.lead.protocol.clone(), |c, v| c.lead.protocol = v.into()),
    );
    v.push(
        Row::new(c, "lead.gate", "merge gate", "[lead] gate", "the check every merged task passes (build, tests)", When::NextMerge, Ctl::Choice { opts: opts(&[("", "detect"), ("none", "none")]), custom: true })
            .io(|c| c.lead.gate.clone(), |c, v| c.lead.gate = v.trim().into()),
    );
    if let Some((root, _)) = &env.repo {
        let name = std::path::Path::new(root).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| root.clone());
        let (g, s) = (root.clone(), root.clone());
        v.push(
            Row::new(c, "lead.gates.here", format!("gate for {name}"), format!("[lead.gates] \"{root}\""), "this repo's own gate: wins over the merge gate", When::NextMerge, Ctl::Choice { opts: opts(&[("", "the merge gate"), ("none", "none")]), custom: true }).io(
                move |c| {
                    let key = crate::config::path_key(std::path::Path::new(&g));
                    c.lead.gates.iter().find(|(k, _)| crate::config::path_key(std::path::Path::new(k.as_str())) == key).map(|(_, v)| v.clone()).unwrap_or_default()
                },
                move |c, v| {
                    let key = crate::config::path_key(std::path::Path::new(&s));
                    c.lead.gates.retain(|k, _| crate::config::path_key(std::path::Path::new(k.as_str())) != key);
                    if !v.trim().is_empty() {
                        c.lead.gates.insert(s.clone(), v.trim().into());
                    }
                },
            ),
        );
    }
    v.push(
        Row::new(c, "lead.gate_timeout_s", "gate timeout", "[lead] gate_timeout_s", "a gate running longer fails", When::NextMerge, Ctl::Number { min: 30.0, max: 7200.0, step: 30.0, unit: Unit::Secs })
            .io(|c| c.lead.gate_timeout_s.to_string(), |c, v| c.lead.gate_timeout_s = num(v)),
    );
    v.push(
        Row::new(c, "lead.stagger_s", "stagger", "[lead] stagger_s", "between two workers on one model, so the second reuses the cache", When::Now, Ctl::Number { min: 0.0, max: 300.0, step: 1.0, unit: Unit::Secs })
            .io(|c| c.lead.stagger_s.to_string(), |c, v| c.lead.stagger_s = num(v)),
    );
    v.push(
        Row::new(c, "lead.hung_after_s", "hung after", "[lead] hung_after_s", "a worker silent this long is checked on", When::Now, Ctl::Number { min: 60.0, max: 3600.0, step: 30.0, unit: Unit::Secs })
            .io(|c| c.lead.hung_after_s.to_string(), |c, v| c.lead.hung_after_s = num(v)),
    );
    // ---------------------------------------------------------------- roster
    let c = 4;
    v.push(Row::new(c, "roster.edit", "edit the roster", "[[roster]]", "the workers a lead can use: model, tier, budget", When::Never, Ctl::Link(Jump::AppKey("agents", 'R'))));
    v.push(Row::new(c, "roster.reset", "reset to the defaults", "roster = []", "one worker per agent installed here", When::Never, Ctl::Act(Do::ResetRoster)));
    // ---------------------------------------------------------------- alerts
    let c = 5;
    v.push(
        Row::new(c, "desktop_notifications", "desktop notifications", "desktop_notifications", "from the OS, while you're in another window", When::Now, Ctl::Toggle)
            .io(|c| b(c.desktop_notifications), |c, v| c.desktop_notifications = v == "true"),
    );
    for k in crate::alerts::KINDS {
        let (g, s) = (k.id(), k.id());
        v.push(
            Row::new(c, format!("alerts.desktop.{}", k.id()), format!("notify: {}", kind_label(*k).0), "[alerts] desktop", format!("when {}", kind_label(*k).1), When::Now, Ctl::Toggle).io(
                move |c| b(c.alerts.desktop.iter().any(|x| x == g)),
                move |c, v| {
                    c.alerts.desktop.retain(|x| x != s);
                    if v == "true" {
                        c.alerts.desktop.push(s.to_string());
                    }
                },
            ),
        );
    }
    v.push(
        Row::new(c, "alerts.memory_pct", "memory warning", "[alerts] memory_pct", "say so when memory is this full", When::Now, Ctl::Number { min: 50.0, max: 99.0, step: 1.0, unit: Unit::Pct })
            .io(|c| c.alerts.memory_pct.to_string(), |c, v| c.alerts.memory_pct = num(v)),
    );
    v.push(
        Row::new(c, "alerts.usage_pct", "usage warning", "[alerts] usage_pct", "an AI's plan window this used (5-hour and the like)", When::Now, Ctl::Number { min: 50.0, max: 100.0, step: 1.0, unit: Unit::Pct })
            .io(|c| c.alerts.usage_pct.to_string(), |c, v| c.alerts.usage_pct = num(v)),
    );
    v.push(
        Row::new(c, "alerts.weekly_usage_pct", "weekly usage warning", "[alerts] weekly_usage_pct", "the weekly window this used", When::Now, Ctl::Number { min: 50.0, max: 100.0, step: 1.0, unit: Unit::Pct })
            .io(|c| c.alerts.weekly_usage_pct.to_string(), |c, v| c.alerts.weekly_usage_pct = num(v)),
    );
    v.push(
        Row::new(c, "calendar.remind_before_min", "calendar reminder", "[calendar] remind_before_min", "before a plan with a time (0 = when it starts)", When::Now, Ctl::Number { min: 0.0, max: 120.0, step: 5.0, unit: Unit::Min })
            .io(|c| c.calendar.remind_before_min.to_string(), |c, v| c.calendar.remind_before_min = num(v)),
    );
    v.push(Row::new(c, "alerts.test", "send a test notification", "", "check that desktop notifications show up here", When::Never, Ctl::Act(Do::TestNotify)));
    // ---------------------------------------------------------------- folders
    let c = 6;
    v.push(Row::new(c, "notes_folder", "notes folder", "notes_folder", "plain .md files: a synced folder or an Obsidian vault works", When::Now, Ctl::Folder).io(|c| c.notes_folder.clone(), |c, v| c.notes_folder = v.trim().into()));
    v.push(
        Row::new(c, "music.folders", "music folders", "[music] folders", "where music looks for songs", When::Now, Ctl::List)
            .io(|c| c.music.folders.join("\n"), |c, v| c.music.folders = v.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()),
    );
    v.push(
        Row::new(c, "music.source", "music source", "[music] source", "whether the audio-player app's library comes first", When::Now, Ctl::Choice { opts: opts(&[("auto", "audio-player library first"), ("folders", "these folders only")]), custom: false })
            .io(|c| c.music.source.clone(), |c, v| c.music.source = v.into()),
    );
    // ---------------------------------------------------------------- tools
    let c = 7;
    v.push(Row::new(c, "tools.saver", "token saver", "", "Frugal / Balanced / Max for Claude Code and Codex", When::Never, Ctl::Link(Jump::AppKey("ais", '4'))));
    v.push(Row::new(c, "tools.ais", "AI tools", "", "install and sign in to AI coding tools", When::Never, Ctl::Link(Jump::AppKey("ais", '2'))));
    v.push(Row::new(c, "tools.updates", "updates", "", "what's new, check now, roll back", When::Never, Ctl::Link(Jump::App("updates"))));
    v.push(Row::new(c, "update_check", "check for updates", "update_check", "look for a newer oriel once a day at start", When::NextStart, Ctl::Toggle).io(|c| b(c.update_check), |c, v| c.update_check = v == "true"));
    v.push(Row::new(c, "tools.tour", "replay the tour", "", "the interactive tour, about a minute", When::Never, Ctl::Link(Jump::Tour)));
    v.push(Row::new(c, "tools.config", "config file", "", "o opens it · c copies where it is", When::Never, Ctl::Act(Do::ConfigFile)));
    v
}
