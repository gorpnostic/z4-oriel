//! Headless tests for review, plans you approve, templates and runnable worktrees: line comments and a second
//! opinion in the diff view, the lead's plan held for your approval, runs saved as templates and run again, and
//! a repo's .oriel/project.toml (setup, copy, run, ports). Git work happens in throwaway repos under
//! target/test-scratch; no real agent, reviewer or shell runs.

use super::lead_tests::{fake_worker, lead_pane, run0};
use super::tests::{id_of, offline, pane, scratch, sh, temp_repo, until, with_cx};
use super::*;
use crate::testkit::Kit;
use crossterm::event::KeyCode;
use serde_json::json;

/// A shell command for the gate shell (what setups and gates run in here): write "port=<ORIEL_PORT>" to `file`.
fn write_port(file: &str) -> String {
    match git::gate_shell().2 {
        "bash" | "sh" => format!("echo \"port=$ORIEL_PORT\" > {file}"),
        "PowerShell" => format!("\"port=$env:ORIEL_PORT\" | Out-File -Encoding ascii {file}"),
        _ => format!("echo port=%ORIEL_PORT%> {file}"),
    }
}

fn exists_cmd(file: &str) -> String {
    match git::gate_shell().2 {
        "bash" | "sh" => format!("test -f {file}"),
        "PowerShell" => format!("if (-not (Test-Path {file})) {{ exit 1 }}"),
        _ => format!("if exist {file} (exit /b 0) else (exit /b 1)"),
    }
}

/// A repo whose .oriel/project.toml (committed, as a team would) says how its checkouts are set up.
fn project_repo(dir: &Path, toml: &str) -> PathBuf {
    let repo = temp_repo(dir);
    std::fs::create_dir_all(repo.join(".oriel")).unwrap();
    std::fs::write(repo.join(".oriel").join("project.toml"), toml).unwrap();
    std::fs::write(repo.join(".gitignore"), ".env\nsetup.out\n").unwrap();
    sh(&repo, &["add", "-A"]);
    sh(&repo, &["commit", "-q", "-m", "project settings"]);
    std::fs::write(repo.join(".env"), "SECRET=1\n").unwrap();
    repo
}

fn toml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap() // a JSON string is a valid TOML basic string
}

/// A new worktree gets the files git doesn't track (copy) and the repo's setup (with its own ORIEL_PORT from the
/// range) before its agent starts; the card shows the port and T offers project.toml's `run`. A setup that fails
/// keeps the task from starting and says why (a run's task is blocked), and leaves no worktree behind.
#[test]
fn agents_worktrees_get_setup_copy_and_a_port() {
    let dir = scratch("wt-setup");
    let toml = format!("setup = {}\ncopy = [\".env\"]\nrun = \"serve --port $ORIEL_PORT\"\nports = \"46100-46109\"\n", toml_str(&write_port("setup.out")));
    let repo = project_repo(&dir, &toml);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo);
    k.render(&mut p, 150, 44);
    until(&mut k, &mut p, 5000, "repo", |p| p.repo.is_some() && p.projects.contains_key(&p.repo_key()));
    let id = p.add_task("Serve it", "x", 0, "");
    p.select(&id);
    k.key(&mut p, KeyCode::Enter);
    until(&mut k, &mut p, 20_000, "started", |p| !p.task(&id).unwrap().worktree.is_empty() || !p.task(&id).unwrap().error.is_empty());
    let t = p.task(&id).unwrap().clone();
    assert!(t.error.is_empty(), "{}", t.error);
    assert!((46100..=46109).contains(&t.port), "{}", t.port);
    let wt = PathBuf::from(&t.worktree);
    assert_eq!(std::fs::read_to_string(wt.join(".env")).unwrap(), "SECRET=1\n", "copied over");
    let out = std::fs::read_to_string(wt.join("setup.out")).unwrap();
    assert!(out.contains(&format!("port={}", t.port)), "the setup ran there with its port: {out}");
    k.actions.clear();
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains(&format!(":{}", t.port)), "the card shows its port: {s}");
    // T: project.toml's run, its port filled in when it starts
    k.key(&mut p, KeyCode::Char('T'));
    assert!(matches!(&p.mode, Mode::Try(v) if v.input.text == "serve --port $ORIEL_PORT" && v.port == t.port && v.setup.is_empty()));
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains(&format!("ORIEL_PORT {}", t.port)), "{s}");
    k.key(&mut p, KeyCode::Esc);
    // a second checkout gets another port
    let two = p.add_task("Another", "x", 0, "");
    p.select(&two);
    k.key(&mut p, KeyCode::Enter);
    until(&mut k, &mut p, 20_000, "started", |p| !p.task(&two).unwrap().worktree.is_empty() || !p.task(&two).unwrap().error.is_empty());
    let port2 = p.task(&two).unwrap().port;
    assert!(port2 != t.port && (46100..=46109).contains(&port2), "{port2}");
    k.actions.clear();

    // a setup that fails: the task doesn't start, the card says why, no worktree is left
    std::fs::write(repo.join(".oriel").join("project.toml"), "setup = \"exit 3\"\n").unwrap();
    sh(&repo, &["commit", "-q", "-am", "broken setup"]);
    let bad = p.add_task("Broken setup", "x", 0, "");
    p.select(&bad);
    k.key(&mut p, KeyCode::Enter);
    until(&mut k, &mut p, 20_000, "refused", |p| !p.task(&bad).unwrap().error.is_empty());
    let t = p.task(&bad).unwrap().clone();
    assert!(t.status == Status::Todo && t.worktree.is_empty() && t.error.starts_with("setup `exit 3` failed"), "{t:?}");
    assert!(k.notices().iter().any(|n| n.starts_with("couldn't start: setup `exit 3` failed")), "{:?}", k.notices());
    let slug = store::slug(&t.title, &t.id);
    assert!(!p.paths.wt.join("repo").join(&slug).exists(), "the worktree went with it");
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains("✗ setup `exit 3` failed"), "the card says why: {s}");
    // a broken project.toml is said, and ignored
    std::fs::write(repo.join(".oriel").join("project.toml"), "setup = [nope").unwrap();
    with_cx(&mut k, |cx| p.on_msg(Msg::Project(p.repo_key(), project::load(&repo)), cx));
    assert!(k.notices().last().is_some_and(|n| n.contains(".oriel/project.toml") && n.contains("ignored")), "{:?}", k.notices());
    let _ = std::fs::remove_dir_all(&dir);
}

/// In a run of your tasks, a setup that fails blocks that task (its worker never starts) and the run settles
/// without it; the gate's own checkout gets the setup too, so a gate that needs it passes.
#[test]
fn agents_run_setup_blocks_and_the_gate_checkout_is_set_up() {
    let dir = scratch("wt-setup-run");
    let toml = format!("setup = {}\n", toml_str(&write_port("setup.out")));
    let repo = project_repo(&dir, &toml);
    let mut k = Kit::new();
    let mut p = lead_pane(&dir, &repo, fake_worker(Arc::default()), fake_worker(Arc::default()));
    p.lead_cfg.gate = exists_cmd("setup.out");
    k.render(&mut p, 150, 44);
    until(&mut k, &mut p, 5000, "repo", |p| p.repo.is_some());
    let a = p.add_task("Add a", "write a2.txt: aaa", 0, "");
    super::lead_tests::run_batch(&mut k, &mut p, &[a.clone()], false);
    until(&mut k, &mut p, 60_000, "merged", |p| p.store.runs[0].state == store::RunState::Review);
    assert_eq!(run0(&p).merged, 1, "the gate found setup.out in its checkout: {:?}", p.task(&a));
    // now the setup breaks: the next run's task is blocked and the run settles
    std::fs::write(repo.join(".oriel").join("project.toml"), "setup = \"exit 4\"\n").unwrap();
    sh(&repo, &["commit", "-q", "-am", "broken setup"]);
    let b = p.add_task("Add b", "write b2.txt: bee", 0, "");
    super::lead_tests::run_batch(&mut k, &mut p, &[b.clone()], false);
    until(&mut k, &mut p, 60_000, "settled", |p| p.store.runs.len() == 2 && p.store.runs[1].state == store::RunState::Review);
    let t = p.task(&b).unwrap().clone();
    assert!(t.blocked && t.questions[0].contains("project.toml") && t.error.starts_with("setup `exit 4` failed"), "{t:?}");
    assert!(p.store.runs[1].summary.contains("1 blocked"), "{}", p.store.runs[1].summary);
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ review: line comments, second opinions

/// A hand task in REVIEW with a.txt's line two changed and a new b.txt, its diff view open and loaded.
fn reviewed_task(dir: &Path) -> (Kit, Agents, String, PathBuf) {
    let repo = temp_repo(dir);
    let mut k = Kit::new();
    let mut p = pane(dir, &repo);
    k.render(&mut p, 150, 44);
    until(&mut k, &mut p, 5000, "repo", |p| p.repo.is_some());
    let id = p.add_task("Shout line two", "x", 0, "");
    p.select(&id);
    k.key(&mut p, KeyCode::Enter);
    until(&mut k, &mut p, 10_000, "started", |p| !p.task(&id).unwrap().worktree.is_empty());
    let wt = PathBuf::from(&p.task(&id).unwrap().worktree);
    std::fs::write(wt.join("a.txt"), "one\nTWO\nthree\nfour\n").unwrap();
    std::fs::write(wt.join("b.txt"), "new file\n").unwrap();
    with_cx(&mut k, |cx| p.to_review(&id, "ready for review", cx));
    k.actions.clear();
    p.select(&id);
    k.key(&mut p, KeyCode::Char('d'));
    until(&mut k, &mut p, 10_000, "diff", |p| matches!(&p.mode, Mode::Diff(v) if v.data.is_some()));
    (k, p, id, wt)
}

fn cursor_line(p: &Agents) -> Option<(String, git::Kind, Option<u32>)> {
    let Mode::Diff(v) = &p.mode else { return None };
    v.at().map(|(f, l)| (f.path.clone(), l.kind.clone(), l.new))
}

/// The diff view has a line cursor: c comments on the line you're on (anchored to path:line, marked in the
/// gutter and listed under the files), space ticks and unticks, del removes, n jumps between them, and enter sends
/// the ticked ones with anything you add as one message through the task's usual follow-up. Esc in that box goes
/// back to the diff with nothing lost.
#[test]
fn agents_diff_line_comments_go_out_as_one_message() {
    let dir = scratch("line-comments");
    let (mut k, mut p, id, _wt) = reviewed_task(&dir);
    assert_eq!(cursor_line(&p), Some(("a.txt".into(), git::Kind::Ctx, Some(1))), "the cursor starts on the first code line");
    k.key(&mut p, KeyCode::Char('j'));
    assert_eq!(cursor_line(&p).map(|c| c.1), Some(git::Kind::Del));
    k.key(&mut p, KeyCode::Down);
    assert_eq!(cursor_line(&p), Some(("a.txt".into(), git::Kind::Add, Some(2))));
    k.key(&mut p, KeyCode::Char('c'));
    k.typ(&mut p, "use a constant");
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains("comment on a.txt:2") && s.contains("use a constant"), "{s}");
    k.key(&mut p, KeyCode::Enter);
    assert_eq!(p.notes_of(&id).len(), 1);
    let n = &p.notes_of(&id)[0];
    assert_eq!((n.path.as_str(), n.line, n.old, n.code.as_str(), n.on), ("a.txt", 2, false, "TWO", true));
    // the next file, and a comment on its first line
    k.key(&mut p, KeyCode::Right);
    assert_eq!(cursor_line(&p).map(|c| (c.0, c.2)), Some(("b.txt".into(), Some(1))));
    k.key(&mut p, KeyCode::Char('c'));
    k.typ(&mut p, "needs a header");
    k.key(&mut p, KeyCode::Enter);
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains("2 to send") && s.contains("enter send 2") && s.contains("needs a header"), "{s}");
    // space unticks what's on this line (and ticks it again); n goes back to the other one
    k.key(&mut p, KeyCode::Char(' '));
    assert_eq!(p.ticked(&id), 1);
    assert!(k.render(&mut p, 150, 44).contains("1 to send"));
    k.key(&mut p, KeyCode::Char(' '));
    assert_eq!(p.ticked(&id), 2);
    k.key(&mut p, KeyCode::Char('n'));
    assert_eq!(cursor_line(&p), Some(("a.txt".into(), git::Kind::Add, Some(2))));
    // a third, removed again with del
    k.key(&mut p, KeyCode::Char('k'));
    k.key(&mut p, KeyCode::Char('c'));
    k.typ(&mut p, "never mind");
    k.key(&mut p, KeyCode::Enter);
    assert_eq!(p.notes_of(&id).len(), 3);
    assert!(p.notes_of(&id)[2].old, "a removed line is anchored to its old number");
    k.key(&mut p, KeyCode::Delete);
    assert_eq!(p.notes_of(&id).len(), 2);
    // enter: the box lists them; esc goes back to the diff, comments kept
    k.key(&mut p, KeyCode::Enter);
    assert!(matches!(&p.mode, Mode::Comment(i, _) if *i == id));
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains("with 2 comments:") && s.contains("a.txt:2: use a constant"), "{s}");
    k.key(&mut p, KeyCode::Esc);
    until(&mut k, &mut p, 10_000, "back in the diff", |p| matches!(&p.mode, Mode::Diff(v) if v.data.is_some()));
    assert_eq!(p.ticked(&id), 2);
    // and sent: the agent's session carries on with them
    k.key(&mut p, KeyCode::Enter);
    k.typ(&mut p, "also rename it");
    k.actions.clear();
    k.key(&mut p, KeyCode::Enter);
    let t = p.task(&id).unwrap().clone();
    assert!(t.status == Status::Running && t.followups == 1 && t.last == "comment: also rename it", "{t:?}");
    assert!(p.notes_of(&id).is_empty(), "sent, so gone");
    assert!(k.actions.iter().any(|a| matches!(a, Action::OpenTagged { tag, .. } if *tag == t.tag())), "its tab carries on");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The message the ticked comments make: what you typed, then each as path:line: text with the line quoted.
#[test]
fn agents_review_message_format() {
    let note = |path: &str, line: u32, old: bool, code: &str, text: &str, finding: Option<bool>, on: bool| review::Note { path: path.into(), line, old, code: code.into(), text: text.into(), finding, on };
    let notes = vec![
        note("src/app.rs", 42, false, "    let c = Color::Red;", "use the theme's colour", None, true),
        note("src/ui.rs", 7, true, "old()", "why was this removed?", None, true),
        note("src/x.rs", 3, false, "x", "unticked", Some(false), false),
        note("src/y.rs", 9, false, "y()", "panics on empty input", Some(true), true),
        note("README.md", 0, false, "", "say how to run it", None, true),
    ];
    let m = review::message(&notes, "  also add a test ");
    assert!(m.starts_with("also add a test\n\nReview comments on your changes"), "{m}");
    assert!(m.contains("- src/app.rs:42: use the theme's colour\n    > let c = Color::Red;\n"), "{m}");
    assert!(m.contains("- src/ui.rs:7 (a removed line): why was this removed?\n    > old()\n"), "{m}");
    assert!(m.contains("- src/y.rs:9: [blocking] panics on empty input"), "{m}");
    assert!(m.contains("- README.md: say how to run it\n") && !m.contains("unticked"), "{m}");
    assert_eq!(review::message(&[], " just this "), "just this");
    assert_eq!(review::message(&notes[2..3], ""), "", "nothing ticked, nothing typed: nothing to send");
}

/// A fake reviewer: blocking on a.txt:2, optional on b.txt:1 (or no findings list at all, `broken`). It records
/// how it was run.
fn fake_reviewer(broken: bool, seen: Arc<Mutex<Vec<run::Spec>>>) -> run::Fake {
    Arc::new(move |spec: &run::Spec, _stop: &AtomicBool, _on: &mut dyn FnMut(stream::Ev)| -> run::Outcome {
        seen.lock().unwrap().push(spec.clone());
        if broken {
            return run::Outcome { text: "looks fine to me".into(), cost: 0.01, ..Default::default() };
        }
        let report = json!({"blocking": [{"path": "a.txt", "line": 2, "why": "TWO is shouting", "repro": "cat a.txt"}], "optional": [{"path": "b.txt", "line": 1, "why": "could say more", "repro": ""}]});
        run::Outcome { report: Some(report), text: "found two things".into(), cost: 0.12, ..Default::default() }
    })
}

/// V: another vendor's AI reviews the diff (codex for Claude's work), read-only; its findings join the list,
/// blocking ones ticked and optional ones not, and only what's ticked is sent. A new opinion replaces the last
/// one's findings but keeps your comments; a reviewer that answers without a list is said to have failed.
#[test]
fn agents_second_opinion_findings_join_the_comments() {
    let dir = scratch("second-opinion");
    let (mut k, mut p, id, _wt) = reviewed_task(&dir);
    let seen: Arc<Mutex<Vec<run::Spec>>> = Arc::default();
    p.fake_reviewer = Some(fake_reviewer(false, seen.clone()));
    // one comment of yours first
    k.key(&mut p, KeyCode::Char('c'));
    k.typ(&mut p, "mine");
    k.key(&mut p, KeyCode::Enter);
    k.key(&mut p, KeyCode::Char('V'));
    assert!(p.reviewing.contains_key(&id));
    assert!(k.render(&mut p, 150, 44).contains("is reviewing"));
    until(&mut k, &mut p, 10_000, "reviewed", |p| !p.reviewing.contains_key(&id));
    let spec = seen.lock().unwrap()[0].clone();
    assert_eq!(spec.agent, "codex", "claude's work gets codex's opinion");
    assert!(spec.args.windows(2).any(|w| w == ["--sandbox", "read-only"]) && spec.args.iter().any(|a| a == "--output-schema"), "{:?}", spec.args);
    assert!(spec.prompt.contains("+TWO") && spec.prompt.contains("b.txt") && spec.prompt.contains("Shout line two"), "{}", spec.prompt);
    let notes = p.notes_of(&id).to_vec();
    assert_eq!(notes.len(), 3);
    let blocking = notes.iter().find(|n| n.finding == Some(true)).unwrap();
    assert!(blocking.on && blocking.path == "a.txt" && blocking.line == 2 && blocking.code == "TWO" && blocking.text.contains("(repro: cat a.txt)"), "{blocking:?}");
    assert!(notes.iter().any(|n| n.finding == Some(false) && !n.on), "optional ones start unticked");
    assert!((p.task(&id).unwrap().spent_before - 0.12).abs() < 1e-9, "its cost counts today");
    assert!(k.notices().iter().any(|n| n.contains("1 blocking, 1 optional")), "{:?}", k.notices());
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains("blocking · ") && s.contains("optional · ") && s.contains("2 to send"), "{s}");
    // a second opinion again: its findings replace the first's, your comment stays
    k.key(&mut p, KeyCode::Char('V'));
    until(&mut k, &mut p, 10_000, "reviewed again", |p| !p.reviewing.contains_key(&id));
    assert_eq!(p.notes_of(&id).len(), 3);
    assert_eq!(p.notes_of(&id).iter().filter(|n| n.finding.is_none()).count(), 1);
    // enter sends the ticked ones: yours and the blocking finding
    k.key(&mut p, KeyCode::Enter);
    assert!(k.render(&mut p, 150, 44).contains("with 2 comments:"));
    k.key(&mut p, KeyCode::Enter);
    let t = p.task(&id).unwrap().clone();
    assert!(t.status == Status::Running && t.last == "comment: 2 line comments", "{t:?}");
    // a reviewer with no findings list failed
    p.fake_reviewer = Some(fake_reviewer(true, seen.clone()));
    with_cx(&mut k, |cx| p.to_review(&id, "ready", cx));
    with_cx(&mut k, |cx| p.second_opinion(&id, cx));
    until(&mut k, &mut p, 10_000, "failed", |p| !p.reviewing.contains_key(&id));
    assert!(k.notices().last().is_some_and(|n| n.contains("second opinion (codex) failed")), "{:?}", k.notices());
    // who reviews whom
    assert_eq!(p.reviewer_for("codex").map(|r| r.0).as_deref(), Some("claude"));
    assert_eq!(p.reviewer_for("kimi").map(|r| r.0).as_deref(), Some("claude"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The reviewer's command lines: read-only everywhere, findings as structured output.
#[test]
fn agents_review_command_lines() {
    let tmp = scratch("review-args");
    let wt = Path::new("/w/t");
    let c = run::review_spec("claude", Path::new("claude"), "sonnet", "look", wt, &tmp, 1.0);
    assert!(c.args.windows(2).any(|w| w == ["--allowedTools", "Read,Grep,Glob,LS"]) && c.args.iter().any(|a| a.starts_with("Edit,Write")), "{:?}", c.args);
    assert!(c.args.windows(2).any(|w| w[0] == "--json-schema" && w[1].contains("blocking")) && c.args.windows(2).any(|w| w == ["--max-budget-usd", "1.00"]));
    assert_eq!(c.stdin.as_deref(), Some("look"));
    let x = run::review_spec("codex", Path::new("codex"), "", "look", wt, &tmp, 1.0);
    assert!(x.args.windows(2).any(|w| w == ["--sandbox", "read-only"]) && x.budget_usd == 1.0);
    assert!(std::fs::read_to_string(&x.cleanup[0]).unwrap().contains("optional"));
    let k = run::review_spec("kimi", Path::new("kimi"), "", "look", wt, &tmp, 1.0);
    assert!(k.args.iter().any(|a| a.contains("```json") && a.starts_with("look")));
    // findings from structured output or the end of an answer
    let f = review::parse_findings(&json!({"blocking": [{"path": "./src\\a.rs", "line": "12", "why": "boom"}], "optional": []})).unwrap();
    assert_eq!(f, vec![review::Finding { path: "src/a.rs".into(), line: 12, why: "boom".into(), repro: String::new(), blocking: true }]);
    assert!(review::parse_findings(&json!({"summary": "x"})).is_none());
    let _ = std::fs::remove_dir_all(&tmp);
}

// ------------------------------------------------------------------ approve the plan first

/// "approve the plan first" on the lead form: the lead's checked plan waits as cards (no worker starts, the
/// lead's wait doesn't return early); x drops one, e edits another's acceptance, a starts the rest, and the lead
/// hears plan_approved with what you dropped and edited. The toggle is remembered for the next run.
#[test]
fn agents_lead_plan_waits_for_your_approval() {
    let dir = scratch("approve-plan");
    let repo = temp_repo(&dir);
    let prompts: Arc<Mutex<Vec<String>>> = Arc::default();
    let plan = json!([
        {"tool": "roster"},
        {"tool": "plan", "args": {"tasks": [
            {"id": "x", "title": "Add x", "goal": "write x.txt: ex", "worker": "w1", "owns": ["x.txt"], "size": "S"},
            {"id": "y", "title": "Add y", "goal": "write y.txt: why", "worker": "w2", "owns": ["y.txt"], "size": "S"},
            {"id": "z", "title": "Add z", "goal": "write z.txt: zed", "worker": "w1", "owns": ["z.txt"], "size": "S"}
        ]}},
        {"tool": "wait", "args": {"timeout_s": 20}}
    ]);
    let mut k = Kit::new();
    let mut p = lead_pane(&dir, &repo, fake_worker(Arc::default()), super::lead_tests::fake_text_lead(plan, 2, prompts.clone()));
    k.render(&mut p, 150, 44);
    until(&mut k, &mut p, 5000, "repo", |p| p.repo.is_some());
    k.key(&mut p, KeyCode::Char('L'));
    k.typ(&mut p, "Three files");
    for _ in 0..6 {
        k.key(&mut p, KeyCode::Tab);
    }
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains("[ ] approve the plan first"), "{s}");
    k.key(&mut p, KeyCode::Char(' '));
    assert!(matches!(&p.mode, Mode::LeadForm(f) if f.approve));
    k.key_mod(&mut p, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert!(run0(&p).approve && p.store.approve_plan, "the run approves plans, and the form remembers it");
    until(&mut k, &mut p, 30_000, "the plan is held", |p| p.store.runs[0].held.len() == 3);
    assert!(p.store.tasks.is_empty(), "nothing starts before you approve");
    assert!(k.notices().iter().any(|n| n.contains("the lead's plan is ready: 3 tasks")), "{:?}", k.notices());
    // the lead's wait doesn't come back with "nothing left" while the plan waits
    std::thread::sleep(Duration::from_millis(300));
    k.poll(&mut p);
    assert!(!p.waiters.is_empty(), "the lead is still waiting");
    // the cards
    p.lead_focus = true;
    assert!(k.render(&mut p, 150, 44).contains("enter review the plan"));
    k.key(&mut p, KeyCode::Enter);
    assert!(matches!(p.mode, Mode::PlanReview(_)));
    let s = k.render_html(&mut p, 150, 44, "target/snap/agents-plan-review.html");
    assert!(s.contains("the lead's plan") && s.contains("x · Add x") && s.contains("owns y.txt") && s.contains("3 of 3 tasks"), "{s}");
    // x drops the first; e edits the second's acceptance
    k.key(&mut p, KeyCode::Char('x'));
    assert_eq!(run0(&p).dropped, vec!["x".to_string()]);
    assert!(k.render(&mut p, 150, 44).contains("dropped"));
    k.key(&mut p, KeyCode::Char('j'));
    k.key(&mut p, KeyCode::Char('e'));
    assert!(matches!(&p.mode, Mode::PlanReview(v) if v.edit.is_some()));
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains("edit task y") && s.contains("write y.txt: why"), "{s}");
    k.key(&mut p, KeyCode::Tab);
    k.key(&mut p, KeyCode::Tab);
    k.typ(&mut p, &super::lead_tests::accept_cmd("y.txt"));
    k.key_mod(&mut p, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert_eq!(run0(&p).edited, vec!["y".to_string()]);
    assert!(k.render(&mut p, 150, 44).contains("edited"));
    // a: the rest goes to work
    k.key(&mut p, KeyCode::Char('a'));
    assert!(matches!(p.mode, Mode::Board));
    assert!(run0(&p).held.is_empty());
    assert_eq!(p.store.tasks.len(), 2);
    assert_eq!(super::lead_tests::by_key(&p, "y").acceptance, super::lead_tests::accept_cmd("y.txt"));
    until(&mut k, &mut p, 60_000, "done", |p| p.store.runs[0].state == store::RunState::Review);
    let r = run0(&p);
    assert_eq!(r.merged, 2);
    assert!(sh(&repo, &["show", &format!("{}:y.txt", r.branch)]) == "why" && git::run(&repo, &["show", &format!("{}:x.txt", r.branch)]).stdout.is_empty());
    let heard = prompts.lock().unwrap().join("\n");
    assert!(heard.contains("plan_approved") && heard.contains("\"dropped\":[\"Add x\"]") && heard.contains("\"edited_by_the_user\":[\"Add y\"]"), "{heard}");
    // the next form starts with it on
    p.lead_focus = false;
    k.key(&mut p, KeyCode::Char('L'));
    assert!(matches!(&p.mode, Mode::LeadForm(f) if f.approve));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A plan you send back with c: nothing starts, the lead hears plan_rejected with your note; a second plan while
/// one waits is refused; a kept task that waits for one you dropped can't be approved; approving with nothing
/// left is refused too.
#[test]
fn agents_plan_sent_back_and_what_approval_refuses() {
    let dir = scratch("approve-reject");
    let mut k = Kit::new();
    let (mut p, run) = {
        let mut p = Agents::with_paths(Paths { agents: dir.join("agents"), wt: dir.join("wt") });
        p.booted = true;
        p.fake_worker = Some(fake_worker(Arc::default()));
        p.roster = super::lead_tests::roster();
        p.store.runs.push(store::Run { id: "r1".into(), repo: dir.join("nope").display().to_string(), goal: "g".into(), agent: "claude".into(), state: store::RunState::Running, branch: "oriel/lead-g".into(), max_parallel: 2, approve: true, created: store::now(), ..Default::default() });
        (p, "r1".to_string())
    };
    p.runs_live.insert(run.clone(), lead::RunLive::new());
    let call = |p: &mut Agents, k: &mut Kit, args: serde_json::Value| -> Result<String, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        with_cx(k, |cx| p.on_call(mcp::Call { run: "r1".into(), tool: "plan".into(), args, reply: tx }, cx));
        rx.recv().unwrap()
    };
    let tasks = json!({"tasks": [
        {"id": "a", "title": "Scaffold", "goal": "g", "worker": "w1", "owns": ["Cargo.toml"], "size": "S"},
        {"id": "b", "title": "Feature", "goal": "g", "worker": "w2", "owns": ["src/b.rs"], "size": "S"},
        {"id": "c", "title": "Docs", "goal": "g", "worker": "w1", "owns": ["docs/**"], "size": "S"}
    ]});
    let r = call(&mut p, &mut k, tasks.clone()).unwrap();
    assert!(r.contains("\"held\":true") && r.contains("Call wait"), "{r}");
    assert_eq!(p.run_ref(&run).unwrap().held.len(), 3);
    assert!(p.run_ref(&run).unwrap().held_notes.iter().any(|n| n.contains("hotspot")), "plan.rs's notes are kept");
    assert!(call(&mut p, &mut k, tasks.clone()).unwrap_err().contains("already waiting"));
    // dropping the scaffold the others wait for: approval says why not
    p.open_plan_review(&run);
    k.key(&mut p, KeyCode::Char('x'));
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains("waits for \"Scaffold\", which you dropped"), "{s}");
    k.key(&mut p, KeyCode::Char('a'));
    assert!(matches!(p.mode, Mode::PlanReview(_)) && p.store.tasks.is_empty());
    assert!(k.notices().last().is_some_and(|n| n.starts_with("not yet:")), "{:?}", k.notices());
    for _ in 0..2 {
        k.key(&mut p, KeyCode::Char('j'));
        k.key(&mut p, KeyCode::Char('x'));
    }
    assert!(k.render(&mut p, 150, 44).contains("you dropped every task"));
    // c: back to the lead with a note
    k.key(&mut p, KeyCode::Char('c'));
    k.typ(&mut p, "one task is enough");
    k.key(&mut p, KeyCode::Enter);
    assert!(matches!(p.mode, Mode::Board));
    let r1 = p.run_ref(&run).unwrap().clone();
    assert!(r1.held.is_empty() && r1.dropped.is_empty() && p.store.tasks.is_empty());
    let ev = p.runs_live[&run].events.last().unwrap();
    assert!(ev.task.is_empty() && ev.card["event"] == "plan_rejected" && ev.card["note"] == "one task is enough", "{:?}", ev.card);
    // a wait for particular tasks still hears about the whole run
    let (tx, rx) = std::sync::mpsc::channel();
    p.waiters.push(lead::Waiter { run: run.clone(), ids: vec!["zz".into()], deadline: Instant::now() + Duration::from_secs(5), reply: tx });
    p.check_waiters();
    assert!(rx.try_recv().unwrap().unwrap().contains("plan_rejected"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// P's cards own files and say how to check them, and go through plan.rs: overlapping files send the plan back
/// to the planner once with what's wrong; a hotspot makes one scaffold card the others come after. On the board
/// the cards keep owns, acceptance and those links (their tab's first prompt says both); cards that still don't
/// check out arrive unmarked.
#[test]
fn agents_planner_cards_own_files_and_are_checked() {
    let item = |key: &str, owns: &[&str], deps: &[&str]| plan::Item { key: key.into(), title: format!("Task {key}"), goal: format!("do {key}"), owns: owns.iter().map(|s| s.to_string()).collect(), depends_on: deps.iter().map(|s| s.to_string()).collect(), acceptance: format!("check {key}"), ..Default::default() };
    let names = vec!["cheap".to_string(), "mid".to_string()];
    // the first answer overlaps; told why, the second doesn't
    let asked: Mutex<Vec<String>> = Mutex::default();
    let planner = |fb: &str| -> Result<(Vec<plan::Item>, f64), String> {
        asked.lock().unwrap().push(fb.to_string());
        Ok(if fb.is_empty() { (vec![item("a", &["src/**"], &[]), item("b", &["src/b.rs"], &[]), item("c", &["docs/x.md"], &[])], 0.10) } else { (vec![item("a", &["src/a.rs"], &[]), item("b", &["src/b.rs"], &[]), item("c", &["docs/x.md"], &[])], 0.05) })
    };
    let out = plan_checked_with(&planner, &names).unwrap();
    let fb = asked.lock().unwrap()[1].clone();
    assert!(fb.contains("rejected") && fb.contains("both own") && fb.contains("\"src/**\""), "{fb}");
    assert!(out.problems.is_empty() && (out.cost - 0.15).abs() < 1e-9);
    assert_eq!(out.items.iter().map(|i| i.worker.as_str()).collect::<Vec<_>>(), vec!["cheap", "mid", "cheap"], "spread over the names in turn");
    // a hotspot: the card that owns it goes first and the others wait for it
    let planner = |_: &str| Ok((vec![item("ui", &["src/ui.rs"], &[]), item("deps", &["Cargo.toml"], &[]), item("doc", &["README.md"], &[])], 0.0));
    let out = plan_checked_with(&planner, &names).unwrap();
    assert_eq!(out.items[0].key, "deps");
    assert!(out.items[1..].iter().all(|i| i.depends_on == vec!["deps".to_string()]) && out.notes[0].contains("hotspot"), "{:?}", out.notes);
    // still overlapping after the second go: the cards come with the problem
    let planner = |_: &str| Ok((vec![item("a", &["src/**"], &[]), item("b", &["src/b.rs"], &[]), item("c", &["x"], &[])], 0.0));
    let bad = plan_checked_with(&planner, &names).unwrap();
    assert!(!bad.problems.is_empty() && bad.items.len() == 3);
    // on the board
    let dir = scratch("planner-cards");
    let mut k = Kit::new();
    let mut p = offline(&dir);
    p.store.tasks.clear();
    let here = p.repo_key();
    with_cx(&mut k, |cx| p.on_msg(Msg::Plan(here.clone(), Ok(out)), cx));
    let deps = id_of(&p, "Task deps");
    let ui = p.task(&id_of(&p, "Task ui")).unwrap().clone();
    assert_eq!((ui.owns.clone(), ui.acceptance.as_str(), ui.depends_on.clone()), (vec!["src/ui.rs".to_string()], "check ui", vec![deps.clone()]));
    assert_eq!(p.marked.len(), 3, "checked cards are marked to run together");
    assert!(k.notices().last().is_some_and(|n| n.contains("hotspot")), "plan.rs's note comes along: {:?}", k.notices());
    let prompt = hand_prompt(&ui);
    assert!(prompt.starts_with("do ui\n\nOnly edit files matching: src/ui.rs.") && prompt.ends_with("\n\nWhen you're done, run `check ui` from the repo root and make it pass."), "{prompt}");
    p.marked.clear();
    with_cx(&mut k, |cx| p.on_msg(Msg::Plan(here.clone(), Ok(bad)), cx));
    assert!(p.marked.is_empty(), "cards that don't check out aren't marked");
    assert!(k.notices().last().is_some_and(|n| n.contains("don't check out")), "{:?}", k.notices());
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ snapshots

/// The demo board's diff view with comments and a second opinion, and a plan waiting for approval (for
/// eyeballing: target/snap/agents-*.html). Small panes don't panic.
#[test]
fn agents_snapshots_review_and_plan() {
    let dir = scratch("snap-review");
    let mut k = Kit::new();
    let mut p = super::tests::demo(&dir);
    let id = id_of(&p, "Usage sink status line");
    p.task_mut(&id).unwrap().worktree = dir.display().to_string();
    let diff = "diff --git a/src/usage.rs b/src/usage.rs\n--- a/src/usage.rs\n+++ b/src/usage.rs\n@@ -10,6 +10,9 @@ pub fn sink() {\n     let v = read();\n-    let total = v.len();\n+    let total = v.iter().map(|r| r.tokens).sum::<u64>();\n+    let line = format!(\"{total} tok\");\n+    status(&line);\n     Ok(())\n }\n";
    let files = git::parse_diff(diff);
    let mut v = DiffView { data: Some(Ok(git::Diff { files, conflicts: Some(vec![]), target: "master".into() })), ..DiffView::new(&id) };
    v.set_file(0);
    v.step(2);
    p.mode = Mode::Diff(v);
    let note = |line: u32, text: &str, finding: Option<bool>, on: bool| review::Note { path: "src/usage.rs".into(), line, old: false, code: String::new(), text: text.into(), finding, on };
    p.notes.insert(id.clone(), vec![note(11, "sum what the records say, not how many there are", None, true), note(12, "an empty history shows \"0 tok\": say \"no usage yet\"", Some(true), true), note(13, "status() could take the number", Some(false), false)]);
    p.reviewing.insert(id.clone(), "codex".into());
    let s = k.render_html(&mut p, 150, 44, "target/snap/agents-review.html");
    println!("{s}");
    assert!(s.contains("3 comments") || s.contains("comments"), "{s}");
    assert!(s.contains("blocking · ") && s.contains("codex is reviewing"), "{s}");
    if let Mode::Diff(v) = &mut p.mode {
        v.typing = Some(Input::new("this should be a constant", false));
    }
    let s = k.render_html(&mut p, 150, 44, "target/snap/agents-review-typing.html");
    println!("{s}");
    for (w, h) in [(60, 16), (30, 9), (20, 8)] {
        k.render(&mut p, w, h);
    }
    // the compose box with the ticked ones
    if let Mode::Diff(v) = &mut p.mode {
        v.typing = None;
    }
    p.mode = Mode::Comment(id.clone(), Input::new("", true));
    let s = k.render_html(&mut p, 150, 44, "target/snap/agents-review-send.html");
    println!("{s}");
    assert!(s.contains("with 2 comments:"), "{s}");
    for (w, h) in [(60, 16), (30, 9), (20, 8)] {
        k.render(&mut p, w, h);
    }
    // a plan waiting for approval
    let here = p.repo_key();
    let it = |key: &str, title: &str, worker: &str, owns: &[&str], deps: &[&str], acc: &str| plan::Item { key: key.into(), title: title.into(), goal: format!("{title}: the whole brief a worker gets, with what to change and where, and how to check it."), worker: worker.into(), owns: owns.iter().map(|s| s.to_string()).collect(), depends_on: deps.iter().map(|s| s.to_string()).collect(), acceptance: acc.into(), size: "S".into(), ..Default::default() };
    p.store.runs.push(store::Run {
        id: "r1".into(),
        repo: here,
        goal: "Show usage in the status line".into(),
        agent: "claude".into(),
        state: store::RunState::Running,
        branch: "oriel/lead-usage-r1".into(),
        approve: true,
        held: vec![it("deps", "Add the tokens crate", "claude-haiku", &["Cargo.toml"], &[], "cargo check"), it("sink", "Sum tokens in the sink", "codex", &["src/usage.rs"], &["deps"], "cargo test usage"), it("ui", "Status line segment", "claude-sonnet", &["src/ui.rs"], &["deps"], "")],
        held_notes: vec!["\"Add the tokens crate\" owns hotspot files (Cargo.toml), so it merges first: Sum tokens in the sink, Status line segment wait for it".into()],
        dropped: vec!["ui".into()],
        edited: vec!["sink".into()],
        created: store::now(),
        ..Default::default()
    });
    p.mode = Mode::Board;
    p.lead_focus = true;
    let s = k.render_html(&mut p, 150, 44, "target/snap/agents-plan-panel.html");
    println!("{s}");
    assert!(s.contains("enter review the plan"), "{s}");
    p.open_plan_review("r1");
    let s = k.render_html(&mut p, 150, 44, "target/snap/agents-plan-cards.html");
    println!("{s}");
    assert!(s.contains("Add the tokens crate") && s.contains("dropped"), "{s}");
    for (w, h) in [(60, 16), (30, 9), (20, 8)] {
        k.render(&mut p, w, h);
    }
    k.key(&mut p, KeyCode::Char('j'));
    k.key(&mut p, KeyCode::Char('e'));
    let s = k.render_html(&mut p, 150, 44, "target/snap/agents-plan-edit.html");
    println!("{s}");
    for (w, h) in [(60, 16), (30, 9), (20, 8)] {
        k.render(&mut p, w, h);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ templates

/// S on a run of your tasks writes them to .oriel/templates/<name>.toml (discarded ones left out, their order
/// kept as `after` keys); e edits the file in the list (a broken one isn't saved); W → enter asks for the {{args}}
/// you put in, drops the tasks into TODO with them filled in and opens the run popup. A lead run that's still
/// going can't be saved yet.
#[test]
fn agents_run_saved_as_template_and_run_again() {
    let dir = scratch("templates");
    let mut k = Kit::new();
    let mut p = offline(&dir);
    p.store.tasks.clear();
    let here = p.repo_key();
    let a = p.add_task("Fix the parser", "Fix the parser\nso it keeps comments", 0, "sonnet");
    let b = p.add_task("Test the parser", "add a regression test", 1, "");
    let c = p.add_task("Thrown away", "x", 0, "");
    for (id, st, out) in [(&a, Status::Done, "merged"), (&b, Status::Done, "merged"), (&c, Status::Done, "discarded")] {
        let t = p.task_mut(id).unwrap();
        (t.status, t.outcome) = (st, out.to_string());
    }
    if let Some(t) = p.task_mut(&a) {
        (t.priority, t.budget_usd, t.acceptance, t.owns) = (1, 1.5, "cargo test parser".into(), vec!["src/parse/**".into()]);
    }
    p.task_mut(&b).unwrap().depends_on = vec![a.clone()];
    p.store.runs.push(store::Run { id: "r1".into(), repo: here.clone(), goal: "together, 3 tasks".into(), agent: "you".into(), state: store::RunState::Review, manual: true, batch: vec![a.clone(), b.clone(), c.clone()], branch: "oriel/batch-x".into(), created: store::now(), ..Default::default() });
    p.lead_focus = true;
    assert!(p.board_hints().contains(&("S", "save as template")));
    k.key(&mut p, KeyCode::Char('S'));
    assert!(matches!(&p.mode, Mode::SaveTemplate(v) if v.name.text == "fix-the-parser"), "named after its first task");
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains("save the run as a template") && s.contains(".oriel/templates/fix-the-parser.toml"), "{s}");
    k.key_mod(&mut p, KeyCode::Char('u'), KeyModifiers::CONTROL);
    k.typ(&mut p, "Parser fix");
    k.key(&mut p, KeyCode::Enter);
    let file = templates::dir(Path::new(&here)).join("parser-fix.toml");
    assert!(file.is_file() && matches!(p.mode, Mode::Board), "{:?}", k.notices());
    assert!(k.notices().last().is_some_and(|n| n.contains("parser-fix.toml") && n.contains("2 tasks")), "{:?}", k.notices());
    let text = std::fs::read_to_string(&file).unwrap();
    let t = templates::parse(&text).unwrap();
    assert_eq!(t.tasks.len(), 2, "the discarded one isn't in it");
    assert_eq!((t.tasks[0].prompt.as_str(), t.tasks[0].model.as_str(), t.tasks[0].priority, t.tasks[0].budget_usd, t.tasks[0].acceptance.as_str()), ("Fix the parser\nso it keeps comments", "sonnet", 1, 1.5, "cargo test parser"));
    assert_eq!((t.tasks[0].owns.clone(), t.tasks[1].agent.as_str(), t.tasks[1].after.clone()), (vec!["src/parse/**".to_string()], "codex", vec!["t1".to_string()]));
    // saving again doesn't overwrite it
    k.key(&mut p, KeyCode::Char('S'));
    k.key_mod(&mut p, KeyCode::Char('u'), KeyModifiers::CONTROL);
    k.typ(&mut p, "parser fix");
    k.key(&mut p, KeyCode::Enter);
    assert!(templates::dir(Path::new(&here)).join("parser-fix-2.toml").is_file());
    std::fs::remove_file(templates::dir(Path::new(&here)).join("parser-fix-2.toml")).unwrap();
    // W, e: a literal becomes an arg (a broken edit isn't saved)
    p.lead_focus = false;
    k.key(&mut p, KeyCode::Char('W'));
    assert!(matches!(&p.mode, Mode::Templates(v) if v.items.len() == 1));
    k.key(&mut p, KeyCode::Char('e'));
    if let Mode::Templates(v) = &mut p.mode {
        v.edit.as_mut().unwrap().1 = Input::new("name = [broken", true);
    }
    k.key_mod(&mut p, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert!(matches!(&p.mode, Mode::Templates(v) if v.edit.as_ref().is_some_and(|e| !e.2.is_empty())), "the error shows, nothing's saved");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
    if let Mode::Templates(v) = &mut p.mode {
        v.edit.as_mut().unwrap().1 = Input::new(&text.replace("title = \"Fix the parser\"", "title = \"Fix issue {{issue}}\"").replace("so it keeps comments", "as issue {{ issue }} says"), true);
    }
    k.key_mod(&mut p, KeyCode::Char('s'), KeyModifiers::CONTROL);
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains("parser-fix") && s.contains("asks for {{issue}}"), "{s}");
    // enter: the arg, then the tasks
    k.key(&mut p, KeyCode::Enter);
    assert!(matches!(&p.mode, Mode::Templates(v) if v.args.as_ref().is_some_and(|a| a.names == vec!["issue".to_string()])));
    k.key(&mut p, KeyCode::Enter);
    assert!(k.render(&mut p, 150, 44).contains("issue is empty"));
    k.typ(&mut p, "42");
    k.key(&mut p, KeyCode::Enter);
    assert!(matches!(p.mode, Mode::Batch(_)), "straight to the run popup");
    let fix = p.store.tasks.iter().find(|t| t.title == "Fix issue 42" && t.status == Status::Todo).cloned().unwrap();
    let test = p.store.tasks.iter().find(|t| t.title == "Test the parser" && t.status == Status::Todo).cloned().unwrap();
    assert_eq!(fix.prompt, "Fix the parser\nas issue 42 says");
    assert_eq!((fix.agent.as_str(), fix.model.as_str(), fix.budget_usd, fix.acceptance.as_str()), ("claude", "sonnet", 1.5, "cargo test parser"));
    assert_eq!(test.depends_on, vec![fix.id.clone()], "its order came along");
    assert_eq!(p.marked, vec![fix.id.clone(), test.id.clone()]);
    k.key(&mut p, KeyCode::Esc);
    // a lead run that's still going can't be saved yet
    p.marked.clear();
    p.store.runs.push(store::Run { id: "r2".into(), repo: here, goal: "g".into(), agent: "claude".into(), state: store::RunState::Running, created: store::now(), ..Default::default() });
    p.lead_focus = true;
    k.key(&mut p, KeyCode::Char('S'));
    assert!(matches!(p.mode, Mode::Board) && k.notices().last().is_some_and(|n| n.contains("still at it")), "{:?}", k.notices());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Template args, names and a small pane.
#[test]
fn agents_template_args_and_names() {
    let t = templates::Template {
        name: "x".into(),
        description: String::new(),
        tasks: vec![templates::TaskT { key: "t1".into(), title: "Fix {{issue}} in {{ area }}".into(), prompt: "see {{issue}}".into(), agent: "claude".into(), owns: vec!["src/{{area}}/**".into()], ..Default::default() }],
    };
    assert_eq!(templates::args(&t), vec!["issue".to_string(), "area".to_string()]);
    let f = templates::fill(&t, &[("issue".into(), "#7".into()), ("area".into(), "cli".into())]);
    assert_eq!((f.tasks[0].title.as_str(), f.tasks[0].prompt.as_str(), f.tasks[0].owns[0].as_str()), ("Fix #7 in cli", "see #7", "src/cli/**"));
    assert_eq!(templates::name_for("Fix issue #12 — fast!"), "fix-issue-12-fast");
    assert_eq!(templates::name_for("!!"), "template");
    assert!(templates::parse("name = \"x\"").unwrap_err().contains("no [[task]]"));
    let text = templates::to_text(&t).unwrap();
    assert!(text.starts_with("# oriel template") && templates::parse(&text).unwrap() == t, "{text}");
    // the list and its forms in small panes
    let dir = scratch("templates-small");
    let mut k = Kit::new();
    let mut p = offline(&dir);
    templates::save(&p.repo.as_ref().unwrap().root.clone(), &t).unwrap();
    k.key(&mut p, KeyCode::Char('W'));
    let s = k.render_html(&mut p, 150, 44, "target/snap/agents-templates.html");
    println!("{s}");
    for (w, h) in [(60, 16), (30, 9), (20, 8)] {
        k.render(&mut p, w, h);
    }
    k.key(&mut p, KeyCode::Enter);
    k.typ(&mut p, "#7");
    let s = k.render_html(&mut p, 150, 44, "target/snap/agents-template-args.html");
    println!("{s}");
    for (w, h) in [(60, 16), (30, 9), (20, 8)] {
        k.render(&mut p, w, h);
    }
    k.key(&mut p, KeyCode::Esc);
    k.key(&mut p, KeyCode::Char('e'));
    for (w, h) in [(150, 44), (60, 16), (30, 9), (20, 8)] {
        k.render(&mut p, w, h);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A finished card says where its review stands: a big diff nudges for a second opinion, one being written shows
/// who's at it, findings show how many are blocking. V on the card asks for one.
#[test]
fn agents_card_shows_review_state_and_v_asks() {
    let dir = scratch("review-chip");
    let mut k = Kit::new();
    let mut p = super::tests::demo(&dir);
    let id = id_of(&p, "Usage sink status line"); // +212 −40
    p.task_mut(&id).unwrap().worktree = dir.display().to_string();
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains("V 2nd opinion?"), "a big diff nudges: {s}");
    p.reviewing.insert(id.clone(), "codex".into());
    assert!(k.render(&mut p, 150, 44).contains("codex"));
    p.reviewing.clear();
    p.notes.insert(id.clone(), vec![review::Note { path: "a".into(), line: 1, old: false, code: String::new(), text: "boom".into(), finding: Some(true), on: true }]);
    assert!(k.render(&mut p, 150, 44).contains("● 1 blocking"));
    // V on the card: here its checkout has no base to diff against, so it says so
    p.select(&id);
    assert!(p.board_hints().contains(&("V", "second opinion")));
    k.key(&mut p, KeyCode::Char('V'));
    assert!(k.notices().last().is_some_and(|n| n.contains("nothing to review")), "{:?}", k.notices());
    let _ = std::fs::remove_dir_all(&dir);
}

/// oriel's own files (.oriel/: a template you just saved, the repo's setup) don't count as uncommitted work when
/// a task starts; your edits still do.
#[test]
fn agents_oriel_files_dont_make_the_checkout_dirty() {
    let dir = scratch("oriel-files");
    let repo = temp_repo(&dir);
    std::fs::create_dir_all(repo.join(".oriel").join("templates")).unwrap();
    std::fs::write(repo.join(".oriel").join("templates").join("x.toml"), "name = \"x\"\n").unwrap();
    assert!(git::start_at(&repo, &dir.join("wt").join("one"), "one-1", "t1", None, None, &git::Dirty::Ask(String::new())).is_ok());
    std::fs::write(repo.join("a.txt"), "changed\n").unwrap();
    let e = git::start_at(&repo, &dir.join("wt").join("two"), "two-1", "t2", None, None, &git::Dirty::Ask(String::new())).err().unwrap();
    assert!(e.starts_with(git::DIRTY) && e.contains("a.txt") && !e.contains(".oriel"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The lead's review tool: a second opinion on one of its finished tasks, answered to the lead (blocking and
/// optional findings with path:line), its cost counted toward the run; a task still working can't be reviewed.
#[test]
fn agents_lead_review_tool() {
    let dir = scratch("lead-review");
    let repo = temp_repo(&dir);
    let mut k = Kit::new();
    let mut p = lead_pane(&dir, &repo, fake_worker(Arc::default()), fake_worker(Arc::default()));
    p.fake_reviewer = Some(fake_reviewer(false, Arc::default()));
    let wt = dir.join("wt").join("rev");
    let s = git::start(&repo, &wt, "rev-1", "t1", None).unwrap();
    std::fs::write(wt.join("a.txt"), "one\nTWO\nthree\nfour\n").unwrap();
    let here = repo.display().to_string();
    p.store.runs.push(store::Run { id: "r1".into(), repo: here.clone(), goal: "g".into(), agent: "claude".into(), state: store::RunState::Running, branch: "oriel/lead-g".into(), created: store::now(), ..Default::default() });
    p.store.tasks.push(Task { id: "k1".into(), key: "a".into(), run: "r1".into(), repo: here, title: "Shout".into(), prompt: "shout line two".into(), agent: "claude".into(), mode: "headless".into(), status: Status::Review, worktree: wt.display().to_string(), base_sha: s.base_sha.clone(), branch: s.branch.clone(), created: store::now(), ..Default::default() });
    p.runs_live.insert("r1".into(), lead::RunLive::new());
    let call = |p: &mut Agents, k: &mut Kit| {
        let (tx, rx) = std::sync::mpsc::channel();
        with_cx(k, |cx| p.on_call(mcp::Call { run: "r1".into(), tool: "review".into(), args: json!({"id": "a"}), reply: tx }, cx));
        rx.recv_timeout(Duration::from_secs(40)).unwrap()
    };
    let v: serde_json::Value = serde_json::from_str(&call(&mut p, &mut k).unwrap()).unwrap();
    assert_eq!((v["reviewer"].as_str(), v["blocking"][0]["path"].as_str(), v["blocking"][0]["line"].as_u64()), (Some("codex"), Some("a.txt"), Some(2)), "{v}");
    assert_eq!(v["optional"].as_array().map(|a| a.len()), Some(1));
    until(&mut k, &mut p, 5000, "its cost", |p| p.store.runs[0].cost_usd > 0.1);
    assert!(run0(&p).log.iter().any(|l| l.contains("second opinion on k1 from codex")), "{:?}", run0(&p).log);
    assert!(p.notes_of("k1").is_empty(), "the lead's review goes to the lead, not into your comments");
    p.task_mut("k1").unwrap().status = Status::Running;
    assert!(call(&mut p, &mut k).unwrap_err().contains("still working"));
    assert!(run::claude_tool_names().contains("mcp__oriel__review"), "a Claude lead may call it");
    let _ = std::fs::remove_dir_all(&dir);
}
