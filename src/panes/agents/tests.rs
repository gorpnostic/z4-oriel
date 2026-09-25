//! Headless tests for the orchestrator. Everything git happens in throwaway repos under target/test-scratch;
//! no real agent runs (a no-op command stands in), and hooks are simulated by calling `store::report`.

use super::*;
use crate::testkit::Kit;
use crossterm::event::KeyCode;

fn scratch(name: &str) -> PathBuf {
    let nanos = SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-scratch").join(format!("agents-{name}-{nanos}"));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn sh(dir: &Path, args: &[&str]) -> String {
    let o = git::run(dir, args);
    assert!(o.ok, "git {args:?} failed: {}", o.stderr);
    o.stdout.trim().to_string()
}

/// A fresh repo with one commit on master.
fn temp_repo(dir: &Path) -> PathBuf {
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

fn noop_agent() -> (String, Vec<String>) {
    if cfg!(windows) { ("cmd.exe".into(), vec!["/c".into(), "exit".into()]) } else { ("true".into(), vec![]) }
}

fn pane(dir: &Path, repo: &Path) -> Agents {
    let mut a = Agents::with_paths(Paths { agents: dir.join("agents"), wt: dir.join("wt") });
    a.start_dir = Some(repo.to_path_buf());
    a.fake_agent = Some(noop_agent());
    a
}

/// Poll until `cond` holds (or fail after `ms`).
fn until(k: &mut Kit, p: &mut Agents, ms: u64, what: &str, cond: impl Fn(&Agents) -> bool) {
    let deadline = Instant::now() + Duration::from_millis(ms);
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

#[test]
fn agents_plan_parse_and_args() {
    let out = r#"{"type":"result","result":"Here you go:\n[{\"title\":\"Fix resize\",\"prompt\":\"Do it\"},{\"title\":\"Add tests\"}]","total_cost_usd":0.12}"#;
    let (items, usd) = parse_plan(out).unwrap();
    assert_eq!(items, vec![("Fix resize".into(), "Do it".into()), ("Add tests".into(), "Add tests".into())]);
    assert_eq!(usd, 0.12);
    assert_eq!(agent_args("claude", "sonnet", "go", false), vec!["--model", "sonnet", "go"]);
    assert_eq!(agent_args("claude", "", "more", true), vec!["--continue", "more"]);
    assert_eq!(agent_args("codex", "", "more", true), vec!["resume", "--last", "more"]);
    assert_eq!(agent_args("kimi", "", "go", false), vec!["-p", "go"]);
    assert_eq!(store::slug("Fix the PTY resize!", "abc123"), "fix-the-pty-resize-c123");
}

/// A board with a card in every state, for the snapshots.
fn demo(dir: &Path) -> Agents {
    let mut p = Agents::with_paths(Paths { agents: dir.join("agents"), wt: dir.join("wt") });
    p.booted = true;
    p.repo_loading = false;
    p.repo = Some(git::RepoInfo { root: PathBuf::from(if cfg!(windows) { r"C:\Code\devtools\oriel" } else { "/home/leif/code/oriel" }), name: "oriel".into(), branch: "master".into() });
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
    p.store.repos = vec![repo, if cfg!(windows) { r"C:\Code\games\rnr-drift".into() } else { "/home/leif/code/rnr-drift".into() }];
    p
}

#[test]
fn agents_snapshots_board_form_diff() {
    let dir = scratch("snap");
    let mut k = Kit::new();
    let mut p = demo(&dir);
    p.col = 1;
    p.row[1] = 0;
    let board = snap(&mut k, &mut p, "board");
    println!("{board}");
    for s in ["TODO", "RUNNING", "REVIEW", "DONE", "BLOCKED", "+212 −40", "$0.61", "Fix pty resize", "master"] {
        assert!(board.contains(s), "board missing {s:?}");
    }
    let side = k.render_side(&mut p, 30, 16);
    println!("{side}");
    assert!(side.contains("new task") && side.contains("rnr-drift") && side.contains("running"));
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
    p.mode = Mode::Diff(DiffView { id, data: Some(Ok(git::Diff { files, conflicts: Some(vec![]), target: "master".into() })), file: 0, scroll: 0 });
    let diff = snap(&mut k, &mut p, "diff");
    println!("{diff}");
    assert!(diff.contains("merges cleanly") && diff.contains("serde_json::Value") && diff.contains("usage.rs"));
    let _ = std::fs::remove_dir_all(&dir);
}
