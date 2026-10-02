//! The ai app: chat list in the sidebar, messages (your words in boxes on the right, replies with a header and
//! markdown on the left), the big logo on a new chat, a composer with a / command menu. Coding agents (Claude
//! Code, Codex) show their whole transcript live: every tool call, diffs, command output, todos, subagents.

mod activity;
mod agent;
mod ckpt;
pub mod approve;
mod inbox;
mod md;
pub mod providers;
mod review;
pub(crate) mod store;
#[cfg(test)]
mod qa_keys;

/// A diff line's background for a theme (line tint, changed-word tint): the themes app previews with it.
pub(crate) use activity::band as diff_band;

/// Markdown drawn the way the chat draws replies (the updates screen shows release notes with it).
pub(crate) fn render_markdown(text: &str, width: usize, t: &crate::theme::Theme) -> Vec<Line<'static>> {
    md::render(text, width, "", t)
}

use crate::editor::Editor;

/// A tool call as the transcript names it ("Update", "src/app.rs"), or None for the todo-list plumbing. The
/// search index keeps one such line per call.
pub(crate) fn tool_brief(name: &str, input: &serde_json::Value, cwd: &std::path::Path) -> Option<(String, String)> {
    (!agent::hidden(name)).then(|| (agent::label(name), agent::target(name, input, cwd)))
}

/// What the chat app should show next time it draws (the search app's `r`): a saved chat by id, or a session from
/// elsewhere to carry on, with the line that says what happened.
pub(crate) enum Open {
    Chat(String),
    Resume(store::Chat, String),
}

thread_local! {
    /// Set just before switching to the chat app (the UI is single-threaded; thread-local keeps tests apart).
    static OPEN: std::cell::RefCell<Option<Open>> = const { std::cell::RefCell::new(None) };
}

pub(crate) fn request_open(o: Open) {
    OPEN.with(|c| *c.borrow_mut() = Some(o));
}

/// The chat sidebar's "this folder only" switch, kept in a small file of its own.
fn folder_pref_path() -> PathBuf {
    crate::config::data_dir().join("chat-sidebar.json")
}

/// Same folder? (Windows paths compare without case and with either slash.)
fn same_dir(a: &str, b: &str) -> bool {
    let n = |s: &str| {
        let s = s.trim_end_matches(['/', '\\']);
        if cfg!(windows) { s.replace('\\', "/").to_lowercase() } else { s.to_string() }
    };
    n(a) == n(b)
}
use crate::pane::{Action, Cx, Pane};
use crate::panes::agents::git;
use crate::ui;
use activity::{Block, Hit};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use providers::Ev;
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

pub(crate) const COMMANDS: &[(&str, &str, &str)] = &[
    ("/new", "", "start a new chat"),
    ("/retry", "", "regenerate the last reply"),
    ("/provider", "<ai>", "switch AI: claude, codex, ollama, openai, anthropic — remembered"),
    ("/model", "<model>", "switch model for the current AI — remembered per AI"),
    ("/cwd", "<folder>", "folder Claude Code / Codex work in for this chat"),
    ("/commit", "[message]", "commit everything in this chat's folder (git add -A), the chat's title as the message"),
    ("/p", "<name>", "put a saved prompt in the box (ctrl+t in agents' forms saves them)"),
    ("/perms", "<ask|edits|auto|plan|bypass|reset>", "what coding agents may do in this chat (shift+tab cycles) · /perms default <mode> for new chats · reset forgets this chat's always-allows"),
    ("/effort", "<low|medium|high|xhigh|max|ultracode>", "how hard coding agents think — remembered for every chat"),
    ("/diff", "[last|turn n]", "coding agents: everything this chat changed in its folder (last: the last turn only) · u in it goes back"),
    ("/undo", "", "put the folder back as it was before the last turn (asks first, lists the new files it deletes)"),
    ("/rewind", "<turn>", "pick one of your turns: its diff against now, or put the folder back to before it"),
    ("/check", "[cmd|off]", "Claude Code / Codex: run a check after each turn, send failures back until it passes"),
    ("/verify", "[cmd]", "run the check now: the last reply gets ✓ or ✗"),
    ("/handoff", "[goal] [ai]", "the AI writes a handoff card and a fresh chat (maybe another AI) starts from it"),
    ("/goal", "<condition>", "Claude Code's own /goal: it keeps going until the condition holds (other /commands pass through too)"),
    ("/open", "<chat>", "open a saved chat: type to search its title (or /chats) · ctrl+pgup/pgdn the previous / next"),
    ("/rename", "<title>", "rename this chat"),
    ("/copy", "[code [n]|all]", "copy the last reply · code: its last code block (n = further back) · all: the whole chat"),
    ("/prompts", "<prompt>", "your earlier prompts from every chat: type to search, enter puts it in the box"),
    ("/lead", "[goal]", "agents: a lead run with this goal (or the last reply) — it plans and hands out the work"),
    ("/task", "<text>", "agents: a new task card with this prompt"),
    ("/tasks", "[goal]", "agents: split this goal (or the last reply) into task cards with the planner"),
    ("/key", "<openai|anthropic> <key>", "save an API key"),
    ("/note", "", "save the last reply to notes"),
    ("/save", "", "export this chat as a markdown file (tool calls folded in; never over an earlier export)"),
    ("/recall", "<words>", "search every AI chat and session on this computer (alt r)"),
    ("/theme", "<name|edit|new>", "switch theme · edit opens the theme editor · new <name> makes your own"),
    ("/play", "", "music: play / pause"),
    ("/next", "", "music: next song"),
    ("/prev", "", "music: previous song"),
    ("/music", "", "go to the music app"),
    ("/sidebar", "", "hide / show the sidebar — remembered"),
    ("/icons", "", "nerd font icons on / off — remembered"),
    ("/settings", "[setting]", "every setting in one place (alt ,) · /settings perms opens that one"),
    ("/info", "", "what's running: AI, model, folder"),
    ("/delete", "", "delete this chat"),
    ("/help", "", "keys and commands (F10: the full guide)"),
    ("/quit", "", "quit oriel"),
];

/// One row of the / menu: what it shows, and what picking it does.
struct MenuItem {
    left: String,
    /// Shown muted after `left`: a command's arguments.
    args: String,
    desc: String,
    /// The composer text after picking it.
    fill: String,
    /// Picking runs it (false = a command that still needs its argument typed).
    run: bool,
    /// Picking puts this in the box instead, unsent (an earlier prompt).
    put: Option<String>,
}

impl MenuItem {
    fn pick(left: String, desc: String, fill: String, run: bool) -> MenuItem {
        MenuItem { left, args: String::new(), desc, fill, run, put: None }
    }
}

/// How many rows the composer grows to before it scrolls.
const COMPOSER_ROWS: usize = 8;

/// A folder a coding agent shouldn't be let loose in by accident: your home folder, a drive root, the system.
fn risky_dir(p: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().trim_end_matches(['/', '\\']).to_lowercase().replace('/', "\\");
    let me = norm(p);
    if p.parent().is_none() || me.is_empty() || me.ends_with(':') || dirs::home_dir().is_some_and(|h| norm(&h) == me) {
        return true;
    }
    let sys = if cfg!(windows) {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()).to_lowercase();
        let drive = root.get(..2).unwrap_or("c:").to_string();
        vec![root, format!(r"{drive}\program files"), format!(r"{drive}\program files (x86)"), format!(r"{drive}\programdata"), format!(r"{drive}\users")]
    } else {
        ["\\usr", "\\etc", "\\bin", "\\sbin", "\\var", "\\opt", "\\system", "\\library", "\\home", "\\users"].map(String::from).to_vec()
    };
    sys.iter().any(|s| me == *s || me.starts_with(&format!("{s}\\")) && !s.ends_with("\\users") && !s.ends_with("\\home"))
}

/// The permission modes for coding agents in chat (Claude Code's own modes). The default for new chats is in the
/// config (settings, /perms default); shift+tab and /perms change one chat.
pub(crate) const PERMS: &[(&str, &str)] = &[
    ("ask", "asks you before each edit or command (y allow · n deny · a always allow it in this chat)"),
    ("edits", "edits files in the chat's folder; anything else is refused (default)"),
    ("auto", "auto mode: Claude decides what's safe and asks you about the rest"),
    ("plan", "read-only: looks and plans, changes nothing"),
    ("bypass", "bypass permissions: never asks, allows everything — only in folders you trust"),
];

/// Canonical permission name; the old "full" / "read" still work.
pub(crate) fn norm_perms(s: &str) -> Option<&'static str> {
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
pub(crate) const EFFORTS: &[(&str, &str)] = &[
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

/// The models oriel knows for an AI, with a word on each (the /model menu and settings suggest them). Ollama's
/// come from what's installed there, so they aren't here.
pub(crate) fn known_models(p: &str) -> &'static [(&'static str, &'static str)] {
    match p {
        "claude" => &[
            ("default", "Claude Code's default"),
            ("opus", "most capable (always the newest Opus)"),
            ("sonnet", "balanced speed and smarts"),
            ("haiku", "fastest and cheapest"),
            ("claude-opus-5-5", "Opus 5.5"),
            ("claude-sonnet-5", "Sonnet 5"),
            ("claude-haiku-4-5", "Haiku 4.5"),
            ("claude-fable-5-1", "Fable 5.1"),
        ],
        "codex" => &[
            ("default", "Codex's default"),
            ("gpt-5.6-sol", "most capable"),
            ("gpt-5.6-terra", "balanced"),
            ("gpt-5.6-luna", "fast and cheap"),
            ("gpt-5.3-codex", "older coding model"),
        ],
        "anthropic" => &[
            ("claude-sonnet-5", "Sonnet 5 — balanced"),
            ("claude-opus-5-5", "Opus 5.5 — most capable"),
            ("claude-haiku-4-5", "Haiku 4.5 — fastest"),
            ("claude-fable-5-1", "Fable 5.1"),
        ],
        "openai" => &[("gpt-5.6-terra", "balanced"), ("gpt-5.6-sol", "most capable"), ("gpt-5.6-luna", "fast and cheap")],
        _ => &[],
    }
}

/// The warning colour (themes don't have one of their own): auto mode, a filling context, a stale check.
const WARN: ratatui::style::Color = ratatui::style::Color::Rgb(0xe6, 0xc4, 0x6a);

/// The mode line on the input box, Claude Code style: glyph, words, colour.
fn perm_badge(p: &str, t: &crate::theme::Theme) -> (&'static str, &'static str, ratatui::style::Color) {
    match p {
        "edits" => ("⏵⏵", "accept edits on", t.accent),
        "auto" => ("⏵⏵", "auto mode on", WARN),
        "plan" => ("⏸", "plan mode on", t.shine),
        "bypass" => ("⏵⏵", "bypass permissions on", t.danger),
        _ => ("⏵", "asks before changes", t.muted),
    }
}

const SUGGESTIONS: &[&str] = &["explain how rainbows form", "give me 3 tips for better sleep", "code a snake game in html", "what is a python function?"];

use activity::SPIN;
const VERBS: &[&str] = &["thinking", "pondering", "noodling", "brewing", "untangling", "scheming", "percolating", "tinkering"];

/// How long a question or approval that just popped up ignores keys: the ones you were typing are for the box.
const GRACE: Duration = Duration::from_millis(300);

struct Stream {
    stop: Arc<AtomicBool>,
    inbox: Arc<Mutex<Vec<Ev>>>,
    status: String,
    started: Instant,
    tokens: u64,
    /// Straight to the running agent (Claude Code reads it at its next step); None = queue until the reply ends.
    steer: Option<std::sync::mpsc::Sender<providers::Steer>>,
    /// The CLI's process id once it runs (0 before; the HTTP APIs have none).
    pid: Arc<AtomicU32>,
    /// The permission mode this reply runs in: a change reaches a running Claude Code, Codex only takes it with
    /// your next message.
    perms: String,
}

/// A reply still running in a chat you switched away from, with everything waiting on it. It keeps going;
/// opening the chat again brings it back on screen.
struct Run {
    stream: Stream,
    asks: VecDeque<approve::Ask>,
    questions: VecDeque<approve::Question>,
    qs: QState,
    queue: Vec<Queued>,
    since: Instant,
}

/// What a batch of a run's events left for the pane to act on.
#[derive(Default)]
struct Outcome {
    finished: bool,
    failed: bool,
    /// its chat had no reply to write into (a safety net: deleting a chat stops its reply first)
    lost: bool,
    /// "N denied" in the closing note
    denied: Option<u32>,
    /// something new that waits on you: what to say about it
    waiting: Vec<String>,
    /// a new question came in (it scrolls into view)
    asked: bool,
    /// a plan was approved in this mode
    perms: Option<String>,
    /// why this turn has no checkpoint
    ckpt_err: Option<String>,
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

/// A click on the new-chat screen: a suggested prompt, or a folder for the agent to work in.
#[derive(Clone)]
enum HeroHit {
    Say(String),
    Folder(String),
}

#[derive(Clone)]
enum SideItem {
    New,
    /// the "all folders / this folder only" switch
    Folder,
    /// by id: a reply finishing in the background can reorder the list between a draw and a click
    Chat(String),
}

/// What background work (git, checks) hands back to the pane. Everything names its chat by id: it may not be the
/// one on screen by the time it's done.
enum Job {
    /// A turn is over (or /check, /verify asked): what it changed since its checkpoint, the folder as a tree, and
    /// the check's result when one ran. `verify` = /verify asked, not the check loop.
    Turn { chat: String, idx: usize, stat: Option<ckpt::Stats>, tree: Option<String>, check: Option<(String, Result<(), String>)>, verify: bool },
    /// /diff: a checkpoint (reply `idx`'s) against the folder now.
    Diff { chat: String, idx: usize, ckpt: String, r: Result<Vec<git::DiffFile>, String> },
    /// /undo, /rewind: what going back to reply `idx`'s checkpoint would do.
    Plan { chat: String, idx: usize, ckpt: String, r: Result<ckpt::Plan, String> },
    Restored { chat: String, idx: usize, r: Result<ckpt::Plan, String> },
}

/// The diff view over the transcript (/diff).
struct Review {
    chat: String,
    /// the reply whose checkpoint it starts from (u goes back to it)
    idx: usize,
    ckpt: String,
    /// "since turn 1", "turn 4"
    title: String,
    data: Option<Result<Vec<git::DiffFile>, String>>,
    file: usize,
    scroll: usize,
    hits: Vec<(Rect, usize)>,
}

/// Going back to a checkpoint, waiting for your y: what it would do (None while that's worked out).
struct Restore {
    chat: String,
    idx: usize,
    ckpt: String,
    plan: Option<Result<ckpt::Plan, String>>,
    /// y was pressed: it's being done
    busy: bool,
}

/// A chat's check loop (/check), saved in the chat file so it carries on after a restart.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
struct Check {
    cmd: String,
    /// checks run so far, and the most it runs
    turn: u32,
    max: u32,
    /// turns in a row that changed nothing
    #[serde(default)]
    idle: u32,
    /// the folder after the last check (no change = no progress)
    #[serde(default)]
    tree: String,
    /// on · passed · stalled · capped · off · broken
    status: String,
    /// "exit 101"
    #[serde(default)]
    last: String,
}

/// How many checks a loop runs before it stops and asks you.
const CHECK_TURNS: u32 = 20;

fn check_of(c: &store::Chat) -> Option<Check> {
    c.extra.get("check").and_then(|v| serde_json::from_value(v.clone()).ok())
}

fn set_check(c: &mut store::Chat, ck: &Check) {
    if let Ok(v) = serde_json::to_value(ck) {
        c.extra.insert("check".into(), v);
    }
}

/// /handoff in progress: the card is being written; then a fresh chat starts from it.
struct Handoff {
    goal: String,
    /// another AI to hand to (None = the same one)
    provider: Option<String>,
}

/// What the AI is asked to write for /handoff.
const HANDOFF_ASK: &str = "Write a handoff card so a fresh session (maybe a different AI) can pick this up without this conversation. Markdown, under 350 words, with these headings: **State** (what works now, what doesn't), **Decisions** (what was chosen, and why), **Files** (the paths that matter, one line each; point to specs and docs by path instead of copying them), **Check** (the command that verifies the work, whether it passes now, and the failing lines if not), **Next** (numbered next steps). Only the card: no preamble, no questions.";

pub struct Chat {
    chats: Vec<store::Chat>,
    chat: store::Chat,
    input: String,
    cursor: usize, // char index into input
    /// The composer's editing: `input` and `cursor` go through it for each key (moves by line and word, undo).
    ed: Editor,
    /// The composer's text width last frame (↑↓ move through its wrapped rows).
    comp_w: usize,
    scroll: usize, // lines up from the bottom; 0 = follow the end
    stream: Option<Stream>,
    menu_sel: usize,
    /// Finished messages, drawn once: by index, (what they were drawn from, the lines).
    cache: HashMap<usize, (u64, Block)>,
    /// The reply streaming on screen, drawn a block at a time: (chat id, message index) and its blocks.
    live: (String, usize, activity::BlockCache),
    provider: String,
    /// This chat's permission mode (shift+tab, /perms); new chats start from `default_perms` (the config's).
    perms: String,
    default_perms: String,
    /// /effort ("" = the agent's default)
    effort: String,
    keys: HashMap<String, String>,
    /// ctrl+o: every call shows its full diff / output.
    expanded: bool,
    /// Calls clicked open (closed, when expanded).
    open: HashSet<String>,
    /// Screen row -> what a click there does (open a call, copy a code block).
    click_hits: Vec<(u16, Hit)>,
    /// Claude Code waiting for a yes/no (/perms ask), oldest first.
    asks: VecDeque<approve::Ask>,
    /// What you said "always" to, per chat, until oriel closes: approve::rule's `Bash(npm test:*)`, `Edit`...
    always: HashMap<String, BTreeSet<String>>,
    /// Claude's questions waiting for you (AskUserQuestion), oldest first, and where you are in the first one.
    questions: VecDeque<approve::Question>,
    qs: QState,
    /// When the prompt that takes the keys (the first question, else the first approval) appeared.
    prompt_since: Instant,
    /// Replies running in the chats you aren't looking at, by chat id.
    parked: HashMap<String, Run>,
    /// This pane has run a reply (until then it has no agent status to show).
    ran: bool,
    /// When the pane was last drawn: while a reply runs that's every 100 ms if it's on screen.
    drawn: Option<Instant>,
    /// Last frame's (chat, width, how far it could scroll, lines): scrolled up, the lines you're reading stay put
    /// as more arrive.
    anchor: Option<(String, u16, usize, usize)>,
    /// Lines that arrived below while you were scrolled up.
    unseen: usize,
    /// This frame's lines changed because you opened or closed calls, not because anything arrived.
    relayout: bool,
    info: Vec<String>,
    avail: Vec<&'static str>,
    /// the model picked per AI (/model), remembered in the config
    models: std::collections::BTreeMap<String, String>,
    /// models installed in Ollama, fetched in the background for the /model menu
    ollama_models: Arc<Mutex<Vec<String>>>,
    confirm_delete: bool,
    side_hits: Vec<(Rect, SideItem)>,
    side_scroll: usize,
    /// The chat the list last scrolled to (it follows the open chat, and leaves the wheel alone otherwise).
    side_follow: String,
    hero_hits: Vec<(Rect, HeroHit)>,
    launch_dir: PathBuf,
    /// You said yes to running this chat's agent in a risky folder (home, a drive root, the system); and you were
    /// told once (enter again says yes).
    risky_ok: bool,
    risky_armed: bool,
    /// Git repos under the usual code folders, found once in the background (the new chat's folder picker).
    repos: Arc<Mutex<Option<Vec<String>>>>,
    /// Where ↑ is in your earlier prompts (0 = the newest), and the chat the recalled one came from if it's another.
    history_pos: Option<usize>,
    history_from: Option<String>,
    /// Plan windows for Claude Code / Codex, (agent, window, used %), read in the background now and then.
    usage: Arc<Mutex<Vec<(String, String, f64)>>>,
    usage_read: Option<Instant>,
    offset: i64,
    /// Messages typed while the AI was replying, oldest first (enter queues, ctrl+x s sends now).
    queue: Vec<Queued>,
    /// ctrl+x was pressed: s sends now.
    chord_x: bool,
    /// Background git work and checks handing back their results.
    jobs: Arc<Mutex<Vec<Job>>>,
    /// /diff's view, over the transcript while it's open.
    review: Option<Review>,
    /// /undo or /rewind waiting for your y.
    restore: Option<Restore>,
    /// Chats whose check is running now.
    checking: HashSet<String>,
    /// Chats writing a /handoff card.
    handoffs: HashMap<String, Handoff>,
    /// Chats already told why they have no checkpoints (said once, not every turn).
    ckpt_warned: HashSet<String>,
    /// A transcript to read, not a chat to type in (the search app shows sessions with it).
    readonly: bool,
    /// Scroll so this message is at the top, on the next draw.
    focus_msg: Option<usize>,
    /// Where each message starts in the last transcript laid out (focus_msg scrolls with it).
    msg_starts: Vec<usize>,
    /// A session carried on from elsewhere (search's `r`): only saved once you send something in it.
    draft: bool,
    /// The sidebar lists only chats from the current chat's folder (ctrl+f).
    this_folder: bool,
}

impl Chat {
    pub fn new(cfg: &crate::config::Config) -> Self {
        let chats = store::load_all();
        let avail = providers::available(&cfg.ai);
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
        let mut c = Self::make(cfg, chats, avail, ollama_models);
        if !cfg!(test) {
            c.this_folder = std::fs::read_to_string(folder_pref_path()).ok().and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()).is_some_and(|v| v["this_folder"] == true);
        }
        c
    }

    /// A read-only view of a transcript (a session from the search app), scrolled to message `focus`. No saved chats
    /// are loaded and no AI is asked about: it only draws.
    pub(crate) fn viewer(cfg: &crate::config::Config, chat: store::Chat, focus: usize) -> Self {
        let mut c = Self::make(cfg, vec![], vec![], Arc::default());
        c.provider = chat.provider.clone().filter(|p| providers::is_known(p)).unwrap_or_else(|| "claude".into());
        c.chat = chat;
        c.readonly = true;
        c.focus_msg = Some(focus);
        c
    }

    fn make(cfg: &crate::config::Config, chats: Vec<store::Chat>, avail: Vec<&'static str>, ollama_models: Arc<Mutex<Vec<String>>>) -> Self {
        // the configured AI if this machine has it, else the first one it does have
        let provider = if !cfg.ai.provider.is_empty() && avail.contains(&cfg.ai.provider.as_str()) {
            cfg.ai.provider.clone()
        } else {
            avail.first().map(|s| s.to_string()).unwrap_or_else(|| "none".into())
        };
        let mut first = store::Chat::new(&provider);
        first.model = cfg.ai.models.get(&provider).cloned().filter(|m| !m.is_empty());
        Chat {
            chats,
            chat: first,
            models: cfg.ai.models.clone(),
            ollama_models,
            input: String::new(),
            cursor: 0,
            ed: Editor::new(""),
            comp_w: 80,
            scroll: 0,
            stream: None,
            menu_sel: 0,
            cache: HashMap::new(),
            live: (String::new(), 0, HashMap::new()),
            provider,
            perms: norm_perms(&cfg.ai.perms).unwrap_or("edits").to_string(),
            default_perms: norm_perms(&cfg.ai.perms).unwrap_or("edits").to_string(),
            effort: cfg.ai.effort.clone(),
            keys: HashMap::new(),
            expanded: false,
            open: HashSet::new(),
            click_hits: vec![],
            asks: VecDeque::new(),
            always: HashMap::new(),
            questions: VecDeque::new(),
            qs: QState::default(),
            prompt_since: Instant::now(),
            parked: HashMap::new(),
            ran: false,
            drawn: None,
            anchor: None,
            unseen: 0,
            relayout: false,
            info: vec![],
            avail,
            confirm_delete: false,
            side_hits: vec![],
            side_scroll: 0,
            side_follow: String::new(),
            hero_hits: vec![],
            launch_dir: std::env::current_dir().unwrap_or_default(),
            risky_ok: false,
            risky_armed: false,
            repos: Arc::default(),
            history_pos: None,
            history_from: None,
            usage: Arc::default(),
            usage_read: None,
            offset: crate::app::local_offset_secs(),
            queue: vec![],
            chord_x: false,
            jobs: Arc::default(),
            review: None,
            restore: None,
            checking: HashSet::new(),
            handoffs: HashMap::new(),
            ckpt_warned: HashSet::new(),
            readonly: false,
            focus_msg: None,
            msg_starts: vec![],
            draft: false,
            this_folder: false,
        }
    }

    /// The search app asked for a chat (or a session to carry on): show it.
    fn take_open(&mut self) {
        let Some(o) = OPEN.with(|c| c.borrow_mut().take()) else { return };
        match o {
            Open::Chat(id) => {
                if !self.chats.iter().any(|c| c.id == id) {
                    self.chats = store::load_all();
                }
                if self.chats.iter().any(|c| c.id == id) {
                    self.open_chat(&id);
                } else {
                    self.info = vec!["that chat isn't there any more".into()];
                }
            }
            Open::Resume(c, note) => {
                self.stop();
                self.persist_if_changed();
                self.chat = c;
                self.draft = true;
                self.cache.clear();
                self.scroll = 0;
                self.info = vec![note];
            }
        }
    }

    /// The folder the sidebar's "this folder" switch means: the current chat's.
    fn folder(&self) -> String {
        self.chat.cwd.clone().unwrap_or_else(|| self.launch_dir.to_string_lossy().to_string())
    }

    fn toggle_folder(&mut self) {
        self.this_folder = !self.this_folder;
        self.side_scroll = 0;
        if !cfg!(test) {
            let _ = std::fs::write(folder_pref_path(), serde_json::json!({ "this_folder": self.this_folder }).to_string());
        }
    }

    /// For other panes' tests: the chat's CLI state (JSON), its info lines, and whether it's still a draft.
    #[cfg(test)]
    pub(crate) fn test_view(&self) -> (String, Vec<String>, bool) {
        (serde_json::to_string(&self.chat.state).unwrap_or_default(), self.info.clone(), self.draft)
    }

    /// The chat's AI, or the current default when it used one oriel doesn't have (an old chat).
    fn provider_of(&self) -> String {
        self.provider_for(&self.chat)
    }

    fn provider_for(&self, c: &store::Chat) -> String {
        match &c.provider {
            Some(p) if providers::is_known(p) => p.clone(),
            _ => self.provider.clone(),
        }
    }

    fn workdir(&self) -> PathBuf {
        self.workdir_for(&self.chat)
    }

    /// Where CLI agents run: the chat's folder (/cwd), else the folder oriel was started in. Only if that folder
    /// no longer exists does it fall back to oriel's own work folder.
    fn workdir_for(&self, c: &store::Chat) -> PathBuf {
        let d = c.cwd.clone().map(PathBuf::from).unwrap_or_else(|| self.launch_dir.clone());
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
        // a reply that's still running keeps going in the background: switching chats never stops anything
        self.park();
        self.persist_if_changed();
        self.draft = false; // a session carried on but never answered is let go
        let p = self.provider_of();
        self.chat = store::Chat::new(&p);
        self.chat.model = self.models.get(&p).cloned().filter(|m| !m.is_empty());
        self.perms = self.default_perms.clone(); // a mode picked for the last chat stays with it
        self.fresh_view();
    }

    fn open_chat(&mut self, id: &str) {
        if id == self.chat.id {
            return;
        }
        self.park();
        self.persist_if_changed();
        // by id: saving the chat we're leaving can reorder the list under us
        let Some(c) = self.chats.iter().find(|c| c.id == id).cloned() else { return };
        self.draft = false;
        self.chat = c;
        self.perms = self.default_perms.clone();
        self.fresh_view();
        self.unpark();
        crate::panes::agents::note_chat_dir(&self.workdir_label());
    }

    /// A different chat on screen: its own lines, scrolled to the end.
    fn fresh_view(&mut self) {
        self.cache.clear();
        self.live.2.clear();
        self.scroll = 0;
        self.unseen = 0;
        self.anchor = None;
        self.info.clear();
        self.history_pos = None;
        self.history_from = None;
        self.risky_ok = false;
        self.risky_armed = false;
        self.review = None;
        self.restore = None;
        // a ctrl+d (or ctrl+x) pending on the chat you clicked away from mustn't land on this one
        self.confirm_delete = false;
        self.chord_x = false;
    }

    /// The chat's folder as the agent will get it, without touching the disk (it's drawn every frame).
    fn workdir_label(&self) -> PathBuf {
        self.chat.cwd.clone().map(PathBuf::from).unwrap_or_else(|| self.launch_dir.clone())
    }

    /// A new Claude Code / Codex chat that would start in your home folder, a drive root or a system folder.
    fn needs_folder(&self) -> bool {
        self.chat.messages.is_empty() && self.chat.cwd.is_none() && !self.risky_ok && matches!(self.provider_of().as_str(), "claude" | "codex") && risky_dir(&self.launch_dir)
    }

    /// The chat on screen's run, out of the pane's fields.
    fn take_run(&mut self) -> Option<Run> {
        let stream = self.stream.take()?;
        Some(Run {
            stream,
            asks: std::mem::take(&mut self.asks),
            questions: std::mem::take(&mut self.questions),
            qs: std::mem::take(&mut self.qs),
            queue: std::mem::take(&mut self.queue),
            since: self.prompt_since,
        })
    }

    fn put_run(&mut self, r: Run) {
        self.stream = Some(r.stream);
        self.asks = r.asks;
        self.questions = r.questions;
        self.qs = r.qs;
        self.queue = r.queue;
        self.prompt_since = r.since;
    }

    /// Leaving a chat: its reply (if one runs) carries on in the background.
    fn park(&mut self) {
        if let Some(r) = self.take_run() {
            self.parked.insert(self.chat.id.clone(), r);
        }
    }

    /// Back on a chat: its reply, and whatever it's waiting on you for, comes back on screen. A prompt that was
    /// waiting there has only just appeared for you: it gets the same grace as a new one (keys typed on the way
    /// in, ctrl+pgdn and on, mustn't answer it).
    fn unpark(&mut self) {
        if let Some(r) = self.parked.remove(&self.chat.id) {
            self.perms = r.stream.perms.clone();
            self.put_run(r);
            if !self.asks.is_empty() || !self.questions.is_empty() {
                self.front_changed();
            }
        }
    }

    /// A different prompt takes the keys now (see GRACE).
    fn front_changed(&mut self) {
        self.prompt_since = Instant::now();
    }

    /// Replies running in chats you aren't looking at: their events go into their own chat, they tell you when
    /// they need you or finish, and a message you queued there goes when its reply ends.
    fn poll_parked(&mut self, cx: &mut Cx) {
        use crate::alerts::Kind;
        let ids: Vec<String> = self.parked.keys().cloned().collect();
        for id in ids {
            let Some(mut run) = self.parked.remove(&id) else { continue };
            let evs: Vec<Ev> = std::mem::take(&mut *run.stream.inbox.lock().unwrap());
            if evs.is_empty() {
                self.parked.insert(id, run);
                continue;
            }
            let Some(i) = self.chats.iter().position(|c| c.id == id) else {
                run.stream.stop.store(true, Ordering::SeqCst); // its chat is gone
                continue;
            };
            let who = providers::label(&self.provider_for(&self.chats[i])).to_lowercase();
            let out = absorb(&mut self.chats[i], &mut run, evs, self.always.get(&id), &who);
            let title = self.chats[i].title.clone();
            for w in &out.waiting {
                cx.alert(Kind::Approval, format!("{w} · in \"{title}\""));
            }
            if let (Some(p), Some(tx)) = (out.perms, &run.stream.steer) {
                let _ = tx.send(providers::Steer::Mode(p));
            }
            if !out.finished {
                self.parked.insert(id, run);
                continue;
            }
            if !out.lost {
                if out.failed {
                    cx.alert(Kind::BuildFailed, format!("{who} stopped with an error: {title}"));
                } else {
                    cx.alert(Kind::AgentDone, format!("{who} finished: {title}"));
                }
            }
            // saved, and up to the top of the list like any chat that just changed
            let mut c = self.chats.remove(i);
            store::save(&mut c);
            // what you queued there and it didn't read is the next message, sent right away
            let text = run.queue.drain(..).map(|q| q.text).collect::<Vec<_>>().join("\n\n");
            drop(run);
            self.chats.insert(0, c);
            self.turn_ended(&id, !(out.failed || out.lost), cx);
            if !text.is_empty() && (out.failed || out.lost) {
                self.info.push(format!("the reply in \"{title}\" failed, so what you queued there wasn't sent: {text}"));
            } else if !text.is_empty() {
                self.send_to(&id, text, cx);
            }
            self.handoff_ready(&id, out.failed || out.lost, cx);
        }
    }

    /// Send `text` as your next message in chat `id`, on screen or not: a reply running there gets it queued, a
    /// chat in the background starts its reply there.
    fn send_to(&mut self, id: &str, text: String, cx: &mut Cx) {
        if id == self.chat.id {
            return self.send(text, cx);
        }
        if let Some(run) = self.parked.get_mut(id) {
            run.queue.push(Queued { text, sent: false });
            return;
        }
        let Some(i) = self.chats.iter().position(|c| c.id == id) else { return };
        let mut c = self.chats.remove(i);
        c.messages.push(store::Msg { role: "user".into(), content: text, ..Default::default() });
        let (stream, msg) = self.launch(&c, cx);
        c.messages.push(msg);
        self.parked.insert(id.to_string(), Run { stream, asks: VecDeque::new(), questions: VecDeque::new(), qs: QState::default(), queue: vec![], since: Instant::now() });
        self.chats.insert(0, c);
    }

    /// Save only if it differs from the saved copy — just looking at a chat mustn't bump it to the top.
    fn persist_if_changed(&mut self) {
        if self.draft || self.readonly {
            return; // a session carried on from elsewhere becomes a chat of yours once you say something in it
        }
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

    /// Stop the reply on screen (esc). Other chats' replies keep going.
    fn stop(&mut self) {
        if let Some(s) = self.stream.take() {
            s.stop.store(true, Ordering::SeqCst);
            self.asks.clear(); // unanswered approvals become a no
            self.questions.clear(); // and unanswered questions a skip
            self.qs = QState::default();
            if let Some(m) = self.chat.messages.last_mut() {
                mark_stopped(m);
            }
            self.persist();
        }
        self.unqueue_to_box("stopped");
    }

    /// Delete the chat on screen. Its reply stops first, so it can't carry on working on a chat that's gone, and
    /// stopping saves the chat, so the file goes after that.
    fn delete_chat(&mut self, cx: &mut Cx) {
        self.queue.clear();
        self.stop();
        let id = self.chat.id.clone();
        store::delete(&id);
        self.chats.retain(|c| c.id != id);
        self.always.remove(&id);
        let p = self.provider_of();
        self.chat = store::Chat::new(&p);
        self.chat.model = self.models.get(&p).cloned().filter(|m| !m.is_empty());
        self.perms = self.default_perms.clone();
        self.fresh_view();
        cx.notify("chat deleted");
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
        let sent = self.stream.as_ref().and_then(|s| s.steer.as_ref()).is_some_and(|tx| tx.send(providers::Steer::Say(text.clone())).is_ok());
        self.queue.push(Queued { text, sent });
        self.scroll = 0;
    }

    /// A new permission mode reaches the running Claude Code at once (the reply on screen only: one running in
    /// another chat, maybe another folder, keeps the mode it started with). Codex takes it with your next message.
    fn push_perms(&mut self) {
        let perms = self.perms.clone();
        if let Some(s) = &mut self.stream {
            if s.steer.as_ref().is_some_and(|tx| tx.send(providers::Steer::Mode(perms.clone())).is_ok()) {
                s.perms = perms;
            }
        }
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
                // skip: the answers you already gave in this set still go back, the rest as skipped
                if let Some(q) = self.questions.pop_front() {
                    let mut answers = std::mem::take(&mut self.qs.answers);
                    let reply = if answers.is_empty() {
                        None
                    } else {
                        for rest in &q.qs[self.qs.idx.min(q.qs.len())..] {
                            answers.insert(rest.question.clone(), serde_json::Value::String("(skipped)".into()));
                        }
                        Some(answers)
                    };
                    let _ = q.reply.send(reply);
                }
                self.qs = QState::default();
                self.front_changed();
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
            self.front_changed();
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
        if self.needs_folder() {
            // not in your home folder or System32 by accident: pick a folder, or enter again says yes to this one
            if !self.risky_armed {
                self.risky_armed = true;
                self.set_input(text);
                if !self.info.is_empty() {
                    // the picker only shows on an empty screen: say it here too
                    self.info.push(format!("{} would work in {} — enter again sends it from there, or /cwd <folder> first", providers::label(&self.provider_of()), self.launch_dir.display()));
                }
                return;
            }
            self.risky_ok = true;
        }
        if let Some(warn) = self.effort_warning() {
            self.info.push(warn);
        }
        if self.chat.messages.is_empty() {
            self.chat.title = store::title_from(&text);
            if self.chat.cwd.is_none() {
                self.chat.cwd = Some(self.launch_dir.to_string_lossy().to_string());
            }
        }
        self.chat.messages.push(store::Msg { role: "user".into(), content: text, ..Default::default() });
        crate::panes::agents::note_chat_dir(&self.workdir_label());
        self.start_reply(cx);
    }

    /// Ask the provider for a reply to the conversation as it stands (last message = the user's).
    fn start_reply(&mut self, cx: &mut Cx) {
        self.draft = false; // you've said something (or regenerated): it's a chat of yours now
        let (stream, msg) = self.launch(&self.chat, cx);
        self.chat.messages.push(msg);
        self.stream = Some(stream);
        self.ran = true;
        // a warning about the plan window stays up while the reply runs
        self.info.retain(|l| l.starts_with('⚠'));
        self.scroll = 0;
    }

    /// Sending at effort max / ultracode with the AI's plan window nearly used up: say so, once, in the info area.
    fn effort_warning(&self) -> Option<String> {
        if !matches!(self.effort.as_str(), "max" | "ultracode") {
            return None;
        }
        let p = self.provider_of();
        let (label, pct) = self.usage.lock().unwrap().iter().filter(|u| u.0 == p).map(|u| (u.1.clone(), u.2)).fold(None, |a: Option<(String, f64)>, u| if a.as_ref().is_some_and(|a| a.1 >= u.1) { a } else { Some(u) })?;
        (pct > 85.0).then(|| format!("⚠ {} {label} window at {pct:.0}% — effort {} uses it up fast (/effort high is lighter)", providers::label(&p).to_lowercase(), self.effort))
    }

    /// "5h 72%" for the chat's AI (its fullest plan window), and whether that's close to the limit.
    fn usage_text(&self) -> Option<(String, bool)> {
        let p = self.provider_of();
        let u = self.usage.lock().unwrap();
        let w = u.iter().filter(|u| u.0 == p).max_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))?;
        let short = match w.1.as_str() {
            "5-hour" => "5h".to_string(),
            "weekly" => "wk".to_string(),
            l => l.replace("-hour", "h").replace("-day", "d"),
        };
        Some((format!("{short} {:.0}%", w.2), w.2 > 85.0))
    }

    /// Plan windows, re-read in the background every minute while a Claude Code / Codex chat is on screen.
    fn refresh_usage(&mut self, waker: crate::pane::Waker) {
        if cfg!(test) || !matches!(self.provider_of().as_str(), "claude" | "codex") || self.usage_read.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
            return;
        }
        self.usage_read = Some(Instant::now());
        let into = self.usage.clone();
        std::thread::spawn(move || {
            let got = crate::panes::agents::usage_limits().into_iter().map(|(a, w)| (a, w.label, w.pct)).collect();
            *into.lock().unwrap() = got;
            waker.wake(); // redraw with the numbers
        });
    }

    /// What this chat's replies have cost so far.
    fn spend(&self) -> f64 {
        self.chat.messages.iter().map(|m| m.cost()).sum()
    }

    /// Start a reply to `chat` (its last message is the user's): the run, and the empty reply to add to the chat.
    /// Works for a chat that isn't on screen too (a queued message sent when its reply ends in the background).
    fn launch(&self, chat: &store::Chat, cx: &mut Cx) -> (Stream, store::Msg) {
        let provider = self.provider_for(chat);
        let (messages, since) = history(chat, &provider);
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
        let pid: Arc<AtomicU32> = Arc::default();
        let cwd = self.workdir_for(chat);
        // a coding agent's folder is checkpointed first, so this turn can be looked at and undone
        let snap = matches!(provider.as_str(), "claude" | "codex") && ckpt_allowed(&cwd);
        let req = providers::Request {
            provider: provider.clone(),
            model: chat.model.clone(),
            messages,
            cwd: cwd.clone(),
            perms: self.perms.clone(),
            state: chat.state.clone(),
            cfg,
            steer: steer_rx,
            effort: self.effort.clone(),
            pid: pid.clone(),
            since,
        };
        let (ib, waker) = (inbox.clone(), cx.waker());
        let send = move |ev| {
            ib.lock().unwrap().push(ev);
            waker.wake();
        };
        if snap {
            let (id, turn, stop) = (chat.id.clone(), chat.messages.len(), stop.clone());
            std::thread::spawn(move || {
                send(Ev::Checkpoint(ckpt::Place::of(&cwd).and_then(|p| ckpt::snapshot(&p, &id, turn, &ckpt::CAPS))));
                if !stop.load(Ordering::SeqCst) {
                    providers::start(req, stop, send);
                }
            });
        } else {
            providers::start(req, stop.clone(), send);
        }
        let stream = Stream { stop, inbox, status: String::new(), started: Instant::now(), tokens: 0, steer, pid, perms: self.perms.clone() };
        (stream, store::Msg { role: "assistant".into(), model: Some(provider), state_before: Some(chat.state.clone()), ..Default::default() })
    }

    fn retry(&mut self, cx: &mut Cx) {
        if self.stream.is_some() {
            return;
        }
        if self.chat.messages.last().map(|m| m.role == "assistant").unwrap_or(false) {
            let m = self.chat.messages.pop().unwrap_or_default();
            self.cache.clear();
            // the CLI session remembers the reply you're throwing away (resumed, it would see your message twice):
            // go back to the session from before it, or start afresh from the conversation's text
            let p = m.model.clone().filter(|p| providers::is_known(p)).unwrap_or_else(|| self.provider_of());
            let sid = |v: Option<&serde_json::Value>| v.and_then(|v| v["session"].as_str().or(v["thread"].as_str()).map(String::from));
            let before = m.state_before.as_ref().and_then(|s| s.get(&p)).cloned();
            match before {
                Some(b) if sid(Some(&b)) != sid(self.chat.state.get(&p)) => {
                    self.chat.state.insert(p.clone(), b);
                }
                _ => {
                    self.chat.state.remove(&p);
                }
            }
            if m.parts.iter().any(|x| matches!(x, store::Part::Tool(t) if matches!(t.label.as_str(), "Update" | "Write" | "Delete" | "Notebook") && t.status == "done")) {
                self.info.push("the files it changed last time are still changed: it starts from the folder as it is now".into());
            }
        }
        if self.chat.messages.last().map(|m| m.role == "user").unwrap_or(false) {
            let note = std::mem::take(&mut self.info);
            self.start_reply(cx);
            self.info = note;
        }
    }

    fn run_command(&mut self, line: &str, cx: &mut Cx) {
        let mut parts = line.splitn(2, ' ');
        let cmd = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("").trim().to_string();
        self.info.clear();
        // agents follows the chat you're in (its /lead and /task run on this folder's repo)
        crate::panes::agents::note_chat_dir(&self.workdir_label());
        match cmd {
            "/new" => self.new_chat(),
            "/commit" => {
                let dir = self.workdir();
                let msg = if arg.is_empty() { self.chat.title.clone() } else { arg.clone() };
                let msg = if msg.trim().is_empty() || msg == "New chat" { "work in progress".to_string() } else { msg };
                match crate::panes::agents::commit_all(&dir, &msg) {
                    Ok(s) => cx.notify(format!("{s} · \"{msg}\"")),
                    Err(e) => self.info.push(format!("couldn't commit in {}: {e}", dir.display())),
                }
            }
            "/p" => {
                let all = crate::panes::agents::prompts::load();
                match crate::panes::agents::prompts::find(&all, &arg) {
                    Some(p) => self.set_input(p.text.clone()),
                    None => {
                        let path = crate::panes::agents::prompts::path();
                        if all.is_empty() {
                            self.info.push(format!("no saved prompts yet: ctrl+t in an agents goal or prompt saves one, or add [[prompt]] name/text to {}", path.display()));
                        } else {
                            self.info.push(if arg.is_empty() { "saved prompts (/p <name>):".to_string() } else { format!("no saved prompt called {arg}:") });
                            for p in &all {
                                self.info.push(format!("  /p {:<16} {}", p.name, ui::fit(&p.text.replace('\n', " "), 60)));
                            }
                        }
                    }
                }
            }
            "/retry" => self.retry(cx),
            "/open" | "/chats" if arg.is_empty() => self.info.push("type /open and part of a chat's title: the list narrows as you type (ctrl+pgup / ctrl+pgdn step through them)".into()),
            "/open" | "/chats" => {
                let q = arg.to_lowercase();
                let found = self.chats.iter().find(|c| c.id == arg).or_else(|| self.chats.iter().find(|c| c.title.to_lowercase().contains(&q))).map(|c| c.id.clone());
                match found {
                    Some(id) => self.open_chat(&id),
                    None => self.info.push(format!("no chat called \"{arg}\"")),
                }
            }
            "/rename" if arg.is_empty() => self.info.push(format!("/rename <title> — this one is \"{}\"", self.chat.title)),
            "/rename" if self.chat.messages.is_empty() => self.info.push("a new chat takes its title from your first message: send one, then /rename it".into()),
            "/rename" => {
                self.chat.title = arg.trim_matches('"').trim().to_string();
                self.persist();
                cx.notify(format!("renamed: {}", self.chat.title));
            }
            "/copy" => self.copy_command(&arg, cx),
            "/prompts" => {
                let q = arg.to_lowercase();
                match self.prompt_history().into_iter().find(|(p, _)| q.is_empty() || p.to_lowercase().contains(&q)) {
                    Some((p, _)) => self.set_input(p),
                    None if q.is_empty() => self.info.push("no earlier prompts yet: what you send is kept, in every chat".into()),
                    None => self.info.push(format!("none of your earlier prompts has \"{arg}\" in it")),
                }
            }
            "/lead" | "/tasks" | "/task" => self.to_agents(cmd, &arg, cx),
            "/diff" => self.diff_command(&arg, cx),
            "/undo" => self.undo_command(cx),
            "/rewind" => self.rewind_command(&arg, cx),
            "/check" => self.check_command(&arg, cx),
            "/verify" => self.verify_command(&arg, cx),
            "/handoff" => self.handoff(&arg, cx),
            "/goal" if arg.is_empty() => self.info.push("/goal <condition> — Claude Code keeps working until it holds (its evaluator reads the transcript; /check runs a real command)".into()),
            "/goal" if self.provider_of() != "claude" => self.info.push("/goal is Claude Code's: /provider claude, or /check <cmd> runs a real check after each turn with any coding agent".into()),
            // /model <name>: a model for the current AI; the old "/model <ai> [model]" still works
            "/model" if !arg.is_empty() && !providers::PROVIDERS.iter().any(|p| p.0 == arg.split_whitespace().next().unwrap_or("").to_lowercase()) => {
                let p = self.provider_of();
                let model = if arg.eq_ignore_ascii_case("default") { None } else { Some(arg.clone()) };
                self.chat.model = model.clone();
                self.chat.state.remove(&p); // a CLI session is tied to its model
                self.models.insert(p.clone(), model.clone().unwrap_or_default());
                let (id, m) = (p.clone(), model.clone().unwrap_or_default());
                cx.edit_config(move |c| {
                    c.ai.models.insert(id, m);
                });
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
                            _ => format!("  add a key in settings (alt ,) › providers & keys, or /key {id} <key>"),
                        });
                    } else if providers::PROVIDERS.iter().any(|p| p.0 == id) {
                        // an explicit model, else the one remembered for this AI
                        let model = a.next().map(String::from).or_else(|| self.models.get(&id).cloned()).filter(|m| !m.is_empty());
                        self.chat.provider = Some(id.clone());
                        self.chat.model = model.clone();
                        self.provider = id.clone();
                        if let Some(m) = &model {
                            self.models.insert(id.clone(), m.clone());
                        }
                        let (pid, m) = (id.clone(), model.clone());
                        cx.edit_config(move |c| {
                            if let Some(m) = m {
                                c.ai.models.insert(pid.clone(), m);
                            }
                            c.ai.provider = pid;
                        });
                        cx.notify(format!("{}{} · {} · remembered", ui::lead("ai"), providers::label(&id), model.unwrap_or_else(|| "default model".into())));
                    } else {
                        self.info.push(format!("no AI called {id} — try /provider"));
                    }
                }
            }
            "/cwd" => {
                // a completed folder ends in a separator: D:\work\ is D:\work (a root stays a root)
                let trimmed = arg.trim_end_matches(['/', '\\']);
                let arg = if trimmed.is_empty() || trimmed.ends_with(':') { arg.clone() } else { trimmed.to_string() };
                let p = PathBuf::from(if arg.starts_with('~') { arg.replacen('~', &dirs::home_dir().unwrap_or_default().to_string_lossy(), 1) } else { arg.clone() });
                if p.is_dir() {
                    self.chat.cwd = Some(p.to_string_lossy().to_string());
                    self.chat.state.clear(); // CLI sessions are per folder
                    crate::panes::agents::note_chat_dir(&p);
                    cx.notify(format!("this chat's agent works in {}", p.display()));
                } else {
                    self.info.push(format!("not a folder: {arg}"));
                }
            }
            "/effort" => {
                let want = arg.trim().to_lowercase();
                if let Some((level, what)) = EFFORTS.iter().find(|(l, _)| *l == want) {
                    self.effort = if *level == "default" { String::new() } else { level.to_string() };
                    let e = self.effort.clone();
                    cx.edit_config(move |c| c.ai.effort = e);
                    cx.notify(format!("effort: {level} · {what}"));
                } else {
                    self.info.push(format!("effort now: {}. Choose one:", if self.effort.is_empty() { "default" } else { &self.effort }));
                    for (l, what) in EFFORTS {
                        self.info.push(format!("  /effort {l:<10} {what}"));
                    }
                }
            }
            "/perms" if arg == "reset" => {
                let n = self.always.remove(&self.chat.id).map(|s| s.len()).unwrap_or(0);
                cx.notify(if n == 0 { "nothing was always allowed in this chat".to_string() } else { format!("forgot {n} \"always allow\" in this chat: it asks again") });
            }
            // /perms default <mode>: where every new chat starts (saved); /perms <mode>: just this chat
            // (a bare "/perms default" says where they start: it isn't "ask", the way Claude Code names that mode)
            "/perms" if arg.split_whitespace().next().is_some_and(|w| w.eq_ignore_ascii_case("default")) => {
                let mode = arg.get(7..).unwrap_or("").trim();
                match norm_perms(mode) {
                    Some(p) => {
                        self.default_perms = p.to_string();
                        if self.chat.messages.is_empty() {
                            self.perms = p.to_string();
                        }
                        cx.edit_config(move |c| c.ai.perms = p.to_string());
                        cx.notify(format!("new chats start in {p} · saved (settings › AI chat has it too)"));
                    }
                    None if mode.is_empty() => self.info.push(format!("new chats start in {} · /perms default <ask|edits|auto|plan|bypass> changes it", self.default_perms)),
                    None => self.info.push(format!("no mode called {mode} — ask · edits · auto · plan · bypass")),
                }
            }
            "/perms" => match norm_perms(&arg) {
                Some(p) => {
                    self.perms = p.to_string();
                    // this chat only (a running Claude Code hears about it at once)
                    self.push_perms();
                    cx.notify(format!("agent permissions: {p} · this chat (/perms default {p} for every new one)"));
                }
                None => {
                    self.info.push(format!("permissions in this chat: {} · new chats start in {} (/perms default <mode>). Choose one:", self.perms, self.default_perms));
                    for (k, what) in PERMS {
                        self.info.push(format!("  /perms {k:<7} {what}"));
                    }
                    if let Some(a) = self.always.get(&self.chat.id).filter(|a| !a.is_empty()) {
                        self.info.push(format!("always allowed in this chat: {} · /perms reset forgets them", a.iter().cloned().collect::<Vec<_>>().join(", ")));
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
                        let (openai, k) = (p == "openai", key.to_string());
                        cx.edit_config(move |c| {
                            if openai {
                                c.ai.openai_key = k;
                            } else {
                                c.ai.anthropic_key = k;
                            }
                        });
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
                    _ => self.info.push("/key openai <key> · /key anthropic <key>  (saved in oriel's config file; settings › providers & keys types it out of sight)".into()),
                }
            }
            "/note" => match self.chat.messages.iter().rev().find(|m| m.role == "assistant" && !m.content.is_empty()) {
                Some(m) => {
                    // the folder the notes app shows (notes_folder, else oriel's own)
                    let dir = crate::config::notes_dir(cx.config);
                    let _ = std::fs::create_dir_all(&dir);
                    let path = free_path(&dir, &self.chat.title);
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
                    // never over an earlier export: "title 2.md", like /note
                    let path = free_path(&dir, &self.chat.title);
                    match std::fs::write(&path, self.export_md()) {
                        Ok(_) => cx.notify(format!("saved {}", path.display())),
                        Err(e) => self.info.push(format!("couldn't save: {e}")),
                    }
                }
            }
            "/recall" => {
                crate::panes::search::request_query(&arg);
                cx.act(Action::GotoApp("search"));
            }
            "/play" => cx.act(Action::AppKey("music", ' ')),
            "/next" => cx.act(Action::AppKey("music", 'n')),
            "/prev" => cx.act(Action::AppKey("music", 'p')),
            "/music" => cx.act(Action::GotoApp("music")),
            "/sidebar" => cx.act(Action::ToggleSidebar),
            "/icons" => cx.act(Action::ToggleIcons),
            "/settings" => {
                crate::panes::settings::jump(&arg);
                cx.act(Action::GotoApp("settings"));
            }
            "/quit" => cx.act(Action::Quit),
            "/info" => {
                let p = self.provider_of();
                self.info.push(format!("oriel {} · {} ({})", env!("CARGO_PKG_VERSION"), providers::label(&p), self.chat.model.clone().unwrap_or("default model".into())));
                self.info.push(format!("  works in {} · permissions {} · effort {}", self.workdir().display(), self.perms, if self.effort.is_empty() { "default" } else { &self.effort }));
                self.info.push(format!("  {} saved chats · config: {}", self.chats.len(), crate::config::path().display()));
                if let Some(a) = self.always.get(&self.chat.id).filter(|a| !a.is_empty()) {
                    self.info.push(format!("  always allowed in this chat: {} (/perms reset)", a.iter().cloned().collect::<Vec<_>>().join(", ")));
                }
                if !self.parked.is_empty() {
                    self.info.push(format!("  {} more repl{} running in other chats (the spinners in the list)", self.parked.len(), if self.parked.len() == 1 { "y" } else { "ies" }));
                }
            }
            "/delete" => self.confirm_delete = true,
            "/help" => {
                self.info.extend(
                    [
                        "enter send · esc clears the box, then stops the reply · ctrl+g regenerate · ctrl+n new chat · ctrl+d delete chat",
                        "switching chats never stops a reply: it keeps going in the background (a spinner in the list, ? when it needs you)",
                        "pgup/pgdn or the wheel scroll · ctrl+end back to the end · ctrl+home the top · ↑ in an empty box recalls your earlier prompts (then other chats') · ctrl+r searches them",
                        "ctrl+j or shift+enter new line · ↑↓ move between lines · ctrl+←/→ a word · ctrl+w or ctrl+backspace delete a word · ctrl+z undo",
                        "ctrl+y copies the last reply's last code block (or the reply) · click a code block's ┌─ header to copy it · /copy for more",
                        "ctrl+pgup / ctrl+pgdn the previous / next chat · /open searches them · /rename · ctrl+f: the sidebar lists only this folder's chats",
                        "/recall <words> or alt r searches every AI session on this computer",
                        "ctrl+o shows every tool call in full (diffs, output) · click a tool line to open just that one",
                        "/perms ask: Claude Code asks first · y allow · n deny · a always allow that command in this chat",
                        "F1-F9 apps · F12 play/pause · alt p palette · alt n terminal beside this",
                        "F10 opens the full guide: every app, lead mode, troubleshooting",
                    ]
                    .map(String::from),
                );
                for (c, a, d) in COMMANDS {
                    self.info.push(format!("  {c} {a} — {d}"));
                }
            }
            // Claude Code's own /goal, and any /command it knows that oriel doesn't (yours, its skills), go to it
            // as the message
            _ if self.provider_of() == "claude" => self.send(line.trim().to_string(), cx),
            _ => self.info.push(format!("unknown command {cmd} — type / to see them")),
        }
    }

    /// The models the /model menu offers for an AI (the current one first, marked).
    fn models_for(&self, p: &str) -> Vec<(String, String)> {
        let s = |v: &[(&str, &str)]| v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect::<Vec<_>>();
        let mut v = match p {
            "claude" | "codex" | "anthropic" | "openai" => s(known_models(p)),
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
            "/perms" => PERMS.iter().chain(&[("reset", "forget what you said \"always allow\" to in this chat")]).map(|(k, w)| (k.to_string(), w.to_string())).collect(),
            "/effort" => EFFORTS.iter().map(|(k, w)| (k.to_string(), w.to_string())).collect(),
            "/p" => crate::panes::agents::prompts::load().into_iter().map(|p| (p.name, ui::fit(&p.text.replace('\n', " "), 60))).collect(),
            "/settings" => crate::panes::settings::topics(),
            "/copy" => {
                let mut v = vec![("reply".to_string(), "the last reply, as markdown".to_string())];
                if let Some(m) = self.last_reply() {
                    for (n, b) in md::fenced_blocks(&m.content).iter().rev().enumerate() {
                        let what = format!("{} · {}", if b.lang.is_empty() { "code" } else { &b.lang }, ui::fit(b.text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim(), 44));
                        v.push(if n == 0 { ("code".into(), format!("its last code block: {what}")) } else { (format!("code {}", n + 1), what) });
                    }
                }
                v.push(("all".into(), "the whole chat as markdown (what /save writes)".into()));
                v
            }
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
                    left: c.to_string(),
                    args: a.to_string(),
                    desc: d.to_string(),
                    fill: if a.contains('<') { format!("{c} ") } else { c.to_string() },
                    run: !a.contains('<'),
                    put: None,
                })
                .collect(),
            Some((cmd, rest)) => {
                let raw = rest.trim_start();
                let q = raw.to_lowercase();
                // starts with what you typed, or has it anywhere once you've typed two letters
                let hit = |s: &str| {
                    let s = s.to_lowercase();
                    s.starts_with(&q) || (q.chars().count() >= 2 && s.contains(&q))
                };
                // these search as you type, spaces and all
                match cmd {
                    "/open" | "/chats" => {
                        return self.chats.iter().filter(|c| hit(&c.title)).take(200).map(|c| MenuItem::pick(c.title.clone(), self.chat_desc(c), format!("/open {}", c.id), true)).collect();
                    }
                    "/prompts" => {
                        return self
                            .prompt_history()
                            .into_iter()
                            .filter(|(p, _)| hit(p))
                            .take(200)
                            .map(|(p, from)| {
                                let first = p.trim().lines().next().unwrap_or("").to_string();
                                let n = p.trim().lines().count();
                                let more = if n > 1 { format!(" · {n} lines") } else { String::new() };
                                let desc = format!("{}{more}", from.map(|t| format!("from {t}")).unwrap_or_else(|| "this chat".into()));
                                MenuItem { left: first, args: String::new(), desc, fill: String::new(), run: false, put: Some(p) }
                            })
                            .collect();
                    }
                    "/cwd" => return self.cwd_options(raw),
                    "/rewind" | "/diff" => return self.turn_options(cmd).into_iter().filter(|m| hit(&m.left) || hit(m.fill[cmd.len()..].trim_start())).collect(),
                    _ => {}
                }
                if q.contains(' ') {
                    return vec![]; // past the argument (e.g. /key openai sk-...)
                }
                self.arg_options(cmd)
                    .into_iter()
                    .filter(|(v, _)| hit(v))
                    .map(|(v, d)| {
                        // /key still needs the key after the provider
                        let fill = if cmd == "/key" { format!("{cmd} {v} ") } else { format!("{cmd} {v}") };
                        MenuItem::pick(v, d, fill, cmd != "/key")
                    })
                    .collect()
            }
        }
    }

    /// The turns /rewind and /diff can go back to, newest first: your prompt, and what the reply changed.
    fn turn_options(&self, cmd: &str) -> Vec<MenuItem> {
        let mut v = vec![];
        if cmd == "/diff" {
            v.push(MenuItem::pick("all".into(), "everything this chat changed since its first checkpoint".into(), "/diff all".into(), true));
            v.push(MenuItem::pick("last".into(), "what the last turn changed".into(), "/diff last".into(), true));
        }
        for (idx, _) in ckpts(&self.chat).into_iter().rev() {
            let n = turn_no(&self.chat, idx);
            let prompt = self.chat.messages[..idx].iter().rev().find(|m| m.role == "user").map(|m| m.content.trim().lines().next().unwrap_or("").to_string()).unwrap_or_default();
            // "+12 −3 · 2 files" off the reply's note, when it changed anything
            let changed = self.chat.messages[idx].note.as_deref().and_then(|n| n.split(" · ").position(|p| p.starts_with('+')).map(|i| n.split(" · ").skip(i).take(2).collect::<Vec<_>>().join(" · ")));
            let what = if cmd == "/rewind" { "d: its diff · y: the folder back to before it" } else { "its changes and everything after" };
            let desc = match changed {
                Some(c) => format!("{c} · {what}"),
                None => what.to_string(),
            };
            v.push(MenuItem::pick(format!("turn {n} · {}", ui::fit(&prompt, 48)), desc, format!("{cmd} {n}"), true));
        }
        v
    }

    /// A saved chat's line in the /open menu: "yesterday · claude code · 4 messages".
    fn chat_desc(&self, c: &store::Chat) -> String {
        let n = c.messages.iter().filter(|m| m.role == "user").count();
        let here = if c.id == self.chat.id { " · open now" } else { "" };
        format!("{} · {} · {n} message{}{here}", store::bucket(c.updated, store::now(), self.offset), providers::label(&self.provider_for(c)).to_lowercase(), if n == 1 { "" } else { "s" })
    }

    /// Your earlier prompts, newest first: this chat's, then every other chat's (newest chat first), each once,
    /// with the title of the chat it came from when that's another one.
    fn prompt_history(&self) -> Vec<(String, Option<String>)> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = vec![];
        let mine = self.chat.messages.iter().rev().map(|m| (m, None));
        let others = self.chats.iter().filter(|c| c.id != self.chat.id).flat_map(|c| c.messages.iter().rev().map(move |m| (m, Some(c.title.as_str()))));
        for (m, from) in mine.chain(others) {
            let text = m.content.trim();
            if m.role != "user" || text.is_empty() || oriel_wrote(text) || !seen.insert(text.to_string()) {
                continue;
            }
            out.push((m.content.clone(), from.map(String::from)));
            if out.len() >= 300 {
                break;
            }
        }
        out
    }

    /// /cwd's choices: the folder you're typing (and the folders inside it, so tab completes a path a segment at a
    /// time), then folders chats have used.
    fn cwd_options(&self, typed: &str) -> Vec<MenuItem> {
        let mut out: Vec<MenuItem> = vec![];
        let pathy = typed.starts_with(['~', '/', '\\', '.']) || typed.contains(['/', '\\']) || typed.get(1..2) == Some(":");
        if pathy {
            let home = dirs::home_dir().unwrap_or_default().to_string_lossy().to_string();
            let real = |s: &str| if let Some(rest) = s.strip_prefix('~') { format!("{home}{rest}") } else { s.to_string() };
            let sep = typed.chars().rev().find(|c| *c == '/' || *c == '\\').unwrap_or(std::path::MAIN_SEPARATOR);
            // "D:\wo" lists D:\ for names starting "wo"; "D:\work\" lists D:\work
            let cut = typed.rfind(['/', '\\']).map(|i| i + 1).unwrap_or(0);
            let (parent, stem) = typed.split_at(cut);
            if !stem.is_empty() || Path::new(&real(typed)).is_dir() {
                if stem.is_empty() {
                    out.push(MenuItem::pick(typed.to_string(), "this folder".into(), format!("/cwd {typed}"), true));
                } else if Path::new(&real(typed)).is_dir() {
                    out.push(MenuItem::pick(format!("{typed}{sep}"), "this folder".into(), format!("/cwd {typed}{sep}"), true));
                }
            }
            let dir = if parent.is_empty() { ".".to_string() } else { real(parent) };
            let low = stem.to_lowercase();
            let mut subs: Vec<String> = std::fs::read_dir(&dir)
                .map(|rd| {
                    rd.flatten()
                        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .filter(|n| n.to_lowercase().starts_with(&low) && (low.starts_with('.') || !n.starts_with('.')) && n.to_lowercase() != low)
                        .collect()
                })
                .unwrap_or_default();
            subs.sort_by_key(|n| n.to_lowercase());
            for n in subs.into_iter().take(40) {
                let full = format!("{parent}{n}{sep}");
                out.push(MenuItem::pick(full.clone(), "folder · tab goes in".into(), format!("/cwd {full}"), true));
            }
        }
        let q = typed.to_lowercase();
        for (d, what) in self.arg_options("/cwd") {
            let dl = d.to_lowercase();
            if (q.is_empty() || dl.starts_with(&q) || (q.chars().count() >= 2 && dl.contains(&q))) && !out.iter().any(|m| m.left.trim_end_matches(['/', '\\']).eq_ignore_ascii_case(&d)) {
                out.push(MenuItem::pick(d.clone(), what, format!("/cwd {d}"), true));
            }
        }
        out
    }

    /// Where a new agent chat could work: folders chats have used, then git repos under the usual code folders
    /// (found once, in the background: `waker` redraws the picker when they're in).
    fn folder_choices(&self, waker: crate::pane::Waker) -> Vec<(String, &'static str)> {
        let mut v: Vec<(String, &'static str)> = self.arg_options("/cwd").into_iter().map(|(d, _)| (d, "used before")).collect();
        let repos = self.repos.lock().unwrap().clone();
        match repos {
            Some(r) => {
                for d in r {
                    if !v.iter().any(|x| x.0.eq_ignore_ascii_case(&d)) {
                        v.push((d, "git repo"));
                    }
                }
            }
            None if !cfg!(test) => {
                *self.repos.lock().unwrap() = Some(vec![]);
                let into = self.repos.clone();
                std::thread::spawn(move || {
                    let found = find_repos();
                    *into.lock().unwrap() = Some(found);
                    waker.wake();
                });
            }
            None => {}
        }
        v
    }

    fn insert(&mut self, s: &str) {
        self.edit(|e| {
            let mut cs = s.chars();
            match (cs.next(), cs.next()) {
                (Some(c), None) if c != '\n' => e.insert_char(c),
                _ => e.insert_str(s),
            }
        });
        self.menu_sel = 0;
    }

    /// Run an edit or a move on the box through the editor (`input` and `cursor` stay the truth). A recalled
    /// prompt you change is your draft from then on: ↑ / ↓ no longer swap it for another one.
    fn edit<R>(&mut self, f: impl FnOnce(&mut Editor) -> R) -> R {
        self.ed.load(&self.input, self.cursor);
        let r = f(&mut self.ed);
        let text = self.ed.text();
        if text != self.input {
            self.history_pos = None;
            self.history_from = None;
        }
        self.input = text;
        self.cursor = self.ed.char_index();
        r
    }

    /// ↑ / ↓ inside a box of several rows: moves the cursor a row and says true; on the first / last row it's
    /// false (history, scrolling).
    fn row_move(&mut self, dy: isize) -> bool {
        let w = self.comp_w;
        self.edit(|e| {
            let segs = e.layout(w);
            let (v, _) = e.cursor_visual(&segs);
            let room = if dy < 0 { v > 0 } else { v + 1 < segs.len() };
            if room {
                e.vmove(&segs, dy);
            }
            room
        })
    }

    /// Replace what's in the box, the cursor at its end (ctrl+z brings the old text back).
    fn set_input(&mut self, text: String) {
        self.ed.load(&self.input, self.cursor);
        if !self.input.is_empty() {
            self.ed.checkpoint();
        }
        self.cursor = text.chars().count();
        self.input = text;
        self.menu_sel = 0;
    }

    /// ↑ / ↓ through your earlier prompts (`older` = further back). Past the newest, the box empties again.
    fn recall(&mut self, older: bool) -> bool {
        let hist = self.prompt_history();
        let pos = match (self.history_pos, older) {
            (None, true) => 0,
            (None, false) => return false,
            (Some(p), true) => p + 1,
            (Some(0), false) => {
                self.history_pos = None;
                self.history_from = None;
                self.set_input(String::new());
                return true;
            }
            (Some(p), false) => p - 1,
        };
        let Some((text, from)) = hist.get(pos).cloned() else { return self.history_pos.is_some() };
        self.history_pos = Some(pos);
        self.history_from = from;
        self.set_input(text);
        true
    }

    /// ctrl+pgup / ctrl+pgdn: the chat above or below this one in the list (a new chat sits above them all).
    fn step_chat(&mut self, down: bool) {
        let at = self.chats.iter().position(|c| c.id == self.chat.id);
        let next = match (at, down) {
            (None, true) => 0,
            (None, false) => return,
            (Some(0), false) => return self.new_chat(),
            (Some(i), false) => i - 1,
            (Some(i), true) => i + 1,
        };
        if let Some(id) = self.chats.get(next).map(|c| c.id.clone()) {
            self.open_chat(&id);
        }
    }

    /// The newest reply with any text.
    fn last_reply(&self) -> Option<&store::Msg> {
        self.chat.messages.iter().rev().find(|m| m.role == "assistant" && !m.content.trim().is_empty())
    }

    /// ctrl+y: the last reply's last code block, or the whole reply when it has none.
    fn copy_quick(&mut self, cx: &mut Cx) {
        let Some(content) = self.last_reply().map(|m| m.content.clone()) else {
            cx.notify("no reply to copy yet");
            return;
        };
        match md::fenced_blocks(&content).pop() {
            Some(b) => {
                crate::clip::copy(&b.text);
                cx.notify(copied_code(&b));
            }
            None => {
                crate::clip::copy(content.trim());
                cx.notify(format!("copied the last reply ({})", lines_of(content.trim())));
            }
        }
    }

    /// /copy (the last reply as markdown) · /copy code [n] (its nth code block from the end) · /copy all (the chat).
    fn copy_command(&mut self, arg: &str, cx: &mut Cx) {
        let mut a = arg.split_whitespace();
        let (what, n) = (a.next().unwrap_or(""), a.next());
        if what == "all" {
            if self.chat.messages.is_empty() {
                self.info.push("nothing to copy yet".into());
                return;
            }
            let md = self.export_md();
            crate::clip::copy(&md);
            cx.notify(format!("copied the whole chat ({} of markdown)", lines_of(&md)));
            return;
        }
        let Some(content) = self.last_reply().map(|m| m.content.clone()) else {
            self.info.push("no reply to copy yet".into());
            return;
        };
        match what {
            "" | "reply" => {
                crate::clip::copy(content.trim());
                cx.notify(format!("copied the last reply ({})", lines_of(content.trim())));
            }
            "code" => {
                let blocks = md::fenced_blocks(&content);
                let k = n.and_then(|n| n.parse::<usize>().ok()).unwrap_or(1).max(1);
                match blocks.len().checked_sub(k).map(|i| &blocks[i]) {
                    Some(b) => {
                        crate::clip::copy(&b.text);
                        cx.notify(copied_code(b));
                    }
                    None if blocks.is_empty() => self.info.push("the last reply has no code blocks — /copy copies all of it".into()),
                    None => self.info.push(format!("the last reply has {} code block{}", blocks.len(), if blocks.len() == 1 { "" } else { "s" })),
                }
            }
            _ => self.info.push("/copy the last reply · /copy code [n] its last code block (2 = the one before) · /copy all the whole chat".into()),
        }
    }

    /// /lead, /tasks, /task: hand the plan to the agents app (F2) with its form filled in. It goes on the clipboard
    /// too, in case agents wasn't ready to take it (still finding its repo, say).
    fn to_agents(&mut self, cmd: &str, arg: &str, cx: &mut Cx) {
        let text = if arg.is_empty() && cmd != "/task" { self.last_reply().map(|m| m.content.trim().to_string()) } else { Some(arg.trim().to_string()) };
        let Some(text) = text.filter(|t| !t.is_empty()) else {
            self.info.push(match cmd {
                "/task" => "/task <what the agent should do> — a new card on the agents board (F2)".to_string(),
                _ => format!("{cmd} <goal> — or talk it through here first: with no goal it takes the last reply"),
            });
            return;
        };
        let (key, what) = match cmd {
            "/lead" => ('L', "a lead run with this goal · ctrl+s starts it"),
            "/tasks" => ('P', "the planner has the goal · enter splits it into task cards"),
            _ => ('n', "a new task with it · ctrl+s adds it"),
        };
        cx.act(Action::AppKey("agents", key));
        cx.act(Action::AppPaste("agents", text.clone()));
        crate::clip::copy(&text);
        cx.notify(format!("agents: {what} (also on the clipboard)"));
    }

    /// The chat as markdown (/save, /copy all): every message, and each run of tool calls as a folded list.
    fn export_md(&self) -> String {
        fn tool_line(t: &store::Tool, depth: usize, md: &mut String) {
            let target = if t.target.is_empty() { String::new() } else if t.target.contains('`') { format!(" {}", t.target) } else { format!(" `{}`", t.target) };
            let summary = if t.summary.is_empty() { String::new() } else { format!(" · {}", t.summary) };
            let status = if matches!(t.status.as_str(), "error" | "stopped") { format!(" ({})", t.status) } else { String::new() };
            md.push_str(&format!("{}- {}{target}{summary}{status}\n", "  ".repeat(depth), t.label));
            for c in &t.children {
                tool_line(c, depth + 1, md);
            }
        }
        fn fold(tools: &mut Vec<&store::Tool>, md: &mut String) {
            if tools.is_empty() {
                return;
            }
            let names: Vec<&str> = tools.iter().map(|t| t.label.as_str()).collect();
            let mut kinds: Vec<&str> = vec![];
            for n in names {
                if !kinds.contains(&n) {
                    kinds.push(n);
                }
            }
            md.push_str(&format!("<details><summary>{} tool call{}: {}</summary>\n\n", tools.len(), if tools.len() == 1 { "" } else { "s" }, kinds.join(", ")));
            for t in tools.drain(..) {
                tool_line(t, 0, md);
            }
            md.push_str("\n</details>\n\n");
        }
        let mut md = format!("# {}\n\n", self.chat.title);
        for m in &self.chat.messages {
            let who = if m.role == "user" { "you".to_string() } else { providers::label(m.model.as_deref().unwrap_or("ai")).to_lowercase() };
            md.push_str(&format!("**{who}:**\n\n"));
            if m.parts.is_empty() {
                md.push_str(&format!("{}\n\n", m.content.trim_end()));
                continue;
            }
            // text and calls in the order they happened
            let mut tools: Vec<&store::Tool> = vec![];
            for p in &m.parts {
                match p {
                    store::Part::Text { text } if !text.trim().is_empty() => {
                        fold(&mut tools, &mut md);
                        md.push_str(&format!("{}\n\n", text.trim()));
                    }
                    store::Part::Tool(t) => tools.push(t),
                    store::Part::User { text } => {
                        fold(&mut tools, &mut md);
                        md.push_str(&format!("> **you, while it worked:** {}\n\n", text.trim()));
                    }
                    _ => {}
                }
            }
            fold(&mut tools, &mut md);
        }
        md
    }

    // ------------------------------------------------------------ drawing helpers
    /// The transcript as shared blocks: message by message (a finished one drawn once and kept, the live reply a
    /// block at a time), then the info lines. Only the lines on screen ever get copied out of them.
    fn transcript(&mut self, width: usize, cx: &Cx) -> Vec<Block> {
        let mut out = vec![lines_block(vec![Line::raw("")], vec![])];
        self.msg_starts.clear();
        for i in 0..self.chat.messages.len() {
            self.msg_starts.push(out.iter().map(|b| b.lines.len()).sum());
            self.msg_blocks(i, width, cx, &mut out);
        }
        let mut info = vec![];
        for l in &self.info {
            info.extend(md::wrap(vec![Span::styled(l.clone(), ui::muted(cx.theme))], width, "  ", "    "));
        }
        if !info.is_empty() {
            out.push(lines_block(info, vec![]));
        }
        out
    }

    /// Message `i`'s blocks onto `out`.
    fn msg_blocks(&mut self, i: usize, width: usize, cx: &Cx, out: &mut Vec<Block>) {
        if self.stream.is_some() && i + 1 == self.chat.messages.len() {
            // the live reply: whatever hasn't changed since the last frame comes from its block cache
            if self.live.0 != self.chat.id || self.live.1 != i {
                self.live = (self.chat.id.clone(), i, HashMap::new());
            }
            let mut cache = std::mem::take(&mut self.live.2);
            out.extend(self.draw_msg(i, width, cx, Some(&mut cache)));
            self.live.2 = cache;
            return;
        }
        let m = &self.chat.messages[i];
        let key = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (m.content.len(), m.note.as_deref().unwrap_or(""), m.steps.len(), m.parts.len(), width, cx.theme.name.as_str()).hash(&mut h);
            // the check badge: /verify's result, and the check it looks for
            (m.verified.as_deref(), self.chat.extra.get("check").and_then(|v| v["cmd"].as_str())).hash(&mut h);
            h.finish()
        };
        if let Some((k, b)) = self.cache.get(&i) {
            if *k == key {
                out.push(b.clone());
                return;
            }
        }
        let b = join_blocks(self.draw_msg(i, width, cx, None));
        self.cache.insert(i, (key, b.clone()));
        out.push(b);
    }

    /// Draw message `i`. A live reply (`cache` given) comes out as several blocks, its parts cached one by one;
    /// anything else as one.
    fn draw_msg(&self, i: usize, width: usize, cx: &Cx, cache: Option<&mut activity::BlockCache>) -> Vec<Block> {
        let t = cx.theme;
        let m = &self.chat.messages[i];
        let streaming_last = cache.is_some();
        let mut blocks: Vec<Block> = vec![];
        let mut out: Vec<Line<'static>> = vec![];
        let mut hits: Vec<(usize, Hit)> = vec![];
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
                match cache {
                    Some(c) => {
                        blocks.push(lines_block(std::mem::take(&mut out), std::mem::take(&mut hits)));
                        blocks.extend(activity::render_blocks(&m.parts, width, t, &view, Some(c)));
                    }
                    None => activity::render(&m.parts, width, t, &view, &mut out, &mut hits),
                }
            } else if !m.content.is_empty() {
                // a plain reply's markdown, its code blocks clickable (streaming, it's drawn again only as it grows)
                let key = {
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    (&m.content, width, t.name.as_str()).hash(&mut h);
                    h.finish()
                };
                let draw = || {
                    let (lines, codes) = md::render_full(&m.content, width.saturating_sub(1), "  ", t);
                    lines_block(lines, codes.into_iter().map(|(n, c)| (n, Hit::Code(c))).collect())
                };
                let b = match cache {
                    Some(c) => {
                        let b = c.get(&usize::MAX).filter(|(k, _)| *k == key).map(|(_, b)| b.clone()).unwrap_or_else(draw);
                        c.insert(usize::MAX, (key, b.clone()));
                        b
                    }
                    None => draw(),
                };
                blocks.push(lines_block(std::mem::take(&mut out), std::mem::take(&mut hits)));
                blocks.push(b);
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
            // was its work checked? (a finished coding agent's reply that edited files or says it's done)
            if !streaming_last && matches!(m.model.as_deref(), Some("claude" | "codex")) {
                let check = check_of(&self.chat).map(|k| k.cmd);
                if let Some((v, text)) = badge(m, check.as_deref()) {
                    let (glyph, st) = match v {
                        Verdict::Checked | Verdict::Failed => ("", Style::default().fg(if v == Verdict::Checked { t.good } else { t.danger })),
                        Verdict::Stale => ("◌ ", Style::default().fg(WARN)),
                        Verdict::NotRun => ("○ ", ui::muted(t)),
                    };
                    if m.note.is_none() {
                        out.push(Line::raw(""));
                    }
                    out.push(Line::from(Span::styled(ui::fit(&format!("  {glyph}{text}"), width.saturating_sub(1)), st)));
                }
            }
        }
        out.push(Line::raw(""));
        blocks.push(lines_block(out, hits));
        blocks
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
                for l in md::wrap(md::inline(&cur.question, Style::default().add_modifier(Modifier::BOLD), t), width.saturating_sub(6), "", "") {
                    let mut sp = vec![bar.clone()];
                    sp.extend(l.spans);
                    out.push(Line::from(sp));
                }
                let lw = cur.options.iter().map(|o| o.0.width()).max().unwrap_or(0).max(7).min(28);
                let n = cur.options.len();
                // a plan's "Other" is what to change about it
                let other = if cur.header == approve::PLAN_HEADER { ("No, keep planning…", "type what to change") } else { ("Other…", "type your own answer") };
                let other = (other.0.to_string(), other.1.to_string());
                for (i, (label, what)) in cur.options.iter().chain(std::iter::once(&other)).enumerate() {
                    let on = i == self.qs.sel;
                    // (Other has no box to tick, but lines up with the choices that do)
                    let tick = match (cur.multi, i < n) {
                        (true, true) if self.qs.ticked.get(i).copied().unwrap_or(false) => "[x] ",
                        (true, true) => "[ ] ",
                        (true, false) => "    ",
                        _ => "",
                    };
                    let st = if on { Style::default().fg(t.accent).add_modifier(Modifier::BOLD) } else { Style::default() };
                    // `code` in a label shows as code, and a long label is cut so its description still shows
                    let mut lab = md::inline(&ui::fit(&format!("{tick}{label}"), lw + tick.len()), st, t);
                    let used: usize = lab.iter().map(|s| s.content.width()).sum();
                    lab.push(Span::raw(" ".repeat((lw + tick.len()).saturating_sub(used))));
                    let mut row = vec![bar.clone(), Span::styled(if on { " ❯ " } else { "   " }, st), Span::styled(format!("{}. ", i + 1), muted)];
                    row.extend(lab);
                    row.push(Span::styled(format!("  {}", ui::fit(what, width.saturating_sub(lw + 16))), muted));
                    out.push(Line::from(row));
                }
                if !self.input.is_empty() {
                    // you were typing when it asked: the box keeps your words, the question waits
                    out.push(Line::from(vec![bar, Span::styled("esc", ui::bold_accent(t)), Span::styled(" clears the box so you can answer  ", muted), Span::styled("enter", ui::bold_accent(t)), Span::styled(" queues what you typed", muted)]));
                } else if let Some(text) = &self.qs.other {
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
            if !self.input.is_empty() && self.questions.is_empty() {
                // typing isn't answering: y / n / a only count with the box empty
                out.push(Line::from(vec![
                    bar,
                    Span::styled("answer the prompt first: ", Style::default().fg(t.shine)),
                    Span::styled("esc", ui::bold_accent(t)),
                    Span::styled(" clears the box, then y / n / a  ", muted),
                    Span::styled("enter", ui::bold_accent(t)),
                    Span::styled(" queues what you typed", muted),
                ]));
            } else {
                out.push(Line::from(vec![
                    bar,
                    Span::styled("y", ui::bold_accent(t)),
                    Span::styled(" allow  ", muted),
                    Span::styled("n", ui::bold_accent(t)),
                    Span::styled(" deny  ", muted),
                    Span::styled("a", ui::bold_accent(t)),
                    Span::styled(format!(" always allow {} in this chat  ", ui::fit(&a.rule, 40)), muted),
                    Span::styled("esc", ui::bold_accent(t)),
                    Span::styled(" stop", muted),
                ]));
            }
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
            "claude" | "codex" => format!("{} · {} · works in {} ({})", providers::label(&p).to_lowercase(), self.chat.model.clone().unwrap_or("default".into()), self.workdir_label().display(), self.perms),
            _ => format!("{} · {}", providers::label(&p).to_lowercase(), self.chat.model.clone().unwrap_or("default".into())),
        };
        // a coding agent about to start in your home folder or System32: where should it work instead?
        let pick = self.needs_folder();
        let mut folders = if pick { self.folder_choices(cx.waker()) } else { vec![] };
        folders.truncate(8);
        folders.push((self.launch_dir.display().to_string(), "where oriel started · use it anyway"));
        let rows = if pick { folders.len() as u16 + 2 } else { SUGGESTIONS.len() as u16 };
        let block_h = 8 + 2 + 2 + 1 + rows;
        let y0 = area.y + area.height.saturating_sub(block_h) / 2;
        let mut y = y0;
        if area.width > lw + 2 && area.height >= block_h {
            ui::big_logo(f, &logo, area.x + (area.width - lw) / 2, y, t, cx.time);
            y += 10;
        }
        let tx = area.x + area.width.saturating_sub(lw.max(if pick { 64 } else { 40 })) / 2;
        let tw = area.width.saturating_sub(tx - area.x);
        f.render_widget(Paragraph::new(Span::styled(ui::fit(&info, tw as usize), ui::muted(t))), Rect { x: tx, y, width: tw, height: 1 });
        y += 2;
        self.hero_hits.clear();
        if pick {
            let who = providers::label(&p);
            f.render_widget(Paragraph::new(Span::styled(format!("where should {who} work? it can change files there"), Style::default().add_modifier(Modifier::BOLD))), Rect { x: tx, y, width: tw, height: 1 });
            y += 2;
            let lw = folders.iter().map(|(d, _)| d.width()).max().unwrap_or(0).min(tw as usize / 2 + 8);
            for (d, what) in &folders {
                if y + 1 >= area.bottom() {
                    break;
                }
                let r = Rect { x: tx, y, width: tw, height: 1 };
                let shown = ui::fit(d, lw);
                let pad = " ".repeat(lw.saturating_sub(shown.width()) + 2);
                f.render_widget(Paragraph::new(Line::from(vec![Span::styled("› ", ui::accent(t)), Span::raw(shown), Span::raw(pad), Span::styled(*what, ui::muted(t))])), r);
                self.hero_hits.push((r, HeroHit::Folder(d.clone())));
                y += 1;
            }
            if y < area.bottom() {
                let (say, st) = if self.risky_armed {
                    (format!("enter again sends it from {} · or click a folder first", self.launch_dir.display()), Style::default().fg(t.shine).add_modifier(Modifier::BOLD))
                } else {
                    ("click one · or /cwd <folder> (tab completes)".to_string(), ui::muted(t))
                };
                f.render_widget(Paragraph::new(Span::styled(ui::fit(&say, tw as usize), st)), Rect { x: tx, y, width: tw, height: 1 });
            }
            return;
        }
        f.render_widget(Paragraph::new(Span::styled("what can I help with?", Style::default().add_modifier(Modifier::BOLD))), Rect { x: tx, y, width: tw, height: 1 });
        y += 2;
        for s in SUGGESTIONS {
            if y >= area.bottom() {
                break;
            }
            let r = Rect { x: tx, y, width: (s.len() as u16 + 2).min(tw), height: 1 };
            f.render_widget(Paragraph::new(Line::from(vec![Span::styled("› ", ui::accent(t)), Span::raw(*s)])), r);
            self.hero_hits.push((r, HeroHit::Say(s.to_string())));
            y += 1;
        }
    }
}

// ------------------------------------------------------------ checkpoints, the check loop, handoffs
impl Chat {
    /// The chat with this id: the one on screen, or one in the list.
    fn chat_ref(&self, id: &str) -> Option<&store::Chat> {
        if self.chat.id == id { Some(&self.chat) } else { self.chats.iter().find(|c| c.id == id) }
    }

    fn chat_mut(&mut self, id: &str) -> Option<&mut store::Chat> {
        if self.chat.id == id { Some(&mut self.chat) } else { self.chats.iter_mut().find(|c| c.id == id) }
    }

    /// Save chat `id`, on screen or not.
    fn save_chat(&mut self, id: &str) {
        if self.chat.id == id {
            self.persist();
        } else if let Some(c) = self.chats.iter_mut().find(|c| c.id == id) {
            store::save(c);
        }
    }

    /// Run `f` on a background thread; what it returns reaches `drain_jobs`.
    fn spawn_job(&self, cx: &Cx, f: impl FnOnce() -> Job + Send + 'static) {
        let (jobs, waker) = (self.jobs.clone(), cx.waker());
        std::thread::spawn(move || {
            let j = f();
            jobs.lock().unwrap().push(j);
            waker.wake();
        });
    }

    fn drain_jobs(&mut self, cx: &mut Cx) {
        let jobs: Vec<Job> = std::mem::take(&mut *self.jobs.lock().unwrap());
        for j in jobs {
            match j {
                Job::Turn { chat, idx, stat, tree, check, verify } => self.on_turn(&chat, idx, stat, tree, check, verify, cx),
                Job::Diff { chat, idx, ckpt, r } => {
                    if let Some(v) = self.review.as_mut().filter(|v| v.chat == chat && v.idx == idx && v.ckpt == ckpt) {
                        v.data = Some(r);
                    }
                }
                Job::Plan { chat, idx, ckpt, r } => {
                    if !self.restore.as_ref().is_some_and(|x| x.chat == chat && x.plan.is_none()) {
                        continue; // cancelled meanwhile
                    }
                    let turn = self.chat_ref(&chat).map(|c| turn_no(c, idx)).unwrap_or(0);
                    match r {
                        Ok(p) if p.write.is_empty() && p.delete.is_empty() => {
                            self.restore = None;
                            self.info.push(format!("nothing to change: the folder is already as it was before turn {turn}"));
                        }
                        Ok(p) => {
                            if let Some(rs) = self.restore.as_mut() {
                                rs.idx = idx;
                                rs.ckpt = ckpt;
                                rs.plan = Some(Ok(p));
                            }
                            self.scroll = 0;
                        }
                        Err(e) => {
                            self.restore = None;
                            self.info.push(format!("can't go back: {e}"));
                        }
                    }
                }
                Job::Restored { chat, idx, r } => {
                    if self.restore.as_ref().is_some_and(|x| x.chat == chat) {
                        self.restore = None;
                    }
                    let turn = self.chat_ref(&chat).map(|c| turn_no(c, idx)).unwrap_or(0);
                    let say = match r {
                        Ok(p) => format!("the folder is back to before turn {turn}: {} restored, {} deleted", count(p.write.len(), "file", "files"), count(p.delete.len(), "new file", "new files")),
                        Err(e) => format!("couldn't put the folder back: {e}"),
                    };
                    if chat == self.chat.id {
                        self.info.push(say.clone());
                    }
                    cx.notify(say);
                }
            }
        }
    }

    /// A reply just ended in chat `id`: what it changed goes on its note, and a check loop runs its check (not
    /// after a reply that failed or that you stopped: `finished` = it ended by itself).
    fn turn_ended(&mut self, id: &str, finished: bool, cx: &mut Cx) {
        let Some(c) = self.chat_ref(id) else { return };
        let Some(idx) = c.messages.len().checked_sub(1) else { return };
        if c.messages[idx].role != "assistant" {
            return;
        }
        let (ckpt, check) = (c.messages[idx].ckpt.clone(), self.loop_check(c).filter(|_| finished));
        if ckpt.is_none() && check.is_none() {
            return;
        }
        let dir = self.workdir_for(c);
        self.run_turn_job(id, idx, dir, ckpt, check, false, cx);
    }

    /// The command chat `c`'s check loop runs, when it has one on (Claude Code and Codex only).
    fn loop_check(&self, c: &store::Chat) -> Option<String> {
        if !matches!(self.provider_for(c).as_str(), "claude" | "codex") {
            return None;
        }
        check_of(c).filter(|k| k.status == "on").map(|k| k.cmd)
    }

    /// In the background: the folder now, what changed since `ckpt`, and the check's result.
    #[allow(clippy::too_many_arguments)]
    fn run_turn_job(&mut self, id: &str, idx: usize, dir: PathBuf, ckpt: Option<String>, check: Option<String>, verify: bool, cx: &Cx) {
        if check.is_some() {
            self.checking.insert(id.to_string());
        }
        let timeout = Duration::from_secs(cx.config.lead.gate_timeout_s.max(30) as u64);
        let (chat, want_tree) = (id.to_string(), ckpt.is_some() || (check.is_some() && !verify));
        self.spawn_job(cx, move || {
            let place = if want_tree && ckpt_allowed(&dir) { ckpt::Place::of(&dir).ok() } else { None };
            let tree = place.as_ref().and_then(|p| ckpt::tree(p, &ckpt::CAPS).ok());
            let stat = match (&place, &ckpt, &tree) {
                (Some(p), Some(c), Some(t)) => ckpt::stat(p, c, t).ok(),
                _ => None,
            };
            let check = check.map(|cmd| {
                let r = git::run_gate(&dir, &cmd, timeout, &[]);
                (cmd, r)
            });
            Job::Turn { chat, idx, stat, tree, check, verify }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn on_turn(&mut self, id: &str, idx: usize, stat: Option<ckpt::Stats>, tree: Option<String>, check: Option<(String, Result<(), String>)>, verify: bool, cx: &mut Cx) {
        use crate::alerts::Kind;
        self.checking.remove(id);
        let stamp = clock_hm();
        let (title, edited, looping) = {
            let Some(c) = self.chat_mut(id) else { return };
            let looping = check_of(c).filter(|k| k.status == "on");
            let Some(m) = c.messages.get_mut(idx).filter(|m| m.role == "assistant") else { return };
            // "+212 −40 · 6 files" on the reply's note
            if let Some(s) = stat.as_ref().filter(|s| s.files > 0) {
                let add = format!("+{} −{} · {}", s.added, s.removed, count(s.files as usize, "file", "files"));
                m.note = Some(match m.note.take().filter(|n| !n.is_empty()) {
                    Some(n) if n.contains(&add) => n,
                    Some(n) => format!("{n} · {add}"),
                    None => add,
                });
            }
            if let Some((cmd, r)) = &check {
                m.verified = Some(match r {
                    Ok(()) => format!("✓ checked {stamp} · {cmd}"),
                    Err(e) => format!("✗ {} at {stamp} · {cmd}", exit_of(e)),
                });
            }
            let edited = activity_edits(&m.parts) > 0;
            (c.title.clone(), edited, looping)
        };
        self.save_chat(id);
        let on_screen = id == self.chat.id;
        let Some((cmd, r)) = check else { return };
        if verify {
            let line = match &r {
                Ok(()) => format!("✓ {cmd} passes"),
                Err(e) => format!("✗ {}", e.lines().next().unwrap_or("")),
            };
            if on_screen { self.info.push(line) } else { cx.notify(format!("{line} · {title}")) }
            return;
        }
        let Some(mut ck) = looping.filter(|k| k.cmd == cmd) else { return };
        ck.turn += 1;
        let mut next = None;
        match &r {
            Ok(()) => {
                ck.status = "passed".into();
                ck.last = "passed".into();
                cx.alert(Kind::AgentDone, format!("check passed after {}: {cmd} · {title}", count(ck.turn as usize, "check", "checks")));
                if on_screen {
                    self.info.push(format!("◎ {cmd} passes: the check loop is done"));
                }
            }
            Err(e) => {
                ck.last = exit_of(e);
                // progress: it edited something, or the folder changed some other way
                let progressed = edited || tree.as_ref().is_some_and(|t| *t != ck.tree);
                if let Some(t) = &tree {
                    ck.tree = t.clone();
                }
                ck.idle = if progressed { 0 } else { ck.idle + 1 };
                let stop = if git::shell_trouble(e) {
                    ck.status = "broken".into();
                    Some(format!("the check command didn't run: {}", e.lines().nth(1).or(e.lines().next()).unwrap_or("").trim()))
                } else if ck.idle >= 3 {
                    ck.status = "stalled".into();
                    Some("check loop stalled: 3 turns without a change".to_string())
                } else if ck.turn >= ck.max {
                    ck.status = "capped".into();
                    Some(format!("check still failing after {} turns: {cmd}", ck.turn))
                } else {
                    next = Some(not_yet(&cmd, e, ck.turn, ck.max));
                    None
                };
                if let Some(s) = stop {
                    cx.alert(Kind::NeedsYou, format!("{s} · {title}"));
                    if on_screen {
                        self.info.push(format!("◎ {s} · /check to start again"));
                    }
                }
            }
        }
        if let Some(c) = self.chat_mut(id) {
            set_check(c, &ck);
        }
        self.save_chat(id);
        if let Some(text) = next {
            self.send_to(id, text, cx);
        }
    }

    /// The check a chat runs when /check or /verify doesn't name one: the one last used in this folder, else the
    /// configured gate, else a compile check oriel can tell from the project.
    fn default_check(&self, dir: &Path, cx: &Cx) -> Option<String> {
        remembered_check(dir)
            .or_else(|| {
                let g = cx.config.lead.gate.trim();
                (!g.is_empty() && g != "none" && g != "off").then(|| g.to_string())
            })
            .or_else(|| git::detect_gate(dir))
    }

    /// /check [cmd|off]: after each turn oriel runs the check itself; a failure goes back as the next message
    /// until it passes, nothing changes for 3 turns, or it has tried CHECK_TURNS times.
    fn check_command(&mut self, arg: &str, cx: &mut Cx) {
        if !matches!(self.provider_of().as_str(), "claude" | "codex") {
            self.info.push("/check runs a command after each turn of a coding agent (Claude Code or Codex) — /provider claude or codex first".into());
            return;
        }
        let cur = check_of(&self.chat);
        let on = cur.as_ref().is_some_and(|k| k.status == "on");
        // "/check on" is the bare /check, not a command called "on"
        let arg = if arg.eq_ignore_ascii_case("on") { "" } else { arg };
        match arg {
            "off" | "stop" => {
                if let Some(mut k) = cur.filter(|_| on) {
                    k.status = "off".into();
                    set_check(&mut self.chat, &k);
                    self.persist();
                    cx.notify("check loop off");
                } else {
                    self.info.push("no check loop is on in this chat".into());
                }
            }
            "" if on => {
                let k = cur.unwrap_or_default();
                self.info.push(format!("◎ checking with {} after each turn · {} of {} so far{} · /check off stops it", k.cmd, k.turn, k.max, if k.last.is_empty() { String::new() } else { format!(" · last: {}", k.last) }));
            }
            _ => {
                let dir = self.workdir();
                let Some(cmd) = (if arg.is_empty() { self.default_check(&dir, cx) } else { Some(arg.to_string()) }) else {
                    self.info.push("/check <command> — e.g. /check cargo test: it runs after each turn, and a failure goes back to the agent until it passes".into());
                    return;
                };
                remember_check(&dir, &cmd);
                set_check(&mut self.chat, &Check { cmd: cmd.clone(), turn: 0, max: CHECK_TURNS, status: "on".into(), ..Default::default() });
                self.persist();
                self.info.push(format!("◎ check on: {cmd} runs after each turn (up to {CHECK_TURNS}) until it passes; a failure goes back as your next message · /check off stops it"));
                // a reply is already there and nothing's running: check it now
                if let Some(idx) = self.chat.messages.iter().rposition(|m| m.role == "assistant").filter(|_| self.stream.is_none()) {
                    self.run_turn_job(&self.chat.id.clone(), idx, dir, None, Some(cmd), false, cx);
                }
            }
        }
    }

    /// /verify [cmd]: run the check now; the last reply's badge says what it found.
    fn verify_command(&mut self, arg: &str, cx: &mut Cx) {
        let dir = self.workdir();
        let cmd = if arg.is_empty() { check_of(&self.chat).filter(|k| k.status == "on").map(|k| k.cmd).or_else(|| self.default_check(&dir, cx)) } else { Some(arg.to_string()) };
        let Some(cmd) = cmd else {
            self.info.push("/verify <command> — e.g. /verify cargo test (next time it remembers it for this folder)".into());
            return;
        };
        if self.stream.is_some() {
            self.info.push("the reply is still running: /verify when it's done (or esc it first)".into());
            return;
        }
        let Some(idx) = self.chat.messages.iter().rposition(|m| m.role == "assistant") else {
            self.info.push("no reply to verify yet".into());
            return;
        };
        if !arg.is_empty() {
            remember_check(&dir, &cmd);
        }
        self.info.push(format!("◎ running {cmd}…"));
        self.run_turn_job(&self.chat.id.clone(), idx, dir, None, Some(cmd), true, cx);
    }

    /// /handoff [goal] [ai]: the AI writes a handoff card now; when it's done a fresh chat starts from it
    /// (handoff_ready), with the same AI or the one named.
    fn handoff(&mut self, arg: &str, cx: &mut Cx) {
        if !self.chat.messages.iter().any(|m| m.role == "assistant") {
            self.info.push("nothing to hand off yet: /handoff carries a long chat over to a fresh one".into());
            return;
        }
        if self.stream.is_some() {
            self.info.push("the reply is still running: /handoff when it's done (or esc it first)".into());
            return;
        }
        let mut words: Vec<&str> = arg.split_whitespace().collect();
        let mut to = None;
        if let Some(id) = words.last().map(|w| w.to_lowercase()).filter(|w| providers::is_known(w)) {
            if !self.avail.contains(&id.as_str()) {
                self.info.push(format!("{} isn't set up on this computer", providers::label(&id)));
                return;
            }
            to = Some(id);
            words.pop();
        }
        let goal = words.join(" ");
        let ask = if goal.is_empty() { HANDOFF_ASK.to_string() } else { format!("{HANDOFF_ASK}\n\nThe next session's goal: {goal}. Shape **Next** around it.") };
        let who = providers::label(to.as_deref().unwrap_or(&self.provider_of())).to_lowercase();
        self.handoffs.insert(self.chat.id.clone(), Handoff { goal, provider: to });
        self.send(ask, cx);
        self.info.push(format!("writing a handoff card: a fresh {who} chat starts from it when it's done"));
    }

    /// A reply ended in chat `id`: if it was a /handoff card, a fresh chat starts from it (on screen if this one
    /// was, else in the background).
    fn handoff_ready(&mut self, id: &str, failed: bool, cx: &mut Cx) {
        if !self.handoffs.contains_key(id) || (id == self.chat.id && self.stream.is_some()) || self.parked.contains_key(id) {
            return; // not one, or a queued message is being answered first
        }
        let Some(h) = self.handoffs.remove(id) else { return };
        let Some(parent) = self.chat_ref(id).cloned() else { return };
        // the reply to the card request (a message you queued meanwhile may have been answered after it)
        let asked = parent.messages.iter().rposition(|m| m.role == "user" && m.content.starts_with(HANDOFF_ASK));
        let card = asked.and_then(|i| parent.messages.get(i + 1)).filter(|m| m.role == "assistant").map(|m| m.content.trim().to_string()).unwrap_or_default();
        if failed || card.is_empty() {
            self.info.push("the handoff card didn't come back: /handoff again to retry".into());
            return;
        }
        let p = h.provider.clone().unwrap_or_else(|| self.provider_for(&parent));
        let mut child = store::Chat::new(&p);
        child.model = self.models.get(&p).cloned().filter(|m| !m.is_empty());
        child.cwd = parent.cwd.clone();
        child.title = parent.title.clone();
        child.extra.insert("parent".into(), serde_json::Value::String(parent.id.clone()));
        let next = if h.goal.is_empty() { "Read it, then say in two or three lines where things stand and what you'd do first. Don't change anything yet.".to_string() } else { format!("Next: {}", h.goal) };
        let first = format!("{HANDOFF_FROM} (\"{}\"):\n\n{card}\n\n{next}", parent.title);
        let who = providers::label(&p).to_lowercase();
        if id == self.chat.id {
            self.persist_if_changed();
            self.chat = child;
            self.fresh_view();
            self.send(first, cx);
            self.chat.title = parent.title.clone();
            self.info.push(format!("↳ handed off from \"{}\" (still in the list) · {who} starts from the card", parent.title));
        } else {
            let cid = child.id.clone();
            self.chats.insert(0, child);
            self.send_to(&cid, first, cx);
            cx.alert(crate::alerts::Kind::AgentDone, format!("handed off: {who} continues \"{}\" in a fresh chat", parent.title));
        }
    }

    /// /diff [last|<turn>]: the diff view, from a checkpoint to the folder as it is now.
    fn diff_command(&mut self, arg: &str, cx: &mut Cx) {
        let cks = ckpts(&self.chat);
        let Some(first) = cks.first().cloned() else {
            self.info.push(NO_CKPT.into());
            return;
        };
        let pick = match arg.trim() {
            "" | "all" => Some(first),
            "last" => cks.last().cloned(),
            n => n.trim_start_matches("turn").trim().parse::<usize>().ok().and_then(|t| cks.iter().find(|(i, _)| turn_no(&self.chat, *i) == t).cloned()),
        };
        match pick {
            Some((idx, sha)) => self.open_review(idx, sha, cx),
            None => self.info.push(format!("no checkpoint for turn {arg} — /rewind lists the ones there are")),
        }
    }

    fn open_review(&mut self, idx: usize, sha: String, cx: &mut Cx) {
        let turn = turn_no(&self.chat, idx);
        let first = ckpts(&self.chat).first().is_some_and(|f| f.0 == idx);
        let title = if first { format!("everything this chat changed (since turn {turn})") } else { format!("changes since turn {turn}") };
        self.review = Some(Review { chat: self.chat.id.clone(), idx, ckpt: sha.clone(), title, data: None, file: 0, scroll: 0, hits: vec![] });
        let (dir, chat) = (self.workdir(), self.chat.id.clone());
        self.spawn_job(cx, move || {
            let r = ckpt::Place::of(&dir).and_then(|p| {
                if !ckpt::exists(&p, &sha) {
                    return Err("that checkpoint isn't in this folder's history (was the chat's folder changed?)".into());
                }
                let now = ckpt::tree(&p, &ckpt::CAPS)?;
                ckpt::diff(&p, &sha, &now)
            });
            Job::Diff { chat, idx, ckpt: sha, r }
        });
    }

    /// /undo: back to before the last turn that changed anything (asks first).
    fn undo_command(&mut self, cx: &mut Cx) {
        let cks = ckpts(&self.chat);
        if cks.is_empty() {
            self.info.push(NO_CKPT.into());
        } else if self.stream.is_some() {
            self.info.push("the reply is still changing files: stop it first (esc), then /undo".into());
        } else {
            self.start_restore(cks.into_iter().rev().collect(), cx);
        }
    }

    /// /rewind <turn>: back to before that turn (asks first; d shows the diff instead).
    fn rewind_command(&mut self, arg: &str, cx: &mut Cx) {
        let cks = ckpts(&self.chat);
        if cks.is_empty() {
            self.info.push(NO_CKPT.into());
            return;
        }
        let t = arg.trim().trim_start_matches("turn").trim();
        let Some(ck) = t.parse::<usize>().ok().and_then(|t| cks.iter().find(|(i, _)| turn_no(&self.chat, *i) == t).cloned()) else {
            self.info.push("/rewind <turn> — type /rewind and a space: your turns are listed (↑↓ enter)".into());
            return;
        };
        if self.stream.is_some() {
            self.info.push("the reply is still changing files: stop it first (esc)".into());
            return;
        }
        self.start_restore(vec![ck], cx);
    }

    /// Work out what going back would do, then ask. `cands` newest first: the first that differs from now wins
    /// (so /undo twice goes back two turns).
    fn start_restore(&mut self, cands: Vec<(usize, String)>, cx: &mut Cx) {
        let Some((idx, sha)) = cands.first().cloned() else { return };
        self.review = None;
        self.restore = Some(Restore { chat: self.chat.id.clone(), idx, ckpt: sha.clone(), plan: None, busy: false });
        let (dir, chat, single) = (self.workdir(), self.chat.id.clone(), cands.len() == 1);
        self.spawn_job(cx, move || {
            let r = ckpt::Place::of(&dir).and_then(|p| {
                let now = ckpt::tree(&p, &ckpt::CAPS)?;
                for (i, c) in &cands {
                    let plan = ckpt::plan(&p, c, &now)?;
                    if single || !plan.write.is_empty() || !plan.delete.is_empty() {
                        return Ok((*i, c.clone(), plan));
                    }
                }
                Err("the folder is already as it was before every turn this chat has a checkpoint for".to_string())
            });
            match r {
                Ok((idx, ckpt, plan)) => Job::Plan { chat, idx, ckpt, r: Ok(plan) },
                Err(e) => Job::Plan { chat, idx, ckpt: sha, r: Err(e) },
            }
        });
    }

    /// y on a restore: do it.
    fn confirm_restore(&mut self, cx: &mut Cx) {
        let dir = self.workdir();
        let Some(r) = self.restore.as_mut() else { return };
        let Some(Ok(plan)) = r.plan.clone() else { return };
        r.busy = true;
        let (chat, idx, sha) = (r.chat.clone(), r.idx, r.ckpt.clone());
        self.spawn_job(cx, move || {
            let r = ckpt::Place::of(&dir).and_then(|p| ckpt::restore(&p, &sha, &plan)).map(|_| plan);
            Job::Restored { chat, idx, r }
        });
    }

    /// The restore prompt above the box: what would come back and what would go.
    fn restore_lines(&self, width: usize, cx: &Cx) -> Vec<Line<'static>> {
        let t = cx.theme;
        let Some(r) = self.restore.as_ref().filter(|r| r.chat == self.chat.id) else { return vec![] };
        let bar = Span::styled("  ▌ ", Style::default().fg(t.shine));
        let muted = ui::muted(t);
        let turn = turn_no(&self.chat, r.idx);
        let mut out = vec![Line::raw("")];
        let Some(Ok(plan)) = &r.plan else {
            out.push(Line::from(vec![bar, Span::styled(format!("{} working out what going back to before turn {turn} would change…", SPIN[(cx.time * 10.0) as usize % SPIN.len()]), muted)]));
            return out;
        };
        let prompt = self.chat.messages[..r.idx.min(self.chat.messages.len())].iter().rev().find(|m| m.role == "user").map(|m| m.content.lines().next().unwrap_or("").to_string()).unwrap_or_default();
        out.push(Line::from(vec![
            bar.clone(),
            Span::styled("put the folder back to before turn ", Style::default().fg(t.shine).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{turn}"), Style::default().fg(t.accent).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" ({}) ?", ui::fit(&prompt, width.saturating_sub(48))), Style::default().fg(t.fg)),
        ]));
        let back = if plan.write.len() == 1 { "comes back as it was" } else { "come back as they were" };
        out.push(Line::from(vec![bar.clone(), Span::styled(format!("  {} {back}", count(plan.write.len(), "file", "files")), muted)]));
        if !plan.delete.is_empty() {
            let names = plan.delete.iter().take(6).cloned().collect::<Vec<_>>().join(", ");
            let more = if plan.delete.len() > 6 { format!(" and {} more", plan.delete.len() - 6) } else { String::new() };
            let gone = if plan.delete.len() == 1 { "made since is deleted" } else { "made since are deleted" };
            out.push(Line::from(vec![bar.clone(), Span::styled(format!("  {} {gone}: ", count(plan.delete.len(), "file", "files")), Style::default().fg(t.danger)), Span::styled(ui::fit(&format!("{names}{more}"), width.saturating_sub(40)), Style::default().fg(t.fg))]));
        }
        // another reply working in the same folder would carry on over the top of it
        let here = self.workdir_label();
        for (id, _) in self.parked.iter() {
            if let Some(c) = self.chats.iter().find(|c| c.id == *id && c.cwd.as_deref().map(PathBuf::from).as_ref() == Some(&here)) {
                out.push(Line::from(vec![bar.clone(), Span::styled(format!("  ⚠ \"{}\" is still running in this folder", c.title), Style::default().fg(t.shine))]));
            }
        }
        if r.busy {
            out.push(Line::from(vec![bar, Span::styled("putting it back…", muted)]));
        } else {
            out.push(Line::from(vec![bar, Span::styled("y", ui::bold_accent(t)), Span::styled(" put it back  ", muted), Span::styled("d", ui::bold_accent(t)), Span::styled(" show the diff first  ", muted), Span::styled("esc", ui::bold_accent(t)), Span::styled(" leave it", muted)]));
        }
        out
    }

    /// Keys in the diff view.
    fn review_key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        let Some(v) = self.review.as_mut() else { return false };
        let n = v.data.as_ref().and_then(|d| d.as_ref().ok()).map(|d| d.len()).unwrap_or(0);
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => self.review = None,
            KeyCode::Up | KeyCode::Char('k') => {
                v.file = v.file.saturating_sub(1);
                v.scroll = 0;
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                v.file = (v.file + 1).min(n.saturating_sub(1));
                v.scroll = 0;
            }
            KeyCode::PageUp => v.scroll = v.scroll.saturating_sub(20),
            KeyCode::PageDown | KeyCode::Char(' ') => v.scroll += 20,
            KeyCode::Home => v.scroll = 0,
            KeyCode::Char('u') => {
                let ck = (v.idx, v.ckpt.clone());
                if self.stream.is_some() {
                    self.info.push("the reply is still changing files: stop it first (esc)".into());
                    self.review = None;
                } else {
                    self.start_restore(vec![ck], cx);
                }
            }
            KeyCode::Char('r') => {
                let (idx, sha) = (v.idx, v.ckpt.clone());
                self.open_review(idx, sha, cx);
            }
            // anything else (F-keys, alt) goes to the app
            _ => return false,
        }
        true
    }

    /// The diff view, over the whole pane.
    fn draw_review(&mut self, f: &mut Frame, area: Rect, cx: &Cx) {
        let t = cx.theme;
        let hints = [("↑↓", "file"), ("pgup/pgdn", "scroll"), ("u", "put the folder back to before it"), ("r", "reload"), ("esc", "back to the chat")];
        let body = ui::hint_line(f, area, &hints, t);
        let body = Rect { x: body.x + 1, width: body.width.saturating_sub(2), ..body };
        let Some(v) = self.review.as_mut() else { return };
        let mut right = vec![];
        match &v.data {
            None => right.push(Span::styled(format!("{} reading the diff", SPIN[(cx.time * 10.0) as usize % SPIN.len()]), ui::muted(t))),
            Some(Err(e)) => right.push(Span::styled(e.clone(), Style::default().fg(t.danger))),
            Some(Ok(d)) => {
                let (a, r) = review::totals(d);
                right.extend([Span::styled(format!("+{a}"), Style::default().fg(t.good).add_modifier(Modifier::BOLD)), Span::styled(format!(" −{r}"), Style::default().fg(t.danger).add_modifier(Modifier::BOLD)), Span::styled(format!(" · {}", count(d.len(), "file", "files")), ui::muted(t))]);
            }
        }
        let left = format!("◆ {}", v.title);
        let rw: usize = right.iter().map(|s| s.content.width()).sum();
        let lw = (body.width as usize).saturating_sub(rw + 1);
        let mut head = vec![Span::styled(ui::fit(&left, lw), Style::default().fg(t.fg).add_modifier(Modifier::BOLD))];
        head.push(Span::raw(" ".repeat(lw.saturating_sub(ui::fit(&left, lw).width()) + 1)));
        head.extend(right);
        f.render_widget(Paragraph::new(Line::from(head)), Rect { height: 1, ..body });
        let main = Rect { y: body.y + 2, height: body.height.saturating_sub(2), ..body };
        v.hits.clear();
        if let Some(Ok(d)) = &v.data {
            v.hits = review::draw(f, main, d, &mut v.file, &mut v.scroll, t);
        }
    }

    /// "◎ check · turn 4/20 · last: exit 101" for the box's top edge, while a check loop is on.
    fn check_status(&self) -> Option<String> {
        let k = check_of(&self.chat).filter(|k| k.status == "on")?;
        if self.checking.contains(&self.chat.id) {
            return Some(format!("◎ running {}…", k.cmd));
        }
        let last = if k.last.is_empty() { String::new() } else { format!(" · last: {}", k.last) };
        Some(format!("◎ check · {}/{}{last}", k.turn, k.max))
    }

    /// How full the context is on this chat's AI: (used, window), from its latest reply.
    fn context_fill(&self) -> Option<(u64, u64)> {
        let p = self.provider_of();
        let last = self.chat.messages.iter().rev().find(|m| m.role == "assistant")?;
        if last.model.as_deref() != Some(p.as_str()) {
            return None; // another AI wrote the last reply: its numbers aren't this one's
        }
        self.chat.messages.iter().rev().filter(|m| m.role == "assistant" && m.model.as_deref() == Some(p.as_str())).find_map(|m| m.ctx).filter(|c| c.1 > 0)
    }

    /// "context 62% · /handoff?" once the context is past 60% or the conversation was compacted. Only a hint.
    fn handoff_hint(&self) -> Option<String> {
        if self.stream.is_some() || self.handoffs.contains_key(&self.chat.id) {
            return None;
        }
        let pct = self.context_fill().map(|(u, w)| u * 100 / w).unwrap_or(0);
        if pct >= 60 {
            return Some(format!("context {pct}% · /handoff?"));
        }
        // the AI's 5-hour window nearly used up: another coding agent with room could carry on
        let p = self.provider_of();
        let five = self.usage.lock().unwrap().iter().filter(|u| u.0 == p && u.1 == "5-hour").map(|u| u.2).fold(0.0, f64::max);
        if five >= 90.0 && self.chat.messages.iter().any(|m| m.role == "assistant") {
            if let Some(to) = self.roomiest_other(&p) {
                return Some(format!("5h {five:.0}% · /handoff {to}?"));
            }
        }
        let last = self.chat.messages.iter().rev().find(|m| m.role == "assistant")?;
        last.parts.iter().any(|p| matches!(p, store::Part::Mark { text } if text.contains("compacted"))).then(|| "compacted · /handoff?".to_string())
    }

    /// The other coding agent set up here with the most room left in its plan windows (for a handoff).
    fn roomiest_other(&self, p: &str) -> Option<&'static str> {
        let u = self.usage.lock().unwrap();
        let fullest = |a: &str| u.iter().filter(|w| w.0 == a).map(|w| w.2).fold(0.0, f64::max);
        ["claude", "codex"].into_iter().filter(|a| *a != p && self.avail.contains(a)).min_by(|a, b| fullest(a).partial_cmp(&fullest(b)).unwrap_or(std::cmp::Ordering::Equal))
    }
}

/// Said when a chat has no checkpoints to diff or go back to.
const NO_CKPT: &str = "no checkpoints in this chat yet: Claude Code and Codex chats take one before each reply (in a git repo, or a shadow copy for other folders)";

/// Checkpoints are taken for a coding agent's folder unless it's your home folder, a drive root or the system
/// (too big, and not a project). Tests only ever snapshot their own scratch folders.
fn ckpt_allowed(dir: &Path) -> bool {
    if cfg!(test) {
        let scratch = std::path::absolute("target/test-scratch").unwrap_or_default();
        return std::path::absolute(dir).is_ok_and(|d| d.starts_with(&scratch));
    }
    !risky_dir(dir)
}

/// The replies in a chat that have a checkpoint, oldest first: (message index, commit).
fn ckpts(c: &store::Chat) -> Vec<(usize, String)> {
    c.messages.iter().enumerate().filter_map(|(i, m)| m.ckpt.clone().filter(|_| m.role == "assistant").map(|s| (i, s))).collect()
}

/// Which of your turns message `idx` answers (1 = the first).
fn turn_no(c: &store::Chat, idx: usize) -> usize {
    c.messages[..idx.min(c.messages.len())].iter().filter(|m| m.role == "user").count()
}

/// "1 file", "3 files".
fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// "14:02", local time.
fn clock_hm() -> String {
    let l = crate::panes::files::clock::local(crate::panes::files::clock::now_secs());
    format!("{:02}:{:02}", l.hour, l.min)
}

/// "exit 101" / "timed out" out of a check's failure.
fn exit_of(e: &str) -> String {
    if e.contains("timed out") {
        return "timed out".into();
    }
    e.split("(exit ").nth(1).and_then(|r| r.split(')').next()).map(|n| format!("exit {n}")).unwrap_or_else(|| "failed".into())
}

/// How the check loop's messages and a handoff's first message start.
const CHECK_SAYS: &str = "◎ check ";
const HANDOFF_FROM: &str = "Handoff from an earlier session";

/// The check loop's next message after a failure: what failed, the first lines, and what to do.
fn not_yet(cmd: &str, err: &str, turn: u32, max: u32) -> String {
    let (head, rest) = err.split_once('\n').unwrap_or((err, ""));
    let body = if rest.trim().is_empty() { String::new() } else { format!("\n\n```\n{}\n```", rest.trim_end()) };
    format!("{CHECK_SAYS}{turn} of {max}: not yet — {}{body}\n\nFix it, then end your turn: oriel runs {cmd} again when you stop.", head.trim_end_matches(':'))
}

/// A message oriel sent in your name (a check loop's "not yet", a handoff): not one of your prompts to recall.
fn oriel_wrote(text: &str) -> bool {
    text.starts_with(CHECK_SAYS) || text.starts_with(HANDOFF_ASK) || text.starts_with(HANDOFF_FROM)
}

/// Files a reply's calls edited, written or deleted.
fn activity_edits(parts: &[store::Part]) -> usize {
    tools_in(parts).iter().filter(|t| is_edit(t)).count()
}

fn is_edit(t: &store::Tool) -> bool {
    matches!(t.label.as_str(), "Update" | "Write" | "Delete" | "Notebook") && t.status == "done"
}

/// Every call in a reply, subagents' included, in order.
fn tools_in(parts: &[store::Part]) -> Vec<&store::Tool> {
    fn walk<'a>(t: &'a store::Tool, out: &mut Vec<&'a store::Tool>) {
        out.push(t);
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

/// Words a command uses when it's a check (tests, builds, linters).
const CHECK_WORDS: &[&str] = &["test", "tests", "pytest", "jest", "vitest", "mocha", "check", "clippy", "build", "vet", "tsc", "lint", "ruff", "mypy", "ctest", "make", "gradlew", "mvn"];

/// Is this command the check? The chat's check command when there is one, else anything test- or build-like.
fn is_check(cmd: &str, check: Option<&str>) -> bool {
    let c = cmd.to_lowercase();
    match check {
        Some(k) => {
            let key = k.to_lowercase().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
            !key.is_empty() && c.contains(&key)
        }
        None => c.split(|ch: char| ch.is_whitespace() || ch == '/' || ch == ';' || ch == '&').map(|w| w.trim_matches(|ch: char| !ch.is_alphanumeric())).any(|w| CHECK_WORDS.contains(&w)),
    }
}

/// What a reply's last words say it did: done, fixed, tests passing.
fn claims_done(text: &str) -> bool {
    let t = text.to_lowercase();
    const SAYS: &[&str] = &[
        "tests pass", "tests now pass", "tests are passing", "tests all pass", "all tests", "test suite passes", "build passes", "builds cleanly", "build succeeds", "compiles", "all green", "is fixed",
        "are fixed", "fixed it", "fixed the", "should work now", "works now", "now works", "is done", "all done", "done:", "complete:", "implemented",
    ];
    t.trim_start().starts_with("done") || SAYS.iter().any(|w| t.contains(w))
}

/// A reply's last words: its final text part (a coding agent's), else all of it.
fn final_text(m: &store::Msg) -> &str {
    m.parts.iter().rev().find_map(|p| if let store::Part::Text { text } = p { Some(text.as_str()) } else { None }).unwrap_or(&m.content)
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Verdict {
    /// the check ran after the last edit and passed (or /verify said so)
    Checked,
    Failed,
    /// edits since the check last ran
    Stale,
    NotRun,
}

/// The line under a coding agent's finished reply that says whether its work was checked: a check ran after
/// its last edit (✓, or ✗ if it failed), files were edited after the last check (stale), or nothing checked it
/// (said only when its last words claim it's done or passing). /verify's result wins over all of that.
fn badge(m: &store::Msg, check: Option<&str>) -> Option<(Verdict, String)> {
    if let Some(v) = &m.verified {
        return Some((if v.starts_with('✓') { Verdict::Checked } else { Verdict::Failed }, v.clone()));
    }
    let tools = tools_in(&m.parts);
    let last_edit = tools.iter().rposition(|t| is_edit(t));
    let claims = claims_done(final_text(m));
    if last_edit.is_none() && !claims {
        return None;
    }
    let last_run = tools.iter().rposition(|t| matches!(t.label.as_str(), "Bash" | "PowerShell" | "Run") && t.status != "running" && is_check(&t.target, check));
    match (last_run, last_edit) {
        (Some(r), e) if e.is_none_or(|e| r > e) => {
            let t = tools[r];
            if t.status == "done" && !t.summary.starts_with("exit ") {
                Some((Verdict::Checked, format!("✓ checked after the last edit · {}", t.target)))
            } else {
                Some((Verdict::Failed, format!("✗ the check after the last edit failed ({}) · {}", if t.summary.is_empty() { "error" } else { &t.summary }, t.target)))
            }
        }
        (Some(r), Some(_)) => {
            let files: BTreeSet<&str> = tools[r + 1..].iter().filter(|t| is_edit(t)).map(|t| t.target.as_str()).collect();
            Some((Verdict::Stale, format!("stale: {} edited since {} ran · /verify", count(files.len().max(1), "file", "files"), tools[r].target)))
        }
        // edits that nothing checked: only worth saying when it claims they work
        _ if claims => Some((Verdict::NotRun, "not run: nothing checked this work · /verify runs the check".into())),
        _ => None,
    }
}

/// The check last used in each folder (/check, /verify with a command), in oriel's own data file.
fn checks_file() -> PathBuf {
    #[cfg(test)]
    {
        std::path::absolute("target/test-scratch/chat/checks.json").unwrap_or_default()
    }
    #[cfg(not(test))]
    {
        crate::config::data_dir().join("chat-checks.json")
    }
}

fn folder_key(dir: &Path) -> String {
    let s = dir.to_string_lossy().trim_end_matches(['/', '\\']).to_string();
    if cfg!(windows) { s.to_lowercase().replace('/', "\\") } else { s }
}

fn remembered_check(dir: &Path) -> Option<String> {
    let m: HashMap<String, String> = std::fs::read_to_string(checks_file()).ok().and_then(|s| serde_json::from_str(&s).ok())?;
    m.get(&folder_key(dir)).cloned().filter(|c| !c.trim().is_empty())
}

fn remember_check(dir: &Path, cmd: &str) {
    let path = checks_file();
    let mut m: HashMap<String, String> = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    m.insert(folder_key(dir), cmd.to_string());
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let _ = std::fs::write(&path, serde_json::to_string_pretty(&m).unwrap_or_default());
}

fn lines_block(lines: Vec<Line<'static>>, hits: Vec<(usize, Hit)>) -> Block {
    Block { lines: Rc::new(lines), hits: Rc::new(hits) }
}

/// Blocks run together into one.
fn join_blocks(blocks: Vec<Block>) -> Block {
    if blocks.len() == 1 {
        return blocks.into_iter().next().unwrap_or_default();
    }
    let (mut lines, mut hits) = (vec![], vec![]);
    for b in blocks {
        let off = lines.len();
        hits.extend(b.hits.iter().map(|(n, h)| (n + off, h.clone())));
        lines.extend(b.lines.iter().cloned());
    }
    lines_block(lines, hits)
}

/// "title.md" in `dir`, or "title 2.md", "title 3.md"... if that's taken (an export never overwrites another).
fn free_path(dir: &Path, title: &str) -> PathBuf {
    let name: String = title.chars().map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' { c } else { '_' }).collect();
    let name = name.trim();
    let mut path = dir.join(format!("{name}.md"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{name} {n}.md"));
        n += 1;
    }
    path
}

/// "12 lines" / "1 line".
fn lines_of(text: &str) -> String {
    let n = text.lines().count().max(1);
    format!("{n} line{}", if n == 1 { "" } else { "s" })
}

/// The toast for a copied code block: "copied 42 lines of rust".
fn copied_code(c: &md::Code) -> String {
    format!("copied {} of {}", lines_of(&c.text), if c.lang.is_empty() { "code" } else { &c.lang })
}

/// Git repos under the usual code folders (home's Code, src, projects... and a drive's own), two levels deep,
/// most recently touched first. Only folder names are listed: nothing is read.
fn find_repos() -> Vec<String> {
    let mut roots: Vec<PathBuf> = vec![];
    if let Some(h) = dirs::home_dir() {
        for d in ["Code", "code", "src", "dev", "projects", "Projects", "repos", "git", "GitHub", "Documents/GitHub", "source/repos", "work"] {
            roots.push(h.join(d));
        }
    }
    if cfg!(windows) {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        for d in ["Code", "code", "dev", "src", "projects", "repos"] {
            roots.push(PathBuf::from(format!("{drive}\\{d}")));
        }
    }
    let mut seen_roots: Vec<PathBuf> = vec![];
    let mut found: Vec<(std::time::SystemTime, String)> = vec![];
    let mut visited = 0;
    let subdirs = |d: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(d).map(|rd| rd.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir()) && !e.file_name().to_string_lossy().starts_with('.')).map(|e| e.path()).collect()).unwrap_or_default()
    };
    for root in roots {
        // Code and code are one folder on Windows and macOS
        let canon = std::fs::canonicalize(&root).unwrap_or(root.clone());
        if !root.is_dir() || seen_roots.contains(&canon) {
            continue;
        }
        seen_roots.push(canon);
        let mut stack: Vec<(PathBuf, usize)> = subdirs(&root).into_iter().map(|p| (p, 1)).collect();
        while let Some((d, depth)) = stack.pop() {
            visited += 1;
            if visited > 3000 || found.len() >= 60 {
                break;
            }
            if d.join(".git").exists() {
                let t = std::fs::metadata(&d).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
                found.push((t, d.to_string_lossy().to_string()));
            } else if depth < 2 {
                stack.extend(subdirs(&d).into_iter().map(|p| (p, depth + 1)));
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().map(|f| f.1).collect()
}

/// A reply that was stopped: running calls end as stopped, and it says so.
fn mark_stopped(m: &mut store::Msg) {
    if m.role != "assistant" {
        return;
    }
    activity::settle(&mut m.parts, "stopped");
    if m.content.trim().is_empty() && m.parts.is_empty() {
        m.content = "(stopped)".into();
    }
    m.note = Some(format!("{} · stopped", m.note.clone().unwrap_or_default()).trim_start_matches(" · ").to_string());
}

/// The conversation as the AI gets it. Every message has text: a reply that only used tools says what it did,
/// and messages you queued mid-reply go in as your turns where the agent read them. Also, when `provider` has a
/// CLI session that another AI added to since its last reply, where that catch-up starts.
fn history(chat: &store::Chat, provider: &str) -> (Vec<(String, String)>, Option<usize>) {
    let upto = chat.state.get(provider).and_then(|s| s["upto"].as_u64()).map(|n| n as usize);
    let mut out: Vec<(String, String)> = vec![];
    let mut since = None;
    for (i, m) in chat.messages.iter().enumerate() {
        if Some(i) == upto {
            since = Some(out.len());
        }
        let queued = m.parts.iter().any(|p| matches!(p, store::Part::User { .. }));
        if m.role != "assistant" || !queued {
            let tools: Vec<&store::Tool> = m.parts.iter().filter_map(|p| if let store::Part::Tool(t) = p { Some(t) } else { None }).collect();
            out.push((m.role.clone(), said(&m.content, &tools)));
            continue;
        }
        let (mut text, mut tools) = (String::new(), vec![]);
        for p in &m.parts {
            match p {
                store::Part::Text { text: t } => {
                    if !text.is_empty() {
                        text.push_str("\n\n");
                    }
                    text.push_str(t.trim());
                }
                store::Part::Tool(t) => tools.push(t),
                store::Part::User { text: u } => {
                    out.push(("assistant".into(), said(&text, &tools)));
                    out.push(("user".into(), u.clone()));
                    text.clear();
                    tools.clear();
                }
                _ => {}
            }
        }
        out.push(("assistant".into(), said(&text, &tools)));
    }
    // only worth it if something besides the new message came after the session's last reply
    let since = since.filter(|&s| s + 1 < out.len());
    (out, since)
}

/// A message's text for the AI, never empty: what it wrote, plus a word on what it did ("edited 2 files, ran 1
/// command").
fn said(text: &str, tools: &[&store::Tool]) -> String {
    let count = |labels: &[&str]| tools.iter().filter(|t| labels.contains(&t.label.as_str())).count();
    let mut did = vec![];
    for (n, verb, one, many) in [
        (count(&["Update", "Write", "Notebook", "Delete"]), "edited", "file", "files"),
        (count(&["Bash", "PowerShell", "Run"]), "ran", "command", "commands"),
        (count(&["Read"]), "read", "file", "files"),
        (count(&["Grep", "Glob", "List", "Web search", "Fetch"]), "ran", "search", "searches"),
        (count(&["Agent"]), "ran", "subagent", "subagents"),
    ] {
        if n > 0 {
            did.push(format!("{verb} {n} {}", if n == 1 { one } else { many }));
        }
    }
    let text = text.trim();
    match (text.is_empty(), did.is_empty()) {
        (false, true) => text.to_string(),
        (false, false) => format!("{text}\n\n({})", did.join(", ")),
        (true, false) => format!("({})", did.join(", ")),
        (true, true) => "(no text)".into(),
    }
}

/// Fold a batch of a run's events into its chat's last message (the reply) and the run. Works the same for the
/// chat on screen and for one running in the background.
fn absorb(chat: &mut store::Chat, run: &mut Run, evs: Vec<Ev>, always: Option<&BTreeSet<String>>, who: &str) -> Outcome {
    let mut out = Outcome::default();
    for ev in evs {
        let Some(m) = chat.messages.last_mut() else {
            // nothing to write into: end the run rather than keep a spinner going forever
            out.finished = true;
            out.lost = true;
            break;
        };
        match ev {
            Ev::Token(t) => activity::push_text(m, &t),
            Ev::Thinking { text, tokens } => activity::push_thinking(m, &text, tokens),
            Ev::Tool(tool) => activity::upsert_tool(m, tool),
            Ev::Todos(items) => activity::set_todos(m, items),
            Ev::Usage(n) => run.stream.tokens = n,
            Ev::Steered(text) => {
                if let Some(i) = run.queue.iter().position(|q| q.text.trim() == text.trim()) {
                    run.queue.remove(i);
                }
                activity::push_user(m, &text);
            }
            Ev::Mark(text) => activity::push_mark(m, &text),
            Ev::Plan(plan) => {
                // the plan in full, in the transcript, while you decide
                activity::push_text(m, "\n\n");
                activity::push_text(m, &format!("{}\n\n", plan.trim()));
            }
            Ev::Perms(p) => {
                run.stream.perms = p.clone();
                out.perms = Some(p);
            }
            Ev::Status(st) => run.stream.status = st,
            Ev::Cost(c) => m.cost_usd = Some(c),
            Ev::Context(used, window) => m.ctx = Some((used, window)),
            Ev::Checkpoint(Ok(sha)) => m.ckpt = Some(sha),
            Ev::Checkpoint(Err(e)) => out.ckpt_err = Some(e),
            Ev::Question(q) => {
                let head = q.qs.first().map(|x| x.question.clone()).unwrap_or_default();
                out.waiting.push(format!("{who} is asking you: {}", ui::fit(&head, 80)));
                if run.questions.is_empty() {
                    run.qs = QState::default();
                    run.since = Instant::now();
                }
                run.questions.push_back(q);
                out.asked = true;
            }
            Ev::Ask(a) => {
                if always.is_some_and(|s| s.contains(&a.rule)) {
                    let _ = a.reply.send(approve::Decision::Allow);
                } else {
                    out.waiting.push(format!("{who} wants to: {} {}", a.label, ui::fit(&a.target, 60)));
                    if run.asks.is_empty() && run.questions.is_empty() {
                        run.since = Instant::now();
                    }
                    run.asks.push_back(a);
                }
            }
            Ev::State(k, v) => {
                let (prov, field) = k.split_once('.').unwrap_or((k.as_str(), "session"));
                let entry = chat.state.entry(prov.to_string()).or_insert_with(|| serde_json::json!({}));
                if let Some(o) = entry.as_object_mut() {
                    o.insert(field.to_string(), serde_json::Value::String(v));
                }
            }
            Ev::Done { note } => {
                activity::settle(&mut m.parts, "done");
                if let Some(n) = note.filter(|n| !n.is_empty()) {
                    // "… · 4 denied · …": say how to allow them
                    if let Some(d) = n.split(" · ").find_map(|x| x.strip_suffix(" denied").and_then(|d| d.trim().parse::<u32>().ok())) {
                        out.denied = Some(d);
                    }
                    m.note = Some(n);
                }
                out.finished = true;
            }
            Ev::Error(e) => {
                out.failed = true;
                activity::settle(&mut m.parts, "stopped");
                if m.content.trim().is_empty() && m.parts.is_empty() {
                    m.content = format!("⚠ {e}");
                } else {
                    m.note = Some(format!("⚠ {e}"));
                }
                out.finished = true;
            }
        }
    }
    if out.finished && !out.failed && !out.lost {
        // the session has seen the chat up to here: another AI's messages after this are news to it
        let p = chat.messages.last().and_then(|m| m.model.clone()).unwrap_or_default();
        let n = chat.messages.len();
        if let Some(o) = chat.state.get_mut(&p).and_then(|s| s.as_object_mut()) {
            o.insert("upto".into(), serde_json::json!(n));
        }
    }
    out
}

impl Chat {
    /// The pane is going away mid-reply: take in what the agent said since the last poll, then stop it and save
    /// the reply as stopped (as far as it got). False if nothing was running on screen.
    fn wind_down(&mut self) -> bool {
        let Some(mut run) = self.take_run() else { return false };
        let evs: Vec<Ev> = std::mem::take(&mut *run.stream.inbox.lock().unwrap());
        let who = providers::label(&self.provider_of()).to_lowercase();
        let allowed = self.always.get(&self.chat.id).cloned();
        let out = absorb(&mut self.chat, &mut run, evs, allowed.as_ref(), &who);
        if out.finished {
            // it had just ended by itself: nothing to stop, only to save (unanswered prompts answer no)
            run.stream.stop.store(true, Ordering::SeqCst);
            self.queue = std::mem::take(&mut run.queue);
            drop(run);
            self.persist();
            self.unqueue_to_box("stopped");
            return true;
        }
        self.put_run(run);
        self.stop();
        true
    }
}

impl Drop for Chat {
    /// Closing the pane or quitting oriel ends every reply it started: nothing carries on editing files unwatched,
    /// nothing waits forever on an approval nobody can give, and each transcript is saved as far as it got.
    fn drop(&mut self) {
        let mut pids: Vec<u32> = self.parked.values().map(|r| r.stream.pid.load(Ordering::SeqCst)).collect();
        pids.extend(self.stream.as_ref().map(|s| s.pid.load(Ordering::SeqCst)));
        self.wind_down();
        for (id, mut r) in std::mem::take(&mut self.parked) {
            r.stream.stop.store(true, Ordering::SeqCst);
            let evs: Vec<Ev> = std::mem::take(&mut *r.stream.inbox.lock().unwrap());
            let who = self.chats.iter().find(|c| c.id == id).map(|c| providers::label(&self.provider_for(c)).to_lowercase()).unwrap_or_default();
            let allowed = self.always.get(&id).cloned();
            if let Some(c) = self.chats.iter_mut().find(|c| c.id == id) {
                // what it said since the last poll, then marked stopped (unless it had just finished)
                let out = absorb(c, &mut r, evs, allowed.as_ref(), &who);
                if let Some(m) = c.messages.last_mut().filter(|_| !out.finished) {
                    mark_stopped(m);
                }
                store::save(c);
            }
            // dropping the run drops its questions and approvals: each answers no
        }
        // at once: the watcher thread that would kill them doesn't get to run before oriel exits
        for pid in pids.into_iter().filter(|p| *p != 0) {
            providers::kill_tree(pid);
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
    fn cwd(&self) -> Option<PathBuf> {
        // where its agents work (without workdir()'s fallback, which makes a folder)
        Some(self.workdir_label()).filter(|d| d.is_dir())
    }
    fn reopen(&self) -> Option<&'static str> {
        Some("ai")
    }
    fn resume_id(&self) -> Option<String> {
        // a chat that's been saved (an empty new one has nothing to come back to)
        self.chats.iter().any(|c| c.id == self.chat.id).then(|| self.chat.id.clone())
    }
    fn resume(&mut self, id: &str) {
        if self.chats.iter().any(|c| c.id == id) {
            self.open_chat(id);
        }
    }
    fn subtitle(&self) -> Option<String> {
        let p = self.provider_of();
        let mut s = providers::label(&p).to_lowercase();
        match p.as_str() {
            // drawn every frame: the folder as set, no disk check (workdir() can create oriel's work folder)
            "claude" | "codex" => s.push_str(&format!(" · {} · {}", self.chat.model.clone().unwrap_or("default".into()), ui::fit(&self.workdir_label().to_string_lossy(), 40))),
            _ => s.push_str(&format!(" · {}", self.chat.model.clone().unwrap_or("default".into()))),
        }
        Some(s)
    }
    fn badge(&self) -> Option<String> {
        let started = self.stream.iter().chain(self.parked.values().map(|r| &r.stream)).map(|s| s.started).min()?;
        Some(SPIN[(started.elapsed().as_millis() / 100) as usize % SPIN.len()].to_string())
    }
    fn tick_every(&self) -> Option<Duration> {
        (self.stream.is_some() || !self.parked.is_empty()).then(|| Duration::from_millis(100))
    }
    /// Replies running: the one on screen and the ones in other chats (closing the pane stops them all).
    fn busy(&self) -> usize {
        self.stream.is_some() as usize + self.parked.len()
    }
    fn wants_images(&self) -> bool {
        true
    }
    fn open_now(&self) -> Vec<crate::alerts::Open> {
        self.open_items()
    }
    fn respond(&mut self, key: &str, r: crate::alerts::Reply, cx: &mut Cx) -> bool {
        self.answer_open(key, r, cx)
    }

    /// Something was remembered elsewhere (another chat's /perms or /model, setup, a hand edit): new chats here use
    /// it, and so does this one while it's still empty.
    fn config_changed(&mut self, cfg: &crate::config::Config) {
        self.models = cfg.ai.models.clone();
        self.effort = cfg.ai.effort.clone();
        // the default mode for new chats; the open one follows only while it's empty (a mode picked for a
        // conversation with shift+tab stays)
        if let Some(p) = norm_perms(&cfg.ai.perms) {
            if p != self.default_perms && self.chat.messages.is_empty() && self.stream.is_none() {
                self.perms = p.to_string();
            }
            self.default_perms = p.to_string();
        }
        // a key saved with /key makes that AI usable here too
        for (id, key) in [("openai", &cfg.ai.openai_key), ("anthropic", &cfg.ai.anthropic_key)] {
            if !key.is_empty() && !self.avail.contains(&id) {
                self.avail.push(id);
            }
        }
        if let Some(p) = self.avail.iter().find(|p| **p == cfg.ai.provider) {
            self.provider = p.to_string();
        }
        if self.chat.messages.is_empty() && self.stream.is_none() {
            let p = self.provider.clone();
            self.chat.model = self.models.get(&p).cloned().filter(|m| !m.is_empty());
            self.chat.provider = Some(p);
        }
    }
    /// The chat on screen: waiting on you, working, or done. (Replies running in other chats say for themselves
    /// when they need you or finish: the app's tracking would name the chat on screen.)
    fn activity(&self) -> Option<crate::pane::Activity> {
        use crate::pane::Activity;
        if !self.asks.is_empty() || !self.questions.is_empty() {
            Some(Activity::Blocked)
        } else if self.stream.is_some() {
            Some(Activity::Working)
        } else if self.ran {
            Some(Activity::Idle)
        } else {
            None
        }
    }

    fn poll(&mut self, cx: &mut Cx) {
        use crate::alerts::Kind;
        self.drain_jobs(cx);
        // ---- the chat on screen
        if let Some(mut run) = self.take_run() {
            let who = providers::label(&self.provider_of()).to_lowercase();
            let evs: Vec<Ev> = std::mem::take(&mut *run.stream.inbox.lock().unwrap());
            let allowed = self.always.get(&self.chat.id).cloned();
            let out = absorb(&mut self.chat, &mut run, evs, allowed.as_ref(), &who);
            // say so when you may not be looking: another window has the focus, or another pane does. (Off screen
            // altogether, the app's agent tracking says "needs you" / "finished", so it isn't said twice.)
            let on_screen = self.drawn.is_some_and(|t| t.elapsed() < Duration::from_millis(1500));
            let tell = on_screen && !(cx.focused && crate::app::term_focused());
            if tell {
                for w in &out.waiting {
                    cx.alert(Kind::Approval, w.clone());
                }
            }
            if out.asked {
                self.scroll = 0;
            }
            if let Some(p) = out.perms {
                // you approved a plan: it runs in the mode you picked (and Claude Code hears about it too)
                self.perms = p.clone();
                if let Some(tx) = &run.stream.steer {
                    let _ = tx.send(providers::Steer::Mode(p));
                }
            }
            if let Some(e) = &out.ckpt_err {
                if self.ckpt_warned.insert(self.chat.id.clone()) {
                    self.info.push(format!("no checkpoint for this turn: {e} (/undo can't go back past it)"));
                }
            }
            if let Some(d) = out.denied {
                self.info.push(format!(
                    "{d} action{} blocked by the permission mode ({}). /perms ask to approve each one, /perms bypass to allow everything (this chat; /perms default <mode> for new ones).",
                    if d == 1 { " was" } else { "s were" },
                    run.stream.perms
                ));
            }
            if !out.finished {
                self.put_run(run);
                return self.poll_parked(cx);
            }
            if tell && !out.lost {
                if out.failed {
                    cx.alert(Kind::BuildFailed, format!("{who} stopped with an error: {}", self.chat.title));
                } else {
                    cx.alert(Kind::AgentDone, format!("{who} finished: {}", self.chat.title));
                }
            }
            // what's left of the run goes: unanswered approvals and questions answer no
            self.queue = std::mem::take(&mut run.queue);
            drop(run);
            self.persist();
            let id = self.chat.id.clone();
            self.turn_ended(&id, !(out.failed || out.lost), cx);
            // anything queued that the agent didn't take mid-reply is the next message
            if out.failed || out.lost {
                self.unqueue_to_box("the reply failed");
            } else if !self.queue.is_empty() {
                let text = self.queue.drain(..).map(|q| q.text).collect::<Vec<_>>().join("\n\n");
                self.send(text, cx);
            }
            self.handoff_ready(&id, out.failed || out.lost, cx);
        }
        self.poll_parked(cx);
    }

    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        if !self.readonly {
            self.take_open();
        }
        let t = cx.theme;
        let has_activity = self.chat.messages.iter().any(|m| !m.parts.is_empty());
        let expand_hint = if self.expanded { "collapse" } else { "expand tools" };
        self.drawn = Some(Instant::now());
        if self.review.as_ref().is_some_and(|v| v.chat == self.chat.id) {
            return self.draw_review(f, area, cx);
        }
        let prompt = !self.asks.is_empty() || !self.questions.is_empty();
        let restoring = self.restore.as_ref().is_some_and(|r| r.chat == self.chat.id && matches!(r.plan, Some(Ok(_))) && !r.busy);
        let hints: Vec<(&str, &str)> = if self.readonly {
            vec![("esc", "back"), ("r", "resume in chat"), ("a", "attach to chat"), ("ctrl+o", expand_hint), ("↑↓ pgup/pgdn", "scroll")]
        } else if self.confirm_delete {
            vec![("y", "delete this chat"), ("esc", "keep it")]
        } else if restoring && self.input.is_empty() {
            vec![("y", "put the folder back"), ("d", "show the diff"), ("esc", "leave it")]
        } else if prompt && !self.input.is_empty() {
            vec![("esc", "clear the box to answer"), ("enter", "queue it")]
        } else if !self.questions.is_empty() {
            vec![("↑↓", "choose"), ("enter", "answer"), ("type", "your own"), ("esc", "skip"), ("pgup", "scroll")]
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
            let mut v = vec![("enter", "send"), ("ctrl+g", "regenerate"), ("ctrl+r", "past prompts"), ("ctrl+n", "new chat"), ("/", "commands")];
            if self.last_reply().is_some() {
                v.push(("ctrl+y", "copy"));
            }
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
        self.refresh_usage(cx.waker());
        // ---- the composer grows with what you type (up to COMPOSER_ROWS rows, then it scrolls); a read-only
        // transcript has none: all of it is the conversation
        let text_w = (area.width as usize).saturating_sub(5).max(4); // the borders, "› ", a column for the cursor
        self.comp_w = text_w;
        self.ed.load(&self.input, self.cursor);
        let segs = self.ed.layout(text_w);
        let rows = if self.input.is_empty() { 1 } else { segs.len() };
        let shown = rows.min(COMPOSER_ROWS.min((area.height as usize).saturating_sub(7).max(1)));
        let comp_h = if self.readonly { 0 } else { shown as u16 + 2 };
        let comp = Rect { y: area.bottom() - comp_h, height: comp_h, ..area };
        let above = Rect { height: area.height - comp_h, ..area };
        let above = Rect { x: above.x + 1, width: above.width.saturating_sub(2), ..above };
        self.click_hits.clear();
        // the transcript (none on a new chat: the hero shows instead), as blocks: only what's on screen is copied out
        let hero = self.chat.messages.is_empty() && self.info.is_empty();
        let width = above.width as usize;
        let blocks = if hero { vec![] } else { self.transcript(width, cx) };
        let total: usize = blocks.iter().map(|b| b.lines.len()).sum();
        // scrolled up while a reply streams: count what arrived below (not what opening a call added)
        let anchor = self.anchor.take().filter(|a| a.0 == self.chat.id && a.1 == above.width && !hero);
        if let Some((_, _, _, prev_len)) = anchor {
            if self.scroll > 0 && self.stream.is_some() && !self.relayout {
                self.unseen += total.saturating_sub(prev_len);
            }
        }
        if self.scroll == 0 {
            self.unseen = 0;
        }
        self.relayout = false;
        // what's pinned above the composer: going back to a checkpoint; while an agent works, an approval prompt,
        // the status line, the todos
        let mut pinned = self.restore_lines(above.width as usize, cx);
        pinned.extend(self.pinned_lines(above.width as usize, cx));
        if self.scroll > 0 && (self.unseen > 0 || self.stream.is_some()) {
            // scrolled up: say what's arriving below, and how to get back to it
            let what = if self.unseen > 0 { format!(" ↓ {} new line{} ", self.unseen, if self.unseen == 1 { "" } else { "s" }) } else { " ↓ still working below ".to_string() };
            let pill = Line::from(vec![Span::styled(what, ui::bold_accent(t)), Span::styled("ctrl+end ", ui::muted(t))]);
            let pad = (above.width as usize).saturating_sub(pill.width()) / 2;
            let mut l = vec![Span::raw(" ".repeat(pad))];
            l.extend(pill.spans);
            pinned.insert(0, Line::from(l));
        }
        pinned.truncate((above.height as usize).saturating_sub(3));
        let ph = pinned.len() as u16;
        let body = Rect { height: above.height - ph, ..above };
        if ph > 0 {
            f.render_widget(Paragraph::new(pinned), Rect { y: body.bottom(), height: ph, ..above });
        }

        // ---- messages (or the hero on a new chat)
        if hero {
            self.draw_hero(f, body, cx);
        } else {
            let h = body.height as usize;
            let max_scroll = total.saturating_sub(h);
            // scrolled up, what you're reading stays put while lines arrive below (or the box above the composer
            // grows); at the bottom it follows the end as ever
            if let (Some((_, _, prev_max, _)), true) = (anchor, self.scroll > 0) {
                if max_scroll >= prev_max {
                    self.scroll += max_scroll - prev_max;
                } else {
                    self.scroll = self.scroll.saturating_sub(prev_max - max_scroll).max(1);
                }
            }
            if let Some(i) = self.focus_msg.take() {
                // open at a given message (the one a search matched), a line of the one before showing
                let at = self.msg_starts.get(i).copied().unwrap_or(0).saturating_sub(1);
                self.scroll = max_scroll - at.min(max_scroll);
            }
            self.anchor = Some((self.chat.id.clone(), body.width, max_scroll, total));
            self.scroll = self.scroll.min(max_scroll);
            let start = max_scroll - self.scroll;
            let mut visible: Vec<Line> = Vec::with_capacity(h);
            let mut at = 0;
            for b in &blocks {
                let n = b.lines.len();
                if at + n > start && at < start + h {
                    let (from, to) = (start.saturating_sub(at), (start + h - at).min(n));
                    visible.extend(b.lines[from..to].iter().cloned());
                    for (k, hit) in b.hits.iter().filter(|(k, _)| (from..to).contains(k)) {
                        self.click_hits.push((body.y + (at + k - start) as u16, hit.clone()));
                    }
                }
                at += n;
                if at >= start + h {
                    break;
                }
            }
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

        if self.readonly {
            return;
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
            // the left column fits the longest entry (up to half the width); two spaces always come before the
            // description, and a command's arguments show muted after its name
            let need = items.iter().map(|it| it.left.width() + if it.args.is_empty() { 0 } else { it.args.width() + 1 }).max().unwrap_or(0) + 2;
            let lw = need.min(inner.width as usize / 2).max(10);
            for (row, it) in items.iter().skip(start).take(rows as usize).enumerate() {
                let on = start + row == self.menu_sel;
                let cs = if on { Style::default().fg(t.accent).add_modifier(Modifier::BOLD) } else { Style::default().add_modifier(Modifier::BOLD) };
                let name = ui::fit(&it.left, lw - 2);
                let room = (lw - 2).saturating_sub(name.width());
                let args = if it.args.is_empty() || room < 4 { String::new() } else { format!(" {}", ui::fit(&it.args, room - 1)) };
                let pad = lw.saturating_sub(name.width() + args.width());
                let dw = (inner.width as usize).saturating_sub(lw + 1);
                let line = Line::from(vec![
                    Span::styled(if on { "▌" } else { " " }, ui::accent(t)),
                    Span::styled(name, cs),
                    Span::styled(args, ui::muted(t)),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(ui::fit(&it.desc, dw), if on { Style::default().add_modifier(Modifier::BOLD) } else { ui::muted(t) }),
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
        let agent = matches!(self.provider_of().as_str(), "claude" | "codex");
        // the bottom edge: effort on the left, spend and plan usage after it, the permission mode on the right
        let mut left_end = comp.x + 1;
        let mut right_start = comp.right().saturating_sub(1);
        if agent && comp.width > 70 && !self.effort.is_empty() {
            let color = match self.effort.as_str() {
                "ultracode" => t.shine,
                "max" | "xhigh" => t.accent,
                _ => t.muted,
            };
            let label = Line::from(vec![Span::raw(" "), Span::styled(format!("◆ {} effort", self.effort), Style::default().fg(color).add_modifier(Modifier::BOLD)), Span::styled(" /effort ", Style::default().fg(t.muted))]);
            let lw = label.width() as u16;
            f.render_widget(Paragraph::new(label), Rect { x: comp.x + 2, y: comp.bottom() - 1, width: lw, height: 1 });
            left_end = comp.x + 2 + lw;
        }
        if agent && comp.width > 40 {
            // the permission mode (a running Codex only takes a new one with your next message)
            let (glyph, words, color) = perm_badge(&self.perms, t);
            let later = self.stream.as_ref().is_some_and(|s| s.perms != self.perms);
            let label = Line::from(vec![
                Span::raw(" "),
                Span::styled(format!("{glyph} {words}"), Style::default().fg(color).add_modifier(Modifier::BOLD)),
                Span::styled(if later { " (from your next message) " } else { " (shift+tab to cycle) " }, Style::default().fg(t.muted)),
            ]);
            let lw = label.width() as u16;
            if lw + 4 < comp.width {
                right_start = comp.right() - lw - 2;
                f.render_widget(Paragraph::new(label), Rect { x: right_start, y: comp.bottom() - 1, width: lw, height: 1 });
            }
        }
        // "ctx 38%": how full the context is (the warning colour from 50%, danger from 70%)
        if let Some((used, window)) = self.context_fill().filter(|_| comp.width > 50) {
            let pct = used * 100 / window;
            let st = match pct {
                70.. => Style::default().fg(t.danger).add_modifier(Modifier::BOLD),
                50.. => Style::default().fg(WARN).add_modifier(Modifier::BOLD),
                _ => ui::muted(t),
            };
            let label = Line::from(vec![Span::raw(" "), Span::styled(format!("ctx {pct}%"), st), Span::raw(" ")]);
            let lw = label.width() as u16;
            let x = if left_end > comp.x + 1 { left_end } else { comp.x + 2 };
            if x + lw + 2 < right_start {
                f.render_widget(Paragraph::new(label), Rect { x, y: comp.bottom() - 1, width: lw, height: 1 });
                left_end = x + lw;
            }
        }
        // "$0.84 this chat · 5h 72%": what the chat has cost, and how full the AI's plan window is
        let spend = self.spend();
        let usage = if agent { self.usage_text() } else { None };
        if spend > 0.0 || usage.is_some() {
            let mut sp = vec![Span::raw(" ")];
            if spend > 0.0 {
                sp.push(Span::styled(format!("${spend:.2} this chat"), ui::muted(t)));
            }
            if let Some((u, hot)) = usage {
                if spend > 0.0 {
                    sp.push(Span::styled(" · ", ui::muted(t)));
                }
                sp.push(Span::styled(u, if hot { Style::default().fg(t.danger).add_modifier(Modifier::BOLD) } else { ui::muted(t) }));
            }
            sp.push(Span::raw(" "));
            let label = Line::from(sp);
            let lw = label.width() as u16;
            let x = left_end + if left_end > comp.x + 1 { 1 } else { 1 };
            if x + lw < right_start {
                f.render_widget(Paragraph::new(label), Rect { x, y: comp.bottom() - 1, width: lw, height: 1 });
            }
        }
        // the top edge: where a recalled prompt came from, and how long the text is
        let mut top_right = comp.right().saturating_sub(1);
        if rows > 1 && comp.width > 30 {
            let n = self.input.split('\n').count();
            let size = if n > 1 { format!("{n} lines") } else { format!("{rows} rows") };
            let label = if comp.width > 60 { format!(" {size} · ctrl+j new line ") } else { format!(" {size} ") };
            let lw = label.width() as u16;
            top_right = comp.right() - lw - 2;
            f.render_widget(Paragraph::new(Span::styled(label, ui::muted(t))), Rect { x: top_right, y: comp.y, width: lw, height: 1 });
        }
        if let Some(from) = &self.history_from {
            let room = top_right.saturating_sub(comp.x + 4) as usize;
            let label = ui::fit(&format!(" from: {from} "), room);
            let lw = label.width() as u16;
            if lw > 8 {
                f.render_widget(Paragraph::new(Span::styled(label, ui::muted(t))), Rect { x: comp.x + 2, y: comp.y, width: lw, height: 1 });
            }
        } else if agent || self.handoff_hint().is_some() {
            // a check loop's progress, and a nudge towards /handoff when the context fills up
            let mut sp = vec![Span::raw(" ")];
            if let Some(s) = self.check_status().filter(|_| agent) {
                sp.push(Span::styled(s, ui::accent(t)));
                sp.push(Span::raw(" "));
            }
            if let Some(h) = self.handoff_hint() {
                sp.push(Span::styled(h, Style::default().fg(WARN)));
                sp.push(Span::raw(" "));
            }
            let room = top_right.saturating_sub(comp.x + 4) as usize;
            let text: String = sp.iter().map(|s| s.content.as_ref()).collect();
            if sp.len() > 1 && text.width() <= room {
                f.render_widget(Paragraph::new(Line::from(sp)), Rect { x: comp.x + 2, y: comp.y, width: text.width() as u16, height: 1 });
            }
        }
        // ---- the text
        if self.input.is_empty() {
            let name = providers::label(&self.provider_of()).to_lowercase();
            let hint = if !self.questions.is_empty() {
                format!("{name} is asking you something above: choose, or just type your own answer")
            } else if self.stream.as_ref().is_some_and(|s| s.steer.is_some()) {
                format!("type to queue a message: {name} reads it after its current step")
            } else if self.stream.is_some() {
                "type to queue a message: it sends when this reply ends".to_string()
            } else {
                format!("message {name}…   (/ for commands · ctrl+j new line)")
            };
            let room = inner.width.saturating_sub(3) as usize;
            f.render_widget(Paragraph::new(Line::from(vec![Span::styled("› ", ui::bold_accent(t)), Span::styled(ui::fit(&hint, room), ui::muted(t))])), inner);
            if cx.focused && !self.confirm_delete {
                f.set_cursor_position(Position { x: inner.x + 2, y: inner.y });
            }
            return;
        }
        self.ed.clamp_scroll(&segs, shown);
        self.ed.follow(&segs, shown);
        let top = self.ed.scroll;
        let lines: Vec<Line> = segs
            .iter()
            .enumerate()
            .skip(top)
            .take(shown)
            .map(|(v, s)| {
                let text: String = self.ed.lines[s.row].chars().skip(s.start).take(s.end - s.start).map(|c| if c == '\t' { ' ' } else { c }).collect();
                let lead = if v == 0 { Span::styled("› ", ui::bold_accent(t)) } else { Span::raw("  ") };
                Line::from(vec![lead, Span::raw(text)])
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
        if cx.focused && !self.confirm_delete {
            let (v, x) = self.ed.cursor_visual(&segs);
            if v >= top && v < top + shown {
                let x = inner.x + 2 + x as u16;
                f.set_cursor_position(Position { x: x.min(inner.right().saturating_sub(1)), y: inner.y + (v - top) as u16 });
            }
        }
    }

    fn key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        if k.modifiers.contains(KeyModifiers::ALT) {
            return false;
        }
        if self.readonly {
            // only scrolling and ctrl+o; the rest is for whoever shows the transcript (the search app)
            match k.code {
                KeyCode::Up | KeyCode::Char('k') => self.scroll += 1,
                KeyCode::Down | KeyCode::Char('j') => self.scroll = self.scroll.saturating_sub(1),
                KeyCode::PageUp => self.scroll += 10,
                KeyCode::PageDown | KeyCode::Char(' ') => self.scroll = self.scroll.saturating_sub(10),
                KeyCode::Home | KeyCode::Char('g') => self.scroll = usize::MAX / 2,
                KeyCode::End | KeyCode::Char('G') => self.scroll = 0,
                KeyCode::Char('o') if ctrl => {
                    self.expanded = !self.expanded;
                    self.open.clear();
                    self.cache.clear();
                }
                _ => return false,
            }
            return true;
        }
        if self.confirm_delete {
            self.confirm_delete = false;
            if k.code == KeyCode::Char('y') {
                self.delete_chat(cx);
            }
            return true;
        }
        // the diff view takes the keys while it's open
        if self.review.as_ref().is_some_and(|v| v.chat == self.chat.id) {
            return self.review_key(k, cx);
        }
        // going back to a checkpoint: y does it, d shows the diff, esc leaves it (with nothing typed)
        if let Some(r) = self.restore.as_ref().filter(|r| r.chat == self.chat.id && !r.busy && self.input.is_empty()) {
            let ready = matches!(r.plan, Some(Ok(_)));
            match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') if ready && !ctrl => {
                    self.confirm_restore(cx);
                    return true;
                }
                KeyCode::Char('d') | KeyCode::Char('D') if ready && !ctrl => {
                    let (idx, sha) = (r.idx, r.ckpt.clone());
                    self.restore = None;
                    self.open_review(idx, sha, cx);
                    return true;
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') if !ctrl => {
                    self.restore = None;
                    self.info.push("left the folder as it is".into());
                    return true;
                }
                _ => {}
            }
        }
        // scrolling, ctrl+o and copying work whatever is waiting on you
        let view_key = matches!(k.code, KeyCode::PageUp | KeyCode::PageDown) || (ctrl && matches!(k.code, KeyCode::Up | KeyCode::Down | KeyCode::Home | KeyCode::End | KeyCode::Char('o') | KeyCode::Char('y')));
        if !view_key && (!self.questions.is_empty() || !self.asks.is_empty()) && self.input.is_empty() {
            // a prompt that just popped up doesn't take the keys you were typing: letters go to the box (and then
            // wait there: with text in the box, typing is typing), and a stray enter or esc does nothing
            if self.prompt_since.elapsed() < GRACE {
                return match k.code {
                    KeyCode::Char(c) if !ctrl => {
                        self.insert(&c.to_string());
                        true
                    }
                    _ => true,
                };
            }
            // a question Claude is waiting on takes the keys first
            if !self.questions.is_empty() {
                self.question_key(k);
                return true;
            }
            // then an approval: y / n / a, each only as a key of its own
            if let Some(a) = self.asks.front() {
                let d = match k.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') if !ctrl => Some(approve::Decision::Allow),
                    KeyCode::Char('n') | KeyCode::Char('N') if !ctrl => Some(approve::Decision::Deny),
                    KeyCode::Char('a') | KeyCode::Char('A') if !ctrl => {
                        let rule = a.rule.clone();
                        self.info.push(format!("always allowing {rule} in this chat · /perms lists what's allowed, /perms reset forgets it"));
                        self.always.entry(self.chat.id.clone()).or_default().insert(rule);
                        Some(approve::Decision::Allow)
                    }
                    _ => None,
                };
                if let Some(d) = d {
                    if let Some(a) = self.asks.pop_front() {
                        let _ = a.reply.send(d);
                    }
                    // "always" also answers the ones already waiting that it covers
                    let allowed = self.always.get(&self.chat.id).cloned().unwrap_or_default();
                    while self.asks.front().is_some_and(|a| allowed.contains(&a.rule)) {
                        let _ = self.asks.pop_front().map(|a| a.reply.send(approve::Decision::Allow));
                    }
                    if !self.asks.is_empty() {
                        self.front_changed(); // the next one: a double tap mustn't answer it too
                    }
                    return true;
                }
            }
        }
        if std::mem::take(&mut self.chord_x) && matches!(k.code, KeyCode::Char('s') | KeyCode::Char('S')) {
            if self.stream.is_some() {
                self.send_now(cx);
            } else {
                let text = self.input.trim().to_string();
                self.set_input(String::new());
                if !text.is_empty() {
                    self.send(text, cx);
                }
            }
            return true;
        }
        let items = self.menu();
        match k.code {
            KeyCode::Char('x') if ctrl => self.chord_x = true,
            // shift+tab: the next permission mode, for this chat only (a bypass for one throwaway chat mustn't
            // become every chat's; the default is in settings, or /perms default <mode>). A running Claude Code
            // switches at once.
            KeyCode::BackTab => {
                let i = PERM_CYCLE.iter().position(|p| *p == self.perms).map(|i| (i + 1) % PERM_CYCLE.len()).unwrap_or(0);
                self.perms = PERM_CYCLE[i].to_string();
                self.push_perms();
            }
            KeyCode::Up if self.input.is_empty() && self.queue.iter().any(|q| !q.sent) => {
                let i = self.queue.iter().rposition(|q| !q.sent).unwrap_or(0);
                let text = self.queue.remove(i).text;
                self.set_input(text);
            }
            KeyCode::Char('o') if ctrl => {
                self.expanded = !self.expanded;
                self.open.clear();
                self.cache.clear();
                self.relayout = true;
            }
            KeyCode::Char('y') if ctrl => self.copy_quick(cx),
            // undo / redo in the box
            KeyCode::Char(z @ ('z' | 'Z')) if ctrl && (shift || z == 'Z') => {
                self.edit(|e| e.redo());
            }
            KeyCode::Char('z') if ctrl => {
                self.edit(|e| e.undo());
            }
            // a new line: ctrl+j works on every terminal (shift+enter only where the terminal tells them apart)
            KeyCode::Char('j') if ctrl => {
                self.edit(|e| e.newline());
                self.menu_sel = 0;
            }
            // delete the word before the cursor (ctrl+backspace arrives as ctrl+h on many terminals)
            KeyCode::Char('w' | 'h') if ctrl => {
                self.edit(|e| e.delete_word_left());
                self.menu_sel = 0;
            }
            KeyCode::Char('n') if ctrl => self.new_chat(),
            KeyCode::Char('g') if ctrl => self.retry(cx),
            // like a shell's reverse search: /prompts, every prompt you've sent from any chat, as you type
            KeyCode::Char('r') if ctrl => {
                if !self.input.starts_with("/prompts") {
                    self.set_input("/prompts ".into());
                }
                self.menu_sel = 0;
            }
            KeyCode::Char('f') if ctrl => self.toggle_folder(),
            KeyCode::Char('d') if ctrl => {
                if !self.chat.messages.is_empty() {
                    self.confirm_delete = true;
                }
            }
            KeyCode::Char('u') if ctrl => self.set_input(String::new()),
            // the start / end of the line you're on
            KeyCode::Char('a') if ctrl => self.edit(|e| e.col = 0),
            KeyCode::Char('e') if ctrl => self.edit(|e| e.col = e.lines[e.row].chars().count()),
            KeyCode::Char(c) if !ctrl => self.insert(&c.to_string()),
            KeyCode::Esc => {
                if !self.input.is_empty() {
                    // clears the box (and the / menu) first: stopping a reply takes an esc on an empty box
                    self.set_input(String::new());
                    self.history_pos = None;
                    self.history_from = None;
                } else if self.stream.is_some() {
                    self.stop();
                } else {
                    self.info.clear();
                }
            }
            KeyCode::Enter => {
                // shift+enter: a new line
                if shift {
                    self.edit(|e| e.newline());
                    return true;
                }
                let mut text = self.input.trim().to_string();
                if let Some(it) = items.get(self.menu_sel.min(items.len().saturating_sub(1))) {
                    if let Some(p) = &it.put {
                        // an earlier prompt: into the box, to edit or send
                        self.set_input(p.clone());
                        return true;
                    }
                    if !it.run {
                        // a command that needs its argument: complete it, the menu then lists the choices
                        self.set_input(it.fill.clone());
                        return true;
                    }
                    text = it.fill.clone();
                }
                if text.is_empty() {
                    return true;
                }
                self.set_input(String::new());
                self.history_pos = None;
                self.history_from = None;
                if text.starts_with('/') {
                    self.run_command(&text, cx);
                } else {
                    self.send(text, cx);
                }
            }
            KeyCode::Tab if !items.is_empty() => {
                let it = &items[self.menu_sel.min(items.len() - 1)];
                // tab on a command that takes an argument goes straight to its choices; on a folder it goes in
                let next = if let Some(p) = &it.put {
                    p.clone()
                } else if it.run && !it.fill.contains(' ') && self.arg_options(&it.fill).is_empty() {
                    it.fill.clone()
                } else if it.fill.ends_with(' ') || it.fill.contains(' ') {
                    it.fill.clone()
                } else {
                    format!("{} ", it.fill)
                };
                self.set_input(next);
            }
            KeyCode::Up if !items.is_empty() => self.menu_sel = self.menu_sel.saturating_sub(1),
            KeyCode::Down if !items.is_empty() => self.menu_sel = (self.menu_sel + 1).min(items.len() - 1),
            KeyCode::Up if ctrl => self.scroll += 1,
            KeyCode::Down if ctrl => self.scroll = self.scroll.saturating_sub(1),
            // ↑↓ move between the lines of what you're writing; from the first line, ↑ goes back through your
            // earlier prompts (this chat's, then every other chat's), like a shell
            KeyCode::Up => {
                if !self.row_move(-1) && !((self.input.is_empty() || self.history_pos.is_some()) && self.recall(true)) {
                    self.scroll += 1;
                }
            }
            KeyCode::Down => {
                if !self.row_move(1) && !self.recall(false) {
                    self.scroll = self.scroll.saturating_sub(1);
                }
            }
            // ctrl+pgup / ctrl+pgdn: the previous / next chat in the list
            KeyCode::PageUp if ctrl => self.step_chat(false),
            KeyCode::PageDown if ctrl => self.step_chat(true),
            KeyCode::PageUp => self.scroll += 10,
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Left if ctrl => self.edit(|e| e.word_left()),
            KeyCode::Right if ctrl => self.edit(|e| e.word_right()),
            KeyCode::Left => self.edit(|e| e.left()),
            KeyCode::Right => self.edit(|e| e.right()),
            // ctrl+end (or end with nothing typed): back to the live end; ctrl+home: the top (render caps it)
            KeyCode::End if ctrl || self.input.is_empty() => {
                self.scroll = 0;
                self.unseen = 0;
            }
            KeyCode::Home if ctrl => self.scroll = usize::MAX / 2,
            KeyCode::Home | KeyCode::End => {
                let (w, home) = (self.comp_w, k.code == KeyCode::Home);
                self.edit(|e| {
                    let segs = e.layout(w);
                    if home { e.home(&segs) } else { e.end(&segs) }
                });
            }
            KeyCode::Backspace if ctrl => {
                self.edit(|e| e.delete_word_left());
                self.menu_sel = 0;
            }
            KeyCode::Backspace => {
                self.edit(|e| e.backspace());
                self.menu_sel = 0;
            }
            KeyCode::Delete => self.edit(|e| e.delete()),
            _ => return false,
        }
        true
    }

    fn paste(&mut self, text: &str, _cx: &mut Cx) {
        // while Claude asks you something, a paste is your answer in your own words
        if self.input.is_empty() {
            if let Some(cur) = self.questions.front().and_then(|q| q.qs.get(self.qs.idx)) {
                self.qs.sel = cur.options.len();
                self.qs.other.get_or_insert_with(String::new).push_str(&text.replace("\r\n", " ").replace('\n', " "));
                return;
            }
        }
        self.insert(&text.replace("\r\n", "\n"));
    }

    fn mouse(&mut self, ev: MouseEvent, _area: Rect, cx: &mut Cx) {
        // the diff view: a click picks a file, the wheel scrolls its hunks
        if let Some(v) = self.review.as_mut().filter(|v| v.chat == self.chat.id) {
            let pos = Position { x: ev.column, y: ev.row };
            match ev.kind {
                MouseEventKind::ScrollUp => v.scroll = v.scroll.saturating_sub(3),
                MouseEventKind::ScrollDown => v.scroll += 3,
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(&(_, i)) = v.hits.iter().find(|(r, _)| r.contains(pos)) {
                        v.file = i;
                        v.scroll = 0;
                    }
                }
                _ => {}
            }
            return;
        }
        match ev.kind {
            MouseEventKind::ScrollUp => self.scroll += 3,
            MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_sub(3),
            MouseEventKind::Down(MouseButton::Left) => {
                let pos = Position { x: ev.column, y: ev.row };
                // a tool line opens / closes just that call; a code block's header copies the block
                if let Some((_, hit)) = self.click_hits.iter().find(|(y, _)| *y == ev.row).cloned() {
                    match hit {
                        Hit::Tool(id) => {
                            if !self.open.remove(&id) {
                                self.open.insert(id);
                            }
                            self.cache.clear();
                            self.relayout = true;
                        }
                        Hit::Code(c) => {
                            crate::clip::copy(&c.text);
                            cx.notify(copied_code(&c));
                        }
                    }
                    return;
                }
                if self.chat.messages.is_empty() {
                    match self.hero_hits.iter().find(|(r, _)| r.contains(pos)).map(|h| h.1.clone()) {
                        Some(HeroHit::Say(s)) => self.send(s, cx),
                        Some(HeroHit::Folder(d)) => self.run_command(&format!("/cwd {d}"), cx),
                        None => {}
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
        // all folders, or only the chats that worked in this chat's folder
        let folder = self.folder();
        let r = Rect { y: area.y + 1, height: 1, ..area };
        if r.bottom() <= area.bottom() {
            let label = if self.this_folder { format!("{} only", crate::panes::ais::usage::project_name(&folder)) } else { "all folders".to_string() };
            ui::side_row(f, r, "files", &label, "ctrl+f", self.this_folder, t);
            self.side_hits.push((r, SideItem::Folder));
        }
        let mut rows: Vec<(Option<&'static str>, usize)> = vec![];
        let now = store::now();
        let mut last = "";
        for (i, c) in self.chats.iter().enumerate() {
            if self.this_folder && !c.cwd.as_deref().is_some_and(|d| same_dir(d, &folder)) {
                continue;
            }
            let b = store::bucket(c.updated, now, self.offset);
            if b != last {
                rows.push((Some(b), 0));
                last = b;
            }
            rows.push((None, i));
        }
        let list = Rect { y: area.y + 3, height: area.height.saturating_sub(3), ..area };
        let h = list.height as usize;
        // a chat just opened (a click, /open, ctrl+pgdn) scrolls into view; the wheel is free the rest of the time
        if self.side_follow != self.chat.id && h > 0 {
            if let Some(at) = rows.iter().position(|(g, i)| g.is_none() && self.chats[*i].id == self.chat.id) {
                if at < self.side_scroll {
                    self.side_scroll = at.saturating_sub(1); // its date heading too, if it's just above
                } else if at >= self.side_scroll + h {
                    self.side_scroll = at + 1 - h;
                }
                self.side_follow = self.chat.id.clone();
            }
        }
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
            // a reply still running in it: a spinner, or ? when it's waiting on you
            let run = if on { self.stream.as_ref().map(|_| !self.asks.is_empty() || !self.questions.is_empty()) } else { self.parked.get(&c.id).map(|r| !r.asks.is_empty() || !r.questions.is_empty()) };
            let mark = match run {
                Some(true) => Span::styled("? ", Style::default().fg(t.shine).add_modifier(Modifier::BOLD)),
                Some(false) => Span::styled(format!("{} ", SPIN[(cx.time * 10.0) as usize % SPIN.len()]), ui::accent(t)),
                None => Span::raw("  "),
            };
            // ↳ a chat that carries on from another one (/handoff)
            let child = c.extra.get("parent").is_some();
            let mut sp = vec![mark];
            if child {
                sp.push(Span::styled("↳ ", ui::muted(t)));
            }
            sp.push(Span::styled(ui::fit(&c.title, (r.width as usize).saturating_sub(if child { 4 } else { 2 })), style));
            f.render_widget(Paragraph::new(Line::from(sp)), r);
            self.side_hits.push((r, SideItem::Chat(c.id.clone())));
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
                        SideItem::Folder => self.toggle_folder(),
                        SideItem::Chat(id) => self.open_chat(&id),
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
            c.open_chat(&format!("t{i}"));
            assert_eq!(c.chat.id, format!("t{i}"), "clicking row {i} opens that chat");
            assert_eq!(order(&c), before, "just opening chats must not reorder the list");
        }
        // ctrl+d, then a click on another chat: the delete it was about to confirm doesn't carry over
        let mut k = k;
        k.key_mod(&mut c, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(c.confirm_delete);
        c.open_chat("t0");
        k.key(&mut c, KeyCode::Char('y'));
        assert!(c.chats.iter().any(|x| x.id == "t0") && c.chat.id == "t0" && c.input == "y");
    }

    /// ctrl+r searches every prompt you've sent (any chat, newest first); ctrl+g regenerates now; ctrl+f narrows the
    /// sidebar to this chat's folder.
    #[test]
    fn chat_prompt_search_and_folder_filter() {
        let mut k = Kit::new();
        let mut c = Chat::new(&k.config);
        let mk = |id: &str, title: &str, cwd: &str, prompts: &[&str], updated: f64| {
            let mut ch = store::Chat::new("claude");
            ch.id = id.into();
            ch.title = title.into();
            ch.cwd = Some(cwd.into());
            ch.updated = updated;
            for p in prompts {
                ch.messages.push(store::Msg { role: "user".into(), content: p.to_string(), ..Default::default() });
                ch.messages.push(store::Msg { role: "assistant".into(), content: "ok".into(), ..Default::default() });
            }
            ch
        };
        let (app, app2, site, here) =
            if cfg!(windows) { ("C:\\work\\app", "C:\\work\\app\\", "C:\\work\\site", "c:/work/app") } else { ("/work/app", "/work/app/", "/work/site", "/work/app") };
        c.chats = vec![
            mk("pa", "login work", app, &["fix the flaky login test", "now the logout test"], 300.0),
            mk("pb", "docs", site, &["write the flaky-test docs", "/model opus"], 200.0),
            mk("pc", "older login", app2, &["fix the flaky login test"], 100.0),
        ];
        c.chat.cwd = Some(here.into());
        k.key_mod(&mut c, KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert_eq!(c.input, "/prompts ", "ctrl+r searches prompts now, it doesn't regenerate");
        k.typ(&mut c, "flaky");
        let s = k.render_html(&mut c, 120, 30, "target/snap/chat-prompt-search.html");
        assert!(s.contains("fix the flaky login test") && s.contains("write the flaky-test docs"), "{s}");
        let items: Vec<String> = c.menu().into_iter().filter_map(|i| i.put).collect();
        assert_eq!(items, vec!["fix the flaky login test", "write the flaky-test docs"], "newest first, each once");
        k.key(&mut c, KeyCode::Down); // older
        k.key(&mut c, KeyCode::Enter);
        assert_eq!(c.input, "write the flaky-test docs", "in the box, not sent");
        assert!(c.stream.is_none());
        // ctrl+r again from there starts a fresh search; esc leaves an empty box
        k.key_mod(&mut c, KeyCode::Char('u'), KeyModifiers::CONTROL);
        k.key_mod(&mut c, KeyCode::Char('r'), KeyModifiers::CONTROL);
        k.typ(&mut c, "zzz");
        assert!(c.menu().is_empty() || c.menu().iter().all(|i| i.put.is_none()));
        k.key(&mut c, KeyCode::Esc);
        assert!(c.input.is_empty(), "esc clears it");

        // the sidebar: every chat, or only this folder's (either slash, any case, a trailing one)
        let side = k.render_side(&mut c, 34, 20);
        assert!(side.contains("all folders") && side.contains("docs"), "{side}");
        k.key_mod(&mut c, KeyCode::Char('f'), KeyModifiers::CONTROL);
        let side = k.render_side(&mut c, 34, 20);
        assert!(side.contains("app only") && side.contains("login work") && side.contains("older login") && !side.contains("docs"), "{side}");
        k.key_mod(&mut c, KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert!(k.render_side(&mut c, 34, 20).contains("docs"));
    }

    /// A session carried on from the search app stays a draft only until you leave it: the chat you open next
    /// saves as usual again.
    #[test]
    fn chat_resumed_draft_is_let_go() {
        let mut k = Kit::new();
        let mut c = Chat::new(&k.config);
        let mut other = store::Chat::new("claude");
        other.id = "other".into();
        other.messages.push(store::Msg { role: "user".into(), content: "hello".into(), ..Default::default() });
        c.chats = vec![other];
        let mut s = store::Chat::new("claude");
        s.messages.push(store::Msg { role: "user".into(), content: "from elsewhere".into(), ..Default::default() });
        request_open(Open::Resume(s, "carrying on".into()));
        k.render(&mut c, 100, 24);
        assert!(c.draft && c.info == ["carrying on"]);
        c.open_chat("other");
        assert!(!c.draft && c.chat.id == "other", "the draft went; this chat is yours");
        request_open(Open::Resume(store::Chat::new("claude"), "again".into()));
        k.render(&mut c, 100, 24);
        assert!(c.draft);
        c.new_chat();
        assert!(!c.draft);
    }

    /// The search app's reader: the chat's renderer with no composer, keys only scroll.
    #[test]
    fn chat_viewer_is_read_only() {
        let mut k = Kit::new();
        let mut ch = store::Chat::new("codex");
        ch.title = "rate limits".into();
        for i in 0..30 {
            ch.messages.push(store::Msg { role: "user".into(), content: format!("question {i}"), ..Default::default() });
            ch.messages.push(store::Msg { role: "assistant".into(), model: Some("codex".into()), content: format!("answer {i}"), ..Default::default() });
        }
        let mut v = Chat::viewer(&k.config, ch, 20);
        let s = k.render_html(&mut v, 100, 24, "target/snap/chat-viewer.html");
        assert!(s.contains("question 10") && !s.contains("question 3\n"), "opens at the message asked for\n{s}");
        assert!(s.contains("resume in chat") && !s.contains("message codex"), "{s}");
        assert!(!k.key(&mut v, KeyCode::Char('r')), "r is for whoever shows it");
        assert!(k.key(&mut v, KeyCode::End));
        assert!(k.render(&mut v, 100, 24).contains("answer 29"));
        assert!(v.input.is_empty());
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
        c.stream = Some(test_stream(None));
        c.stream.as_ref().unwrap().inbox.lock().unwrap().push(Ev::Done { note: Some("20s · 880 tokens · 5 turns · 4 denied · claude code".into()) });
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
        assert!(matches!(rx.try_recv(), Ok(providers::Steer::Say(t)) if t == "use nextest instead"), "handed to the agent right away");
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
        ripe(&mut c);
        let s = k.render_html(&mut c, 120, 36, "target/snap/chat-question.html");
        assert!(s.contains("Quiz 1") && s.contains("1 of 3") && s.contains("A unique borrow") && s.contains("Other…") && s.contains("Asking you"), "{s}");
        assert!(s.contains("What does &mut give you?") && !s.contains("`&mut`"), "the question's `code` is drawn as code: {s}");
        assert!(s.contains("esc skip") && !s.contains("esc stop"), "the hints say what esc does here: {s}");
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
        ripe(&mut c);
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
        c.stream = Some(test_stream(steer));
        c.ran = true;
        c
    }

    fn test_stream(steer: Option<std::sync::mpsc::Sender<providers::Steer>>) -> Stream {
        Stream { stop: Arc::default(), inbox: Arc::default(), status: String::new(), started: Instant::now(), tokens: 0, steer, pid: Arc::default(), perms: "edits".into() }
    }

    /// The prompt on screen has been up long enough to take keys (see GRACE).
    fn ripe(c: &mut Chat) {
        c.prompt_since = Instant::now() - Duration::from_secs(1);
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

    /// The Claude fixture replayed `n` times into one live reply (fresh call ids each time): a long agent run.
    fn long_live_chat(k: &mut Kit, n: usize) -> Chat {
        let mut c = agent_chat(k, "claude", PROMPT);
        for rep in 0..n {
            let mut p = agent::Claude::new(std::path::Path::new("C:\\work\\demo"));
            for evs in parse_fixture(CLAUDE_FIXTURE, |v, t, s| p.feed(v, t, s)) {
                let evs: Vec<Ev> = evs
                    .into_iter()
                    .map(|e| match e {
                        Ev::Tool(mut t) => {
                            t.id = format!("{rep}-{}", t.id);
                            t.parent = t.parent.map(|p| format!("{rep}-{p}"));
                            Ev::Tool(t)
                        }
                        e => e,
                    })
                    .collect();
                deliver(k, &mut c, evs);
            }
        }
        c
    }

    /// Frame time of a long live reply at 140x44. `cargo test chat_render_perf -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn chat_render_perf() {
        let mut k = Kit::new();
        let mut c = long_live_chat(&mut k, 20);
        let _forget = Forget(c.chat.id.clone());
        for expanded in [false, true] {
            c.expanded = expanded;
            c.cache.clear();
            k.render(&mut c, 140, 44);
            let t0 = Instant::now();
            for _ in 0..30 {
                k.render(&mut c, 140, 44);
            }
            println!("live, expanded={expanded}: {:.2} ms/frame", t0.elapsed().as_secs_f64() * 1000.0 / 30.0);
        }
        // with a command running at the end (its spinner redraws that block every frame)
        deliver(&mut k, &mut c, [Ev::Tool(store::Tool { id: "perf-run".into(), name: "Bash".into(), label: "Bash".into(), target: "cargo build".into(), status: "running".into(), ..Default::default() })]);
        c.expanded = false;
        let t0 = Instant::now();
        for i in 0..30 {
            k.time = 1.0 + i as f64 * 0.1;
            k.render(&mut c, 140, 44);
        }
        println!("live, a call running: {:.2} ms/frame", t0.elapsed().as_secs_f64() * 1000.0 / 30.0);
        // finished: the same reply 30 times over, every message cached
        c.stream = None;
        c.expanded = true;
        let m = c.chat.messages.clone();
        for _ in 0..30 {
            c.chat.messages.extend(m.iter().cloned());
        }
        c.cache.clear();
        k.render(&mut c, 140, 44);
        let t0 = Instant::now();
        for _ in 0..30 {
            k.render(&mut c, 140, 44);
        }
        println!("finished, 62 messages: {:.2} ms/frame", t0.elapsed().as_secs_f64() * 1000.0 / 30.0);
        let mut e = Chat::new(&k.config);
        e.info.push("x".into());
        let t0 = Instant::now();
        for _ in 0..30 {
            k.render(&mut e, 140, 44);
        }
        println!("empty (the test harness itself): {:.2} ms/frame", t0.elapsed().as_secs_f64() * 1000.0 / 30.0);
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
        let (row, _) = c.click_hits.iter().find(|(_, h)| *h == Hit::Tool("t2".into())).cloned().unwrap();
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
        let ask = |tool: &str, tx: &std::sync::mpsc::Sender<approve::Decision>| approve::Ask { label: tool.into(), target: "rm -rf build".into(), body: body.clone(), rule: format!("{tool}(rm:*)"), reply: tx.clone() };
        deliver(&mut k, &mut c, [Ev::Ask(ask("Bash", &tx)), Ev::Ask(ask("Bash", &tx))]);
        ripe(&mut c);
        let s = k.render_html(&mut c, 120, 30, "target/snap/chat-agent-ask.html");
        println!("{s}");
        assert!(s.contains("allow Bash rm -rf build ?") && s.contains("always allow Bash(rm:*) in this chat"), "the prompt says exactly what 'always' covers");
        assert!(!s.contains("more lines"), "a one-line command isn't repeated under itself");
        k.key(&mut c, KeyCode::Char('n'));
        assert_eq!(rx.try_recv(), Ok(approve::Decision::Deny));
        assert_eq!(c.asks.len(), 1);
        k.key(&mut c, KeyCode::Char('a'));
        assert!(rx.try_recv().is_err() && c.input == "a", "the next prompt took over just now: a double tap doesn't answer it");
        k.key(&mut c, KeyCode::Backspace);
        ripe(&mut c);
        k.key(&mut c, KeyCode::Char('a'));
        assert_eq!(rx.try_recv(), Ok(approve::Decision::Allow));
        assert!(c.asks.is_empty() && c.input.is_empty(), "y/n/a don't leak into the composer");
        assert!(c.info.iter().any(|l| l.contains("always allowing Bash(rm:*) in this chat")), "{:?}", c.info);
        // after "always", the next one answers itself
        deliver(&mut k, &mut c, [Ev::Ask(ask("Bash", &tx))]);
        assert_eq!(rx.try_recv(), Ok(approve::Decision::Allow));
        assert!(c.asks.is_empty());
        // ... in this chat only: another chat asks again
        let a_id = c.chat.id.clone();
        c.new_chat();
        let _forget2 = Forget(c.chat.id.clone());
        c.chat.messages.push(store::Msg { role: "user".into(), content: "tidy up".into(), ..Default::default() });
        c.chat.messages.push(store::Msg { role: "assistant".into(), model: Some("claude".into()), ..Default::default() });
        c.stream = Some(test_stream(None));
        deliver(&mut k, &mut c, [Ev::Ask(ask("Bash", &tx))]);
        assert!(rx.try_recv().is_err() && c.asks.len() == 1, "a new chat doesn't inherit 'always'");
        assert!(c.always.get(&a_id).is_some_and(|s| s.contains("Bash(rm:*)")), "the first chat keeps its own");
        // /perms reset forgets them
        c.always.entry(c.chat.id.clone()).or_default().insert("Edit".into());
        ripe(&mut c);
        k.typ(&mut c, "/perms reset");
        k.key(&mut c, KeyCode::Enter);
        assert!(!c.always.contains_key(&c.chat.id));
        c.stop();
    }

    fn user(text: &str) -> store::Msg {
        store::Msg { role: "user".into(), content: text.into(), ..Default::default() }
    }

    fn reply(text: &str) -> store::Msg {
        store::Msg { role: "assistant".into(), content: text.into(), model: Some("ollama".into()), ..Default::default() }
    }

    /// A plain (non-agent) chat with one exchange.
    fn plain_chat(k: &Kit, you: &str, ai: &str) -> Chat {
        let mut c = Chat::new(&k.config);
        c.provider = "ollama".into();
        c.chat.provider = Some("ollama".into());
        c.chat.title = store::title_from(you);
        c.chat.messages.push(user(you));
        c.chat.messages.push(reply(ai));
        c
    }

    fn click(row: u16) -> MouseEvent {
        MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 10, row, modifiers: KeyModifiers::NONE }
    }

    fn enter(k: &mut Kit, c: &mut Chat, line: &str) {
        k.typ(c, line);
        k.key(c, KeyCode::Enter);
    }

    /// ctrl+y copies the last reply's last code block (or all of a reply without one), /copy the reply's markdown,
    /// /copy code 2 the block before, /copy all the chat; a click on a block's ┌─ header copies that block.
    #[test]
    fn chat_copy_reply_and_code() {
        use crate::clip::last_copied;
        let mut k = Kit::new();
        let rust = "fn main() {\n    println!(\"hi\");\n}";
        let mut c = plain_chat(&k, "two snippets please", &format!("Here:\n\n```sh\nls -la\n```\n\nand\n\n```rust\n{rust}\n```\nDone."));
        k.key_mod(&mut c, KeyCode::Char('y'), KeyModifiers::CONTROL);
        assert_eq!(last_copied().as_deref(), Some(rust));
        assert!(k.notices().iter().any(|n| n == "copied 3 lines of rust"), "{:?}", k.notices());
        enter(&mut k, &mut c, "/copy code 2");
        assert_eq!(last_copied().as_deref(), Some("ls -la"));
        enter(&mut k, &mut c, "/copy");
        assert!(last_copied().is_some_and(|t| t.starts_with("Here:") && t.ends_with("Done.") && t.contains("```rust")), "the reply's own markdown");
        enter(&mut k, &mut c, "/copy all");
        assert!(last_copied().is_some_and(|t| t.starts_with("# two snippets please") && t.contains("**you:**") && t.contains("```sh")));
        enter(&mut k, &mut c, "/copy code 3");
        assert!(c.info.iter().any(|l| l.contains("has 2 code blocks")), "{:?}", c.info);
        // the menu after /copy lists the blocks, newest first
        c.input = "/copy ".into();
        c.cursor = 6;
        let s = k.render_html(&mut c, 130, 40, "target/snap/chat-copy-menu.html");
        assert!(s.contains("its last code block: rust · fn main() {") && s.contains("code 2") && s.contains("sh · ls -la"), "{s}");
        c.input.clear();
        c.cursor = 0;
        // a click on a block's header copies it
        let s = k.render_html(&mut c, 130, 40, "target/snap/chat-code-headers.html");
        assert!(s.contains("┌─ sh  click to copy"), "{s}");
        let (row, _) = c.click_hits.iter().find(|(_, h)| matches!(h, Hit::Code(b) if b.lang == "sh")).cloned().unwrap();
        k.mouse(&mut c, click(row), Rect::new(0, 0, 130, 40));
        assert_eq!(last_copied().as_deref(), Some("ls -la"));
        // no code in the reply: ctrl+y takes all of it
        let mut c = plain_chat(&k, "hi", "Just words,\nno code.");
        k.key_mod(&mut c, KeyCode::Char('y'), KeyModifiers::CONTROL);
        assert_eq!(last_copied().as_deref(), Some("Just words,\nno code."));
        assert!(k.notices().iter().any(|n| n == "copied the last reply (2 lines)"), "{:?}", k.notices());
    }

    /// /save and /copy all keep an agent's tool calls, as a folded list where they happened; /save never writes
    /// over an earlier export.
    #[test]
    fn chat_export_folds_tools_and_never_overwrites() {
        let k = Kit::new();
        let mut c = agent_chat(&k, "claude", "fix it");
        c.stream = None;
        let tool = |label: &str, target: &str, summary: &str, status: &str| store::Part::Tool(store::Tool { id: label.into(), name: label.into(), label: label.into(), target: target.into(), summary: summary.into(), status: status.into(), ..Default::default() });
        let m = c.chat.messages.last_mut().unwrap();
        m.parts = vec![store::Part::Text { text: "Looking.".into() }, tool("Update", "a.rs", "+1 -1", "done"), tool("Bash", "cargo test", "exit 1", "error"), store::Part::Text { text: "Fixed.".into() }];
        m.content = "Looking.\n\nFixed.".into();
        let md = c.export_md();
        println!("{md}");
        assert!(md.contains("<details><summary>2 tool calls: Update, Bash</summary>"), "{md}");
        assert!(md.contains("- Update `a.rs` · +1 -1\n- Bash `cargo test` · exit 1 (error)"), "{md}");
        let at = |s: &str| md.find(s).unwrap();
        assert!(at("Looking.") < at("<details>") && at("</details>") < at("Fixed."), "in the order they happened");
        let dir = std::path::absolute("target/test-scratch/chat/save").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let first = free_path(&dir, "fix: it");
        assert_eq!(first.file_name().unwrap(), "fix_ it.md");
        std::fs::write(&first, "x").unwrap();
        assert_eq!(free_path(&dir, "fix: it").file_name().unwrap(), "fix_ it 2.md", "an earlier export is kept");
    }

    /// The chat list from the keyboard: /open searches titles as you type and opens by id, ctrl+pgup / ctrl+pgdn
    /// step through the list (the sidebar keeps the open chat in view), /rename renames and saves.
    #[test]
    fn chat_list_keyboard_search_and_rename() {
        let mut k = Kit::new();
        let mut c = Chat::new(&k.config);
        c.chats = (0..30)
            .map(|i| {
                let mut ch = store::Chat::new("claude");
                ch.id = format!("kb{i}");
                ch.title = if i == 17 { "the regex one".into() } else { format!("chat {i}") };
                ch.updated = 1000.0 - i as f64;
                ch.messages.push(user(&format!("hi {i}")));
                ch
            })
            .collect();
        let _forget = Forget("kb16".into());
        k.typ(&mut c, "/open ");
        let items = c.menu();
        assert_eq!((items.len(), items[0].left.as_str()), (30, "chat 0"), "every chat, newest first");
        k.typ(&mut c, "regex");
        assert_eq!(c.menu().len(), 1, "narrowed as you type");
        let s = k.render_html(&mut c, 120, 30, "target/snap/chat-open-menu.html");
        assert!(s.contains("the regex one") && s.contains("older · claude code · 1 message"), "{s}");
        k.key(&mut c, KeyCode::Enter);
        assert_eq!(c.chat.id, "kb17");
        let side = k.render_side(&mut c, 32, 12);
        assert!(side.contains("the regex one"), "the list scrolled to the open chat: {side}");
        k.key_mod(&mut c, KeyCode::PageDown, KeyModifiers::CONTROL);
        assert_eq!(c.chat.id, "kb18");
        k.key_mod(&mut c, KeyCode::PageUp, KeyModifiers::CONTROL);
        k.key_mod(&mut c, KeyCode::PageUp, KeyModifiers::CONTROL);
        assert_eq!(c.chat.id, "kb16");
        enter(&mut k, &mut c, "/rename regex notes");
        assert_eq!(c.chat.title, "regex notes");
        assert!(store::load_all().iter().any(|x| x.id == "kb16" && x.title == "regex notes"), "saved");
        // /open with typed words (no pick) opens the first title that has them
        enter(&mut k, &mut c, "/open chat 3");
        assert_eq!(c.chat.id, "kb3");
        // above the first chat is a new one
        let top = c.chats[0].id.clone();
        c.open_chat(&top);
        k.key_mod(&mut c, KeyCode::PageUp, KeyModifiers::CONTROL);
        assert!(c.chat.messages.is_empty());
    }

    /// The composer grows with what you write (up to 8 rows, then scrolls), ctrl+j / shift+enter add lines, ↑↓ move
    /// between them before they reach history, and the word keys and undo work like the notes editor.
    #[test]
    fn chat_composer_grows_and_edits_by_line() {
        let mut k = Kit::new();
        let mut c = plain_chat(&k, "earlier prompt", "ok");
        k.typ(&mut c, "first line");
        k.key_mod(&mut c, KeyCode::Char('j'), KeyModifiers::CONTROL);
        k.typ(&mut c, "second line");
        k.key_mod(&mut c, KeyCode::Enter, KeyModifiers::SHIFT);
        k.typ(&mut c, "third");
        assert_eq!(c.input, "first line\nsecond line\nthird");
        let s = k.render_html(&mut c, 100, 30, "target/snap/chat-composer-lines.html");
        assert!(s.contains("› first line") && s.contains("│  second line") && s.contains("3 lines · ctrl+j new line"), "{s}");
        // ↑ goes up a line keeping the column; on the first line it leaves your text alone
        k.key(&mut c, KeyCode::Up);
        assert_eq!(c.cursor, "first line\nsecon".chars().count());
        k.key(&mut c, KeyCode::Up);
        k.key(&mut c, KeyCode::Up);
        assert_eq!((c.input.as_str(), c.cursor), ("first line\nsecond line\nthird", 5), "no history over what you typed");
        k.key(&mut c, KeyCode::Down);
        k.key(&mut c, KeyCode::Down);
        assert_eq!(c.cursor, "first line\nsecond line\nthird".chars().count());
        // words: ctrl+left, ctrl+w, ctrl+z
        k.key_mod(&mut c, KeyCode::Left, KeyModifiers::CONTROL);
        assert_eq!(c.cursor, "first line\nsecond line\n".chars().count());
        k.key_mod(&mut c, KeyCode::Char('e'), KeyModifiers::CONTROL);
        k.key_mod(&mut c, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(c.input, "first line\nsecond line\n");
        k.key_mod(&mut c, KeyCode::Backspace, KeyModifiers::CONTROL);
        assert_eq!(c.input, "first line\nsecond line", "ctrl+backspace at a line's start joins it up");
        k.key_mod(&mut c, KeyCode::Char('z'), KeyModifiers::CONTROL);
        k.key_mod(&mut c, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(c.input, "first line\nsecond line\nthird", "undone");
        // a long paste: 8 rows show, the rest scrolls, and the box says how long it is
        c.input.clear();
        c.cursor = 0;
        let mut acts = vec![];
        c.paste(&(1..=20).map(|i| format!("row{i:02}")).collect::<Vec<_>>().join("\r\n"), &mut cx_of(&k, &mut acts, true));
        let s = k.render_html(&mut c, 100, 30, "target/snap/chat-composer-paste.html");
        let rows = s.lines().filter(|l| l.contains("row")).count();
        assert!(rows == 8 && s.contains("row20") && !s.contains("row12") && s.contains("20 lines"), "{rows} rows: {s}");
        // enter sends all of it; ctrl+z brings it back
        k.key(&mut c, KeyCode::Enter);
        assert!(c.input.is_empty() && c.chat.messages.iter().any(|m| m.content.starts_with("row01\nrow02")));
        c.stop();
        k.key_mod(&mut c, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert!(c.input.starts_with("row01"), "{:?}", c.input);
    }

    /// ↑ past this chat's first message carries on into your prompts from other chats (each once, newest chat first)
    /// with "from: <chat>" on the box; /prompts searches them and puts one in the box without sending it.
    #[test]
    fn chat_history_reaches_other_chats() {
        let mut k = Kit::new();
        let mut c = plain_chat(&k, "hello here", "hi");
        let old = |id: &str, title: &str, prompts: &[&str]| {
            let mut ch = store::Chat::new("ollama");
            ch.id = id.into();
            ch.title = title.into();
            for p in prompts {
                ch.messages.push(user(p));
                ch.messages.push(reply("done"));
            }
            ch
        };
        let races = "review this diff for races, list file:line";
        c.chats = vec![old("h1", "races review", &[races]), old("h2", "names", &["name my app", races])];
        let up = |k: &mut Kit, c: &mut Chat| k.key(c, KeyCode::Up);
        up(&mut k, &mut c);
        assert_eq!((c.input.as_str(), c.history_from.as_deref()), ("hello here", None));
        up(&mut k, &mut c);
        assert_eq!((c.input.as_str(), c.history_from.as_deref()), (races, Some("races review")));
        let s = k.render_html(&mut c, 110, 30, "target/snap/chat-history-from.html");
        assert!(s.contains("from: races review"), "{s}");
        up(&mut k, &mut c);
        assert_eq!(c.input, "name my app", "the same prompt in another chat comes once");
        up(&mut k, &mut c);
        assert_eq!(c.input, "name my app", "the oldest stays");
        k.key(&mut c, KeyCode::Down);
        k.key(&mut c, KeyCode::Down);
        assert_eq!(c.input, "hello here");
        k.key(&mut c, KeyCode::Down);
        assert!(c.input.is_empty() && c.history_from.is_none());
        // /prompts: search as you type, enter puts it in the box unsent
        k.typ(&mut c, "/prompts diff for");
        let items = c.menu();
        assert!(items.len() == 1 && items[0].put.as_deref() == Some(races) && items[0].desc == "from races review", "{:?}", items.iter().map(|i| &i.left).collect::<Vec<_>>());
        let sent = c.chat.messages.len();
        k.key(&mut c, KeyCode::Enter);
        assert_eq!((c.input.as_str(), c.chat.messages.len()), (races, sent));
        // a recalled prompt you change is your draft: ↑ / ↓ scroll the chat instead of swapping it
        c.set_input(String::new());
        up(&mut k, &mut c);
        k.key(&mut c, KeyCode::Backspace);
        up(&mut k, &mut c);
        k.key(&mut c, KeyCode::Down);
        assert_eq!((c.input.as_str(), c.history_from.as_deref()), ("hello her", None));
        // what oriel sent in your name (a check loop's "not yet", a handoff) isn't one of your prompts
        c.chats[0].messages.push(user(&not_yet("cargo test", "`cargo test` failed (exit 1):\nboom", 1, 20)));
        c.chats[0].messages.push(user(&format!("{HANDOFF_ASK}\n\nThe next session's goal: x.")));
        assert!(c.prompt_history().iter().all(|(p, _)| !p.starts_with("◎ check") && !p.starts_with("Write a handoff card")), "{:?}", c.prompt_history());
    }

    /// The live reply's cached blocks draw exactly what a fresh draw does: every line and every click target, folded
    /// and expanded; a running call still animates.
    #[test]
    fn chat_live_blocks_match_a_full_draw() {
        let mut k = Kit::new();
        let mut c = long_live_chat(&mut k, 3);
        let _forget = Forget(c.chat.id.clone());
        let draw = |c: &mut Chat, k: &Kit| {
            let mut acts = vec![];
            let cx = cx_of(k, &mut acts, true);
            let b = c.transcript(138, &cx);
            (b.iter().flat_map(|b| b.lines.iter().map(|l| l.to_string()).collect::<Vec<_>>()).collect::<Vec<_>>(), b.iter().map(|b| b.hits.len()).sum::<usize>())
        };
        for expanded in [false, true] {
            c.expanded = expanded;
            let first = draw(&mut c, &k);
            let cached = draw(&mut c, &k);
            assert!(c.live.2.len() > 10, "blocks kept: {}", c.live.2.len());
            c.live.2.clear();
            let fresh = draw(&mut c, &k);
            assert_eq!(first, fresh);
            assert_eq!(cached, fresh);
            assert!(fresh.1 > 10, "click targets: {}", fresh.1);
        }
        // the same reply once it's done (drawn in one go) reads the same too
        let live = draw(&mut c, &k).0;
        let stream = c.stream.take();
        let done = draw(&mut c, &k).0;
        assert_eq!(live, done);
        c.stream = stream;
        // a running call animates: its spinner moves from one frame to the next
        deliver(&mut k, &mut c, [Ev::Tool(store::Tool { id: "run1".into(), name: "Bash".into(), label: "Bash".into(), target: "sleep 5".into(), status: "running".into(), ..Default::default() })]);
        let spin = |k: &mut Kit, c: &mut Chat| k.render(c, 140, 44).lines().find(|l| l.contains("Bash(sleep 5)")).map(|l| l.trim().chars().next()).flatten();
        k.time = 1.0;
        let a = spin(&mut k, &mut c);
        k.time = 1.25;
        let b = spin(&mut k, &mut c);
        assert!(a.is_some() && a != b, "{a:?} {b:?}");
        c.stop();
    }

    /// /lead and /tasks hand the goal (or the last reply) to the agents app's lead form / planner, /task makes a
    /// card; the text also goes on the clipboard.
    #[test]
    fn chat_hands_plans_to_agents() {
        let k = Kit::new();
        let plan = "## Plan\n1. split the parser\n2. add tests";
        let mut c = plain_chat(&k, "plan the refactor", plan);
        let run = |c: &mut Chat, line: &str| {
            let mut acts = vec![];
            c.run_command(line, &mut cx_of(&k, &mut acts, true));
            acts.into_iter()
                .filter_map(|a| match a {
                    Action::AppKey(app, key) => Some(format!("key {app} {key}")),
                    Action::AppPaste(app, text) => Some(format!("paste {app} {text}")),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(run(&mut c, "/lead"), [format!("key agents L"), format!("paste agents {plan}")]);
        assert_eq!(crate::clip::last_copied().as_deref(), Some(plan));
        assert_eq!(run(&mut c, "/lead ship the parser"), ["key agents L", "paste agents ship the parser"]);
        assert_eq!(run(&mut c, "/tasks"), [format!("key agents P"), format!("paste agents {plan}")]);
        assert_eq!(run(&mut c, "/task write the docs"), ["key agents n", "paste agents write the docs"]);
        assert!(run(&mut c, "/task").is_empty() && c.info.iter().any(|l| l.contains("/task <what the agent should do>")));
        let mut empty = Chat::new(&k.config);
        assert!(run(&mut empty, "/lead").is_empty() && empty.info.iter().any(|l| l.contains("takes the last reply")));
    }

    /// A new Claude Code / Codex chat that would start in your home folder (or a drive root, or the system) shows a
    /// folder picker instead of suggestions and only runs there once you say so; /cwd completes paths with tab.
    #[test]
    fn chat_new_agent_chat_asks_for_a_folder() {
        let mut k = Kit::new();
        let home = dirs::home_dir().unwrap();
        assert!(risky_dir(&home) && !risky_dir(&home.join("proj")));
        assert!(risky_dir(Path::new(if cfg!(windows) { "C:\\" } else { "/" })));
        if cfg!(windows) {
            assert!(risky_dir(Path::new("C:\\Windows\\System32")) && !risky_dir(Path::new("D:\\work\\thing")));
        } else {
            assert!(risky_dir(Path::new("/usr/bin")) && !risky_dir(Path::new("/srv/thing")));
        }
        let base = std::path::absolute("target/test-scratch/chat/cwd-pick").unwrap();
        let _ = std::fs::remove_dir_all(&base);
        for d in ["alpha/inner", "alpine", "beta"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        let mut c = Chat::new(&k.config);
        c.provider = "claude".into();
        c.chat.provider = Some("claude".into());
        c.launch_dir = home.clone();
        let mut used = store::Chat::new("claude");
        used.cwd = Some(base.join("beta").to_string_lossy().to_string());
        c.chats = vec![used];
        let s = k.render_html(&mut c, 130, 40, "target/snap/chat-folder-pick.html");
        assert!(s.contains("where should Claude Code work?") && s.contains("used before") && s.contains("use it anyway") && !s.contains("rainbows"), "{s}");
        // enter once is held (with a warning), enter again sends it from there
        k.typ(&mut c, "hi");
        k.key(&mut c, KeyCode::Enter);
        assert!(c.chat.messages.is_empty() && c.input == "hi" && c.risky_armed);
        assert!(k.render(&mut c, 130, 40).contains("enter again sends it from"));
        k.key(&mut c, KeyCode::Enter);
        let _forget = Forget(c.chat.id.clone());
        assert!(c.chat.messages.len() == 2 && c.chat.cwd.as_deref() == Some(home.to_string_lossy().as_ref()));
        c.stop();
        // a click on a folder picks it
        c.new_chat();
        k.render(&mut c, 130, 40);
        let r = c.hero_hits.iter().find(|(_, h)| matches!(h, HeroHit::Folder(d) if d.ends_with("beta"))).map(|h| h.0).unwrap();
        k.mouse(&mut c, MouseEvent { column: r.x + 2, ..click(r.y) }, Rect::new(0, 0, 130, 40));
        assert!(c.chat.cwd.as_deref().is_some_and(|d| d.ends_with("beta")) && !c.needs_folder());
        // /cwd completes a path a folder at a time
        let sep = std::path::MAIN_SEPARATOR;
        c.input = format!("/cwd {}{sep}al", base.display());
        c.cursor = c.input.chars().count();
        let items = c.menu();
        assert_eq!(items.iter().take(2).map(|i| i.left.rsplit(sep).nth(1).unwrap_or("")).collect::<Vec<_>>(), ["alpha", "alpine"], "{:?}", items.iter().map(|i| &i.left).collect::<Vec<_>>());
        k.key(&mut c, KeyCode::Tab);
        assert_eq!(c.input, format!("/cwd {}{sep}alpha{sep}", base.display()));
        let items = c.menu();
        assert!(items[0].desc == "this folder" && items.iter().any(|i| i.left.ends_with(&format!("inner{sep}"))));
        k.key(&mut c, KeyCode::Enter);
        assert_eq!(c.chat.cwd.as_deref(), Some(base.join("alpha").to_string_lossy().as_ref()));
    }

    /// The box's bottom edge shows what the chat has cost and how full the AI's plan window is (red past 85%); sending
    /// at effort max that close to the limit says so.
    #[test]
    fn chat_spend_and_plan_usage_on_the_box() {
        let mut k = Kit::new();
        let mut c = agent_chat(&k, "claude", "go");
        let _forget = Forget(c.chat.id.clone());
        deliver(&mut k, &mut c, [Ev::Token("Done.".into()), Ev::Cost(0.5), Ev::Done { note: Some("3s · 1k tokens · $0.500 · claude code".into()) }]);
        assert_eq!(c.chat.messages.last().unwrap().cost_usd, Some(0.5));
        // an older reply only has it in its note
        c.chat.messages.insert(0, store::Msg { role: "assistant".into(), content: "old".into(), note: Some("9s · 2k tokens · $0.340 · 3 turns".into()), ..Default::default() });
        assert!((c.spend() - 0.84).abs() < 1e-9);
        *c.usage.lock().unwrap() = vec![("claude".into(), "5-hour".into(), 72.0), ("claude".into(), "weekly".into(), 20.0), ("codex".into(), "5-hour".into(), 99.0)];
        let s = k.render_html(&mut c, 130, 30, "target/snap/chat-spend.html");
        assert!(s.contains("$0.84 this chat · 5h 72%"), "{s}");
        c.usage.lock().unwrap()[0].2 = 91.0;
        c.effort = "max".into();
        enter(&mut k, &mut c, "and more");
        assert!(c.info.iter().any(|l| l.starts_with("⚠ claude code 5-hour window at 91%")), "{:?}", c.info);
        c.stop();
        // a plain AI shows no plan window
        let p = plain_chat(&k, "hi", "yo");
        *p.usage.lock().unwrap() = vec![("claude".into(), "5-hour".into(), 50.0)];
        assert!(p.usage_text().is_none());
    }

    /// In the / menu a long command is cut with room to spare: two spaces always come before its description.
    #[test]
    fn chat_menu_names_never_run_into_descriptions() {
        let mut k = Kit::new();
        let mut c = Chat::new(&k.config);
        c.input = "/".into();
        c.cursor = 1;
        for w in [60u16, 80, 130] {
            let s = k.render_html(&mut c, w, 40, &format!("target/snap/chat-slash-{w}.html"));
            let row = s.lines().find(|l| l.contains("/effort")).unwrap_or_else(|| panic!("{s}"));
            let after = &row[row.find("/effort").unwrap()..];
            assert!(after.contains("  how hard"), "w={w}: {row}");
            let perms = s.lines().find(|l| l.contains("/perms")).unwrap();
            assert!(perms[perms.find("/perms").unwrap()..].contains("  what coding agents"), "w={w}: {perms}");
        }
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

    fn bash_ask(cmd: &str, tx: &std::sync::mpsc::Sender<approve::Decision>) -> approve::Ask {
        approve::Ask { label: "Bash".into(), target: cmd.into(), body: vec![], rule: approve::rule("Bash", &serde_json::json!({"command": cmd})), reply: tx.clone() }
    }

    fn cx_of<'a>(k: &'a Kit, acts: &'a mut Vec<Action>, focused: bool) -> Cx<'a> {
        Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: acts, focused, time: 1.0 }
    }

    /// An approval or a question popping up while you type doesn't take your keys: typing "can you" doesn't
    /// answer "always allow", and enter queues your words instead of answering the question with them.
    #[test]
    fn chat_prompt_doesnt_eat_typing() {
        let mut k = Kit::new();
        let mut c = agent_chat(&k, "claude", "clean the build");
        let _forget = Forget(c.chat.id.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        deliver(&mut k, &mut c, [Ev::Ask(bash_ask("rm -rf build", &tx))]);
        k.typ(&mut c, "can you");
        assert!(rx.try_recv().is_err(), "typing never answers");
        assert!(c.always.is_empty() && c.input == "can you" && c.asks.len() == 1, "{:?}", c.input);
        // a moment later too: with text in the box, y / n / a are just letters
        ripe(&mut c);
        k.typ(&mut c, " say no");
        assert!(rx.try_recv().is_err() && c.input == "can you say no");
        let s = k.render(&mut c, 130, 30);
        assert!(s.contains("answer the prompt first") && s.contains("clear the box to answer"), "{s}");
        // esc clears the box (the reply keeps going), then y answers
        k.key(&mut c, KeyCode::Esc);
        assert!(c.input.is_empty() && c.stream.is_some());
        k.key(&mut c, KeyCode::Char('y'));
        assert_eq!(rx.try_recv(), Ok(approve::Decision::Allow));
        assert!(c.always.is_empty());
        // a question mid-word: the rest of the word is yours, enter queues it rather than answering
        let (qtx, qrx) = std::sync::mpsc::channel();
        let q = approve::Q { question: "Which one?".into(), header: String::new(), multi: false, options: vec![("A".into(), "".into()), ("B".into(), "".into())] };
        deliver(&mut k, &mut c, [Ev::Question(approve::Question { qs: vec![q], reply: qtx })]);
        k.typ(&mut c, "2 more");
        k.key(&mut c, KeyCode::Enter);
        assert!(qrx.try_recv().is_err() && c.questions.len() == 1);
        assert_eq!(c.queue.iter().map(|q| q.text.as_str()).collect::<Vec<_>>(), ["2 more"]);
        // box empty, the question up a moment: now keys answer it
        ripe(&mut c);
        k.key(&mut c, KeyCode::Char('2'));
        assert_eq!(qrx.try_recv().unwrap().unwrap()["Which one?"], "B");
        c.stop();
    }

    /// Switching chats never stops a reply: it runs on in the background, its events land in its own chat, the
    /// list shows a spinner (? when it needs you), and opening it brings back what it's waiting on.
    #[test]
    fn chat_switching_keeps_replies_running() {
        let mut k = Kit::new();
        let mut c = agent_chat(&k, "claude", "refactor the parser");
        let _forget = Forget(c.chat.id.clone());
        let a = c.chat.id.clone();
        let (stop, inbox) = (c.stream.as_ref().unwrap().stop.clone(), c.stream.as_ref().unwrap().inbox.clone());
        deliver(&mut k, &mut c, [Ev::Token("Looking at the parser.".into())]);
        // esc with something typed clears it; the reply carries on
        k.typ(&mut c, "hmm");
        k.key(&mut c, KeyCode::Esc);
        assert!(c.input.is_empty() && c.stream.is_some(), "esc cleared the box, it didn't stop the reply");
        k.key_mod(&mut c, KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert!(!stop.load(Ordering::SeqCst), "a new chat doesn't stop it");
        assert!(c.stream.is_none() && c.parked.contains_key(&a) && c.badge().is_some());
        let side = k.render_side(&mut c, 40, 12);
        assert!(side.lines().any(|l| l.contains(&format!("{} refactor the parser", SPIN[10 % SPIN.len()]))), "a spinner by the running chat: {side}");
        // its events keep landing in it; an approval shows as ? in the list, and says so
        let (tx, rx) = std::sync::mpsc::channel();
        inbox.lock().unwrap().extend([Ev::Token(" Splitting it up.".into()), Ev::Ask(bash_ask("cargo test", &tx))]);
        k.poll(&mut c);
        assert!(c.chats.iter().find(|x| x.id == a).unwrap().messages.last().unwrap().content.contains("Splitting it up"));
        assert!(k.notices().iter().any(|n| n.contains("wants to: Bash cargo test") && n.contains("refactor the parser")), "{:?}", k.notices());
        let side = k.render_side(&mut c, 40, 12);
        assert!(side.contains("? refactor the parser"), "{side}");
        // back on it: the prompt is there to answer, though not by a key typed the moment it showed up
        c.parked.get_mut(&a).unwrap().since = Instant::now() - Duration::from_secs(5);
        c.open_chat(&a);
        assert!(c.stream.is_some() && c.asks.len() == 1 && c.chat.messages.last().unwrap().content.contains("Splitting it up"));
        k.key(&mut c, KeyCode::Char('n'));
        assert!(rx.try_recv().is_err() && c.input == "n", "a key on the way in goes to the box: {:?}", c.input);
        k.key(&mut c, KeyCode::Esc);
        ripe(&mut c);
        k.key(&mut c, KeyCode::Char('y'));
        assert_eq!(rx.try_recv(), Ok(approve::Decision::Allow));
        c.open_chat(&a); // its own row again: nothing happens
        assert!(c.stream.is_some());
        // finishing in the background: saved, up to the top of the list, and it tells you
        c.new_chat();
        inbox.lock().unwrap().push(Ev::Done { note: None });
        k.poll(&mut c);
        assert!(c.parked.is_empty() && !stop.load(Ordering::SeqCst));
        assert!(k.notices().iter().any(|n| n.contains("finished: refactor the parser")), "{:?}", k.notices());
        assert_eq!(c.chats[0].id, a);
        assert!(store::load_all().iter().any(|x| x.id == a && x.messages.last().is_some_and(|m| m.content.contains("Splitting it up"))));
    }

    /// Closing the pane or quitting stops every reply it started (an approval nobody can give answers no, the
    /// transcript is saved); deleting a chat mid-reply stops it first and leaves nothing behind.
    #[test]
    fn chat_close_and_delete_stop_the_reply() {
        let mut k = Kit::new();
        let mut c = agent_chat(&k, "claude", "rewrite the lexer");
        let _forget = Forget(c.chat.id.clone());
        let (id, stop) = (c.chat.id.clone(), c.stream.as_ref().unwrap().stop.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        deliver(&mut k, &mut c, [Ev::Token("Starting.".into()), Ev::Ask(bash_ask("rm lexer.rs", &tx))]);
        // and one in the background
        c.new_chat();
        let _forget2 = Forget(c.chat.id.clone());
        c.chat.messages.push(store::Msg { role: "user".into(), content: "and the parser".into(), ..Default::default() });
        c.chat.messages.push(store::Msg { role: "assistant".into(), model: Some("claude".into()), ..Default::default() });
        c.stream = Some(test_stream(None));
        let stop2 = c.stream.as_ref().unwrap().stop.clone();
        drop(tx);
        drop(c);
        assert!(stop.load(Ordering::SeqCst) && stop2.load(Ordering::SeqCst), "both replies stopped");
        assert!(rx.recv().is_err(), "the approval answers no");
        assert!(store::load_all().iter().any(|x| x.id == id && x.messages.last().and_then(|m| m.note.clone()).is_some_and(|n| n.contains("stopped"))), "saved as far as it got");

        // ctrl+d mid-reply: the reply stops before the chat goes, and stays gone
        let mut c = agent_chat(&k, "codex", "port it to rust");
        let (id, stop) = (c.chat.id.clone(), c.stream.as_ref().unwrap().stop.clone());
        k.typ(&mut c, "and add tests");
        k.key(&mut c, KeyCode::Enter);
        k.key_mod(&mut c, KeyCode::Char('d'), KeyModifiers::CONTROL);
        k.key(&mut c, KeyCode::Char('y'));
        assert!(stop.load(Ordering::SeqCst) && c.stream.is_none() && c.queue.is_empty() && c.input.is_empty());
        assert!(c.chat.messages.is_empty() && !c.chats.iter().any(|x| x.id == id));
        assert!(!store::load_all().iter().any(|x| x.id == id), "stopping saved it, but the delete came after");
        let s = k.render(&mut c, 120, 30);
        assert!(!s.contains("queue a message"), "the next message isn't queued behind a dead reply: {s}");

        // a run with nothing left to write into ends instead of spinning forever
        let mut c = agent_chat(&k, "claude", "x");
        c.chat.messages.clear();
        deliver(&mut k, &mut c, [Ev::Token("hi".into()), Ev::Done { note: None }]);
        assert!(c.stream.is_none());
    }

    /// Told when you might not be looking: another window has the focus, or another pane does. The tab's status
    /// dot follows the chat on screen.
    #[test]
    fn chat_alerts_when_you_might_not_be_looking() {
        use crate::pane::Activity;
        let mut k = Kit::new();
        let mut c = agent_chat(&k, "claude", "deploy it");
        let _forget = Forget(c.chat.id.clone());
        assert_eq!(c.activity(), Some(Activity::Working));
        assert_eq!(Chat::new(&k.config).activity(), None, "a chat pane that never ran anything has no status");
        k.render(&mut c, 100, 30); // on screen
        let (tx, _rx) = std::sync::mpsc::channel();
        deliver(&mut k, &mut c, [Ev::Ask(bash_ask("kubectl apply", &tx))]);
        assert_eq!(c.activity(), Some(Activity::Blocked));
        assert!(k.notices().is_empty(), "right in front of you: nothing to say");
        // in another window: the approval and the end are alerts (the app turns them into desktop notifications)
        k.render(&mut c, 100, 30);
        crate::app::set_term_focused(false);
        deliver(&mut k, &mut c, [Ev::Ask(bash_ask("kubectl rollout", &tx)), Ev::Done { note: None }]);
        crate::app::set_term_focused(true);
        let n = k.notices();
        assert!(n.iter().any(|x| x.contains("wants to: Bash kubectl rollout")) && n.iter().any(|x| x.contains("finished: deploy it")), "{n:?}");
        assert_eq!(c.activity(), Some(Activity::Idle));
        // another pane focused, this one still on screen
        let mut c = agent_chat(&k, "claude", "deploy it again");
        let _forget2 = Forget(c.chat.id.clone());
        k.render(&mut c, 100, 30);
        c.stream.as_ref().unwrap().inbox.lock().unwrap().push(Ev::Ask(bash_ask("kubectl apply", &tx)));
        let mut acts = vec![];
        c.poll(&mut cx_of(&k, &mut acts, false));
        assert!(acts.iter().any(|a| matches!(a, Action::Alert(crate::alerts::Kind::Approval, s) if s.contains("kubectl apply"))));
        c.stop();
    }

    /// Scrolled up while a reply streams: the lines you're reading stay put, a pill says how much arrived below,
    /// ctrl+end goes back to the end and ctrl+home to the top.
    #[test]
    fn chat_scrolled_up_stays_put() {
        let mut k = Kit::new();
        let mut c = agent_chat(&k, "ollama", "count for me");
        let _forget = Forget(c.chat.id.clone());
        deliver(&mut k, &mut c, [Ev::Token((0..40).map(|i| format!("line {i}\n\n")).collect())]);
        k.render(&mut c, 100, 30);
        c.scroll = 10;
        let top = |s: &str| s.lines().take(8).collect::<Vec<_>>().join("\n");
        let before = top(&k.render(&mut c, 100, 30));
        assert!(before.contains("line "), "{before}");
        deliver(&mut k, &mut c, [Ev::Token((40..60).map(|i| format!("more {i}\n\n")).collect())]);
        let s = k.render(&mut c, 100, 30);
        k.render_html(&mut c, 100, 30, "target/snap/chat-scrolled.html");
        assert_eq!(top(&s), before, "what you were reading didn't move");
        assert!(s.contains("new lines") && s.contains("ctrl+end"), "{s}");
        k.key_mod(&mut c, KeyCode::End, KeyModifiers::CONTROL);
        let s = k.render(&mut c, 100, 30);
        assert!(c.scroll == 0 && s.contains("more 59") && !s.contains("new lines"), "{s}");
        k.key_mod(&mut c, KeyCode::Home, KeyModifiers::CONTROL);
        let s = k.render(&mut c, 100, 30);
        assert!(s.contains("line 0") && !s.contains("more 59"), "{s}");
        c.stop();
    }

    /// While Claude asks: scroll keys still scroll, a paste is your own answer, esc part way through a set sends
    /// the answers you gave (the rest as skipped), and a long choice is cut so its description shows.
    #[test]
    fn chat_question_picker_keys() {
        let mut k = Kit::new();
        let mut c = agent_chat(&k, "claude", "set up the project");
        let _forget = Forget(c.chat.id.clone());
        deliver(&mut k, &mut c, [Ev::Token((0..60).map(|i| format!("step {i}\n\n")).collect())]);
        let (tx, rx) = std::sync::mpsc::channel();
        let q = |question: &str| approve::Q { question: question.into(), header: String::new(), multi: false, options: vec![("A very long option label that goes on and on and on".into(), "its description".into()), ("B".into(), "".into())] };
        deliver(&mut k, &mut c, [Ev::Question(approve::Question { qs: vec![q("Name?"), q("License?"), q("CI?")], reply: tx })]);
        ripe(&mut c);
        let s = k.render(&mut c, 70, 40);
        assert!(s.contains("its description"), "{s}");
        k.key(&mut c, KeyCode::PageUp);
        k.key_mod(&mut c, KeyCode::Up, KeyModifiers::CONTROL);
        assert_eq!((c.scroll, c.qs.sel), (11, 0), "scrolled, and the choice didn't move");
        let mut acts = vec![];
        c.paste("oriel\r\n", &mut cx_of(&k, &mut acts, true));
        assert_eq!((c.qs.other.as_deref(), c.input.as_str()), (Some("oriel "), ""));
        k.key(&mut c, KeyCode::Enter);
        k.key(&mut c, KeyCode::Esc);
        let ans = rx.try_recv().unwrap().unwrap();
        assert_eq!((ans["Name?"].as_str(), ans["License?"].as_str(), ans["CI?"].as_str()), (Some("oriel"), Some("(skipped)"), Some("(skipped)")));
        assert!(c.stream.is_some() && c.questions.is_empty());
        c.stop();
    }

    /// The end of plan mode: the plan shows in the transcript, you pick how it starts, and the chat (and the
    /// running Claude Code) switch to that mode.
    #[test]
    fn chat_plan_approval() {
        let mut k = Kit::new();
        let mut c = agent_chat(&k, "claude", "plan the refactor");
        let _forget = Forget(c.chat.id.clone());
        c.perms = "plan".into();
        let (steer_tx, steer_rx) = std::sync::mpsc::channel();
        c.stream.as_mut().unwrap().steer = Some(steer_tx);
        c.stream.as_mut().unwrap().perms = "plan".into();
        let (tx, rx) = std::sync::mpsc::channel();
        let plan = "## Split the parser\n\n1. move the lexer to `lexer.rs`\n2. add tests";
        deliver(&mut k, &mut c, [Ev::Plan(plan.into()), Ev::Question(approve::Question { qs: vec![approve::plan_question()], reply: tx })]);
        ripe(&mut c);
        let s = k.render_html(&mut c, 120, 40, "target/snap/chat-plan.html");
        assert!(s.contains("Split the parser") && s.contains("move the lexer to lexer.rs") && s.contains("Start on it?") && s.contains("No, keep planning"), "{s}");
        k.key(&mut c, KeyCode::Char('1'));
        let ans = rx.try_recv().unwrap().unwrap();
        assert_eq!(ans.values().next().and_then(|v| v.as_str()), Some(approve::PLAN_CHOICES[0].0));
        deliver(&mut k, &mut c, [Ev::Perms("edits".into())]);
        assert_eq!((c.perms.as_str(), c.stream.as_ref().unwrap().perms.as_str()), ("edits", "edits"));
        assert!(matches!(steer_rx.try_recv(), Ok(providers::Steer::Mode(m)) if m == "edits"));
        c.stop();
    }

    /// shift+tab mid-reply reaches a running Claude Code at once; Codex can't change mid-run, and the badge says so.
    #[test]
    fn chat_mode_change_mid_reply() {
        let mut k = Kit::new();
        let mut c = agent_chat(&k, "claude", "tidy up");
        let _forget = Forget(c.chat.id.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        c.stream.as_mut().unwrap().steer = Some(tx);
        c.perms = "bypass".into();
        c.stream.as_mut().unwrap().perms = "bypass".into();
        k.key_mod(&mut c, KeyCode::BackTab, KeyModifiers::SHIFT);
        assert!(matches!(rx.try_recv(), Ok(providers::Steer::Mode(m)) if m == "ask"));
        assert_eq!(c.stream.as_ref().unwrap().perms, "ask");
        assert!(!k.render(&mut c, 120, 30).contains("from your next message"));
        c.stop();
        let mut c = agent_chat(&k, "codex", "tidy up");
        let _forget2 = Forget(c.chat.id.clone());
        k.key_mod(&mut c, KeyCode::BackTab, KeyModifiers::SHIFT);
        let s = k.render(&mut c, 120, 30);
        assert!(s.contains("auto mode on") && s.contains("from your next message"), "{s}");
        c.stop();
    }

    /// Regenerating goes back to before the reply you're throwing away (a resumed CLI session would see your
    /// message twice), and what goes to an AI never has an empty message or loses what you queued mid-reply.
    #[test]
    fn chat_retry_and_history() {
        let mut k = Kit::new();
        let mut c = agent_chat(&k, "claude", "fix the bug");
        let _forget = Forget(c.chat.id.clone());
        // first reply: it made the session and edited a file
        deliver(&mut k, &mut c, [Ev::State("claude.session".into(), "s1".into()), Ev::Tool(store::Tool { id: "t1".into(), name: "Edit".into(), label: "Update".into(), target: "a.rs".into(), status: "done".into(), ..Default::default() }), Ev::Done { note: None }]);
        assert_eq!(c.chat.state["claude"]["upto"], 2, "the session has seen the chat up to here");
        let regen = |k: &mut Kit, c: &mut Chat| k.key_mod(c, KeyCode::Char('g'), KeyModifiers::CONTROL);
        regen(&mut k, &mut c);
        assert!(!c.chat.state.contains_key("claude"), "the session only knew the thrown-away reply: start afresh");
        assert!(c.info.iter().any(|l| l.contains("still changed")), "{:?}", c.info);
        assert_eq!(c.chat.messages.len(), 2);
        c.stop();
        // a later reply resumed the same session: that session has the bad reply in it too, so it's dropped
        c.chat.messages.truncate(1);
        c.chat.state = serde_json::from_value(serde_json::json!({"claude": {"session": "s1", "upto": 2}})).unwrap();
        c.chat.messages.push(store::Msg { role: "assistant".into(), content: "done".into(), model: Some("claude".into()), ..Default::default() });
        c.chat.messages.push(store::Msg { role: "user".into(), content: "and the other one".into(), ..Default::default() });
        let before = c.chat.state.clone();
        c.chat.messages.push(store::Msg { role: "assistant".into(), content: "bad".into(), model: Some("claude".into()), state_before: Some(before), ..Default::default() });
        regen(&mut k, &mut c);
        assert!(!c.chat.state.contains_key("claude"));
        c.stop();
        // a CLI that forks sessions: the one from before is intact, so it goes back to it
        c.chat.messages.truncate(1);
        c.chat.state = serde_json::from_value(serde_json::json!({"claude": {"session": "s2"}})).unwrap();
        c.chat.messages.push(store::Msg { role: "assistant".into(), content: "bad".into(), model: Some("claude".into()), state_before: Some(serde_json::from_value(serde_json::json!({"claude": {"session": "s1"}})).unwrap()), ..Default::default() });
        regen(&mut k, &mut c);
        assert_eq!(c.chat.state["claude"]["session"], "s1");
        c.stop();

        // history: a tool-only reply says what it did; a message queued mid-reply is a turn of its own
        let mut ch = store::Chat::new("anthropic");
        let tool = |label: &str| store::Part::Tool(store::Tool { label: label.into(), ..Default::default() });
        ch.messages.push(store::Msg { role: "user".into(), content: "go".into(), ..Default::default() });
        ch.messages.push(store::Msg { role: "assistant".into(), parts: vec![tool("Update"), tool("Update"), tool("Bash")], ..Default::default() });
        ch.messages.push(store::Msg { role: "user".into(), content: "more".into(), ..Default::default() });
        ch.messages.push(store::Msg {
            role: "assistant".into(),
            content: "On it.Done.".into(),
            parts: vec![store::Part::Text { text: "On it.".into() }, store::Part::User { text: "use tabs".into() }, store::Part::Text { text: "Done.".into() }],
            ..Default::default()
        });
        ch.messages.push(store::Msg { role: "user".into(), content: "thanks".into(), ..Default::default() });
        let (h, since) = history(&ch, "anthropic");
        let roles: Vec<&str> = h.iter().map(|m| m.0.as_str()).collect();
        assert_eq!(roles, ["user", "assistant", "user", "assistant", "user", "assistant", "user"]);
        assert_eq!(h[1].1, "(edited 2 files, ran 1 command)");
        assert_eq!((h[3].1.as_str(), h[4].1.as_str(), h[5].1.as_str()), ("On it.", "use tabs", "Done."));
        assert!(h.iter().all(|m| !m.1.trim().is_empty()) && since.is_none());
        // Claude's session last replied at message 2; Codex talked since: that's what Claude catches up on
        ch.state = serde_json::from_value(serde_json::json!({"claude": {"session": "s", "upto": 2}})).unwrap();
        let (h, since) = history(&ch, "claude");
        assert_eq!(since, Some(2));
        assert_eq!(h[2].1, "more");
    }

    // ------------------------------------------------------------ checkpoints, checks, context, handoffs

    /// Poll until `done` holds (background git work and checks report back through the pane's poll).
    fn wait_until(k: &mut Kit, c: &mut Chat, what: &str, mut done: impl FnMut(&Chat) -> bool) {
        let t0 = Instant::now();
        while !done(c) {
            assert!(t0.elapsed() < Duration::from_secs(60), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(15));
            k.poll(c);
        }
    }

    /// A folder of its own under target/test-scratch (the only place tests take checkpoints), with no leftover
    /// shadow history from an earlier run.
    fn ckpt_folder(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let d = std::path::absolute(format!("target/test-scratch/chat/flow/{name}")).unwrap();
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for (f, text) in files {
            std::fs::write(d.join(f), text).unwrap();
        }
        if let Some(s) = ckpt::Place::of(&d).unwrap().shadow {
            let _ = std::fs::remove_dir_all(s);
        }
        d
    }

    /// A Claude Code chat working in `dir`, nothing running.
    fn coding_chat(k: &Kit, dir: &Path) -> Chat {
        let mut c = Chat::new(&k.config);
        c.provider = "claude".into();
        c.chat.provider = Some("claude".into());
        c.chat.cwd = Some(dir.to_string_lossy().to_string());
        c.avail = vec!["claude", "codex", "ollama"];
        c
    }

    /// Send a message and let its reply end (in tests the AI answers with an error straight away; the folder's
    /// checkpoint is taken first all the same).
    fn turn(k: &mut Kit, c: &mut Chat, text: &str) {
        enter(k, c, text);
        wait_until(k, c, "the reply to end", |c| c.stream.is_none());
    }

    /// Checkpoints end to end: each coding-agent turn snapshots the folder first; the reply's note says what the
    /// turn changed; /diff shows it all file by file; /undo puts the folder back a turn at a time (listing the
    /// files it deletes, and only after y); /rewind lists your turns and d shows one's diff.
    #[test]
    fn chat_checkpoints_diff_undo_rewind() {
        let mut k = Kit::new();
        let d = ckpt_folder("undo", &[("a.txt", "one\n"), ("b.txt", "keep\n")]);
        let mut c = coding_chat(&k, &d);
        let _forget = Forget(c.chat.id.clone());
        turn(&mut k, &mut c, "change a");
        assert!(c.chat.messages[1].ckpt.is_some(), "a checkpoint before the first turn");
        // the agent's work in turn 1: an edit and a new file; then the reply ends
        std::fs::write(d.join("a.txt"), "ONE\nmore\n").unwrap();
        std::fs::write(d.join("new.txt"), "fresh\n").unwrap();
        {
            let id = c.chat.id.clone();
            let mut acts = vec![];
            let mut cx = cx_of(&k, &mut acts, true);
            c.turn_ended(&id, true, &mut cx);
        }
        wait_until(&mut k, &mut c, "the note's numbers", |c| c.chat.messages[1].note.as_deref().is_some_and(|n| n.contains("files")));
        let note = c.chat.messages[1].note.clone().unwrap();
        assert!(note.contains("+3 −1 · 2 files"), "{note}");
        turn(&mut k, &mut c, "more");
        assert!(c.chat.messages[3].ckpt.is_some());
        std::fs::remove_file(d.join("b.txt")).unwrap(); // turn 2 deleted a file
        // /diff: everything since the first checkpoint
        enter(&mut k, &mut c, "/diff");
        wait_until(&mut k, &mut c, "the diff", |c| c.review.as_ref().is_some_and(|v| v.data.is_some()));
        let s = k.render_html(&mut c, 120, 30, "target/snap/chat-diff.html");
        assert!(s.contains("everything this chat changed") && s.contains("new.txt") && s.contains("b.txt") && s.contains("3 files"), "{s}");
        k.key(&mut c, KeyCode::Down);
        assert_eq!(c.review.as_ref().unwrap().file, 1);
        // a click on a file row picks it
        let row = c.review.as_ref().unwrap().hits[2].0;
        k.mouse(&mut c, MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: row.x + 2, row: row.y, modifiers: KeyModifiers::NONE }, Rect::new(0, 0, 120, 30));
        assert_eq!(c.review.as_ref().unwrap().file, 2);
        k.key(&mut c, KeyCode::Esc);
        assert!(c.review.is_none());
        // /undo: back to before turn 2 (b.txt comes back), nothing happens until y
        enter(&mut k, &mut c, "/undo");
        wait_until(&mut k, &mut c, "the restore plan", |c| c.restore.as_ref().is_some_and(|r| r.plan.is_some()));
        let s = k.render(&mut c, 120, 30);
        assert!(s.contains("before turn 2") && s.contains("1 file comes back as it was"), "{s}");
        assert!(!d.join("b.txt").exists(), "not before y");
        k.key(&mut c, KeyCode::Char('y'));
        wait_until(&mut k, &mut c, "the restore", |c| c.restore.is_none());
        assert_eq!(std::fs::read_to_string(d.join("b.txt")).unwrap(), "keep\n");
        assert!(d.join("new.txt").exists(), "turn 1's work is still there");
        // /undo again: nothing left to undo in turn 2, so back to before turn 1 — new.txt is listed and deleted
        enter(&mut k, &mut c, "/undo");
        wait_until(&mut k, &mut c, "the second plan", |c| c.restore.as_ref().is_some_and(|r| r.plan.is_some()));
        let s = k.render_html(&mut c, 120, 30, "target/snap/chat-undo.html");
        assert!(s.contains("before turn 1") && s.contains("1 file made since is deleted: new.txt"), "{s}");
        k.key(&mut c, KeyCode::Char('y'));
        wait_until(&mut k, &mut c, "the second restore", |c| c.restore.is_none());
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "one\n");
        assert!(!d.join("new.txt").exists());
        assert!(c.info.iter().any(|l| l.contains("back to before turn 1")), "{:?}", c.info);
        // /rewind lists your turns; picking one asks, and d shows its diff instead
        c.input = "/rewind ".into();
        c.cursor = 8;
        let items: Vec<String> = c.menu().into_iter().map(|m| m.left).collect();
        assert_eq!(items.len(), 2, "{items:?}");
        assert!(items[0].starts_with("turn 2 · more") && items[1].starts_with("turn 1 · change a"), "{items:?}");
        c.input.clear();
        c.cursor = 0;
        std::fs::write(d.join("a.txt"), "changed again\n").unwrap();
        enter(&mut k, &mut c, "/rewind 1");
        wait_until(&mut k, &mut c, "the rewind plan", |c| c.restore.as_ref().is_some_and(|r| r.plan.is_some()));
        k.key(&mut c, KeyCode::Char('d'));
        assert!(c.restore.is_none() && c.review.is_some());
        wait_until(&mut k, &mut c, "the rewind diff", |c| c.review.as_ref().is_some_and(|v| v.data.is_some()));
        let files = c.review.as_ref().unwrap().data.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["a.txt"]);
        k.key(&mut c, KeyCode::Esc);
        // a chat that never ran a coding agent has nothing to go back to
        let mut p = plain_chat(&k, "hi", "yo");
        enter(&mut k, &mut p, "/undo");
        assert!(p.info.iter().any(|l| l.contains("no checkpoints")), "{:?}", p.info);
    }

    /// /check: oriel runs the check itself after each turn; a failure goes back to the agent as the next message
    /// (exit code and first lines), a pass stops the loop with an alert and puts ✓ on the reply. A loop with no
    /// progress stalls, one that keeps failing stops at its cap; both tell you. Only for coding agents.
    #[test]
    fn chat_check_loop() {
        let mut k = Kit::new();
        let d = ckpt_folder("check", &[("want.txt", "yes\n"), ("got.txt", "no\n")]);
        let mut c = coding_chat(&k, &d);
        let _forget = Forget(c.chat.id.clone());
        c.chat.messages.push(user("make got.txt say yes"));
        c.chat.messages.push(store::Msg { role: "assistant".into(), content: "Done: got.txt is fixed.".into(), model: Some("claude".into()), ..Default::default() });
        let cmd = "git diff --no-index want.txt got.txt";
        enter(&mut k, &mut c, &format!("/check {cmd}"));
        assert_eq!(check_of(&c.chat).map(|k| k.status), Some("on".into()));
        assert!(c.check_status().is_some_and(|s| s.contains("running")), "the check runs on the reply already there");
        wait_until(&mut k, &mut c, "the check's verdict", |c| c.chat.messages.len() > 2);
        let sent = c.chat.messages[2].content.clone();
        assert!(sent.starts_with("◎ check 1 of 20: not yet") && sent.contains("exit 1") && sent.contains("-yes") && sent.contains("+no") && sent.contains(cmd), "{sent}");
        assert!(c.chat.messages[1].verified.as_deref().is_some_and(|v| v.starts_with('✗')), "the reply it checked gets ✗");
        wait_until(&mut k, &mut c, "that reply to end", |c| c.stream.is_none());
        let s = k.render_html(&mut c, 120, 30, "target/snap/chat-check.html");
        assert!(s.contains("◎ check · 1/20 · last: exit 1"), "{s}");
        // the agent fixes it; its turn ends; the check passes
        std::fs::write(d.join("got.txt"), "yes\n").unwrap();
        let last = c.chat.messages.len() - 1;
        c.chat.messages[last] = store::Msg { role: "assistant".into(), content: "Fixed it.".into(), model: Some("claude".into()), ..Default::default() };
        {
            let id = c.chat.id.clone();
            let mut acts = vec![];
            let mut cx = cx_of(&k, &mut acts, true);
            c.turn_ended(&id, true, &mut cx);
        }
        wait_until(&mut k, &mut c, "the passing check", |c| check_of(&c.chat).is_some_and(|k| k.status != "on"));
        let ck = check_of(&c.chat).unwrap();
        assert_eq!((ck.status.as_str(), ck.turn), ("passed", 2));
        assert!(k.notices().iter().any(|n| n.contains("check passed after 2 checks")), "{:?}", k.notices());
        assert!(c.chat.messages[last].verified.as_deref().is_some_and(|v| v.starts_with("✓ checked") && v.contains(cmd)));
        assert_eq!(remembered_check(&d).as_deref(), Some(cmd), "remembered for this folder");
        // a loop that changes nothing stalls after 3 turns; one that keeps failing stops at its cap
        let id = c.chat.id.clone();
        let fail = || Some((cmd.to_string(), Err::<(), String>(format!("`{cmd}` failed (exit 1):\n-no\n+yes"))));
        for (max, turns, want) in [(20, 4, "stalled"), (2, 2, "capped")] {
            set_check(&mut c.chat, &Check { cmd: cmd.into(), turn: 0, max, status: "on".into(), ..Default::default() });
            let mut acts = vec![];
            for n in 0..turns {
                c.stream = None;
                let mut cx = cx_of(&k, &mut acts, true);
                // the folder changes every turn under the cap; never in the stall
                let tree = if want == "capped" { format!("t{n}") } else { "same".into() };
                c.on_turn(&id, last, None, Some(tree), fail(), false, &mut cx);
            }
            assert_eq!(check_of(&c.chat).unwrap().status, want);
            let said: Vec<String> = acts.iter().filter_map(|a| if let Action::Alert(crate::alerts::Kind::NeedsYou, s) = a { Some(s.clone()) } else { None }).collect();
            assert_eq!(said.len(), 1, "{want}: {said:?}");
            assert!(said[0].contains(if want == "stalled" { "stalled" } else { "still failing after 2" }), "{said:?}");
        }
        c.stream = None;
        enter(&mut k, &mut c, "/check off");
        assert!(c.info.iter().any(|l| l.contains("no check loop is on")), "{:?}", c.info);
        // not for chats without tools
        let mut p = plain_chat(&k, "hi", "yo");
        enter(&mut k, &mut p, "/check cargo test");
        assert!(p.info.iter().any(|l| l.contains("coding agent")) && check_of(&p.chat).is_none(), "{:?}", p.info);
    }

    /// /verify runs the check now and the last reply's badge says what it found (the command is remembered for the
    /// folder, so a bare /verify runs it again); not while a reply is running.
    #[test]
    fn chat_verify_badges_the_last_reply() {
        let mut k = Kit::new();
        let d = ckpt_folder("verify", &[("ok.txt", "x
")]);
        let mut c = coding_chat(&k, &d);
        let _forget = Forget(c.chat.id.clone());
        c.chat.messages.push(user("tidy it"));
        let edit = store::Part::Tool(store::Tool { label: "Update".into(), target: "ok.txt".into(), status: "done".into(), ..Default::default() });
        c.chat.messages.push(store::Msg { role: "assistant".into(), model: Some("claude".into()), content: "Done, all tests pass.".into(), parts: vec![edit, store::Part::Text { text: "Done, all tests pass.".into() }], ..Default::default() });
        let s = k.render(&mut c, 120, 24);
        assert!(s.contains("○ not run: nothing checked this work"), "{s}");
        enter(&mut k, &mut c, "/verify git --version");
        wait_until(&mut k, &mut c, "the check", |c| c.chat.messages[1].verified.is_some());
        assert!(c.chat.messages[1].verified.as_deref().unwrap().starts_with("✓ checked") && c.info.iter().any(|l| l == "✓ git --version passes"), "{:?}", c.info);
        let s = k.render(&mut c, 120, 24);
        assert!(s.contains("✓ checked") && s.contains("git --version") && !s.contains("not run"), "{s}");
        assert_eq!(remembered_check(&d).as_deref(), Some("git --version"));
        c.stream = Some(test_stream(None));
        enter(&mut k, &mut c, "/verify");
        assert!(c.info.iter().any(|l| l.contains("still running")), "{:?}", c.info);
    }

    /// The badge under a finished coding agent's reply: ✓ when a check ran after its last edit, ✗ when that check
    /// failed, stale when files were edited after it, "not run" when it claims it's done but nothing checked it;
    /// /verify's result wins. Nothing for a reply that neither edited nor claims anything.
    #[test]
    fn chat_check_badges() {
        let tool = |label: &str, target: &str, status: &str, summary: &str| store::Part::Tool(store::Tool { label: label.into(), target: target.into(), status: status.into(), summary: summary.into(), ..Default::default() });
        let msg = |parts: Vec<store::Part>, text: &str| {
            let mut parts = parts;
            parts.push(store::Part::Text { text: text.into() });
            store::Msg { role: "assistant".into(), model: Some("claude".into()), content: text.into(), parts, ..Default::default() }
        };
        let edit = || tool("Update", "src/lib.rs", "done", "+3 -1");
        let v = |m: &store::Msg| badge(m, None).map(|b| b.0);
        assert_eq!(v(&msg(vec![edit(), tool("Bash", "cargo test", "done", "12 lines")], "All tests pass.")), Some(Verdict::Checked));
        assert_eq!(v(&msg(vec![edit(), tool("Bash", "cargo test", "error", "exit 101")], "Done.")), Some(Verdict::Failed));
        let stale = msg(vec![tool("Bash", "cargo test", "done", "12 lines"), edit(), tool("Write", "src/new.rs", "done", "")], "Tests pass now.");
        let b = badge(&stale, None).unwrap();
        assert_eq!(b.0, Verdict::Stale);
        assert!(b.1.contains("2 files edited since cargo test ran"), "{}", b.1);
        assert_eq!(v(&msg(vec![edit()], "Implemented it, all tests pass.")), Some(Verdict::NotRun));
        assert_eq!(v(&msg(vec![edit()], "Here's a first pass; want me to go on?")), None, "an edit that claims nothing: no badge");
        assert_eq!(v(&msg(vec![tool("Read", "src/lib.rs", "done", "")], "It reads a file.")), None);
        // a subagent's check counts; a Bash that isn't a check doesn't
        let mut agent = store::Tool { label: "Agent".into(), status: "done".into(), ..Default::default() };
        agent.children.push(store::Tool { label: "Bash".into(), target: "npm test".into(), status: "done".into(), ..Default::default() });
        assert_eq!(v(&msg(vec![edit(), store::Part::Tool(agent)], "Done.")), Some(Verdict::Checked));
        assert_eq!(v(&msg(vec![edit(), tool("Bash", "ls -la", "done", "")], "All done.")), Some(Verdict::NotRun));
        // the chat's own check command decides what counts
        assert_eq!(badge(&msg(vec![edit(), tool("Bash", "cargo build", "done", "")], "Done."), Some("cargo test -q")).map(|b| b.0), Some(Verdict::NotRun));
        let mut m = msg(vec![edit()], "Done.");
        m.verified = Some("✓ checked 14:02 · cargo test".into());
        assert_eq!(badge(&m, None), Some((Verdict::Checked, "✓ checked 14:02 · cargo test".into())));
        // on screen, under the reply
        let mut k = Kit::new();
        let mut c = agent_chat(&k, "claude", "fix it");
        c.stream = None;
        let last = c.chat.messages.len() - 1;
        c.chat.messages[last] = stale;
        let s = k.render_html(&mut c, 120, 30, "target/snap/chat-badge.html");
        assert!(s.contains("stale: 2 files edited since cargo test ran · /verify"), "{s}");
    }

    /// Claude Code's own slash commands go to it as the message (/goal first of all); for other AIs /goal says
    /// what to use instead, and an unknown command is still unknown.
    #[test]
    fn chat_slash_commands_pass_through_to_claude() {
        let mut k = Kit::new();
        let mut c = Chat::new(&k.config);
        c.provider = "claude".into();
        c.chat.provider = Some("claude".into());
        c.chat.cwd = Some(std::env::temp_dir().to_string_lossy().to_string());
        let _forget = Forget(c.chat.id.clone());
        enter(&mut k, &mut c, "/goal all tests pass");
        assert_eq!(c.chat.messages.first().map(|m| m.content.as_str()), Some("/goal all tests pass"));
        c.stop();
        enter(&mut k, &mut c, "/my-skill do it");
        assert_eq!(c.chat.messages.iter().filter(|m| m.role == "user").last().map(|m| m.content.as_str()), Some("/my-skill do it"));
        c.stop();
        let mut x = agent_chat(&k, "codex", "hi");
        x.stream = None;
        enter(&mut k, &mut x, "/goal tests pass");
        assert!(x.info.iter().any(|l| l.contains("/goal is Claude Code's") && l.contains("/check")), "{:?}", x.info);
        let mut o = plain_chat(&k, "hi", "yo");
        enter(&mut k, &mut o, "/frobnicate");
        assert!(o.info.iter().any(|l| l.contains("unknown command /frobnicate")), "{:?}", o.info);
    }

    /// How full the context is: Claude Code's usage (fresh + cached input) over its window, `[1m]` a million; Codex's
    /// from its session log. On the box as "ctx 39%", and past 60% (or after a compaction, or with the 5-hour window
    /// nearly used up) a hint to /handoff.
    #[test]
    fn chat_context_meter_and_hints() {
        let mut p = agent::Claude::new(Path::new("C:\\work\\demo"));
        let lines = [
            r#"{"type":"system","subtype":"init","model":"claude-opus-4-5[1m]","session_id":"s"}"#,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"usage":{"input_tokens":2000,"cache_read_input_tokens":300000,"cache_creation_input_tokens":8000,"output_tokens":1}}}}"#,
        ];
        let mut got = vec![];
        for l in lines {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            p.feed(&v, 0, &mut |e| got.push(e)).unwrap();
        }
        assert!(got.iter().any(|e| matches!(e, Ev::Context(310_000, 1_000_000))), "{:?}", got.iter().filter(|e| matches!(e, Ev::Context(..))).count());
        assert_eq!(agent::context_window("sonnet"), 200_000);
        let log = [
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":90000},"last_token_usage":{"input_tokens":50000,"cached_input_tokens":40000,"output_tokens":900,"total_tokens":50900},"model_context_window":272000}}}"#,
            r#"{"type":"response_item","payload":{"type":"message"}}"#,
        ]
        .join("\n");
        assert_eq!(agent::codex_context(&log), Some((50_900, 272_000)));
        // the session log is found by thread id, newest day first
        let sessions = std::path::absolute("target/test-scratch/chat/codex-sessions").unwrap();
        let _ = std::fs::remove_dir_all(&sessions);
        let day = sessions.join("2026").join("09").join("26");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("rollout-2026-09-26T10-00-00-abc123.jsonl"), &log).unwrap();
        assert_eq!(agent::codex_log(&sessions, "abc123").map(|f| f.file_name().unwrap().to_string_lossy().to_string()).as_deref(), Some("rollout-2026-09-26T10-00-00-abc123.jsonl"));
        assert!(agent::codex_log(&sessions, "zzz").is_none());
        // on the box
        let mut k = Kit::new();
        let mut c = agent_chat(&k, "claude", "go");
        deliver(&mut k, &mut c, [Ev::Context(78_000, 200_000), Ev::Token("ok".into()), Ev::Done { note: None }]);
        let s = k.render(&mut c, 120, 24);
        assert!(s.contains("ctx 39%") && !s.contains("/handoff?"), "{s}");
        let last = c.chat.messages.len() - 1;
        c.chat.messages[last].ctx = Some((130_000, 200_000));
        let s = k.render_html(&mut c, 120, 24, "target/snap/chat-context.html");
        assert!(s.contains("ctx 65%") && s.contains("context 65% · /handoff?"), "{s}");
        // another AI wrote the last reply: its numbers aren't this one's
        c.chat.messages[last].model = Some("codex".into());
        assert!(c.context_fill().is_none());
        c.chat.messages[last].model = Some("claude".into());
        c.chat.messages[last].ctx = Some((20_000, 200_000));
        c.chat.messages[last].parts.push(store::Part::Mark { text: "conversation compacted (from 180k tokens)".into() });
        assert_eq!(c.handoff_hint().as_deref(), Some("compacted · /handoff?"));
        c.chat.messages[last].parts.pop();
        c.avail = vec!["claude", "codex"];
        *c.usage.lock().unwrap() = vec![("claude".into(), "5-hour".into(), 93.0), ("codex".into(), "5-hour".into(), 10.0)];
        assert_eq!(c.handoff_hint().as_deref(), Some("5h 93% · /handoff codex?"));
    }

    /// /handoff: the AI writes a card; when it's done a fresh chat (here with Codex) starts from it, the card and
    /// the next goal as its first message, the old chat kept and linked as its parent (↳ in the list). A card that
    /// doesn't come back says so.
    #[test]
    fn chat_handoff_to_a_fresh_chat() {
        let mut k = Kit::new();
        let mut c = agent_chat(&k, "claude", "build the parser");
        c.avail = vec!["claude", "codex"];
        deliver(&mut k, &mut c, [Ev::Token("parser half done".into()), Ev::Done { note: None }]);
        let parent = c.chat.id.clone();
        let _forget = Forget(parent.clone());
        enter(&mut k, &mut c, "/handoff finish the parser codex");
        let ask = c.chat.messages.iter().rev().find(|m| m.role == "user").unwrap().content.clone();
        assert!(ask.starts_with("Write a handoff card") && ask.contains("goal: finish the parser"), "{ask}");
        assert!(c.info.iter().any(|l| l.contains("a fresh codex chat starts from it")), "{:?}", c.info);
        // the card comes back, with a message you queued meanwhile answered after it
        c.stream = Some(test_stream(None));
        c.queue.push(Queued { text: "also note the lexer's quirks".into(), sent: false });
        deliver(&mut k, &mut c, [Ev::Token("**State** lexer works\n**Next** 1. the parser".into()), Ev::Done { note: None }]);
        assert_eq!(c.chat.id, parent, "the queued message is answered first");
        c.stream = Some(test_stream(None));
        deliver(&mut k, &mut c, [Ev::Token("Noted.".into()), Ev::Done { note: None }]);
        assert_ne!(c.chat.id, parent, "a fresh chat is on screen");
        let _forget2 = Forget(c.chat.id.clone());
        assert_eq!(c.chat.provider.as_deref(), Some("codex"));
        assert_eq!(c.chat.extra.get("parent").and_then(|v| v.as_str()), Some(parent.as_str()));
        let first = &c.chat.messages[0].content;
        assert!(first.starts_with("Handoff from an earlier session") && first.contains("**State** lexer works") && !first.contains("Noted") && first.ends_with("Next: finish the parser"), "{first}");
        assert!(c.chats.iter().any(|x| x.id == parent), "the old chat stays in the list");
        c.stop();
        c.persist();
        let side = k.render_side(&mut c, 32, 12);
        assert!(side.contains("↳"), "{side}");
        // a failed card
        let mut c = agent_chat(&k, "claude", "go");
        deliver(&mut k, &mut c, [Ev::Token("ok".into()), Ev::Done { note: None }]);
        let _forget3 = Forget(c.chat.id.clone());
        let id = c.chat.id.clone();
        enter(&mut k, &mut c, "/handoff");
        c.stream = Some(test_stream(None));
        deliver(&mut k, &mut c, [Ev::Error("network down".into())]);
        assert_eq!(c.chat.id, id, "no fresh chat");
        assert!(c.info.iter().any(|l| l.contains("handoff card didn't come back")), "{:?}", c.info);
    }

    /// Every screen the chat can show draws at any size without a panic, down to a single cell: the new-chat
    /// picker, a reply with a question, an approval, queued messages and todos, the restore prompt, the diff view,
    /// the / menu and a tall composer.
    #[test]
    fn chat_small_sizes_dont_panic() {
        let mut k = Kit::new();
        let sizes: Vec<(u16, u16)> = [1u16, 2, 5, 9, 20, 33, 45, 61, 80].iter().flat_map(|&w| [1u16, 2, 3, 4, 5, 6, 8, 11, 20].map(|h| (w, h))).collect();
        let all = |c: &mut Chat, k: &mut Kit, what: &str| {
            for &(w, h) in &sizes {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    k.render(c, w, h);
                    k.render_side(c, w, h);
                }));
                assert!(r.is_ok(), "{what} at {w}x{h}");
            }
        };
        // the new agent chat's folder picker
        let mut c = Chat::new(&k.config);
        c.provider = "claude".into();
        c.chat.provider = Some("claude".into());
        c.launch_dir = dirs::home_dir().unwrap_or_default();
        all(&mut c, &mut k, "folder picker");
        // a reply with everything pinned above the box
        let mut c = long_live_chat(&mut k, 1);
        let _forget = Forget(c.chat.id.clone());
        let (tx, _rx) = std::sync::mpsc::channel();
        let (qtx, _qrx) = std::sync::mpsc::channel();
        let q = approve::Q { question: "Which `license` should the new crate use?".into(), header: "License".into(), multi: true, options: vec![("MIT".into(), "short".into()), ("Apache-2.0 with a very long label".into(), "patents".into())] };
        deliver(&mut k, &mut c, [Ev::Question(approve::Question { qs: vec![q.clone(), q], reply: qtx }), Ev::Ask(bash_ask("cargo publish --dry-run", &tx))]);
        c.queue.push(Queued { text: "and a README\nwith two lines".into(), sent: false });
        c.scroll = 5;
        all(&mut c, &mut k, "live reply");
        c.input = "a draft\nover\nseveral\nlines that go on for a while so they wrap in a narrow box".into();
        c.cursor = 7;
        all(&mut c, &mut k, "live reply with a draft");
        c.input = "/".into();
        c.cursor = 1;
        all(&mut c, &mut k, "the / menu");
        c.input.clear();
        c.cursor = 0;
        c.stop();
        // the restore prompt, then the diff view
        c.restore = Some(Restore { chat: c.chat.id.clone(), idx: 1, ckpt: "abc".into(), plan: Some(Ok(ckpt::Plan { write: vec!["src/a.rs".into()], delete: vec!["new.txt".into(); 8] })), busy: false });
        all(&mut c, &mut k, "restore prompt");
        c.restore = None;
        let diff = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,2 @@\n fn a() {}\n-let x = 1;\n+let x = 2;\ndiff --git a/new.txt b/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+fresh\n";
        c.review = Some(Review { chat: c.chat.id.clone(), idx: 1, ckpt: "abc".into(), title: "everything this chat changed (since turn 1)".into(), data: Some(Ok(git::parse_diff(diff))), file: 1, scroll: 3, hits: vec![] });
        all(&mut c, &mut k, "diff view");
        c.review = None;
    }

    /// In a question where several choices can be ticked, "Other…" (no box of its own) still lines up with them.
    #[test]
    fn chat_question_other_lines_up() {
        let mut k = Kit::new();
        let mut c = agent_chat(&k, "claude", "set it up");
        let _forget = Forget(c.chat.id.clone());
        let (tx, _rx) = std::sync::mpsc::channel();
        let q = approve::Q { question: "Which ones?".into(), header: String::new(), multi: true, options: vec![("MIT".into(), "short".into()), ("Apache-2.0".into(), "patents".into())] };
        deliver(&mut k, &mut c, [Ev::Question(approve::Question { qs: vec![q], reply: tx })]);
        let s = k.render(&mut c, 100, 30);
        let col = |what: &str| s.lines().find(|l| l.contains(what)).and_then(|l| l.find(what).map(|b| l[..b].chars().count()));
        assert!(col("short").is_some() && col("short") == col("patents") && col("short") == col("type your own answer"), "{s}");
        c.stop();
    }

    #[test]
    fn chat_reopens_the_chat_you_left() {
        let k = Kit::new();
        let mut c = Chat::new(&k.config);
        assert_eq!(c.resume_id(), None, "a blank new chat has nothing to come back to");
        c.chats = (0..3)
            .map(|i| {
                let mut ch = store::Chat::new("claude");
                ch.id = format!("r{i}");
                ch.messages.push(store::Msg { role: "user".into(), content: format!("hi {i}"), ..Default::default() });
                ch
            })
            .collect();
        c.resume("r2");
        assert_eq!((c.chat.id.as_str(), c.resume_id().as_deref()), ("r2", Some("r2")));
        c.resume("gone"); // deleted since: stays put
        assert_eq!(c.chat.id, "r2");
        assert_eq!(c.reopen(), Some("ai"));
        // alt n from here opens the terminal in the chat's folder
        let d = std::path::absolute("target/test-scratch/shell").unwrap();
        std::fs::create_dir_all(&d).unwrap();
        c.chat.cwd = Some(d.display().to_string());
        assert_eq!(c.cwd(), Some(d));
    }

    /// Run a slash command, then do what the app does with the config changes it asked for.
    fn run_cmd(k: &mut Kit, c: &mut Chat, line: &str) -> usize {
        let mut acts = vec![];
        {
            let mut cx = Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: &mut acts, focused: true, time: 1.0 };
            c.run_command(line, &mut cx);
        }
        k.actions.extend(acts);
        k.apply_config(c)
    }

    /// /perms, /effort, /model, /provider, /key and shift+tab never write config.toml themselves: they ask the
    /// app (its one writer), so a later theme change can't undo them, and chats that are already open follow.
    #[test]
    fn chat_settings_go_through_the_app() {
        let mut k = Kit::new();
        let mut c = Chat::new(&k.config);
        c.avail = vec!["claude", "codex"];
        c.provider = "claude".into();
        c.chat.provider = Some("claude".into());
        for cmd in ["/perms default bypass", "/effort high", "/model opus", "/key anthropic sk-test", "/provider codex"] {
            assert_eq!(run_cmd(&mut k, &mut c, cmd), 1, "{cmd} asks the app to save it");
        }
        let a = &k.config.ai;
        assert_eq!((a.perms.as_str(), a.effort.as_str(), a.anthropic_key.as_str(), a.provider.as_str()), ("bypass", "high", "sk-test", "codex"));
        assert_eq!(a.models.get("claude").map(String::as_str), Some("opus"));
        // shift+tab and /perms <mode> change this chat only: a bypass for one chat isn't every chat's
        k.key_mod(&mut c, KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(k.apply_config(&mut c), 0);
        assert_eq!(c.perms, "ask");
        assert_eq!(run_cmd(&mut k, &mut c, "/perms plan"), 0);
        assert_eq!((c.perms.as_str(), k.config.ai.perms.as_str()), ("plan", "bypass"));
        c.new_chat();
        assert_eq!(c.perms, "bypass", "a new chat starts from the default");
        assert_eq!(run_cmd(&mut k, &mut c, "/perms default ask"), 1);
        assert_eq!(k.config.ai.perms, "ask");
        // a bare /perms default says where new chats start, and changes nothing
        run_cmd(&mut k, &mut c, "/perms plan");
        assert_eq!(run_cmd(&mut k, &mut c, "/perms Default"), 0);
        assert!(c.info.iter().any(|l| l.contains("new chats start in ask")), "{:?}", c.info);
        assert_eq!(c.perms, "plan", "not taken as ask");
        // a chat that was open all along follows (its empty chat switches AI too) ...
        let mut before = Chat::new(&crate::config::Config::default());
        before.avail = vec!["claude", "codex"];
        before.config_changed(&k.config);
        assert_eq!((before.perms.as_str(), before.effort.as_str(), before.provider.as_str()), ("ask", "high", "codex"));
        assert_eq!(before.chat.provider.as_deref(), Some("codex"));
        assert!(before.avail.contains(&"anthropic"), "the saved key makes the Anthropic API usable here too");
        assert_eq!(before.models.get("claude").map(String::as_str), Some("opus"));
        // ... but one with a conversation in it keeps its AI
        let mut busy = Chat::new(&crate::config::Config::default());
        busy.avail = vec!["claude", "codex"];
        busy.chat.provider = Some("claude".into());
        busy.chat.messages.push(store::Msg { role: "user".into(), content: "hi".into(), ..Default::default() });
        busy.config_changed(&k.config);
        assert_eq!(busy.chat.provider.as_deref(), Some("claude"));
        // ... and its own mode: an unrelated setting changing doesn't touch it, a new default does not either
        busy.perms = "bypass".into();
        k.config.theme = "ocean".into();
        busy.config_changed(&k.config);
        k.config.ai.perms = "plan".into();
        busy.config_changed(&k.config);
        assert_eq!((busy.perms.as_str(), busy.default_perms.as_str()), ("bypass", "plan"));
        // and a chat opened now starts from it
        assert_eq!(Chat::new(&k.config).perms, "plan");
    }

    /// /note saves where the notes app looks (notes_folder), and neither /note nor /save overwrites a file.
    #[test]
    fn chat_note_goes_to_the_notes_folder() {
        let dir = std::path::absolute("target/test-scratch/config/chat-notes").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let mut k = Kit::new();
        k.config.notes_folder = dir.to_string_lossy().to_string();
        let mut c = Chat::new(&k.config);
        c.chat.title = "fix the tests".into();
        c.chat.messages.push(store::Msg { role: "user".into(), content: "why do they fail".into(), ..Default::default() });
        c.chat.messages.push(store::Msg { role: "assistant".into(), content: "a stale fixture".into(), ..Default::default() });
        run_cmd(&mut k, &mut c, "/note");
        run_cmd(&mut k, &mut c, "/note");
        assert!(std::fs::read_to_string(dir.join("fix the tests.md")).unwrap().contains("a stale fixture"));
        assert!(dir.join("fix the tests 2.md").exists(), "the second one gets its own file");
        assert!(k.notices().iter().any(|n| n.contains("fix the tests 2.md")), "{:?}", k.notices());
        // /save names its export the same way
        assert_eq!(free_path(&dir, "fix the tests"), dir.join("fix the tests 3.md"));
        assert_eq!(free_path(&dir, "a/b: c?"), dir.join("a_b_ c_.md"));
    }

    #[test]
    fn chat_closing_mid_reply_stops_the_agent() {
        let k = Kit::new();
        let c = agent_chat(&k, "claude", "fix the flaky test");
        let _forget = Forget(c.chat.id.clone());
        let stop = c.stream.as_ref().unwrap().stop.clone();
        assert_eq!(c.busy(), 1, "closing or quitting asks first");
        drop(c); // the pane closing, or oriel quitting
        assert!(stop.load(Ordering::SeqCst), "the agent's process tree is told to stop, not left running unseen");
        // what it said after the last poll isn't lost with the pane
        let mut c = agent_chat(&k, "claude", "fix the flaky test");
        let _forget2 = Forget(c.chat.id.clone());
        c.stream.as_ref().unwrap().inbox.lock().unwrap().push(Ev::Token("half an answer".into()));
        assert!(c.wind_down());
        assert!(c.stream.is_none() && c.busy() == 0);
        assert!(serde_json::to_string(c.chat.messages.last().unwrap()).unwrap().contains("half an answer"));
        assert!(!c.wind_down(), "nothing left running");
    }

    /// /commit commits the chat's folder (the title as the message by default), /p puts a saved prompt in the box,
    /// and /cwd says it's this chat's agent that moves (not the agents app).
    #[test]
    fn chat_commit_prompts_and_cwd() {
        let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-scratch").join(format!("chat-commit-{}", std::process::id()));
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let o = std::process::Command::new("git").arg("-C").arg(&repo).args(args).output().unwrap();
            assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "oriel test"]);
        git(&["config", "user.email", "test@example.invalid"]);
        git(&["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        let mut k = Kit::new();
        let mut c = Chat::new(&k.config);
        c.chat.cwd = Some(repo.display().to_string());
        c.chat.title = "add the a file".into();
        k.typ(&mut c, "/commit");
        k.key(&mut c, KeyCode::Enter);
        assert_eq!(git(&["log", "-1", "--format=%s"]), "add the a file", "the chat's title");
        assert!(k.notices().iter().any(|n| n.contains("committed 1 file")), "{:?}", k.notices());
        std::fs::write(repo.join("b.txt"), "b\n").unwrap();
        k.typ(&mut c, "/commit b too");
        k.key(&mut c, KeyCode::Enter);
        assert_eq!(git(&["log", "-1", "--format=%s"]), "b too");
        k.typ(&mut c, "/commit");
        k.key(&mut c, KeyCode::Enter);
        assert!(c.info.iter().any(|l| l.contains("nothing to commit")), "{:?}", c.info);
        // /p <name>
        let file = base.join("prompts.toml");
        crate::panes::agents::prompts::save_to(&file, &[crate::panes::agents::prompts::Prompt { name: "careful".into(), text: "keep changes small".into() }]).unwrap();
        crate::panes::agents::prompts::TEST_PATH.with(|t| *t.borrow_mut() = Some(file.clone()));
        c.input = "/p ".into();
        c.cursor = 3;
        assert!(k.render(&mut c, 130, 40).contains("careful"), "the / menu lists them");
        c.input.clear();
        c.cursor = 0;
        k.typ(&mut c, "/p careful");
        k.key(&mut c, KeyCode::Enter);
        assert_eq!(c.input, "keep changes small", "in the box, not sent");
        assert!(c.chat.messages.is_empty());
        crate::panes::agents::prompts::TEST_PATH.with(|t| *t.borrow_mut() = None);
        // /cwd
        c.input.clear();
        c.cursor = 0;
        k.typ(&mut c, &format!("/cwd {}", base.display()));
        k.key(&mut c, KeyCode::Enter);
        assert!(k.notices().last().is_some_and(|n| n.starts_with("this chat's agent works in")), "{:?}", k.notices());
        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
pub fn demo_now() -> f64 {
    store::now()
}

