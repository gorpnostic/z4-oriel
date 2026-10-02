//! Headless tests for the orchestrator. Everything git happens in throwaway repos under target/test-scratch;
//! no real agent runs (a no-op command stands in), and hooks are simulated by calling `store::report`.

use super::*;
use crate::testkit::Kit;
use crossterm::event::KeyCode;

pub(super) fn scratch(name: &str) -> PathBuf {
    let nanos = SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-scratch").join(format!("agents-{name}-{nanos}"));
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub(super) fn sh(dir: &Path, args: &[&str]) -> String {
    let o = git::run(dir, args);
    assert!(o.ok, "git {args:?} failed: {}", o.stderr);
    o.stdout.trim().to_string()
}

/// A fresh repo with one commit on master.
pub(super) fn temp_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    sh(&repo, &["init", "-q", "-b", "master"]);
    sh(&repo, &["config", "user.name", "oriel test"]);
    sh(&repo, &["config", "user.email", "test@example.invalid"]);
    sh(&repo, &["config", "commit.gpgsign", "false"]);
    sh(&repo, &["config", "core.autocrlf", "false"]);
    std::fs::write(repo.join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
    sh(&repo, &["add", "-A"]);
    sh(&repo, &["commit", "-q", "-m", "init"]);
    repo
}

pub(super) fn noop_agent() -> (String, Vec<String>) {
    if cfg!(windows) { ("cmd.exe".into(), vec!["/c".into(), "exit".into()]) } else { ("true".into(), vec![]) }
}

pub(super) fn pane(dir: &Path, repo: &Path) -> Agents {
    let mut a = Agents::with_paths(Paths { agents: dir.join("agents"), wt: dir.join("wt") });
    a.start_dir = Some(repo.to_path_buf());
    a.fake_agent = Some(noop_agent());
    a
}

/// Poll until `cond` holds (or fail after `ms`, times four: the suite runs many git-heavy tests at once, often
/// next to builds, and a passing wait returns the moment its condition holds anyway).
pub(super) fn until(k: &mut Kit, p: &mut Agents, ms: u64, what: &str, cond: impl Fn(&Agents) -> bool) {
    let deadline = Instant::now() + Duration::from_millis(ms * 4);
    loop {
        k.poll(p);
        if cond(p) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn only(p: &Agents) -> &Task {
    &p.store.tasks[0]
}

fn snap(k: &mut Kit, p: &mut Agents, name: &str) -> String {
    k.render_html(p, 150, 44, &format!("target/snap/agents-{name}.html"))
}

#[test]
fn agents_full_flow_start_hooks_review_diff_merge() {
    let dir = scratch("flow");
    let repo = temp_repo(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo);
    k.render(&mut p, 150, 44);
    until(&mut k, &mut p, 5000, "repo detected", |p| p.repo.is_some());
    assert_eq!(p.repo.as_ref().unwrap().branch, "master");

    // new task through the form
    k.key(&mut p, KeyCode::Char('n'));
    k.typ(&mut p, "Add a b file");
    k.key(&mut p, KeyCode::Tab);
    k.typ(&mut p, "Create b.txt and change line two of a.txt");
    let form = k.render(&mut p, 150, 44);
    assert!(form.contains("new task") && form.contains("Add a b file"), "{form}");
    k.key_mod(&mut p, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert_eq!(p.store.tasks.len(), 1);
    assert_eq!(only(&p).status, Status::Todo);
    let id = only(&p).id.clone();

    // enter starts it: worktree + branch + hooks, then the agent's tab
    k.key(&mut p, KeyCode::Enter);
    until(&mut k, &mut p, 10000, "worktree created", |p| !only(p).worktree.is_empty() || !only(p).error.is_empty());
    let t = only(&p).clone();
    assert!(t.error.is_empty(), "start failed: {}", t.error);
    assert_eq!(t.status, Status::Running);
    let wt = PathBuf::from(&t.worktree);
    assert!(wt.join("a.txt").is_file());
    assert!(t.branch.starts_with("oriel/add-a-b-file-"), "{}", t.branch);
    assert_eq!(t.base_branch, "master");
    let settings = std::fs::read_to_string(wt.join(".claude").join("settings.local.json")).unwrap();
    assert!(settings.contains(&format!("report --task {id} --state idle")), "{settings}");
    assert!(settings.contains("PreToolUse") && settings.contains("Notification") && settings.contains("SessionStart"));
    assert_eq!(sh(&wt, &["status", "--porcelain"]), "", "settings.local.json must be excluded from the task's branch");
    let opened = k.actions.iter().any(|a| matches!(a, Action::OpenTagged { tag, focus: false, .. } if *tag == t.tag()));
    assert!(opened, "the agent tab should open (unfocused)");
    k.actions.clear(); // drops the fake agent's terminal

    // the app reports the tab alive; the agent edits files
    p.tagged_panes(&[(t.tag(), Some(Activity::Working))]);
    std::fs::write(wt.join("a.txt"), "one\nTWO\nthree\nfour\n").unwrap();
    std::fs::write(wt.join("b.txt"), "new file\n").unwrap();

    // hooks: a tool call, a question, then Stop
    let paths = p.paths.clone();
    let hook = |state: &str, json: &str| store::report(&paths, &["--task".into(), id.clone(), "--state".into(), state.into()], Some(json));
    hook("session", r#"{"session_id":"s-1","transcript_path":"/nope/s-1.jsonl","hook_event_name":"SessionStart"}"#);
    hook("running", r#"{"hook_event_name":"PreToolUse","tool_name":"Edit","tool_input":{"file_path":"C:/x/a.txt"}}"#);
    until(&mut k, &mut p, 5000, "hook status applied", |p| only(p).last == "Edit a.txt");
    assert_eq!(only(&p).session_id, "s-1");
    hook("blocked", r#"{"hook_event_name":"Notification","message":"Claude needs your permission to use Bash"}"#);
    until(&mut k, &mut p, 5000, "blocked", |p| only(p).status == Status::Blocked);
    assert!(only(&p).question.contains("permission to use Bash"));
    assert!(k.notices().iter().any(|n| n.contains("needs you")));
    let board = k.render(&mut p, 150, 44);
    assert!(board.contains("BLOCKED") && board.contains("to use Bash"), "{board}");
    hook("running", r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cargo test"}}"#);
    until(&mut k, &mut p, 5000, "running again", |p| only(p).status == Status::Running);
    hook("idle", r#"{"hook_event_name":"Stop","session_id":"s-1"}"#);
    until(&mut k, &mut p, 5000, "review", |p| only(p).status == Status::Review);
    until(&mut k, &mut p, 8000, "+/- counted", |p| only(p).files == 2);
    assert_eq!((only(&p).added, only(&p).removed), (2, 1));
    assert!(k.notices().iter().any(|n| n.contains("ready for review")));
    let board = k.render(&mut p, 150, 44);
    assert!(board.contains("+2 −1"), "{board}");
    assert_eq!(p.col, 2, "the selection follows the card into REVIEW");

    // diff view: both files, numbered, merges cleanly
    k.key(&mut p, KeyCode::Char('d'));
    until(&mut k, &mut p, 8000, "diff loaded", |p| matches!(&p.mode, Mode::Diff(v) if v.data.is_some()));
    {
        let Mode::Diff(v) = &p.mode else { unreachable!() };
        let d = v.data.as_ref().unwrap().as_ref().unwrap();
        let names: Vec<&str> = d.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "b.txt"]);
        assert_eq!(d.files[1].note, "new");
        assert_eq!(d.conflicts, Some(vec![]));
        let add = d.files[0].lines.iter().find(|l| l.kind == git::Kind::Add).unwrap();
        assert_eq!((add.new, add.text.as_str()), (Some(2), "TWO"));
    }
    let diff = k.render(&mut p, 150, 44);
    assert!(diff.contains("merges cleanly into master") && diff.contains("TWO"), "{diff}");

    // merge: confirm, squash into master, worktree + branch gone
    k.key(&mut p, KeyCode::Char('m'));
    assert!(matches!(p.mode, Mode::Confirm(_)));
    assert!(k.render(&mut p, 150, 44).contains("Squash-merge"));
    k.key(&mut p, KeyCode::Char('y'));
    until(&mut k, &mut p, 15000, "merged", |p| only(p).status == Status::Done || !only(p).error.is_empty());
    assert!(only(&p).error.is_empty(), "merge failed: {}", only(&p).error);
    assert_eq!(only(&p).outcome, "merged");
    assert!(sh(&repo, &["log", "-1", "--format=%s"]).ends_with("(oriel agent)"));
    assert_eq!(std::fs::read_to_string(repo.join("b.txt")).unwrap().trim(), "new file");
    assert!(!wt.exists(), "worktree removed");
    assert!(!git::run(&repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{}", t.branch)]).ok, "branch deleted");
    assert_eq!(sh(&repo, &["status", "--porcelain"]), "");
    assert!(k.actions.iter().any(|a| matches!(a, Action::CloseTag(tag) if *tag == t.tag())));
    // saved to disk (the writer thread is async)
    std::thread::sleep(Duration::from_millis(200));
    let saved = std::fs::read_to_string(dir.join("agents").join("tasks.json")).unwrap();
    assert!(saved.contains("\"merged\""), "{saved}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agents_closed_tab_goes_to_todo_or_review_and_discard() {
    let dir = scratch("gone");
    let repo = temp_repo(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo);
    k.render(&mut p, 150, 44);
    until(&mut k, &mut p, 5000, "repo", |p| p.repo.is_some());
    let id = p.add_task("Tidy the readme", "tidy it", 0, "");
    p.select(&id);

    // started, then the user closes the tab without the agent changing anything -> back to TODO, worktree gone
    k.key(&mut p, KeyCode::Enter);
    until(&mut k, &mut p, 10000, "started", |p| !only(p).worktree.is_empty());
    let wt = PathBuf::from(&only(&p).worktree);
    k.actions.clear();
    p.tagged_panes(&[(only(&p).tag(), Some(Activity::Working))]);
    k.poll(&mut p);
    p.tagged_panes(&[]);
    until(&mut k, &mut p, 8000, "back to todo", |p| only(p).status == Status::Todo);
    assert!(!wt.exists(), "an empty worktree is cleaned up");
    assert!(only(&p).worktree.is_empty());

    // again, this time with a change -> REVIEW
    p.select(&id);
    k.key(&mut p, KeyCode::Enter);
    until(&mut k, &mut p, 10000, "started again", |p| !only(p).worktree.is_empty());
    let wt = PathBuf::from(&only(&p).worktree);
    k.actions.clear();
    p.tagged_panes(&[(only(&p).tag(), Some(Activity::Working))]);
    k.poll(&mut p);
    std::fs::write(wt.join("README.md"), "# tidy\n").unwrap();
    p.tagged_panes(&[]);
    until(&mut k, &mut p, 8000, "review", |p| only(p).status == Status::Review);

    // comment -> a follow-up session in the same worktree, back to RUNNING
    k.key(&mut p, KeyCode::Char('c'));
    assert!(matches!(p.mode, Mode::Comment(..)));
    k.typ(&mut p, "also add a license line");
    k.key(&mut p, KeyCode::Enter);
    assert_eq!(only(&p).status, Status::Running);
    assert_eq!(only(&p).followups, 1);
    assert!(k.actions.iter().any(|a| matches!(a, Action::OpenTagged { .. })));
    k.actions.clear();

    // discard: confirm, worktree + branch removed, card done
    let branch = only(&p).branch.clone();
    k.key(&mut p, KeyCode::Char('x'));
    assert!(k.render(&mut p, 150, 44).contains("Throw away"));
    k.key(&mut p, KeyCode::Char('y'));
    until(&mut k, &mut p, 15000, "discarded", |p| only(p).status == Status::Done);
    assert_eq!(only(&p).outcome, "discarded");
    assert!(!wt.exists());
    assert!(!git::run(&repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).ok);
    assert!(!repo.join("README.md").exists(), "nothing reached the main repo");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agents_merge_refuses_dirty_repo() {
    let dir = scratch("dirty");
    let repo = temp_repo(&dir);
    let wt = dir.join("wt").join("x");
    let s = git::start(&repo, &wt, "x-1", "t1", None).unwrap();
    std::fs::write(wt.join("c.txt"), "c\n").unwrap();
    std::fs::write(repo.join("a.txt"), "dirty\n").unwrap();
    let e = git::merge(&repo, &wt, &s.branch, &s.base_branch, "t", &|_| {}).unwrap_err();
    assert!(e.contains("uncommitted changes"), "{e}");
    assert!(wt.exists(), "nothing was cleaned up");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agents_conflict_detected_in_diff() {
    let dir = scratch("conflict");
    let repo = temp_repo(&dir);
    let wt = dir.join("wt").join("y");
    let s = git::start(&repo, &wt, "y-1", "t1", None).unwrap();
    std::fs::write(wt.join("a.txt"), "one\nAGENT\nthree\nfour\n").unwrap();
    std::fs::write(repo.join("a.txt"), "one\nHUMAN\nthree\nfour\n").unwrap();
    sh(&repo, &["commit", "-qam", "human edit"]);
    let d = git::diff(&repo, &wt, &s.base_sha).unwrap();
    assert_eq!(d.conflicts, Some(vec!["a.txt".to_string()]));
    let e = git::merge(&repo, &wt, &s.branch, &s.base_branch, "t", &|_| {}).unwrap_err();
    assert!(e.contains("conflict"), "{e}");
    assert_eq!(sh(&repo, &["status", "--porcelain"]), "", "a failed merge leaves the repo clean");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agents_report_cli_merges_and_ignores_junk() {
    let dir = scratch("report");
    let paths = Paths { agents: dir.join("agents"), wt: dir.join("wt") };
    let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(store::report(&paths, &a(&["--task", "../evil", "--state", "idle"]), None), 0);
    assert_eq!(store::report(&paths, &a(&["--task", "abc", "--state", "bogus"]), None), 0);
    assert!(!paths.status_dir().exists() || std::fs::read_dir(paths.status_dir()).unwrap().count() == 0);
    store::report(&paths, &a(&["--task", "abc", "--state", "session"]), Some(r#"{"session_id":"S","transcript_path":"T"}"#));
    store::report(&paths, &a(&["--task", "abc", "--state", "blocked"]), Some(r#"{"message":"Claude is waiting for your input","notification_type":"idle_prompt"}"#));
    let st = store::read_status(&paths.status("abc")).unwrap();
    assert_eq!((st.state.as_str(), st.session_id.as_str(), st.transcript_path.as_str()), ("idle", "S", "T"));
    store::report(&paths, &a(&["--task", "abc", "--state", "running"]), Some("not json"));
    assert_eq!(store::read_status(&paths.status("abc")).unwrap().state, "running");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agents_cost_from_transcript_dedupes() {
    let line = |id: &str, req: &str, model: &str, i: u64, o: u64| {
        format!(r#"{{"type":"assistant","requestId":"{req}","message":{{"id":"{id}","model":"{model}","usage":{{"input_tokens":{i},"output_tokens":{o},"cache_read_input_tokens":1000000,"cache_creation":{{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0}}}}}}}}"#)
    };
    let text = [line("m1", "r1", "claude-sonnet-5", 1_000_000, 0), line("m1", "r1", "claude-sonnet-5", 1_000_000, 0), line("m2", "r2", "claude-opus-5-5", 0, 1_000_000), r#"{"type":"user"}"#.into()].join("\n");
    let c = cost::claude_lines(std::io::Cursor::new(text), &mut Default::default());
    // sonnet 5: 1M in ($2) + 1M cache read ($0.20); opus 5.5: 1M out ($20) + 1M cache read ($0.20)
    assert!((c.usd - 22.4).abs() < 1e-6, "{}", c.usd);
    let codex = r#"{"type":"session_meta","payload":{"cwd":"C:\\w\\x"}}
{"type":"turn_context","payload":{"model":"gpt-5.6-terra"}}
{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1000000,"cached_input_tokens":500000,"output_tokens":100000}}}}"#;
    let (cwd, c) = cost::codex_lines(std::io::Cursor::new(codex));
    assert_eq!(cwd, "C:\\w\\x");
    assert!((c.usd - (1.0 + 0.1 + 1.2)).abs() < 1e-6, "{}", c.usd);
}

/// The planner's cards (P), as its thread would send them: no worker yet (the board spreads them).
pub(super) fn plan_out(items: &[(&str, &str)], cost: f64) -> PlanOut {
    let items = items.iter().enumerate().map(|(i, (t, g))| plan::Item { key: format!("t{}", i + 1), title: t.to_string(), goal: g.to_string(), ..Default::default() }).collect();
    PlanOut { items, notes: vec![], problems: vec![], cost }
}

#[test]
fn agents_plan_parse_and_args() {
    let out = r#"{"type":"result","result":"Here you go:\n[{\"title\":\"Fix resize\",\"prompt\":\"Do it\",\"owns\":[\"src/term.rs\"],\"acceptance\":\"cargo test term\"},{\"id\":\"t\",\"title\":\"Add tests\",\"depends_on\":[\"t1\"]}]","total_cost_usd":0.12}"#;
    let (items, usd) = parse_plan(out).unwrap();
    assert_eq!(items.iter().map(|i| (i.key.as_str(), i.title.as_str(), i.goal.as_str())).collect::<Vec<_>>(), vec![("t1", "Fix resize", "Do it"), ("t", "Add tests", "Add tests")]);
    assert_eq!((items[0].owns.clone(), items[0].acceptance.as_str()), (vec!["src/term.rs".to_string()], "cargo test term"));
    assert_eq!(items[1].depends_on, vec!["t1".to_string()]);
    assert_eq!(usd, 0.12);
    assert_eq!(agent_args("claude", "sonnet", "go", false), vec!["--model", "sonnet", "go"]);
    assert_eq!(agent_args("claude", "", "more", true), vec!["--continue", "more"]);
    assert_eq!(agent_args("codex", "", "more", true), vec!["resume", "--last", "more"]);
    assert_eq!(agent_args("kimi", "", "go", false), vec!["-p", "go"]);
    assert_eq!(store::slug("Fix the PTY resize!", "abc123"), "fix-the-pty-resize-c123");
}

#[test]
fn agents_terminal_opens_in_the_selected_worktree() {
    let dir = scratch("cwd");
    let mut p = demo(&dir);
    // TODO column, no worktree yet: the repo
    assert_eq!(p.cwd(), p.repo.as_ref().map(|r| r.root.clone()));
    // a running task with its worktree on disk: there
    let wt = dir.join("wt").join("fix-resize");
    std::fs::create_dir_all(&wt).unwrap();
    let id = p.store.tasks.iter().find(|t| t.status == Status::Running).unwrap().id.clone();
    p.store.tasks.iter_mut().find(|t| t.id == id).unwrap().worktree = wt.display().to_string();
    p.select(&id);
    assert_eq!(p.cwd(), Some(wt));
}

/// A board with a card in every state, for the snapshots.
pub(super) fn demo(dir: &Path) -> Agents {
    let mut p = Agents::with_paths(Paths { agents: dir.join("agents"), wt: dir.join("wt") });
    p.booted = true;
    p.repo_loading = false;
    p.repo = Some(git::RepoInfo { root: PathBuf::from(if cfg!(windows) { r"C:\code\demo" } else { "/home/you/code/demo" }), name: "demo".into(), branch: "master".into() });
    p.agents = KINDS.iter().map(|k| (k.0, if k.0 == "kimi" { None } else { Some(PathBuf::from(k.0)) })).collect();
    let repo = p.repo_key();
    let now = store::now();
    let mut add = |title: &str, agent: &str, model: &str, status: Status, f: &dyn Fn(&mut Task)| {
        let mut t = Task { id: format!("t{}", p.store.tasks.len()), repo: repo.clone(), title: title.into(), agent: agent.into(), model: model.into(), status, created: now - 900, started: now - 300, ..Default::default() };
        f(&mut t);
        p.store.tasks.push(t);
    };
    add("Theme toggle in the palette", "claude", "sonnet", Status::Todo, &|t| t.prompt = "Add a 'toggle light/dark' command to the palette that flips between the current theme and its light variant".into());
    add("Kimi usage parser", "codex", "", Status::Todo, &|t| t.prompt = "Parse ~/.kimi-code wire.jsonl StatusUpdate records into daily token totals".into());
    add("Fix pty resize on split", "codex", "gpt-5.6-terra", Status::Running, &|t| {
        t.last = "Bash cargo test term::resize".into();
        t.tokens = 48_200;
        t.cost_usd = 0.21;
        t.started = now - 250;
    });
    add("ais pane: plan bars", "claude", "opus", Status::Blocked, &|t| {
        t.question = "Claude needs your permission to use Bash: rm -rf target/snap".into();
        t.cost_usd = 0.43;
    });
    add("Usage sink status line", "claude", "opus-5.5", Status::Review, &|t| {
        t.last = "ready for review".into();
        (t.added, t.removed, t.files, t.cost_usd) = (212, 40, 6, 0.61);
        t.finished = now - 60;
        t.followups = 1;
    });
    add("Font cache warmup", "claude", "haiku", Status::Done, &|t| {
        t.outcome = "merged".into();
        t.last = "merged into master as 3f9c2e1".into();
        (t.added, t.removed, t.files, t.cost_usd) = (38, 12, 2, 0.08);
        t.finished = now - 3000;
    });
    add("Try a GPU text renderer", "codex", "", Status::Done, &|t| {
        t.outcome = "discarded".into();
        t.last = "discarded".into();
        (t.added, t.removed, t.files) = (940, 18, 11);
        t.finished = now - 7000;
    });
    p.store.repos = vec![repo, if cfg!(windows) { r"C:\code\website".into() } else { "/home/you/code/website".into() }];
    p
}

#[test]
fn agents_snapshots_board_form_diff() {
    let dir = scratch("snap");
    let mut k = Kit::new();
    let mut p = demo(&dir);
    p.col = 2;
    let board = snap(&mut k, &mut p, "board");
    println!("{board}");
    for s in ["TODO", "RUNNING", "REVIEW", "DONE", "BLOCKED", "+212 −40", "$0.61", "Fix pty resize", "master"] {
        assert!(board.contains(s), "board missing {s:?}");
    }
    let side = k.render_side(&mut p, 30, 16);
    println!("{side}");
    assert!(side.contains("new task") && side.contains("website") && side.contains("running"));
    assert_eq!(p.badge().as_deref(), Some("1 running · 1 ⚠"));

    // the new-task form, part filled in
    k.key(&mut p, KeyCode::Char('n'));
    k.typ(&mut p, "Sidebar: agent status dots");
    k.key(&mut p, KeyCode::Tab);
    p.paste("Show a coloured dot per running task next to the agents entry in the sidebar.\nUse the theme's accent for running and danger for blocked.", &mut crate::pane::Cx {
        id: 1,
        theme: &k.theme,
        config: &k.config,
        tx: &k.tx,
        actions: &mut vec![],
        focused: true,
        time: 1.0,
    });
    let form = snap(&mut k, &mut p, "form");
    println!("{form}");
    assert!(form.contains("new task") && form.contains("add & start") && form.contains("not installed"));
    k.key(&mut p, KeyCode::Esc);

    // the diff view, from a parsed diff
    let text = "diff --git a/src/panes/ais.rs b/src/panes/ais.rs\nindex 1..2 100644\n--- a/src/panes/ais.rs\n+++ b/src/panes/ais.rs\n@@ -1,6 +1,8 @@ //! your AIs\n use crate::pane::{Cx, Pane};\n-use ratatui::Frame;\n+use ratatui::{Frame, layout::Rect};\n+use serde_json::Value;\n \n pub fn cli(args: &[String]) -> i32 {\n-    0\n+    let stdin = read_stdin();\n+    sink(args, stdin)\n }\n@@ -40,3 +42,4 @@ impl Ais {\n     fn bars(&self) {\n+        // five-hour and weekly windows\n     }\ndiff --git a/src/usage.rs b/src/usage.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/usage.rs\n@@ -0,0 +1,3 @@\n+//! usage sink\n+pub struct Usage;\n+impl Usage {}\n";
    let files = git::parse_diff(text);
    assert_eq!(files.len(), 2);
    assert_eq!((files[0].added, files[0].removed), (5, 2));
    let id = p.store.tasks[4].id.clone();
    p.store.tasks[4].branch = "oriel/usage-sink-status-li-t4".into();
    p.store.tasks[4].base_branch = "master".into();
    p.mode = Mode::Diff(DiffView { data: Some(Ok(git::Diff { files, conflicts: Some(vec![]), target: "master".into() })), ..DiffView::new(&id) });
    let diff = snap(&mut k, &mut p, "diff");
    println!("{diff}");
    assert!(diff.contains("merges cleanly") && diff.contains("serde_json::Value") && diff.contains("usage.rs"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The demo board on a repo path that doesn't exist (in the scratch folder): any git work a test sets off in the
/// background fails at once instead of reaching a real folder.
pub(super) fn offline(dir: &Path) -> Agents {
    let mut p = demo(dir);
    let root = dir.join("no-repo");
    let key = root.display().to_string();
    for t in &mut p.store.tasks {
        t.repo = key.clone();
    }
    p.repo = Some(git::RepoInfo { root, name: "demo".into(), branch: "master".into() });
    p
}

pub(super) fn with_cx<R>(k: &mut Kit, f: impl FnOnce(&mut Cx) -> R) -> R {
    let mut cx = Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: &mut k.actions, focused: true, time: 1.0 };
    f(&mut cx)
}

pub(super) fn id_of(p: &Agents, title: &str) -> String {
    p.store.tasks.iter().find(|t| t.title == title).map(|t| t.id.clone()).unwrap_or_else(|| panic!("no task {title}"))
}

/// Running marked tasks together follows their own "after" links whatever order you marked them in, and refuses
/// what could never finish: a loop, a link to a task that isn't coming along, an agent that isn't installed
/// (B19, B18, B44). The form refuses a loop too, and takes the ids of finished tasks it comes back with.
#[test]
fn agents_batch_order_follows_links_and_refuses_loops() {
    let dir = scratch("batch-order");
    let mut k = Kit::new();
    let mut p = offline(&dir);
    let api = p.add_task("Add the API", "api", 0, "");
    let ui = p.add_task("Add the UI", "ui", 0, "");
    p.task_mut(&ui).unwrap().depends_on = vec![api.clone()];
    // UI marked first, then the API it waits for: one after another still means API first
    let run = with_cx(&mut k, |cx| p.start_batch(&[ui.clone(), api.clone()], true, "", cx)).unwrap();
    assert_eq!(p.run_ref(&run).unwrap().batch, vec![api.clone(), ui.clone()]);
    // a loop
    let x = p.add_task("Task x", "x", 0, "");
    let y = p.add_task("Task y", "y", 0, "");
    p.task_mut(&x).unwrap().depends_on = vec![y.clone()];
    p.task_mut(&y).unwrap().depends_on = vec![x.clone()];
    let e = with_cx(&mut k, |cx| p.start_batch(&[x.clone(), y.clone()], false, "", cx)).unwrap_err();
    assert!(e.contains("wait for each other") && e.contains("Task x") && e.contains("Task y"), "{e}");
    // a link to an open task that isn't marked
    let e = with_cx(&mut k, |cx| p.start_batch(&[x.clone()], false, "", cx)).unwrap_err();
    assert!(e.contains("isn't marked") && e.contains("Task y"), "{e}");
    // an agent that isn't installed (kimi, in the demo)
    let z = p.add_task("Task z", "z", 2, "");
    let e = with_cx(&mut k, |cx| p.start_batch(&[z.clone()], false, "", cx)).unwrap_err();
    assert!(e.contains("kimi") && e.contains("isn't installed"), "{e}");
    // the form: y already waits for x, so x can't wait for y
    p.task_mut(&x).unwrap().depends_on.clear();
    let e = p.resolve_after("Task y", Some(&x)).unwrap_err();
    assert!(e.contains("'Task y' already waits for 'Task x'"), "{e}");
    // finished tasks by id: a merged one is met and drops out, a discarded one never will be
    let merged = id_of(&p, "Font cache warmup");
    let discarded = id_of(&p, "Try a GPU text renderer");
    assert_eq!(p.resolve_after(&format!("{merged}, Task z"), Some(&x)).unwrap(), vec![z.clone()]);
    assert!(p.resolve_after(&discarded, Some(&x)).unwrap_err().contains("was discarded"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A task waiting on one that won't merge says so on its card instead of waiting quietly forever (B19).
#[test]
fn agents_card_names_a_dead_dependency() {
    let dir = scratch("dead-dep");
    let mut k = Kit::new();
    let mut p = offline(&dir);
    p.store.tasks.clear();
    let repo = p.repo_key();
    p.store.runs.push(store::Run { id: "r9".into(), repo: repo.clone(), goal: "together".into(), state: store::RunState::Running, branch: "oriel/batch-x".into(), manual: true, max_parallel: 1, created: store::now(), ..Default::default() });
    let task = |id: &str, title: &str, status: Status| Task { id: id.into(), key: id.into(), title: title.into(), repo: repo.clone(), run: "r9".into(), mode: "headless".into(), agent: "claude".into(), status, created: store::now(), ..Default::default() };
    p.store.tasks.push(Task { outcome: "discarded".into(), ..task("d1", "Old parser", Status::Done) });
    p.store.tasks.push(Task { queued: true, depends_on: vec!["d1".into()], prompt: "use the new parser".into(), ..task("d2", "Use the parser", Status::Todo) });
    let board = k.render(&mut p, 300, 44);
    assert!(board.contains("stuck: Old parser was discarded · x drops it"), "{board}");
    assert!(p.card(p.task("d2").unwrap())["stuck"].as_str().is_some_and(|s| s.contains("discarded")));
    // and the run doesn't wait for it: nothing else is going, so it's ready for review
    assert!(p.working("r9").is_empty());
    k.poll(&mut p);
    assert_eq!(p.run_ref("r9").unwrap().state, store::RunState::Review);
    assert!(p.run_ref("r9").unwrap().summary.contains("1 not started"), "{}", p.run_ref("r9").unwrap().summary);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A hand-made task's "after" and budget mean something without a run (B44): it won't start before what it waits
/// for is merged, and a task in a tab is stopped once its transcript says the budget is spent. A retry keeps
/// counting what the first go cost (B43).
#[test]
fn agents_hand_task_after_budget_and_retry_spend() {
    let dir = scratch("hand-after");
    let mut k = Kit::new();
    let mut p = offline(&dir);
    let first = p.add_task("First part", "one", 0, "");
    let second = p.add_task("Second part", "two", 0, "");
    p.task_mut(&second).unwrap().depends_on = vec![first.clone()];
    p.select(&second);
    k.key(&mut p, KeyCode::Enter);
    assert_eq!(p.task(&second).unwrap().status, Status::Todo, "it waits");
    assert!(k.notices().iter().any(|n| n.contains("waits for \"First part\" to be merged")), "{:?}", k.notices());
    // what it waits for was thrown away: it never will be merged, so say how to get out
    if let Some(t) = p.task_mut(&first) {
        (t.status, t.outcome) = (Status::Done, "discarded".into());
    }
    k.key(&mut p, KeyCode::Enter);
    assert_eq!(p.task(&second).unwrap().status, Status::Todo);
    assert!(k.notices().last().is_some_and(|n| n.contains("which was discarded — edit it (e)")), "{:?}", k.notices());
    // over budget in its tab
    let run = id_of(&p, "Fix pty resize on split");
    p.task_mut(&run).unwrap().budget_usd = 0.20;
    with_cx(&mut k, |cx| p.on_msg(Msg::Refreshed(run.clone(), None, Some(cost::Cost { usd: 0.25, tokens: 900 })), cx));
    let t = p.task(&run).unwrap().clone();
    assert!(t.error.contains("stopped at its budget ($0.25 of $0.20)"), "{t:?}");
    assert!(k.actions.iter().any(|a| matches!(a, Action::CloseTag(tag) if *tag == t.tag())), "its tab is closed");
    // telling it to carry on anyway lifts the budget (or the new tab would close at once)
    p.task_mut(&run).unwrap().worktree = dir.display().to_string();
    p.fake_agent = Some(noop_agent()); // the tab it opens runs a no-op, never a real agent
    with_cx(&mut k, |cx| p.comment(&run, "finish the last bit", cx));
    assert_eq!(p.task(&run).unwrap().budget_usd, 0.0);
    assert!(k.notices().iter().any(|n| n.contains("lifted for this follow-up")), "{:?}", k.notices());
    k.actions.clear(); // drops the tab the comment opened
    // retry: what it spent still counts today
    let rev = id_of(&p, "Usage sink status line");
    let before = p.today();
    with_cx(&mut k, |cx| p.on_msg(Msg::Removed(rev.clone(), Ok(()), Then::Retry), cx));
    let t = p.task(&rev).unwrap();
    assert_eq!((t.cost_usd, t.spent_before), (0.0, 0.61));
    assert!((p.today() - before).abs() < 1e-9, "today's total didn't drop");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Marks belong to this repo's TODO cards (B45): deleted ones and other repos' go, and the planner's cards land in
/// the repo it read even if the board moved meanwhile, without yanking the cursor.
#[test]
fn agents_marks_and_plans_stay_with_their_repo() {
    let dir = scratch("marks");
    let mut k = Kit::new();
    let mut p = demo(&dir);
    let (a, b) = (id_of(&p, "Theme toggle in the palette"), id_of(&p, "Kimi usage parser"));
    p.marked = vec![a.clone(), b.clone()];
    p.store.tasks.retain(|t| t.id != a);
    k.poll(&mut p);
    assert_eq!(p.marked, vec![b.clone()], "a deleted card's mark goes");
    let here = p.repo_key();
    let other = if cfg!(windows) { r"C:\code\website" } else { "/home/you/code/website" }.to_string();
    p.col = 2;
    with_cx(&mut k, |cx| p.on_msg(Msg::Plan(other.clone(), Ok(plan_out(&[("Hero image", "add it")], 0.0))), cx));
    assert_eq!(p.store.tasks.iter().find(|t| t.title == "Hero image").map(|t| t.repo.clone()), Some(other.clone()));
    assert_eq!(p.col, 2, "the cursor stayed");
    assert!(k.notices().iter().any(|n| n.contains("planned 1 tasks in website")), "{:?}", k.notices());
    // another repo: this one's marks don't come along
    p.set_repo(git::RepoInfo { root: PathBuf::from(&other), name: "website".into(), branch: "main".into() });
    assert!(p.marked.is_empty());
    assert_ne!(here, p.repo_key());
    let _ = std::fs::remove_dir_all(&dir);
}

/// No second git step for a task while one runs (B46): the diff view's and the board's keys refuse with the
/// reason, and a worker that exits after its task was merged doesn't bring it back.
#[test]
fn agents_busy_task_refuses_and_done_stays_done() {
    let dir = scratch("busy");
    let mut k = Kit::new();
    let mut p = demo(&dir);
    let id = id_of(&p, "Usage sink status line");
    p.task_mut(&id).unwrap().worktree = dir.display().to_string();
    p.live(&id).busy = true;
    p.mode = Mode::Diff(DiffView::new(&id));
    for key in ['c', 'm', 'x'] {
        k.key(&mut p, KeyCode::Char(key));
        assert!(matches!(p.mode, Mode::Diff(_)), "{key} refused in the diff view");
    }
    assert!(k.notices().iter().filter(|n| n.contains("git step")).count() >= 3, "{:?}", k.notices());
    p.mode = Mode::Board;
    p.select(&id);
    k.key(&mut p, KeyCode::Char('x'));
    assert!(matches!(p.mode, Mode::Board), "no discard while it's busy");
    // a transcript's c, on a headless task being merged
    p.live(&id).busy = false;
    p.merging = Some(id.clone());
    p.task_mut(&id).unwrap().mode = "headless".into();
    p.mode = Mode::Log(LogView { target: LogTarget::Task(id.clone()), scroll: 0, back: None });
    k.key(&mut p, KeyCode::Char('c'));
    assert!(matches!(p.mode, Mode::Log(_)), "no follow-up mid-merge");
    assert!(k.notices().last().is_some_and(|n| n.contains("being merged")));
    // merged, then its worker exits: it stays merged
    p.merging = None;
    if let Some(t) = p.task_mut(&id) {
        (t.status, t.outcome) = (Status::Done, "merged".into());
    }
    p.live(&id).stop = Some(Arc::new(AtomicBool::new(true)));
    with_cx(&mut k, |cx| p.on_worker_done(&id, run::Outcome { stopped: true, ..Default::default() }, cx));
    assert_eq!(p.task(&id).map(|t| (t.status, t.outcome.clone())), Some((Status::Done, "merged".to_string())));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The cursor stays on the card you picked while others arrive and leave (REVIEW is newest first), and a
/// discard elsewhere doesn't pull it into DONE.
#[test]
fn agents_selection_stays_on_its_card() {
    let dir = scratch("selection");
    let mut k = Kit::new();
    let mut p = demo(&dir);
    let old = p.add_task("Older review", "x", 0, "");
    if let Some(t) = p.task_mut(&old) {
        (t.status, t.finished) = (Status::Review, store::now() - 5000);
    }
    k.render(&mut p, 150, 44);
    p.col = 2;
    k.key(&mut p, KeyCode::Char('j'));
    assert_eq!(p.selected().as_deref(), Some(old.as_str()));
    // a worker finishes: its card lands on top of REVIEW
    let new = p.add_task("Fresh result", "y", 0, "");
    if let Some(t) = p.task_mut(&new) {
        (t.status, t.finished) = (Status::Review, store::now());
    }
    k.poll(&mut p);
    assert_eq!(p.selected().as_deref(), Some(old.as_str()), "still on the card you picked");
    // another task is discarded in the background
    let blocked = id_of(&p, "ais pane: plan bars");
    with_cx(&mut k, |cx| p.on_msg(Msg::Removed(blocked, Ok(()), Then::Discarded), cx));
    k.poll(&mut p);
    assert_eq!((p.col, p.selected()), (2, Some(old.clone())), "the cursor didn't jump to DONE");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Another app's /lead or /task opens this one and pastes right away (L or n, then the text): it lands in the lead's
/// goal or the task's prompt, even on a fresh pane that doesn't know its repo yet. The form's after and budget
/// fields and the comment box take a paste too.
#[test]
fn agents_paste_reaches_the_form_opened_for_it() {
    let dir = scratch("paste");
    let repo = temp_repo(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo);
    // L arrives before the repo is detected, the goal right after it
    k.key(&mut p, KeyCode::Char('L'));
    assert!(p.repo.is_none() && matches!(p.mode, Mode::Board), "still detecting the repo");
    with_cx(&mut k, |cx| p.paste("Add dark mode\nto every pane", cx));
    until(&mut k, &mut p, 5000, "repo", |p| p.repo.is_some());
    match &p.mode {
        Mode::LeadForm(f) => assert_eq!(f.goal.text, "Add dark mode\nto every pane"),
        _ => panic!("the lead form should be open"),
    }
    k.key(&mut p, KeyCode::Esc);
    // n, then a task description: the prompt, not the one-line title
    k.key(&mut p, KeyCode::Char('n'));
    let long = "Fix the login button on narrow windows: it stops responding once the sidebar is open";
    with_cx(&mut k, |cx| p.paste(long, cx));
    let Mode::Form(f) = &mut p.mode else { panic!("the task form should be open") };
    assert_eq!((f.title.text.as_str(), f.prompt.text.as_str(), f.field), ("", long, 1));
    // a short line on the title is a title; after and budget take theirs
    f.field = 0;
    f.prompt = Input::new("", true);
    with_cx(&mut k, |cx| p.paste("Fix login", cx));
    for (field, text) in [(5, "k3f9"), (6, " 1.5 ")] {
        if let Mode::Form(f) = &mut p.mode {
            f.field = field;
        }
        with_cx(&mut k, |cx| p.paste(text, cx));
    }
    let Mode::Form(f) = &p.mode else { panic!() };
    assert_eq!((f.title.text.as_str(), f.prompt.text.as_str(), f.after.text.as_str(), f.budget.text.as_str()), ("Fix login", "", "k3f9", "1.5"));
    // the comment box
    p.mode = Mode::Comment("x".into(), Input::new("", true));
    with_cx(&mut k, |cx| p.paste("use the default", cx));
    assert!(matches!(&p.mode, Mode::Comment(_, i) if i.text == "use the default"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agents_snapshots_empty_and_picker() {
    let dir = scratch("snap2");
    let mut k = Kit::new();
    let mut p = demo(&dir);
    p.store.tasks.clear();
    let empty = snap(&mut k, &mut p, "empty");
    assert!(empty.contains("no tasks yet") && empty.contains("new task"), "{empty}");
    k.key(&mut p, KeyCode::Char('o'));
    assert!(matches!(p.mode, Mode::Repo(_)));
    let picker = snap(&mut k, &mut p, "repo");
    println!("{picker}");
    assert!(picker.contains("open a repo") && picker.contains("website"));
    // a path that isn't a repo: an error, the picker stays
    k.typ(&mut p, &dir.join("nope").display().to_string());
    k.key(&mut p, KeyCode::Enter);
    until(&mut k, &mut p, 5000, "checked", |p| matches!(&p.mode, Mode::Repo(pk) if !pk.checking));
    let Mode::Repo(pk) = &p.mode else { unreachable!() };
    assert!(pk.err.contains("isn't a folder"), "{}", pk.err);
    let _ = std::fs::remove_dir_all(&dir);
}


/// The task form has an acceptance row: what's typed there is the task's acceptance command (the worker is told
/// to make it pass, and a run's merge gate runs it), and editing the task shows it again.
#[test]
fn agents_form_acceptance() {
    let dir = scratch("form-acceptance");
    let repo = temp_repo(&dir);
    let mut k = Kit::new();
    let mut p = pane(&dir, &repo);
    k.render(&mut p, 150, 44);
    until(&mut k, &mut p, 5000, "repo detected", |p| p.repo.is_some());
    k.key(&mut p, KeyCode::Char('n'));
    k.typ(&mut p, "Parser");
    for _ in 0..7 {
        k.key(&mut p, KeyCode::Tab);
    }
    k.typ(&mut p, "cargo test parser");
    let form = k.render_html(&mut p, 150, 44, "target/snap/agents-form-acceptance.html");
    assert!(form.contains("acceptance (a command that proves it works)") && form.contains("cargo test parser") && form.contains("add & start"), "{form}");
    k.key_mod(&mut p, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert_eq!(only(&p).acceptance, "cargo test parser");
    k.key(&mut p, KeyCode::Char('e'));
    let Mode::Form(f) = &p.mode else { panic!("e edits the task") };
    assert_eq!(f.acceptance.text, "cargo test parser");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The merge queue's tamper scan: an added #[ignore] / skip / xfail, or a file losing more asserts than it gains,
/// is flagged; changing an assert, or adding tests, isn't.
#[test]
fn agents_tamper_scan() {
    let diff = "diff --git a/tests/parse.rs b/tests/parse.rs\n--- a/tests/parse.rs\n+++ b/tests/parse.rs\n@@ -3,0 +4 @@\n+#[ignore]\n@@ -9,2 +10,0 @@\n-    assert_eq!(parse(\"1\"), 1);\n-    assert!(ok);\n\
diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-    assert_eq!(x, 1);\n+    assert_eq!(x, 2);\n\
diff --git a/web/app.test.js b/web/app.test.js\n--- a/web/app.test.js\n+++ b/web/app.test.js\n@@ -1 +1 @@\n-it('adds', () => {\n+it.skip('adds', () => {\n\
diff --git a/py/test_x.py b/py/test_x.py\ndeleted file mode 100644\n--- a/py/test_x.py\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-def test_x():\n-    assert f() == 2\n";
    let hits = git::tamper_scan(diff);
    assert_eq!(hits, ["tests/parse.rs: #[ignore added", "tests/parse.rs: 2 asserts removed", "web/app.test.js: it.skip( added", "py/test_x.py: 1 assert removed"], "{hits:?}");
    let fine = "diff --git a/tests/new.rs b/tests/new.rs\n--- /dev/null\n+++ b/tests/new.rs\n@@ -0,0 +1,2 @@\n+#[test]\n+fn t() { assert!(true); }\n";
    assert!(git::tamper_scan(fine).is_empty());
    // a diff without its --- / +++ header lines counts all the same
    assert_eq!(git::tamper_scan("diff --git a/x.py b/x.py\n@@ -1 +0,0 @@\n-assert a\n"), ["x.py: 1 assert removed"]);
}
