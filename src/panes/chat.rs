//! The ai app, nest style: chat list in the sidebar, messages (your words in boxes on the right, replies with a
//! header and markdown on the left), the big logo on a new chat, a composer with a / command menu.

mod md;
pub mod providers;
mod store;

use crate::pane::{Action, Cx, Pane};
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use providers::Ev;
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

const COMMANDS: &[(&str, &str, &str)] = &[
    ("/new", "", "start a new chat"),
    ("/retry", "", "regenerate the last reply"),
    ("/model", "<ai> [model]", "switch AI: wren, claude, codex, ollama, openai, anthropic"),
    ("/mode", "<fast|balanced|smart>", "wren: fast = quick + simpler · smart = slower, 2 drafts, best kept"),
    ("/cwd", "<folder>", "folder Claude Code / Codex work in for this chat"),
    ("/perms", "<ask|edits|full|read>", "what agents may do: edits (default), full, read-only, ask"),
    ("/memory", "", "what wren remembers about you"),
    ("/remember", "<fact>", "tell wren something to remember"),
    ("/forget", "<text>", "forget memories containing <text>"),
    ("/best", "", "wren: rewrite the last reply in smart mode (2 drafts, best kept)"),
    ("/research", "<question>", "wren: look it up on the web first, then answer"),
    ("/web", "<on|off>", "wren: allow web lookups (on by default)"),
    ("/key", "<openai|anthropic> <key>", "save an API key"),
    ("/note", "", "save the last reply to notes"),
    ("/save", "", "export this chat as a markdown file"),
    ("/theme", "<name>", "switch colour theme (live preview with just /theme)"),
    ("/play", "", "music: play / pause"),
    ("/next", "", "music: next song"),
    ("/prev", "", "music: previous song"),
    ("/music", "", "go to the music app"),
    ("/sidebar", "", "hide / show the sidebar"),
    ("/icons", "", "nerd font icons on / off"),
    ("/info", "", "what's running: AI, model, folder, memory"),
    ("/delete", "", "delete this chat"),
    ("/help", "", "keys and commands"),
    ("/quit", "", "quit oriel"),
];

/// One row of the / menu: what it shows, and what picking it does.
struct MenuItem {
    left: String,
    desc: String,
    /// The composer text after picking it.
    fill: String,
    /// Picking runs it (false = a command that still needs its argument typed).
    run: bool,
}

const SUGGESTIONS: &[&str] = &["explain how rainbows form", "give me 3 tips for better sleep", "code a snake game in html", "what is a python function?"];

const SPIN: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const VERBS: &[&str] = &["thinking", "pondering", "noodling", "brewing", "untangling", "scheming", "percolating", "tinkering"];

struct Stream {
    stop: Arc<AtomicBool>,
    inbox: Arc<Mutex<Vec<Ev>>>,
    status: String,
    started: Instant,
}

#[derive(Clone)]
enum SideItem {
    New,
    Chat(usize),
}

pub struct Chat {
    chats: Vec<store::Chat>,
    chat: store::Chat,
    input: String,
    cursor: usize, // char index into input
    scroll: usize, // lines up from the bottom; 0 = follow the end
    stream: Option<Stream>,
    menu_sel: usize,
    cache: HashMap<usize, (u64, Vec<Line<'static>>)>,
    memory: Vec<String>,
    provider: String,
    mode: String,
    perms: String,
    web: bool,
    once_mode: Option<String>,
    once_research: bool,
    keys: HashMap<String, String>,
    info: Vec<String>,
    avail: Vec<&'static str>,
    confirm_delete: bool,
    side_hits: Vec<(Rect, SideItem)>,
    side_scroll: usize,
    hero_hits: Vec<(Rect, String)>,
    launch_dir: PathBuf,
    history_pos: Option<usize>,
    offset: i64,
}

fn memory_path() -> PathBuf {
    crate::config::data_dir().join("memory.json")
}

impl Chat {
    pub fn new(cfg: &crate::config::Config) -> Self {
        let chats = store::load_all();
        let memory = std::fs::read_to_string(memory_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let avail = providers::available(&cfg.ai);
        // the configured AI if this machine has it, else the first one it does have
        let provider = if !cfg.ai.provider.is_empty() && avail.contains(&cfg.ai.provider.as_str()) {
            cfg.ai.provider.clone()
        } else {
            avail.first().map(|s| s.to_string()).unwrap_or_else(|| "none".into())
        };
        Chat {
            chats,
            chat: store::Chat::new(&provider),
            input: String::new(),
            cursor: 0,
            scroll: 0,
            stream: None,
            menu_sel: 0,
            cache: HashMap::new(),
            memory,
            provider,
            mode: if cfg.ai.wren_mode.is_empty() { "balanced".into() } else { cfg.ai.wren_mode.clone() },
            perms: "edits".into(),
            web: true,
            once_mode: None,
            once_research: false,
            keys: HashMap::new(),
            info: vec![],
            avail,
            confirm_delete: false,
            side_hits: vec![],
            side_scroll: 0,
            hero_hits: vec![],
            launch_dir: std::env::current_dir().unwrap_or_default(),
            history_pos: None,
            offset: crate::app::local_offset_secs(),
        }
    }

    fn provider_of(&self) -> String {
        self.chat.provider.clone().unwrap_or_else(|| self.provider.clone())
    }

    /// Where CLI agents run: the chat's folder, else where oriel started — never the home folder itself (on
    /// Leif's PC home is a git repo, so Claude Code would file every chat's memories there).
    fn workdir(&self) -> PathBuf {
        let d = self.chat.cwd.clone().map(PathBuf::from).unwrap_or_else(|| self.launch_dir.clone());
        let home = dirs::home_dir().unwrap_or_default();
        if !d.is_dir() || d == home {
            let w = crate::config::data_dir().join("work");
            if !w.join(".git").exists() {
                let _ = std::fs::create_dir_all(&w);
                if crate::config::which("git").is_some() {
                    let mut c = std::process::Command::new("git");
                    c.args(["init", "-q"]).arg(&w).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
                    #[cfg(windows)]
                    {
                        use std::os::windows::process::CommandExt;
                        c.creation_flags(0x08000000);
                    }
                    let _ = c.status();
                }
            }
            return w;
        }
        d
    }

    fn save_memory(&self) {
        if let Ok(s) = serde_json::to_string_pretty(&self.memory) {
            let _ = std::fs::write(memory_path(), s);
        }
    }

    fn new_chat(&mut self) {
        self.stop();
        self.persist();
        self.chat = store::Chat::new(&self.provider_of());
        self.cache.clear();
        self.scroll = 0;
        self.info.clear();
    }

    fn open_chat(&mut self, i: usize) {
        if i >= self.chats.len() {
            return;
        }
        self.stop();
        self.persist();
        self.chat = self.chats[i].clone();
        self.cache.clear();
        self.scroll = 0;
        self.info.clear();
    }

    /// Save the current chat and refresh it in the list.
    fn persist(&mut self) {
        if self.chat.messages.is_empty() {
            return;
        }
        store::save(&mut self.chat);
        self.chats.retain(|c| c.id != self.chat.id);
        self.chats.insert(0, self.chat.clone());
    }

    fn stop(&mut self) {
        if let Some(s) = self.stream.take() {
            s.stop.store(true, Ordering::SeqCst);
            if let Some(m) = self.chat.messages.last_mut() {
                if m.role == "assistant" {
                    if m.content.trim().is_empty() {
                        m.content = "(stopped)".into();
                    }
                    m.note = Some(format!("{} · stopped", m.note.clone().unwrap_or_default()).trim_start_matches(" · ").to_string());
                }
            }
            self.persist();
        }
    }

    fn send(&mut self, text: String, cx: &mut Cx) {
        if self.stream.is_some() {
            return;
        }
        if self.chat.messages.is_empty() {
            self.chat.title = store::title_from(&text);
            if self.chat.cwd.is_none() {
                self.chat.cwd = Some(self.launch_dir.to_string_lossy().to_string());
            }
        }
        self.chat.messages.push(store::Msg { role: "user".into(), content: text, model: None, note: None, steps: vec![] });
        self.start_reply(cx);
    }

    /// Ask the provider for a reply to the conversation as it stands (last message = the user's).
    fn start_reply(&mut self, cx: &mut Cx) {
        let provider = self.provider_of();
        let messages: Vec<(String, String)> = self.chat.messages.iter().map(|m| (m.role.clone(), m.content.clone())).collect();
        self.chat.messages.push(store::Msg { role: "assistant".into(), content: String::new(), model: Some(provider.clone()), note: None, steps: vec![] });
        self.info.clear();
        self.scroll = 0;
        let stop = Arc::new(AtomicBool::new(false));
        let inbox: Arc<Mutex<Vec<Ev>>> = Arc::default();
        let mut cfg = cx.config.ai.clone();
        if let Some(k) = self.keys.get("openai") {
            cfg.openai_key = k.clone();
        }
        if let Some(k) = self.keys.get("anthropic") {
            cfg.anthropic_key = k.clone();
        }
        let req = providers::Request {
            web: self.web,
            research: std::mem::take(&mut self.once_research),
            provider: provider.clone(),
            model: self.chat.model.clone(),
            messages,
            cwd: self.workdir(),
            perms: self.perms.clone(),
            mode: self.once_mode.take().unwrap_or_else(|| self.mode.clone()),
            memory: self.memory.clone(),
            state: self.chat.state.clone(),
            cfg,
        };
        let (ib, waker) = (inbox.clone(), cx.waker());
        providers::start(req, stop.clone(), move |ev| {
            ib.lock().unwrap().push(ev);
            waker.wake();
        });
        self.stream = Some(Stream { stop, inbox, status: String::new(), started: Instant::now() });
    }

    fn retry(&mut self, cx: &mut Cx) {
        if self.stream.is_some() {
            return;
        }
        if self.chat.messages.last().map(|m| m.role == "assistant").unwrap_or(false) {
            self.chat.messages.pop();
            self.cache.clear();
        }
        if self.chat.messages.last().map(|m| m.role == "user").unwrap_or(false) {
            self.start_reply(cx);
        }
    }

    fn run_command(&mut self, line: &str, cx: &mut Cx) {
        let mut parts = line.splitn(2, ' ');
        let cmd = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("").trim().to_string();
        self.info.clear();
        match cmd {
            "/new" => self.new_chat(),
            "/retry" => self.retry(cx),
            "/model" => {
                if arg.is_empty() {
                    self.info.push("pick an AI with /model <name> [model]:".into());
                    for (id, label, what) in providers::PROVIDERS {
                        self.info.push(format!("  {id:<10} {label} — {what}"));
                    }
                } else {
                    let mut a = arg.split_whitespace();
                    let id = a.next().unwrap_or("wren").to_lowercase();
                    if providers::PROVIDERS.iter().any(|p| p.0 == id) && !self.avail.contains(&id.as_str()) {
                        self.info.push(format!("{} isn't set up on this computer", providers::label(&id)));
                        self.info.push(match id.as_str() {
                            "wren" => "  wren runs on its owner's PC (its server on 127.0.0.1:5237)".into(),
                            "claude" => "  install Claude Code: npm i -g @anthropic-ai/claude-code, then run `claude` once to sign in".into(),
                            "codex" => "  install Codex: npm i -g @openai/codex, then `codex login`".into(),
                            "ollama" => "  install Ollama from ollama.com and pull a model (ollama pull llama3.2)".into(),
                            _ => format!("  add a key: /key {id} <key>"),
                        });
                    } else if providers::PROVIDERS.iter().any(|p| p.0 == id) {
                        let model = a.next().map(String::from);
                        self.chat.provider = Some(id.clone());
                        self.chat.model = model.clone();
                        self.provider = id.clone();
                        cx.notify(format!("{}{}{}", ui::lead("ai"), providers::label(&id), model.map(|m| format!(" · {m}")).unwrap_or_default()));
                    } else {
                        self.info.push(format!("no AI called {id} — try /model"));
                    }
                }
            }
            "/mode" => {
                if ["fast", "balanced", "smart"].contains(&arg.as_str()) {
                    self.mode = arg.clone();
                    cx.notify(format!("wren mode: {arg}"));
                } else {
                    self.info.push("/mode fast · balanced · smart".into());
                }
            }
            "/cwd" => {
                let p = PathBuf::from(if arg.starts_with('~') { arg.replacen('~', &dirs::home_dir().unwrap_or_default().to_string_lossy(), 1) } else { arg.clone() });
                if p.is_dir() {
                    self.chat.cwd = Some(p.to_string_lossy().to_string());
                    self.chat.state.clear(); // CLI sessions are per folder
                    cx.notify(format!("agents work in {}", p.display()));
                } else {
                    self.info.push(format!("not a folder: {arg}"));
                }
            }
            "/perms" => {
                if ["ask", "edits", "full", "read"].contains(&arg.as_str()) {
                    self.perms = arg.clone();
                    cx.notify(format!("agent permissions: {arg}"));
                } else {
                    self.info.push("/perms edits (default: edit files in the folder) · full (anything) · read (read-only) · ask (refuse what needs approval)".into());
                }
            }
            "/memory" => {
                if self.memory.is_empty() {
                    self.info.push("wren doesn't remember anything yet. Tell it things like \"my name is …\" or use /remember".into());
                } else {
                    self.info.push("wren remembers:".into());
                    for m in &self.memory {
                        self.info.push(format!("  • {m}"));
                    }
                }
            }
            "/remember" if !arg.is_empty() => {
                self.memory.push(arg.clone());
                self.save_memory();
                cx.notify("remembered");
            }
            "/forget" if !arg.is_empty() => {
                let before = self.memory.len();
                let a = arg.to_lowercase();
                self.memory.retain(|m| !m.to_lowercase().contains(&a));
                self.save_memory();
                cx.notify(format!("forgot {} memor{}", before - self.memory.len(), if before - self.memory.len() == 1 { "y" } else { "ies" }));
            }
            "/theme" if !arg.is_empty() => cx.act(Action::SetTheme(arg)),
            "/theme" => cx.act(Action::Palette("theme ".into())),
            "/best" => {
                if self.provider_of() == "wren" {
                    self.once_mode = Some("smart".into());
                    self.retry(cx);
                } else {
                    self.info.push("/best is wren's smart mode; other AIs: /retry".into());
                }
            }
            "/research" if !arg.is_empty() => {
                self.once_research = true;
                self.send(arg, cx);
            }
            "/web" => {
                self.web = match arg.as_str() {
                    "on" => true,
                    "off" => false,
                    _ => !self.web,
                };
                cx.notify(format!("web lookups {}", if self.web { "on" } else { "off" }));
            }
            "/key" => {
                let mut a = arg.split_whitespace();
                match (a.next(), a.next()) {
                    (Some(p @ ("openai" | "anthropic")), Some(key)) => {
                        let mut c = crate::config::load();
                        if p == "openai" {
                            c.ai.openai_key = key.to_string();
                        } else {
                            c.ai.anthropic_key = key.to_string();
                        }
                        crate::config::save(&c);
                        self.keys.insert(p.to_string(), key.to_string());
                        let id: &'static str = if p == "openai" { "openai" } else { "anthropic" };
                        if !self.avail.contains(&id) {
                            self.avail.push(id);
                        }
                        if self.provider_of() == "none" {
                            self.provider = id.to_string();
                            self.chat.provider = Some(id.to_string());
                        }
                        cx.notify(format!("{p} key saved"));
                    }
                    _ => self.info.push("/key openai <key> · /key anthropic <key>  (saved in oriel's config file)".into()),
                }
            }
            "/note" => match self.chat.messages.iter().rev().find(|m| m.role == "assistant" && !m.content.is_empty()) {
                Some(m) => {
                    let dir = crate::config::data_dir().join("notes");
                    let _ = std::fs::create_dir_all(&dir);
                    let name: String = self.chat.title.chars().map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' { c } else { '_' }).collect();
                    let mut path = dir.join(format!("{}.md", name.trim()));
                    let mut n = 2;
                    while path.exists() {
                        path = dir.join(format!("{} {n}.md", name.trim()));
                        n += 1;
                    }
                    match std::fs::write(&path, format!("# {}\n\n{}\n", self.chat.title, m.content)) {
                        Ok(_) => cx.notify(format!("saved to notes: {}", path.file_name().unwrap_or_default().to_string_lossy())),
                        Err(e) => self.info.push(format!("couldn't save: {e}")),
                    }
                }
                None => self.info.push("no reply to save yet".into()),
            },
            "/save" => {
                if self.chat.messages.is_empty() {
                    self.info.push("nothing to save yet".into());
                } else {
                    let dir = dirs::document_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default()).join("oriel chats");
                    let _ = std::fs::create_dir_all(&dir);
                    let name: String = self.chat.title.chars().map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' { c } else { '_' }).collect();
                    let path = dir.join(format!("{}.md", name.trim()));
                    let mut md = format!("# {}\n\n", self.chat.title);
                    for m in &self.chat.messages {
                        let who = if m.role == "user" { "you".to_string() } else { providers::label(m.model.as_deref().unwrap_or("wren")).to_lowercase() };
                        md.push_str(&format!("**{who}:**\n\n{}\n\n", m.content));
                    }
                    match std::fs::write(&path, md) {
                        Ok(_) => cx.notify(format!("saved {}", path.display())),
                        Err(e) => self.info.push(format!("couldn't save: {e}")),
                    }
                }
            }
            "/play" => cx.act(Action::AppKey("music", ' ')),
            "/next" => cx.act(Action::AppKey("music", 'n')),
            "/prev" => cx.act(Action::AppKey("music", 'p')),
            "/music" => cx.act(Action::GotoApp("music")),
            "/sidebar" => cx.act(Action::ToggleSidebar),
            "/icons" => cx.act(Action::ToggleIcons),
            "/quit" => cx.act(Action::Quit),
            "/info" => {
                let p = self.provider_of();
                self.info.push(format!("oriel {} · {} ({})", env!("CARGO_PKG_VERSION"), providers::label(&p), self.chat.model.clone().unwrap_or("default model".into())));
                if p == "wren" {
                    self.info.push(format!("  wren mode {} · web lookups {} · servers: {}", self.mode, if self.web { "on" } else { "off" }, cx.config.ai.wren_urls.join(", ")));
                } else {
                    self.info.push(format!("  works in {} · permissions {}", self.workdir().display(), self.perms));
                }
                self.info.push(format!("  {} memories · {} saved chats · config: {}", self.memory.len(), self.chats.len(), crate::config::path().display()));
            }
            "/delete" => self.confirm_delete = true,
            "/help" => {
                self.info.extend(
                    [
                        "enter send · esc stop · ctrl+r regenerate · ctrl+n new chat · ctrl+d delete chat",
                        "pgup/pgdn or the wheel scroll · ↑ in an empty box recalls your last message",
                        "F1-F7 apps · F8 play/pause · alt p palette · alt n terminal beside this",
                    ]
                    .map(String::from),
                );
                for (c, a, d) in COMMANDS {
                    self.info.push(format!("  {c} {a} — {d}"));
                }
            }
            _ => self.info.push(format!("unknown command {cmd} — type / to see them")),
        }
    }

    /// Choices for a command's argument (what nest listed after `/model `, `/theme `...).
    fn arg_options(&self, cmd: &str) -> Vec<(String, String)> {
        let pairs = |v: &[(&str, &str)]| v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect::<Vec<_>>();
        match cmd {
            "/model" => providers::PROVIDERS
                .iter()
                .filter(|(id, ..)| self.avail.contains(id) || *id != "wren") // wren only where it's installed
                .map(|(id, label, what)| (id.to_string(), if self.avail.contains(id) { format!("{label} — {what}") } else { format!("{label} — not set up here") }))
                .collect(),
            "/mode" => pairs(&[("fast", "quick and simpler"), ("balanced", "the default"), ("smart", "slower: 2 drafts, the best is kept")]),
            "/perms" => pairs(&[
                ("edits", "edit files in the chat's folder (default)"),
                ("full", "anything, never asks"),
                ("read", "read-only"),
                ("ask", "refuse whatever would need approval"),
            ]),
            "/web" => pairs(&[("on", "wren may look things up"), ("off", "answer from what it knows")]),
            "/key" => pairs(&[("openai", "OpenAI-compatible key"), ("anthropic", "Anthropic API key")]),
            "/theme" => crate::theme::names().into_iter().map(|n| (n.clone(), if n == "omarchy" { "follows your Omarchy theme".into() } else if n == "terminal" { "your terminal's own colours".into() } else { String::new() })).collect(),
            "/cwd" => {
                // folders chats have used, newest first
                let mut seen = vec![];
                for c in &self.chats {
                    if let Some(d) = &c.cwd {
                        if !seen.contains(d) && std::path::Path::new(d).is_dir() {
                            seen.push(d.clone());
                        }
                    }
                }
                seen.into_iter().take(12).map(|d| (d, "used before".to_string())).collect()
            }
            _ => vec![],
        }
    }

    fn menu(&self) -> Vec<MenuItem> {
        if !self.input.starts_with('/') || self.input.contains('\n') {
            return vec![];
        }
        match self.input.split_once(' ') {
            None => COMMANDS
                .iter()
                .filter(|c| c.0.starts_with(self.input.as_str()))
                .map(|(c, a, d)| MenuItem {
                    left: format!("{c} {a}"),
                    desc: d.to_string(),
                    fill: if a.contains('<') { format!("{c} ") } else { c.to_string() },
                    run: !a.contains('<'),
                })
                .collect(),
            Some((cmd, rest)) => {
                let q = rest.trim_start().to_lowercase();
                if q.contains(' ') {
                    return vec![]; // past the argument (e.g. /key openai sk-...)
                }
                self.arg_options(cmd)
                    .into_iter()
                    .filter(|(v, _)| v.to_lowercase().starts_with(&q) || (q.len() >= 2 && v.to_lowercase().contains(&q)))
                    .map(|(v, d)| MenuItem {
                        left: v.clone(),
                        desc: d,
                        // /key still needs the key after the provider
                        fill: if cmd == "/key" { format!("{cmd} {v} ") } else { format!("{cmd} {v}") },
                        run: cmd != "/key",
                    })
                    .collect()
            }
        }
    }

    fn insert(&mut self, s: &str) {
        let byte = self.input.char_indices().nth(self.cursor).map(|x| x.0).unwrap_or(self.input.len());
        self.input.insert_str(byte, s);
        self.cursor += s.chars().count();
        self.menu_sel = 0;
    }

    // ------------------------------------------------------------ drawing helpers
    fn msg_lines(&mut self, i: usize, width: usize, cx: &Cx) -> Vec<Line<'static>> {
        let t = cx.theme;
        let m = &self.chat.messages[i];
        let streaming_last = self.stream.is_some() && i + 1 == self.chat.messages.len();
        let key = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (m.content.len(), m.note.as_deref().unwrap_or(""), m.steps.len(), width, streaming_last, t.name.as_str()).hash(&mut h);
            h.finish()
        };
        if !streaming_last {
            if let Some((k, lines)) = self.cache.get(&i) {
                if *k == key {
                    return lines.clone();
                }
            }
        }
        let mut out: Vec<Line<'static>> = vec![];
        if m.role == "user" {
            // a box on the right with "you" set into its top border
            let max_w = (width * 7 / 10).max(20);
            let body = md::wrap(vec![Span::raw(m.content.clone())], max_w.saturating_sub(4), "", "");
            let inner_w = body.iter().map(|l| l.width()).max().unwrap_or(0).max(6);
            let box_w = inner_w + 4;
            let pad = " ".repeat(width.saturating_sub(box_w + 1));
            let who = format!(" {}you ", ui::lead("you"));
            let top_fill = box_w.saturating_sub(2 + who.width() + 1);
            let frame = Style::default().fg(t.user);
            out.push(Line::from(vec![
                Span::raw(pad.clone()),
                Span::styled(format!("╭{}", "─".repeat(top_fill)), frame),
                Span::styled(who, Style::default().fg(t.muted)),
                Span::styled("─╮", frame),
            ]));
            for l in body {
                let w = l.width();
                let mut spans = vec![Span::raw(pad.clone()), Span::styled("│ ", frame)];
                spans.extend(l.spans.into_iter().map(|s| Span::styled(s.content.to_string(), Style::default().fg(t.fg))));
                spans.push(Span::raw(" ".repeat(inner_w - w)));
                spans.push(Span::styled(" │", frame));
                out.push(Line::from(spans));
            }
            out.push(Line::from(vec![Span::raw(pad), Span::styled(format!("╰{}╯", "─".repeat(box_w - 2)), frame)]));
        } else {
            let who = m.model.clone().unwrap_or_else(|| "wren".into());
            let icon = if who == "wren" { "wren" } else { "ai" };
            out.push(Line::from(vec![
                Span::styled(ui::lead(icon), Style::default().fg(t.accent)),
                Span::styled(providers::label(&who).to_lowercase(), Style::default().fg(t.accent).add_modifier(Modifier::BOLD)),
            ]));
            out.push(Line::raw(""));
            for (label, state, detail) in &m.steps {
                let mark = match state.as_str() {
                    "done" => Span::styled("  ✓ ", Style::default().fg(t.good)),
                    "error" | "failed" => Span::styled("  ✗ ", Style::default().fg(t.danger)),
                    _ => Span::styled("  · ", Style::default().fg(t.muted)),
                };
                out.push(Line::from(vec![mark, Span::styled(label.clone(), Style::default().fg(t.muted))]));
                if let Some(d) = detail {
                    for dl in d.lines().take(40) {
                        let style = if dl.starts_with('+') {
                            Style::default().fg(t.good)
                        } else if dl.starts_with('-') {
                            Style::default().fg(t.danger)
                        } else {
                            Style::default().fg(t.muted)
                        };
                        out.push(Line::from(Span::styled(format!("      {}", ui::fit(dl, width.saturating_sub(8))), style)));
                    }
                }
            }
            if !m.steps.is_empty() && !m.content.is_empty() {
                out.push(Line::raw(""));
            }
            if !m.content.is_empty() {
                out.extend(md::render(&m.content, width.saturating_sub(1), "  ", t));
            }
            if let Some(n) = &m.note {
                out.push(Line::raw(""));
                out.extend(md::wrap(vec![Span::styled(n.clone(), Style::default().fg(t.muted))], width.saturating_sub(1), "  ", "  "));
            }
        }
        out.push(Line::raw(""));
        if !streaming_last {
            self.cache.insert(i, (key, out.clone()));
        }
        out
    }

    fn draw_hero(&mut self, f: &mut Frame, area: Rect, cx: &Cx) {
        let t = cx.theme;
        let p = self.provider_of();
        if p == "none" {
            let logo = crate::font::render("oriel");
            let lw = logo.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
            let lines = [
                "no AI is set up on this computer yet. Any one of these works:",
                "  › Claude Code   npm i -g @anthropic-ai/claude-code, then run `claude` once",
                "  › Codex         npm i -g @openai/codex, then `codex login`",
                "  › Ollama        ollama.com, then `ollama pull llama3.2` (free, runs locally)",
                "  › an API key    /key openai <key>  or  /key anthropic <key>",
                "then restart oriel, or pick it with /model",
            ];
            let y0 = area.y + area.height.saturating_sub(8 + 2 + lines.len() as u16) / 2;
            if area.width > lw + 2 && area.height >= 8 + 2 + lines.len() as u16 {
                let x = area.x + (area.width - lw) / 2;
                for (r, l) in logo.iter().enumerate() {
                    f.render_widget(Paragraph::new(Span::styled(l.clone(), ui::accent(t))), Rect { x, y: y0 + r as u16, width: lw, height: 1 });
                }
            }
            let tx = area.x + area.width.saturating_sub(78) / 2;
            for (i, l) in lines.iter().enumerate() {
                let style = if i == 0 { Style::default().add_modifier(Modifier::BOLD) } else { ui::muted(t) };
                f.render_widget(Paragraph::new(Span::styled(*l, style)), Rect { x: tx, y: y0 + 10 + i as u16, width: area.width.saturating_sub(tx - area.x), height: 1 });
            }
            return;
        }
        let word = match p.as_str() {
            "claude" => "claude",
            "codex" => "codex",
            "ollama" => "ollama",
            "openai" => "openai",
            "anthropic" => "claude",
            _ => "wren",
        };
        let logo = crate::font::render(word);
        let lw = logo.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
        let info = match p.as_str() {
            "wren" => format!("wren · {} mode · research + memory", self.mode),
            "claude" | "codex" => format!("{} · {} · works in {} ({})", providers::label(&p).to_lowercase(), self.chat.model.clone().unwrap_or("default".into()), self.workdir().display(), self.perms),
            _ => format!("{} · {}", providers::label(&p).to_lowercase(), self.chat.model.clone().unwrap_or("default".into())),
        };
        let block_h = 8 + 2 + 2 + 1 + SUGGESTIONS.len() as u16;
        let y0 = area.y + area.height.saturating_sub(block_h) / 2;
        let mut y = y0;
        if area.width > lw + 2 && area.height >= block_h {
            let x = area.x + (area.width - lw) / 2;
            for (r, l) in logo.iter().enumerate() {
                let spans: Vec<Span> = if t.animated {
                    l.chars().enumerate().map(|(i, c)| Span::styled(c.to_string(), Style::default().fg(crate::theme::rainbow(i, cx.time)))).collect()
                } else {
                    vec![Span::styled(l.clone(), Style::default().fg(t.accent))]
                };
                f.render_widget(Paragraph::new(Line::from(spans)), Rect { x, y: y + r as u16, width: lw, height: 1 });
            }
            y += 10;
        }
        let tx = area.x + area.width.saturating_sub(lw.max(40)) / 2;
        let tw = area.width.saturating_sub(tx - area.x);
        f.render_widget(Paragraph::new(Span::styled(ui::fit(&info, tw as usize), ui::muted(t))), Rect { x: tx, y, width: tw, height: 1 });
        y += 2;
        f.render_widget(Paragraph::new(Span::styled("what can I help with?", Style::default().add_modifier(Modifier::BOLD))), Rect { x: tx, y, width: tw, height: 1 });
        y += 2;
        self.hero_hits.clear();
        for s in SUGGESTIONS {
            if y >= area.bottom() {
                break;
            }
            let r = Rect { x: tx, y, width: (s.len() as u16 + 2).min(tw), height: 1 };
            f.render_widget(Paragraph::new(Line::from(vec![Span::styled("› ", ui::accent(t)), Span::raw(*s)])), r);
            self.hero_hits.push((r, s.to_string()));
            y += 1;
        }
    }
}

impl Pane for Chat {
    fn title(&self) -> String {
        if self.chat.messages.is_empty() { "new chat".into() } else { self.chat.title.to_lowercase() }
    }
    fn icon(&self) -> &'static str {
        "ai"
    }
    fn subtitle(&self) -> Option<String> {
        let p = self.provider_of();
        let mut s = providers::label(&p).to_lowercase();
        match p.as_str() {
            "wren" => s.push_str(&format!(" · {}", self.mode)),
            "claude" | "codex" => s.push_str(&format!(" · {} · {} · {}", self.chat.model.clone().unwrap_or("default".into()), ui::fit(&self.workdir().to_string_lossy(), 40), self.perms)),
            _ => s.push_str(&format!(" · {}", self.chat.model.clone().unwrap_or("default".into()))),
        }
        Some(s)
    }
    fn badge(&self) -> Option<String> {
        self.stream.as_ref().map(|s| SPIN[(s.started.elapsed().as_millis() / 100) as usize % SPIN.len()].to_string())
    }
    fn tick_every(&self) -> Option<Duration> {
        self.stream.as_ref().map(|_| Duration::from_millis(100))
    }

    fn poll(&mut self, cx: &mut Cx) {
        let Some(s) = &mut self.stream else { return };
        let evs: Vec<Ev> = std::mem::take(&mut *s.inbox.lock().unwrap());
        let mut finished = false;
        let mut save_mem = false;
        for ev in evs {
            let Some(m) = self.chat.messages.last_mut() else { break };
            match ev {
                Ev::Token(t) => m.content.push_str(&t),
                Ev::Status(st) => s.status = st,
                Ev::Step(l, st, d) => {
                    // a step with the same label updates in place (running -> done)
                    if let Some(x) = m.steps.iter_mut().find(|x| x.0 == l) {
                        x.1 = st;
                        x.2 = d;
                    } else {
                        m.steps.push((l, st, d));
                    }
                }
                Ev::State(k, v) => {
                    let (prov, field) = k.split_once('.').unwrap_or((k.as_str(), "session"));
                    let entry = self.chat.state.entry(prov.to_string()).or_insert_with(|| serde_json::json!({}));
                    if let Some(o) = entry.as_object_mut() {
                        o.insert(field.to_string(), serde_json::Value::String(v));
                    }
                }
                Ev::Done { note, sources, memory } => {
                    let mut n = note.unwrap_or_default();
                    if !sources.is_empty() {
                        let list: Vec<String> = sources.iter().take(4).map(|(t, _)| ui::fit(t, 36)).collect();
                        n = format!("{n} · sources: {}", list.join(" · ")).trim_start_matches(" · ").to_string();
                    }
                    if !n.is_empty() {
                        m.note = Some(n);
                    }
                    if let Some(mem) = memory {
                        if mem != self.memory {
                            self.memory = mem;
                            save_mem = true;
                        }
                    }
                    finished = true;
                }
                Ev::Error(e) => {
                    if m.content.trim().is_empty() {
                        m.content = format!("⚠ {e}");
                    } else {
                        m.note = Some(format!("⚠ {e}"));
                    }
                    finished = true;
                }
            }
        }
        if save_mem {
            self.save_memory();
        }
        if finished {
            self.stream = None;
            self.persist();
            let _ = cx;
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        let hints: &[(&str, &str)] = if self.confirm_delete {
            &[("y", "delete this chat"), ("esc", "keep it")]
        } else if self.stream.is_some() {
            &[("esc", "stop"), ("F2-F7", "apps"), ("F8", "play/pause"), ("alt p", "palette")]
        } else {
            &[("enter", "send"), ("ctrl+r", "regenerate"), ("ctrl+n", "new chat"), ("/", "commands"), ("F2-F7", "apps"), ("F8", "play/pause"), ("alt p", "palette")]
        };
        let area = ui::hint_line(f, area, hints, t);
        if area.height < 5 {
            return;
        }
        let comp = Rect { y: area.bottom() - 3, height: 3, ..area };
        let body = Rect { height: area.height - 3, ..area };
        let body = Rect { x: body.x + 1, width: body.width.saturating_sub(2), ..body };

        // ---- messages (or the hero on a new chat)
        if self.chat.messages.is_empty() && self.info.is_empty() {
            self.draw_hero(f, body, cx);
        } else {
            let width = body.width as usize;
            let mut lines: Vec<Line<'static>> = vec![Line::raw("")];
            for i in 0..self.chat.messages.len() {
                lines.extend(self.msg_lines(i, width, cx));
            }
            if let Some(s) = &self.stream {
                let last_empty = self.chat.messages.last().map(|m| m.content.is_empty()).unwrap_or(true);
                if last_empty {
                    // drop the trailing blank of the empty reply and show the spinner under its header
                    lines.pop();
                    let e = s.started.elapsed();
                    let spin = SPIN[(e.as_millis() / 100) as usize % SPIN.len()];
                    let verb = VERBS[(e.as_secs() / 3) as usize % VERBS.len()];
                    let status = if s.status.is_empty() { String::new() } else { format!(" · {}", s.status) };
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {spin} "), ui::accent(t)),
                        Span::styled(format!("{verb}…"), Style::default().fg(t.shine)),
                        Span::styled(format!("{status} · {}s", e.as_secs()), ui::muted(t)),
                    ]));
                }
            }
            for l in &self.info {
                lines.extend(md::wrap(vec![Span::styled(l.clone(), ui::muted(t))], width, "  ", "    "));
            }
            let h = body.height as usize;
            let max_scroll = lines.len().saturating_sub(h);
            self.scroll = self.scroll.min(max_scroll);
            let start = max_scroll - self.scroll;
            let visible: Vec<Line> = lines.into_iter().skip(start).take(h).collect();
            f.render_widget(Paragraph::new(visible), body);
            if max_scroll > 0 {
                // a thin scrollbar on the right edge
                let bar_h = ((h * h) / (h + max_scroll)).max(1);
                let pos = ((start * (h - bar_h)) / max_scroll.max(1)).min(h - bar_h);
                for r in 0..bar_h {
                    f.render_widget(Paragraph::new(Span::styled("▐", Style::default().fg(t.frame))), Rect { x: body.right(), y: body.y + (pos + r) as u16, width: 1, height: 1 });
                }
            }
        }

        // ---- the / command menu, above the composer
        let items = self.menu();
        if !items.is_empty() && cx.focused {
            let rows = items.len().min(10) as u16;
            let mh = rows + 2;
            let mr = Rect { x: comp.x, y: comp.y.saturating_sub(mh), width: comp.width, height: mh };
            f.render_widget(ratatui::widgets::Clear, mr);
            let title = match self.input.split_once(' ') {
                Some((cmd, _)) => format!("{cmd} · pick one · ↑↓ tab enter"),
                None => "commands · ↑↓ tab enter".to_string(),
            };
            let inner = ui::frame(f, mr, &title, Some(&format!("{}/{}", self.menu_sel.min(items.len() - 1) + 1, items.len())), false, t);
            self.menu_sel = self.menu_sel.min(items.len() - 1);
            let start = self.menu_sel.saturating_sub(rows as usize - 1);
            for (row, it) in items.iter().skip(start).take(rows as usize).enumerate() {
                let d = it.desc.as_str();
                let on = start + row == self.menu_sel;
                let cs = if on { Style::default().fg(t.accent).add_modifier(Modifier::BOLD) } else { Style::default().add_modifier(Modifier::BOLD) };
                let left = it.left.clone();
                let lw = (inner.width as usize / 2).min(36);
                let line = Line::from(vec![
                    Span::styled(if on { "▌" } else { " " }, ui::accent(t)),
                    Span::styled(format!("{:<lw$}", ui::fit(&left, lw)), cs),
                    Span::styled(d.to_string(), if on { Style::default().add_modifier(Modifier::BOLD) } else { ui::muted(t) }),
                ]);
                f.render_widget(Paragraph::new(line), Rect { y: inner.y + row as u16, height: 1, ..inner });
            }
        }

        // ---- composer
        let border = if cx.focused { t.accent } else { t.frame };
        let block = ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(border));
        let inner = block.inner(comp);
        f.render_widget(block, comp);
        let room = inner.width.saturating_sub(3) as usize;
        let shown: String = self.input.replace('\n', "⏎");
        let before: String = shown.chars().take(self.cursor).collect();
        let bw = before.width();
        let skip = bw.saturating_sub(room.saturating_sub(1));
        let (text, style) = if self.input.is_empty() {
            let name = providers::label(&self.provider_of()).to_lowercase();
            (format!("message {name}…   (/ for commands · /model to switch)"), ui::muted(t))
        } else {
            let mut s = String::new();
            let mut w = 0;
            for c in shown.chars() {
                let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                if w + cw > skip {
                    s.push(c);
                }
                w += cw;
            }
            (ui::fit(&s, room), Style::default())
        };
        f.render_widget(Paragraph::new(Line::from(vec![Span::styled("› ", ui::bold_accent(t)), Span::styled(text, style)])), inner);
        if cx.focused && !self.confirm_delete {
            let x = inner.x + 2 + (bw - skip) as u16;
            f.set_cursor_position(Position { x: x.min(inner.right().saturating_sub(1)), y: inner.y });
        }
    }

    fn key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if k.modifiers.contains(KeyModifiers::ALT) {
            return false;
        }
        if self.confirm_delete {
            self.confirm_delete = false;
            if k.code == KeyCode::Char('y') {
                store::delete(&self.chat.id);
                let id = self.chat.id.clone();
                self.chats.retain(|c| c.id != id);
                self.chat = store::Chat::new(&self.provider_of());
                self.cache.clear();
                cx.notify("chat deleted");
            }
            return true;
        }
        let items = self.menu();
        match k.code {
            KeyCode::Char('n') if ctrl => self.new_chat(),
            KeyCode::Char('r') if ctrl => self.retry(cx),
            KeyCode::Char('d') if ctrl => {
                if !self.chat.messages.is_empty() {
                    self.confirm_delete = true;
                }
            }
            KeyCode::Char('u') if ctrl => {
                self.input.clear();
                self.cursor = 0;
            }
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.input.chars().count(),
            KeyCode::Char(c) if !ctrl => {
                self.insert(&c.to_string());
                self.history_pos = None;
            }
            KeyCode::Esc => {
                if self.stream.is_some() {
                    self.stop();
                } else if !self.input.is_empty() {
                    self.input.clear();
                    self.cursor = 0;
                } else {
                    self.info.clear();
                }
            }
            KeyCode::Enter => {
                // shift/alt+enter: newline
                if k.modifiers.contains(KeyModifiers::SHIFT) {
                    self.insert("\n");
                    return true;
                }
                let mut text = self.input.trim().to_string();
                if let Some(it) = items.get(self.menu_sel.min(items.len().saturating_sub(1))) {
                    if !it.run {
                        // a command that needs its argument: complete it, the menu then lists the choices
                        self.input = it.fill.clone();
                        self.cursor = self.input.chars().count();
                        self.menu_sel = 0;
                        return true;
                    }
                    text = it.fill.clone();
                }
                if text.is_empty() {
                    return true;
                }
                self.input.clear();
                self.cursor = 0;
                self.history_pos = None;
                if text.starts_with('/') {
                    self.run_command(&text, cx);
                } else {
                    self.send(text, cx);
                }
            }
            KeyCode::Tab if !items.is_empty() => {
                let it = &items[self.menu_sel.min(items.len() - 1)];
                // tab on a command that takes an argument goes straight to its choices
                self.input = if it.run && !it.fill.contains(' ') && self.arg_options(&it.fill).is_empty() { it.fill.clone() } else if it.fill.ends_with(' ') || it.fill.contains(' ') { it.fill.clone() } else { format!("{} ", it.fill) };
                self.cursor = self.input.chars().count();
                self.menu_sel = 0;
            }
            KeyCode::Up if !items.is_empty() => self.menu_sel = self.menu_sel.saturating_sub(1),
            KeyCode::Down if !items.is_empty() => self.menu_sel = (self.menu_sel + 1).min(items.len() - 1),
            KeyCode::Up if ctrl => self.scroll += 1,
            KeyCode::Down if ctrl => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Up => {
                // recall earlier messages you sent
                let mine: Vec<&str> = self.chat.messages.iter().filter(|m| m.role == "user").map(|m| m.content.as_str()).collect();
                if !mine.is_empty() && (self.input.is_empty() || self.history_pos.is_some()) {
                    let pos = self.history_pos.map(|p| p.saturating_sub(1)).unwrap_or(mine.len() - 1);
                    self.history_pos = Some(pos);
                    self.input = mine[pos].to_string();
                    self.cursor = self.input.chars().count();
                } else {
                    self.scroll += 1;
                }
            }
            KeyCode::Down => {
                if let Some(p) = self.history_pos {
                    let mine: Vec<&str> = self.chat.messages.iter().filter(|m| m.role == "user").map(|m| m.content.as_str()).collect();
                    if p + 1 < mine.len() {
                        self.history_pos = Some(p + 1);
                        self.input = mine[p + 1].to_string();
                    } else {
                        self.history_pos = None;
                        self.input.clear();
                    }
                    self.cursor = self.input.chars().count();
                } else {
                    self.scroll = self.scroll.saturating_sub(1);
                }
            }
            KeyCode::PageUp => self.scroll += 10,
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.chars().count()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let mut chars: Vec<char> = self.input.chars().collect();
                    chars.remove(self.cursor - 1);
                    self.input = chars.into_iter().collect();
                    self.cursor -= 1;
                    self.menu_sel = 0;
                }
            }
            KeyCode::Delete => {
                let mut chars: Vec<char> = self.input.chars().collect();
                if self.cursor < chars.len() {
                    chars.remove(self.cursor);
                    self.input = chars.into_iter().collect();
                }
            }
            _ => return false,
        }
        true
    }

    fn paste(&mut self, text: &str, _cx: &mut Cx) {
        self.insert(&text.replace("\r\n", "\n"));
    }

    fn mouse(&mut self, ev: MouseEvent, _area: Rect, cx: &mut Cx) {
        match ev.kind {
            MouseEventKind::ScrollUp => self.scroll += 3,
            MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_sub(3),
            MouseEventKind::Down(MouseButton::Left) => {
                let pos = Position { x: ev.column, y: ev.row };
                if self.chat.messages.is_empty() {
                    if let Some((_, s)) = self.hero_hits.iter().find(|(r, _)| r.contains(pos)).cloned() {
                        self.send(s, cx);
                    }
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ sidebar: new chat + the chat list
    fn side(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        self.side_hits.clear();
        let r = Rect { height: 1, ..area };
        ui::side_row(f, r, "new", "new chat", "ctrl+n", false, t);
        self.side_hits.push((r, SideItem::New));
        let mut rows: Vec<(Option<&'static str>, usize)> = vec![];
        let now = store::now();
        let mut last = "";
        for (i, c) in self.chats.iter().enumerate() {
            let b = store::bucket(c.updated, now, self.offset);
            if b != last {
                rows.push((Some(b), 0));
                last = b;
            }
            rows.push((None, i));
        }
        let list = Rect { y: area.y + 2, height: area.height.saturating_sub(2), ..area };
        let h = list.height as usize;
        self.side_scroll = self.side_scroll.min(rows.len().saturating_sub(h));
        for (n, (group, i)) in rows.iter().skip(self.side_scroll).take(h).enumerate() {
            let r = Rect { y: list.y + n as u16, height: 1, ..list };
            if let Some(g) = group {
                f.render_widget(Paragraph::new(Span::styled(format!("── {g}"), ui::muted(t))), r);
                continue;
            }
            let c = &self.chats[*i];
            let on = c.id == self.chat.id;
            let style = if on { Style::default().fg(t.accent).add_modifier(Modifier::BOLD) } else { Style::default() };
            f.render_widget(Paragraph::new(Span::styled(format!("  {}", ui::fit(&c.title, r.width as usize - 2)), style)), r);
            self.side_hits.push((r, SideItem::Chat(*i)));
        }
    }

    fn side_mouse(&mut self, ev: MouseEvent, _area: Rect, _cx: &mut Cx) {
        match ev.kind {
            MouseEventKind::ScrollUp => self.side_scroll = self.side_scroll.saturating_sub(3),
            MouseEventKind::ScrollDown => self.side_scroll += 3,
            MouseEventKind::Down(MouseButton::Left) => {
                let pos = Position { x: ev.column, y: ev.row };
                if let Some((_, item)) = self.side_hits.iter().find(|(r, _)| r.contains(pos)).cloned() {
                    match item {
                        SideItem::New => self.new_chat(),
                        SideItem::Chat(i) => self.open_chat(i),
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Kit;

    #[test]
    fn chat_views() {
        let mut k = Kit::new();
        let mut c = Chat::new(&k.config);
        println!("{}", k.render_html(&mut c, 130, 40, "target/snap/chat-hero.html"));
        // a canned conversation (no network) to check the message layout
        c.chat.messages.push(store::Msg { role: "user".into(), content: "what's a good name for a terminal app?".into(), model: None, note: None, steps: vec![] });
        c.chat.messages.push(store::Msg {
            role: "assistant".into(),
            content: "A few ideas:\n\n- **oriel** — a window that juts out\n- `nest`, but taken\n\n```rust\nfn main() { println!(\"hi\"); }\n```\nPick the one you like.".into(),
            model: Some("wren".into()),
            note: Some("34 tokens · 171 tok/s · balanced".into()),
            steps: vec![],
        });
        c.chat.title = "names".into();
        println!("{}", k.render_html(&mut c, 130, 40, "target/snap/chat-msgs.html"));
        k.typ(&mut c, "/mo");
        println!("{}", k.render_html(&mut c, 130, 40, "target/snap/chat-menu.html"));
        k.key(&mut c, KeyCode::Enter); // completes "/model " and lists the AIs
        assert_eq!(c.input, "/model ");
        let s = k.render_html(&mut c, 130, 40, "target/snap/chat-menu-model.html");
        assert!(s.contains("claude") && s.contains("ollama"), "/model choices missing");
        k.typ(&mut c, "cl");
        assert_eq!(c.menu().len(), 1);
        c.input = "/theme ".into();
        c.cursor = 7;
        let s = k.render(&mut c, 130, 40);
        assert!(s.contains("ocean") && s.contains("terminal"), "/theme choices missing");
        c.input.clear();
        c.cursor = 0;
        println!("{}", k.render_side(&mut c, 32, 24));
    }

    #[test]
    fn chat_no_ai() {
        let mut k = Kit::new();
        let mut c = Chat::new(&k.config);
        c.provider = "none".into();
        c.chat.provider = None;
        let s = k.render_html(&mut c, 130, 40, "target/snap/chat-none.html");
        assert!(s.contains("no AI is set up"));
        println!("available here: {:?}", providers::available(&k.config.ai));
    }

    /// Talks to Wren's real server if it's up; skipped otherwise. `cargo test chat_wren_live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn chat_wren_live() {
        let mut k = Kit::new();
        let mut c = Chat::new(&k.config);
        c.chat.provider = Some("wren".into());
        let mut cx_actions = vec![];
        {
            let mut cx = Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: &mut cx_actions, focused: true, time: 1.0 };
            c.send("what is 12*7?".into(), &mut cx);
        }
        for _ in 0..300 {
            k.wait_wake(&mut c, 100);
            if c.stream.is_none() {
                break;
            }
        }
        let last = c.chat.messages.last().unwrap();
        println!("reply: {:?}\nnote: {:?}", last.content, last.note);
        assert!(!last.content.is_empty());
        store::delete(&c.chat.id); // don't leave a test chat in the list
    }
}

#[cfg(test)]
pub fn demo_now() -> f64 {
    store::now()
}

