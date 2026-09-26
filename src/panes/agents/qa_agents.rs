//! QA pass on the agents engine: runs of your own tasks (batch.rs) pushed through the awkward paths — a task
//! discarded mid-run, a blocked task answered with a comment, oriel restarted mid-run, a run stopped and
//! resumed by hand, marks on deleted tasks,
//! priorities against dependencies, budgets, two runs at once, merging a run that merged nothing — plus the
//! board's forms and priority keys.
//!
//! Everything is headless: fake workers (below) drive the real board, git worktrees, merge queue and run
//! branches in throwaway repos under target/test-scratch/qa-agents. No real agent, network or clipboard.
//!
//! The fake worker reads a tiny script from its task's prompt, one command per line:
//!   `write FILE: CONTENT`  write a file in its worktree (every life)
//!   `cost 0.10`            what the run costs
//!   `block QUESTION`       first life only: report status "blocked" with that question, write nothing
//!   `hold FLAG`            first life only: wait until target/.../flags/FLAG exists (or it's stopped)
//!   `pre FILE: CONTENT`    first life only: write a file right away (a half-done edit, before a hold)
//! A follow-up (a comment, a restart) resumes the same session and runs the writes of the original script.

use super::stream::Ev;
use super::tests::{noop_agent, sh, temp_repo, until};
use super::*;
use crate::config::RosterEntry;
use crate::testkit::Kit;
use crossterm::event::KeyCode;
use serde_json::json;
use std::sync::atomic::AtomicUsize;

fn scratch(name: &str) -> PathBuf {
    let nanos = SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-scratch").join("qa-agents").join(format!("{name}-{nanos}"));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn snap(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-scratch").join("qa-agents").join("snap").join(format!("{name}.html")).display().to_string()
}

#[derive(Clone, Debug)]
struct Call {
    task: String,
    prompt: String,
    resume: String,
}

/// Fake workers sharing one memory (sessions survive a "restart": a new pane with a new fake).
#[derive(Clone)]
struct Workers {
    calls: Arc<Mutex<Vec<Call>>>,
    scripts: Arc<Mutex<HashMap<String, (String, String)>>>,
    flags: PathBuf,
    n: Arc<AtomicUsize>,
}

impl Workers {
    fn new(dir: &Path) -> Workers {
        let flags = dir.join("flags");
        std::fs::create_dir_all(&flags).unwrap();
        Workers { calls: Arc::default(), scripts: Arc::default(), flags, n: Arc::default() }
    }

    fn release(&self, flag: &str) {
        std::fs::write(self.flags.join(flag), "go").unwrap();
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    /// Index of the first call for this task (None = it never started).
    fn first(&self, task: &str) -> Option<usize> {
        self.calls().iter().position(|c| c.task == task)
    }

    fn fake(&self) -> run::Fake {
        let (calls, scripts, flags, n) = (self.calls.clone(), self.scripts.clone(), self.flags.clone(), self.n.clone());
        Arc::new(move |spec: &run::Spec, stop: &AtomicBool, on: &mut dyn FnMut(Ev)| -> run::Outcome {
            let first_life = spec.resume.is_empty();
            let session = if !first_life {
                spec.resume.clone()
            } else if !spec.session.is_empty() {
                spec.session.clone()
            } else {
                format!("qa-{}", n.fetch_add(1, Ordering::SeqCst))
            };
            let (task, script) = if first_life {
                let task = spec.prompt.lines().find_map(|l| l.strip_prefix("TASK ")).and_then(|l| l.split(':').next()).unwrap_or("").to_string();
                scripts.lock().unwrap().insert(session.clone(), (task.clone(), spec.prompt.clone()));
                (task, spec.prompt.clone())
            } else {
                scripts.lock().unwrap().get(&session).cloned().unwrap_or_default()
            };
            calls.lock().unwrap().push(Call { task: task.clone(), prompt: spec.prompt.clone(), resume: spec.resume.clone() });
            on(Ev::Session(session.clone()));
            let mut cost = 0.05;
            let mut blocked: Option<String> = None;
            for line in script.lines() {
                if let Some(q) = line.strip_prefix("block ") {
                    if first_life {
                        blocked = Some(q.trim().to_string());
                    }
                } else if let Some(rest) = line.strip_prefix("pre ") {
                    // first life only: a half-done edit made before it holds (what a stopped agent leaves behind)
                    if first_life {
                        let (file, content) = rest.split_once(": ").unwrap_or((rest, ""));
                        std::fs::write(spec.cwd.join(file.trim()), content.to_string() + "\n").unwrap();
                        on(Ev::Touched(file.trim().to_string()));
                    }
                } else if let Some(f) = line.strip_prefix("hold ") {
                    if first_life {
                        let flag = flags.join(f.trim());
                        let t0 = Instant::now();
                        while !flag.exists() && !stop.load(Ordering::SeqCst) && t0.elapsed() < Duration::from_secs(60) {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        if !flag.exists() {
                            return run::Outcome { session, cost: 0.01, stopped: true, ..Default::default() };
                        }
                    }
                } else if let Some(c) = line.strip_prefix("cost ") {
                    cost = c.trim().parse().unwrap_or(cost);
                }
            }
            on(Ev::Cost(cost));
            if let Some(q) = blocked {
                let report = json!({"status": "blocked", "summary": "needs a decision", "questions": [q]});
                on(Ev::Report(report.clone()));
                return run::Outcome { session, cost, text: "blocked".into(), report: Some(report), ..Default::default() };
            }
            for line in script.lines() {
                if let Some(rest) = line.strip_prefix("write ") {
                    let (file, content) = rest.split_once(": ").unwrap_or((rest, ""));
                    let path = spec.cwd.join(file.trim());
                    if let Some(d) = path.parent() {
                        std::fs::create_dir_all(d).unwrap();
                    }
                    std::fs::write(&path, content.replace("\\n", "\n") + "\n").unwrap();
                    on(Ev::Touched(file.trim().to_string()));
                }
            }
            let summary = if first_life { format!("did {task}") } else { format!("did {task} after: {}", spec.prompt.lines().next().unwrap_or("")) };
            let report = json!({"status": "done", "summary": summary, "questions": []});
            on(Ev::Report(report.clone()));
            run::Outcome { session, cost, text: summary, report: Some(report), ..Default::default() }
        })
    }
}

/// A lead that never does anything until it's stopped (the lead-form tests only need the run to exist).
fn idle_lead() -> run::Fake {
    Arc::new(|_spec: &run::Spec, stop: &AtomicBool, on: &mut dyn FnMut(Ev)| -> run::Outcome {
        on(Ev::Session("qa-lead".into()));
        let t0 = Instant::now();
        while !stop.load(Ordering::SeqCst) && t0.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(10));
        }
        run::Outcome { session: "qa-lead".into(), stopped: true, ..Default::default() }
    })
}

fn roster() -> Vec<RosterEntry> {
    let w = |name: &str, agent: &str, model: &str, tier: &str| RosterEntry { name: name.into(), agent: agent.into(), model: model.into(), tier: tier.into(), good_at: format!("{tier} work"), max_turns: 20, budget_usd: 1.0, enabled: true };
    vec![w("w1", "claude", "haiku", "cheap"), w("w2", "codex", "", "mid"), w("w3", "claude", "sonnet", "premium")]
}

fn pane(dir: &Path, repo: &Path, w: &Workers) -> Agents {
    let mut p = Agents::with_paths(Paths { agents: dir.join("agents"), wt: dir.join("wt") });
    p.start_dir = Some(repo.to_path_buf());
    p.fake_agent = Some(noop_agent());
    p.fake_worker = Some(w.fake());
    p.fake_lead = Some(idle_lead());
    p.stagger = Duration::ZERO;
    p.roster = roster();
    p.lead_cfg.agent = "claude".into();
    p.lead_cfg.protocol = "text".into();
    p.lead_cfg.gate = "none".into();
    p.lead_cfg.max_parallel = 2;
    p
}

fn boot(k: &mut Kit, p: &mut Agents) {
    k.render(p, 150, 44);
    until(k, p, 5000, "repo detected", |p| p.repo.is_some());
}

/// Mark these TODO cards with space (in this order), enter, then p (together) or s (one after another).
fn mark_and_run(k: &mut Kit, p: &mut Agents, ids: &[String], serial: bool) -> String {
    p.lead_focus = false;
    for id in ids {
        p.select(id);
        k.key(p, KeyCode::Char(' '));
    }
    assert_eq!(&p.marked, ids, "marked in order");
    let before = p.store.runs.len();
    k.key(p, KeyCode::Enter);
    assert!(matches!(p.mode, Mode::Batch), "enter with marks opens the run popup");
    k.key(p, KeyCode::Char(if serial { 's' } else { 'p' }));
    assert_eq!(p.store.runs.len(), before + 1, "a run started: {:?}", k.notices());
    p.store.runs.last().unwrap().id.clone()
}

fn run_of(p: &Agents, id: &str) -> store::Run {
    p.run_ref(id).cloned().unwrap_or_else(|| panic!("no run {id}"))
}

fn task_of(p: &Agents, id: &str) -> Task {
    p.task(id).cloned().unwrap_or_else(|| panic!("no task {id}"))
}

fn merged(t: &Task) -> bool {
    t.status == Status::Done && t.outcome == "merged"
}

/// x on a card, y to confirm (the lead panel must not have the keys: there x discards the whole run).
fn discard_card(k: &mut Kit, p: &mut Agents, id: &str) {
    p.lead_focus = false;
    p.select(id);
    assert_eq!(p.selected().as_deref(), Some(id));
    k.key(p, KeyCode::Char('x'));
    assert!(matches!(p.mode, Mode::Confirm(_)), "x asks first");
    k.key(p, KeyCode::Char('y'));
}

/// The task's worker process is really going (its status flips to Running before the worktree even exists,
/// and x/c are ignored while that git step runs).
fn working(p: &Agents, id: &str) -> bool {
    p.task(id).is_some_and(|t| t.status == Status::Running) && p.live.get(id).is_some_and(|l| l.stop.is_some() && !l.busy)
}

fn worktrees(repo: &Path) -> usize {
    sh(repo, &["worktree", "list", "--porcelain"]).lines().filter(|l| l.starts_with("worktree ")).count()
}

// ------------------------------------------------------------------ discarding mid-run

/// Two tasks together: one merges, you discard the other while it's still working. Every task is now settled
/// (merged or discarded), so the run should become ready for review — like it does when the last one merges.
#[test]
#[ignore = "fails: discarding the last open task never calls batch_check; the run stays Running"]
fn qa_batch_discarding_the_last_open_task_settles_the_run() {
    let dir = scratch("discard-last");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    boot(&mut k, &mut p);
    let a = p.add_task("Quick one", "write q.txt: quick", 0, "");
    let b = p.add_task("Slow one", "hold never\nwrite s.txt: slow", 0, "");
    let run = mark_and_run(&mut k, &mut p, &[a.clone(), b.clone()], false);
    until(&mut k, &mut p, 30_000, "a merged while b works", |p| merged(&task_of(p, &a)) && working(p, &b));
    discard_card(&mut k, &mut p, &b);
    until(&mut k, &mut p, 15_000, "b discarded", |p| task_of(p, &b).status == Status::Done);
    assert_eq!(task_of(&p, &b).outcome, "discarded");
    assert!(task_of(&p, &b).worktree.is_empty(), "its worktree is gone");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && run_of(&p, &run).state.active() {
        k.poll(&mut p);
        std::thread::sleep(Duration::from_millis(20));
    }
    let r = run_of(&p, &run);
    assert_eq!(r.state, store::RunState::Review, "every task is merged or discarded, but the run still says {:?}; log: {:?}", r.state, r.log);
    let _ = std::fs::remove_dir_all(&dir);
}

/// One after another: you discard the first while it works. The second waited for the first to be *merged*,
/// which can now never happen — it should start (or the run should settle), not wait forever.
#[test]
#[ignore = "fails: a discarded dependency never counts as merged; the rest of the run waits forever"]
fn qa_serial_batch_discarding_the_first_task_does_not_strand_the_rest() {
    let dir = scratch("discard-serial");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    boot(&mut k, &mut p);
    let a = p.add_task("First", "hold never\nwrite f.txt: first", 0, "");
    let b = p.add_task("Second", "write g.txt: second", 0, "");
    let run = mark_and_run(&mut k, &mut p, &[a.clone(), b.clone()], true);
    until(&mut k, &mut p, 30_000, "the first is working", |p| working(p, &a));
    assert_eq!(task_of(&p, &b).status, Status::Todo, "the second waits its turn");
    discard_card(&mut k, &mut p, &a);
    until(&mut k, &mut p, 15_000, "the first is discarded", |p| task_of(p, &a).status == Status::Done);
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline && task_of(&p, &b).status == Status::Todo && run_of(&p, &run).state.active() {
        k.poll(&mut p);
        std::thread::sleep(Duration::from_millis(20));
    }
    let (tb, r) = (task_of(&p, &b), run_of(&p, &run));
    assert!(tb.status != Status::Todo || !r.state.active(), "the second task still waits (queued={}, after {:?}) for a task that was discarded, and the run is {:?} forever", tb.queued, tb.depends_on, r.state);
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ blocked, answered with a comment

/// One after another: the first asks a question (blocked), the second waits. `c` answers it; the same session
/// continues, finishes, merges, and the second runs after it.
#[test]
fn qa_batch_blocked_task_answered_with_a_comment() {
    let dir = scratch("blocked");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    boot(&mut k, &mut p);
    let a = p.add_task("Pick a colour", "block Which colour, red or blue?\nwrite colour.txt: red", 0, "");
    let b = p.add_task("After colour", "write after.txt: done", 0, "");
    let run = mark_and_run(&mut k, &mut p, &[a.clone(), b.clone()], true);
    // blocked is set when the worker ends; the alert comes after the post-run check (background)
    until(&mut k, &mut p, 30_000, "a is blocked and you're told", |p| task_of(p, &a).blocked && p.live.get(&a).is_some_and(|l| !l.checking && l.stop.is_none()));
    let ta = task_of(&p, &a);
    assert_eq!(ta.status, Status::Review);
    assert_eq!(ta.questions, vec!["Which colour, red or blue?".to_string()]);
    assert!(k.notices().iter().any(|n| n.contains("is blocked") && n.contains("c answers it")), "{:?}", k.notices());
    assert_eq!(task_of(&p, &b).status, Status::Todo, "b waits for a");
    assert!(run_of(&p, &run).state.active());
    p.lead_focus = false;
    p.select(&a);
    let board = k.render_html(&mut p, 150, 44, &snap("batch-blocked"));
    assert!(board.contains("? Which colour"), "the card shows the question:\n{board}");
    let session = ta.session_id.clone();
    k.key(&mut p, KeyCode::Char('c'));
    assert!(matches!(p.mode, Mode::Comment(..)), "c opens the answer box");
    k.typ(&mut p, "red please");
    k.key(&mut p, KeyCode::Enter);
    assert_eq!(task_of(&p, &a).status, Status::Running, "the answer sends it back to work");
    until(&mut k, &mut p, 30_000, "the run is ready", |p| run_of(p, &run).state == store::RunState::Review);
    let r = run_of(&p, &run);
    assert_eq!(r.merged, 2, "{r:?}");
    let answer = w.calls().into_iter().find(|c| c.prompt.contains("red please")).expect("the worker got the answer");
    assert_eq!(answer.resume, session, "in the same session");
    let (ta, tb) = (task_of(&p, &a), task_of(&p, &b));
    assert!(merged(&ta) && merged(&tb));
    assert!(!ta.blocked && ta.questions.is_empty(), "the question is gone: {ta:?}");
    assert!(tb.started >= ta.finished, "b ran after a merged");
    assert_eq!(sh(&repo, &["show", &format!("{}:colour.txt", r.branch)]), "red");
    assert_eq!(sh(&repo, &["show", &format!("{}:after.txt", r.branch)]), "done");
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ restarts

/// oriel closes while the first of two (one after another) works. It comes back stopped; r resumes: the cut-off
/// worker continues its session, the second follows, the run finishes.
#[test]
fn qa_batch_restart_mid_run_then_resume() {
    let dir = scratch("restart");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    boot(&mut k, &mut p);
    let a = p.add_task("Long one", "hold never\nwrite long.txt: long", 0, "");
    let b = p.add_task("Next one", "write next.txt: next", 0, "");
    let run = mark_and_run(&mut k, &mut p, &[a.clone(), b.clone()], true);
    until(&mut k, &mut p, 30_000, "a is working", |p| task_of(p, &a).status == Status::Running && !task_of(p, &a).session_id.is_empty() && w.first(&a).is_some());
    let session = task_of(&p, &a).session_id.clone();
    drop(p); // oriel closes
    std::thread::sleep(Duration::from_millis(300));
    let mut p = pane(&dir, &repo, &w);
    let r = run_of(&p, &run);
    assert_eq!(r.state, store::RunState::Stopped);
    assert!(r.error.contains("r resumes"), "{}", r.error);
    let ta = task_of(&p, &a);
    assert_eq!((ta.status, ta.last.as_str()), (Status::Review, "stopped: oriel was closed"));
    assert!(task_of(&p, &b).status == Status::Todo && task_of(&p, &b).queued);
    boot(&mut k, &mut p);
    let board = k.render_html(&mut p, 150, 44, &snap("batch-restarted"));
    assert!(board.contains("stopped"), "{board}");
    k.key(&mut p, KeyCode::Up);
    assert!(p.lead_focus, "up from the first card reaches the run panel");
    k.key(&mut p, KeyCode::Char('r'));
    until(&mut k, &mut p, 30_000, "resumed and finished", |p| run_of(p, &run).state == store::RunState::Review);
    let resumed = w.calls().into_iter().filter(|c| c.task == a).last().unwrap();
    assert_eq!(resumed.resume, session, "the cut-off worker continued its own session");
    assert!(resumed.prompt.contains("oriel was restarted"), "{}", resumed.prompt);
    let r = run_of(&p, &run);
    assert_eq!(r.merged, 2, "{r:?}");
    assert_eq!(sh(&repo, &["show", &format!("{}:long.txt", r.branch)]), "long");
    assert_eq!(sh(&repo, &["show", &format!("{}:next.txt", r.branch)]), "next");
    let _ = std::fs::remove_dir_all(&dir);
}

/// oriel closes while finished tasks wait in the merge queue (another merge's gate is slow). The queue lives in
/// memory only; after r they should still get merged, not sit in REVIEW forever with the run "running".
#[test]
#[ignore = "fails: the in-memory merge queue isn't rebuilt on resume; tasks sit 'waiting to merge' forever"]
fn qa_batch_restart_with_merges_queued_resumes_them() {
    let dir = scratch("restart-queue");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    boot(&mut k, &mut p);
    let a = p.add_task("Queued a", "write qa.txt: a", 0, "");
    let b = p.add_task("Queued b", "write qb.txt: b", 0, "");
    // stands in for a long gate on some other merge: everything that finishes waits in the queue
    p.merging = Some("slow-gate".into());
    let run = mark_and_run(&mut k, &mut p, &[a.clone(), b.clone()], false);
    until(&mut k, &mut p, 30_000, "both finished and queued to merge", |p| {
        [&a, &b].iter().all(|id| {
            let t = task_of(p, id);
            t.status == Status::Review && t.want_merge && p.merge_queue.contains(&t.id)
        })
    });
    drop(p);
    std::thread::sleep(Duration::from_millis(300));
    let mut p = pane(&dir, &repo, &w);
    assert_eq!(run_of(&p, &run).state, store::RunState::Stopped);
    boot(&mut k, &mut p);
    k.key(&mut p, KeyCode::Up);
    assert!(p.lead_focus);
    k.key(&mut p, KeyCode::Char('r'));
    assert!(run_of(&p, &run).state.active(), "resumed");
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && run_of(&p, &run).state != store::RunState::Review {
        k.poll(&mut p);
        std::thread::sleep(Duration::from_millis(20));
    }
    let (ta, tb) = (task_of(&p, &a), task_of(&p, &b));
    assert_eq!(run_of(&p, &run).state, store::RunState::Review, "after resume: a {:?} want_merge={} last={:?}; b {:?} last={:?}; queue {:?}", ta.status, ta.want_merge, ta.last, tb.status, tb.last, p.merge_queue);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Stop a run of yours (s on the panel) while its first task is half done. Stop means stop: the interrupted
/// worker's half-finished edit must not be merged into the run's branch as if the task were finished.
#[test]
#[ignore = "fails: after_check queues a stopped worker's half-done work for merging in runs of your own"]
fn qa_batch_stop_does_not_merge_the_interrupted_task() {
    let dir = scratch("stop-partial");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    boot(&mut k, &mut p);
    let a = p.add_task("Half done", "pre half.txt: half\nhold never\nwrite whole.txt: whole", 0, "");
    let run = mark_and_run(&mut k, &mut p, &[a.clone()], false);
    until(&mut k, &mut p, 30_000, "it works and has made its half edit", |p| working(p, &a) && task_of(p, &a).touched.contains(&"half.txt".to_string()));
    p.lead_focus = false;
    p.row[p.col] = 0;
    k.key(&mut p, KeyCode::Up);
    assert!(p.lead_focus);
    k.key(&mut p, KeyCode::Char('s'));
    assert!(matches!(p.mode, Mode::Confirm(_)), "s asks first");
    k.key(&mut p, KeyCode::Char('y'));
    assert_eq!(run_of(&p, &run).state, store::RunState::Stopped);
    until(&mut k, &mut p, 15_000, "the worker has stopped", |p| p.live.get(&a).is_some_and(|l| l.stop.is_none() && !l.checking));
    // give a merge the time it would take
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        k.poll(&mut p);
        std::thread::sleep(Duration::from_millis(20));
    }
    let (ta, r) = (task_of(&p, &a), run_of(&p, &run));
    let on_branch = git::run(&repo, &["cat-file", "-e", &format!("{}:half.txt", r.branch)]).ok;
    assert!(!merged(&ta) && !on_branch, "after stop the interrupted task was merged anyway: status {:?} outcome {:?} last {:?}; half.txt on {}: {on_branch}; log {:?}", ta.status, ta.outcome, ta.last, r.branch, r.log);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Stop a run of yours while the first of two works (max 1 at a time), then r: the one that never started
/// should run after all. (stop_run un-queues it; resume only restarts cut-off workers.)
#[test]
#[ignore = "fails: stop_run un-queues waiting tasks and resume_run never queues them again"]
fn qa_batch_stop_then_resume_starts_the_tasks_that_never_ran() {
    let dir = scratch("stop-resume");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    p.lead_cfg.max_parallel = 1;
    boot(&mut k, &mut p);
    let a = p.add_task("Working one", "hold never\nwrite w1.txt: one", 0, "");
    let b = p.add_task("Waiting one", "write w2.txt: two", 0, "");
    let run = mark_and_run(&mut k, &mut p, &[a.clone(), b.clone()], false);
    until(&mut k, &mut p, 30_000, "a works, b waits", |p| working(p, &a) && task_of(p, &b).status == Status::Todo);
    p.lead_focus = false;
    p.row[p.col] = 0;
    k.key(&mut p, KeyCode::Up);
    k.key(&mut p, KeyCode::Char('s'));
    k.key(&mut p, KeyCode::Char('y'));
    assert_eq!(run_of(&p, &run).state, store::RunState::Stopped);
    until(&mut k, &mut p, 15_000, "a has stopped", |p| p.live.get(&a).is_some_and(|l| l.stop.is_none() && !l.checking && !l.busy) && p.merging.is_none());
    let board = k.render_html(&mut p, 150, 44, &snap("batch-stopped-by-you"));
    assert!(board.contains("r resume") || board.contains("stopped"), "{board}");
    p.lead_focus = false;
    p.row[p.col] = 0;
    k.key(&mut p, KeyCode::Up);
    assert!(p.lead_focus);
    k.key(&mut p, KeyCode::Char('r'));
    assert!(run_of(&p, &run).state.active(), "r resumed it");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && run_of(&p, &run).state != store::RunState::Review {
        k.poll(&mut p);
        std::thread::sleep(Duration::from_millis(20));
    }
    let (ta, tb, r) = (task_of(&p, &a), task_of(&p, &b), run_of(&p, &run));
    assert_eq!(r.state, store::RunState::Review, "after s then r: a {:?}/{:?} {:?}; b {:?} queued={} {:?}; {} worker starts for b", ta.status, ta.outcome, ta.last, tb.status, tb.queued, tb.last, w.calls().iter().filter(|c| c.task == b).count());
    assert!(merged(&tb));
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ marks

/// Delete a marked card (x, y): its mark must go too. With every marked card deleted, enter is enter again.
#[test]
#[ignore = "fails: deleting a task (Pending::Delete) leaves its id in marked"]
fn qa_deleting_a_marked_task_clears_its_mark() {
    let dir = scratch("marks-delete");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    boot(&mut k, &mut p);
    let a = p.add_task("Marked a", "write ma.txt: a", 0, "");
    let b = p.add_task("Marked b", "write mb.txt: b", 0, "");
    let c = p.add_task("Plain c", "write mc.txt: c", 0, "");
    for id in [&a, &b] {
        p.select(id);
        k.key(&mut p, KeyCode::Char(' '));
    }
    assert_eq!(p.marked.len(), 2);
    discard_card(&mut k, &mut p, &a);
    assert!(p.task(&a).is_none(), "a is deleted");
    assert!(!p.marked.contains(&a), "a deleted task stays marked: {:?}", p.marked);
    let s = k.render_html(&mut p, 150, 44, &snap("marks-after-delete"));
    assert!(!s.contains("[2]"), "the board still counts two marks:\n{s}");
    discard_card(&mut k, &mut p, &b);
    assert!(p.marked.is_empty(), "every marked card is gone, the marks aren't: {:?}", p.marked);
    p.select(&c);
    k.key(&mut p, KeyCode::Enter);
    assert!(!matches!(p.mode, Mode::Batch), "enter opened 'run 2 tasks together' for tasks that no longer exist");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Marks made in one repo don't come along when you open another (o): the run would start in the new repo with
/// the old repo's tasks.
#[test]
#[ignore = "fails: marks survive a repo switch; the run takes the old repo's tasks into the new repo"]
fn qa_marks_do_not_follow_you_to_another_repo() {
    let dir = scratch("marks-repo");
    let repo = temp_repo(&dir);
    let other = temp_repo(&dir.join("other"));
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    boot(&mut k, &mut p);
    let a = p.add_task("Repo one task", "write one.txt: one", 0, "");
    p.select(&a);
    k.key(&mut p, KeyCode::Char(' '));
    assert_eq!(p.marked, vec![a.clone()]);
    k.key(&mut p, KeyCode::Char('o'));
    assert!(matches!(p.mode, Mode::Repo(_)));
    k.typ(&mut p, &other.display().to_string());
    k.key(&mut p, KeyCode::Enter);
    until(&mut k, &mut p, 5000, "the other repo is open", |p| matches!(p.mode, Mode::Board) && p.repo.as_ref().is_some_and(|r| r.name == "repo" && r.root.starts_with(dir.join("other"))));
    let repo_one = task_of(&p, &a).repo.clone();
    if !p.marked.is_empty() {
        // the hints still offer "run the marked together" here: try it
        k.key(&mut p, KeyCode::Enter);
        if matches!(p.mode, Mode::Batch) {
            k.key(&mut p, KeyCode::Char('p'));
        }
        until(&mut k, &mut p, 10_000, "whatever started settles", |p| p.store.runs.iter().all(|r| r.state != store::RunState::Starting));
    }
    let t = task_of(&p, &a);
    assert!(p.store.runs.is_empty(), "a run started in {} with a task from {repo_one}; the task now says repo={} run={}", p.repo_key(), t.repo, t.run);
    assert_eq!(t.repo, repo_one);
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ priorities and dependencies

fn prio(p: &mut Agents, id: &str, v: i8) {
    p.task_mut(id).unwrap().priority = v;
}

/// Together, one at a time: an urgent task waits on a low one; a normal one is free. Dependencies are kept:
/// the urgent task starts only after the low one merged, and everything lands.
#[test]
fn qa_priority_with_dependency_keeps_the_order() {
    let dir = scratch("prio-deps");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    p.lead_cfg.max_parallel = 1;
    boot(&mut k, &mut p);
    let low = p.add_task("Low base", "write low.txt: low", 0, "");
    let urgent = p.add_task("Urgent on top", "write urgent.txt: urgent", 0, "");
    let normal = p.add_task("Normal side", "write normal.txt: normal", 0, "");
    prio(&mut p, &low, -1);
    prio(&mut p, &urgent, 2);
    p.task_mut(&urgent).unwrap().depends_on = vec![low.clone()];
    let run = mark_and_run(&mut k, &mut p, &[low.clone(), urgent.clone(), normal.clone()], false);
    until(&mut k, &mut p, 60_000, "the run is ready", |p| run_of(p, &run).state == store::RunState::Review);
    assert_eq!(run_of(&p, &run).merged, 3);
    let (tl, tu) = (task_of(&p, &low), task_of(&p, &urgent));
    assert!(tu.started >= tl.finished, "the urgent task started after its dependency merged");
    assert!(w.first(&low).unwrap() < w.first(&urgent).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Same board: nothing else waits on "Normal side", but the urgent task waits on "Low base". Starting the normal
/// one first delays the urgent one (priority inversion); the low task carries the urgent one's weight.
#[test]
#[ignore = "fails: no priority inheritance: an urgent task's low dependency waits behind normal tasks"]
fn qa_priority_urgent_task_pulls_its_dependency_forward() {
    let dir = scratch("prio-inversion");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    p.lead_cfg.max_parallel = 1;
    boot(&mut k, &mut p);
    let low = p.add_task("Low base", "write low.txt: low", 0, "");
    let urgent = p.add_task("Urgent on top", "write urgent.txt: urgent", 0, "");
    let normal = p.add_task("Normal side", "write normal.txt: normal", 0, "");
    prio(&mut p, &low, -1);
    prio(&mut p, &urgent, 2);
    p.task_mut(&urgent).unwrap().depends_on = vec![low.clone()];
    let run = mark_and_run(&mut k, &mut p, &[low.clone(), urgent.clone(), normal.clone()], false);
    until(&mut k, &mut p, 60_000, "the run is ready", |p| run_of(p, &run).state == store::RunState::Review);
    let order: Vec<String> = w.calls().iter().map(|c| p.task(&c.task).map(|t| t.title.clone()).unwrap_or_default()).collect();
    assert!(w.first(&low).unwrap() < w.first(&normal).unwrap(), "start order {order:?}: the urgent task's dependency waited behind a normal task");
    let _ = std::fs::remove_dir_all(&dir);
}

/// B says "after A". Marked B then A and run one after another: the order you marked adds "A after B", which
/// with your own link is a cycle. It must not start a run that can never move.
#[test]
#[ignore = "fails: serial order plus an opposite after link is a cycle; the run starts nothing, forever"]
fn qa_serial_batch_against_an_after_link_does_not_deadlock() {
    let dir = scratch("serial-cycle");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    boot(&mut k, &mut p);
    let a = p.add_task("Base A", "write ca.txt: a", 0, "");
    let b = p.add_task("Builds on A", "write cb.txt: b", 0, "");
    p.task_mut(&b).unwrap().depends_on = vec![a.clone()];
    p.lead_focus = false;
    for id in [&b, &a] {
        p.select(id);
        k.key(&mut p, KeyCode::Char(' '));
    }
    k.key(&mut p, KeyCode::Enter);
    k.key(&mut p, KeyCode::Char('s'));
    if p.store.runs.is_empty() {
        return; // refused up front: fine
    }
    let run = p.store.runs[0].id.clone();
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && run_of(&p, &run).state.active() {
        k.poll(&mut p);
        std::thread::sleep(Duration::from_millis(20));
    }
    let (ta, tb) = (task_of(&p, &a), task_of(&p, &b));
    assert!(!run_of(&p, &run).state.active(), "deadlock: A waits for {:?}, B waits for {:?}, nothing ever starts ({} worker calls)", ta.depends_on, tb.depends_on, w.calls().len());
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ budgets

/// run_budget_usd = 0 means no cap (like the lead's budget checks): the run just runs.
#[test]
fn qa_batch_with_budget_zero_runs_uncapped() {
    let dir = scratch("budget-zero");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    p.lead_cfg.run_budget_usd = 0.0;
    boot(&mut k, &mut p);
    let a = p.add_task("Pricey a", "write pa.txt: a\ncost 2.50", 0, "");
    let b = p.add_task("Pricey b", "write pb.txt: b\ncost 2.50", 0, "");
    let run = mark_and_run(&mut k, &mut p, &[a.clone(), b.clone()], false);
    assert_eq!(run_of(&p, &run).budget_usd, 0.0);
    until(&mut k, &mut p, 30_000, "the run is ready", |p| run_of(p, &run).state == store::RunState::Review);
    assert_eq!(run_of(&p, &run).merged, 2);
    assert!((p.spend(&run) - 5.0).abs() < 1e-9, "{}", p.spend(&run));
    assert!(!k.notices().iter().any(|n| n.contains("budget")), "no budget warning for an uncapped run: {:?}", k.notices());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The run popup with run_budget_usd = 0 tells you the budget is "$0.00" — which reads as "nothing may run".
#[test]
#[ignore = "fails: the run popup shows an uncapped (0) budget as 'Budget $0.00'"]
fn qa_batch_popup_says_what_a_zero_budget_means() {
    let dir = scratch("budget-popup");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    p.lead_cfg.run_budget_usd = 0.0;
    boot(&mut k, &mut p);
    let a = p.add_task("Some task", "write x.txt: x", 0, "");
    p.select(&a);
    k.key(&mut p, KeyCode::Char(' '));
    k.key(&mut p, KeyCode::Enter);
    let s = k.render_html(&mut p, 150, 44, &snap("batch-popup-zero-budget"));
    assert!(s.contains("run 1 task together"), "{s}");
    assert!(!s.contains("Budget $0.00"), "an uncapped run is shown as a $0.00 budget:\n{s}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A $0.08 budget, three $0.05 tasks one at a time: the third never starts (right), you're told (right) —
/// and then the run should settle so it can be reviewed and merged, not stay "running" with nothing running.
#[test]
#[ignore = "fails: after the budget stops new workers the run never settles (stays Running, m refused)"]
fn qa_batch_budget_spent_mid_run_settles() {
    let dir = scratch("budget-spent");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    p.lead_cfg.run_budget_usd = 0.08;
    p.lead_cfg.max_parallel = 1;
    boot(&mut k, &mut p);
    let ids: Vec<String> = ["one", "two", "three"].iter().map(|n| p.add_task(&format!("Task {n}"), &format!("write {n}.txt: {n}\ncost 0.05"), 0, "")).collect();
    let run = mark_and_run(&mut k, &mut p, &ids, false);
    until(&mut k, &mut p, 30_000, "two merged", |p| p.store.tasks.iter().filter(|t| t.run == run && merged(t)).count() == 2);
    until(&mut k, &mut p, 5000, "told about the budget", |_| true);
    assert!(k.notices().iter().any(|n| n.contains("budget")), "{:?}", k.notices());
    let third = p.store.tasks.iter().find(|t| t.run == run && !merged(t)).cloned().unwrap();
    assert_eq!(third.status, Status::Todo, "the third never started");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && run_of(&p, &run).state.active() {
        k.poll(&mut p);
        std::thread::sleep(Duration::from_millis(20));
    }
    let r = run_of(&p, &run);
    assert!(!r.state.active(), "nothing is running or merging and nothing can start, but the run stays {:?} (m refuses: 'the run is still working'); log: {:?}", r.state, r.log);
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ two runs at once

/// Two runs of your own at the same time, the first slower: both finish, each on its own branch, and both merge
/// into master one after the other.
#[test]
fn qa_two_batch_runs_at_once_both_merge() {
    let dir = scratch("two-runs");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    boot(&mut k, &mut p);
    let a = p.add_task("First run task", "hold go-first\nwrite first.txt: first", 0, "");
    let b = p.add_task("Second run task", "write second.txt: second", 0, "");
    let r1 = mark_and_run(&mut k, &mut p, &[a.clone()], false);
    until(&mut k, &mut p, 30_000, "the first run's task works", |p| task_of(p, &a).status == Status::Running);
    let r2 = mark_and_run(&mut k, &mut p, &[b.clone()], false);
    until(&mut k, &mut p, 30_000, "the second run is ready", |p| run_of(p, &r2).state == store::RunState::Review);
    assert!(run_of(&p, &r1).state.active(), "the first still works");
    assert_ne!(run_of(&p, &r1).branch, run_of(&p, &r2).branch);
    w.release("go-first");
    until(&mut k, &mut p, 30_000, "the first run is ready", |p| run_of(p, &r1).state == store::RunState::Review);
    let (b1, b2) = (run_of(&p, &r1).branch, run_of(&p, &r2).branch);
    assert_eq!(sh(&repo, &["show", &format!("{b1}:first.txt")]), "first");
    assert!(!git::run(&repo, &["cat-file", "-e", &format!("{b1}:second.txt")]).ok, "the runs don't mix");
    // merge whichever the panel shows, then the other
    for _ in 0..2 {
        let cur = p.current_run().map(|r| r.id.clone()).expect("a run on the panel");
        p.lead_focus = false;
        p.row[p.col] = 0;
        k.key(&mut p, KeyCode::Up);
        assert!(p.lead_focus);
        k.key(&mut p, KeyCode::Char('m'));
        assert!(matches!(p.mode, Mode::Confirm(_)), "m asks first");
        k.key(&mut p, KeyCode::Char('y'));
        until(&mut k, &mut p, 15_000, "merged", |p| run_of(p, &cur).state == store::RunState::Merged || !run_of(p, &cur).error.is_empty());
        assert_eq!(run_of(&p, &cur).state, store::RunState::Merged, "{}", run_of(&p, &cur).error);
    }
    assert_eq!(std::fs::read_to_string(repo.join("first.txt")).unwrap().trim(), "first");
    assert_eq!(std::fs::read_to_string(repo.join("second.txt")).unwrap().trim(), "second");
    for br in [&b1, &b2] {
        assert!(!git::run(&repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{br}")]).ok, "{br} deleted");
    }
    assert_eq!(sh(&repo, &["status", "--porcelain"]), "");
    assert_eq!(worktrees(&repo), 1, "only the main checkout is left: {}", sh(&repo, &["worktree", "list"]));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Two runs, the *older* one finishes first. The alert says "your run is ready · d reviews, m merges", but the
/// run panel shows the newest open run (still working): d and m should reach the run that's ready.
#[test]
#[ignore = "fails: the run panel only shows the newest open run; an older ready run can't be reviewed"]
fn qa_two_batch_runs_the_ready_older_run_is_reachable() {
    let dir = scratch("two-runs-reach");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    boot(&mut k, &mut p);
    let a = p.add_task("Older quick", "write older.txt: older", 0, "");
    let b = p.add_task("Newer slow", "hold go-newer\nwrite newer.txt: newer", 0, "");
    let r1 = mark_and_run(&mut k, &mut p, &[a.clone()], false);
    until(&mut k, &mut p, 30_000, "the first run's branch exists", |p| !run_of(p, &r1).branch.is_empty() && task_of(p, &a).status != Status::Todo);
    let r2 = mark_and_run(&mut k, &mut p, &[b.clone()], false);
    until(&mut k, &mut p, 30_000, "the older run is ready", |p| run_of(p, &r1).state == store::RunState::Review);
    until(&mut k, &mut p, 30_000, "the newer one is working", |p| task_of(p, &b).status == Status::Running);
    assert!(k.notices().iter().any(|n| n.contains("your run is ready")), "{:?}", k.notices());
    p.lead_focus = false;
    p.row[p.col] = 0;
    k.key(&mut p, KeyCode::Up);
    let board = k.render_html(&mut p, 150, 44, &snap("two-runs-panel"));
    k.key(&mut p, KeyCode::Char('d'));
    let shown = match &p.mode {
        Mode::Diff(v) => v.id.clone(),
        _ => String::new(),
    };
    w.release("go-newer");
    assert_eq!(shown, r1, "d on the run panel opened {} (newer, still running) instead of the ready run {r1}; newer={r2}\n{board}", if shown.is_empty() { "nothing".to_string() } else { shown.clone() });
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ merging a run that merged nothing

/// The only task changes nothing. The run still reaches review, and m finishes cleanly with nothing to merge:
/// master unchanged, the run's branch and checkouts gone.
#[test]
fn qa_merge_a_run_whose_task_changed_nothing() {
    let dir = scratch("merge-nothing");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    boot(&mut k, &mut p);
    let head = sh(&repo, &["rev-parse", "HEAD"]);
    let a = p.add_task("Look but don't touch", "cost 0.01", 0, "");
    let run = mark_and_run(&mut k, &mut p, &[a.clone()], false);
    until(&mut k, &mut p, 30_000, "the run is ready", |p| run_of(p, &run).state == store::RunState::Review);
    let r = run_of(&p, &run);
    let ta = task_of(&p, &a);
    assert_eq!(ta.status, Status::Done);
    assert!(ta.last.contains("nothing to merge"), "{}", ta.last);
    p.lead_focus = false;
    p.row[p.col] = 0;
    k.key(&mut p, KeyCode::Up);
    assert!(p.lead_focus);
    k.key(&mut p, KeyCode::Char('d'));
    until(&mut k, &mut p, 8000, "run diff", |p| matches!(&p.mode, Mode::Diff(v) if v.data.is_some()));
    let diff = k.render_html(&mut p, 150, 44, &snap("merge-nothing-diff"));
    assert!(diff.contains("0 files") || diff.contains("+0"), "{diff}");
    k.key(&mut p, KeyCode::Char('m'));
    k.key(&mut p, KeyCode::Char('y'));
    until(&mut k, &mut p, 15_000, "merged", |p| run_of(p, &run).state == store::RunState::Merged || !run_of(p, &run).error.is_empty());
    let r2 = run_of(&p, &run);
    assert_eq!(r2.state, store::RunState::Merged, "{}", r2.error);
    assert!(r2.log.iter().any(|l| l.contains("nothing to merge")), "{:?}", r2.log);
    assert_eq!(sh(&repo, &["rev-parse", "HEAD"]), head, "master didn't move");
    assert!(!git::run(&repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{}", r.branch)]).ok, "the run's branch is gone");
    assert!(!Path::new(&r.worktree).exists(), "the run's checkout is gone");
    assert_eq!(worktrees(&repo), 1, "{}", sh(&repo, &["worktree", "list"]));
    assert_eq!(sh(&repo, &["status", "--porcelain"]), "");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every task discarded, the run stopped by hand (s), then m: nothing to merge, and it all cleans up.
#[test]
fn qa_merge_a_stopped_run_with_everything_discarded() {
    let dir = scratch("merge-discarded");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    boot(&mut k, &mut p);
    let head = sh(&repo, &["rev-parse", "HEAD"]);
    let a = p.add_task("Doomed", "hold never\nwrite doomed.txt: x", 0, "");
    let run = mark_and_run(&mut k, &mut p, &[a.clone()], false);
    until(&mut k, &mut p, 30_000, "it works", |p| working(p, &a));
    discard_card(&mut k, &mut p, &a);
    until(&mut k, &mut p, 15_000, "discarded", |p| task_of(p, &a).status == Status::Done);
    let r = run_of(&p, &run);
    if r.state.active() {
        // the run doesn't settle by itself (see qa_batch_discarding_the_last_open_task_settles_the_run): stop it
        p.lead_focus = false;
        p.row[p.col] = 0;
        k.key(&mut p, KeyCode::Up);
        assert!(p.lead_focus);
        k.key(&mut p, KeyCode::Char('s'));
        assert!(matches!(p.mode, Mode::Confirm(_)));
        k.key(&mut p, KeyCode::Char('y'));
        assert_eq!(run_of(&p, &run).state, store::RunState::Stopped);
    }
    p.lead_focus = false;
    p.row[p.col] = 0;
    k.key(&mut p, KeyCode::Up);
    k.key(&mut p, KeyCode::Char('m'));
    assert!(matches!(p.mode, Mode::Confirm(_)), "m is offered on a finished run");
    k.key(&mut p, KeyCode::Char('y'));
    until(&mut k, &mut p, 15_000, "merged", |p| run_of(p, &run).state == store::RunState::Merged || !run_of(p, &run).error.is_empty());
    assert_eq!(run_of(&p, &run).state, store::RunState::Merged, "{}", run_of(&p, &run).error);
    assert_eq!(sh(&repo, &["rev-parse", "HEAD"]), head);
    assert!(!repo.join("doomed.txt").exists());
    assert_eq!(worktrees(&repo), 1, "{}", sh(&repo, &["worktree", "list"]));
    assert_eq!(sh(&repo, &["status", "--porcelain"]), "");
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ the task form

fn open_form(k: &mut Kit, p: &mut Agents, title: &str) {
    p.lead_focus = false;
    k.key(p, KeyCode::Char('n'));
    assert!(matches!(p.mode, Mode::Form(_)));
    k.typ(p, title);
}

fn to_field(k: &mut Kit, p: &mut Agents, field: usize) {
    for _ in 0..8 {
        if matches!(&p.mode, Mode::Form(f) if f.field == field) {
            return;
        }
        k.key(p, KeyCode::Tab);
    }
    panic!("never reached field {field}");
}

fn form_err(p: &Agents) -> Option<(String, usize)> {
    match &p.mode {
        Mode::Form(f) => Some((f.err.clone(), f.field)),
        _ => None,
    }
}

/// Bad input in the new-task form: an unknown "after", a budget that isn't dollars, a negative one. Each keeps
/// the form open on the right field with a message; a good one saves.
#[test]
fn qa_form_rejects_unknown_after_and_bad_budgets() {
    let dir = scratch("form-bad");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    boot(&mut k, &mut p);
    let done = p.add_task("Finished thing", "x", 0, "");
    p.task_mut(&done).unwrap().status = Status::Done;
    let ctrl_s = |k: &mut Kit, p: &mut Agents| {
        k.key_mod(p, KeyCode::Char('s'), KeyModifiers::CONTROL);
    };
    open_form(&mut k, &mut p, "Validate me");
    to_field(&mut k, &mut p, 5);
    k.typ(&mut p, "no such task");
    ctrl_s(&mut k, &mut p);
    let (err, field) = form_err(&p).expect("the form stays open");
    assert!(err.contains("no open task called 'no such task'") && field == 5, "{err} @ {field}");
    let s = k.render_html(&mut p, 150, 44, &snap("form-unknown-after"));
    assert!(s.contains("no open task called"), "the error is on screen:\n{s}");
    // a finished task isn't something to wait for
    k.key_mod(&mut p, KeyCode::Char('u'), KeyModifiers::CONTROL);
    k.typ(&mut p, "Finished");
    ctrl_s(&mut k, &mut p);
    assert!(form_err(&p).is_some_and(|(e, f)| e.contains("no open task") && f == 5), "{:?}", form_err(&p));
    k.key_mod(&mut p, KeyCode::Char('u'), KeyModifiers::CONTROL);
    to_field(&mut k, &mut p, 6);
    for bad in ["abc", "-1", "1,5", "$-2"] {
        k.key_mod(&mut p, KeyCode::Char('u'), KeyModifiers::CONTROL);
        k.typ(&mut p, bad);
        ctrl_s(&mut k, &mut p);
        let (err, field) = form_err(&p).unwrap_or_else(|| panic!("budget {bad:?} was accepted"));
        assert!(err.contains("number of dollars") && field == 6, "{bad}: {err} @ {field}");
    }
    assert_eq!(p.store.tasks.len(), 1, "nothing saved yet");
    k.key_mod(&mut p, KeyCode::Char('u'), KeyModifiers::CONTROL);
    k.typ(&mut p, "$1.5");
    ctrl_s(&mut k, &mut p);
    assert!(matches!(p.mode, Mode::Board), "{:?}", form_err(&p));
    let t = p.store.tasks.iter().find(|t| t.title == "Validate me").unwrap();
    assert_eq!((t.budget_usd, t.status), (1.5, Status::Todo));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A budget of "inf" (or 1e400) parses as infinity and is saved. JSON can't hold infinity: serde writes null,
/// and the next load of tasks.json fails as a whole — every task, run and record is gone.
#[test]
#[ignore = "fails: budget 'inf' is accepted, saved as null, and the next load drops every task"]
fn qa_form_infinite_budget_is_refused_and_the_board_survives_a_restart() {
    let dir = scratch("form-inf");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    boot(&mut k, &mut p);
    p.add_task("An older task", "keep me", 0, "");
    open_form(&mut k, &mut p, "Endless budget");
    to_field(&mut k, &mut p, 6);
    k.typ(&mut p, "inf");
    k.key_mod(&mut p, KeyCode::Char('s'), KeyModifiers::CONTROL);
    let refused = form_err(&p).is_some_and(|(e, _)| !e.is_empty());
    let saved = p.store.tasks.len();
    drop(p);
    std::thread::sleep(Duration::from_millis(200));
    let p = pane(&dir, &repo, &w);
    let back = p.store.tasks.len();
    assert!(refused, "an infinite budget was accepted ({saved} tasks before the restart, {back} after)");
    assert_eq!(back, saved, "tasks lost across a restart");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The lead-run form: a budget pasted as "inf" gets the same null-in-JSON treatment on the run.
#[test]
#[ignore = "fails: a pasted 'inf' run budget is accepted; the next load drops every task and run"]
fn qa_lead_form_pasted_infinite_budget_is_refused() {
    let dir = scratch("lead-form-inf");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    boot(&mut k, &mut p);
    p.add_task("An older task", "keep me", 0, "");
    k.key(&mut p, KeyCode::Char('L'));
    assert!(matches!(p.mode, Mode::LeadForm(_)));
    k.typ(&mut p, "A goal");
    for _ in 0..4 {
        k.key(&mut p, KeyCode::Tab);
    }
    k.key_mod(&mut p, KeyCode::Char('u'), KeyModifiers::CONTROL);
    let mut cx = crate::pane::Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: &mut vec![], focused: true, time: 1.0 };
    p.paste("inf", &mut cx);
    k.key_mod(&mut p, KeyCode::Char('s'), KeyModifiers::CONTROL);
    let started = p.store.runs.len();
    let budget = p.store.runs.first().map(|r| r.budget_usd).unwrap_or(0.0);
    drop(p);
    std::thread::sleep(Duration::from_millis(200));
    let p = pane(&dir, &repo, &w);
    assert!(budget.is_finite(), "a run started with budget {budget}; after a restart the board has {} tasks and {} runs (had 1 and {started})", p.store.tasks.len(), p.store.runs.len());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The lead-run form: a budget that isn't a number ("1.2.3") silently becomes the default instead of an error.
#[test]
#[ignore = "fails: an unparseable lead-run budget silently becomes the default"]
fn qa_lead_form_bad_budget_is_not_silently_replaced() {
    let dir = scratch("lead-form-bad");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    p.lead_cfg.run_budget_usd = 8.0;
    boot(&mut k, &mut p);
    k.key(&mut p, KeyCode::Char('L'));
    k.typ(&mut p, "A goal");
    for _ in 0..4 {
        k.key(&mut p, KeyCode::Tab);
    }
    k.key_mod(&mut p, KeyCode::Char('u'), KeyModifiers::CONTROL);
    k.typ(&mut p, "1.2.3");
    k.key_mod(&mut p, KeyCode::Char('s'), KeyModifiers::CONTROL);
    let got = p.store.runs.first().map(|r| r.budget_usd);
    assert!(matches!(p.mode, Mode::LeadForm(ref f) if !f.err.is_empty()), "typed budget 1.2.3, the run started with budget {got:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// "after" in the form promises "waits for these to be merged". add & start (the default button) with an
/// unmerged "after" starts the task right away anyway.
#[test]
#[ignore = "fails: add & start ignores 'after' although the form says it waits for them to be merged"]
fn qa_form_add_and_start_waits_for_after() {
    let dir = scratch("form-after-start");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    boot(&mut k, &mut p);
    let a = p.add_task("Foundation", "lay it", 0, "");
    open_form(&mut k, &mut p, "Roof");
    to_field(&mut k, &mut p, 5);
    k.typ(&mut p, "Foundation");
    to_field(&mut k, &mut p, 7);
    assert!(matches!(&p.mode, Mode::Form(f) if f.button == 1), "add & start is the default");
    k.key(&mut p, KeyCode::Enter);
    let roof = p.store.tasks.iter().find(|t| t.title == "Roof").cloned().expect("saved");
    assert_eq!(roof.depends_on, vec![a.clone()]);
    std::thread::sleep(Duration::from_millis(300));
    k.poll(&mut p);
    let roof = task_of(&p, &roof.id);
    assert_eq!(roof.status, Status::Todo, "Roof is {:?} while Foundation ({:?}) isn't merged", roof.status, task_of(&p, &a).status);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Editing A to wait for B while B already waits for A: a cycle nothing can ever start from.
#[test]
#[ignore = "fails: the task form saves an after-cycle (A after B, B after A)"]
fn qa_form_refuses_an_after_cycle() {
    let dir = scratch("form-cycle");
    let repo = temp_repo(&dir);
    let w = Workers::new(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo, &w);
    boot(&mut k, &mut p);
    let a = p.add_task("Alpha", "a", 0, "");
    let b = p.add_task("Beta", "b", 0, "");
    p.task_mut(&b).unwrap().depends_on = vec![a.clone()];
    p.lead_focus = false;
    p.select(&a);
    k.key(&mut p, KeyCode::Char('e'));
    assert!(matches!(&p.mode, Mode::Form(f) if f.editing.as_deref() == Some(a.as_str())));
    to_field(&mut k, &mut p, 5);
    k.typ(&mut p, "Beta");
    k.key_mod(&mut p, KeyCode::Char('s'), KeyModifiers::CONTROL);
    let ta = task_of(&p, &a);
    assert!(form_err(&p).is_some_and(|(e, _)| !e.is_empty()), "saved a cycle: Alpha after {:?}, Beta after {:?}", ta.depends_on, task_of(&p, &b).depends_on);
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ +/- priority

/// + and - on each column: a waiting card changes (and clamps at urgent/low); a finished card doesn't. A card
/// that already started (hand-made, in a tab) has nothing left to wait for, so a priority can't do anything for
/// it — it shouldn't claim "priority: high".
#[test]
#[ignore = "fails: +/- changes and announces priority on running cards, where it does nothing"]
fn qa_priority_keys_on_cards_in_every_column() {
    let dir = scratch("prio-keys");
    let mut k = Kit::new();
    let mut p = super::tests::demo(&dir);
    let id_of = |p: &Agents, s: Status| p.store.tasks.iter().find(|t| t.status == s).unwrap().id.clone();
    let (todo, running, review, done) = (id_of(&p, Status::Todo), id_of(&p, Status::Running), id_of(&p, Status::Review), id_of(&p, Status::Done));
    let press = |k: &mut Kit, p: &mut Agents, id: &str, c: char| {
        p.lead_focus = false;
        p.select(id);
        k.actions.clear();
        k.key(p, KeyCode::Char(c));
        (p.task(id).unwrap().priority, k.notices())
    };
    // waiting: changes, clamps
    assert_eq!(press(&mut k, &mut p, &todo, '+').0, 1);
    assert_eq!(press(&mut k, &mut p, &todo, '=').0, 2);
    let (v, n) = press(&mut k, &mut p, &todo, '+');
    assert_eq!((v, n), (2, vec!["priority: urgent".to_string()]));
    for _ in 0..4 {
        press(&mut k, &mut p, &todo, '-');
    }
    assert_eq!(p.task(&todo).unwrap().priority, -1, "clamps at low");
    let board = k.render_html(&mut p, 150, 44, &snap("priority-low"));
    assert!(board.contains("↓"), "a low card shows ↓:\n{board}");
    // finished: nothing
    let (v, n) = press(&mut k, &mut p, &done, '+');
    assert_eq!(v, 0, "a done card keeps its priority");
    assert!(n.is_empty(), "{n:?}");
    // in review (hand-made): it can go back to work with c, so nothing to object to either way
    press(&mut k, &mut p, &review, '+');
    // running in its own tab: nothing waits any more
    let (v, n) = press(&mut k, &mut p, &running, '+');
    assert!(v == 0 && !n.iter().any(|m| m.starts_with("priority:")), "a running hand-made card got priority {v} and said {n:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
