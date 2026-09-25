//! agents: the orchestrator. A kanban board of coding-agent tasks on a git repo (TODO · RUNNING · REVIEW ·
//! DONE). Each task gets its own worktree and branch and runs Claude Code / Codex / Kimi in a real terminal tab;
//! hooks (`oriel report`) and the terminal's screen-scraped activity say what it's doing; a diff view reviews
//! the work, and `m` squash-merges it back. Design: docs/ai-research.md §3.
//!
//! Nothing slow runs on the UI thread: git, transcript parsing and status-file reads all happen on background
//! threads that send a `Msg` back and wake the pane.

mod cost;
mod git;
mod input;
mod store;
#[cfg(test)]
mod tests;
mod view;

use crate::pane::{Action, Activity, Cx, Pane, Waker};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use input::Input;
use ratatui::{
    Frame,
    layout::{Position, Rect},
};
use std::collections::HashMap;
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
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Pending {
    Merge,
    Discard,
    Delete,
    Retry,
}

struct Confirm {
    id: String,
    what: Pending,
}

struct Form {
    /// Some = editing this TODO task
    editing: Option<String>,
    field: usize, // 0 title, 1 prompt, 2 agent, 3 model, 4 buttons
    title: Input,
    prompt: Input,
    agent: usize,
    model: Input,
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

enum Mode {
    Board,
    Form(Form),
    Diff(DiffView),
    Confirm(Confirm),
    Comment(String, Input),
    Plan(Input),
    Repo(Picker),
}

#[derive(Clone)]
enum Hit {
    Card(usize, usize),
    Column(usize),
    File(usize),
    DiffBody,
    Repo(String),
    NewTask,
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
}

impl Agents {
    pub fn new(_cfg: &crate::config::Config) -> Self {
        Self::with_paths(Paths::default())
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
        Agents {
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
        }
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
        self.store
            .tasks
            .iter()
            .filter(|t| {
                let d = crate::panes::files::clock::local(t.started.max(t.created));
                (d.year, d.month, d.day) == (now.year, now.month, now.day)
            })
            .fold(0.0, |a, t| a + t.cost_usd)
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
        if dirty {
            self.save();
        }
        let any = self.store.tasks.iter().any(|t| t.status.active());
        self.active.store(any, Ordering::Relaxed);
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
        }
    }

    fn on_status(&mut self, id: &str, st: StatusFile, cx: &mut Cx) {
        let run_ts = self.live(id).run_ts;
        let Some(t) = self.task(id) else { return };
        if st.ts + 2 < run_ts || !matches!(t.status, Status::Running | Status::Blocked | Status::Review) {
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
        cx.notify(format!("◆ {title} is ready for review"));
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
        self.spawn(cx, move |send| {
            let wt = PathBuf::from(&t.worktree);
            let stats = if wt.is_dir() && !t.base_sha.is_empty() { git::stats(&wt, &t.base_sha).ok() } else { None };
            let c = match t.agent.as_str() {
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
        let ids: Vec<String> = self.store.tasks.iter().filter(|t| matches!(t.status, Status::Running | Status::Blocked | Status::Review)).map(|t| t.id.clone()).collect();
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
        if self.fake_agent.is_none() && self.installed(&t.agent).is_none() {
            let msg = format!("{} isn't installed — edit the task (e) to pick another agent", t.agent);
            self.task_mut(id).unwrap().error = msg.clone();
            cx.notify(msg);
            return;
        }
        if self.live(id).busy {
            return;
        }
        let Some(repo) = self.repo.clone() else { return };
        let slug = store::slug(&t.title, &t.id);
        let wt = self.paths.wt.join(&repo.name).join(&slug);
        let hook = if t.agent == "claude" { self.hook_exe.clone() } else { None };
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
        *self.live(id) = Live { busy: true, since: Some(Instant::now()), run_ts: store::now(), ..Default::default() };
        self.select(id);
        self.save();
        let (id2, root) = (id.to_string(), repo.root.clone());
        self.spawn(cx, move |send| send(Msg::Started(id2.clone(), git::start(&root, &wt, &slug, &id2, hook.as_deref()))));
    }

    fn on_started(&mut self, id: &str, r: Result<git::Started, String>, cx: &mut Cx) {
        self.live(id).busy = false;
        match r {
            Err(e) => {
                if let Some(t) = self.task_mut(id) {
                    t.status = Status::Todo;
                    t.last.clear();
                    t.error = e.clone();
                }
                cx.notify(format!("couldn't start: {e}"));
                self.select(id);
            }
            Ok(s) => {
                let Some(t) = self.task_mut(id) else { return };
                t.branch = s.branch;
                t.base_branch = s.base_branch;
                t.base_sha = s.base_sha;
                t.worktree = s.worktree.display().to_string();
                t.last = format!("starting {}…", t.agent);
                let t = t.clone();
                self.open_agent(&t, &t.prompt, false, cx);
            }
        }
    }

    /// Open (or re-open) the task's agent tab in its worktree.
    fn open_agent(&mut self, t: &Task, prompt: &str, followup: bool, cx: &mut Cx) {
        let (prog, args) = match &self.fake_agent {
            Some(f) => f.clone(),
            None => {
                let Some(bin) = self.installed(&t.agent) else {
                    cx.notify(format!("{} isn't installed", t.agent));
                    return;
                };
                wrap_cmd(&bin, agent_args(&t.agent, &t.model, prompt, followup), &t.id)
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
        self.spawn(cx, move |send| send(Msg::Diff(id, git::diff(Path::new(&t.repo), Path::new(&t.worktree), &t.base_sha))));
    }

    fn merge(&mut self, id: &str, cx: &mut Cx) {
        let Some(t) = self.task(id).cloned() else { return };
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

    /// enter on a card: start it, jump to its agent, or review it.
    fn activate(&mut self, cx: &mut Cx) {
        let Some(id) = self.selected() else { return };
        let t = self.task(&id).unwrap().clone();
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
            Pending::Discard => self.remove(&c.id, Then::Discarded, cx),
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
                button: 0,
                err: String::new(),
            },
            None => Form { editing: None, field: 0, title: Input::new("", false), prompt: Input::new("", true), agent: self.default_agent(), model: Input::new("", false), button: 1, err: String::new() },
        }
    }

    fn submit_form(&mut self, mut form: Form, start: bool, cx: &mut Cx) {
        if form.prompt.text.trim().is_empty() && form.title.text.trim().is_empty() {
            form.err = "give it a title or a prompt".into();
            form.field = if form.title.text.trim().is_empty() { 0 } else { 1 };
            self.mode = Mode::Form(form);
            return;
        }
        let prompt = if form.prompt.text.trim().is_empty() { form.title.text.clone() } else { form.prompt.text.clone() };
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
        self.mode = Mode::Board;
        self.select(&id);
        if start {
            self.start(&id, cx);
        }
    }

    // ------------------------------------------------------------------ keys

    fn board_key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        let n = |s: &Self, c: usize| s.column(c).len();
        match k.code {
            KeyCode::Left | KeyCode::Char('h') => self.col = self.col.saturating_sub(1),
            KeyCode::Right | KeyCode::Char('l') => self.col = (self.col + 1).min(3),
            KeyCode::Tab => self.col = (self.col + 1) % 4,
            KeyCode::BackTab => self.col = (self.col + 3) % 4,
            KeyCode::Up | KeyCode::Char('k') => self.row[self.col] = self.row[self.col].min(n(self, self.col).saturating_sub(1)).saturating_sub(1),
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
            _ => {
                let Some(id) = self.selected() else { return matches!(k.code, KeyCode::Char('d' | 'c' | 'm' | 'x' | 'r')) };
                let t = self.task(&id).unwrap().clone();
                let busy = self.live(&id).busy;
                let has_wt = !t.worktree.is_empty();
                match k.code {
                    KeyCode::Char('d') if has_wt => self.open_diff(&id, cx),
                    KeyCode::Char('c') if has_wt && t.status != Status::Done && !busy => self.mode = Mode::Comment(id, Input::new("", true)),
                    KeyCode::Char('m') if has_wt && !busy && matches!(t.status, Status::Review | Status::Running | Status::Blocked) => self.confirm(Pending::Merge),
                    KeyCode::Char('x') if !busy && t.status == Status::Todo => self.confirm(Pending::Delete),
                    KeyCode::Char('x') if !busy && t.status != Status::Done => self.confirm(Pending::Discard),
                    KeyCode::Char('r') if !busy && t.status != Status::Todo && !(t.status == Status::Done && t.outcome == "merged") => self.confirm(Pending::Retry),
                    KeyCode::Char('d' | 'c' | 'm' | 'x' | 'r') => {}
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
                self.open_diff(&id, cx);
            }
            KeyCode::Char('m') => {
                let id = v.id.clone();
                self.mode = Mode::Confirm(Confirm { id, what: Pending::Merge });
            }
            KeyCode::Char('x') => {
                let id = v.id.clone();
                self.mode = Mode::Confirm(Confirm { id, what: Pending::Discard });
            }
            KeyCode::Char('c') => {
                let id = v.id.clone();
                self.mode = Mode::Comment(id, Input::new("", true));
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
            KeyCode::Tab => form.field = (form.field + 1) % 5,
            KeyCode::BackTab => form.field = (form.field + 4) % 5,
            _ => {
                let used = match form.field {
                    0 => form.title.key(k),
                    1 => form.prompt.key(k),
                    3 => form.model.key(k),
                    _ => false,
                };
                if !used {
                    match k.code {
                        KeyCode::Enter if form.field == 4 => {
                            let start = form.button == 1;
                            self.submit_form(form, start, cx);
                            return true;
                        }
                        KeyCode::Enter | KeyCode::Down => form.field = (form.field + 1).min(4),
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
                        KeyCode::Left | KeyCode::Right if form.field == 4 => form.button = 1 - form.button,
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
        if self.store.tasks.iter().any(|t| t.status.active() || self.live.get(&t.id).map(|l| l.busy).unwrap_or(false)) || self.planning {
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
            _ => {}
        }
    }

    fn mouse(&mut self, ev: MouseEvent, _area: Rect, cx: &mut Cx) {
        if !matches!(self.mode, Mode::Board | Mode::Diff(_)) {
            return; // a popup is open: the board under it doesn't take clicks
        }
        let pos = Position { x: ev.column, y: ev.row };
        let hit = self.hits.iter().rev().find(|(r, _)| r.contains(pos)).map(|h| h.1.clone());
        match (ev.kind, hit) {
            (MouseEventKind::Down(MouseButton::Left), Some(Hit::Card(c, r))) => {
                if self.col == c && self.row[c] == r {
                    self.activate(cx);
                } else {
                    self.col = c;
                    self.row[c] = r;
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
                Some(Hit::Column(c)) if matches!(self.mode, Mode::Board) => self.col = c,
                _ => {}
            }
        }
    }
}
