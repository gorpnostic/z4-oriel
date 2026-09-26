//! The ai app: chat list in the sidebar, messages (your words in boxes on the right, replies with a header and
//! markdown on the left), the big logo on a new chat, a composer with a / command menu. Coding agents (Claude
//! Code, Codex) show their whole transcript live: every tool call, diffs, command output, todos, subagents.

mod activity;
mod agent;
pub mod approve;
mod md;
pub mod providers;
mod store;

/// A diff line's background for a theme (line tint, changed-word tint): the themes app previews with it.
pub(crate) use activity::band as diff_band;

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
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

pub(crate) const COMMANDS: &[(&str, &str, &str)] = &[
    ("/new", "", "start a new chat"),
    ("/retry", "", "regenerate the last reply"),
    ("/provider", "<ai>", "switch AI: claude, codex, ollama, openai, anthropic — remembered"),
    ("/model", "<model>", "switch model for the current AI — remembered per AI"),
    ("/cwd", "<folder>", "folder Claude Code / Codex work in for this chat"),
    ("/perms", "<ask|edits|auto|plan|bypass>", "what coding agents may do — remembered for every chat (shift+tab cycles)"),
    ("/effort", "<low|medium|high|xhigh|max|ultracode>", "how hard coding agents think — remembered for every chat"),
    ("/key", "<openai|anthropic> <key>", "save an API key"),
    ("/note", "", "save the last reply to notes"),
    ("/save", "", "export this chat as a markdown file"),
    ("/theme", "<name|edit|new>", "switch theme · edit opens the theme editor · new <name> makes your own"),
    ("/play", "", "music: play / pause"),
    ("/next", "", "music: next song"),
    ("/prev", "", "music: previous song"),
    ("/music", "", "go to the music app"),
    ("/sidebar", "", "hide / show the sidebar"),
    ("/icons", "", "nerd font icons on / off"),
    ("/info", "", "what's running: AI, model, folder"),
    ("/delete", "", "delete this chat"),
    ("/help", "", "keys and commands (F10: the full guide)"),
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

/// The permission modes for coding agents in chat (Claude Code's own modes). Remembered in the config.
const PERMS: &[(&str, &str)] = &[
    ("ask", "asks you before each edit or command (y allow · n deny · a always allow that tool)"),
    ("edits", "edits files in the chat's folder; anything else is refused (default)"),
    ("auto", "auto mode: Claude decides what's safe and asks you about the rest"),
    ("plan", "read-only: looks and plans, changes nothing"),
    ("bypass", "bypass permissions: never asks, allows everything — only in folders you trust"),
];

/// Canonical permission name; the old "full" / "read" still work.
fn norm_perms(s: &str) -> Option<&'static str> {
    match s.trim().to_lowercase().as_str() {
        "ask" | "default" => Some("ask"),
        "edits" | "acceptedits" | "accept" => Some("edits"),
        "plan" | "read" | "readonly" | "read-only" => Some("plan"),
        "auto" => Some("auto"),
        "bypass" | "full" | "yolo" | "bypasspermissions" => Some("bypass"),
        _ => None,
    }
}

/// /effort levels, like Claude Code's. ultracode = max plus its multi-agent mode.
const EFFORTS: &[(&str, &str)] = &[
    ("low", "quick answers, fewest tokens"),
    ("medium", "a balance"),
    ("high", "thinks it through"),
    ("xhigh", "thinks harder"),
    ("max", "thinks as hard as it can"),
    ("ultracode", "max, plus Claude Code's multi-agent mode: it can run a whole team of agents (uses a lot)"),
    ("default", "the agent's own default"),
];

/// shift+tab walks through these, like Claude Code.
const PERM_CYCLE: &[&str] = &["ask", "edits", "auto", "plan", "bypass"];

/// The mode line on the input box, Claude Code style: glyph, words, colour.
fn perm_badge(p: &str, t: &crate::theme::Theme) -> (&'static str, &'static str, ratatui::style::Color) {
    use ratatui::style::Color;
    match p {
        "edits" => ("⏵⏵", "accept edits on", t.accent),
        "auto" => ("⏵⏵", "auto mode on", Color::Rgb(0xe6, 0xc4, 0x6a)),
        "plan" => ("⏸", "plan mode on", t.shine),
        "bypass" => ("⏵⏵", "bypass permissions on", t.danger),
        _ => ("⏵", "asks before changes", t.muted),
    }
}

const SUGGESTIONS: &[&str] = &["explain how rainbows form", "give me 3 tips for better sleep", "code a snake game in html", "what is a python function?"];

use activity::SPIN;
const VERBS: &[&str] = &["thinking", "pondering", "noodling", "brewing", "untangling", "scheming", "percolating", "tinkering"];

struct Stream {
    stop: Arc<AtomicBool>,
    inbox: Arc<Mutex<Vec<Ev>>>,
    status: String,
    started: Instant,
    tokens: u64,
    /// Straight to the running agent (Claude Code reads it at its next step); None = queue until the reply ends.
    steer: Option<std::sync::mpsc::Sender<String>>,
}

/// Answering a set of Claude's questions: which one, the highlighted choice, ticks (several allowed), your own
/// words (Other), and the answers so far.
#[derive(Default)]
struct QState {
    idx: usize,
    sel: usize,
    ticked: Vec<bool>,
    other: Option<String>,
    answers: serde_json::Map<String, serde_json::Value>,
}

/// A message typed while a reply was running.
struct Queued {
    text: String,
    /// already handed to the agent mid-reply (it can't be taken back, only waited for)
    sent: bool,
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
    cache: HashMap<usize, (u64, Vec<Line<'static>>, Vec<(usize, String)>)>,
    provider: String,
    perms: String,
    /// /effort ("" = the agent's default)
    effort: String,
    keys: HashMap<String, String>,
    /// ctrl+o: every call shows its full diff / output.
    expanded: bool,
    /// Calls clicked open (closed, when expanded).
    open: HashSet<String>,
    /// Screen row -> call id, for clicks.
    tool_hits: Vec<(u16, String)>,
    /// Claude Code waiting for a yes/no (/perms ask), oldest first.
    asks: VecDeque<approve::Ask>,
    /// Tools the user said "always" to, for this session.
    always: HashSet<String>,
    /// Claude's questions waiting for you (AskUserQuestion), oldest first, and where you are in the first one.
    questions: VecDeque<approve::Question>,
    qs: QState,
    info: Vec<String>,
    avail: Vec<&'static str>,
    /// the model picked per AI (/model), remembered in the config
    models: std::collections::BTreeMap<String, String>,
    /// models installed in Ollama, fetched in the background for the /model menu
    ollama_models: Arc<Mutex<Vec<String>>>,
    confirm_delete: bool,
    side_hits: Vec<(Rect, SideItem)>,
    side_scroll: usize,
    hero_hits: Vec<(Rect, String)>,
    launch_dir: PathBuf,
    history_pos: Option<usize>,
    offset: i64,
    /// Messages typed while the AI was replying, oldest first (enter queues, ctrl+x s sends now).
    queue: Vec<Queued>,
    /// ctrl+x was pressed: s sends now.
    chord_x: bool,
}

impl Chat {
    pub fn new(cfg: &crate::config::Config) -> Self {
        let chats = store::load_all();
        let avail = providers::available(&cfg.ai);
        // the configured AI if this machine has it, else the first one it does have
        let provider = if !cfg.ai.provider.is_empty() && avail.contains(&cfg.ai.provider.as_str()) {
            cfg.ai.provider.clone()
        } else {
            avail.first().map(|s| s.to_string()).unwrap_or_else(|| "none".into())
        };
        let ollama_models: Arc<Mutex<Vec<String>>> = Arc::default();
        if avail.contains(&"ollama") && !cfg!(test) {
            let (url, into) = (cfg.ai.ollama_url.clone(), ollama_models.clone());
            std::thread::spawn(move || {
                let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(3))).build().into();
                if let Ok(r) = agent.get(&format!("{}/api/tags", url.trim_end_matches('/'))).call() {
                    if let Some(v) = r.into_body().read_to_string().ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) {
                        let names: Vec<String> = v["models"].as_array().map(|a| a.iter().filter_map(|m| m["name"].as_str().map(String::from)).collect()).unwrap_or_default();
                        *into.lock().unwrap() = names;
                    }
                }
            });
        }
        let mut first = store::Chat::new(&provider);
        first.model = cfg.ai.models.get(&provider).cloned().filter(|m| !m.is_empty());
        Chat {
            chats,
            chat: first,
            models: cfg.ai.models.clone(),
            ollama_models,
            input: String::new(),
            cursor: 0,
            scroll: 0,
            stream: None,
            menu_sel: 0,
            cache: HashMap::new(),
            provider,
            perms: norm_perms(&cfg.ai.perms).unwrap_or("edits").to_string(),
            effort: cfg.ai.effort.clone(),
            keys: HashMap::new(),
            expanded: false,
            open: HashSet::new(),
            tool_hits: vec![],
            asks: VecDeque::new(),
            always: HashSet::new(),
            questions: VecDeque::new(),
            qs: QState::default(),
            info: vec![],
            avail,
            confirm_delete: false,
            side_hits: vec![],
            side_scroll: 0,
            hero_hits: vec![],
            launch_dir: std::env::current_dir().unwrap_or_default(),
            history_pos: None,
            offset: crate::app::local_offset_secs(),
            queue: vec![],
            chord_x: false,
        }
    }

    /// The chat's AI, or the current default when it used one oriel doesn't have (an old chat).
    fn provider_of(&self) -> String {
        match &self.chat.provider {
            Some(p) if providers::is_known(p) => p.clone(),
            _ => self.provider.clone(),
        }
    }

    /// Where CLI agents run: the chat's folder (/cwd), else the folder oriel was started in. Only if that folder
    /// no longer exists does it fall back to oriel's own work folder.
    fn workdir(&self) -> PathBuf {
        let d = self.chat.cwd.clone().map(PathBuf::from).unwrap_or_else(|| self.launch_dir.clone());
        if !d.is_dir() {
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

    fn new_chat(&mut self) {
        self.stop();
        self.persist_if_changed();
        let p = self.provider_of();
        self.chat = store::Chat::new(&p);
        self.chat.model = self.models.get(&p).cloned().filter(|m| !m.is_empty());
        self.cache.clear();
        self.scroll = 0;
        self.info.clear();
    }

    fn open_chat(&mut self, i: usize) {
        if i >= self.chats.len() {
            return;
        }
        // by id: saving the chat we're leaving can reorder the list under us
        let id = self.chats[i].id.clone();
        self.stop();
        self.persist_if_changed();
        let Some(c) = self.chats.iter().find(|c| c.id == id).cloned() else { return };
        self.chat = c;
        self.cache.clear();
        self.scroll = 0;
        self.info.clear();
    }

    /// Save only if it differs from the saved copy — just looking at a chat mustn't bump it to the top.
    fn persist_if_changed(&mut self) {
        if let Some(old) = self.chats.iter().find(|c| c.id == self.chat.id) {
            if serde_json::to_value(old).ok() == serde_json::to_value(&self.chat).ok() {
                return;
            }
        }
        self.persist();
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
            self.asks.clear(); // unanswered approvals become a no
            self.questions.clear(); // and unanswered questions a skip
            if let Some(m) = self.chat.messages.last_mut() {
                if m.role == "assistant" {
                    activity::settle(&mut m.parts, "stopped");
                    if m.content.trim().is_empty() && m.parts.is_empty() {
                        m.content = "(stopped)".into();
                    }
                    m.note = Some(format!("{} · stopped", m.note.clone().unwrap_or_default()).trim_start_matches(" · ").to_string());
                }
            }
            self.persist();
        }
        self.unqueue_to_box("stopped");
    }

    /// Queued messages that never got sent go back into the composer, so nothing you typed is lost.
    fn unqueue_to_box(&mut self, why: &str) {
        if self.queue.is_empty() {
            return;
        }
        let mut parts: Vec<String> = self.queue.drain(..).map(|q| q.text).collect();
        if !self.input.trim().is_empty() {
            parts.push(self.input.trim().to_string());
        }
        self.input = parts.join("\n\n");
        self.cursor = self.input.chars().count();
        self.info.push(format!("{why} · your queued message is back in the box: enter sends it"));
    }

    /// Typed while a reply runs: hand it to the agent now if it can take it mid-reply, else hold it.
    fn enqueue(&mut self, text: String) {
        let sent = self.stream.as_ref().and_then(|s| s.steer.as_ref()).is_some_and(|tx| tx.send(text.clone()).is_ok());
        self.queue.push(Queued { text, sent });
        self.scroll = 0;
    }

    /// Keys while Claude is asking you something: ↑↓ or a number to choose, space to tick (when several are
    /// allowed), enter to answer, typing (or Other) for your own words, esc to skip.
    fn question_key(&mut self, k: KeyEvent) {
        let Some(q) = self.questions.front() else { return };
        let Some(cur) = q.qs.get(self.qs.idx).cloned() else { return };
        let n = cur.options.len() + 1; // the choices, then Other
        if self.qs.ticked.len() != cur.options.len() {
            self.qs.ticked = vec![false; cur.options.len()];
        }
        if let Some(text) = &mut self.qs.other {
            match k.code {
                KeyCode::Esc => self.qs.other = None,
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Enter => {
                    let a = text.trim().to_string();
                    if !a.is_empty() {
                        self.answer_question(&cur.question, a);
                    }
                }
                KeyCode::Char(c) if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => text.push(c),
                _ => {}
            }
            return;
        }
        match k.code {
            KeyCode::Up => self.qs.sel = (self.qs.sel + n - 1) % n,
            KeyCode::Down | KeyCode::Tab => self.qs.sel = (self.qs.sel + 1) % n,
            KeyCode::Char(c @ '1'..='9') if (c as usize - '0' as usize) <= n => {
                self.qs.sel = c as usize - '1' as usize;
                if self.qs.sel == n - 1 {
                    self.qs.other = Some(String::new());
                } else if cur.multi {
                    self.qs.ticked[self.qs.sel] = !self.qs.ticked[self.qs.sel];
                } else {
                    self.answer_question(&cur.question, cur.options[self.qs.sel].0.clone());
                }
            }
            KeyCode::Char(' ') if cur.multi && self.qs.sel < cur.options.len() => self.qs.ticked[self.qs.sel] = !self.qs.ticked[self.qs.sel],
            KeyCode::Enter => {
                if self.qs.sel == n - 1 {
                    self.qs.other = Some(String::new());
                } else if cur.multi {
                    let mut picked: Vec<String> = cur.options.iter().zip(&self.qs.ticked).filter(|(_, t)| **t).map(|(o, _)| o.0.clone()).collect();
                    if picked.is_empty() {
                        picked.push(cur.options[self.qs.sel].0.clone());
                    }
                    self.answer_question(&cur.question, picked.join(", "));
                } else {
                    self.answer_question(&cur.question, cur.options[self.qs.sel].0.clone());
                }
            }
            KeyCode::Esc => {
                if let Some(q) = self.questions.pop_front() {
                    let _ = q.reply.send(None);
                }
                self.qs = QState::default();
            }
            // just start typing to answer in your own words
            KeyCode::Char(c) if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                self.qs.sel = n - 1;
                self.qs.other = Some(c.to_string());
            }
            _ => {}
        }
    }

    fn answer_question(&mut self, question: &str, answer: String) {
        self.qs.answers.insert(question.to_string(), serde_json::Value::String(answer));
        self.qs.idx += 1;
        self.qs.sel = 0;
        self.qs.ticked.clear();
        self.qs.other = None;
        let done = self.questions.front().is_some_and(|q| self.qs.idx >= q.qs.len());
        if done {
            if let Some(q) = self.questions.pop_front() {
                let _ = q.reply.send(Some(std::mem::take(&mut self.qs.answers)));
            }
            self.qs = QState::default();
        }
    }

    /// ctrl+x s: stop the reply and send everything queued (and whatever is in the box) right away.
    fn send_now(&mut self, cx: &mut Cx) {
        let mut parts: Vec<String> = self.queue.drain(..).map(|q| q.text).collect();
        let typed = self.input.trim().to_string();
        if !typed.is_empty() {
            parts.push(typed);
        }
        if parts.is_empty() {
            return;
        }
        self.input.clear();
        self.cursor = 0;
        self.stop();
        self.send(parts.join("\n\n"), cx);
    }

    fn send(&mut self, text: String, cx: &mut Cx) {
        if self.stream.is_some() {
            self.enqueue(text);
            return;
        }
        if self.chat.messages.is_empty() {
            self.chat.title = store::title_from(&text);
            if self.chat.cwd.is_none() {
                self.chat.cwd = Some(self.launch_dir.to_string_lossy().to_string());
            }
        }
        self.chat.messages.push(store::Msg { role: "user".into(), content: text, ..Default::default() });
        self.start_reply(cx);
    }

    /// Ask the provider for a reply to the conversation as it stands (last message = the user's).
    fn start_reply(&mut self, cx: &mut Cx) {
        let provider = self.provider_of();
        let messages: Vec<(String, String)> = self.chat.messages.iter().map(|m| (m.role.clone(), m.content.clone())).collect();
        self.chat.messages.push(store::Msg { role: "assistant".into(), model: Some(provider.clone()), ..Default::default() });
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
        let (steer, steer_rx) = if providers::steerable(&provider) {
            let (tx, rx) = std::sync::mpsc::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let req = providers::Request {
            provider: provider.clone(),
            model: self.chat.model.clone(),
            messages,
            cwd: self.workdir(),
            perms: self.perms.clone(),
            state: self.chat.state.clone(),
            cfg,
            steer: steer_rx,
            effort: self.effort.clone(),
        };
        let (ib, waker) = (inbox.clone(), cx.waker());
        providers::start(req, stop.clone(), move |ev| {
            ib.lock().unwrap().push(ev);
            waker.wake();
        });
        self.stream = Some(Stream { stop, inbox, status: String::new(), started: Instant::now(), tokens: 0, steer });
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
            // /model <name>: a model for the current AI; the old "/model <ai> [model]" still works
            "/model" if !arg.is_empty() && !providers::PROVIDERS.iter().any(|p| p.0 == arg.split_whitespace().next().unwrap_or("").to_lowercase()) => {
                let p = self.provider_of();
                let model = if arg.eq_ignore_ascii_case("default") { None } else { Some(arg.clone()) };
                self.chat.model = model.clone();
                self.chat.state.remove(&p); // a CLI session is tied to its model
                self.models.insert(p.clone(), model.clone().unwrap_or_default());
                let mut c = crate::config::load();
                c.ai.models.insert(p.clone(), model.clone().unwrap_or_default());
                crate::config::save(&c);
                cx.notify(format!("{}{} · {} · remembered", ui::lead("ai"), providers::label(&p), model.unwrap_or_else(|| "default model".into())));
            }
            "/model" if arg.is_empty() => {
                let p = self.provider_of();
                self.info.push(format!("{} is using {}. Pick one with /model <name>:", providers::label(&p), self.chat.model.clone().unwrap_or_else(|| "its default model".into())));
                for (m, what) in self.models_for(&p) {
                    self.info.push(format!("  {m:<22} {what}"));
                }
            }
            "/provider" if arg.is_empty() => {
                self.info.push("pick an AI with /provider <name> (then /model for its model):".into());
                for (id, label, what) in providers::PROVIDERS {
                    let here = if self.avail.contains(id) { "" } else { "  (not set up here)" };
                    self.info.push(format!("  {id:<10} {label} — {what}{here}"));
                }
            }
            "/provider" | "/model" => {
                {
                    let mut a = arg.split_whitespace();
                    let id = a.next().unwrap_or("claude").to_lowercase();
                    if providers::PROVIDERS.iter().any(|p| p.0 == id) && !self.avail.contains(&id.as_str()) {
                        self.info.push(format!("{} isn't set up on this computer", providers::label(&id)));
                        self.info.push(match id.as_str() {
                            "claude" => "  install Claude Code: npm i -g @anthropic-ai/claude-code, then run `claude` once to sign in".into(),
                            "codex" => "  install Codex: npm i -g @openai/codex, then `codex login`".into(),
                            "ollama" => "  install Ollama from ollama.com and pull a model (ollama pull llama3.2)".into(),
                            _ => format!("  add a key: /key {id} <key>"),
                        });
                    } else if providers::PROVIDERS.iter().any(|p| p.0 == id) {
                        // an explicit model, else the one remembered for this AI
                        let model = a.next().map(String::from).or_else(|| self.models.get(&id).cloned()).filter(|m| !m.is_empty());
                        self.chat.provider = Some(id.clone());
                        self.chat.model = model.clone();
                        self.provider = id.clone();
                        let mut c = crate::config::load();
                        c.ai.provider = id.clone();
                        if let Some(m) = &model {
                            c.ai.models.insert(id.clone(), m.clone());
                            self.models.insert(id.clone(), m.clone());
                        }
                        crate::config::save(&c);
                        cx.notify(format!("{}{} · {} · remembered", ui::lead("ai"), providers::label(&id), model.unwrap_or_else(|| "default model".into())));
                    } else {
                        self.info.push(format!("no AI called {id} — try /provider"));
                    }
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
            "/effort" => {
                let want = arg.trim().to_lowercase();
                if let Some((level, what)) = EFFORTS.iter().find(|(l, _)| *l == want) {
                    self.effort = if *level == "default" { String::new() } else { level.to_string() };
                    let mut c = crate::config::load();
                    c.ai.effort = self.effort.clone();
                    crate::config::save(&c);
                    cx.notify(format!("effort: {level} · {what}"));
                } else {
                    self.info.push(format!("effort now: {}. Choose one:", if self.effort.is_empty() { "default" } else { &self.effort }));
                    for (l, what) in EFFORTS {
                        self.info.push(format!("  /effort {l:<10} {what}"));
                    }
                }
            }
            "/perms" => match norm_perms(&arg) {
                Some(p) => {
                    self.perms = p.to_string();
                    // remembered: every new chat (and the next time oriel starts) uses it
                    let mut c = crate::config::load();
                    c.ai.perms = p.to_string();
                    crate::config::save(&c);
                    cx.notify(format!("agent permissions: {p} · saved for every chat"));
                }
                None => {
                    self.info.push(format!("permissions now: {} (saved for every chat). Choose one:", self.perms));
                    for (k, what) in PERMS {
                        self.info.push(format!("  /perms {k:<7} {what}"));
                    }
                }
            },
            "/theme" if arg == "edit" || arg == "editor" => cx.act(Action::GotoApp("themes")),
            "/theme" if arg == "new" || arg.starts_with("new ") => {
                // your own theme, starting from the one you're in, then the editor to colour it
                let name = crate::theme::free_name(arg.strip_prefix("new").unwrap_or("").trim().trim_matches('"'));
                let name = if name == "mine" && !arg.contains(' ') { crate::theme::free_name("my-theme") } else { name };
                let mut th = cx.theme.clone();
                let base = if crate::theme::is_custom(&th.name) { crate::theme::base_of(&th.name) } else { th.name.clone() };
                th.name = name.clone();
                match crate::theme::save_custom(&name, &base, &th) {
                    Ok(path) => {
                        self.info.push(format!("made your theme {name} ({}): colour it in the themes app", path.display()));
                        cx.act(Action::ApplyTheme(name));
                        cx.act(Action::GotoApp("themes"));
                    }
                    Err(e) => self.info.push(format!("couldn't make the theme: {e}")),
                }
            }
            "/theme" if !arg.is_empty() => cx.act(Action::SetTheme(arg)),
            "/theme" => cx.act(Action::Palette("theme ".into())),
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
                        let who = if m.role == "user" { "you".to_string() } else { providers::label(m.model.as_deref().unwrap_or("ai")).to_lowercase() };
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
                self.info.push(format!("  works in {} · permissions {} · effort {}", self.workdir().display(), self.perms, if self.effort.is_empty() { "default" } else { &self.effort }));
                self.info.push(format!("  {} saved chats · config: {}", self.chats.len(), crate::config::path().display()));
            }
            "/delete" => self.confirm_delete = true,
            "/help" => {
                self.info.extend(
                    [
                        "enter send · esc stop · ctrl+r regenerate · ctrl+n new chat · ctrl+d delete chat",
                        "pgup/pgdn or the wheel scroll · ↑ in an empty box recalls your last message",
                        "ctrl+o shows every tool call in full (diffs, output) · click a tool line to open just that one",
                        "/perms ask: Claude Code asks first · y allow · n deny · a always allow that tool",
                        "F1-F9 apps · F12 play/pause · alt p palette · alt n terminal beside this",
                        "F10 opens the full guide: every app, lead mode, troubleshooting",
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

    /// The models the /model menu offers for an AI (the current one first, marked).
    fn models_for(&self, p: &str) -> Vec<(String, String)> {
        let s = |v: &[(&str, &str)]| v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect::<Vec<_>>();
        let mut v = match p {
            "claude" => s(&[
                ("default", "Claude Code's default"),
                ("opus", "most capable (always the newest Opus)"),
                ("sonnet", "balanced speed and smarts"),
                ("haiku", "fastest and cheapest"),
                ("claude-opus-5-5", "Opus 5.5"),
                ("claude-sonnet-5", "Sonnet 5"),
                ("claude-haiku-4-5", "Haiku 4.5"),
                ("claude-fable-5-1", "Fable 5.1"),
            ]),
            "codex" => s(&[
                ("default", "Codex's default"),
                ("gpt-5.6-sol", "most capable"),
                ("gpt-5.6-terra", "balanced"),
                ("gpt-5.6-luna", "fast and cheap"),
                ("gpt-5.3-codex", "older coding model"),
            ]),
            "anthropic" => s(&[
                ("claude-sonnet-5", "Sonnet 5 — balanced"),
                ("claude-opus-5-5", "Opus 5.5 — most capable"),
                ("claude-haiku-4-5", "Haiku 4.5 — fastest"),
                ("claude-fable-5-1", "Fable 5.1"),
            ]),
            "openai" => s(&[("gpt-5.6-terra", "balanced"), ("gpt-5.6-sol", "most capable"), ("gpt-5.6-luna", "fast and cheap")]),
            "ollama" => {
                let got = self.ollama_models.lock().unwrap().clone();
                if got.is_empty() {
                    s(&[("llama3.2", "pull models with `ollama pull <name>`")])
                } else {
                    got.into_iter().map(|m| (m, "installed in Ollama".to_string())).collect()
                }
            }
            _ => vec![],
        };
        if let Some(cur) = &self.chat.model {
            if let Some(i) = v.iter().position(|(m, _)| m == cur) {
                let (m, w) = v.remove(i);
                v.insert(0, (m, format!("{w} · current")));
            } else {
                v.insert(0, (cur.clone(), "current".into()));
            }
        }
        v
    }

    /// Choices for a command's argument (what gets listed after `/model `, `/theme `...).
    fn arg_options(&self, cmd: &str) -> Vec<(String, String)> {
        let pairs = |v: &[(&str, &str)]| v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect::<Vec<_>>();
        match cmd {
            "/provider" => providers::PROVIDERS
                .iter()
                .map(|(id, label, what)| (id.to_string(), if self.avail.contains(id) { format!("{label} — {what}") } else { format!("{label} — not set up here") }))
                .collect(),
            "/model" => self.models_for(&self.provider_of()),
            "/perms" => PERMS.iter().map(|(k, w)| (k.to_string(), w.to_string())).collect(),
            "/effort" => EFFORTS.iter().map(|(k, w)| (k.to_string(), w.to_string())).collect(),
            "/key" => pairs(&[("openai", "OpenAI-compatible key"), ("anthropic", "Anthropic API key")]),
            "/theme" => {
                let mut v: Vec<(String, String)> = vec![("edit".into(), "open the theme editor".into()), ("new ".into(), "make your own theme from this one".into())];
                v.extend(crate::theme::names().into_iter().map(|n| {
                    let what = if n == "omarchy" {
                        "follows your Omarchy theme".into()
                    } else if n == "terminal" {
                        "your terminal's own colours".into()
                    } else if crate::theme::is_custom(&n) {
                        "yours".into()
                    } else {
                        String::new()
                    };
                    (n, what)
                }));
                v
            }
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
    /// A message's lines, plus (line index, call id) for each tool line in it.
    fn msg_lines(&mut self, i: usize, width: usize, cx: &Cx) -> (Vec<Line<'static>>, Vec<(usize, String)>) {
        let t = cx.theme;
        let m = &self.chat.messages[i];
        let streaming_last = self.stream.is_some() && i + 1 == self.chat.messages.len();
        let key = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (m.content.len(), m.note.as_deref().unwrap_or(""), m.steps.len(), m.parts.len(), width, streaming_last, t.name.as_str()).hash(&mut h);
            h.finish()
        };
        if !streaming_last {
            if let Some((k, lines, hits)) = self.cache.get(&i) {
                if *k == key {
                    return (lines.clone(), hits.clone());
                }
            }
        }
        let mut out: Vec<Line<'static>> = vec![];
        let mut hits: Vec<(usize, String)> = vec![];
        let agent_chat = matches!(self.provider_of().as_str(), "claude" | "codex");
        if m.role == "user" && agent_chat {
            // like the Claude Code TUI: the prompt on a full-width grey band, "❯ " in front
            let base = match t.bg {
                ratatui::style::Color::Rgb(..) => t.bg,
                _ => ratatui::style::Color::Rgb(14, 14, 17),
            };
            let band = crate::theme::mix(base, ratatui::style::Color::Rgb(205, 205, 212), 0.13);
            let body = md::wrap(vec![Span::raw(m.content.clone())], width.saturating_sub(3), "", "");
            for (n, l) in body.into_iter().enumerate() {
                let w = l.width();
                let mut spans = vec![Span::styled(if n == 0 { "❯ " } else { "  " }, Style::default().fg(t.muted).bg(band))];
                spans.extend(l.spans.into_iter().map(|s| Span::styled(s.content.to_string(), Style::default().fg(t.fg).bg(band))));
                spans.push(Span::styled(" ".repeat(width.saturating_sub(w + 3)), Style::default().bg(band)));
                out.push(Line::from(spans));
            }
        } else if m.role == "user" {
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
            let who = m.model.clone().unwrap_or_default();
            // agent chats read like the Claude Code TUI: no name above each reply, unless another AI wrote it
            if !(agent_chat && who == self.provider_of()) {
                out.push(Line::from(vec![
                    Span::styled(ui::lead("ai"), Style::default().fg(t.accent)),
                    Span::styled(providers::label(&who).to_lowercase(), Style::default().fg(t.accent).add_modifier(Modifier::BOLD)),
                ]));
                out.push(Line::raw(""));
            }
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
            if !m.parts.is_empty() {
                let view = activity::View { expanded: self.expanded, open: &self.open, live: streaming_last, time: cx.time };
                activity::render(&m.parts, width, t, &view, &mut out, &mut hits);
            } else if !m.content.is_empty() {
                out.extend(md::render(&m.content, width.saturating_sub(1), "  ", t));
            }
            if let Some(n) = &m.note {
                out.push(Line::raw(""));
                let first = n.split(" · ").next().unwrap_or("");
                let timed = !m.parts.is_empty() && first.chars().next().is_some_and(|c| c.is_ascii_digit()) && first.ends_with(['s', 'm']);
                if timed {
                    // Claude Code's sign-off, with a word of its own: "✻ Sautéed for 19s"
                    const DONE: &[&str] = &["Worked", "Cooked", "Brewed", "Sautéed", "Crunched", "Baked", "Churned", "Simmered"];
                    let verb = DONE[(i * 7 + m.content.len()) % DONE.len()];
                    let rest: Vec<&str> = n.split(" · ").skip(1).filter(|x| !matches!(*x, "claude code" | "codex")).collect();
                    let tail = if rest.is_empty() { String::new() } else { format!(" · {}", rest.join(" · ")) };
                    out.push(Line::from(vec![
                        Span::styled("✻ ", Style::default().fg(t.accent)),
                        Span::styled(format!("{verb} for {first}{tail}"), Style::default().fg(t.muted)),
                    ]));
                } else {
                    out.extend(md::wrap(vec![Span::styled(n.clone(), Style::default().fg(t.muted))], width.saturating_sub(1), "  ", "  "));
                }
            }
        }
        out.push(Line::raw(""));
        if !streaming_last {
            self.cache.insert(i, (key, out.clone(), hits.clone()));
        }
        (out, hits)
    }

    /// Pinned above the composer while a reply streams: a pending approval, the live status line
    /// ("⠹ Editing src/app.rs… (12s · ↓ 1.2k tokens · esc to stop)") and the agent's todo progress.
    fn pinned_lines(&self, width: usize, cx: &Cx) -> Vec<Line<'static>> {
        let t = cx.theme;
        let mut out: Vec<Line<'static>> = vec![];
        let Some(s) = &self.stream else { return out };
        let muted = ui::muted(t);
        let m = self.chat.messages.last();
        let parts: &[store::Part] = m.map(|m| m.parts.as_slice()).unwrap_or(&[]);
        out.push(Line::raw(""));
        // ---- a question for you
        if let Some(q) = self.questions.front() {
            if let Some(cur) = q.qs.get(self.qs.idx) {
                let bar = Span::styled("  ▌ ", Style::default().fg(t.shine));
                let mut head = vec![bar.clone()];
                if !cur.header.is_empty() {
                    head.push(Span::styled(format!("{}  ", cur.header), Style::default().fg(t.shine).add_modifier(Modifier::BOLD)));
                }
                if q.qs.len() > 1 {
                    head.push(Span::styled(format!("{} of {}", self.qs.idx + 1, q.qs.len()), muted));
                }
                out.push(Line::from(head));
                for l in md::wrap(vec![Span::styled(cur.question.clone(), Style::default().add_modifier(Modifier::BOLD))], width.saturating_sub(6), "", "") {
                    let mut sp = vec![bar.clone()];
                    sp.extend(l.spans);
                    out.push(Line::from(sp));
                }
                let lw = cur.options.iter().map(|o| o.0.width()).max().unwrap_or(0).max(7).min(28);
                let n = cur.options.len();
                for (i, (label, what)) in cur.options.iter().chain(std::iter::once(&("Other…".to_string(), "type your own answer".to_string()))).enumerate() {
                    let on = i == self.qs.sel;
                    let tick = if cur.multi && i < n { if self.qs.ticked.get(i).copied().unwrap_or(false) { "[x] " } else { "[ ] " } } else { "" };
                    let st = if on { Style::default().fg(t.accent).add_modifier(Modifier::BOLD) } else { Style::default() };
                    let lab = format!("{tick}{label}");
                    out.push(Line::from(vec![
                        bar.clone(),
                        Span::styled(if on { " ❯ " } else { "   " }, st),
                        Span::styled(format!("{}. ", i + 1), muted),
                        Span::styled(format!("{lab:<w$}", w = lw + tick.len()), st),
                        Span::styled(format!("  {}", ui::fit(what, width.saturating_sub(lw + 16))), muted),
                    ]));
                }
                if let Some(text) = &self.qs.other {
                    out.push(Line::from(vec![bar.clone(), Span::styled("your answer › ", ui::bold_accent(t)), Span::raw(text.clone()), Span::styled("▏", ui::accent(t))]));
                    out.push(Line::from(vec![bar, Span::styled("enter", ui::bold_accent(t)), Span::styled(" send it  ", muted), Span::styled("esc", ui::bold_accent(t)), Span::styled(" back to the choices", muted)]));
                } else {
                    let mut keys = vec![bar, Span::styled("↑↓", ui::bold_accent(t)), Span::styled(" or ", muted), Span::styled("1-9", ui::bold_accent(t)), Span::styled(" choose  ", muted)];
                    if cur.multi {
                        keys.extend([Span::styled("space", ui::bold_accent(t)), Span::styled(" tick  ", muted)]);
                    }
                    keys.extend([Span::styled("enter", ui::bold_accent(t)), Span::styled(" answer  ", muted), Span::styled("type", ui::bold_accent(t)), Span::styled(" your own  ", muted), Span::styled("esc", ui::bold_accent(t)), Span::styled(" skip", muted)]);
                    out.push(Line::from(keys));
                }
                out.push(Line::raw(""));
            }
        }
        // ---- an approval prompt
        if let Some(a) = self.asks.front() {
            let bar = Span::styled("  ▌ ", Style::default().fg(t.shine));
            let mut head = vec![
                bar.clone(),
                Span::styled("allow ", Style::default().fg(t.shine).add_modifier(Modifier::BOLD)),
                Span::styled(a.label.clone(), Style::default().fg(t.accent).add_modifier(Modifier::BOLD)),
            ];
            if !a.target.is_empty() {
                head.push(Span::raw(" "));
                head.push(Span::styled(ui::fit(&a.target, width.saturating_sub(20 + a.label.len())), Style::default().fg(t.fg)));
            }
            head.push(Span::styled(" ?", Style::default().fg(t.shine).add_modifier(Modifier::BOLD)));
            if self.asks.len() > 1 {
                head.push(Span::styled(format!("  (+{} more)", self.asks.len() - 1), muted));
            }
            out.push(Line::from(head));
            let dup = a.body.len() == 1 && agent::split_line(&a.body[0]).2 == a.target;
            let show = if dup { 0 } else { a.body.len().min(6) };
            for l in &a.body[..show] {
                let (k, _, text) = agent::split_line(l);
                let style = match k {
                    '+' => Style::default().fg(t.good),
                    '-' => Style::default().fg(t.danger),
                    '>' => Style::default().fg(t.fg),
                    _ => muted,
                };
                let mark = if matches!(k, '+' | '-') { format!("{k} ") } else { String::new() };
                out.push(Line::from(vec![bar.clone(), Span::styled(format!("  {mark}{}", ui::fit(text, width.saturating_sub(10))), style)]));
            }
            if a.body.len() > show && !dup {
                out.push(Line::from(vec![bar.clone(), Span::styled(format!("  … {} more lines", a.body.len() - show), muted)]));
            }
            out.push(Line::from(vec![
                bar,
                Span::styled("y", ui::bold_accent(t)),
                Span::styled(" allow  ", muted),
                Span::styled("n", ui::bold_accent(t)),
                Span::styled(" deny  ", muted),
                Span::styled("a", ui::bold_accent(t)),
                Span::styled(format!(" always allow {} in this chat  ", a.label), muted),
                Span::styled("esc", ui::bold_accent(t)),
                Span::styled(" stop", muted),
            ]));
            out.push(Line::raw(""));
        }
        // ---- the status line
        let e = s.started.elapsed();
        // not ✳: Windows draws that one as a colour emoji two cells wide
        const STAR: &[&str] = &["·", "✢", "✶", "✻", "✽", "✻", "✶", "✢"];
        let spin = STAR[(e.as_millis() / 120) as usize % STAR.len()];
        let action = if !self.questions.is_empty() {
            "asking you".to_string()
        } else if !self.asks.is_empty() {
            "waiting for you".to_string()
        } else if s.status.starts_with("compacting") || s.status.starts_with("API hiccup") {
            s.status.clone()
        } else if let Some(a) = activity::current_action(parts) {
            a
        } else if m.map(|m| !m.content.is_empty()).unwrap_or(false) {
            "writing".into()
        } else {
            VERBS[(e.as_secs() / 3) as usize % VERBS.len()].to_string()
        };
        let mut meta = vec![agent::human_ms(e.as_millis() as u64 / 1000 * 1000)];
        if s.tokens > 0 {
            meta.push(format!("↓ {} tokens", agent::human_tokens(s.tokens)));
        }
        if !s.status.is_empty() && parts.is_empty() {
            meta.push(s.status.clone());
        }
        meta.push("esc to interrupt".into());
        let act = ui::fit(&format!("{}…", activity::capitalize(&action)), width.saturating_sub(40).max(20));
        out.push(Line::from(vec![
            Span::styled(format!("  {spin} "), ui::accent(t)),
            Span::styled(act, Style::default().fg(t.shine).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" ({})", meta.join(" · ")), muted),
        ]));
        // ---- todo progress: "3/7 done · now: writing tests" and the items around the current one
        if let Some(items) = activity::todos(parts).filter(|v| !v.is_empty()) {
            let done = items.iter().filter(|x| x.status == "completed").count();
            let cur = items.iter().position(|x| x.status == "in_progress");
            let mut line = vec![Span::styled("    └  ", muted), Span::styled(format!("{done}/{} done", items.len()), Style::default().fg(t.good))];
            if let Some(c) = cur {
                let it = &items[c];
                let now = if it.active.is_empty() { &it.text } else { &it.active };
                line.push(Span::styled(" · now: ", muted));
                line.push(Span::styled(ui::fit(now, width.saturating_sub(30)), Style::default().fg(t.fg)));
            }
            out.push(Line::from(line));
            let keep = 5.min(items.len());
            let first = cur.unwrap_or(done.min(items.len() - 1)).saturating_sub(1).min(items.len() - keep);
            for it in &items[first..first + keep] {
                out.push(activity::todo_row(it, "       ", width, t));
            }
        }
        // ---- what you queued
        if !self.queue.is_empty() {
            out.push(Line::raw(""));
            let who = providers::label(&self.provider_of()).to_lowercase();
            for q in &self.queue {
                let tag = if q.sent { format!("  queued · {who} reads it after its current step") } else { "  queued · sends when this reply ends".to_string() };
                let first = q.text.lines().next().unwrap_or("").to_string();
                let more = if q.text.lines().count() > 1 { " …" } else { "" };
                out.push(Line::from(vec![
                    Span::styled("  ❯ ", Style::default().fg(t.user).add_modifier(Modifier::BOLD)),
                    Span::styled(ui::fit(&format!("{first}{more}"), width.saturating_sub(tag.width() + 6).max(12)), Style::default().fg(t.fg)),
                    Span::styled(tag, muted),
                ]));
            }
            let mut keys = vec![Span::raw("    "), Span::styled("ctrl+x s", ui::bold_accent(t)), Span::styled(" send now (stops this reply)", muted)];
            if self.queue.iter().any(|q| !q.sent) {
                keys.extend([Span::styled("  ↑", ui::bold_accent(t)), Span::styled(" edit", muted)]);
            }
            keys.extend([Span::styled("  esc", ui::bold_accent(t)), Span::styled(" stop (keeps what you typed)", muted)]);
            out.push(Line::from(keys));
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
                ui::big_logo(f, &logo, area.x + (area.width - lw) / 2, y0, t, cx.time);
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
            _ => "oriel",
        };
        let logo = crate::font::render(word);
        let lw = logo.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
        let info = match p.as_str() {
            "claude" | "codex" => format!("{} · {} · works in {} ({})", providers::label(&p).to_lowercase(), self.chat.model.clone().unwrap_or("default".into()), self.workdir().display(), self.perms),
            _ => format!("{} · {}", providers::label(&p).to_lowercase(), self.chat.model.clone().unwrap_or("default".into())),
        };
        let block_h = 8 + 2 + 2 + 1 + SUGGESTIONS.len() as u16;
        let y0 = area.y + area.height.saturating_sub(block_h) / 2;
        let mut y = y0;
        if area.width > lw + 2 && area.height >= block_h {
            ui::big_logo(f, &logo, area.x + (area.width - lw) / 2, y, t, cx.time);
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
            "claude" | "codex" => s.push_str(&format!(" · {} · {}", self.chat.model.clone().unwrap_or("default".into()), ui::fit(&self.workdir().to_string_lossy(), 40))),
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
        let mut failed = false;
        let mut denied_hint: Option<u32> = None;
        for ev in evs {
            let Some(m) = self.chat.messages.last_mut() else { break };
            match ev {
                Ev::Token(t) => activity::push_text(m, &t),
                Ev::Thinking { text, tokens } => activity::push_thinking(m, &text, tokens),
                Ev::Tool(tool) => activity::upsert_tool(m, tool),
                Ev::Todos(items) => activity::set_todos(m, items),
                Ev::Usage(n) => s.tokens = n,
                Ev::Steered(text) => {
                    if let Some(i) = self.queue.iter().position(|q| q.text.trim() == text.trim()) {
                        self.queue.remove(i);
                    }
                    activity::push_user(m, &text);
                }
                Ev::Mark(text) => activity::push_mark(m, &text),
                Ev::Status(st) => s.status = st,
                Ev::Question(q) => {
                    if self.questions.is_empty() {
                        self.qs = QState::default();
                    }
                    self.questions.push_back(q);
                    self.scroll = 0;
                }
                Ev::Ask(a) => {
                    if self.always.contains(&a.tool) {
                        let _ = a.reply.send(approve::Decision::Allow);
                    } else {
                        self.asks.push_back(a);
                    }
                }
                Ev::State(k, v) => {
                    let (prov, field) = k.split_once('.').unwrap_or((k.as_str(), "session"));
                    let entry = self.chat.state.entry(prov.to_string()).or_insert_with(|| serde_json::json!({}));
                    if let Some(o) = entry.as_object_mut() {
                        o.insert(field.to_string(), serde_json::Value::String(v));
                    }
                }
                Ev::Done { note } => {
                    activity::settle(&mut m.parts, "done");
                    if let Some(n) = note.filter(|n| !n.is_empty()) {
                        // "… · 4 denied · …": say how to allow them
                        if let Some(d) = n.split(" · ").find_map(|x| x.strip_suffix(" denied").and_then(|d| d.trim().parse::<u32>().ok())) {
                            denied_hint = Some(d);
                        }
                        m.note = Some(n);
                    }
                    finished = true;
                }
                Ev::Error(e) => {
                    failed = true;
                    activity::settle(&mut m.parts, "stopped");
                    if m.content.trim().is_empty() && m.parts.is_empty() {
                        m.content = format!("⚠ {e}");
                    } else {
                        m.note = Some(format!("⚠ {e}"));
                    }
                    finished = true;
                }
            }
        }
        if let Some(d) = denied_hint {
            self.info.push(format!(
                "{d} action{} blocked by the permission mode ({}). /perms ask to approve each one, /perms bypass to allow everything — saved for every chat.",
                if d == 1 { " was" } else { "s were" },
                self.perms
            ));
        }
        if finished {
            self.stream = None;
            self.asks.clear();
            self.questions.clear();
            self.persist();
            // anything queued that the agent didn't take mid-reply is the next message
            if failed {
                self.unqueue_to_box("the reply failed");
            } else if !self.queue.is_empty() {
                let text = self.queue.drain(..).map(|q| q.text).collect::<Vec<_>>().join("\n\n");
                self.send(text, cx);
            }
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        let has_activity = self.chat.messages.iter().any(|m| !m.parts.is_empty());
        let expand_hint = if self.expanded { "collapse" } else { "expand tools" };
        let hints: Vec<(&str, &str)> = if self.confirm_delete {
            vec![("y", "delete this chat"), ("esc", "keep it")]
        } else if !self.asks.is_empty() {
            vec![("y", "allow"), ("n", "deny"), ("a", "always allow"), ("esc", "stop")]
        } else if self.chord_x {
            vec![("s", "send now: stop this reply and send what you queued"), ("any other key", "cancel")]
        } else if self.stream.is_some() {
            let mut v = vec![("esc", "stop"), ("enter", "queue"), ("ctrl+x s", "send now")];
            if has_activity {
                v.push(("ctrl+o", expand_hint));
            }
            v.extend([("F10", "help"), ("alt p", "palette")]);
            v
        } else {
            let mut v = vec![("enter", "send"), ("ctrl+r", "regenerate"), ("ctrl+n", "new chat"), ("/", "commands")];
            if has_activity {
                v.push(("ctrl+o", expand_hint));
            }
            v.extend([("F10", "help"), ("alt p", "palette")]);
            v
        };
        let area = ui::hint_line(f, area, &hints, t);
        if area.height < 5 {
            return;
        }
        let comp = Rect { y: area.bottom() - 3, height: 3, ..area };
        let above = Rect { height: area.height - 3, ..area };
        let above = Rect { x: above.x + 1, width: above.width.saturating_sub(2), ..above };
        // what's pinned above the composer while an agent works: an approval prompt, the status line, the todos
        let mut pinned = self.pinned_lines(above.width as usize, cx);
        pinned.truncate((above.height as usize).saturating_sub(3));
        let ph = pinned.len() as u16;
        let body = Rect { height: above.height - ph, ..above };
        if ph > 0 {
            f.render_widget(Paragraph::new(pinned), Rect { y: body.bottom(), height: ph, ..above });
        }
        self.tool_hits.clear();

        // ---- messages (or the hero on a new chat)
        if self.chat.messages.is_empty() && self.info.is_empty() {
            self.draw_hero(f, body, cx);
        } else {
            let width = body.width as usize;
            let mut lines: Vec<Line<'static>> = vec![Line::raw("")];
            let mut hits: Vec<(usize, String)> = vec![];
            for i in 0..self.chat.messages.len() {
                let (l, h) = self.msg_lines(i, width, cx);
                let off = lines.len();
                hits.extend(h.into_iter().map(|(n, id)| (n + off, id)));
                lines.extend(l);
            }
            for l in &self.info {
                lines.extend(md::wrap(vec![Span::styled(l.clone(), ui::muted(t))], width, "  ", "    "));
            }
            let h = body.height as usize;
            let max_scroll = lines.len().saturating_sub(h);
            self.scroll = self.scroll.min(max_scroll);
            let start = max_scroll - self.scroll;
            self.tool_hits = hits.into_iter().filter(|(n, _)| *n >= start && *n < start + h).map(|(n, id)| (body.y + (n - start) as u16, id)).collect();
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
        let border = if cx.focused { crate::theme::mix(t.accent, ratatui::style::Color::Rgb(96, 96, 104), 0.3) } else { t.frame };
        let block = ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(border));
        let inner = block.inner(comp);
        f.render_widget(block, comp);
        if t.animated && cx.focused {
            // ultra: a rainbow drifting round the input box
            let (w, h) = (comp.width as usize, comp.height as usize);
            let per = (2 * (w + h)).max(1) as f64;
            let mut ring: Vec<(u16, u16)> = (0..comp.width).map(|x| (comp.x + x, comp.y)).collect();
            ring.extend((1..comp.height).map(|y| (comp.right() - 1, comp.y + y)));
            ring.extend((0..comp.width.saturating_sub(1)).rev().map(|x| (comp.x + x, comp.bottom() - 1)));
            ring.extend((1..comp.height.saturating_sub(1)).rev().map(|y| (comp.x, comp.y + y)));
            let buf = f.buffer_mut();
            for (i, (x, y)) in ring.into_iter().enumerate() {
                if let Some(c) = buf.cell_mut(Position { x, y }) {
                    c.set_fg(crate::theme::rainbow_at(i as f64 / per - cx.time * 0.12, 0.55));
                }
            }
        }
        if matches!(self.provider_of().as_str(), "claude" | "codex") && comp.width > 70 && !self.effort.is_empty() {
            // the effort, bottom-left
            let color = match self.effort.as_str() {
                "ultracode" => t.shine,
                "max" | "xhigh" => t.accent,
                _ => t.muted,
            };
            let label = Line::from(vec![Span::raw(" "), Span::styled(format!("◆ {} effort", self.effort), Style::default().fg(color).add_modifier(Modifier::BOLD)), Span::styled(" /effort ", Style::default().fg(t.muted))]);
            let lw = label.width() as u16;
            f.render_widget(Paragraph::new(label), Rect { x: comp.x + 2, y: comp.bottom() - 1, width: lw, height: 1 });
        }
        if matches!(self.provider_of().as_str(), "claude" | "codex") && comp.width > 40 {
            // the permission mode, on the box's bottom edge
            let (glyph, words, color) = perm_badge(&self.perms, t);
            let label = Line::from(vec![
                Span::raw(" "),
                Span::styled(format!("{glyph} {words}"), Style::default().fg(color).add_modifier(Modifier::BOLD)),
                Span::styled(" (shift+tab to cycle) ", Style::default().fg(t.muted)),
            ]);
            let lw = label.width() as u16;
            if lw + 4 < comp.width {
                f.render_widget(Paragraph::new(label), Rect { x: comp.right() - lw - 2, y: comp.bottom() - 1, width: lw, height: 1 });
            }
        }
        let room = inner.width.saturating_sub(3) as usize;
        let shown: String = self.input.replace('\n', "⏎");
        let before: String = shown.chars().take(self.cursor).collect();
        let bw = before.width();
        let skip = bw.saturating_sub(room.saturating_sub(1));
        let (text, style) = if self.input.is_empty() {
            let name = providers::label(&self.provider_of()).to_lowercase();
            if !self.questions.is_empty() {
                (format!("{name} is asking you something above: choose, or just type your own answer"), ui::muted(t))
            } else if self.stream.is_some() && self.stream.as_ref().is_some_and(|s| s.steer.is_some()) {
                (format!("type to queue a message: {name} reads it after its current step"), ui::muted(t))
            } else if self.stream.is_some() {
                ("type to queue a message: it sends when this reply ends".to_string(), ui::muted(t))
            } else {
                (format!("message {name}…   (/ for commands · /model to switch)"), ui::muted(t))
            }
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
        // a question Claude is waiting on takes the keys first
        if !self.questions.is_empty() {
            self.question_key(k);
            return true;
        }
        // an approval Claude Code is waiting on takes y / n / a first
        if let Some(a) = self.asks.front() {
            let d = match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Some(approve::Decision::Allow),
                KeyCode::Char('n') | KeyCode::Char('N') if !ctrl => Some(approve::Decision::Deny),
                KeyCode::Char('a') | KeyCode::Char('A') if !ctrl => {
                    self.always.insert(a.tool.clone());
                    Some(approve::Decision::Allow)
                }
                _ => None,
            };
            if let Some(d) = d {
                if let Some(a) = self.asks.pop_front() {
                    let _ = a.reply.send(d);
                }
                // "always" also answers the ones already queued for that tool
                if let Some(tool) = self.asks.front().map(|a| a.tool.clone()) {
                    if self.always.contains(&tool) {
                        while let Some(a) = self.asks.front() {
                            if !self.always.contains(&a.tool) {
                                break;
                            }
                            let _ = self.asks.pop_front().map(|a| a.reply.send(approve::Decision::Allow));
                        }
                    }
                }
                return true;
            }
        }
        if std::mem::take(&mut self.chord_x) && matches!(k.code, KeyCode::Char('s') | KeyCode::Char('S')) {
            if self.stream.is_some() {
                self.send_now(cx);
            } else {
                let text = self.input.trim().to_string();
                self.input.clear();
                self.cursor = 0;
                if !text.is_empty() {
                    self.send(text, cx);
                }
            }
            return true;
        }
        let items = self.menu();
        match k.code {
            KeyCode::Char('x') if ctrl => self.chord_x = true,
            // shift+tab: the next permission mode, remembered like /perms
            KeyCode::BackTab => {
                let i = PERM_CYCLE.iter().position(|p| *p == self.perms).map(|i| (i + 1) % PERM_CYCLE.len()).unwrap_or(0);
                self.perms = PERM_CYCLE[i].to_string();
                let mut c = crate::config::load();
                c.ai.perms = self.perms.clone();
                crate::config::save(&c);
            }
            KeyCode::Up if self.input.is_empty() && self.queue.iter().any(|q| !q.sent) => {
                let i = self.queue.iter().rposition(|q| !q.sent).unwrap_or(0);
                self.input = self.queue.remove(i).text;
                self.cursor = self.input.chars().count();
            }
            KeyCode::Char('o') if ctrl => {
                self.expanded = !self.expanded;
                self.open.clear();
                self.cache.clear();
            }
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
                // a tool line: open / close just that call
                if let Some((_, id)) = self.tool_hits.iter().find(|(y, _)| *y == ev.row).cloned() {
                    if !self.open.remove(&id) {
                        self.open.insert(id);
                    }
                    self.cache.clear();
                    return;
                }
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
        c.chat.messages.push(store::Msg { role: "user".into(), content: "what's a good name for a terminal app?".into(), ..Default::default() });
        c.chat.messages.push(store::Msg {
            role: "assistant".into(),
            content: "A few ideas:\n\n- **oriel** — a window that juts out\n- `nest`, but taken\n\n```rust\nfn main() { println!(\"hi\"); }\n```\nPick the one you like.".into(),
            model: Some("ollama".into()),
            note: Some("34 tokens · 171 tok/s · llama3.2".into()),
            ..Default::default()
        });
        c.chat.title = "names".into();
        println!("{}", k.render_html(&mut c, 130, 40, "target/snap/chat-msgs.html"));
        k.typ(&mut c, "/pro");
        println!("{}", k.render_html(&mut c, 130, 40, "target/snap/chat-menu.html"));
        k.key(&mut c, KeyCode::Enter); // completes "/provider " and lists the AIs
        assert_eq!(c.input, "/provider ");
        let s = k.render_html(&mut c, 130, 40, "target/snap/chat-menu-model.html");
        assert!(s.contains("claude") && s.contains("ollama"), "/provider choices missing");
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
    fn chat_list_order_stable_when_clicking() {
        let k = Kit::new();
        let mut c = Chat::new(&k.config);
        c.chats = (0..4)
            .map(|i| {
                let mut ch = store::Chat::new("claude");
                ch.id = format!("t{i}");
                ch.title = format!("chat {i}");
                ch.updated = 1000.0 - i as f64;
                ch.messages.push(store::Msg { role: "user".into(), content: format!("hi {i}"), ..Default::default() });
                ch
            })
            .collect();
        let order = |c: &Chat| c.chats.iter().map(|x| x.id.clone()).collect::<Vec<_>>();
        let before = order(&c);
        for i in [2, 0, 3, 1, 2] {
            c.open_chat(i);
            assert_eq!(c.chat.id, format!("t{i}"), "clicking row {i} opens that chat");
            assert_eq!(order(&c), before, "just opening chats must not reorder the list");
        }
    }

    #[test]
    fn chat_perms_saved_and_bypass() {
        let mut k = Kit::new();
        k.config.ai.perms = "bypass".into();
        let mut c = Chat::new(&k.config);
        assert_eq!(c.perms, "bypass", "a new chat starts with the saved mode");
        k.config.ai.perms = String::new();
        let mut c2 = Chat::new(&k.config);
        assert_eq!(c2.perms, "edits");
        // /perms: the menu lists Claude Code's modes, bypass included
        c2.input = "/perms ".into();
        c2.cursor = 7;
        let s = k.render(&mut c2, 130, 40);
        assert!(s.contains("bypass") && s.contains("plan") && s.contains("ask") && s.contains("edits"), "{s}");
        c2.input.clear();
        c2.cursor = 0;
        k.typ(&mut c2, "/perms full");
        k.key(&mut c2, KeyCode::Enter);
        assert_eq!(c2.perms, "bypass", "old name still works");
        assert_eq!(norm_perms("read"), Some("plan"));
        assert_eq!(norm_perms("nope"), None);
        // a reply with blocked tools says how to allow them
        c.provider = "claude".into();
        c.chat.provider = Some("claude".into());
        c.perms = "edits".into();
        c.chat.messages.push(store::Msg { role: "user".into(), content: "look".into(), ..Default::default() });
        c.chat.messages.push(store::Msg { role: "assistant".into(), model: Some("claude".into()), ..Default::default() });
        c.stream = Some(Stream { stop: Arc::new(AtomicBool::new(false)), inbox: Arc::new(Mutex::new(vec![Ev::Done { note: Some("20s · 880 tokens · 5 turns · 4 denied · claude code".into()) }])), status: String::new(), started: Instant::now(), tokens: 0, steer: None });
        k.poll(&mut c);
        assert!(c.info.iter().any(|l| l.contains("4 actions were blocked") && l.contains("/perms bypass")), "{:?}", c.info);
        store::delete(&c.chat.id);
    }

    #[test]
    fn chat_provider_and_model_commands() {
        let mut k = Kit::new();
        let mut c = Chat::new(&k.config);
        c.avail = vec!["claude", "codex"];
        c.provider = "claude".into();
        c.chat.provider = Some("claude".into());
        // /model lists the current AI's models
        c.input = "/model ".into();
        c.cursor = 7;
        let s = k.render(&mut c, 130, 40);
        assert!(s.contains("opus") && s.contains("sonnet") && s.contains("haiku"), "{s}");
        c.input.clear();
        c.cursor = 0;
        k.typ(&mut c, "/model opus");
        k.key(&mut c, KeyCode::Enter);
        assert_eq!(c.chat.model.as_deref(), Some("opus"));
        assert_eq!(c.models.get("claude").map(String::as_str), Some("opus"));
        // /provider switches the AI; its own remembered model comes back with it
        k.typ(&mut c, "/provider codex");
        k.key(&mut c, KeyCode::Enter);
        assert_eq!(c.provider_of(), "codex");
        assert_eq!(c.chat.model, None);
        c.input = "/model ".into();
        c.cursor = 7;
        assert!(k.render(&mut c, 130, 40).contains("gpt-5.6-terra"));
        c.input.clear();
        c.cursor = 0;
        k.typ(&mut c, "/provider claude");
        k.key(&mut c, KeyCode::Enter);
        assert_eq!(c.chat.model.as_deref(), Some("opus"), "claude's model remembered");
        c.new_chat();
        assert_eq!(c.chat.model.as_deref(), Some("opus"), "new chats use it too");
        // the old one-shot form still works
        k.typ(&mut c, "/model codex gpt-5.6-luna");
        k.key(&mut c, KeyCode::Enter);
        assert_eq!((c.provider_of().as_str(), c.chat.model.as_deref()), ("codex", Some("gpt-5.6-luna")));
    }

    /// enter while a reply runs queues the message; Claude Code gets it mid-reply and it lands inline, others get
    /// it when the reply ends; esc gives it back; ctrl+x s sends it now.
    #[test]
    fn chat_queue_messages() {
        let mut k = Kit::new();
        // ---- an AI that can't take messages mid-reply: held, sent as the next message
        let mut c = agent_chat(&k, "codex", "refactor the parser");
        let _forget = Forget(c.chat.id.clone());
        k.typ(&mut c, "also update the docs");
        k.key(&mut c, KeyCode::Enter);
        assert_eq!(c.queue.len(), 1);
        assert!(!c.queue[0].sent);
        assert!(c.input.is_empty());
        let s = k.render_html(&mut c, 130, 36, "target/snap/chat-queue-held.html");
        assert!(s.contains("also update the docs") && s.contains("sends when this reply ends") && s.contains("ctrl+x s"), "{s}");
        // up takes it back to edit, enter queues it again
        k.key(&mut c, KeyCode::Up);
        assert_eq!(c.input, "also update the docs");
        assert!(c.queue.is_empty());
        k.typ(&mut c, " and the README");
        k.key(&mut c, KeyCode::Enter);
        deliver(&mut k, &mut c, [Ev::Token("Done.".into()), Ev::Done { note: None }]);
        let n = c.chat.messages.len();
        assert_eq!(c.chat.messages[n - 2].content, "also update the docs and the README", "the queue became the next message");
        assert!(c.queue.is_empty());
        // (the provider refuses to run in tests: the new reply fails, which is fine here)
        c.stop();

        // ---- Claude Code: straight to the running agent, shown inline once it's read
        let mut c = agent_chat(&k, "claude", "fix the flaky test");
        let _forget2 = Forget(c.chat.id.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        c.stream.as_mut().unwrap().steer = Some(tx);
        deliver(&mut k, &mut c, [Ev::Tool(store::Tool { id: "t1".into(), name: "Bash".into(), label: "Bash".into(), target: "cargo test".into(), status: "running".into(), ..Default::default() })]);
        k.typ(&mut c, "use nextest instead");
        k.key(&mut c, KeyCode::Enter);
        assert_eq!(rx.try_recv().ok().as_deref(), Some("use nextest instead"), "handed to the agent right away");
        assert!(c.queue[0].sent);
        let s = k.render(&mut c, 130, 36);
        assert!(s.contains("claude code reads it after its current step"), "{s}");
        k.key(&mut c, KeyCode::Up); // can't take back what the agent already has: ↑ recalls history as usual
        assert!(c.input == "fix the flaky test" && c.queue.len() == 1, "{:?}", c.input);
        k.key_mod(&mut c, KeyCode::Char('u'), KeyModifiers::CONTROL);
        deliver(&mut k, &mut c, [Ev::Steered("use nextest instead".into()), Ev::Token("Switching to nextest.".into())]);
        assert!(c.queue.is_empty());
        let s = k.render_html(&mut c, 130, 36, "target/snap/chat-queue-steered.html");
        assert!(s.contains("❯ you  use nextest instead") && s.contains("Switching to nextest"), "{s}");
        let before = c.chat.messages.len();
        deliver(&mut k, &mut c, [Ev::Done { note: None }]);
        assert_eq!(c.chat.messages.len(), before, "nothing left to send");

        // ---- esc stops and gives the queue back; ctrl+x s sends now
        let mut c = agent_chat(&k, "codex", "write a parser");
        let _forget3 = Forget(c.chat.id.clone());
        k.typ(&mut c, "in rust please");
        k.key(&mut c, KeyCode::Enter);
        k.key(&mut c, KeyCode::Esc);
        assert!(c.stream.is_none());
        assert_eq!(c.input, "in rust please");
        let mut c = agent_chat(&k, "codex", "write a parser");
        let _forget4 = Forget(c.chat.id.clone());
        k.typ(&mut c, "in rust please");
        k.key(&mut c, KeyCode::Enter);
        k.key_mod(&mut c, KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert!(k.render(&mut c, 130, 36).contains("send now: stop this reply"));
        k.key(&mut c, KeyCode::Char('s'));
        let n = c.chat.messages.len();
        assert_eq!(c.chat.messages[n - 2].content, "in rust please");
        assert!(c.chat.messages[n - 3].note.as_deref().unwrap_or("").contains("stopped"), "the old reply was stopped");
        assert!(c.input.is_empty() && c.queue.is_empty());
        c.stop();
    }

    /// The same prompt in the real Claude Code TUI (run in a pty, off screen) and in oriel's chat, snapshotted side by
    /// side to see what oriel's transcript is missing. Uses your Claude: run it on purpose, with the scratch folder
    /// outside anything your hooks log.
    /// `ORIEL_COMPARE_DIR=<folder> cargo test chat_compare_tui -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn chat_compare_tui() {
        use crate::panes::term::Term;
        let base = PathBuf::from(std::env::var("ORIEL_COMPARE_DIR").expect("set ORIEL_COMPARE_DIR"));
        let model = std::env::var("ORIEL_COMPARE_MODEL").unwrap_or_else(|_| "sonnet".into());
        let js = std::env::var("ORIEL_COMPARE_SCENARIO").is_ok_and(|s| s == "js");
        let prompt = std::env::var("ORIEL_COMPARE_PROMPT").unwrap_or_else(|_| {
            if js {
                "Make a short todo list first, then: every function in calc.js should throw a TypeError when given a non-number, and divide() should throw a RangeError on division by zero. Add calc.test.js using node:test, and run it with node --test.".into()
            } else {
                "stats.py has two bugs: mean() crashes on an empty list (it should return None) and median() is wrong for even-length lists. Fix both, add test_stats.py with unittest tests for them, and run the tests.".into()
            }
        });
        let setup = |d: &std::path::Path| {
            let _ = std::fs::remove_dir_all(d);
            std::fs::create_dir_all(d).unwrap();
            if js {
                std::fs::write(d.join("calc.js"), "function add(a, b) {\n  return a + b;\n}\n\nfunction subtract(a, b) {\n  return a - b;\n}\n\nfunction multiply(a, b) {\n  return a * b;\n}\n\nfunction divide(a, b) {\n  return a / b;\n}\n\nmodule.exports = { add, subtract, multiply, divide };\n").unwrap();
                std::fs::write(d.join("package.json"), "{\n  \"name\": \"calc\",\n  \"version\": \"1.0.0\",\n  \"scripts\": { \"test\": \"node --test\" }\n}\n").unwrap();
            } else {
                std::fs::write(d.join("stats.py"), "def mean(xs):\n    return sum(xs) / len(xs)\n\n\ndef median(xs):\n    xs = sorted(xs)\n    return xs[len(xs) // 2]\n\n\ndef mode(xs):\n    return max(set(xs), key=xs.count)\n").unwrap();
            }
        };
        let only_oriel = std::env::var("ORIEL_COMPARE_ONLY").is_ok_and(|s| s == "oriel");
        let (tui_dir, oriel_dir) = (base.join("tui"), base.join("oriel"));
        setup(&tui_dir);
        setup(&oriel_dir);
        let (w, h) = (130u16, 140u16);
        let mut k = Kit::new();
        let dump = |name: &str, text: &str| {
            let t: Vec<&str> = text.lines().map(|l| l.trim_end()).collect();
            let last = t.iter().rposition(|l| !l.is_empty()).unwrap_or(0);
            std::fs::write(base.join(format!("{name}.txt")), t[..=last].join("\n")).unwrap();
        };

        // ---- the real thing, in a pty
        let mut cx_actions = vec![];
        if !only_oriel {
        let claude = crate::config::which("claude").expect("claude isn't installed");
        let args = vec!["--model".into(), model.clone(), "--permission-mode".into(), "bypassPermissions".into()];
        let mut term = Term::new("claude", "claude", &claude.to_string_lossy(), args, Some(tui_dir.clone()));
        let t0 = Instant::now();
        let mut ready = false;
        while t0.elapsed() < Duration::from_secs(60) {
            k.wait_wake(&mut term, 400);
            let s = k.render(&mut term, w, h);
            if s.contains("Yes, I accept") {
                k.key(&mut term, KeyCode::Down);
                k.key(&mut term, KeyCode::Enter);
            } else if s.contains("Yes, proceed") || s.contains("Yes, I trust") {
                k.key(&mut term, KeyCode::Enter);
            } else if s.contains("? for shortcuts") || s.contains("bypass permissions on") {
                ready = true;
                break;
            }
        }
        dump("tui-start", &k.render(&mut term, w, h));
        assert!(ready, "the TUI never came up: see tui-start.txt");
        {
            let mut cx = Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: &mut cx_actions, focused: true, time: 1.0 };
            term.paste(&prompt, &mut cx);
        }
        std::thread::sleep(Duration::from_millis(500));
        k.key(&mut term, KeyCode::Enter);
        let (t0, mut last, mut changed, mut shots) = (Instant::now(), String::new(), Instant::now(), vec![6u64, 14, 24]);
        while t0.elapsed() < Duration::from_secs(420) {
            k.wait_wake(&mut term, 300);
            let s = k.render(&mut term, w, h);
            if shots.first().is_some_and(|&at| t0.elapsed() >= Duration::from_secs(at)) {
                let at = shots.remove(0);
                k.render_html(&mut term, w, h, &base.join(format!("tui-mid{at}.html")).to_string_lossy());
                dump(&format!("tui-mid{at}"), &s);
            }
            // the spinner animates while it works, so a screen that holds still for 8s means it's done
            if s != last {
                last = s;
                changed = Instant::now();
            } else if t0.elapsed() > Duration::from_secs(10) && changed.elapsed() > Duration::from_secs(8) {
                break;
            }
        }
        let s = k.render_html(&mut term, w, h, &base.join("tui.html").to_string_lossy());
        dump("tui", &s);
        println!("TUI done in {:.0}s", t0.elapsed().as_secs_f64());
        {
            let mut cx = Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: &mut cx_actions, focused: true, time: 1.0 };
            term.paste("/exit", &mut cx);
        }
        k.key(&mut term, KeyCode::Enter);
        std::thread::sleep(Duration::from_secs(2));
        drop(term);
        }

        // ---- oriel's chat, same prompt, same model
        providers::LIVE.store(true, Ordering::SeqCst);
        let mut c = Chat::new(&k.config);
        let _forget = Forget(c.chat.id.clone());
        c.provider = "claude".into();
        c.chat.provider = Some("claude".into());
        c.chat.model = Some(model.clone());
        c.perms = "bypass".into();
        c.chat.cwd = Some(oriel_dir.to_string_lossy().to_string());
        {
            let mut cx = Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: &mut cx_actions, focused: true, time: 1.0 };
            c.send(prompt.clone(), &mut cx);
        }
        let (t0, mut shots) = (Instant::now(), vec![6u64, 14, 24]);
        while c.stream.is_some() && t0.elapsed() < Duration::from_secs(420) {
            k.wait_wake(&mut c, 300);
            if shots.first().is_some_and(|&at| t0.elapsed() >= Duration::from_secs(at)) {
                let at = shots.remove(0);
                let s = k.render_html(&mut c, w, h, &base.join(format!("oriel-mid{at}.html")).to_string_lossy());
                dump(&format!("oriel-mid{at}"), &s);
            }
        }
        let s = k.render_html(&mut c, w, h, &base.join("oriel.html").to_string_lossy());
        dump("oriel", &s);
        println!("oriel done in {:.0}s", t0.elapsed().as_secs_f64());
        std::fs::write(base.join("oriel-parts.json"), serde_json::to_string_pretty(&c.chat.messages).unwrap()).unwrap();
    }

    /// Re-draw the transcript chat_compare_tui saved, with the current renderer (no AI call).
    /// `ORIEL_COMPARE_DIR=<folder> cargo test chat_compare_redraw -- --ignored`
    #[test]
    #[ignore]
    fn chat_compare_redraw() {
        let base = PathBuf::from(std::env::var("ORIEL_COMPARE_DIR").expect("set ORIEL_COMPARE_DIR"));
        let msgs: Vec<store::Msg> = serde_json::from_str(&std::fs::read_to_string(base.join("oriel-parts.json")).unwrap()).unwrap();
        let mut k = Kit::new();
        let mut c = Chat::new(&k.config);
        c.provider = "claude".into();
        c.chat.provider = Some("claude".into());
        c.chat.model = Some("sonnet".into());
        c.chat.title = store::title_from(&msgs[0].content);
        c.chat.messages = msgs;
        k.render_html(&mut c, 130, 140, &base.join("oriel2.html").to_string_lossy());
    }

    /// Claude asking: the picker shows, numbers and arrows choose, several can be ticked, Other takes your words,
    /// and the answers go back as question -> answer.
    #[test]
    fn chat_questions() {
        let mut k = Kit::new();
        let mut c = agent_chat(&k, "claude", "quiz me on rust");
        let (tx, rx) = std::sync::mpsc::channel();
        let qs = vec![
            approve::Q { question: "What does `&mut` give you?".into(), header: "Quiz 1".into(), multi: false, options: vec![("A copy".into(), "".into()), ("A unique borrow".into(), "one writer at a time".into()), ("Ownership".into(), "".into())] },
            approve::Q { question: "Which are Copy?".into(), header: "Quiz 2".into(), multi: true, options: vec![("i32".into(), "".into()), ("String".into(), "".into()), ("bool".into(), "".into())] },
            approve::Q { question: "Your favourite crate?".into(), header: "Quiz 3".into(), multi: false, options: vec![("serde".into(), "".into()), ("tokio".into(), "".into())] },
        ];
        deliver(&mut k, &mut c, [Ev::Question(approve::Question { qs, reply: tx })]);
        let s = k.render_html(&mut c, 120, 36, "target/snap/chat-question.html");
        assert!(s.contains("Quiz 1") && s.contains("1 of 3") && s.contains("A unique borrow") && s.contains("Other…") && s.contains("Asking you"), "{s}");
        k.key(&mut c, KeyCode::Char('2')); // a number answers a single choice
        k.key(&mut c, KeyCode::Char(' ')); // tick i32
        k.key(&mut c, KeyCode::Down);
        k.key(&mut c, KeyCode::Down);
        k.key(&mut c, KeyCode::Char(' ')); // tick bool
        let s = k.render(&mut c, 120, 36);
        assert!(s.contains("[x] i32") && s.contains("[x] bool") && s.contains("space tick"), "{s}");
        k.key(&mut c, KeyCode::Enter);
        k.typ(&mut c, "anyhow"); // typing answers in your own words
        k.key(&mut c, KeyCode::Enter);
        let ans = rx.try_recv().unwrap().unwrap();
        assert_eq!(ans["What does `&mut` give you?"], "A unique borrow");
        assert_eq!(ans["Which are Copy?"], "i32, bool");
        assert_eq!(ans["Your favourite crate?"], "anyhow");
        assert!(c.questions.is_empty());
        // esc skips
        let (tx, rx) = std::sync::mpsc::channel();
        deliver(&mut k, &mut c, [Ev::Question(approve::Question { qs: vec![approve::Q { question: "Proceed?".into(), header: String::new(), multi: false, options: vec![("Yes".into(), "".into())] }], reply: tx })]);
        k.key(&mut c, KeyCode::Esc);
        assert_eq!(rx.try_recv().unwrap(), None);
        assert!(c.stream.is_some(), "esc skipped the question, it didn't stop the reply");
    }

    /// The input box shows the permission mode (shift+tab cycles it) and the effort.
    #[test]
    fn chat_mode_and_effort() {
        let mut k = Kit::new();
        let mut c = agent_chat(&k, "claude", "make it faster");
        c.stream = None;
        c.chat.messages.push(store::Msg { role: "assistant".into(), model: Some("claude".into()), content: "- **Where:** here
- **Mode:** bypass".into(), ..Default::default() });
        c.perms = "edits".into();
        k.key_mod(&mut c, KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(c.perms, "auto");
        k.key_mod(&mut c, KeyCode::BackTab, KeyModifiers::SHIFT);
        k.key_mod(&mut c, KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(c.perms, "bypass");
        k.typ(&mut c, "/effort high");
        k.key(&mut c, KeyCode::Enter);
        assert_eq!(c.effort, "high");
        for name in ["matrix", "ultra"] {
            k.theme = crate::theme::get(name);
            let s = k.render_html(&mut c, 120, 24, &format!("target/snap/chat-badges-{name}.html"));
            assert!(s.contains("bypass permissions on") && s.contains("shift+tab") && s.contains("high effort"), "{s}");
        }
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

    /// Deletes the test chat from the real chat list even if the test fails half way.
    struct Forget(String);
    impl Drop for Forget {
        fn drop(&mut self) {
            store::delete(&self.0);
        }
    }

    /// Run a captured CLI transcript through a parser, as the provider thread would (fake clock: 60 ms a line).
    fn parse_fixture(text: &str, mut feed: impl FnMut(&serde_json::Value, u64, &mut dyn FnMut(Ev)) -> Result<(), String>) -> Vec<Vec<Ev>> {
        text.lines()
            .enumerate()
            .map(|(i, l)| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                let mut evs = vec![];
                feed(&v, i as u64 * 60, &mut |e| evs.push(e)).unwrap();
                evs
            })
            .collect()
    }

    /// A chat mid-reply, as if `send` had started a stream.
    fn agent_chat(k: &Kit, provider: &str, prompt: &str) -> Chat {
        let mut c = Chat::new(&k.config);
        c.provider = provider.into();
        c.chat.provider = Some(provider.into());
        c.chat.title = store::title_from(prompt);
        c.chat.cwd = Some("C:\\work\\demo".into());
        c.chat.messages.push(store::Msg { role: "user".into(), content: prompt.into(), ..Default::default() });
        c.chat.messages.push(store::Msg { role: "assistant".into(), model: Some(provider.into()), ..Default::default() });
        // a steerable AI gets a pipe, as a real run would (its far end is gone, so nothing is sent anywhere)
        let steer = providers::steerable(provider).then(|| std::sync::mpsc::channel().0);
        c.stream = Some(Stream { stop: Arc::default(), inbox: Arc::default(), status: String::new(), started: Instant::now(), tokens: 0, steer });
        c
    }

    fn deliver(k: &mut Kit, c: &mut Chat, evs: impl IntoIterator<Item = Ev>) {
        if let Some(s) = &c.stream {
            s.inbox.lock().unwrap().extend(evs);
        }
        k.poll(c);
    }

    fn all_tools(parts: &[store::Part]) -> Vec<store::Tool> {
        fn walk(t: &store::Tool, out: &mut Vec<store::Tool>) {
            out.push(t.clone());
            t.children.iter().for_each(|c| walk(c, out));
        }
        let mut out = vec![];
        for p in parts {
            if let store::Part::Tool(t) = p {
                walk(t, &mut out);
            }
        }
        out
    }

    const CLAUDE_FIXTURE: &str = include_str!("chat/fixtures/claude-stream.jsonl");
    const CODEX_FIXTURE: &str = include_str!("chat/fixtures/codex-exec.jsonl");
    const PROMPT: &str = "track 5 steps with todos: create hello.txt (alpha, beta, gamma), change beta to BETA, cat it, grep for gamma, then have a subagent count its lines";

    /// README screenshot: a live Claude Code run in the dracula theme. `cargo test docs_agent -- --ignored`
    #[test]
    #[ignore]
    fn docs_agent_screenshot() {
        let mut k = Kit::new();
        k.theme = crate::theme::get("dracula");
        let mut c = agent_chat(&k, "claude", PROMPT);
        c.chat.cwd = Some(if cfg!(windows) { "C:\\work\\demo".into() } else { "/home/you/demo".into() });
        let _forget = Forget(c.chat.id.clone());
        let mut p = agent::Claude::new(std::path::Path::new("C:\\work\\demo"));
        let per_line = parse_fixture(CLAUDE_FIXTURE, |v, t, s| p.feed(v, t, s));
        let grep_at = CLAUDE_FIXTURE.lines().position(|l| l.contains(r#""name": "Grep""#) && l.contains(r#""type": "assistant""#)).unwrap();
        for evs in per_line.into_iter().take(grep_at + 1) {
            deliver(&mut k, &mut c, evs);
        }
        k.render_html(&mut c, 150, 44, "docs/screenshot-agent.html");
    }

    #[test]
    fn chat_agent_claude_fixture() {
        let mut k = Kit::new();
        let mut c = agent_chat(&k, "claude", PROMPT);
        let _forget = Forget(c.chat.id.clone());
        let mut p = agent::Claude::new(std::path::Path::new("C:\\work\\demo"));
        let per_line = parse_fixture(CLAUDE_FIXTURE, |v, t, s| p.feed(v, t, s));
        // live: stop part way, while the Bash call runs
        let bash_at = CLAUDE_FIXTURE.lines().position(|l| l.contains(r#""name": "Bash""#) && l.contains(r#""type": "assistant""#)).unwrap();
        let mut rest = per_line.into_iter();
        for evs in rest.by_ref().take(bash_at + 1) {
            deliver(&mut k, &mut c, evs);
        }
        let s = k.render_html(&mut c, 140, 44, "target/snap/chat-agent-live.html");
        println!("{s}");
        assert!(s.contains("Running cat hello.txt…"), "live status line");
        assert!(s.contains("2/5 done · now: Running cat command"), "pinned todo progress");
        // the rest, then the end of the reply
        for evs in rest {
            deliver(&mut k, &mut c, evs);
        }
        deliver(&mut k, &mut c, [Ev::Done { note: Some(p.note(Duration::from_millis(45_300))) }]);
        assert!(c.stream.is_none());
        let m = c.chat.messages.last().unwrap();
        let tools = all_tools(&m.parts);
        let names: Vec<String> = tools.iter().map(|t| format!("{} {} [{}]", t.label, t.target, t.status)).collect();
        println!("{names:#?}");
        // every call in order, todo plumbing hidden, the subagent's Read nested under it
        assert_eq!(
            names,
            ["Write hello.txt [done]", "Update hello.txt [done]", "Bash cat hello.txt [done]", "Grep 'gamma' in hello.txt [done]", "Agent Read hello.txt and count lines [done]", "Read hello.txt [done]"]
        );
        assert_eq!(tools[1].summary, "+1 -1");
        assert!(tools[1].body.iter().any(|l| l == "-2\tbeta") && tools[1].body.iter().any(|l| l == "+2\tBETA"), "{:?}", tools[1].body);
        assert_eq!(tools[2].body.len(), 3);
        assert_eq!(tools[3].summary, "1 match");
        assert_eq!(tools[5].parent.as_deref(), Some(tools[4].id.as_str()));
        let todos = activity::todos(&m.parts).unwrap();
        assert_eq!(todos.len(), 5);
        assert!(todos.iter().all(|t| t.status == "completed"), "{todos:?}");
        // text and calls interleave in the order they happened
        let kinds: String = m.parts.iter().map(|p| match p {
            store::Part::Text { .. } => 'T',
            store::Part::Tool(_) => 't',
            store::Part::Todos { .. } => 'L',
            store::Part::Thinking { .. } | store::Part::User { .. } | store::Part::Mark { .. } => '.',
        }).filter(|k| *k != '.').collect();
        println!("{kinds}");
        assert!(kinds.starts_with("TTL") || kinds.starts_with("TL") || kinds.contains("TtttttT"), "{kinds}");
        assert!(kinds.ends_with('T'));
        assert!(m.content.starts_with("I'll work through") && m.content.contains("All five steps completed"));
        let note = m.note.clone().unwrap();
        assert!(note.contains("4.3k tokens") && note.contains("$0.136") && note.contains("24 turns"), "{note}");
        let s = k.render_html(&mut c, 140, 44, "target/snap/chat-agent.html");
        println!("{s}");
        assert!(s.contains("Update(hello.txt)") && s.contains("⎿  Added 1 line, removed 1 line"), "Claude Code's wording");
        assert!(!s.contains("● TaskCreate") && !s.contains("● ToolSearch"), "todo plumbing stays hidden");
        // ctrl+o: everything in full
        k.key_mod(&mut c, KeyCode::Char('o'), KeyModifiers::CONTROL);
        c.scroll = 12;
        let s = k.render_html(&mut c, 140, 44, "target/snap/chat-agent-expanded.html");
        println!("{s}");
        // the saved file keeps the transcript and loads back
        let json = serde_json::to_string(&c.chat).unwrap();
        let back: store::Chat = serde_json::from_str(&json).unwrap();
        assert_eq!(back.messages[1].parts, c.chat.messages[1].parts);
    }

    #[test]
    fn chat_agent_click_and_errors() {
        let mut k = Kit::new();
        let mut c = agent_chat(&k, "claude", "fix the failing test");
        let _forget = Forget(c.chat.id.clone());
        let mut p = agent::Claude::new(std::path::Path::new("/home/me/proj"));
        // the older todo tool and a failing command, hand-written in stream-json's shape
        let lines = [
            serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Let me run the tests."},
                {"type": "tool_use", "id": "t1", "name": "TodoWrite", "input": {"todos": [
                    {"content": "Run the tests", "status": "in_progress", "activeForm": "Running the tests"},
                    {"content": "Fix the parser", "status": "pending", "activeForm": "Fixing the parser"}]}},
                {"type": "tool_use", "id": "t2", "name": "Bash", "input": {"command": "cargo test"}}]}}),
            serde_json::json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t2", "is_error": true,
                "content": "Exit code 101\nrunning 3 tests\ntest parse ... FAILED\nerror: test failed"}]}}),
            serde_json::json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t3", "name": "MultiEdit", "input": {
                "file_path": "/home/me/proj/src/parse.rs", "edits": [{"old_string": "let n = 0;", "new_string": "let n = 1;"}, {"old_string": "a\nb", "new_string": "a\nc\nb"}]}},
                {"type": "tool_use", "id": "t4", "name": "WebFetch", "input": {"url": "https://docs.rs/serde", "prompt": "x"}}]}}),
        ];
        for (i, l) in lines.iter().enumerate() {
            let mut evs = vec![];
            p.feed(l, i as u64 * 1500, &mut |e| evs.push(e)).unwrap();
            deliver(&mut k, &mut c, evs);
        }
        let s = k.render_html(&mut c, 120, 36, "target/snap/chat-agent-errors.html");
        println!("{s}");
        assert!(s.contains("Bash(cargo test)") && s.contains("⎿  Error: exit 101"));
        assert!(s.contains("error: test failed"));
        assert!(s.contains("Update(src/parse.rs)") && s.contains("Added 2 lines, removed 1 line"), "MultiEdit diff counts");
        assert!(s.contains("Fetch(https://docs.rs/serde)"));
        assert!(s.contains("0/2 done · now: Running the tests"));
        // clicking the Bash line opens it (all output), clicking again closes it
        let (row, _) = c.tool_hits.iter().find(|(_, id)| id == "t2").cloned().unwrap();
        let click = |row| MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 10, row, modifiers: KeyModifiers::NONE };
        k.mouse(&mut c, click(row), Rect::new(0, 0, 120, 36));
        assert!(c.open.contains("t2"));
        k.mouse(&mut c, click(row), Rect::new(0, 0, 120, 36));
        assert!(!c.open.contains("t2"));
        // esc: running calls end as stopped
        k.key(&mut c, KeyCode::Esc);
        assert!(all_tools(&c.chat.messages[1].parts).iter().all(|t| t.status != "running"));
    }

    #[test]
    fn chat_agent_codex_fixture() {
        let mut k = Kit::new();
        let mut c = agent_chat(&k, "codex", "make a short plan, create notes.txt (one, two, three), change two to TWO, print it");
        let _forget = Forget(c.chat.id.clone());
        let mut p = agent::Codex::new(std::path::Path::new("C:\\work\\demo"));
        for evs in parse_fixture(CODEX_FIXTURE, |v, t, s| p.feed(v, t, s)) {
            deliver(&mut k, &mut c, evs);
        }
        deliver(&mut k, &mut c, [Ev::Done { note: Some(p.note(Duration::from_millis(21_000))) }]);
        let m = c.chat.messages.last().unwrap();
        let tools = all_tools(&m.parts);
        let names: Vec<String> = tools.iter().map(|t| format!("{} {} [{}]", t.label, t.target, t.status)).collect();
        println!("{names:#?}");
        assert_eq!(names[0], "Write notes.txt [done]");
        assert_eq!(names[1], "Update notes.txt [done]");
        assert!(names[2].starts_with("Run Get-Content -LiteralPath notes.txt [error]"), "{}", names[2]);
        assert_eq!(tools[2].summary, "exit -1");
        assert_eq!(activity::todos(&m.parts).map(|t| t.iter().filter(|x| x.status == "completed").count()), Some(2));
        assert!(m.note.as_deref().unwrap_or("").contains("776 tokens"));
        let s = k.render_html(&mut c, 140, 44, "target/snap/chat-agent-codex.html");
        println!("{s}");
    }

    #[test]
    fn chat_agent_ask_prompt() {
        let mut k = Kit::new();
        let mut c = agent_chat(&k, "claude", "delete the build folder");
        let _forget = Forget(c.chat.id.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        let body = vec![agent::line('>', None, "rm -rf build")];
        let ask = |tool: &str, tx: &std::sync::mpsc::Sender<approve::Decision>| approve::Ask { tool: tool.into(), label: tool.into(), target: "rm -rf build".into(), body: body.clone(), reply: tx.clone() };
        deliver(&mut k, &mut c, [Ev::Ask(ask("Bash", &tx)), Ev::Ask(ask("Bash", &tx))]);
        let s = k.render_html(&mut c, 120, 30, "target/snap/chat-agent-ask.html");
        println!("{s}");
        assert!(s.contains("allow Bash rm -rf build ?") && s.contains("always allow Bash"));
        assert!(!s.contains("more lines"), "a one-line command isn't repeated under itself");
        k.key(&mut c, KeyCode::Char('n'));
        assert_eq!(rx.try_recv(), Ok(approve::Decision::Deny));
        assert_eq!(c.asks.len(), 1);
        k.key(&mut c, KeyCode::Char('a'));
        assert_eq!(rx.try_recv(), Ok(approve::Decision::Allow));
        assert!(c.asks.is_empty() && c.input.is_empty(), "y/n/a don't leak into the composer");
        // after "always", the next one answers itself
        deliver(&mut k, &mut c, [Ev::Ask(ask("Bash", &tx))]);
        assert_eq!(rx.try_recv(), Ok(approve::Decision::Allow));
        assert!(c.asks.is_empty());
    }

    #[test]
    fn chat_old_unknown_provider() {
        let mut k = Kit::new();
        let mut c = Chat::new(&k.config);
        c.provider = "claude".into();
        c.chat = serde_json::from_str(
            r#"{"id":"old1","title":"an old chat","created":1,"updated":2,"provider":"retired-ai",
            "messages":[{"role":"user","content":"hello"},{"role":"assistant","model":"retired-ai","content":"hi there","steps":[["looked it up","done",null]]}]}"#,
        )
        .unwrap();
        assert_eq!(c.provider_of(), "claude", "a chat with an AI oriel doesn't have continues with the current one");
        let s = k.render(&mut c, 100, 24);
        println!("{s}");
        assert!(s.contains("hi there") && s.contains("looked it up"));
        assert!(!s.contains("retired"));
    }
}

#[cfg(test)]
pub fn demo_now() -> f64 {
    store::now()
}

