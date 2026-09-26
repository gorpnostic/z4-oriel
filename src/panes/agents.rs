//! agents: the orchestrator. A kanban board of coding-agent tasks on a git repo (TODO · RUNNING · REVIEW ·
//! DONE). Each task gets its own worktree and branch and runs Claude Code / Codex / Kimi in a real terminal tab;
//! hooks (`oriel report`) and the terminal's screen-scraped activity say what it's doing; a diff view reviews
//! the work, and `m` squash-merges it back. Design: docs/ai-research.md §3.
//!
//! Lead mode (`L`, lead.rs): one goal, a lead agent of the user's choosing (Claude Code, Codex, Kimi Code)
//! orchestrating headless workers from the roster on an integration branch, through oriel's MCP tools (mcp.rs)
//! or a JSON text protocol. `w` watches it all side by side, `t` takes a worker over in a terminal tab.
//!
//! Nothing slow runs on the UI thread: git, transcript parsing and status-file reads all happen on background
//! threads that send a `Msg` back and wake the pane.

mod batch;
mod cost;
mod git;
mod input;
mod lead;
#[cfg(test)]
mod lead_tests;
mod lead_view;
mod mcp;
mod plan;
mod roster;
mod run;
mod store;
mod stream;
#[cfg(test)]
mod tests;
mod view;

/// `oriel mcp-lead <port> <token>`: the MCP stdio server a lead's CLI starts (bridges to the running oriel).
/// Every AI's plan windows from disk, flattened: (agent, window). The event center warns near a limit.
pub fn usage_limits() -> Vec<(String, roster::Window)> {
    let now = crate::panes::files::clock::now_secs();
    let l = roster::read(&roster::LimitPaths::real(), now);
    l.claude.into_iter().map(|w| ("claude".to_string(), w)).chain(l.codex.into_iter().map(|w| ("codex".to_string(), w))).collect()
}

/// "2h 10m" until a unix time.
pub fn until(ts: i64) -> String {
    let s = (ts - crate::panes::files::clock::now_secs()).max(0);
    if s >= 86400 { format!("{}d {}h", s / 86400, s % 86400 / 3600) } else if s >= 3600 { format!("{}h {}m", s / 3600, s % 3600 / 60) } else { format!("{}m", s / 60) }
}

pub fn mcp_lead(port: &str, token: &str) {
    mcp::serve_stdio(port, token);
}

use crate::pane::{Action, Activity, Cx, Pane, Waker};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use input::Input;
use ratatui::{
    Frame,
    layout::{Position, Rect},
};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use store::{Paths, Status, StatusFile, Store, Task};

/// `oriel report --task <id> --state <running|blocked|idle|session>` — called by the agents' hooks with the
/// hook JSON on stdin. Quick, silent, always exits 0.
pub fn cli(args: &[String]) -> i32 {
    let stdin = store::read_stdin_quick();
    store::report(&Paths::default(), args, stdin.as_deref())
}

/// The coding agents the board can run: (id, icon).
pub const KINDS: &[(&str, &str)] = &[("claude", "claude"), ("codex", "robot"), ("kimi", "robot")];

/// Find an agent's binary: PATH, then the folders installers use.
fn find_agent(id: &str) -> Option<PathBuf> {
    if let Some(p) = crate::config::which(id) {
        return Some(p);
    }
    let home = dirs::home_dir()?;
    let exe = if cfg!(windows) { format!("{id}.exe") } else { id.to_string() };
    [home.join(format!(".{id}-code")).join("bin").join(&exe), home.join(".local").join("bin").join(&exe)].into_iter().find(|p| p.is_file())
}

/// Program + args for a terminal pane running `bin` with `args`. `.cmd` shims (npm installs on Windows) go
/// through cmd.exe, whose quoting can't carry newlines or double quotes, so those get flattened.
fn wrap_cmd(bin: &Path, args: Vec<String>, task_id: &str) -> (String, Vec<String>) {
    let p = bin.to_string_lossy().to_string();
    let low = p.to_lowercase();
    if cfg!(windows) && (low.ends_with(".cmd") || low.ends_with(".bat")) {
        let mut a = vec!["/c".to_string(), p];
        a.extend(args.into_iter().map(|s| s.replace(['\r', '\n'], " ").replace('"', "'")));
        ("cmd.exe".into(), a)
    } else if cfg!(unix) {
        // no env API on terminal panes: `env` sets ORIEL_TASK_ID for the agent (and its hooks)
        let mut a = vec![format!("ORIEL_TASK_ID={task_id}"), p];
        a.extend(args);
        ("env".into(), a)
    } else {
        (p, args)
    }
}

/// The agent's arguments. `followup` = continue the task's last session with `prompt` as the next message.
pub fn agent_args(agent: &str, model: &str, prompt: &str, followup: bool) -> Vec<String> {
    let mut a: Vec<String> = vec![];
    let model = model.trim();
    match agent {
        "codex" => {
            if followup {
                a.extend(["resume".into(), "--last".into()]);
            }
            if !model.is_empty() {
                a.extend(["-m".into(), model.into()]);
            }
            a.push(prompt.into());
        }
        "kimi" => {
            // kimi's interactive mode takes no first prompt, so tasks run it in prompt mode (visible in the tab)
            if !model.is_empty() {
                a.extend(["-m".into(), model.into()]);
            }
            if followup {
                a.push("-c".into());
            }
            a.extend(["-p".into(), prompt.into()]);
        }
        _ => {
            if !model.is_empty() {
                a.extend(["--model".into(), model.into()]);
            }
            if followup {
                a.push("--continue".into());
            }
            a.push(prompt.into());
        }
    }
    a
}

/// Results from background threads.
enum Msg {
    Repo(Result<git::RepoInfo, String>, bool),
    Agents(Vec<(&'static str, Option<PathBuf>)>),
    Status(String, StatusFile),
    Started(String, Result<git::Started, String>),
    Refreshed(String, Option<git::Stats>, Option<cost::Cost>),
    Gone(String, bool),
    Diff(String, Result<git::Diff, String>),
    Progress(String, String),
    Merged(String, Result<String, String>),
    Removed(String, Result<(), String>, Then),
    Plan(Result<(Vec<(String, String)>, f64), String>),
    // ---- lead mode
    /// A headless worker's live event, and its end.
    Worker(String, stream::Ev),
    WorkerDone(String, run::Outcome),
    /// Post-run check: per-file stats, conflicts with the integration branch, a finished conflict resolution.
    Checked(String, Option<Vec<(String, u64, u64)>>, Result<Vec<String>, String>, Option<Result<(), String>>),
    /// The lead's live event, the end of one of its turns, its end.
    Lead(String, stream::Ev),
    LeadTurn(String, run::Outcome),
    LeadDone(String, Result<String, String>),
    /// The lead's MCP tools didn't load: it continues on the text protocol.
    LeadProto(String),
    /// A lead tool call to answer.
    Call(mcp::Call),
    RunStarted(String, Result<git::RunStarted, String>),
    RunMerged(String, Result<String, git::MergeErr>),
    Resolve(String, Result<Vec<String>, String>, Option<lead::Reply>),
    Redispatched(String, Result<(), String>, crate::config::RosterEntry),
    RunFinal(String, Result<String, String>),
    RunCleaned(String, Result<(), String>),
    TookOver(String),
    Limits(roster::Limits),
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Then {
    Discarded,
    Retry,
}

/// What the app knows about a task right now (not saved).
#[derive(Default)]
struct Live {
    /// the task's terminal tab is open
    pane: bool,
    /// ...and has been seen open since this run started
    seen: bool,
    since: Option<Instant>,
    /// hook reports older than this (unix secs) belong to an earlier run
    run_ts: i64,
    hooked: bool,
    was_working: bool,
    activity: Option<Activity>,
    /// a git job is running for it
    busy: bool,
    refreshed: Option<Instant>,
    // ---- headless (lead-mode) workers
    /// Its live transcript.
    log: Vec<stream::Entry>,
    /// Set while its process runs; storing true stops it.
    stop: Option<Arc<AtomicBool>>,
    /// Cost/tokens before this process started (follow-ups add to them).
    cost_base: f64,
    tokens_base: u64,
    /// When it stops: open it in a terminal tab / discard it (answer the lead) / the watchdog's (reason, fresh).
    takeover: bool,
    discard: Option<lead::Reply>,
    nudge: Option<(String, bool)>,
    /// Answer the lead once a removal finishes.
    reply: Option<lead::Reply>,
    resolving: bool,
    checking: bool,
    // watchdog evidence
    last_event: Option<Instant>,
    repeat: (String, u32),
    fails: HashMap<String, u32>,
    calls_since_edit: u32,
    nudged: u32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Pending {
    Merge,
    Discard,
    Delete,
    Retry,
    StopRun,
    MergeRun,
    DiscardRun,
}

struct Confirm {
    id: String,
    what: Pending,
}

/// Task priority: (value, name). Waiting tasks start highest first.
pub(super) const PRIORITIES: &[(i8, &str)] = &[(-1, "low"), (0, "normal"), (1, "high"), (2, "urgent")];

struct Form {
    /// Some = editing this TODO task
    editing: Option<String>,
    field: usize, // 0 title, 1 prompt, 2 agent, 3 model, 4 priority, 5 after, 6 budget, 7 buttons
    title: Input,
    prompt: Input,
    agent: usize,
    model: Input,
    /// index into PRIORITIES
    priority: usize,
    /// tasks it waits for: ids or the start of their titles, comma separated
    after: Input,
    /// spend cap in USD ("" = the roster's)
    budget: Input,
    button: usize, // 0 add, 1 add & start
    err: String,
}

struct DiffView {
    id: String,
    data: Option<Result<git::Diff, String>>,
    file: usize,
    scroll: usize,
}

struct Picker {
    input: Input,
    /// 0 = the path field, 1.. = recent repos
    sel: usize,
    err: String,
    checking: bool,
}

/// `L`: a new lead run.
struct LeadForm {
    field: usize, // 0 goal, 1 lead agent, 2 model, 3 max parallel, 4 run budget, 5 start
    goal: Input,
    agent: usize,
    model: Input,
    parallel: Input,
    budget: Input,
    err: String,
}

/// `R`: the roster, editable.
struct RosterView {
    sel: usize,
    edit: Option<RosterEdit>,
}

struct RosterEdit {
    /// None = a new worker
    idx: Option<usize>,
    field: usize, // 0 name, 1 agent, 2 model, 3 tier, 4 good at, 5 max turns, 6 budget, 7 save
    name: Input,
    agent: usize,
    model: Input,
    tier: usize,
    good_at: Input,
    turns: Input,
    budget: Input,
    enabled: bool,
    err: String,
}

/// `w`: the lead and its workers side by side, live.
struct WatchView {
    run: String,
    sel: usize,
}

/// One agent's live transcript, full screen.
#[derive(Clone, PartialEq)]
enum LogTarget {
    Lead(String),
    Task(String),
}

struct LogView {
    target: LogTarget,
    /// Lines up from the bottom (0 = follow the newest).
    scroll: usize,
    /// Came from the watch view (esc goes back there).
    back: Option<String>,
}

enum Mode {
    Board,
    Form(Form),
    Diff(DiffView),
    Confirm(Confirm),
    Comment(String, Input),
    Plan(Input),
    Repo(Picker),
    LeadForm(LeadForm),
    Roster(RosterView),
    Watch(WatchView),
    Log(LogView),
    /// Run the marked tasks together: all at once or one after another.
    Batch,
}

#[derive(Clone)]
enum Hit {
    Card(usize, usize),
    Column(usize),
    File(usize),
    DiffBody,
    Repo(String),
    NewTask,
    LeadPanel,
    Tile(usize),
}

pub struct Agents {
    paths: Paths,
    store: Store,
    repo: Option<git::RepoInfo>,
    repo_loading: bool,
    agents: Vec<(&'static str, Option<PathBuf>)>,
    live: HashMap<String, Live>,
    /// latest list of the app's tagged panes (None until the app first sends one)
    tagged: Option<Vec<(String, Option<Activity>)>>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    waker: Arc<Mutex<Option<Waker>>>,
    stop: Arc<AtomicBool>,
    /// true while some task is running: the watcher then wakes the pane every second or so
    active: Arc<AtomicBool>,
    saver: Sender<Vec<u8>>,
    /// held while tasks.json is written; true once the pane is gone (its final write wins)
    save_lock: Arc<Mutex<bool>>,
    booted: bool,
    mode: Mode,
    col: usize,
    row: [usize; 4],
    scroll: [usize; 4],
    hits: Vec<(Rect, Hit)>,
    side_hits: Vec<(Rect, Hit)>,
    planning: bool,
    /// The oriel binary the hooks call.
    hook_exe: Option<PathBuf>,
    /// Tests: run this instead of the real agent.
    fake_agent: Option<(String, Vec<String>)>,
    /// Tests: detect the repo here instead of the current directory.
    start_dir: Option<PathBuf>,
    // ---- lead mode
    lead_cfg: crate::config::LeadConfig,
    /// The config's roster (empty = defaults from what's installed, see `roster()`).
    roster: Vec<crate::config::RosterEntry>,
    runs_live: HashMap<String, lead::RunLive>,
    waiters: Vec<lead::Waiter>,
    /// Held by every git job that touches a worktree's real index or the integration branch.
    merge_lock: Arc<Mutex<()>>,
    merge_queue: VecDeque<String>,
    merging: Option<String>,
    limits: roster::Limits,
    limit_paths: Option<roster::LimitPaths>,
    limits_read: Option<Instant>,
    /// Vendors paused by a rejected rate limit, until (unix secs).
    paused: HashMap<String, i64>,
    /// When a worker on (agent, model) last started (starts are staggered so caches warm up).
    last_start: HashMap<(String, String), Instant>,
    stagger: Duration,
    hung_after: Duration,
    /// The lead panel at the top of the board has the keyboard.
    lead_focus: bool,
    /// Tasks marked with space, in the order they were marked (enter runs them together).
    marked: Vec<String>,
    /// Tests: run these instead of real workers / leads.
    fake_worker: Option<run::Fake>,
    fake_lead: Option<run::Fake>,
    /// The oriel binary lead CLIs start as their MCP server (tests point it at the built binary).
    mcp_exe: Option<PathBuf>,
}

impl Agents {
    pub fn new(cfg: &crate::config::Config) -> Self {
        let mut a = Self::with_paths(Paths::default());
        a.lead_cfg = cfg.lead.clone();
        a.roster = cfg.roster.clone();
        a.stagger = Duration::from_secs(cfg.lead.stagger_s as u64);
        a.limit_paths = Some(roster::LimitPaths::real());
        a
    }

    fn with_paths(paths: Paths) -> Self {
        let (tx, rx) = channel();
        let store = store::load(&paths);
        // one writer thread for tasks.json: always writes the newest snapshot, never blocks the UI
        let (saver, srx) = channel::<Vec<u8>>();
        let tasks_path = paths.tasks();
        let save_lock = Arc::new(Mutex::new(false));
        let lock = save_lock.clone();
        std::thread::spawn(move || {
            while let Ok(mut data) = srx.recv() {
                while let Ok(newer) = srx.try_recv() {
                    data = newer;
                }
                let closed = lock.lock().unwrap();
                if !*closed {
                    let _ = store::write_atomic(&tasks_path, &data);
                }
            }
        });
        let mut live = HashMap::new();
        for t in &store.tasks {
            live.insert(t.id.clone(), Live { since: Some(Instant::now()), ..Default::default() });
        }
        let mut a = Agents {
            paths,
            store,
            repo: None,
            repo_loading: true,
            agents: KINDS.iter().map(|k| (k.0, None)).collect(),
            live,
            tagged: None,
            tx,
            rx,
            waker: Arc::new(Mutex::new(None)),
            stop: Arc::new(AtomicBool::new(false)),
            active: Arc::new(AtomicBool::new(false)),
            saver,
            save_lock,
            booted: false,
            mode: Mode::Board,
            col: 0,
            row: [0; 4],
            scroll: [0; 4],
            hits: vec![],
            side_hits: vec![],
            planning: false,
            hook_exe: std::env::current_exe().ok(),
            fake_agent: None,
            start_dir: None,
            lead_cfg: crate::config::LeadConfig::default(),
            roster: vec![],
            runs_live: HashMap::new(),
            waiters: vec![],
            merge_lock: Arc::new(Mutex::new(())),
            merge_queue: VecDeque::new(),
            merging: None,
            limits: roster::Limits::default(),
            limit_paths: None,
            limits_read: None,
            paused: HashMap::new(),
            last_start: HashMap::new(),
            stagger: Duration::from_secs(5),
            hung_after: Duration::from_secs(300),
            lead_focus: false,
            marked: vec![],
            fake_worker: None,
            fake_lead: None,
            mcp_exe: None,
        };
        a.settle_after_restart();
        a
    }

    // ------------------------------------------------------------------ plumbing

    fn save(&self) {
        if let Ok(j) = serde_json::to_vec_pretty(&self.store) {
            let _ = self.saver.send(j);
        }
    }

    /// Run `f` on a background thread; its Msg comes back through poll.
    fn spawn(&self, cx: &Cx, f: impl FnOnce(&dyn Fn(Msg)) + Send + 'static) {
        let tx = self.tx.clone();
        let w = cx.waker();
        std::thread::spawn(move || {
            let send = |m: Msg| {
                let _ = tx.send(m);
                w.wake();
            };
            f(&send);
        });
    }

    /// First render/poll: detect the repo and the agents, start watching status files.
    fn boot(&mut self, cx: &Cx) {
        *self.waker.lock().unwrap() = Some(cx.waker());
        if self.booted {
            return;
        }
        self.booted = true;
        let dir = self.start_dir.clone().or_else(|| std::env::current_dir().ok()).unwrap_or_default();
        self.spawn(cx, move |send| {
            send(Msg::Agents(KINDS.iter().map(|k| (k.0, find_agent(k.0))).collect()));
            send(Msg::Repo(git::repo_info(&dir), false));
        });
        self.start_watcher();
    }

    /// A thread that notices status files changing (hooks write them) and sends them over. Cheap: one
    /// directory listing a second.
    fn start_watcher(&self) {
        let (dir, tx, waker, stop, active) = (self.paths.status_dir(), self.tx.clone(), self.waker.clone(), self.stop.clone(), self.active.clone());
        std::thread::spawn(move || {
            let mut seen: HashMap<PathBuf, SystemTime> = HashMap::new();
            let mut last_tick = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                let mut changed = false;
                if let Ok(rd) = std::fs::read_dir(&dir) {
                    for e in rd.flatten() {
                        let p = e.path();
                        if p.extension().map(|x| x != "json").unwrap_or(true) {
                            continue;
                        }
                        let Some(m) = e.metadata().ok().and_then(|m| m.modified().ok()) else { continue };
                        if seen.get(&p) == Some(&m) {
                            continue;
                        }
                        seen.insert(p.clone(), m);
                        if let (Some(id), Some(st)) = (p.file_stem().map(|s| s.to_string_lossy().to_string()), store::read_status(&p)) {
                            let _ = tx.send(Msg::Status(id, st));
                            changed = true;
                        }
                    }
                }
                let tick = active.load(Ordering::Relaxed) && last_tick.elapsed() >= Duration::from_millis(1000);
                if changed || tick {
                    last_tick = Instant::now();
                    if let Some(w) = waker.lock().unwrap().as_ref() {
                        w.wake();
                    }
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        });
    }

    fn task(&self, id: &str) -> Option<&Task> {
        self.store.tasks.iter().find(|t| t.id == id)
    }

    fn task_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.store.tasks.iter_mut().find(|t| t.id == id)
    }

    fn live(&mut self, id: &str) -> &mut Live {
        self.live.entry(id.to_string()).or_default()
    }

    fn repo_key(&self) -> String {
        self.repo.as_ref().map(|r| r.root.display().to_string()).unwrap_or_default()
    }

    /// Task indices in board column `c` for the current repo, in display order.
    fn column(&self, c: usize) -> Vec<usize> {
        let key = self.repo_key();
        let mut v: Vec<usize> = self.store.tasks.iter().enumerate().filter(|(_, t)| t.repo == key && t.status.column() == c).map(|(i, _)| i).collect();
        let ts = &self.store.tasks;
        match c {
            0 => v.sort_by_key(|&i| ts[i].created),
            1 => v.sort_by_key(|&i| (ts[i].status != Status::Blocked, ts[i].started)),
            _ => v.sort_by_key(|&i| std::cmp::Reverse(ts[i].finished.max(ts[i].started))),
        }
        v
    }

    fn selected(&self) -> Option<String> {
        let col = self.column(self.col);
        col.get(self.row[self.col].min(col.len().saturating_sub(1))).map(|&i| self.store.tasks[i].id.clone())
    }

    /// Put the selection on task `id` (after it moves column).
    fn select(&mut self, id: &str) {
        for c in 0..4 {
            if let Some(r) = self.column(c).iter().position(|&i| self.store.tasks[i].id == id) {
                self.col = c;
                self.row[c] = r;
            }
        }
    }

    fn installed(&self, agent: &str) -> Option<PathBuf> {
        self.agents.iter().find(|a| a.0 == agent).and_then(|a| a.1.clone())
    }

    fn set_repo(&mut self, r: git::RepoInfo) {
        let key = r.root.display().to_string();
        self.store.repos.retain(|x| *x != key);
        self.store.repos.insert(0, key);
        self.store.repos.truncate(12);
        self.repo = Some(r);
        self.row = [0; 4];
        self.scroll = [0; 4];
        self.save();
    }

    fn open_repo(&mut self, path: PathBuf, cx: &Cx) {
        if let Mode::Repo(p) = &mut self.mode {
            p.checking = true;
            p.err.clear();
        }
        self.spawn(cx, move |send| send(Msg::Repo(git::repo_info(&path), true)));
    }

    fn today(&self) -> f64 {
        let now = crate::panes::files::clock::local(store::now());
        let same = |t: i64| {
            let d = crate::panes::files::clock::local(t);
            (d.year, d.month, d.day) == (now.year, now.month, now.day)
        };
        // workers, plus the leads that orchestrated them
        self.store.tasks.iter().filter(|t| same(t.started.max(t.created))).fold(0.0, |a, t| a + t.cost_usd) + self.store.runs.iter().filter(|r| same(r.created)).fold(0.0, |a, r| a + r.cost_usd)
    }

    // ------------------------------------------------------------------ the state machine

    /// Apply everything background threads sent, follow the agents' tabs, notice tabs that went away.
    fn sync(&mut self, cx: &mut Cx) {
        self.boot(cx);
        let mut dirty = false;
        while let Ok(m) = self.rx.try_recv() {
            self.on_msg(m, cx);
            dirty = true;
        }
        if self.follow_tabs(cx) {
            dirty = true;
        }
        // lead mode: start what can start, merge what's queued, watch the workers, answer waits
        let runs = self.store.runs.iter().any(|r| r.state.active()) || !self.merge_queue.is_empty() || self.merging.is_some() || self.live.values().any(|l| l.stop.is_some());
        if runs || !self.waiters.is_empty() {
            let before = (self.store.tasks.iter().filter(|t| t.status == Status::Running).count(), self.merge_queue.len(), self.merging.clone());
            self.schedule(cx);
            self.pump_merges(cx);
            self.watchdog();
            self.check_waiters();
            self.watch_budgets(cx);
            if before != (self.store.tasks.iter().filter(|t| t.status == Status::Running).count(), self.merge_queue.len(), self.merging.clone()) {
                dirty = true;
            }
        }
        self.refresh_limits(cx, runs);
        if dirty {
            self.save();
        }
        let any = self.store.tasks.iter().any(|t| t.status.active()) || runs;
        self.active.store(any, Ordering::Relaxed);
    }

    /// Plan limits for the roster: read at boot and every minute while a run is going (background).
    fn refresh_limits(&mut self, cx: &Cx, running: bool) {
        let Some(p) = self.limit_paths.clone() else { return };
        let due = match self.limits_read {
            None => true,
            Some(t) => running && t.elapsed() > Duration::from_secs(60),
        };
        if due {
            self.limits_read = Some(Instant::now());
            self.spawn(cx, move |send| send(Msg::Limits(roster::read(&p, store::now()))));
        }
    }

    fn on_msg(&mut self, m: Msg, cx: &mut Cx) {
        match m {
            Msg::Agents(a) => self.agents = a,
            Msg::Repo(r, picked) => {
                self.repo_loading = false;
                match r {
                    Ok(info) => {
                        self.set_repo(info);
                        if matches!(self.mode, Mode::Repo(_)) {
                            self.mode = Mode::Board;
                        }
                    }
                    Err(e) => {
                        if picked {
                            if let Mode::Repo(p) = &mut self.mode {
                                p.checking = false;
                                p.err = e;
                            }
                        } else if self.repo.is_none() {
                            // not started in a repo: pick one (recent ones listed)
                            self.mode = Mode::Repo(Picker { input: Input::new("", false), sel: if self.store.repos.is_empty() { 0 } else { 1 }, err: String::new(), checking: false });
                        }
                    }
                }
            }
            Msg::Status(id, st) => self.on_status(&id, st, cx),
            Msg::Started(id, r) => self.on_started(&id, r, cx),
            Msg::Refreshed(id, stats, c) => {
                self.live(&id).busy = false;
                if let Some(t) = self.task_mut(&id) {
                    if let Some(s) = stats {
                        (t.added, t.removed, t.files) = (s.added, s.removed, s.files);
                    }
                    if let Some(c) = c {
                        if c.usd > 0.0 || c.tokens > 0 {
                            (t.cost_usd, t.tokens) = (c.usd, c.tokens);
                        }
                    }
                }
            }
            Msg::Gone(id, changes) => {
                self.live(&id).busy = false;
                let Some(t) = self.task(&id) else { return };
                if !t.status.active() {
                    return;
                }
                if changes {
                    self.to_review(&id, "agent tab closed · ready for review", cx);
                } else if let Some(t) = self.task_mut(&id) {
                    t.status = Status::Todo;
                    t.last = "agent tab closed with no changes".into();
                    t.question.clear();
                    t.branch.clear();
                    t.worktree.clear();
                    t.base_sha.clear();
                    self.select(&id);
                }
            }
            Msg::Diff(id, r) => {
                if let Ok(d) = &r {
                    let (a, rm) = d.files.iter().fold((0, 0), |acc, f| (acc.0 + f.added, acc.1 + f.removed));
                    let n = d.files.len() as u64;
                    if let Some(t) = self.task_mut(&id) {
                        (t.added, t.removed, t.files) = (a, rm, n);
                    }
                }
                if let Mode::Diff(v) = &mut self.mode {
                    if v.id == id {
                        v.data = Some(r);
                    }
                }
            }
            Msg::Progress(id, s) => {
                if let Some(t) = self.task_mut(&id) {
                    t.last = s;
                }
            }
            Msg::Merged(id, r) => {
                self.live(&id).busy = false;
                let sp = self.paths.status(&id);
                let Some(t) = self.task_mut(&id) else { return };
                match r {
                    Ok(summary) => {
                        t.status = Status::Done;
                        t.outcome = "merged".into();
                        t.finished = store::now();
                        t.last = summary.clone();
                        t.error.clear();
                        let title = t.title.clone();
                        let _ = std::fs::remove_file(&sp);
                        cx.notify(format!("✓ {title} · {summary}"));
                        self.select(&id);
                    }
                    Err(e) => {
                        t.last.clear();
                        t.error = e.clone();
                        cx.notify(format!("merge failed: {e}"));
                    }
                }
            }
            Msg::Removed(id, r, then) => {
                self.live(&id).busy = false;
                if let Some(reply) = self.live(&id).reply.take() {
                    let _ = reply.send(r.clone().map(|_| format!("{id} discarded")));
                    if r.is_ok() {
                        if let Some(run) = self.task(&id).map(|t| t.run.clone()) {
                            let title = self.task(&id).map(|t| t.title.clone()).unwrap_or_default();
                            if let Some(x) = self.run_mut(&run) {
                                x.log(&format!("discarded {title}"));
                            }
                        }
                    }
                }
                let sp = self.paths.status(&id);
                let Some(t) = self.task_mut(&id) else { return };
                match r {
                    Err(e) => {
                        t.error = e.clone();
                        t.last.clear();
                        cx.notify(e);
                    }
                    Ok(()) => {
                        t.worktree.clear();
                        t.branch.clear();
                        t.error.clear();
                        let _ = std::fs::remove_file(&sp);
                        if then == Then::Retry {
                            let t = self.task_mut(&id).unwrap();
                            t.status = Status::Todo;
                            (t.added, t.removed, t.files, t.cost_usd, t.tokens) = (0, 0, 0, 0.0, 0);
                            self.start(&id, cx);
                        } else {
                            t.status = Status::Done;
                            t.outcome = "discarded".into();
                            t.finished = store::now();
                            t.last = "discarded".into();
                            self.select(&id);
                        }
                    }
                }
            }
            Msg::Plan(r) => {
                self.planning = false;
                match r {
                    Ok((items, usd)) if !items.is_empty() => {
                        let n = items.len();
                        let agent = self.default_agent();
                        for (title, prompt) in items {
                            self.add_task(&title, &prompt, agent, "");
                        }
                        self.col = 0;
                        cx.notify(format!("planned {n} tasks{}", if usd > 0.0 { format!(" · ${usd:.2}") } else { String::new() }));
                    }
                    Ok(_) => cx.notify("the planner came back with no tasks"),
                    Err(e) => cx.notify(format!("plan failed: {e}")),
                }
            }
            Msg::Worker(id, e) => self.on_worker_ev(&id, e),
            Msg::WorkerDone(id, o) => self.on_worker_done(&id, o, cx),
            Msg::Checked(id, stats, conflicts, resolved) => self.on_checked(&id, stats, conflicts, resolved, cx),
            Msg::Lead(id, e) => self.on_lead_ev(&id, e),
            Msg::LeadTurn(id, o) => self.on_lead_turn(&id, o),
            Msg::LeadDone(id, r) => self.on_lead_done(&id, r, cx),
            Msg::LeadProto(id) => {
                if let Some(r) = self.run_mut(&id) {
                    r.protocol = "text".into();
                    r.log("MCP tools didn't load — switched to the text protocol");
                }
            }
            Msg::Call(c) => self.on_call(c, cx),
            Msg::RunStarted(id, r) => self.on_run_started(&id, r, cx),
            Msg::RunMerged(id, r) => self.on_run_merged(&id, r, cx),
            Msg::Resolve(id, r, reply) => self.on_resolve(&id, r, reply, cx),
            Msg::Redispatched(id, r, w) => self.on_redispatched(&id, r, w, cx),
            Msg::RunFinal(id, r) => self.on_run_final(&id, r, cx),
            Msg::RunCleaned(id, r) => self.on_run_cleaned(&id, r, cx),
            Msg::TookOver(id) => self.on_took_over(&id, cx),
            Msg::Limits(l) => {
                // keep numbers the workers' own streams reported more recently
                if !l.claude.is_empty() || self.limits.claude.is_empty() {
                    self.limits.claude = l.claude;
                }
                self.limits.codex = l.codex;
            }
        }
    }

    fn on_status(&mut self, id: &str, st: StatusFile, cx: &mut Cx) {
        let run_ts = self.live(id).run_ts;
        let Some(t) = self.task(id) else { return };
        if t.headless() || st.ts + 2 < run_ts || !matches!(t.status, Status::Running | Status::Blocked | Status::Review) {
            return;
        }
        let refresh = {
            let l = self.live(id);
            l.hooked = true;
            l.refreshed.map(|r| r.elapsed() > Duration::from_secs(8)).unwrap_or(true)
        };
        let t = self.task_mut(id).unwrap();
        if !st.session_id.is_empty() {
            t.session_id = st.session_id.clone();
        }
        if !st.transcript_path.is_empty() {
            t.transcript_path = st.transcript_path.clone();
        }
        if !st.last_tool.is_empty() && t.status != Status::Review {
            t.last = st.last_tool.clone();
        }
        match st.state.as_str() {
            "running" => {
                if matches!(t.status, Status::Blocked | Status::Review) {
                    t.status = Status::Running;
                    t.question.clear();
                    t.error.clear();
                    let id = id.to_string();
                    self.select_if_following(&id);
                }
                if refresh {
                    self.refresh(id, cx);
                }
            }
            "blocked" if t.status == Status::Running => {
                t.status = Status::Blocked;
                t.question = if st.message.is_empty() { "waiting for you in its tab".into() } else { st.message.clone() };
                let msg = format!("⚠ {} needs you: {}", t.title, t.question);
                cx.notify(msg);
            }
            "idle" if t.status.active() => self.to_review(id, "ready for review", cx),
            _ => {}
        }
    }

    /// Keep the selection on a card the user is looking at when it moves column.
    fn select_if_following(&mut self, id: &str) {
        if self.selected().as_deref() == Some(id) {
            self.select(id);
        }
    }

    fn to_review(&mut self, id: &str, why: &str, cx: &mut Cx) {
        let following = self.selected().as_deref() == Some(id);
        let Some(t) = self.task_mut(id) else { return };
        t.status = Status::Review;
        t.finished = store::now();
        t.question.clear();
        t.last = why.to_string();
        let title = t.title.clone();
        cx.alert(crate::alerts::Kind::AgentDone, format!("{title} is ready for review"));
        if following {
            self.select(id);
        }
        self.live(id).refreshed = None;
        self.refresh(id, cx);
    }

    /// Recount +/- and cost on a background thread.
    fn refresh(&mut self, id: &str, cx: &Cx) {
        let Some(t) = self.task(id).cloned() else { return };
        if t.worktree.is_empty() {
            return;
        }
        self.live(id).refreshed = Some(Instant::now());
        let id = id.to_string();
        // tasks that ran headless before a take-over already know their cost from their stream
        let from_stream = !t.run.is_empty();
        self.spawn(cx, move |send| {
            let wt = PathBuf::from(&t.worktree);
            let stats = if wt.is_dir() && !t.base_sha.is_empty() { git::stats(&wt, &t.base_sha).ok() } else { None };
            let c = match t.agent.as_str() {
                _ if from_stream => None,
                "claude" => Some(cost::claude_for_dir(&wt, &t.transcript_path, t.started)),
                "codex" => Some(cost::codex_for_dir(&wt, t.started)),
                _ => None,
            };
            send(Msg::Refreshed(id, stats, c));
        });
    }

    /// Follow the tasks' terminal tabs: activity as a fallback for hooks, and tabs the user closed.
    fn follow_tabs(&mut self, cx: &mut Cx) -> bool {
        let Some(tagged) = self.tagged.clone() else { return false };
        let mut changed = false;
        let ids: Vec<String> = self.store.tasks.iter().filter(|t| !t.headless() && matches!(t.status, Status::Running | Status::Blocked | Status::Review)).map(|t| t.id.clone()).collect();
        for id in ids {
            let tag = self.task(&id).unwrap().tag();
            let found = tagged.iter().find(|(t, _)| *t == tag).map(|x| x.1);
            let status = self.task(&id).unwrap().status;
            let l = self.live(&id);
            match found {
                Some(act) => {
                    l.pane = true;
                    l.seen = true;
                    // only changes in what the tab shows count: a stale "working" mustn't undo a hook's "blocked"
                    let fresh = l.activity != act;
                    l.activity = act;
                    let hooked = l.hooked;
                    if act == Some(Activity::Working) {
                        l.was_working = true;
                    }
                    let was_working = l.was_working;
                    if !fresh {
                        continue;
                    }
                    match act {
                        Some(Activity::Working) if status == Status::Blocked || (status == Status::Review && !hooked) => {
                            let t = self.task_mut(&id).unwrap();
                            t.status = Status::Running;
                            t.question.clear();
                            changed = true;
                        }
                        Some(Activity::Blocked) if status == Status::Running => {
                            let t = self.task_mut(&id).unwrap();
                            t.status = Status::Blocked;
                            if t.question.is_empty() {
                                t.question = "waiting for you in its tab".into();
                            }
                            let msg = format!("⚠ {} needs you", t.title);
                            cx.notify(msg);
                            changed = true;
                        }
                        Some(Activity::Idle) if !hooked && was_working && status.active() => {
                            self.to_review(&id, "ready for review", cx);
                            changed = true;
                        }
                        _ => {}
                    }
                }
                None => {
                    let was = l.pane;
                    l.pane = false;
                    let grace = l.since.map(|s| s.elapsed() > Duration::from_secs(5)).unwrap_or(true);
                    if status.active() && !l.busy && (l.seen || grace) {
                        l.busy = true;
                        l.seen = false;
                        let t = self.task(&id).unwrap().clone();
                        let repo = PathBuf::from(&t.repo);
                        self.spawn(cx, move |send| {
                            let wt = PathBuf::from(&t.worktree);
                            let changes = wt.is_dir() && git::has_changes(&wt, &t.base_sha);
                            if !changes && wt.is_dir() {
                                let _ = git::remove(&repo, &wt, &t.branch);
                            }
                            send(Msg::Gone(t.id, changes));
                        });
                        changed = true;
                    } else if was {
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    // ------------------------------------------------------------------ actions

    fn default_agent(&self) -> usize {
        KINDS.iter().position(|k| self.installed(k.0).is_some()).unwrap_or(0)
    }

    fn add_task(&mut self, title: &str, prompt: &str, agent: usize, model: &str) -> String {
        let id = store::new_id(&self.store.tasks);
        let title = if title.trim().is_empty() { prompt.lines().next().unwrap_or("task").chars().take(60).collect() } else { title.trim().to_string() };
        self.store.tasks.push(Task {
            id: id.clone(),
            repo: self.repo_key(),
            title,
            prompt: prompt.trim().to_string(),
            agent: KINDS[agent.min(KINDS.len() - 1)].0.to_string(),
            model: model.trim().to_string(),
            created: store::now(),
            ..Default::default()
        });
        self.live.insert(id.clone(), Live::default());
        self.save();
        id
    }

    /// TODO -> RUNNING: make the worktree (background), then open the agent's tab (on_started).
    fn start(&mut self, id: &str, cx: &mut Cx) {
        let Some(t) = self.task(id).cloned() else { return };
        let faked = if t.headless() { self.fake_worker.is_some() } else { self.fake_agent.is_some() };
        if !faked && self.installed(&t.agent).is_none() {
            let msg = format!("{} isn't installed — edit the task (e) to pick another agent", t.agent);
            self.task_mut(id).unwrap().error = msg.clone();
            cx.notify(msg);
            return;
        }
        if self.live(id).busy {
            return;
        }
        // lead-run tasks branch off the run's integration branch, in the run's repo
        let base = if t.run.is_empty() { None } else { self.run_ref(&t.run).map(|r| r.branch.clone()) };
        let (root, name) = match (&base, &self.repo) {
            (Some(_), _) => {
                let root = PathBuf::from(&t.repo);
                let name = root.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "repo".into());
                (root, name)
            }
            (None, Some(r)) => (r.root.clone(), r.name.clone()),
            (None, None) => return,
        };
        let slug = store::slug(&t.title, &t.id);
        let wt = self.paths.wt.join(&name).join(&slug);
        let hook = if t.agent == "claude" && !t.headless() { self.hook_exe.clone() } else { None };
        let _ = std::fs::remove_file(self.paths.status(id));
        {
            let t = self.task_mut(id).unwrap();
            t.status = Status::Running;
            t.started = store::now();
            t.finished = 0;
            t.outcome.clear();
            t.error.clear();
            t.question.clear();
            t.last = "creating worktree…".into();
        }
        let log = std::mem::take(&mut self.live(id).log);
        *self.live(id) = Live { busy: true, since: Some(Instant::now()), run_ts: store::now(), log, ..Default::default() };
        if !t.headless() {
            self.select(id);
        }
        self.save();
        let id2 = id.to_string();
        self.spawn(cx, move |send| send(Msg::Started(id2.clone(), git::start_at(&root, &wt, &slug, &id2, hook.as_deref(), base.as_deref()))));
    }

    fn on_started(&mut self, id: &str, r: Result<git::Started, String>, cx: &mut Cx) {
        self.live(id).busy = false;
        match r {
            Err(e) => {
                let headless = self.task(id).is_some_and(|t| t.headless());
                if let Some(t) = self.task_mut(id) {
                    t.status = if headless { Status::Review } else { Status::Todo };
                    t.last.clear();
                    t.error = e.clone();
                }
                cx.notify(format!("couldn't start: {e}"));
                if headless {
                    self.event_failed(id);
                } else {
                    self.select(id);
                }
            }
            Ok(s) => {
                let Some(t) = self.task_mut(id) else { return };
                t.branch = s.branch;
                t.base_branch = s.base_branch;
                t.base_sha = s.base_sha;
                t.worktree = s.worktree.display().to_string();
                t.last = format!("starting {}…", if t.worker.is_empty() { &t.agent } else { &t.worker });
                let t = t.clone();
                if t.headless() {
                    self.start_worker(id, cx);
                } else {
                    self.open_agent(&t, &t.prompt, false, cx);
                }
            }
        }
    }

    fn event_failed(&mut self, id: &str) {
        self.event(id, "failed");
    }

    /// Open (or re-open) the task's agent tab in its worktree.
    fn open_agent(&mut self, t: &Task, prompt: &str, followup: bool, cx: &mut Cx) {
        self.open_agent_with(t, agent_args(&t.agent, &t.model, prompt, followup), cx);
    }

    /// Open the task's agent tab with exactly these arguments.
    fn open_agent_with(&mut self, t: &Task, agent_args: Vec<String>, cx: &mut Cx) {
        let (prog, args) = match &self.fake_agent {
            Some(f) => f.clone(),
            None => {
                let Some(bin) = self.installed(&t.agent) else {
                    cx.notify(format!("{} isn't installed", t.agent));
                    return;
                };
                wrap_cmd(&bin, agent_args, &t.id)
            }
        };
        let icon = KINDS.iter().find(|k| k.0 == t.agent).map(|k| k.1).unwrap_or("robot");
        let name: String = ui_short(&t.title, 22);
        let pane = crate::panes::term::Term::new(&format!("{} · {}", t.agent, t.title), icon, &prog, args, Some(PathBuf::from(&t.worktree)));
        cx.act(Action::CloseTag(t.tag()));
        cx.act(Action::OpenTagged { pane: Box::new(pane), tag: t.tag(), name, focus: false });
        let l = self.live(&t.id);
        *l = Live { since: Some(Instant::now()), run_ts: store::now(), ..Default::default() };
    }

    fn open_diff(&mut self, id: &str, cx: &Cx) {
        let Some(t) = self.task(id).cloned() else { return };
        if t.worktree.is_empty() || !Path::new(&t.worktree).is_dir() {
            return;
        }
        self.mode = Mode::Diff(DiffView { id: id.to_string(), data: None, file: 0, scroll: 0 });
        let id = id.to_string();
        // a lead-run task merges into its integration branch: check conflicts against that
        let target = if t.run.is_empty() { None } else { self.run_ref(&t.run).map(|r| r.branch.clone()) };
        self.spawn(cx, move |send| send(Msg::Diff(id, git::diff_against(Path::new(&t.repo), Path::new(&t.worktree), &t.base_sha, target.as_deref()))));
    }

    fn merge(&mut self, id: &str, cx: &mut Cx) {
        let Some(t) = self.task(id).cloned() else { return };
        if !t.run.is_empty() {
            // a lead-run task lands on the run's integration branch, through the gated merge queue
            if !t.headless() {
                cx.act(Action::CloseTag(t.tag()));
                if let Some(tm) = self.task_mut(id) {
                    tm.mode = "headless".into();
                    tm.status = Status::Review;
                }
            }
            match self.queue_merge(id) {
                Ok(_) => {
                    let title = t.title.clone();
                    if let Some(r) = self.run_mut(&t.run) {
                        r.log(&format!("you queued {title} for merging"));
                    }
                }
                Err(e) => cx.notify(e),
            }
            return;
        }
        cx.act(Action::CloseTag(t.tag()));
        {
            let l = self.live(id);
            l.busy = true;
            l.pane = false;
        }
        let tm = self.task_mut(id).unwrap();
        tm.last = "merging…".into();
        tm.error.clear();
        let id = id.to_string();
        self.spawn(cx, move |send| {
            let progress = |s: &str| send(Msg::Progress(id.clone(), s.to_string()));
            let r = git::merge(Path::new(&t.repo), Path::new(&t.worktree), &t.branch, &t.base_branch, &t.title, &progress);
            send(Msg::Merged(id.clone(), r));
        });
    }

    fn remove(&mut self, id: &str, then: Then, cx: &mut Cx) {
        let Some(t) = self.task(id).cloned() else { return };
        cx.act(Action::CloseTag(t.tag()));
        self.live(id).busy = true;
        self.task_mut(id).unwrap().last = if then == Then::Retry { "resetting…".into() } else { "discarding…".into() };
        let id = id.to_string();
        self.spawn(cx, move |send| {
            let r = if t.worktree.is_empty() && t.branch.is_empty() { Ok(()) } else { git::remove(Path::new(&t.repo), Path::new(&t.worktree), &t.branch) };
            send(Msg::Removed(id, r, then));
        });
    }

    /// Feedback for a task: continue its agent's session in the worktree with the comment as the next prompt.
    fn comment(&mut self, id: &str, text: &str, cx: &mut Cx) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let Some(t) = self.task(id).cloned() else { return };
        if t.worktree.is_empty() || !Path::new(&t.worktree).is_dir() {
            cx.notify("that task has no worktree any more");
            return;
        }
        if t.headless() {
            if self.live.get(id).is_some_and(|l| l.stop.is_some()) {
                cx.notify("it's still working — comment when it's done, or t to take it over");
                return;
            }
            self.worker_again(id, text, cx);
            self.save();
            return;
        }
        self.open_agent(&t, text, true, cx);
        let tm = self.task_mut(id).unwrap();
        tm.status = Status::Running;
        tm.followups += 1;
        tm.question.clear();
        tm.error.clear();
        tm.last = format!("comment: {}", text.lines().next().unwrap_or(""));
        self.select(id);
        self.save();
    }

    fn plan(&mut self, goal: &str, cx: &Cx) {
        let goal = goal.trim().to_string();
        let (Some(repo), Some(bin)) = (self.repo.clone(), self.installed("claude")) else { return };
        if goal.is_empty() {
            return;
        }
        self.planning = true;
        self.spawn(cx, move |send| send(Msg::Plan(run_planner(&bin, &repo.root, &goal))));
    }

    /// enter on a card: start it, jump to its agent, or review it. Headless workers show their live transcript.
    fn activate(&mut self, cx: &mut Cx) {
        let Some(id) = self.selected() else { return };
        let t = self.task(&id).unwrap().clone();
        if t.headless() {
            self.mode = Mode::Log(LogView { target: LogTarget::Task(id), scroll: 0, back: None });
            return;
        }
        let pane = self.live(&id).pane;
        match t.status {
            Status::Todo => self.start(&id, cx),
            Status::Running | Status::Blocked if pane => cx.act(Action::FocusTag(t.tag())),
            Status::Review if pane => cx.act(Action::FocusTag(t.tag())),
            Status::Review => self.open_diff(&id, cx),
            _ => {}
        }
    }

    fn confirm(&mut self, what: Pending) {
        if let Some(id) = self.selected().or_else(|| if let Mode::Diff(v) = &self.mode { Some(v.id.clone()) } else { None }) {
            self.mode = Mode::Confirm(Confirm { id, what });
        }
    }

    fn do_confirm(&mut self, c: Confirm, cx: &mut Cx) {
        self.mode = Mode::Board;
        match c.what {
            Pending::Merge => self.merge(&c.id, cx),
            Pending::Discard if self.task(&c.id).is_some_and(|t| !t.run.is_empty()) => self.discard_task(&c.id, None, cx),
            Pending::Discard => self.remove(&c.id, Then::Discarded, cx),
            Pending::StopRun => self.stop_run(&c.id, cx),
            Pending::MergeRun => self.merge_run(&c.id, cx),
            Pending::DiscardRun => self.discard_run(&c.id, cx),
            Pending::Retry => self.remove(&c.id, Then::Retry, cx),
            Pending::Delete => {
                self.store.tasks.retain(|t| t.id != c.id);
                self.live.remove(&c.id);
                let _ = std::fs::remove_file(self.paths.status(&c.id));
            }
        }
        self.save();
    }

    fn new_form(&self, editing: Option<&Task>) -> Form {
        match editing {
            Some(t) => Form {
                editing: Some(t.id.clone()),
                field: 0,
                title: Input::new(&t.title, false),
                prompt: Input::new(&t.prompt, true),
                agent: KINDS.iter().position(|k| k.0 == t.agent).unwrap_or(0),
                model: Input::new(&t.model, false),
                priority: PRIORITIES.iter().position(|p| p.0 == t.priority).unwrap_or(1),
                after: Input::new(&t.depends_on.join(", "), false),
                budget: Input::new(&if t.budget_usd > 0.0 { format!("{}", t.budget_usd) } else { String::new() }, false),
                button: 0,
                err: String::new(),
            },
            None => Form {
                editing: None,
                field: 0,
                title: Input::new("", false),
                prompt: Input::new("", true),
                agent: self.default_agent(),
                model: Input::new("", false),
                priority: 1,
                after: Input::new("", false),
                budget: Input::new("", false),
                button: 1,
                err: String::new(),
            },
        }
    }

    /// "a1b2, fix the parser" -> task ids: exact ids, or the TODO task whose title starts with it.
    fn resolve_after(&self, text: &str, me: Option<&str>) -> Result<Vec<String>, String> {
        let mut out = vec![];
        for part in text.split(',').map(|p| p.trim()).filter(|p| !p.is_empty()) {
            let low = part.to_lowercase();
            let hit = self.store.tasks.iter().filter(|t| Some(t.id.as_str()) != me && t.status != Status::Done).find(|t| t.id == part || t.title.to_lowercase().starts_with(&low));
            match hit {
                Some(t) => out.push(t.id.clone()),
                None => return Err(format!("no open task called '{part}'")),
            }
        }
        Ok(out)
    }

    fn submit_form(&mut self, mut form: Form, start: bool, cx: &mut Cx) {
        if form.prompt.text.trim().is_empty() && form.title.text.trim().is_empty() {
            form.err = "give it a title or a prompt".into();
            form.field = if form.title.text.trim().is_empty() { 0 } else { 1 };
            self.mode = Mode::Form(form);
            return;
        }
        let prompt = if form.prompt.text.trim().is_empty() { form.title.text.clone() } else { form.prompt.text.clone() };
        let after = match self.resolve_after(&form.after.text, form.editing.as_deref()) {
            Ok(a) => a,
            Err(e) => {
                form.err = e;
                form.field = 5;
                self.mode = Mode::Form(form);
                return;
            }
        };
        let budget = match form.budget.text.trim().trim_start_matches('$') {
            "" => 0.0,
            b => match b.parse::<f64>() {
                Ok(v) if v >= 0.0 => v,
                _ => {
                    form.err = "the budget is a number of dollars, like 1.5".into();
                    form.field = 6;
                    self.mode = Mode::Form(form);
                    return;
                }
            },
        };
        let priority = PRIORITIES[form.priority.min(PRIORITIES.len() - 1)].0;
        let id = match &form.editing {
            Some(id) => {
                let id = id.clone();
                let title = form.title.text.trim().to_string();
                if let Some(t) = self.task_mut(&id) {
                    if !title.is_empty() {
                        t.title = title;
                    }
                    t.prompt = prompt.trim().to_string();
                    t.agent = KINDS[form.agent].0.to_string();
                    t.model = form.model.text.trim().to_string();
                    t.error.clear();
                }
                self.save();
                id
            }
            None => self.add_task(&form.title.text, &prompt, form.agent, &form.model.text),
        };
        if let Some(t) = self.task_mut(&id) {
            t.priority = priority;
            t.depends_on = after;
            t.budget_usd = budget;
        }
        self.save();
        self.mode = Mode::Board;
        self.select(&id);
        if start {
            self.start(&id, cx);
        }
    }

    // ------------------------------------------------------------------ keys

    /// Keys while the lead panel is focused. false = not ours (the board handles it).
    fn lead_key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        let Some(run) = self.current_run().cloned() else {
            self.lead_focus = false;
            return false;
        };
        match k.code {
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => self.lead_focus = false,
            KeyCode::Up | KeyCode::Char('k') => {}
            KeyCode::Esc | KeyCode::Char('s') if run.state.active() => self.mode = Mode::Confirm(Confirm { id: run.id, what: Pending::StopRun }),
            KeyCode::Esc => self.lead_focus = false,
            KeyCode::Char('w') => self.mode = Mode::Watch(WatchView { run: run.id, sel: 0 }),
            KeyCode::Enter => self.mode = Mode::Log(LogView { target: LogTarget::Lead(run.id), scroll: 0, back: None }),
            KeyCode::Char('d') if !run.branch.is_empty() => self.open_run_diff(&run.id, cx),
            KeyCode::Char('m') if !run.state.active() && !run.branch.is_empty() => self.mode = Mode::Confirm(Confirm { id: run.id, what: Pending::MergeRun }),
            KeyCode::Char('x') => self.mode = Mode::Confirm(Confirm { id: run.id, what: Pending::DiscardRun }),
            KeyCode::Char('r') if run.state == store::RunState::Stopped => self.resume_run(&run.id, cx),
            KeyCode::Char('d' | 'm' | 'r' | 's') => {}
            _ => return false,
        }
        true
    }

    fn board_key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        // marked tasks: enter runs them, esc unmarks, whatever panel has focus
        if !self.marked.is_empty() {
            match k.code {
                KeyCode::Enter => {
                    self.mode = Mode::Batch;
                    return true;
                }
                KeyCode::Esc => {
                    self.marked.clear();
                    return true;
                }
                _ => {}
            }
        }
        if self.lead_focus {
            if self.lead_key(k, cx) {
                return true;
            }
        }
        let n = |s: &Self, c: usize| s.column(c).len();
        match k.code {
            KeyCode::Left | KeyCode::Char('h') => self.col = self.col.saturating_sub(1),
            KeyCode::Right | KeyCode::Char('l') => self.col = (self.col + 1).min(3),
            KeyCode::Tab => self.col = (self.col + 1) % 4,
            KeyCode::BackTab => self.col = (self.col + 3) % 4,
            KeyCode::Up | KeyCode::Char('k') if self.row[self.col] == 0 && self.current_run().is_some() => self.lead_focus = true,
            KeyCode::Up | KeyCode::Char('k') => self.row[self.col] = self.row[self.col].min(n(self, self.col).saturating_sub(1)).saturating_sub(1),
            KeyCode::Char('L') => {
                if self.repo.is_some() {
                    self.mode = Mode::LeadForm(self.new_lead_form());
                }
            }
            KeyCode::Char('R') => self.mode = Mode::Roster(RosterView { sel: 0, edit: None }),
            KeyCode::Char('w') => {
                if let Some(r) = self.current_run() {
                    self.mode = Mode::Watch(WatchView { run: r.id.clone(), sel: 0 });
                }
            }
            KeyCode::Down | KeyCode::Char('j') => self.row[self.col] = (self.row[self.col] + 1).min(n(self, self.col).saturating_sub(1)),
            KeyCode::Home | KeyCode::Char('g') => self.row[self.col] = 0,
            KeyCode::End | KeyCode::Char('G') => self.row[self.col] = n(self, self.col).saturating_sub(1),
            KeyCode::Char('n') => {
                if self.repo.is_some() {
                    self.mode = Mode::Form(self.new_form(None));
                }
            }
            KeyCode::Char('e') => {
                if let Some(t) = self.selected().and_then(|id| self.task(&id).cloned()) {
                    if t.status == Status::Todo {
                        self.mode = Mode::Form(self.new_form(Some(&t)));
                    }
                }
            }
            KeyCode::Char('o') => self.mode = Mode::Repo(Picker { input: Input::new("", false), sel: if self.store.repos.is_empty() { 0 } else { 1 }, err: String::new(), checking: false }),
            KeyCode::Char('P') => {
                if self.installed("claude").is_none() {
                    cx.notify("the planner needs claude installed");
                } else if self.planning {
                    cx.notify("already planning…");
                } else if self.repo.is_some() {
                    self.mode = Mode::Plan(Input::new("", true));
                }
            }
            KeyCode::Enter => self.activate(cx),
            KeyCode::Char(' ') => {
                if let Some(t) = self.selected().and_then(|id| self.task(&id).cloned()) {
                    if t.status == Status::Todo && t.run.is_empty() {
                        if let Some(i) = self.marked.iter().position(|m| *m == t.id) {
                            self.marked.remove(i);
                        } else {
                            self.marked.push(t.id.clone());
                        }
                    } else {
                        cx.notify("only tasks waiting in TODO can be marked to run together");
                    }
                }
            }
            KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Char('-') => {
                if let Some(id) = self.selected() {
                    let up = k.code != KeyCode::Char('-');
                    if let Some(t) = self.task_mut(&id).filter(|t| t.status != Status::Done) {
                        t.priority = (t.priority + if up { 1 } else { -1 }).clamp(-1, 2);
                        let name = PRIORITIES.iter().find(|p| p.0 == t.priority).map(|p| p.1).unwrap_or("normal");
                        cx.notify(format!("priority: {name}"));
                    }
                    self.save();
                }
            }
            _ => {
                let Some(id) = self.selected() else { return matches!(k.code, KeyCode::Char('d' | 'c' | 'm' | 'x' | 'r' | 't')) };
                let t = self.task(&id).unwrap().clone();
                let busy = self.live(&id).busy;
                let has_wt = !t.worktree.is_empty();
                let running_headless = t.headless() && self.live(&id).stop.is_some();
                let in_run = !t.run.is_empty();
                match k.code {
                    KeyCode::Char('d') if has_wt => self.open_diff(&id, cx),
                    KeyCode::Char('c') if has_wt && t.status != Status::Done && !busy && !running_headless => self.mode = Mode::Comment(id, Input::new("", true)),
                    KeyCode::Char('m') if in_run && has_wt && !busy && t.status == Status::Review => self.confirm(Pending::Merge),
                    KeyCode::Char('m') if !in_run && has_wt && !busy && matches!(t.status, Status::Review | Status::Running | Status::Blocked) => self.confirm(Pending::Merge),
                    KeyCode::Char('x') if !busy && t.status == Status::Todo && !in_run => self.confirm(Pending::Delete),
                    KeyCode::Char('x') if !busy && t.status != Status::Done => self.confirm(Pending::Discard),
                    KeyCode::Char('r') if !in_run && !busy && t.status != Status::Todo && !(t.status == Status::Done && t.outcome == "merged") => self.confirm(Pending::Retry),
                    KeyCode::Char('t') if t.headless() && has_wt && !busy => self.take_over(&id, cx),
                    KeyCode::Char('d' | 'c' | 'm' | 'x' | 'r' | 't') => {}
                    _ => return false,
                }
            }
        }
        true
    }

    fn diff_key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        let Mode::Diff(v) = &mut self.mode else { return false };
        let nfiles = v.data.as_ref().and_then(|d| d.as_ref().ok()).map(|d| d.files.len()).unwrap_or(0);
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('d') => self.mode = Mode::Board,
            KeyCode::Up | KeyCode::Char('k') => {
                v.file = v.file.saturating_sub(1);
                v.scroll = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                v.file = (v.file + 1).min(nfiles.saturating_sub(1));
                v.scroll = 0;
            }
            KeyCode::PageDown | KeyCode::Char(' ') => v.scroll += 20,
            KeyCode::PageUp => v.scroll = v.scroll.saturating_sub(20),
            KeyCode::Char('J') => v.scroll += 1,
            KeyCode::Char('K') => v.scroll = v.scroll.saturating_sub(1),
            KeyCode::Home | KeyCode::Char('g') => v.scroll = 0,
            KeyCode::Char('R') => {
                let id = v.id.clone();
                if self.run_ref(&id).is_some() {
                    self.open_run_diff(&id, cx);
                } else {
                    self.open_diff(&id, cx);
                }
            }
            KeyCode::Char('m') => {
                let id = v.id.clone();
                let what = if self.run_ref(&id).is_some() { Pending::MergeRun } else { Pending::Merge };
                self.mode = Mode::Confirm(Confirm { id, what });
            }
            KeyCode::Char('x') => {
                let id = v.id.clone();
                let what = if self.run_ref(&id).is_some() { Pending::DiscardRun } else { Pending::Discard };
                self.mode = Mode::Confirm(Confirm { id, what });
            }
            KeyCode::Char('c') => {
                let id = v.id.clone();
                if self.task(&id).is_some() {
                    self.mode = Mode::Comment(id, Input::new("", true));
                }
            }
            KeyCode::Enter => {
                let id = v.id.clone();
                if self.live(&id).pane {
                    let tag = self.task(&id).map(|t| t.tag()).unwrap_or_default();
                    cx.act(Action::FocusTag(tag));
                }
            }
            _ => return false,
        }
        true
    }

    fn form_key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        let Mode::Form(mut form) = std::mem::replace(&mut self.mode, Mode::Board) else { return false };
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        form.err.clear();
        match k.code {
            KeyCode::Esc => return true, // mode already Board
            KeyCode::Char('s') if ctrl => {
                self.submit_form(form, false, cx);
                return true;
            }
            KeyCode::Tab => form.field = (form.field + 1) % 8,
            KeyCode::BackTab => form.field = (form.field + 7) % 8,
            _ => {
                let used = match form.field {
                    0 => form.title.key(k),
                    1 => form.prompt.key(k),
                    3 => form.model.key(k),
                    5 => form.after.key(k),
                    6 => form.budget.key(k),
                    _ => false,
                };
                if !used {
                    match k.code {
                        KeyCode::Enter if form.field == 7 => {
                            let start = form.button == 1;
                            self.submit_form(form, start, cx);
                            return true;
                        }
                        KeyCode::Enter | KeyCode::Down => form.field = (form.field + 1).min(7),
                        KeyCode::Left if form.field == 4 => form.priority = form.priority.saturating_sub(1),
                        KeyCode::Right | KeyCode::Char(' ') if form.field == 4 => form.priority = (form.priority + 1).min(PRIORITIES.len() - 1),
                        KeyCode::Up => form.field = form.field.saturating_sub(1),
                        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if form.field == 2 => {
                            // cycle through installed agents (all of them if none are)
                            let any = self.agents.iter().any(|a| a.1.is_some());
                            let step = if k.code == KeyCode::Left { KINDS.len() - 1 } else { 1 };
                            for _ in 0..KINDS.len() {
                                form.agent = (form.agent + step) % KINDS.len();
                                if !any || self.installed(KINDS[form.agent].0).is_some() {
                                    break;
                                }
                            }
                        }
                        KeyCode::Left | KeyCode::Right if form.field == 7 => form.button = 1 - form.button,
                        _ => {}
                    }
                }
            }
        }
        self.mode = Mode::Form(form);
        true
    }

    fn picker_key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        let Mode::Repo(p) = &mut self.mode else { return false };
        let n = self.store.repos.len();
        match k.code {
            KeyCode::Esc => {
                if self.repo.is_some() {
                    self.mode = Mode::Board;
                }
            }
            KeyCode::Up => p.sel = p.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => p.sel = (p.sel + 1).min(n),
            KeyCode::Enter => {
                let path = if p.sel == 0 { expand(p.input.text.trim()) } else { PathBuf::from(&self.store.repos[p.sel - 1]) };
                if path.as_os_str().is_empty() {
                    p.err = "type a path to a git repo".into();
                } else {
                    self.open_repo(path, cx);
                }
            }
            KeyCode::Delete if p.sel > 0 => {
                self.store.repos.remove(p.sel - 1);
                p.sel = p.sel.min(self.store.repos.len());
                self.save();
            }
            _ => {
                if p.input.key(k) {
                    p.sel = 0;
                    p.err.clear();
                }
            }
        }
        true
    }
}

fn expand(s: &str) -> PathBuf {
    let s = s.trim_matches('"');
    if let Some(rest) = s.strip_prefix("~") {
        if let Some(h) = dirs::home_dir() {
            return h.join(rest.trim_start_matches(['/', '\\']));
        }
    }
    PathBuf::from(s)
}

fn ui_short(s: &str, n: usize) -> String {
    crate::ui::fit(s, n)
}

/// The lead planner: a headless read-only Claude run that splits a goal into independent task cards.
fn run_planner(bin: &Path, repo: &Path, goal: &str) -> Result<(Vec<(String, String)>, f64), String> {
    let prompt = format!(
        "You are the lead planner for several coding agents that will work in parallel on this repository, each alone in its own git worktree.\n\nGoal: {goal}\n\nLook at the code as much as you need, then reply with ONLY a JSON array (no prose, no code fence) of 2 to 6 independent tasks. Each item: {{\"title\": \"short imperative title, under 50 characters\", \"prompt\": \"complete, self-contained instructions for one agent: what to change, where, and how to check it works\"}}. Tasks must not depend on each other or edit the same lines."
    );
    let p = bin.to_string_lossy().to_string();
    let args = ["-p", &prompt, "--output-format", "json", "--permission-mode", "plan", "--max-turns", "8"];
    let mut cmd = if cfg!(windows) && (p.to_lowercase().ends_with(".cmd") || p.to_lowercase().ends_with(".bat")) {
        let mut c = std::process::Command::new("cmd.exe");
        c.arg("/c").arg(&p);
        c
    } else {
        std::process::Command::new(&p)
    };
    cmd.args(args).current_dir(repo).stdin(std::process::Stdio::null());
    cmd.env_remove("NO_COLOR");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().map_err(|e| format!("couldn't run claude: {e}"))?;
    parse_plan(&String::from_utf8_lossy(&out.stdout)).ok_or_else(|| {
        let err = String::from_utf8_lossy(&out.stderr);
        err.lines().rev().find(|l| !l.trim().is_empty()).map(|s| s.trim().to_string()).unwrap_or_else(|| "the planner's answer wasn't a task list".into())
    })
}

/// Pull `[{title, prompt}]` out of `claude -p --output-format json` output.
pub fn parse_plan(stdout: &str) -> Option<(Vec<(String, String)>, f64)> {
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    let text = v["result"].as_str()?;
    let (a, b) = (text.find('[')?, text.rfind(']')?);
    let arr: serde_json::Value = serde_json::from_str(text.get(a..=b)?).ok()?;
    let items: Vec<(String, String)> = arr
        .as_array()?
        .iter()
        .filter_map(|it| {
            let title = it["title"].as_str()?.trim().to_string();
            let prompt = it["prompt"].as_str().unwrap_or(&title).trim().to_string();
            (!title.is_empty()).then_some((title, prompt))
        })
        .collect();
    Some((items, v["total_cost_usd"].as_f64().unwrap_or(0.0)))
}

impl Drop for Agents {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // stop every headless agent this pane started (their runners kill the process trees)
        let mut any = false;
        for l in self.live.values() {
            if let Some(s) = &l.stop {
                s.store(true, Ordering::SeqCst);
                any = true;
            }
        }
        for l in self.runs_live.values() {
            if l.driving {
                l.stop.store(true, Ordering::SeqCst);
                any = true;
            }
        }
        for w in self.waiters.drain(..) {
            let _ = w.reply.send(Err("oriel is closing".into()));
        }
        if any && !cfg!(test) {
            std::thread::sleep(Duration::from_millis(300)); // give the killers a moment before oriel exits
        }
        // oriel is quitting: write the newest state now rather than trusting the writer thread to get there
        let mut closed = self.save_lock.lock().unwrap();
        if let Ok(j) = serde_json::to_vec_pretty(&self.store) {
            let _ = store::write_atomic(&self.paths.tasks(), &j);
        }
        *closed = true;
    }
}

impl Pane for Agents {
    fn title(&self) -> String {
        "agents".into()
    }
    fn icon(&self) -> &'static str {
        "robot"
    }
    fn subtitle(&self) -> Option<String> {
        let repo = self.repo.as_ref()?;
        let today = self.today();
        Some(if today > 0.0 { format!("{} · ${today:.2} today", repo.name) } else { repo.name.clone() })
    }
    fn badge(&self) -> Option<String> {
        let count = |s: Status| self.store.tasks.iter().filter(|t| t.status == s).count();
        let (run, blocked, review) = (count(Status::Running), count(Status::Blocked), count(Status::Review));
        let mut parts = vec![];
        if run > 0 {
            parts.push(format!("{run} running"));
        }
        if blocked > 0 {
            parts.push(format!("{blocked} ⚠"));
        }
        if parts.is_empty() && review > 0 {
            parts.push(format!("{review} review"));
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
    fn tick_every(&self) -> Option<Duration> {
        // elapsed times and spinners on running cards
        let lead = self.store.runs.iter().any(|r| r.state.active()) || self.merging.is_some() || !self.waiters.is_empty();
        if lead || self.store.tasks.iter().any(|t| t.status.active() || self.live.get(&t.id).map(|l| l.busy).unwrap_or(false)) || self.planning {
            Some(Duration::from_millis(250))
        } else {
            None
        }
    }
    fn poll(&mut self, cx: &mut Cx) {
        self.sync(cx);
    }
    fn tagged_panes(&mut self, live: &[(String, Option<Activity>)]) {
        let mine: Vec<(String, Option<Activity>)> = live.iter().filter(|(t, _)| t.starts_with("agent-task:")).cloned().collect();
        if self.tagged.as_ref() != Some(&mine) {
            self.tagged = Some(mine);
            if let Some(w) = self.waker.lock().unwrap().as_ref() {
                w.wake();
            }
        } else if self.tagged.is_none() {
            self.tagged = Some(mine);
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        self.sync(cx);
        self.draw(f, area, cx);
    }

    fn key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        self.sync(cx);
        match &self.mode {
            Mode::Board => self.board_key(k, cx),
            Mode::Diff(_) => self.diff_key(k, cx),
            Mode::Form(_) => self.form_key(k, cx),
            Mode::Repo(_) => self.picker_key(k, cx),
            Mode::LeadForm(_) => self.lead_form_key(k, cx),
            Mode::Roster(_) => self.roster_key(k),
            Mode::Watch(_) => self.watch_key(k, cx),
            Mode::Log(_) => self.log_key(k, cx),
            Mode::Batch => {
                self.mode = Mode::Board;
                let serial = match k.code {
                    KeyCode::Char('p') | KeyCode::Char('P') | KeyCode::Enter => Some(false),
                    KeyCode::Char('s') | KeyCode::Char('S') => Some(true),
                    KeyCode::Esc => None,
                    _ => {
                        self.mode = Mode::Batch;
                        return true;
                    }
                };
                if let Some(serial) = serial {
                    let ids = std::mem::take(&mut self.marked);
                    match self.start_batch(&ids, serial, cx) {
                        Ok(_) => cx.notify(format!("running {} task{} {}", ids.len(), if ids.len() == 1 { "" } else { "s" }, if serial { "one after another" } else { "together" })),
                        Err(e) => {
                            self.marked = ids;
                            cx.notify(e);
                        }
                    }
                }
                true
            }
            Mode::Confirm(_) => {
                let Mode::Confirm(c) = std::mem::replace(&mut self.mode, Mode::Board) else { return true };
                match k.code {
                    KeyCode::Char('y') | KeyCode::Enter => self.do_confirm(c, cx),
                    KeyCode::Char('n') | KeyCode::Esc => {}
                    _ => self.mode = Mode::Confirm(c),
                }
                true
            }
            Mode::Comment(..) | Mode::Plan(_) => {
                let (id, mut inp, plan) = match std::mem::replace(&mut self.mode, Mode::Board) {
                    Mode::Comment(id, i) => (Some(id), i, false),
                    Mode::Plan(i) => (None, i, true),
                    _ => return true,
                };
                match k.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter if !k.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) => {
                        if plan {
                            self.plan(&inp.text, cx);
                        } else if let Some(id) = id {
                            self.comment(&id, &inp.text, cx);
                        }
                    }
                    _ => {
                        inp.key(k);
                        self.mode = if plan { Mode::Plan(inp) } else { Mode::Comment(id.unwrap(), inp) };
                    }
                }
                true
            }
        }
    }

    fn paste(&mut self, text: &str, _cx: &mut Cx) {
        match &mut self.mode {
            Mode::Form(f) => match f.field {
                0 => f.title.insert(text),
                3 => f.model.insert(text),
                _ => {
                    f.field = 1;
                    f.prompt.insert(text)
                }
            },
            Mode::Comment(_, i) | Mode::Plan(i) => i.insert(text),
            Mode::Repo(p) => {
                p.input.insert(text.trim());
                p.sel = 0;
            }
            Mode::LeadForm(f) => match f.field {
                2 => f.model.insert(text.trim()),
                3 => f.parallel.insert(text.trim()),
                4 => f.budget.insert(text.trim()),
                _ => {
                    f.field = 0;
                    f.goal.insert(text)
                }
            },
            Mode::Roster(RosterView { edit: Some(e), .. }) => match e.field {
                0 => e.name.insert(text),
                2 => e.model.insert(text),
                4 => e.good_at.insert(text),
                5 => e.turns.insert(text.trim()),
                6 => e.budget.insert(text.trim()),
                _ => {}
            },
            _ => {}
        }
    }

    fn mouse(&mut self, ev: MouseEvent, _area: Rect, cx: &mut Cx) {
        if !matches!(self.mode, Mode::Board | Mode::Diff(_) | Mode::Watch(_) | Mode::Log(_)) {
            return; // a popup is open: the board under it doesn't take clicks
        }
        let pos = Position { x: ev.column, y: ev.row };
        let hit = self.hits.iter().rev().find(|(r, _)| r.contains(pos)).map(|h| h.1.clone());
        if let Mode::Log(v) = &mut self.mode {
            match ev.kind {
                MouseEventKind::ScrollUp => v.scroll += 3,
                MouseEventKind::ScrollDown => v.scroll = v.scroll.saturating_sub(3),
                _ => {}
            }
            return;
        }
        match (ev.kind, hit) {
            (MouseEventKind::Down(MouseButton::Left), Some(Hit::Card(c, r))) => {
                self.lead_focus = false;
                if self.col == c && self.row[c] == r {
                    self.activate(cx);
                } else {
                    self.col = c;
                    self.row[c] = r;
                }
            }
            (MouseEventKind::Down(MouseButton::Left), Some(Hit::LeadPanel)) => {
                if self.lead_focus {
                    if let Some(r) = self.current_run() {
                        self.mode = Mode::Log(LogView { target: LogTarget::Lead(r.id.clone()), scroll: 0, back: None });
                    }
                } else {
                    self.lead_focus = true;
                }
            }
            (MouseEventKind::Down(MouseButton::Left), Some(Hit::Tile(i))) => {
                if let Mode::Watch(w) = &mut self.mode {
                    if w.sel == i {
                        self.open_tile(i);
                    } else {
                        w.sel = i;
                    }
                }
            }
            (MouseEventKind::Down(MouseButton::Left), Some(Hit::Column(c))) => self.col = c,
            (MouseEventKind::Down(MouseButton::Left), Some(Hit::File(i))) => {
                if let Mode::Diff(v) = &mut self.mode {
                    v.file = i;
                    v.scroll = 0;
                }
            }
            (MouseEventKind::ScrollDown | MouseEventKind::ScrollUp, Some(h)) => {
                let down = ev.kind == MouseEventKind::ScrollDown;
                match (&mut self.mode, h) {
                    (Mode::Diff(v), Hit::File(_)) => {
                        let n = v.data.as_ref().and_then(|d| d.as_ref().ok()).map(|d| d.files.len()).unwrap_or(0);
                        v.file = if down { (v.file + 1).min(n.saturating_sub(1)) } else { v.file.saturating_sub(1) };
                        v.scroll = 0;
                    }
                    (Mode::Diff(v), _) => v.scroll = if down { v.scroll + 3 } else { v.scroll.saturating_sub(3) },
                    (Mode::Board, Hit::Card(c, _) | Hit::Column(c)) => {
                        self.col = c;
                        let n = self.column(c).len();
                        self.row[c] = if down { (self.row[c] + 1).min(n.saturating_sub(1)) } else { self.row[c].saturating_sub(1) };
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn side(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        self.sync(cx);
        self.draw_side(f, area, cx);
    }

    fn side_mouse(&mut self, ev: MouseEvent, _area: Rect, cx: &mut Cx) {
        if let MouseEventKind::Down(MouseButton::Left) = ev.kind {
            let pos = Position { x: ev.column, y: ev.row };
            match self.side_hits.iter().find(|(r, _)| r.contains(pos)).map(|h| h.1.clone()) {
                Some(Hit::Repo(path)) => {
                    if self.repo_key() != path {
                        self.open_repo(PathBuf::from(path), cx);
                    }
                }
                Some(Hit::NewTask) if self.repo.is_some() => self.mode = Mode::Form(self.new_form(None)),
                Some(Hit::Column(c)) if matches!(self.mode, Mode::Board) => {
                    self.col = c;
                    self.lead_focus = false;
                }
                Some(Hit::LeadPanel) if matches!(self.mode, Mode::Board) => self.lead_focus = true,
                _ => {}
            }
        }
    }
}
