//! Lead mode: one goal, one lead agent orchestrating headless workers on an integration branch.
//!
//! The lead runs on a driver thread. MCP leads call oriel's tools through `oriel mcp-lead` and the Broker;
//! text-protocol leads end each turn with JSON actions the driver runs and answers. Either way every tool call
//! arrives here as a `Call` and is handled on the UI thread against the board's state; git work, gates and
//! waits happen on background threads or are parked, and their answers go back through the call's reply.
//!
//! What oriel enforces by itself, so the lead doesn't have to (docs/ai-research.md "lead mode"):
//!  * plans are checked mechanically (plan.rs) and tasks start only when their dependencies are merged
//!  * merges are serialized: conflict check → one candidate commit → the gate on the merged tree → move the
//!    branch; a conflict or failed gate goes back to the same worker (2 tries), then to a fresh stronger one
//!  * a watchdog nudges stuck, spinning or hung workers, then re-dispatches them; a rejected rate limit pauses
//!    that vendor; the run's budget stops new work
//!  * the lead hears about all of it through `wait` events with compact result cards, never transcripts

use super::git;
use super::mcp::{self, Call};
use super::plan;
use super::roster;
use super::run::{self, Outcome, Proto};
use super::store::{self, Record, Run, RunState, Status, Task};
use super::stream::{Entry, Ev};
use super::{Agents, Msg, Then};
use crate::pane::Cx;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

pub type Reply = Sender<Result<String, String>>;

/// Most lines a live transcript keeps.
const LOG_CAP: usize = 400;
/// A text-protocol lead gets at most this many turns.
const MAX_TURNS: u32 = 80;
/// Fix rounds in the same session before a fresh worker takes over.
const MAX_ATTEMPTS: u32 = 2;

/// Something the lead should hear about.
pub struct RunEvent {
    pub task: String,
    pub card: Value,
    pub delivered: bool,
}

/// What the app knows about a lead run right now (not saved).
pub struct RunLive {
    pub stop: Arc<AtomicBool>,
    pub broker: Option<mcp::Broker>,
    pub log: Vec<Entry>,
    pub driving: bool,
    pub budget_warned: bool,
    pub events: Vec<RunEvent>,
}

impl RunLive {
    pub fn new() -> RunLive {
        RunLive { stop: Arc::new(AtomicBool::new(false)), broker: None, log: vec![], driving: false, budget_warned: false, events: vec![] }
    }
}

/// A `wait` call parked until something happens.
pub struct Waiter {
    pub run: String,
    pub ids: Vec<String>,
    pub deadline: Instant,
    pub reply: Reply,
}

/// Add or update (same id) an entry in a transcript.
pub fn upsert(log: &mut Vec<Entry>, e: Entry) {
    if !e.id.is_empty() {
        if let Some(x) = log.iter_mut().rev().take(60).find(|x| x.id == e.id) {
            *x = e;
            return;
        }
    }
    log.push(e);
    if log.len() > LOG_CAP {
        log.drain(..log.len() - LOG_CAP);
    }
}

// ------------------------------------------------------------------ the driver thread

pub struct Driver {
    pub run: String,
    pub agent: String,
    pub bin: PathBuf,
    pub model: String,
    pub cwd: PathBuf,
    pub tmp: PathBuf,
    /// The first prompt (the goal, or a state summary when resuming after a restart).
    pub goal: String,
    pub brief_mcp: String,
    pub brief_text: String,
    pub proto: Proto,
    pub budget: f64,
    /// Resume this session (after a restart).
    pub resume: String,
    pub fake: Option<run::Fake>,
    pub calls: Option<Arc<AtomicUsize>>,
}

/// Run the lead to the end. `send` reaches the pane; tool calls block until the pane answers (or the run stops).
pub fn drive(d: Driver, stop: Arc<AtomicBool>, send: &dyn Fn(Msg)) {
    let call = |tool: &str, args: Value| -> Result<String, String> {
        let (tx, rx) = mpsc::channel();
        send(Msg::Call(Call { run: d.run.clone(), tool: tool.to_string(), args, reply: tx }));
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(r) => return r,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err("oriel dropped the call".into()),
                Err(mpsc::RecvTimeoutError::Timeout) if stop.load(Ordering::SeqCst) => return Err("the run was stopped".into()),
                Err(_) => {}
            }
        }
    };
    let turn = |prompt: &str, system: &str, resume: &str, proto: &Proto, budget: f64| -> Outcome {
        let spec = run::lead_spec(&d.agent, &d.bin, &d.model, prompt, system, resume, proto, budget, &d.cwd, &d.tmp);
        let o = run::run(&spec, &stop, d.fake.as_ref(), &mut |e| send(Msg::Lead(d.run.clone(), e)));
        send(Msg::LeadTurn(d.run.clone(), o.clone()));
        o
    };
    let done = |r: Result<String, String>| send(Msg::LeadDone(d.run.clone(), r));
    let mut session = d.resume.clone();
    let mut spent = 0.0;
    let mut prompt = d.goal.clone();
    if let Proto::Mcp(..) = d.proto {
        let o = turn(&d.goal, &d.brief_mcp, &session, &d.proto, d.budget);
        spent += o.cost;
        if !o.session.is_empty() {
            session = o.session.clone();
        }
        if o.stopped || stop.load(Ordering::SeqCst) {
            return done(Err("stopped".into()));
        }
        let used = d.calls.as_ref().map(|c| c.load(Ordering::SeqCst)).unwrap_or(1);
        if used > 0 {
            return done(match o.error {
                Some(e) => Err(e),
                None => Ok(o.text),
            });
        }
        // the lead never reached oriel: its CLI didn't load the MCP server. Carry on with the text protocol.
        send(Msg::Lead(d.run.clone(), Ev::Entry(Entry::note("oriel's MCP tools didn't reach the lead — switching to the text protocol"))));
        send(Msg::LeadProto(d.run.clone()));
        prompt = format!("oriel's MCP tools were not available in your session. From now on use this protocol instead.\n{}\n\n{}", mcp::text_protocol(), d.goal);
    }
    let mut bad = 0;
    for _ in 0..MAX_TURNS {
        if stop.load(Ordering::SeqCst) {
            return done(Err("stopped".into()));
        }
        let left = if d.budget > 0.0 { d.budget - spent } else { 0.0 };
        if d.budget > 0.0 && left <= 0.0 {
            return done(Err(format!("the lead reached its own budget (${:.2})", d.budget)));
        }
        let o = turn(&prompt, &d.brief_text, &session, &Proto::Text, left.max(0.0));
        spent += o.cost;
        if !o.session.is_empty() {
            session = o.session.clone();
        }
        if o.stopped || stop.load(Ordering::SeqCst) {
            return done(Err("stopped".into()));
        }
        let Some(actions) = mcp::parse_actions(&o.text).filter(|a| !a.is_empty()) else {
            if let Some(e) = o.error {
                return done(Err(e));
            }
            bad += 1;
            if bad > 2 {
                return done(Err("the lead stopped answering with actions".into()));
            }
            prompt = "Your reply had no ```json actions block. Reply again, ending with exactly one ```json {\"actions\": [...]} block (use the done tool when the goal is met).".into();
            continue;
        };
        bad = 0;
        let mut results = vec![];
        for (i, (tool, args)) in actions.into_iter().enumerate() {
            if tool == "done" {
                let summary = args["summary"].as_str().unwrap_or("").to_string();
                let _ = call("done", args);
                return done(Ok(summary));
            }
            let r = call(&tool, args);
            results.push(format!("{}. {tool} → {}", i + 1, match r {
                Ok(t) => t,
                Err(e) => format!("ERROR: {e}"),
            }));
            if stop.load(Ordering::SeqCst) {
                return done(Err("stopped".into()));
            }
        }
        prompt = format!("Results from oriel:\n{}\n\nWhat next? End your reply with the ```json actions block (done when finished).", results.join("\n"));
    }
    done(Err("the lead ran out of turns".into()))
}

// ------------------------------------------------------------------ the pane side

pub(super) fn state_name(t: &Task) -> &'static str {
    match t.status {
        Status::Todo if t.queued => "queued",
        Status::Todo => "not started",
        Status::Running | Status::Blocked if !t.headless() => "running (the user took it over in a terminal tab)",
        Status::Running | Status::Blocked => "running",
        Status::Review if t.blocked => "blocked",
        Status::Review if !t.error.is_empty() => "failed",
        Status::Review => "review",
        Status::Done if t.outcome == "merged" => "merged",
        Status::Done => "discarded",
    }
}

fn arg<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("").trim()
}

/// Commands that legitimately run long without editing anything (the watchdog leaves them alone).
fn is_build(line: &str) -> bool {
    let l = line.to_lowercase();
    ["cargo", "npm", "pnpm", "yarn", "test", "build", "make", "pytest", "go ", "gradle", "mvn", "dotnet", "sleep", "tsc"].iter().any(|k| l.contains(k))
}

impl Agents {
    pub(super) fn run_ref(&self, id: &str) -> Option<&Run> {
        self.store.runs.iter().find(|r| r.id == id)
    }

    pub(super) fn run_mut(&mut self, id: &str) -> Option<&mut Run> {
        self.store.runs.iter_mut().find(|r| r.id == id)
    }

    /// The run shown at the top of the board: the newest open one in this repo.
    pub(super) fn current_run(&self) -> Option<&Run> {
        let key = self.repo_key();
        self.store.runs.iter().rev().find(|r| r.repo == key && r.state.open())
    }

    /// Lead + every worker of the run.
    pub(super) fn spend(&self, run: &str) -> f64 {
        let lead = self.run_ref(run).map(|r| r.cost_usd).unwrap_or(0.0);
        lead + self.store.tasks.iter().filter(|t| t.run == run).map(|t| t.cost_usd).sum::<f64>()
    }

    pub(super) fn run_tasks(&self, run: &str) -> Vec<&Task> {
        self.store.tasks.iter().filter(|t| t.run == run).collect()
    }

    /// The roster in use: the config's, or defaults from what's installed.
    pub(super) fn roster(&self) -> Vec<crate::config::RosterEntry> {
        if !self.roster.is_empty() {
            return self.roster.clone();
        }
        let installed = |a: &str| self.installed(a).is_some();
        roster::defaults(&installed)
    }

    fn installed_or_fake(&self, agent: &str, fake: bool) -> Option<PathBuf> {
        self.installed(agent).or_else(|| fake.then(|| PathBuf::from(agent)))
    }

    fn record(&mut self, worker: &str) -> &mut Record {
        self.store.records.entry(worker.to_string()).or_default()
    }

    /// Is a dependency (a task key or id in the run) merged?
    fn dep_state(&self, run: &str, dep: &str) -> Option<&Task> {
        self.store.tasks.iter().rev().find(|t| t.run == run && (t.key == dep || t.id == dep))
    }

    fn waiting_for(&self, t: &Task) -> Vec<String> {
        t.depends_on.iter().filter(|d| self.dep_state(&t.run, d).is_none_or(|x| !(x.status == Status::Done && x.outcome == "merged"))).cloned().collect()
    }

    /// The compact result card the lead sees.
    pub(super) fn card(&self, t: &Task) -> Value {
        let mut v = json!({
            "id": t.id,
            "key": t.key,
            "title": t.title,
            "worker": t.worker,
            "state": state_name(t),
        });
        let summary = if !t.summary.is_empty() { t.summary.clone() } else { t.last.clone() };
        if !summary.is_empty() {
            let words: Vec<&str> = summary.split_whitespace().take(120).collect();
            v["summary"] = json!(words.join(" "));
        }
        if !t.file_stats.is_empty() {
            v["files"] = Value::Array(t.file_stats.iter().take(30).map(|(p, a, r)| json!({"path": p, "+": a, "-": r})).collect());
        } else if !t.touched.is_empty() && t.status != Status::Todo {
            v["files"] = json!(t.touched);
        }
        if !t.gate.is_empty() {
            v["gate"] = if t.gate == "pass" { json!({"pass": true}) } else { json!({"pass": false, "first_failure": t.gate}) };
        }
        if !t.conflicts.is_empty() {
            v["conflicts"] = json!(t.conflicts);
        }
        if !t.questions.is_empty() {
            v["questions"] = json!(t.questions);
        }
        if !t.error.is_empty() {
            v["error"] = json!(t.error);
        }
        if t.status == Status::Todo && t.queued {
            let w = self.waiting_for(t);
            if !w.is_empty() {
                v["waiting_for"] = json!(w);
            }
        }
        if t.attempts + t.redispatches > 0 {
            v["attempts"] = json!(format!("{} fix round(s), {} fresh re-dispatch(es)", t.attempts, t.redispatches));
        }
        if t.tokens > 0 {
            v["tokens"] = json!(t.tokens);
        }
        v["cost_usd"] = json!((t.cost_usd * 1000.0).round() / 1000.0);
        v
    }

    fn board_json(&self, run: &str) -> Value {
        let ids = |f: &dyn Fn(&Task) -> bool| -> Vec<String> { self.run_tasks(run).into_iter().filter(|t| f(t)).map(|t| t.id.clone()).collect() };
        // a worker that just stopped is still "running" until its round is checked and reported
        let busy = |t: &Task| matches!(t.status, Status::Running | Status::Blocked) || self.live.get(&t.id).is_some_and(|l| l.stop.is_some() || l.checking || (l.busy && !t.want_merge));
        json!({
            "running": ids(&|t| busy(t)),
            "queued": ids(&|t| t.status == Status::Todo && t.queued),
            "review": ids(&|t| t.status == Status::Review && !t.want_merge && !t.blocked && !busy(t)),
            "merging": ids(&|t| t.want_merge && t.status == Status::Review),
            "blocked": ids(&|t| t.blocked && t.status == Status::Review),
            "merged": ids(&|t| t.status == Status::Done && t.outcome == "merged"),
            "spend_usd": (self.spend(run) * 1000.0).round() / 1000.0,
            "budget_usd": self.run_ref(run).map(|r| r.budget_usd).unwrap_or(0.0),
        })
    }

    /// Tell the lead (and wake any `wait`).
    pub(super) fn event(&mut self, task: &str, kind: &str) {
        let Some(t) = self.task(task).cloned() else { return };
        let mut card = self.card(&t);
        card["event"] = json!(kind);
        match kind {
            "bounced" => card["action"] = json!(format!("oriel sent it back to {} (fix round {} of {MAX_ATTEMPTS}); it merges by itself once fixed — just wait", t.worker, t.attempts)),
            "redispatched" => card["action"] = json!(format!("starting over with {} in a fresh worktree; review its card when it finishes", t.worker)),
            "acceptance_broken" => card["action"] = json!(format!("the acceptance command didn't run (see gate) — nothing was merged. Merge again with a command that works in {}: merge {{\"id\": \"{}\", \"acceptance\": \"...\"}} (\"\" for none)", git::gate_shell().2, t.id)),
            _ => {}
        }
        let l = self.runs_live.entry(t.run.clone()).or_insert_with(RunLive::new);
        l.events.push(RunEvent { task: task.to_string(), card, delivered: false });
        if l.events.len() > 300 {
            l.events.remove(0);
        }
        let why = match kind {
            "bounced" => format!(" — {}, back to {}", t.last.split(':').next().unwrap_or("").trim(), t.worker),
            "blocked" | "failed" => format!(" — {}", t.questions.first().cloned().unwrap_or_else(|| t.error.clone())),
            "redispatched" => format!(" — starting over with {}", t.worker),
            _ => String::new(),
        };
        let line = format!("{kind}: {}{why}", t.title);
        if let Some(r) = self.run_mut(&t.run) {
            r.log(&line);
        }
        self.check_waiters();
    }

    fn check_budget(&mut self, run: &str, cx: &mut Cx) -> Result<(), String> {
        let Some(r) = self.run_ref(run) else { return Err("no such run".into()) };
        let (spend, budget) = (self.spend(run), r.budget_usd);
        if budget > 0.0 && spend >= budget {
            self.warn_budget(run, cx);
            return Err(format!("the run's budget is spent (${spend:.2} of ${budget:.2}): no more worker runs. Merge what's finished, then call done."));
        }
        Ok(())
    }

    fn warn_budget(&mut self, run: &str, cx: &mut Cx) {
        let l = self.runs_live.entry(run.to_string()).or_insert_with(RunLive::new);
        if !l.budget_warned {
            l.budget_warned = true;
            let (spend, budget) = (self.spend(run), self.run_ref(run).map(|r| r.budget_usd).unwrap_or(0.0));
            if let Some(r) = self.run_mut(run) {
                r.log(&format!("budget reached: ${spend:.2} of ${budget:.2} — no more workers"));
            }
            cx.notify(format!("⚑ lead run hit its budget (${spend:.2} of ${budget:.2}) — no more workers"));
        }
    }

    /// Runs that just went over budget get told once (the board shows it too).
    pub(super) fn watch_budgets(&mut self, cx: &mut Cx) {
        let over: Vec<String> = self.store.runs.iter().filter(|r| r.state.active() && r.budget_usd > 0.0).filter(|r| self.spend(&r.id) >= r.budget_usd).map(|r| r.id.clone()).collect();
        for r in over {
            self.warn_budget(&r, cx);
        }
    }

    // ---------------------------------------------------------------- starting a run

    /// L → a new lead run: the integration branch + the lead's checkout (background), then the lead.
    pub(super) fn start_run(&mut self, goal: &str, agent: &str, model: &str, max_parallel: u32, budget: f64, cx: &mut Cx) -> Result<String, String> {
        let goal = goal.trim().to_string();
        if goal.is_empty() {
            return Err("what's the goal?".into());
        }
        let Some(repo) = self.repo.clone() else { return Err("open a repo first".into()) };
        if self.installed_or_fake(agent, self.fake_lead.is_some()).is_none() {
            return Err(format!("{agent} isn't installed"));
        }
        let taken: Vec<Task> = self.store.runs.iter().map(|r| Task { id: r.id.clone(), ..Default::default() }).chain(self.store.tasks.iter().cloned()).collect();
        let id = store::new_id(&taken);
        let slug = store::slug(&goal, &id);
        let wt = self.paths.wt.join(&repo.name).join(format!("lead-{slug}"));
        let proto = match self.lead_cfg.protocol.as_str() {
            "text" => "text",
            _ if run::supports_mcp(agent) => "mcp",
            _ => "text",
        };
        self.store.runs.push(Run {
            id: id.clone(),
            repo: repo.root.display().to_string(),
            goal: goal.clone(),
            agent: agent.to_string(),
            model: model.trim().to_string(),
            protocol: proto.into(),
            state: RunState::Starting,
            lead_budget_usd: self.lead_cfg.budget_usd,
            budget_usd: budget,
            max_parallel: max_parallel.clamp(1, 5),
            created: store::now(),
            ..Default::default()
        });
        self.runs_live.insert(id.clone(), RunLive::new());
        // remember the choice for next time
        self.lead_cfg.agent = agent.to_string();
        self.lead_cfg.model = model.trim().to_string();
        self.lead_cfg.max_parallel = max_parallel.clamp(1, 5);
        self.lead_cfg.run_budget_usd = budget;
        self.save_config();
        self.lead_focus = true;
        self.save();
        let (root, id2) = (repo.root.clone(), id.clone());
        self.spawn(cx, move |send| send(Msg::RunStarted(id2, git::start_run(&root, &wt, &slug))));
        Ok(id)
    }

    pub(super) fn on_run_started(&mut self, id: &str, r: Result<git::RunStarted, String>, cx: &mut Cx) {
        let Some(run) = self.run_mut(id) else { return };
        match r {
            Err(e) => {
                run.state = RunState::Stopped;
                run.error = e.clone();
                run.finished = store::now();
                cx.notify(format!("couldn't start the lead run: {e}"));
            }
            Ok(s) => {
                run.branch = s.branch;
                run.base_branch = s.base_branch;
                run.base_sha = s.base_sha;
                run.worktree = s.worktree.display().to_string();
                run.state = RunState::Running;
                let msg = format!("integration branch {} off {}", run.branch, run.base_branch);
                run.log(&msg);
                let goal = format!("Goal: {}\n\nStart with roster, then submit one plan.", run.goal);
                self.launch_lead(id, goal, String::new(), cx);
            }
        }
    }

    /// Start (or restart) the lead's driver thread.
    fn launch_lead(&mut self, id: &str, first: String, resume: String, cx: &mut Cx) {
        let Some(r) = self.run_ref(id).cloned() else { return };
        let fake = self.fake_lead.clone();
        let Some(bin) = self.installed_or_fake(&r.agent, fake.is_some()) else {
            if let Some(run) = self.run_mut(id) {
                run.state = RunState::Stopped;
                run.error = format!("{} isn't installed", r.agent);
            }
            return;
        };
        let live = self.runs_live.entry(id.to_string()).or_insert_with(RunLive::new);
        live.stop = Arc::new(AtomicBool::new(false));
        let stop = live.stop.clone();
        let mut proto = Proto::Text;
        let mut calls = None;
        if r.protocol == "mcp" {
            let exe = self.mcp_exe.clone().or_else(|| std::env::current_exe().ok());
            let (tx, waker) = (self.tx.clone(), self.waker.clone());
            let deliver: Arc<dyn Fn(Call) + Send + Sync> = Arc::new(move |c| {
                let _ = tx.send(Msg::Call(c));
                if let Some(w) = waker.lock().unwrap().as_ref() {
                    w.wake();
                }
            });
            match (exe, mcp::Broker::start(id.to_string(), deliver)) {
                (Some(exe), Ok(b)) => {
                    if r.agent == "kimi" {
                        // Kimi reads MCP servers only from the project: the lead's own checkout
                        let wt = PathBuf::from(&r.worktree);
                        let _ = mcp::write_kimi_mcp(&wt, &exe, b.port, &b.token);
                        git::exclude_path(&wt, ".kimi-code/");
                    }
                    proto = Proto::Mcp(exe, b.port, b.token.clone());
                    calls = Some(b.calls.clone());
                    self.runs_live.get_mut(id).unwrap().broker = Some(b);
                }
                _ => {
                    if let Some(run) = self.run_mut(id) {
                        run.protocol = "text".into();
                        run.log("couldn't start oriel's MCP bridge — using the text protocol");
                    }
                }
            }
        }
        let roster = self.roster();
        let brief = mcp::Brief { integration: &r.branch, base: &r.base_branch, max_parallel: r.max_parallel, budget: r.budget_usd, roster: &roster::roster_brief(&roster), shell: git::gate_shell().2 };
        let d = Driver {
            run: id.to_string(),
            agent: r.agent.clone(),
            bin,
            model: r.model.clone(),
            cwd: PathBuf::from(&r.worktree),
            tmp: self.paths.agents.join("tmp"),
            goal: first,
            brief_mcp: mcp::lead_system(&brief, true),
            brief_text: mcp::lead_system(&brief, false),
            proto,
            budget: (r.lead_budget_usd - if resume.is_empty() { 0.0 } else { r.cost_usd }).max(0.05),
            resume,
            fake,
            calls,
        };
        let label = format!("lead: {} · {} · {}", r.agent, if r.model.is_empty() { "default model" } else { &r.model }, if matches!(d.proto, Proto::Mcp(..)) { "MCP tools" } else { "text protocol" });
        let l = self.runs_live.get_mut(id).unwrap();
        l.driving = true;
        upsert(&mut l.log, Entry::note(&label));
        self.spawn(cx, move |send| drive(d, stop, send));
    }

    /// After a restart: pick a stopped run back up — the lead resumes its session with the current state, and
    /// the workers that were cut off continue theirs.
    pub(super) fn resume_run(&mut self, id: &str, cx: &mut Cx) {
        let Some(r) = self.run_ref(id).cloned() else { return };
        if r.state != RunState::Stopped || r.branch.is_empty() || !Path::new(&r.worktree).is_dir() {
            cx.notify("that run can't be resumed (its branch or checkout is gone)");
            return;
        }
        if let Some(run) = self.run_mut(id) {
            run.state = RunState::Running;
            run.error.clear();
            run.finished = 0;
            run.log("resumed");
        }
        let cut: Vec<String> = self.run_tasks(id).iter().filter(|t| t.headless() && t.status == Status::Review && t.last.starts_with("stopped")).map(|t| t.id.clone()).collect();
        for t in &cut {
            self.worker_again(t, "oriel was restarted while you worked. Continue the task where you left off and finish with your report.", cx);
        }
        let state = json!({"board": self.board_json(id), "tasks": self.run_tasks(id).iter().map(|t| self.card(t)).collect::<Vec<_>>()});
        let first = format!("oriel restarted and resumed this run. Goal (unchanged): {}\n\nCurrent state:\n{state}\n\nCarry on: wait for events, merge what's ready, finish with done.", r.goal);
        self.launch_lead(id, first, r.session_id.clone(), cx);
        self.schedule(cx);
    }

    // ---------------------------------------------------------------- lead events

    pub(super) fn on_lead_ev(&mut self, id: &str, e: Ev) {
        match e {
            Ev::Entry(en) => {
                if en.kind == 's' {
                    let first = en.label.lines().find(|l| !l.trim().is_empty()).unwrap_or("").to_string();
                    if let Some(r) = self.run_mut(id) {
                        r.log(&format!("“ {first}"));
                    }
                }
                if let Some(l) = self.runs_live.get_mut(id) {
                    upsert(&mut l.log, en);
                }
            }
            Ev::Session(s) => {
                if let Some(r) = self.run_mut(id) {
                    r.session_id = s;
                }
            }
            Ev::Limit(v) => self.on_limit(&v, "claude"),
            Ev::Error(m) => {
                if let Some(l) = self.runs_live.get_mut(id) {
                    upsert(&mut l.log, Entry::error(&m));
                }
            }
            _ => {}
        }
    }

    pub(super) fn on_lead_turn(&mut self, id: &str, o: Outcome) {
        if let Some(r) = self.run_mut(id) {
            r.cost_usd += o.cost;
            r.turns += 1;
            if !o.session.is_empty() {
                r.session_id = o.session;
            }
        }
    }

    pub(super) fn on_lead_done(&mut self, id: &str, r: Result<String, String>, cx: &mut Cx) {
        if let Some(l) = self.runs_live.get_mut(id) {
            l.driving = false;
            l.broker = None;
        }
        self.answer_waiters(id, "the lead has finished");
        let Some(run) = self.run_mut(id) else { return };
        if run.state != RunState::Running {
            return; // stopped by the user: already handled
        }
        run.finished = store::now();
        match r {
            Ok(summary) => {
                if run.summary.is_empty() {
                    run.summary = summary.lines().find(|l| !l.trim().is_empty()).unwrap_or("").to_string();
                }
                run.state = RunState::Review;
                let msg = format!("lead finished · review {} (d) and merge it (m)", run.branch);
                run.log(&msg);
                cx.notify(format!("⚑ {msg}"));
            }
            Err(e) if e == "stopped" => run.state = RunState::Stopped,
            Err(e) => {
                run.state = RunState::Stopped;
                run.error = e.clone();
                run.log(&format!("lead stopped: {e}"));
                cx.notify(format!("⚑ lead stopped: {e}"));
            }
        }
    }

    /// A rate limit event: a rejected one pauses that vendor until it resets.
    fn on_limit(&mut self, v: &Value, agent: &str) {
        let now = store::now();
        let w = roster::claude_event(v, now);
        if !w.is_empty() && agent == "claude" {
            self.limits.claude = w;
        }
        if v["status"].as_str() == Some("rejected") {
            let until = v["resetsAt"].as_i64().or(v["resets_at"].as_i64()).filter(|t| *t > now).unwrap_or(now + 15 * 60);
            self.paused.insert(agent.to_string(), until);
        }
    }

    // ---------------------------------------------------------------- tool calls

    pub(super) fn on_call(&mut self, c: Call, cx: &mut Cx) {
        let Some(run) = self.run_ref(&c.run).cloned() else {
            let _ = c.reply.send(Err("that lead run is gone".into()));
            return;
        };
        if !run.state.active() && !matches!(c.tool.as_str(), "task_status" | "roster" | "note" | "done" | "task_diff") {
            let _ = c.reply.send(Err("the run has been stopped".into()));
            return;
        }
        let a = &c.args;
        let r: Result<String, String> = match c.tool.as_str() {
            "roster" => Ok(self.roster_json().to_string()),
            "plan" => self.call_plan(&run, a, cx),
            "spawn_task" => {
                let one = json!({"tasks": [{
                    "id": if arg(a, "id").is_empty() { format!("s{}", self.run_tasks(&run.id).len() + 1) } else { arg(a, "id").to_string() },
                    "title": a["title"], "goal": if a["goal"].is_string() { a["goal"].clone() } else { a["prompt"].clone() },
                    "worker": a["worker"], "owns": if a["owns"].is_null() { a["files"].clone() } else { a["owns"].clone() },
                    "depends_on": a["depends_on"], "acceptance": a["acceptance"], "size": a["size"],
                }]});
                self.call_plan(&run, &one, cx)
            }
            "task_status" => {
                let id = arg(a, "id");
                if id.is_empty() {
                    let cards: Vec<Value> = self.run_tasks(&run.id).iter().map(|t| self.card(t)).collect();
                    Ok(json!({"integration_branch": run.branch, "board": self.board_json(&run.id), "tasks": cards}).to_string())
                } else {
                    self.task_in(&run, id).map(|t| self.card(&t).to_string())
                }
            }
            "wait" => return self.call_wait(&run, a, c.reply),
            "task_diff" | "get_diff" => {
                return match self.task_in(&run, arg(a, "id")) {
                    Err(e) => {
                        let _ = c.reply.send(Err(e));
                    }
                    Ok(t) if t.worktree.is_empty() => {
                        let _ = c.reply.send(Err(format!("{} has no worktree ({})", t.id, state_name(&t))));
                    }
                    Ok(t) => {
                        let (path, page) = (arg(a, "path").to_string(), a["page"].as_u64().unwrap_or(1) as usize);
                        let reply = c.reply;
                        self.spawn(cx, move |_| {
                            let _ = reply.send(git::diff_text(Path::new(&t.worktree), &t.base_sha, &path, page, 200));
                        });
                    }
                };
            }
            "send_followup" => self.call_followup(&run, arg(a, "id"), arg(a, "text"), cx),
            "merge" => self.task_in(&run, arg(a, "id")).and_then(|t| {
                if let Some(acc) = a["acceptance"].as_str() {
                    if let Some(tm) = self.task_mut(&t.id) {
                        tm.acceptance = acc.trim().to_string();
                        tm.gate.clear();
                    }
                }
                self.queue_merge(&t.id)
            }),
            "resolve_conflicts" => return self.call_resolve(&run, arg(a, "id"), c.reply, cx),
            "discard" => {
                return match self.task_in(&run, arg(a, "id")) {
                    Ok(t) => self.discard_task(&t.id, Some(c.reply), cx),
                    Err(e) => {
                        let _ = c.reply.send(Err(e));
                    }
                };
            }
            "note" => {
                let text = arg(a, "text").to_string();
                if text.is_empty() {
                    Err("note needs text".into())
                } else {
                    let needs = a["needs_user"].as_bool() == Some(true);
                    if let Some(r) = self.run_mut(&run.id) {
                        r.notes.push((store::now(), text.clone()));
                        r.log(&format!("✎ {text}"));
                    }
                    if needs {
                        cx.notify(format!("⚑ the lead needs you: {text}"));
                    }
                    Ok("noted on the board".into())
                }
            }
            "done" => {
                let summary = arg(a, "summary").to_string();
                if let Some(r) = self.run_mut(&run.id) {
                    r.summary = summary.clone();
                    r.log(&format!("done: {summary}"));
                }
                let open = self.run_tasks(&run.id).iter().filter(|t| matches!(t.status, Status::Running | Status::Blocked) || (t.status == Status::Todo && t.queued) || t.want_merge && t.status == Status::Review).count();
                Ok(if open > 0 { format!("noted. {open} task(s) are still running or merging; the user reviews {} when they finish", run.branch) } else { format!("thanks — the user will review {} and merge it", run.branch) })
            }
            other => Err(format!("no tool called {other}")),
        };
        self.log_call(&run.id, &c.tool, a, &r);
        let _ = c.reply.send(r);
    }

    pub(super) fn roster_json(&self) -> Value {
        let roster = self.roster();
        let installed = |ag: &str| self.fake_worker.is_some() || self.installed(ag).is_some();
        let busy = |name: &str| self.store.tasks.iter().filter(|t| t.worker == name && t.status == Status::Running).count();
        let mut v = roster::roster_json(&roster, &self.limits, &installed, &busy);
        let now = store::now();
        for w in v["workers"].as_array_mut().into_iter().flatten() {
            let name = w["name"].as_str().unwrap_or("").to_string();
            let agent = w["agent"].as_str().unwrap_or("").to_string();
            if let Some(r) = self.store.records.get(&name).filter(|r| r.tasks + r.merged > 0) {
                w["track_record"] = json!(format!(
                    "{} task(s): {} merged ({} first try), {} gate fail(s), {} conflict(s), {} failed, avg ${:.2}",
                    r.tasks,
                    r.merged,
                    r.first_try,
                    r.gate_fails,
                    r.conflicts,
                    r.failed,
                    if r.tasks > 0 { r.cost_usd / r.tasks as f64 } else { 0.0 }
                ));
            }
            if let Some(until) = self.paused.get(&agent).filter(|u| **u > now) {
                w["paused"] = json!(format!("rate limited for {} more minutes — don't use it now", (until - now) / 60 + 1));
            }
        }
        v
    }

    /// A one-line, human record of a tool call in the run's log (the lead panel shows the last few).
    fn log_call(&mut self, run: &str, tool: &str, a: &Value, r: &Result<String, String>) {
        let title = |id: &str| self.store.tasks.iter().rev().find(|t| t.run == run && (t.id == id || t.key == id)).map(|t| t.title.clone()).unwrap_or_else(|| id.to_string());
        let line = match (tool, r) {
            ("roster" | "task_status" | "note" | "done" | "wait" | "task_diff" | "get_diff", _) => return,
            ("plan" | "spawn_task", Ok(t)) => {
                let v: Value = serde_json::from_str(t).unwrap_or(Value::Null);
                let items: Vec<String> = v["tasks"].as_array().into_iter().flatten().map(|x| format!("{} → {}", x["key"].as_str().unwrap_or("?"), x["worker"].as_str().unwrap_or("?"))).collect();
                let mut s = format!("planned {} task{}: {}", items.len(), if items.len() == 1 { "" } else { "s" }, items.join(", "));
                if v["solo"] == true {
                    s.push_str(" (one at a time)");
                }
                s
            }
            ("plan" | "spawn_task", Err(e)) => format!("plan refused: {}", e.lines().nth(1).unwrap_or(e).trim_start_matches("- ")),
            ("merge", Ok(_)) => format!("queued {} for merging", title(arg(a, "id"))),
            ("send_followup", Ok(_)) => format!("follow-up to {}: {}", title(arg(a, "id")), arg(a, "text").lines().next().unwrap_or("")),
            (t, Ok(x)) => format!("{t} {} ✓ {}", title(arg(a, "id")), x.lines().next().unwrap_or("")),
            (t, Err(e)) => format!("{t} {} ✗ {}", title(arg(a, "id")), e.lines().next().unwrap_or("")),
        };
        if let Some(x) = self.run_mut(run) {
            x.log(&crate::ui::fit(&line, 180));
        }
    }

    fn task_in(&self, run: &Run, id: &str) -> Result<Task, String> {
        match self.store.tasks.iter().rev().find(|t| t.run == run.id && (t.id == id || (!t.key.is_empty() && t.key == id))) {
            Some(t) => Ok(t.clone()),
            None => {
                let ids: Vec<&str> = self.run_tasks(&run.id).iter().map(|t| t.id.as_str()).collect();
                Err(format!("no task {id:?} in this run (tasks: {})", if ids.is_empty() { "none yet".into() } else { ids.join(", ") }))
            }
        }
    }

    fn call_plan(&mut self, run: &Run, a: &Value, cx: &mut Cx) -> Result<String, String> {
        let items: Vec<plan::Item> = a["tasks"].as_array().ok_or("plan needs a tasks list")?.iter().enumerate().map(|(i, v)| plan::parse_item(v, i)).collect();
        let existing: Vec<plan::Existing> = self
            .run_tasks(&run.id)
            .iter()
            .filter(|t| !(t.status == Status::Done && t.outcome == "discarded"))
            .map(|t| plan::Existing { key: t.key.clone(), id: t.id.clone(), owns: if t.owns.is_empty() { t.touched.clone() } else { t.owns.clone() }, open: t.status != Status::Done })
            .collect();
        if let Some(dup) = items.iter().find(|i| existing.iter().any(|e| e.key == i.key)) {
            return Err(format!("task id {:?} is already used in this run — pick new ids", dup.key));
        }
        let roster = self.roster();
        let workers: Vec<String> = roster.iter().filter(|w| w.enabled && (self.fake_worker.is_some() || self.installed(&w.agent).is_some())).map(|w| w.name.clone()).collect();
        let checked = plan::check(items, &existing, &workers).map_err(|e| format!("plan rejected, nothing started — fix these and submit again:\n- {}", e.join("\n- ")))?;
        self.check_budget(&run.id, cx)?;
        let mut out = vec![];
        for it in checked.items {
            let w = roster.iter().find(|w| w.name.eq_ignore_ascii_case(&it.worker)).cloned().unwrap_or_default();
            let id = store::new_id(&self.store.tasks);
            self.store.tasks.push(Task {
                id: id.clone(),
                repo: run.repo.clone(),
                title: it.title.chars().take(80).collect(),
                prompt: it.goal.clone(),
                agent: w.agent.clone(),
                model: w.model.clone(),
                mode: "headless".into(),
                run: run.id.clone(),
                worker: w.name.clone(),
                tier: w.tier.clone(),
                queued: true,
                max_turns: w.max_turns,
                budget_usd: w.budget_usd,
                key: it.key.clone(),
                owns: it.owns.clone(),
                reads: it.reads.clone(),
                depends_on: it.depends_on.clone(),
                acceptance: it.acceptance.clone(),
                size: it.size.clone(),
                kind: it.kind.clone(),
                created: store::now(),
                last: "queued".into(),
                ..Default::default()
            });
            self.live.insert(id.clone(), Default::default());
            out.push((id, it.key, w.name));
        }
        self.schedule(cx);
        let tasks: Vec<Value> = out
            .iter()
            .map(|(id, key, w)| {
                let t = self.task(id).unwrap();
                let mut v = json!({"id": id, "key": key, "worker": w, "state": state_name(t)});
                let wf = self.waiting_for(t);
                if !wf.is_empty() {
                    v["waiting_for"] = json!(wf);
                }
                v
            })
            .collect();
        let mut v = json!({"tasks": tasks});
        if checked.serial {
            v["solo"] = json!(true);
        }
        if !checked.notes.is_empty() {
            v["notes"] = json!(checked.notes);
        }
        Ok(v.to_string())
    }

    fn call_followup(&mut self, run: &Run, id: &str, text: &str, cx: &mut Cx) -> Result<String, String> {
        let t = self.task_in(run, id)?;
        if text.is_empty() {
            return Err("send_followup needs text".into());
        }
        if matches!(t.status, Status::Running | Status::Blocked) || self.live.get(&t.id).is_some_and(|l| l.stop.is_some() || l.busy || l.checking) {
            return Err(if t.want_merge {
                format!("{} is already back with its worker fixing a failed merge (round {} of {MAX_ATTEMPTS}); oriel merges it when that's done — wait for the event", t.id, t.attempts)
            } else {
                format!("{} is still running — wait for it first", t.id)
            });
        }
        if t.worktree.is_empty() || !Path::new(&t.worktree).is_dir() {
            return Err(format!("{} has no worktree any more ({})", t.id, state_name(&t)));
        }
        self.check_budget(&run.id, cx)?;
        if let Some(tm) = self.task_mut(&t.id) {
            tm.blocked = false;
            tm.questions.clear();
        }
        self.worker_again(&t.id, text, cx);
        Ok(format!("sent to {} — {} is running again", t.worker, t.id))
    }

    fn call_resolve(&mut self, run: &Run, id: &str, reply: Reply, cx: &mut Cx) {
        let r = self.task_in(run, id).and_then(|t| {
            let busy = self.live.get(&t.id).is_some_and(|l| l.stop.is_some() || l.busy || l.checking);
            if matches!(t.status, Status::Running | Status::Blocked) || busy {
                Err(format!("{} is busy — wait for it first", t.id))
            } else if t.worktree.is_empty() || !Path::new(&t.worktree).is_dir() {
                Err(format!("{} has no worktree any more ({})", t.id, state_name(&t)))
            } else {
                Ok(t)
            }
        });
        match r.and_then(|t| self.check_budget(&run.id, cx).map(|_| t)) {
            Err(e) => {
                let _ = reply.send(Err(e));
            }
            Ok(t) => self.start_resolve(&t.id, Some(reply), cx),
        }
    }

    /// Merge the integration branch into the task's worktree (background), then hand the markers to its worker.
    fn start_resolve(&mut self, id: &str, reply: Option<Reply>, cx: &mut Cx) {
        let Some(t) = self.task(id).cloned() else { return };
        let integ = self.run_ref(&t.run).map(|r| r.branch.clone()).unwrap_or_default();
        self.live(id).busy = true;
        if let Some(tm) = self.task_mut(id) {
            tm.last = format!("merging {integ} into its worktree…");
        }
        let lock = self.merge_lock.clone();
        let id2 = id.to_string();
        self.spawn(cx, move |send| {
            let _g = lock.lock().unwrap_or_else(|e| e.into_inner());
            let r = git::start_resolve(Path::new(&t.worktree), &integ, &t.title);
            send(Msg::Resolve(id2, r, reply));
        });
    }

    pub(super) fn on_resolve(&mut self, id: &str, r: Result<Vec<String>, String>, reply: Option<Reply>, cx: &mut Cx) {
        self.live(id).busy = false;
        let Some(t) = self.task(id).cloned() else { return };
        let integ = self.run_ref(&t.run).map(|r| r.branch.clone()).unwrap_or_default();
        let answer = match r {
            Err(e) => Err(e),
            Ok(files) if files.is_empty() => {
                if let Some(tm) = self.task_mut(id) {
                    tm.conflicts.clear();
                    tm.last = format!("{integ} merged in cleanly");
                }
                if t.want_merge {
                    let _ = self.queue_merge(id);
                }
                Ok(format!("{integ} merged into {id}'s worktree without conflicts{}", if t.want_merge { "; it's queued for merging again" } else { "; call merge" }))
            }
            Ok(files) => {
                let prompt = format!(
                    "oriel merged the latest integration branch ({integ}) into your worktree to bring in other workers' finished work. These files now contain git conflict markers (<<<<<<< / ======= / >>>>>>>): {}.\n\
                     Resolve every conflict: keep the intent of both sides (the integration side is other merged work, keep it), remove all the markers, and make sure the code still builds. Don't run git commands: oriel commits the merge for you. Finish with your report.",
                    files.join(", ")
                );
                self.live(id).resolving = true;
                if let Some(tm) = self.task_mut(id) {
                    tm.conflicts = files.clone();
                }
                self.worker_again(id, &prompt, cx);
                Ok(format!("{} is resolving conflicts in {}{}", t.worker, files.join(", "), if t.want_merge { "; oriel merges it when it's done" } else { " — wait for it, then merge" }))
            }
        };
        if let Some(reply) = reply {
            let _ = reply.send(answer);
        } else if let Err(e) = answer {
            self.bounce_failed(id, &format!("couldn't start resolving conflicts: {e}"), cx);
        }
    }

    fn call_wait(&mut self, run: &Run, a: &Value, reply: Reply) {
        let ids: Vec<String> = a["ids"].as_array().map(|x| x.iter().filter_map(|i| i.as_str()).map(String::from).collect()).unwrap_or_default();
        let ids: Vec<String> = ids.iter().map(|i| self.task_in(run, i).map(|t| t.id).unwrap_or_else(|_| i.clone())).collect();
        let secs = a["timeout_s"].as_u64().unwrap_or(300).clamp(1, 600);
        self.waiters.push(super::lead::Waiter { run: run.id.clone(), ids, deadline: Instant::now() + Duration::from_secs(secs), reply });
        self.check_waiters();
    }

    /// Answer the waits that have news (or timed out).
    pub(super) fn check_waiters(&mut self) {
        if self.waiters.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut keep = vec![];
        for w in std::mem::take(&mut self.waiters) {
            let board = self.board_json(&w.run);
            let nothing_left = board["running"].as_array().is_some_and(|a| a.is_empty()) && board["queued"].as_array().is_some_and(|a| a.is_empty()) && board["merging"].as_array().is_some_and(|a| a.is_empty());
            let l = self.runs_live.entry(w.run.clone()).or_insert_with(RunLive::new);
            let news: Vec<usize> = l.events.iter().enumerate().filter(|(_, e)| !e.delivered && (w.ids.is_empty() || w.ids.contains(&e.task))).map(|(i, _)| i).collect();
            if !news.is_empty() || now >= w.deadline || (nothing_left && l.events.iter().all(|e| e.delivered)) {
                let cards: Vec<Value> = news
                    .iter()
                    .map(|&i| {
                        l.events[i].delivered = true;
                        l.events[i].card.clone()
                    })
                    .collect();
                let mut v = json!({"events": cards, "board": board});
                if cards.is_empty() {
                    v["note"] = json!(if nothing_left { "nothing is running, queued or merging" } else { "no news before the timeout" });
                }
                let _ = w.reply.send(Ok(v.to_string()));
            } else {
                keep.push(w);
            }
        }
        self.waiters.extend(keep);
    }

    fn answer_waiters(&mut self, run: &str, why: &str) {
        let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.waiters).into_iter().partition(|w: &Waiter| w.run == run);
        self.waiters = rest;
        for w in mine {
            let _ = w.reply.send(Err(why.to_string()));
        }
    }

    // ---------------------------------------------------------------- workers

    /// Start queued workers whose dependencies are merged, while their run has free slots (and the vendor
    /// isn't paused, and the same model didn't just start: its cache is warming).
    pub(super) fn schedule(&mut self, cx: &mut Cx) {
        let now = store::now();
        self.paused.retain(|_, until| *until > now);
        let runs: Vec<(String, u32)> = self.store.runs.iter().filter(|r| r.state == RunState::Running).map(|r| (r.id.clone(), r.max_parallel.clamp(1, 5))).collect();
        for (run, max) in runs {
            loop {
                let running = self.store.tasks.iter().filter(|t| t.run == run && t.headless() && matches!(t.status, Status::Running | Status::Blocked)).count() as u32;
                if running >= max {
                    break;
                }
                let mut ready: Vec<&Task> = self.store.tasks.iter().filter(|t| t.run == run && t.status == Status::Todo && t.queued && !self.paused.contains_key(&t.agent)).filter(|t| self.waiting_for(t).is_empty()).collect();
                ready.sort_by_key(|t| (t.created, t.id.clone()));
                let pick = ready.iter().find(|t| self.last_start.get(&(t.agent.clone(), t.model.clone())).is_none_or(|s| s.elapsed() >= self.stagger)).map(|t| t.id.clone());
                let Some(id) = pick else { break };
                if self.check_budget(&run, cx).is_err() {
                    break;
                }
                let t = self.task(&id).unwrap();
                self.last_start.insert((t.agent.clone(), t.model.clone()), Instant::now());
                if let Some(t) = self.task_mut(&id) {
                    t.queued = false;
                }
                self.start(&id, cx);
                if self.task(&id).map(|t| t.status) != Some(Status::Running) {
                    break; // couldn't start (its card says why)
                }
            }
        }
    }

    /// A headless worker's first run, once its worktree exists: the shared preamble, then its task block.
    pub(super) fn start_worker(&mut self, id: &str, cx: &mut Cx) {
        let Some(t) = self.task(id).cloned() else { return };
        let prompt = format!("{}{}", run::WORKER_PREAMBLE, run::task_block(&t.id, &t.title, &t.prompt, &t.owns, &t.reads, &t.acceptance, &t.size, &t.history, git::gate_shell().2));
        self.spawn_worker(id, &prompt, "", cx);
    }

    /// Send a finished worker more to do in the same session.
    pub(super) fn worker_again(&mut self, id: &str, prompt: &str, cx: &mut Cx) {
        let Some(t) = self.task(id).cloned() else { return };
        if let Some(tm) = self.task_mut(id) {
            tm.status = Status::Running;
            tm.followups += 1;
            tm.error.clear();
            tm.finished = 0;
            tm.last = format!("› {}", prompt.lines().next().unwrap_or(""));
        }
        self.spawn_worker(id, prompt, &t.session_id, cx);
    }

    fn spawn_worker(&mut self, id: &str, prompt: &str, resume: &str, cx: &mut Cx) {
        let Some(t) = self.task(id).cloned() else { return };
        let fake = self.fake_worker.clone();
        let Some(bin) = self.installed_or_fake(&t.agent, fake.is_some()) else {
            if let Some(tm) = self.task_mut(id) {
                tm.status = Status::Review;
                tm.error = format!("{} isn't installed", t.agent);
            }
            self.event(id, "failed");
            return;
        };
        let wt = PathBuf::from(&t.worktree);
        let spec = run::worker_spec(&t.agent, &bin, &t.model, prompt, resume, t.max_turns, t.budget_usd, &wt, &self.paths.agents.join("tmp"));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let l = self.live(id);
            l.stop = Some(stop.clone());
            l.cost_base = t.cost_usd;
            l.tokens_base = t.tokens;
            l.since = Some(Instant::now());
            l.last_event = Some(Instant::now());
            l.repeat = (String::new(), 0);
            l.fails.clear();
            l.calls_since_edit = 0;
            upsert(&mut l.log, Entry::note(&if resume.is_empty() { format!("{} started", t.worker) } else { format!("› {}", prompt.lines().next().unwrap_or("")) }));
        }
        if let Some(tm) = self.task_mut(id) {
            tm.status = Status::Running;
            if resume.is_empty() {
                tm.started = store::now();
                // known up front (claude --session-id), so a crash can still resume it
                if !spec.session.is_empty() {
                    tm.session_id = spec.session.clone();
                }
            }
        }
        let id2 = id.to_string();
        self.spawn(cx, move |send| {
            let o = run::run(&spec, &stop, fake.as_ref(), &mut |e| send(Msg::Worker(id2.clone(), e)));
            send(Msg::WorkerDone(id2, o));
        });
    }

    pub(super) fn on_worker_ev(&mut self, id: &str, e: Ev) {
        let (cost_base, tokens_base) = self.live.get(id).map(|l| (l.cost_base, l.tokens_base)).unwrap_or_default();
        self.live(id).last_event = Some(Instant::now());
        match e {
            Ev::Entry(en) => {
                if en.kind == 't' && en.status != "running" {
                    // the watchdog's evidence: repeats, failing commands, calls without edits
                    let line = en.line();
                    let l = self.live(id);
                    if l.repeat.0 == line {
                        l.repeat.1 += 1;
                    } else {
                        l.repeat = (line.clone(), 1);
                    }
                    if en.status == "error" && matches!(en.label.as_str(), "Bash" | "PowerShell" | "Run") {
                        *l.fails.entry(en.target.clone()).or_insert(0) += 1;
                    }
                    if !is_build(&line) {
                        l.calls_since_edit += 1;
                    }
                }
                if let Some(t) = self.task_mut(id) {
                    if en.kind == 't' {
                        t.last = en.line();
                        if en.status != "running" {
                            t.remember(&en.line());
                        }
                    } else if en.kind == 'e' {
                        t.remember(&format!("✗ {}", en.label));
                    }
                }
                upsert(&mut self.live(id).log, en);
            }
            Ev::Touched(f) => {
                self.live(id).calls_since_edit = 0;
                if let Some(t) = self.task_mut(id) {
                    if !t.touched.contains(&f) {
                        t.touched.push(f);
                    }
                }
            }
            Ev::Session(s) => {
                if let Some(t) = self.task_mut(id) {
                    t.session_id = s;
                }
            }
            Ev::Cost(c) => {
                if let Some(t) = self.task_mut(id) {
                    t.cost_usd = cost_base + c;
                }
            }
            Ev::Tokens(n) => {
                if let Some(t) = self.task_mut(id) {
                    t.tokens = tokens_base + n;
                }
            }
            Ev::Limit(v) => {
                let agent = self.task(id).map(|t| t.agent.clone()).unwrap_or_default();
                self.on_limit(&v, &agent);
            }
            Ev::Error(m) => upsert(&mut self.live(id).log, Entry::error(&m)),
            Ev::Report(_) | Ev::Final(_) => {}
        }
    }

    pub(super) fn on_worker_done(&mut self, id: &str, o: Outcome, cx: &mut Cx) {
        let (takeover, discard, nudge) = {
            let l = self.live(id);
            l.stop = None;
            (std::mem::take(&mut l.takeover), l.discard.take(), l.nudge.take())
        };
        let cost_base = self.live.get(id).map(|l| l.cost_base).unwrap_or(0.0);
        let Some(t) = self.task_mut(id) else { return };
        if !o.session.is_empty() {
            t.session_id = o.session.clone();
        }
        if o.cost > 0.0 {
            t.cost_usd = cost_base + o.cost;
        }
        for f in &o.touched {
            if !t.touched.contains(f) {
                t.touched.push(f.clone());
            }
        }
        t.status = Status::Review;
        t.finished = store::now();
        if let Some(reply) = discard {
            self.live(id).reply = Some(reply);
            self.remove(id, Then::Discarded, cx);
            return;
        }
        if takeover {
            self.take_over_now(id, cx);
            return;
        }
        if let Some((reason, fresh)) = nudge {
            if fresh {
                self.redispatch(id, &format!("watchdog: {reason}"), cx);
            } else {
                let p = format!("oriel's watchdog stopped you: you seem {reason}. Step back and try a different approach. If you can't finish within OWNS, stop and report status \"blocked\" with your question.");
                upsert(&mut self.live(id).log, Entry::note(&format!("watchdog: {reason} — nudged")));
                self.worker_again(id, &p, cx);
            }
            return;
        }
        // rate limited: the vendor is paused; put it back in the queue for later instead of failing it
        let limited = o.error.as_deref().is_some_and(|e| {
            let e = e.to_lowercase();
            e.contains("rate limit") || e.contains("usage limit") || e.contains("429")
        });
        let agent = self.task(id).map(|t| t.agent.clone()).unwrap_or_default();
        if limited {
            self.paused.entry(agent).or_insert(store::now() + 15 * 60);
            if let Some(t) = self.task_mut(id) {
                t.status = Status::Todo;
                t.queued = true;
                t.last = "rate limited — queued until its plan resets".into();
            }
            return;
        }
        let t = self.task_mut(id).unwrap();
        if let Some(r) = &o.report {
            t.summary = r["summary"].as_str().unwrap_or("").to_string();
            t.blocked = r["status"].as_str() == Some("blocked");
            t.questions = r["questions"].as_array().map(|q| q.iter().filter_map(|x| x.as_str()).map(String::from).collect()).unwrap_or_default();
        } else if !o.text.trim().is_empty() {
            t.summary = o.text.split_whitespace().take(120).collect::<Vec<_>>().join(" ");
        }
        if o.stopped && o.error.is_none() {
            t.last = "stopped".into();
        } else if let Some(e) = &o.error {
            t.error = e.clone();
            t.last.clear();
        } else {
            t.last = t.summary.lines().next().unwrap_or("finished").chars().take(160).collect();
        }
        let failed = !t.error.is_empty();
        upsert(&mut self.live(id).log, if failed { Entry::error(&format!("finished with an error: {}", o.error.clone().unwrap_or_default())) } else { Entry::note("finished") });
        self.check_task(id, cx);
    }

    /// After a worker finishes: commit a conflict resolution if one was in progress, recount, conflict-check
    /// against the integration branch (background, serialized with merges). The lead hears about it after.
    fn check_task(&mut self, id: &str, cx: &mut Cx) {
        let Some(t) = self.task(id).cloned() else { return };
        let integ = self.run_ref(&t.run).map(|r| r.branch.clone()).unwrap_or_default();
        let resolving = std::mem::take(&mut self.live(id).resolving);
        if t.worktree.is_empty() || integ.is_empty() {
            self.after_check(id, cx);
            return;
        }
        self.live(id).checking = true;
        let lock = self.merge_lock.clone();
        let id2 = id.to_string();
        self.spawn(cx, move |send| {
            let _g = lock.lock().unwrap_or_else(|e| e.into_inner());
            let wt = Path::new(&t.worktree);
            let resolved = if resolving { Some(git::finish_resolve(wt).map(|_| ())) } else { None };
            let stats = git::file_stats(wt, &t.base_sha).ok();
            let conflicts = git::conflicts_with(Path::new(&t.repo), wt, &t.base_sha, &integ);
            send(Msg::Checked(id2, stats, conflicts, resolved));
        });
    }

    pub(super) fn on_checked(&mut self, id: &str, stats: Option<Vec<(String, u64, u64)>>, conflicts: Result<Vec<String>, String>, resolved: Option<Result<(), String>>, cx: &mut Cx) {
        self.live(id).checking = false;
        let Some(t) = self.task_mut(id) else { return };
        if let Some(s) = stats {
            t.added = s.iter().map(|f| f.1).sum();
            t.removed = s.iter().map(|f| f.2).sum();
            t.files = s.len() as u64;
            t.file_stats = s;
        }
        if let Ok(c) = conflicts {
            t.conflicts = c;
        }
        if let Some(Err(e)) = resolved {
            t.error = format!("conflicts not resolved: {e}");
        }
        self.after_check(id, cx);
    }

    /// A worker's round is over and checked: merge it by itself if the lead already asked, else tell the lead.
    fn after_check(&mut self, id: &str, cx: &mut Cx) {
        let Some(t) = self.task(id).cloned() else { return };
        if !t.headless() || t.run.is_empty() {
            return;
        }
        let worker = t.worker.clone();
        if !t.error.is_empty() {
            self.record(&worker).failed += 1;
            if t.want_merge && t.attempts > 0 {
                return self.bounce_failed(id, &t.error.clone(), cx);
            }
            cx.notify(format!("✗ {}: {} failed", t.worker, t.title));
            return self.event(id, "failed");
        }
        if t.blocked {
            if let Some(tm) = self.task_mut(id) {
                tm.want_merge = false;
            }
            cx.notify(format!("⚑ {} is blocked: {}", t.title, t.questions.first().cloned().unwrap_or_default()));
            return self.event(id, "blocked");
        }
        if t.want_merge {
            let _ = self.queue_merge(id);
            return;
        }
        self.event(id, "finished");
    }

    // ---------------------------------------------------------------- the merge queue

    /// Put a finished task in line for the integration branch. Merges run one at a time (pump_merges).
    pub(super) fn queue_merge(&mut self, id: &str) -> Result<String, String> {
        let Some(t) = self.task(id).cloned() else { return Err(format!("no task {id}")) };
        let l = self.live.get(id);
        if matches!(t.status, Status::Running | Status::Blocked) || l.is_some_and(|l| l.stop.is_some()) {
            return Err(format!("{id} is still running — wait for it first"));
        }
        if t.status != Status::Review || t.worktree.is_empty() {
            return Err(format!("{id} can't be merged: it's {}", state_name(&t)));
        }
        if let Some(tm) = self.task_mut(id) {
            tm.want_merge = true;
            tm.blocked = false;
        }
        if !self.merge_queue.contains(&id.to_string()) && self.merging.as_deref() != Some(id) {
            self.merge_queue.push_back(id.to_string());
        }
        let pos = self.merge_queue.iter().position(|x| x == id).map(|p| p + 1 + self.merging.is_some() as usize).unwrap_or(0);
        if let Some(tm) = self.task_mut(id) {
            tm.last = if pos <= 1 { "merging…".into() } else { format!("waiting to merge (#{pos})") };
        }
        Ok(format!("{id} is queued for merging (position {pos}). oriel checks conflicts and runs the gate on the merged result; wait tells you when it's merged or bounced"))
    }

    /// Start the next merge when none is running.
    pub(super) fn pump_merges(&mut self, cx: &mut Cx) {
        while self.merging.is_none() {
            let Some(id) = self.merge_queue.pop_front() else { return };
            let Some(t) = self.task(&id).cloned() else { continue };
            let busy = self.live.get(&id).is_some_and(|l| l.stop.is_some() || l.busy || l.checking);
            if t.status != Status::Review || t.worktree.is_empty() || busy {
                continue; // it went back to work; it queues again when it's done
            }
            let Some(run) = self.run_ref(&t.run).cloned() else { continue };
            self.merging = Some(id.clone());
            self.live(&id).busy = true;
            if let Some(tm) = self.task_mut(&id) {
                tm.last = format!("merging into {}…", run.branch);
            }
            let lock = self.merge_lock.clone();
            let gate_cmd = self.lead_cfg.gate.trim().to_string();
            let timeout = Duration::from_secs(self.lead_cfg.gate_timeout_s.max(30) as u64);
            let gate_wt = PathBuf::from(format!("{}-gate", run.worktree.trim_end_matches(['/', '\\'])));
            self.spawn(cx, move |send| {
                let _g = lock.lock().unwrap_or_else(|e| e.into_inner());
                let repo = Path::new(&t.repo);
                let gate = |sha: &str| -> Result<(), String> {
                    let base = match gate_cmd.as_str() {
                        "none" | "off" => None,
                        "" => None, // detected after checkout
                        c => Some(c.to_string()),
                    };
                    let mut cmds: Vec<String> = vec![];
                    if gate_cmd.is_empty() || base.is_some() || !t.acceptance.is_empty() {
                        git::gate_checkout(repo, &gate_wt, sha)?;
                        if let Some(c) = base.or_else(|| if gate_cmd.is_empty() { git::detect_gate(&gate_wt) } else { None }) {
                            cmds.push(c);
                        }
                        if !t.acceptance.is_empty() {
                            cmds.push(t.acceptance.clone());
                        }
                    }
                    let n = cmds.len();
                    for (i, c) in cmds.into_iter().enumerate() {
                        // the acceptance command goes last; its failures are marked so a broken command isn't
                        // blamed on the worker
                        let acceptance = !t.acceptance.is_empty() && i + 1 == n;
                        git::run_gate(&gate_wt, &c, timeout, &[]).map_err(|e| if acceptance { format!("acceptance {e}") } else { e })?;
                    }
                    Ok(())
                };
                let lead = PathBuf::from(&run.worktree);
                let job = git::MergeJob { repo, wt: Path::new(&t.worktree), branch: &t.branch, integration: &run.branch, title: &t.title, worker: &t.worker, task_id: &t.id, lead_wt: Some(&lead) };
                let r = git::merge_into(&job, &gate);
                send(Msg::RunMerged(id.clone(), r));
            });
        }
    }

    pub(super) fn on_run_merged(&mut self, id: &str, r: Result<String, git::MergeErr>, cx: &mut Cx) {
        self.live(id).busy = false;
        if self.merging.as_deref() == Some(id) {
            self.merging = None;
        }
        let Some(t) = self.task(id).cloned() else { return };
        match r {
            Ok(summary) => {
                {
                    let rec = self.record(&t.worker);
                    rec.tasks += 1;
                    rec.merged += 1;
                    if t.attempts == 0 && t.redispatches == 0 {
                        rec.first_try += 1;
                    }
                    rec.tokens += t.tokens;
                    rec.cost_usd += t.cost_usd;
                    rec.secs += (store::now() - t.started).max(0);
                }
                let tm = self.task_mut(id).unwrap();
                tm.status = Status::Done;
                tm.outcome = "merged".into();
                tm.finished = store::now();
                tm.last = summary.clone();
                tm.conflicts.clear();
                tm.worktree.clear();
                tm.branch.clear();
                tm.want_merge = false;
                if tm.gate.is_empty() || tm.gate != "pass" {
                    tm.gate = "pass".into();
                }
                let _ = std::fs::remove_file(self.paths.status(id));
                if let Some(r) = self.run_mut(&t.run) {
                    r.merged += 1;
                }
                cx.notify(format!("✓ {} · {summary}", t.title));
                self.event(id, "merged");
                self.schedule(cx);
            }
            Err(git::MergeErr::Conflicts(files)) => {
                self.record(&t.worker).conflicts += 1;
                if let Some(tm) = self.task_mut(id) {
                    tm.conflicts = files.clone();
                    tm.last = format!("conflicts with the integration branch in {}", files.join(", "));
                }
                self.bounce(id, &format!("merge conflict in {}", files.join(", ")), true, cx);
            }
            Err(git::MergeErr::Gate(msg)) => {
                let acceptance = msg.starts_with("acceptance ");
                let msg = msg.trim_start_matches("acceptance ").to_string();
                if let Some(tm) = self.task_mut(id) {
                    tm.gate = msg.clone();
                    tm.last = format!("gate failed: {}", msg.lines().next().unwrap_or(""));
                }
                if acceptance && git::shell_trouble(&msg) {
                    // the lead's acceptance command didn't even run: nothing for the worker to fix
                    if let Some(tm) = self.task_mut(id) {
                        tm.want_merge = false;
                        tm.last = "the acceptance command itself failed to run".into();
                    }
                    self.event(id, "acceptance_broken");
                    return;
                }
                self.record(&t.worker).gate_fails += 1;
                self.bounce(id, &msg, false, cx);
            }
            Err(git::MergeErr::Failed(e)) => {
                if let Some(tm) = self.task_mut(id) {
                    tm.error = e.clone();
                    tm.last.clear();
                    tm.want_merge = false;
                }
                cx.notify(format!("merge: {e}"));
                self.event(id, "failed");
            }
        }
    }

    /// A merge bounced (conflict or gate): back to the same worker, twice; then a fresh worker.
    fn bounce(&mut self, id: &str, why: &str, conflict: bool, cx: &mut Cx) {
        let Some(t) = self.task(id).cloned() else { return };
        if t.attempts >= MAX_ATTEMPTS {
            return self.redispatch(id, why, cx);
        }
        self.record(&t.worker).retries += 1;
        if let Some(tm) = self.task_mut(id) {
            tm.attempts += 1;
            tm.want_merge = true;
        }
        if self.check_budget(&t.run, cx).is_err() {
            return self.bounce_failed(id, "the run's budget is spent", cx);
        }
        if conflict {
            self.start_resolve(id, None, cx);
        } else {
            let p = format!(
                "oriel tried to merge your work, but the gate failed on the merged result (your changes plus everything already merged):\n{why}\n\nFix it within OWNS and finish with your report; oriel merges it again when you're done (attempt {} of {MAX_ATTEMPTS}). If the failure isn't caused by your code (e.g. the check itself is wrong), change nothing and report status \"blocked\" saying why.",
                t.attempts + 1
            );
            self.worker_again(id, &p, cx);
        }
        // told after the fix started, so the card says what's actually happening
        self.event(id, "bounced");
    }

    fn bounce_failed(&mut self, id: &str, why: &str, cx: &mut Cx) {
        if let Some(tm) = self.task_mut(id) {
            tm.want_merge = false;
            tm.blocked = true;
            tm.questions = vec![format!("merge failed: {why}")];
        }
        self.event(id, "blocked");
        cx.notify(format!("⚑ a task needs attention: {why}"));
    }

    /// Start a task over with a fresh worker (a premium one, if the roster has another), carrying notes about
    /// what went wrong. The second time, it's blocked for the lead or the user to decide.
    fn redispatch(&mut self, id: &str, why: &str, cx: &mut Cx) {
        let Some(t) = self.task(id).cloned() else { return };
        if t.redispatches >= 1 {
            self.record(&t.worker).failed += 1;
            if let Some(tm) = self.task_mut(id) {
                tm.error = format!("gave up after {} attempts: {}", t.attempts + t.redispatches + 1, why.lines().next().unwrap_or(""));
                tm.want_merge = false;
                tm.blocked = true;
            }
            cx.notify(format!("⚑ {} is blocked after several attempts", t.title));
            return self.event(id, "blocked");
        }
        let roster = self.roster();
        let usable = |w: &&crate::config::RosterEntry| w.enabled && (self.fake_worker.is_some() || self.installed(&w.agent).is_some()) && !self.paused.contains_key(&w.agent);
        let next = roster
            .iter()
            .filter(usable)
            .filter(|w| w.tier == "premium" && w.name != t.worker)
            .next()
            .or_else(|| roster.iter().filter(usable).find(|w| w.agent != t.agent))
            .cloned()
            .unwrap_or_else(|| roster.iter().find(|w| w.name == t.worker).cloned().unwrap_or_default());
        let note = format!("{} ({}) tried and failed: {}", t.worker, t.agent, crate::ui::fit(why.lines().next().unwrap_or(""), 200));
        if let Some(tm) = self.task_mut(id) {
            tm.history.push(note);
            tm.last = format!("starting over with {}", next.name);
        }
        self.live(id).busy = true;
        let id2 = id.to_string();
        let (repo, wt, branch) = (t.repo.clone(), t.worktree.clone(), t.branch.clone());
        let pick = next.clone();
        self.spawn(cx, move |send| {
            std::thread::sleep(Duration::from_millis(200));
            let r = if wt.is_empty() { Ok(()) } else { git::remove(Path::new(&repo), Path::new(&wt), &branch) };
            send(Msg::Redispatched(id2, r, pick));
        });
    }

    pub(super) fn on_redispatched(&mut self, id: &str, r: Result<(), String>, w: crate::config::RosterEntry, cx: &mut Cx) {
        self.live(id).busy = false;
        let Some(t) = self.task_mut(id) else { return };
        if let Err(e) = r {
            t.error = e;
            return;
        }
        t.redispatches += 1;
        t.attempts = 0;
        t.status = Status::Todo;
        t.queued = true;
        t.worktree.clear();
        t.branch.clear();
        t.base_sha.clear();
        t.session_id.clear();
        t.want_merge = false;
        t.error.clear();
        t.blocked = false;
        t.questions.clear();
        t.gate.clear();
        t.conflicts.clear();
        t.summary.clear();
        t.file_stats.clear();
        t.touched = t.owns.clone();
        t.worker = w.name;
        t.agent = w.agent;
        t.model = w.model;
        t.tier = w.tier;
        t.max_turns = w.max_turns;
        t.budget_usd = w.budget_usd;
        self.event(id, "redispatched");
        self.schedule(cx);
    }

    /// Discard a task: stop its worker first if it's running. `reply` answers the lead once it's gone.
    pub(super) fn discard_task(&mut self, id: &str, reply: Option<Reply>, cx: &mut Cx) {
        if self.merging.as_deref() == Some(id) || self.live.get(id).is_some_and(|l| l.busy || l.checking) {
            let msg = format!("{id} is in the middle of a git step (merging or checking) — try again in a moment");
            match reply {
                Some(r) => {
                    let _ = r.send(Err(msg));
                }
                None => cx.notify(msg),
            }
            return;
        }
        self.merge_queue.retain(|x| x != id);
        if let Some(stop) = self.live.get(id).and_then(|l| l.stop.clone()) {
            stop.store(true, Ordering::SeqCst);
            let r = reply.unwrap_or_else(|| mpsc::channel().0);
            self.live(id).discard = Some(r);
            return;
        }
        if self.task(id).is_some_and(|t| t.status == Status::Done) {
            if let Some(r) = reply {
                let _ = r.send(Err(format!("{id} is already {}", self.task(id).map(state_name).unwrap_or(""))));
            }
            return;
        }
        if let Some(t) = self.task_mut(id) {
            t.queued = false;
            t.want_merge = false;
        }
        self.live(id).reply = reply;
        self.remove(id, Then::Discarded, cx);
    }

    // ---------------------------------------------------------------- the watchdog

    /// Stuck (the same call 4×, the same failing command 3×), spinning (25 calls without an edit), hung (no
    /// output for 5 minutes): first a nudge in the same session, then a fresh worker, then blocked.
    pub(super) fn watchdog(&mut self) {
        let ids: Vec<String> = self.store.tasks.iter().filter(|t| t.headless() && t.status == Status::Running).map(|t| t.id.clone()).collect();
        for id in ids {
            let hung = self.hung_after;
            let Some(l) = self.live.get_mut(&id) else { continue };
            let Some(stop) = l.stop.clone() else { continue };
            if l.nudge.is_some() {
                continue;
            }
            let reason = if l.last_event.is_some_and(|t| t.elapsed() > hung) {
                Some(format!("hung (no output for {} minutes)", hung.as_secs() / 60))
            } else if l.repeat.1 >= 4 {
                Some(format!("stuck (repeating \"{}\")", crate::ui::fit(&l.repeat.0, 60)))
            } else if let Some((cmd, _)) = l.fails.iter().find(|(_, n)| **n >= 3) {
                Some(format!("stuck (\"{}\" failed 3 times)", crate::ui::fit(cmd, 60)))
            } else if l.calls_since_edit >= 25 {
                Some("spinning (25 tool calls without editing anything)".to_string())
            } else {
                None
            };
            if let Some(r) = reason {
                let fresh = l.nudged >= 1;
                l.nudged += 1;
                l.nudge = Some((r, fresh));
                stop.store(true, Ordering::SeqCst);
            }
        }
    }

    // ---------------------------------------------------------------- the user's controls

    /// Stop the lead and every worker of the run. The integration branch and finished work stay for review.
    pub(super) fn stop_run(&mut self, id: &str, cx: &mut Cx) {
        if let Some(l) = self.runs_live.get_mut(id) {
            l.stop.store(true, Ordering::SeqCst);
            l.broker = None;
        }
        let ids: Vec<String> = self.run_tasks(id).iter().map(|t| t.id.clone()).collect();
        for tid in ids {
            if let Some(stop) = self.live.get(&tid).and_then(|l| l.stop.clone()) {
                stop.store(true, Ordering::SeqCst);
            }
            self.merge_queue.retain(|x| *x != tid);
            if let Some(t) = self.task_mut(&tid) {
                t.want_merge = false;
                if t.status == Status::Todo && t.queued {
                    t.queued = false;
                    t.last = "not started (the run was stopped)".into();
                }
            }
        }
        self.answer_waiters(id, "the run was stopped by the user");
        if let Some(r) = self.run_mut(id) {
            if r.state.active() {
                r.state = RunState::Stopped;
                r.finished = store::now();
                r.log("stopped by you");
                cx.notify("⚑ lead run stopped");
            }
        }
        self.save();
    }

    /// The run's review diff: the integration branch against where it started.
    pub(super) fn open_run_diff(&mut self, id: &str, cx: &Cx) {
        let Some(r) = self.run_ref(id).cloned() else { return };
        if r.branch.is_empty() {
            return;
        }
        self.mode = super::Mode::Diff(super::DiffView { id: id.to_string(), data: None, file: 0, scroll: 0 });
        let id = id.to_string();
        self.spawn(cx, move |send| send(Msg::Diff(id, git::branch_diff(Path::new(&r.repo), &r.base_sha, &r.branch))));
    }

    /// Merge the whole run into the user's branch (same rules as a task: clean tree, same branch).
    pub(super) fn merge_run(&mut self, id: &str, cx: &mut Cx) {
        let Some(r) = self.run_ref(id).cloned() else { return };
        let running = self.run_tasks(id).iter().filter(|t| matches!(t.status, Status::Running | Status::Blocked)).count();
        if r.state.active() || running > 0 || self.merging.as_ref().is_some_and(|m| self.task(m).is_some_and(|t| t.run == id)) {
            cx.notify("the run is still working — stop it (s) or let it finish before merging");
            return;
        }
        if r.branch.is_empty() || r.state == RunState::Merged {
            return;
        }
        if let Some(x) = self.run_mut(id) {
            x.log("merging into your branch…");
        }
        let id2 = id.to_string();
        self.spawn(cx, move |send| {
            let repo = Path::new(&r.repo);
            let _ = git::remove(repo, Path::new(&format!("{}-gate", r.worktree.trim_end_matches(['/', '\\']))), "");
            let title: String = r.goal.lines().next().unwrap_or("lead run").chars().take(72).collect();
            let res = git::merge(repo, Path::new(""), &r.branch, &r.base_branch, &title, &|_| {});
            if res.is_ok() {
                let _ = git::remove(repo, Path::new(&r.worktree), "");
            }
            send(Msg::RunFinal(id2, res));
        });
    }

    pub(super) fn on_run_final(&mut self, id: &str, r: Result<String, String>, cx: &mut Cx) {
        let Some(run) = self.run_mut(id) else { return };
        match r {
            Ok(s) => {
                run.state = RunState::Merged;
                run.finished = store::now();
                run.worktree.clear();
                run.log(&s);
                cx.notify(format!("⚑ lead run {s}"));
                if matches!(self.mode, super::Mode::Diff(ref v) if v.id == id) {
                    self.mode = super::Mode::Board;
                }
                self.lead_focus = false;
            }
            Err(e) => {
                run.error = e.clone();
                run.log(&format!("merge failed: {e}"));
                cx.notify(format!("merge failed: {e}"));
            }
        }
    }

    /// Throw the whole run away: its workers' worktrees, the lead's checkout and the integration branch.
    pub(super) fn discard_run(&mut self, id: &str, cx: &mut Cx) {
        self.stop_run(id, cx);
        let Some(r) = self.run_ref(id).cloned() else { return };
        let tasks: Vec<Task> = self.run_tasks(id).into_iter().filter(|t| t.status != Status::Done).cloned().collect();
        let id2 = id.to_string();
        self.spawn(cx, move |send| {
            let repo = Path::new(&r.repo);
            std::thread::sleep(Duration::from_millis(300)); // let stopped workers let go of their folders
            let mut err = Ok(());
            for t in &tasks {
                if !t.worktree.is_empty() || !t.branch.is_empty() {
                    if let Err(e) = git::remove(repo, Path::new(&t.worktree), &t.branch) {
                        err = Err(e);
                    }
                }
            }
            let _ = git::remove(repo, Path::new(&format!("{}-gate", r.worktree.trim_end_matches(['/', '\\']))), "");
            if let Err(e) = git::remove(repo, Path::new(&r.worktree), &r.branch) {
                err = Err(e);
            }
            send(Msg::RunCleaned(id2, err));
        });
    }

    pub(super) fn on_run_cleaned(&mut self, id: &str, r: Result<(), String>, cx: &mut Cx) {
        let ids: Vec<String> = self.run_tasks(id).iter().filter(|t| t.status != Status::Done).map(|t| t.id.clone()).collect();
        for tid in ids {
            if let Some(t) = self.task_mut(&tid) {
                t.status = Status::Done;
                t.outcome = "discarded".into();
                t.finished = store::now();
                t.last = "discarded with the run".into();
                t.worktree.clear();
                t.branch.clear();
                t.queued = false;
            }
        }
        if let Some(run) = self.run_mut(id) {
            run.state = RunState::Discarded;
            run.worktree.clear();
            if let Err(e) = r {
                run.error = e.clone();
                cx.notify(format!("discarded, but: {e}"));
            } else {
                cx.notify("⚑ lead run discarded");
            }
        }
        self.lead_focus = false;
    }

    /// `t` on a headless task: continue its session in a real terminal tab (the card follows the tab).
    pub(super) fn take_over(&mut self, id: &str, cx: &mut Cx) {
        let Some(t) = self.task(id).cloned() else { return };
        if !t.headless() || t.worktree.is_empty() {
            return;
        }
        if self.fake_worker.is_none() && self.installed(&t.agent).is_none() {
            cx.notify(format!("{} isn't installed", t.agent));
            return;
        }
        self.merge_queue.retain(|x| x != id);
        if let Some(tm) = self.task_mut(id) {
            tm.want_merge = false;
        }
        if let Some(stop) = self.live.get(id).and_then(|l| l.stop.clone()) {
            self.live(id).takeover = true;
            stop.store(true, Ordering::SeqCst);
            if let Some(tm) = self.task_mut(id) {
                tm.last = "handing over to a terminal tab…".into();
            }
            return;
        }
        self.take_over_now(id, cx);
    }

    fn take_over_now(&mut self, id: &str, cx: &mut Cx) {
        let Some(t) = self.task(id).cloned() else { return };
        // Claude reports through hooks in the tab; write them into the worktree first (background)
        let hook = if t.agent == "claude" { self.hook_exe.clone() } else { None };
        let id2 = id.to_string();
        self.spawn(cx, move |send| {
            if let Some(exe) = hook {
                let _ = git::add_hooks(Path::new(&t.worktree), &t.id, &exe);
            }
            send(Msg::TookOver(id2));
        });
    }

    pub(super) fn on_took_over(&mut self, id: &str, cx: &mut Cx) {
        let Some(t) = self.task_mut(id) else { return };
        t.mode = "interactive".into();
        t.status = Status::Running;
        t.error.clear();
        t.last = "taken over in a terminal tab".into();
        let t = t.clone();
        let args = run::takeover_args(&t.agent, &t.model, &t.session_id);
        self.open_agent_with(&t, args, cx);
        let _ = std::fs::remove_file(self.paths.status(id));
        cx.act(crate::pane::Action::FocusTag(t.tag()));
        self.select(id);
    }

    /// Runs that were going when oriel closed can't reattach to their processes: mark them stopped (and their
    /// workers), ready to be resumed with `r`.
    pub(super) fn settle_after_restart(&mut self) {
        for r in &mut self.store.runs {
            if r.state.active() {
                r.state = RunState::Stopped;
                r.error = "oriel was closed while it ran — r resumes it".into();
            }
        }
        for t in &mut self.store.tasks {
            if t.headless() && matches!(t.status, Status::Running | Status::Blocked) {
                t.status = Status::Review;
                t.last = "stopped: oriel was closed".into();
            }
        }
    }

    /// Save the lead choice + roster to the config file (never under `cargo test`).
    pub(super) fn save_config(&self) {
        if cfg!(test) {
            return; // tests never read or write the real config
        }
        let mut c = crate::config::load();
        c.lead = self.lead_cfg.clone();
        if !self.roster.is_empty() {
            c.roster = self.roster.clone();
        }
        crate::config::save(&c);
    }
}
