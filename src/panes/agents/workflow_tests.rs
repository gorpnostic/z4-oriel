//! Headless tests for the workflow around the board: the planner's marked cards, drafts, recall and saved prompts,
//! uncommitted changes when work starts or merges, the gate a repo gets, `T`, and following the chat's folder.
//! Git work happens in throwaway repos under target/test-scratch; no real agent or shell runs.

use super::tests::{demo, id_of, noop_agent, offline, pane, scratch, sh, temp_repo, until, with_cx};
use super::*;
use crate::testkit::Kit;
use crossterm::event::KeyCode;

fn form_text(p: &Agents) -> (String, String) {
    match &p.mode {
        Mode::Form(f) => (f.title.text.clone(), f.prompt.text.clone()),
        _ => panic!("the task form should be open"),
    }
}

/// P: the planner's cards arrive marked (enter runs them) and spread over the roster, cheap tier first; P stays in
/// the hints next to a run, since planning only reads the repo.
#[test]
fn agents_plan_marks_cards_and_spreads_them() {
    let dir = scratch("plan-marks");
    let mut k = Kit::new();
    let mut p = offline(&dir);
    let here = p.repo_key();
    with_cx(&mut k, |cx| p.on_msg(Msg::Plan(here.clone(), Ok(super::tests::plan_out(&[("One", "do one"), ("Two", "do two"), ("Three", "do three")], 0.05))), cx));
    let ids: Vec<String> = ["One", "Two", "Three"].iter().map(|t| id_of(&p, t)).collect();
    assert_eq!(p.marked, ids, "marked in plan order");
    // the default roster from what's installed (claude, codex): haiku (cheap), codex (mid), sonnet (premium)
    let who: Vec<(String, String)> = ids.iter().map(|id| p.task(id).map(|t| (t.agent.clone(), t.model.clone())).unwrap()).collect();
    assert_eq!(who, vec![("claude".into(), "haiku".into()), ("codex".into(), String::new()), ("claude".into(), "sonnet".into())]);
    assert!(k.notices().iter().any(|n| n.contains("planned 3 tasks · $0.05 · enter runs them (p together, s in order) · e edits one")), "{:?}", k.notices());
    k.poll(&mut p);
    assert_eq!(p.marked.len(), 3, "sync keeps the marks");
    k.key(&mut p, KeyCode::Enter);
    assert!(matches!(p.mode, Mode::Batch(_)), "enter goes straight to running them");
    k.key(&mut p, KeyCode::Esc);
    // a run at the top of the board: P is still offered
    p.marked.clear();
    p.store.runs.push(store::Run { id: "r1".into(), repo: here, goal: "g".into(), state: store::RunState::Running, created: store::now(), ..Default::default() });
    assert!(p.board_hints().contains(&("P", "plan")), "{:?}", p.board_hints());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Esc keeps what you typed: n, L, P and c bring it back with a hint, ctrl+u throws it away, and text pasted into
/// a restored form (/task, /lead) starts a fresh one without losing the draft. x on a card can be undone with u.
#[test]
fn agents_esc_keeps_drafts_and_u_undoes_a_delete() {
    let dir = scratch("drafts");
    let mut k = Kit::new();
    let mut p = offline(&dir);
    // the task form
    k.key(&mut p, KeyCode::Char('n'));
    k.typ(&mut p, "Half a title");
    k.key(&mut p, KeyCode::Tab);
    k.typ(&mut p, "a long prompt nobody wants to retype");
    k.key(&mut p, KeyCode::Esc);
    assert!(matches!(p.mode, Mode::Board));
    k.key(&mut p, KeyCode::Char('n'));
    assert_eq!(form_text(&p), ("Half a title".into(), "a long prompt nobody wants to retype".into()));
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains("restored your draft · ctrl+u clears"), "{s}");
    k.typ(&mut p, "!");
    assert!(!k.render(&mut p, 150, 44).contains("restored your draft"), "the hint goes with the first key");
    k.key(&mut p, KeyCode::Esc);
    k.key(&mut p, KeyCode::Char('n'));
    k.key_mod(&mut p, KeyCode::Char('u'), KeyModifiers::CONTROL);
    assert_eq!(form_text(&p), (String::new(), String::new()), "ctrl+u on a restored draft clears it all");
    k.key(&mut p, KeyCode::Esc);
    assert!(p.drafts.task.is_none(), "an empty form leaves no draft");
    // a restored draft gets a paste straight away (/task): the paste starts fresh, the draft waits
    k.key(&mut p, KeyCode::Char('n'));
    k.typ(&mut p, "Keep me");
    k.key(&mut p, KeyCode::Esc);
    k.key(&mut p, KeyCode::Char('n'));
    with_cx(&mut k, |cx| p.paste("Fix the login button on narrow windows: it stops responding once the sidebar is open", cx));
    assert_eq!(form_text(&p).0, "", "a fresh form");
    assert!(form_text(&p).1.starts_with("Fix the login"));
    assert_eq!(p.drafts.task.as_ref().map(|f| f.title.text.clone()), Some("Keep me".into()));
    k.key(&mut p, KeyCode::Esc);
    // the lead form
    k.key(&mut p, KeyCode::Char('L'));
    k.typ(&mut p, "a whole team's worth of goal");
    k.key(&mut p, KeyCode::Esc);
    k.key(&mut p, KeyCode::Char('L'));
    assert!(matches!(&p.mode, Mode::LeadForm(f) if f.goal.text == "a whole team's worth of goal"));
    assert!(k.render(&mut p, 150, 44).contains("restored your draft"));
    k.key(&mut p, KeyCode::Esc);
    // the plan box
    k.key(&mut p, KeyCode::Char('P'));
    k.typ(&mut p, "split this up");
    k.key(&mut p, KeyCode::Esc);
    k.key(&mut p, KeyCode::Char('P'));
    assert!(matches!(&p.mode, Mode::Plan(i) if i.text == "split this up"));
    k.key(&mut p, KeyCode::Esc);
    // a comment, per card
    let rev = id_of(&p, "Usage sink status line");
    p.task_mut(&rev).unwrap().worktree = dir.display().to_string();
    p.select(&rev);
    k.key(&mut p, KeyCode::Char('c'));
    k.typ(&mut p, "the icon is wrong");
    k.key(&mut p, KeyCode::Esc);
    k.key(&mut p, KeyCode::Char('c'));
    assert!(matches!(&p.mode, Mode::Comment(id, i) if *id == rev && i.text == "the icon is wrong"));
    k.key(&mut p, KeyCode::Esc);
    // x deletes a waiting task; u brings it back
    let todo = id_of(&p, "Kimi usage parser");
    p.select(&todo);
    k.key(&mut p, KeyCode::Char('x'));
    k.key(&mut p, KeyCode::Char('y'));
    assert!(p.task(&todo).is_none());
    assert!(k.notices().last().is_some_and(|n| n.contains("deleted \"Kimi usage parser\" · u undo")), "{:?}", k.notices());
    k.key(&mut p, KeyCode::Char('u'));
    assert!(p.task(&todo).is_some_and(|t| t.status == Status::Todo), "it's back");
    assert_eq!(p.selected().as_deref(), Some(todo.as_str()));
    k.key(&mut p, KeyCode::Char('u'));
    assert!(k.notices().last().is_some_and(|n| n.contains("nothing to undo")));
    let _ = std::fs::remove_dir_all(&dir);
}

/// ↑/↓ in an empty prompt or goal step through what you asked before in this repo; ctrl+t saves the field as a
/// prompt and inserts saved ones.
#[test]
fn agents_recall_and_saved_prompts() {
    let dir = scratch("recall");
    let mut k = Kit::new();
    let mut p = offline(&dir);
    p.store.tasks.clear();
    let here = p.repo_key();
    let a = p.add_task("A", "the older prompt", 0, "");
    let b = p.add_task("B", "the newer prompt", 0, "");
    p.task_mut(&a).unwrap().created = 100;
    p.task_mut(&b).unwrap().created = 200;
    p.store.runs.push(store::Run { id: "r1".into(), repo: here.clone(), goal: "a lead goal".into(), state: store::RunState::Merged, created: 300, ..Default::default() });
    p.store.runs.push(store::Run { id: "r2".into(), repo: "elsewhere".into(), goal: "another repo's goal".into(), state: store::RunState::Merged, created: 400, ..Default::default() });
    k.key(&mut p, KeyCode::Char('n'));
    k.key(&mut p, KeyCode::Tab);
    k.key(&mut p, KeyCode::Up);
    assert_eq!(form_text(&p).1, "a lead goal", "newest first, this repo only");
    k.key(&mut p, KeyCode::Up);
    assert_eq!(form_text(&p).1, "the newer prompt");
    k.key(&mut p, KeyCode::Up);
    k.key(&mut p, KeyCode::Up);
    assert_eq!(form_text(&p).1, "the older prompt", "stops at the oldest");
    k.key(&mut p, KeyCode::Down);
    assert_eq!(form_text(&p).1, "the newer prompt");
    k.key(&mut p, KeyCode::Down);
    k.key(&mut p, KeyCode::Down);
    assert_eq!(form_text(&p).1, "", "down past the newest empties it");
    // edited text isn't replaced by ↑
    k.key(&mut p, KeyCode::Up);
    k.typ(&mut p, "!");
    k.key(&mut p, KeyCode::Up);
    assert_eq!(form_text(&p).1, "a lead goal!");
    k.key(&mut p, KeyCode::Esc);
    p.drafts = Drafts::default();
    // the lead form's goal
    k.key(&mut p, KeyCode::Char('L'));
    k.key(&mut p, KeyCode::Up);
    assert!(matches!(&p.mode, Mode::LeadForm(f) if f.goal.text == "a lead goal"));
    k.key(&mut p, KeyCode::Esc);
    p.drafts = Drafts::default();
    // saved prompts: none yet; save what's in the field, then insert it into another form
    let file = dir.join("prompts.toml");
    prompts::TEST_PATH.with(|t| *t.borrow_mut() = Some(file.clone()));
    k.key(&mut p, KeyCode::Char('n'));
    k.key(&mut p, KeyCode::Tab);
    k.typ(&mut p, "keep changes small, run cargo test");
    k.key_mod(&mut p, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert!(p.prompt_pick.is_some());
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains("saved prompts") && s.contains("save what's in the field as \"keep changes small run\""), "{s}");
    k.typ(&mut p, "careful");
    k.key(&mut p, KeyCode::Enter);
    assert!(p.prompt_pick.is_none());
    assert_eq!(prompts::load_from(&file), vec![prompts::Prompt { name: "careful".into(), text: "keep changes small, run cargo test".into() }]);
    k.key(&mut p, KeyCode::Esc);
    p.drafts = Drafts::default();
    k.key(&mut p, KeyCode::Char('L'));
    k.typ(&mut p, "Add dark mode");
    k.key_mod(&mut p, KeyCode::Char('t'), KeyModifiers::CONTROL);
    k.key(&mut p, KeyCode::Enter);
    assert!(matches!(&p.mode, Mode::LeadForm(f) if f.goal.text == "Add dark mode\nkeep changes small, run cargo test"), "on its own line after the goal");
    // del removes one
    k.key_mod(&mut p, KeyCode::Char('t'), KeyModifiers::CONTROL);
    k.key(&mut p, KeyCode::Delete);
    assert!(prompts::load_from(&file).is_empty());
    prompts::TEST_PATH.with(|t| *t.borrow_mut() = None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Starting a task from a checkout with uncommitted changes (a chat's edits) asks: i takes them along in a
/// snapshot (your checkout untouched), c commits them first, enter starts without them (and doesn't ask again
/// for the same changes), esc doesn't start.
#[test]
fn agents_dirty_checkout_asks_before_starting() {
    let dir = scratch("dirty-start");
    let repo = temp_repo(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo);
    k.render(&mut p, 150, 44);
    until(&mut k, &mut p, 5000, "repo", |p| p.repo.is_some());
    std::fs::write(repo.join("a.txt"), "one\nCHAT\nthree\nfour\n").unwrap();
    std::fs::write(repo.join("new.rs"), "fn chat() {}\n").unwrap();
    let head = sh(&repo, &["rev-parse", "HEAD"]);
    let start = |k: &mut Kit, p: &mut Agents, title: &str| -> String {
        let id = p.add_task(title, "x", 0, "");
        p.select(&id);
        k.key(p, KeyCode::Enter);
        id
    };
    // i: a snapshot
    let one = start(&mut k, &mut p, "Take them along");
    until(&mut k, &mut p, 10_000, "asked", |p| matches!(p.mode, Mode::Dirty(_)));
    assert_eq!(p.task(&one).unwrap().status, Status::Todo);
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains("uncommitted changes") && s.contains("a.txt, new.rs") && s.contains("take them along"), "{s}");
    k.key(&mut p, KeyCode::Char('i'));
    until(&mut k, &mut p, 10_000, "started", |p| !p.task(&one).unwrap().worktree.is_empty() || !p.task(&one).unwrap().error.is_empty());
    let wt = PathBuf::from(&p.task(&one).unwrap().worktree);
    assert!(std::fs::read_to_string(wt.join("a.txt")).unwrap().contains("CHAT") && wt.join("new.rs").is_file(), "the worktree has your edits");
    assert_eq!(sh(&repo, &["rev-parse", "HEAD"]), head, "your branch didn't move");
    assert!(!sh(&repo, &["status", "--porcelain"]).is_empty(), "your checkout is as it was");
    k.actions.clear();
    // c: committed first
    let two = start(&mut k, &mut p, "Commit them");
    until(&mut k, &mut p, 10_000, "asked", |p| matches!(p.mode, Mode::Dirty(_)));
    k.key(&mut p, KeyCode::Char('c'));
    until(&mut k, &mut p, 10_000, "started", |p| !p.task(&two).unwrap().worktree.is_empty() || !p.task(&two).unwrap().error.is_empty());
    assert_eq!(sh(&repo, &["status", "--porcelain"]), "", "committed");
    assert_eq!(sh(&repo, &["log", "-1", "--format=%s"]), "work in progress (before starting \"Commit them\")");
    assert!(PathBuf::from(&p.task(&two).unwrap().worktree).join("new.rs").is_file());
    k.actions.clear();
    // enter: without them, and the same changes don't ask twice
    std::fs::write(repo.join("b.txt"), "b\n").unwrap();
    let three = start(&mut k, &mut p, "Without them");
    until(&mut k, &mut p, 10_000, "asked", |p| matches!(p.mode, Mode::Dirty(_)));
    k.key(&mut p, KeyCode::Enter);
    until(&mut k, &mut p, 10_000, "started", |p| !p.task(&three).unwrap().worktree.is_empty());
    assert!(!PathBuf::from(&p.task(&three).unwrap().worktree).join("b.txt").exists());
    k.actions.clear();
    let four = start(&mut k, &mut p, "No second ask");
    until(&mut k, &mut p, 10_000, "started", |p| !p.task(&four).unwrap().worktree.is_empty());
    assert!(!matches!(p.mode, Mode::Dirty(_)));
    k.actions.clear();
    // esc: not started
    std::fs::write(repo.join("c.txt"), "c\n").unwrap();
    let five = start(&mut k, &mut p, "Never mind");
    until(&mut k, &mut p, 10_000, "asked", |p| matches!(p.mode, Mode::Dirty(_)));
    k.key(&mut p, KeyCode::Esc);
    assert!(matches!(p.mode, Mode::Board));
    assert_eq!(p.task(&five).map(|t| (t.status, t.worktree.is_empty())), Some((Status::Todo, true)));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A merge your own uncommitted edits block offers to commit them and merge, instead of only an error.
#[test]
fn agents_merge_blocked_by_your_edits_commits_and_retries() {
    let dir = scratch("dirty-merge");
    let repo = temp_repo(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo);
    k.render(&mut p, 150, 44);
    until(&mut k, &mut p, 5000, "repo", |p| p.repo.is_some());
    let id = p.add_task("Add c", "x", 0, "");
    p.select(&id);
    k.key(&mut p, KeyCode::Enter);
    until(&mut k, &mut p, 10_000, "started", |p| !p.task(&id).unwrap().worktree.is_empty());
    k.actions.clear();
    std::fs::write(PathBuf::from(&p.task(&id).unwrap().worktree).join("c.txt"), "c\n").unwrap();
    std::fs::write(repo.join("a.txt"), "one\ntwo\nthree\nmine\n").unwrap();
    p.select(&id);
    k.key(&mut p, KeyCode::Char('m'));
    k.key(&mut p, KeyCode::Char('y'));
    until(&mut k, &mut p, 15_000, "asked", |p| matches!(p.mode, Mode::Dirty(_)));
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains("commit your changes and merge"), "{s}");
    k.key(&mut p, KeyCode::Char('c'));
    until(&mut k, &mut p, 15_000, "merged", |p| p.task(&id).unwrap().status == Status::Done);
    assert_eq!(p.task(&id).unwrap().outcome, "merged");
    assert_eq!(sh(&repo, &["status", "--porcelain"]), "");
    assert!(repo.join("c.txt").is_file());
    let log = sh(&repo, &["log", "--format=%s", "-3"]);
    assert!(log.contains("work in progress (before merging \"Add c\")") && log.contains("Add c (oriel agent)"), "{log}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A branch started from a snapshot of your checkout carries your new files: merging it back while they're still
/// untracked in your checkout is refused the same way as uncommitted edits (so the board offers to commit them),
/// and goes through once they're committed.
#[test]
fn agents_snapshot_branch_merges_after_your_files_are_committed() {
    let dir = scratch("snapshot-merge");
    let repo = temp_repo(&dir);
    std::fs::write(repo.join("new.rs"), "fn chat() {}\n").unwrap();
    let wt = dir.join("wt").join("snap");
    let s = git::start_at(&repo, &wt, "snap-1", "t1", None, None, &git::Dirty::Include).unwrap();
    assert!(wt.join("new.rs").is_file());
    std::fs::write(wt.join("c.txt"), "c\n").unwrap();
    let e = git::merge(&repo, &wt, &s.branch, &s.base_branch, "Add c", &|_| {}).unwrap_err();
    assert!(e.contains(git::UNCOMMITTED), "{e}");
    assert!(wt.exists(), "nothing was cleaned up");
    git::commit_all(&repo, "my new file").unwrap();
    git::merge(&repo, &wt, &s.branch, &s.base_branch, "Add c", &|_| {}).unwrap();
    assert!(repo.join("c.txt").is_file() && repo.join("new.rs").is_file());
    assert_eq!(sh(&repo, &["status", "--porcelain"]), "");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The gate a repo gets by itself: Cargo, Go, package.json scripts (only with node_modules installed), pytest in a
/// .venv; otherwise none, with the reason. A gate you type is remembered per repo when it isn't the default, and
/// the batch popup says what actually runs.
#[test]
fn agents_gate_detection_and_memory() {
    let dir = scratch("gate-detect");
    let d = |name: &str| {
        let x = dir.join(name);
        std::fs::create_dir_all(&x).unwrap();
        x
    };
    let js = d("js");
    std::fs::write(js.join("package.json"), r#"{"scripts":{"typecheck":"tsc --noEmit","test":"vitest","build":"vite build"}}"#).unwrap();
    std::fs::write(js.join("pnpm-lock.yaml"), "").unwrap();
    let (g, why) = git::detect_gate_in(&js);
    assert_eq!(g, None, "no node_modules yet");
    assert!(why.contains("no node_modules") && why.contains("pnpm run typecheck && pnpm run test"), "{why}");
    std::fs::create_dir_all(js.join("node_modules")).unwrap();
    assert_eq!(git::detect_gate_in(&js).0.as_deref(), Some("pnpm run typecheck && pnpm run test"));
    let npm = d("npm");
    std::fs::write(npm.join("package.json"), r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1","build":"tsc"}}"#).unwrap();
    std::fs::create_dir_all(npm.join("node_modules")).unwrap();
    assert_eq!(git::detect_gate_in(&npm).0.as_deref(), Some("npm run build"), "npm init's placeholder test is skipped");
    let bare = d("bare");
    std::fs::write(bare.join("package.json"), "{}").unwrap();
    assert!(git::detect_gate_in(&bare).1.contains("no typecheck, test or build script"));
    let rs = d("rs");
    std::fs::write(rs.join("Cargo.toml"), "").unwrap();
    assert_eq!(git::detect_gate_in(&rs).0.as_deref(), Some("cargo check --quiet"));
    let py = d("py");
    std::fs::write(py.join("pyproject.toml"), "[tool.pytest.ini_options]\n").unwrap();
    assert!(git::detect_gate_in(&py).1.contains("no .venv with pytest"));
    let (bin, site) = if cfg!(windows) {
        (py.join(".venv").join("Scripts"), py.join(".venv").join("Lib").join("site-packages").join("pytest"))
    } else {
        (py.join(".venv").join("bin"), py.join(".venv").join("lib").join("python3.12").join("site-packages").join("pytest"))
    };
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&site).unwrap();
    std::fs::write(bin.join(if cfg!(windows) { "python.exe" } else { "python" }), "").unwrap();
    let g = git::detect_gate_in(&py).0.unwrap();
    assert!(g.ends_with("-m pytest -q -x") && g.contains(".venv/"), "{g}");
    assert!(git::detect_gate_in(&d("empty")).1.contains("none detected"));
    // the form's row: the detected gate, and what you type is remembered for the repo
    let mut k = Kit::new();
    let mut p = offline(&dir);
    let here = p.repo_key();
    p.gate_detect.insert(here.clone(), (Some("cargo check --quiet".into()), "detected: a Cargo project".into()));
    k.key(&mut p, KeyCode::Char('L'));
    assert!(matches!(&p.mode, Mode::LeadForm(f) if f.gate.text == "cargo check --quiet"));
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains("gate (runs on every merged result)") && s.contains("detected: a Cargo project"), "{s}");
    k.key(&mut p, KeyCode::Esc);
    p.remember_gate("cargo test");
    assert_eq!(p.gate_prefill().0, "cargo test");
    p.remember_gate("");
    assert_eq!(p.gate_prefill(), (String::new(), "none — you took it off for this repo · type one to gate merges".into()));
    p.remember_gate("cargo check --quiet");
    assert!(p.store.gates.is_empty(), "the default isn't stored");
    // the batch popup says what actually runs
    p.store.gates.insert(here, String::new());
    p.marked = vec![id_of(&p, "Theme toggle in the palette")];
    k.key(&mut p, KeyCode::Enter);
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains("conflict check only") && s.contains("g sets one"), "{s}");
    k.key(&mut p, KeyCode::Char('g'));
    k.typ(&mut p, "cargo test");
    k.key(&mut p, KeyCode::Enter);
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains("`cargo test` on the merged result"), "{s}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A checkout borrows the main one's node_modules through a link, and removing the checkout never deletes
/// through it.
#[test]
fn agents_borrowed_node_modules_survive_removal() {
    let dir = scratch("deps");
    let repo = temp_repo(&dir);
    std::fs::create_dir_all(repo.join("node_modules").join("left-pad")).unwrap();
    std::fs::write(repo.join("node_modules").join("left-pad").join("index.js"), "module.exports = 1\n").unwrap();
    let wt = dir.join("wt").join("deps");
    let s = git::start(&repo, &wt, "deps-1", "t1", None).unwrap();
    std::fs::write(wt.join("package.json"), "{}").unwrap();
    git::link_deps(&repo, &wt);
    assert!(wt.join("node_modules").join("left-pad").join("index.js").is_file(), "borrowed");
    git::remove(&repo, &wt, &s.branch).unwrap();
    assert!(!wt.exists());
    assert!(repo.join("node_modules").join("left-pad").join("index.js").is_file(), "the main checkout's are untouched");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A worker going back to work in a checkout where T borrowed your node_modules gets the link taken out first: an
/// install it runs can't reach your checkout's node_modules through it.
#[test]
fn agents_worker_never_runs_on_borrowed_node_modules() {
    let dir = scratch("deps-worker");
    let repo = temp_repo(&dir);
    std::fs::create_dir_all(repo.join("node_modules").join("left-pad")).unwrap();
    let mut k = Kit::new();
    let mut p = super::lead_tests::lead_pane(&dir, &repo, super::lead_tests::fake_worker(Arc::default()), super::lead_tests::fake_worker(Arc::default()));
    let wt = dir.join("wt").join("deps");
    let s = git::start(&repo, &wt, "deps-1", "t1", None).unwrap();
    std::fs::write(wt.join("package.json"), "{}").unwrap();
    git::link_deps(&repo, &wt);
    assert!(wt.join("node_modules").join("left-pad").is_dir(), "T borrowed them");
    let here = repo.display().to_string();
    p.store.runs.push(store::Run { id: "r1".into(), repo: here.clone(), goal: "g".into(), agent: "claude".into(), state: store::RunState::Running, branch: "oriel/lead-g".into(), created: store::now(), ..Default::default() });
    p.store.tasks.push(Task { id: "k1".into(), key: "a".into(), run: "r1".into(), repo: here, title: "Deps".into(), prompt: "x".into(), agent: "claude".into(), mode: "headless".into(), status: Status::Review, worktree: wt.display().to_string(), base_sha: s.base_sha.clone(), branch: s.branch.clone(), created: store::now(), ..Default::default() });
    p.runs_live.insert("r1".into(), lead::RunLive::new());
    with_cx(&mut k, |cx| p.worker_again("k1", "one more thing", cx));
    assert_eq!(p.task("k1").unwrap().status, Status::Running);
    assert!(wt.join("node_modules").symlink_metadata().is_err(), "the link went before the worker started");
    assert!(repo.join("node_modules").join("left-pad").is_dir(), "yours are untouched");
    until(&mut k, &mut p, 10_000, "the round is over", |p| p.task("k1").is_some_and(|t| t.status != Status::Running) && !p.live.get("k1").is_some_and(|l| l.checking));
    let _ = std::fs::remove_dir_all(&dir);
}

/// T opens a terminal tab in the task's checkout (tagged, so it closes before the checkout goes), starting the
/// command remembered for the repo; the shell keeps running after it.
#[test]
fn agents_try_opens_a_terminal_in_the_checkout() {
    let dir = scratch("try");
    let mut k = Kit::new();
    let mut p = offline(&dir);
    p.fake_agent = Some(noop_agent()); // the terminal runs a no-op, never a real shell
    let id = id_of(&p, "Usage sink status line");
    p.task_mut(&id).unwrap().worktree = dir.display().to_string();
    p.select(&id);
    k.key(&mut p, KeyCode::Char('T'));
    assert!(matches!(p.mode, Mode::Try(_)));
    assert!(k.render(&mut p, 150, 44).contains("run there (remembered"));
    k.typ(&mut p, "pnpm dev");
    k.key(&mut p, KeyCode::Enter);
    let tag = format!("agent-try:{id}");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !k.actions.iter().any(|a| matches!(a, Action::OpenTagged { tag: t, focus: true, .. } if *t == tag)) {
        assert!(Instant::now() < deadline, "no terminal opened");
        k.poll(&mut p);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(p.store.try_cmds.get(&p.repo_key()).map(String::as_str), Some("pnpm dev"), "remembered for the repo");
    k.actions.clear();
    // the checkout goes (a retry): its terminal is closed first
    with_cx(&mut k, |cx| p.remove(&id, Then::Retry, cx));
    assert!(k.actions.iter().any(|a| matches!(a, Action::CloseTag(t) if *t == tag)));
    k.actions.clear();
    // a running run's checkout moves under you: refused
    let here = p.repo_key();
    p.store.runs.push(store::Run { id: "r1".into(), repo: here, goal: "g".into(), state: store::RunState::Running, worktree: dir.display().to_string(), branch: "oriel/lead-g".into(), created: store::now(), ..Default::default() });
    with_cx(&mut k, |cx| p.open_try("r1", cx));
    assert!(!matches!(p.mode, Mode::Try(_)));
    // the shell: runs the command, then stays
    let cfg = |s: &str| crate::config::Config { shell: s.into(), ..Default::default() };
    assert_eq!(try_shell(&cfg("pwsh -NoLogo"), "", "pnpm dev", 0).1, vec!["-NoLogo", "-NoExit", "-Command", "pnpm dev"]);
    assert_eq!(try_shell(&cfg("bash"), "", "pnpm dev", 0).1, vec!["-c", "pnpm dev; exec bash"]);
    assert_eq!(try_shell(&cfg("cmd.exe"), "", "npm start", 0).1, vec!["/k", "npm start"]);
    assert!(try_shell(&cfg("bash"), "", " ", 0).1.is_empty(), "no command: just the shell");
    // with the checkout's port set in it, and a setup that has to work before the command runs
    assert_eq!(try_shell(&cfg("pwsh -NoLogo"), "pnpm install", "pnpm dev", 5401).1, vec!["-NoLogo", "-NoExit", "-Command", "$env:ORIEL_PORT='5401'; pnpm install; if ($?) { pnpm dev }"]);
    assert_eq!(try_shell(&cfg("bash"), "pnpm install", "pnpm dev", 5401).1, vec!["-c", "export ORIEL_PORT=5401; pnpm install && pnpm dev; exec bash"]);
    assert_eq!(try_shell(&cfg("cmd.exe"), "", "npm start", 5401).1, vec!["/k", "set ORIEL_PORT=5401&& npm start"]);
    assert_eq!(try_shell(&cfg("cmd.exe"), "", "", 5401).1, vec!["/k", "set ORIEL_PORT=5401"], "a bare shell still gets its port");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Agents follows the chat you're in: the picker lists your chats' folders, a pane that isn't started in a repo
/// opens on the chat's, and a /lead straight from a chat in another repo runs there.
#[test]
fn agents_follow_the_chats_folder() {
    let dir = scratch("chat-dir");
    let repo = temp_repo(&dir);
    let other = temp_repo(&dir.join("two"));
    // the folders chats work in, newest chat first (gone folders and chats without one skipped)
    let store_dir = dir.join("chats");
    std::fs::create_dir_all(&store_dir).unwrap();
    let chat = |name: &str, cwd: Option<&Path>, updated: f64| {
        let v = serde_json::json!({"id": name, "title": name, "created": 1.0, "updated": updated, "messages": [], "cwd": cwd.map(|c| c.display().to_string())});
        std::fs::write(store_dir.join(format!("{name}.json")), v.to_string()).unwrap();
    };
    chat("a", Some(&repo), 2.0);
    chat("b", Some(&other), 3.0);
    chat("c", Some(&dir.join("gone")), 9.0);
    chat("d", None, 5.0);
    assert_eq!(chat_dirs(&store_dir), vec![other.display().to_string(), repo.display().to_string()]);
    // started outside any repo (a folder that isn't there: every real one under target/ is inside oriel's own
    // checkout), just after using a chat in `other`
    let outside = dir.join("no-repo");
    let mut k = Kit::new();
    let mut p = pane(&dir, &outside);
    p.chat_store = Some(store_dir.clone());
    *p.chat_hint.lock().unwrap() = Some((other.clone(), Instant::now()));
    k.render(&mut p, 150, 44);
    until(&mut k, &mut p, 5000, "repo", |p| p.repo.is_some());
    assert!(p.repo_key().contains("two"), "{}", p.repo_key());
    until(&mut k, &mut p, 5000, "chat folders", |p| p.chat_dirs.len() == 2);
    k.key(&mut p, KeyCode::Char('o'));
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains("from your chats"), "{s}");
    k.key(&mut p, KeyCode::Esc);
    // /lead from a chat working in `repo`: the form opens there, with the goal it sent
    *p.chat_hint.lock().unwrap() = Some((repo.clone(), Instant::now()));
    k.key(&mut p, KeyCode::Char('L'));
    with_cx(&mut k, |cx| p.paste("Add dark mode", cx));
    until(&mut k, &mut p, 5000, "the form", |p| matches!(p.mode, Mode::LeadForm(_)));
    assert!(matches!(&p.mode, Mode::LeadForm(f) if f.goal.text == "Add dark mode"));
    assert!(!p.repo_key().contains("two"), "{}", p.repo_key());
    assert!(k.notices().iter().any(|n| n.contains("the repo this chat works in")), "{:?}", k.notices());
    k.key(&mut p, KeyCode::Esc);
    // the chat said so a while ago: L is just L, on the board's repo
    *p.chat_hint.lock().unwrap() = Some((other.clone(), Instant::now() - Duration::from_secs(10)));
    p.drafts = Drafts::default();
    k.key(&mut p, KeyCode::Char('L'));
    assert!(matches!(p.mode, Mode::LeadForm(_)) && !p.repo_key().contains("two"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The new popups fit (or clip) in small panes without panicking.
#[test]
fn agents_workflow_popups_in_small_panes() {
    let dir = scratch("small-popups");
    let mut k = Kit::new();
    let mut p = demo(&dir);
    let id = id_of(&p, "Theme toggle in the palette");
    let here = p.repo_key();
    p.store.runs.push(store::Run { id: "r1".into(), repo: here, goal: "g".into(), state: store::RunState::Review, branch: "oriel/lead-g".into(), worktree: dir.display().to_string(), budget_usd: 1.0, cost_usd: 2.0, created: store::now(), ..Default::default() });
    let modes: Vec<Box<dyn Fn(&mut Agents)>> = vec![
        Box::new(move |p| p.mode = Mode::Dirty(DirtyAsk { then: DirtyThen::Task(id.clone()), status: " M a.rs".into() })),
        Box::new(|p| p.mode = Mode::Dirty(DirtyAsk { then: DirtyThen::MergeRun("r1".into()), status: String::new() })),
        Box::new(|p| p.mode = Mode::Try(TryView { id: "r1".into(), dir: PathBuf::from("x"), title: "a title".into(), input: Input::new("pnpm dev", false), setup: "pnpm install".into(), port: 5401 })),
        Box::new(|p| p.mode = Mode::Batch(BatchView { gate: Input::new("cargo test", false), editing: true })),
        Box::new(|p| p.mode = Mode::Comment("r1".into(), Input::new("more", true))),
        Box::new(|p| p.mode = Mode::LeadForm(p.new_lead_form())),
        Box::new(|p| {
            p.mode = Mode::Plan(Input::new("", true));
            p.prompt_pick = Some(PromptPick { items: vec![prompts::Prompt { name: "a".into(), text: "b".into() }], filter: Input::new("", false), sel: 0, field: "text".into() });
        }),
    ];
    for set in &modes {
        for (w, h) in [(150, 44), (60, 16), (30, 9), (20, 8)] {
            set(&mut p);
            k.render(&mut p, w, h);
        }
        p.prompt_pick = None;
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The demo board's snapshots with the new popups (for eyeballing: target/snap/agents-*.html).
#[test]
fn agents_snapshots_workflow_popups() {
    let dir = scratch("snap-workflow");
    let mut k = Kit::new();
    let mut p = demo(&dir);
    let id = id_of(&p, "Theme toggle in the palette");
    p.mode = Mode::Dirty(DirtyAsk { then: DirtyThen::Task(id), status: " M src/app.rs\n?? src/theme_toggle.rs".into() });
    let s = k.render_html(&mut p, 150, 44, "target/snap/agents-dirty.html");
    println!("{s}");
    assert!(s.contains("src/app.rs, src/theme_toggle.rs") && s.contains("commit them first"), "{s}");
    p.mode = Mode::Board;
    p.marked = vec![id_of(&p, "Theme toggle in the palette"), id_of(&p, "Kimi usage parser")];
    k.key(&mut p, KeyCode::Enter);
    let s = k.render_html(&mut p, 150, 44, "target/snap/agents-batch-gate.html");
    println!("{s}");
    assert!(s.contains("gate · g edits"), "{s}");
    k.key(&mut p, KeyCode::Esc);
    p.marked.clear();
    // the lead form with its gate row
    let here = p.repo_key();
    p.gate_detect.insert(here.clone(), (Some("pnpm run typecheck && pnpm run test".into()), "detected from package.json (it borrows your node_modules)".into()));
    k.key(&mut p, KeyCode::Char('L'));
    k.typ(&mut p, "Add a theme toggle to the palette");
    let s = k.render_html(&mut p, 150, 44, "target/snap/agents-lead-form.html");
    println!("{s}");
    assert!(s.contains("pnpm run typecheck"), "{s}");
    k.key(&mut p, KeyCode::Esc);
    // a finished lead run: its panel's hints, and the feedback box
    let wt = dir.join("lead-checkout");
    std::fs::create_dir_all(&wt).unwrap();
    p.store.runs.push(store::Run { id: "r1".into(), repo: here, goal: "Add a theme toggle to the palette".into(), agent: "claude".into(), state: store::RunState::Review, branch: "oriel/lead-add-a-theme-r1".into(), base_branch: "master".into(), worktree: wt.display().to_string(), budget_usd: 8.0, cost_usd: 0.4, created: store::now() - 600, finished: store::now(), log: vec!["merged Theme toggle".into(), "lead finished · review oriel/lead-add-a-theme-r1 (d) and merge it (m)".into()], ..Default::default() });
    p.lead_focus = true;
    let s = k.render_html(&mut p, 150, 44, "target/snap/agents-lead-review.html");
    println!("{s}");
    assert!(s.contains("c feedback") && s.contains("T try it"), "{s}");
    k.key(&mut p, KeyCode::Char('c'));
    k.typ(&mut p, "also add tests");
    let s = k.render_html(&mut p, 150, 44, "target/snap/agents-feedback.html");
    println!("{s}");
    assert!(s.contains("feedback to the lead"), "{s}");
    k.key(&mut p, KeyCode::Esc);
    k.key(&mut p, KeyCode::Char('T'));
    let s = k.render_html(&mut p, 150, 44, "target/snap/agents-try.html");
    println!("{s}");
    k.key(&mut p, KeyCode::Esc);
    // saved prompts over the task form
    let file = dir.join("prompts.toml");
    prompts::save_to(&file, &[prompts::Prompt { name: "careful".into(), text: "keep changes small, run cargo test, update the README".into() }, prompts::Prompt { name: "ui".into(), text: "match the theme's colours".into() }]).unwrap();
    prompts::TEST_PATH.with(|t| *t.borrow_mut() = Some(file.clone()));
    p.lead_focus = false;
    k.key(&mut p, KeyCode::Char('n'));
    k.key(&mut p, KeyCode::Tab);
    k.typ(&mut p, "Show a dot per running task");
    k.key_mod(&mut p, KeyCode::Char('t'), KeyModifiers::CONTROL);
    let s = k.render_html(&mut p, 150, 44, "target/snap/agents-prompts.html");
    println!("{s}");
    prompts::TEST_PATH.with(|t| *t.borrow_mut() = None);
    let _ = std::fs::remove_dir_all(&dir);
}
