//! Lead mode, headless: fake leads and fake workers drive the real board, git, merge queue and gate in
//! throwaway repos under target/test-scratch. No real agent runs (the one live test is #[ignore]).
//!
//! Fake workers follow a tiny script in their task's goal: `write FILE: CONTENT` (\n for newlines),
//! `cost 0.10`, `spin` (repeat one call until the watchdog steps in). Follow-ups about conflict markers,
//! a failed gate or the watchdog are handled the way a real worker would.

use super::stream::{Entry, Ev};
use super::tests::{noop_agent, scratch, sh, temp_repo, until};
use super::*;
use crate::config::RosterEntry;
use crate::testkit::Kit;
use crossterm::event::KeyCode;
use serde_json::{Value, json};
use std::sync::atomic::AtomicUsize;

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if name == ".git" || name == ".claude" || name == ".kimi-code" {
            continue;
        }
        if p.is_dir() {
            files_under(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// A worker that does what its goal's script says.
fn fake_worker(seen: Arc<Mutex<Vec<String>>>) -> run::Fake {
    let n = Arc::new(AtomicUsize::new(0));
    Arc::new(move |spec: &run::Spec, stop: &AtomicBool, on: &mut dyn FnMut(Ev)| -> run::Outcome {
        let p = spec.prompt.clone();
        let first = p.lines().find(|l| l.starts_with("TASK ")).or_else(|| p.lines().next()).unwrap_or("").to_string();
        seen.lock().unwrap().push(first.clone());
        let session = if spec.resume.is_empty() { format!("fake-{}", n.fetch_add(1, Ordering::SeqCst)) } else { spec.resume.clone() };
        on(Ev::Session(session.clone()));
        let cwd = spec.cwd.clone();
        let mut cost = 0.05;
        let mut k = 0;
        let mut tool = |label: &str, target: &str, on: &mut dyn FnMut(Ev)| {
            k += 1;
            on(Ev::Entry(Entry { kind: 't', id: format!("c{k}"), label: label.into(), target: target.into(), status: "done".into(), ..Default::default() }));
        };
        let write = |file: &str, content: &str, on: &mut dyn FnMut(Ev)| {
            let path = cwd.join(file);
            if let Some(d) = path.parent() {
                std::fs::create_dir_all(d).unwrap();
            }
            std::fs::write(&path, content).unwrap();
            on(Ev::Touched(file.to_string()));
        };
        let mut summary = String::new();
        if p.contains("conflict markers") {
            let mut fs = vec![];
            files_under(&cwd, &mut fs);
            for f in fs {
                let text = std::fs::read_to_string(&f).unwrap_or_default();
                if text.contains("<<<<<<<") {
                    let kept: Vec<&str> = text.lines().filter(|l| !(l.starts_with("<<<<<<<") || l.starts_with("=======") || l.starts_with(">>>>>>>"))).collect();
                    let rel = f.strip_prefix(&cwd).unwrap().to_string_lossy().replace('\\', "/");
                    write(&rel, &(kept.join("\n") + "\n"), on);
                    tool("Edit", &rel, on);
                }
            }
            summary = "resolved the conflicts, kept both sides".into();
        } else if p.contains("gate failed") {
            let mut fs = vec![];
            files_under(&cwd, &mut fs);
            for f in fs {
                let text = std::fs::read_to_string(&f).unwrap_or_default();
                if text.contains("BROKEN") {
                    let rel = f.strip_prefix(&cwd).unwrap().to_string_lossy().replace('\\', "/");
                    write(&rel, &text.replace("BROKEN", "fixed"), on);
                    tool("Edit", &rel, on);
                }
            }
            summary = "fixed what the gate complained about".into();
        } else if p.contains("watchdog") {
            summary = "took a different approach and finished".into();
        } else {
            for line in p.lines() {
                if let Some(rest) = line.strip_prefix("write ") {
                    let (file, content) = rest.split_once(": ").unwrap_or((rest, ""));
                    write(file.trim(), &(content.replace("\\n", "\n") + "\n"), on);
                    tool("Write", file.trim(), on);
                } else if let Some(c) = line.strip_prefix("cost ") {
                    cost = c.trim().parse().unwrap_or(cost);
                } else if line.trim() == "spin" {
                    for _ in 0..6 {
                        on(Ev::Entry(Entry { kind: 't', id: format!("s{}", k), label: "Read".into(), target: "a.txt".into(), status: "done".into(), ..Default::default() }));
                        k += 1;
                    }
                    let t0 = Instant::now();
                    while !stop.load(Ordering::SeqCst) && t0.elapsed() < Duration::from_secs(20) {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    return run::Outcome { session, cost: 0.01, stopped: true, ..Default::default() };
                }
            }
            summary = format!("did {}", first.trim_start_matches("TASK "));
        }
        on(Ev::Cost(cost));
        let report = json!({"status": "done", "summary": summary, "questions": []});
        on(Ev::Report(report.clone()));
        run::Outcome { session, cost, text: summary, report: Some(report), ..Default::default() }
    })
}

/// A scripted text-protocol lead: `first` is its first reply's actions; afterwards it merges every task that
/// finished and waits, until `target` tasks are merged — then it's done.
fn fake_text_lead(first: Value, target: usize, prompts: Arc<Mutex<Vec<String>>>) -> run::Fake {
    Arc::new(move |spec: &run::Spec, _stop: &AtomicBool, on: &mut dyn FnMut(Ev)| -> run::Outcome {
        let p = spec.prompt.clone();
        prompts.lock().unwrap().push(p.clone());
        on(Ev::Session("lead-1".into()));
        let actions = if p.contains("Start with roster") {
            first.clone()
        } else {
            let mut merges = vec![];
            let mut merged = 0;
            for line in p.lines() {
                let Some((_, j)) = line.split_once(" → ") else { continue };
                let Ok(v) = serde_json::from_str::<Value>(j) else { continue };
                for e in v["events"].as_array().into_iter().flatten() {
                    if e["event"] == "finished" {
                        merges.push(json!({"tool": "merge", "args": {"id": e["key"]}}));
                    }
                }
                if let Some(m) = v["board"]["merged"].as_array() {
                    merged = m.len();
                }
            }
            if merged >= target {
                json!([{"tool": "note", "args": {"text": "all merged"}}, {"tool": "done", "args": {"summary": format!("{merged} tasks merged")}}])
            } else {
                merges.push(json!({"tool": "wait", "args": {"timeout_s": 20}}));
                Value::Array(merges)
            }
        };
        on(Ev::Entry(Entry::say("Looking at the results.")));
        on(Ev::Cost(0.01));
        run::Outcome { session: "lead-1".into(), cost: 0.01, text: format!("Next steps.\n```json\n{}\n```", json!({ "actions": actions })), ..Default::default() }
    })
}

fn roster() -> Vec<RosterEntry> {
    let w = |name: &str, agent: &str, model: &str, tier: &str| RosterEntry { name: name.into(), agent: agent.into(), model: model.into(), tier: tier.into(), good_at: format!("{tier} work"), max_turns: 20, budget_usd: 1.0, enabled: true };
    vec![w("w1", "claude", "haiku", "cheap"), w("w2", "codex", "", "mid"), w("w3", "claude", "sonnet", "premium")]
}

fn lead_pane(dir: &Path, repo: &Path, worker: run::Fake, lead: run::Fake) -> Agents {
    let mut p = Agents::with_paths(Paths { agents: dir.join("agents"), wt: dir.join("wt") });
    p.start_dir = Some(repo.to_path_buf());
    p.fake_agent = Some(noop_agent());
    p.fake_worker = Some(worker);
    p.fake_lead = Some(lead);
    p.stagger = Duration::ZERO;
    p.roster = roster();
    p.lead_cfg.agent = "claude".into();
    p.lead_cfg.protocol = "text".into();
    p
}

/// L → type the goal → ctrl+s.
fn start(k: &mut Kit, p: &mut Agents, goal: &str) {
    k.key(p, KeyCode::Char('L'));
    assert!(matches!(p.mode, Mode::LeadForm(_)));
    k.typ(p, goal);
    k.key_mod(p, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert!(matches!(p.mode, Mode::Board), "the form closes on start");
    assert_eq!(p.store.runs.len(), 1);
}

fn gate_cmd() -> String {
    if cfg!(windows) { "findstr /m BROKEN *.txt >nul && exit /b 1 || exit /b 0".into() } else { "! grep -q BROKEN *.txt".into() }
}

fn accept_cmd(file: &str) -> String {
    if cfg!(windows) { format!("if exist {file} (exit /b 0) else (exit /b 1)") } else { format!("test -f {file}") }
}

fn run0(p: &Agents) -> store::Run {
    p.store.runs[0].clone()
}

fn by_key<'a>(p: &'a Agents, key: &str) -> &'a Task {
    p.store.tasks.iter().find(|t| t.key == key).unwrap_or_else(|| panic!("no task {key}"))
}

/// The whole thing through the text protocol: plan → 4 workers (3 at a time) → merge queue, where one task
/// conflicts (a worker edited outside its files) and one fails the gate; both go back to their workers and merge
/// on the second try → done → the user merges the integration branch into master with `m`.
#[test]
fn agents_lead_text_protocol_plan_conflict_gate_merge() {
    let dir = scratch("lead-flow");
    let repo = temp_repo(&dir);
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let prompts: Arc<Mutex<Vec<String>>> = Arc::default();
    let plan = json!([
        {"tool": "roster"},
        {"tool": "plan", "args": {"tasks": [
            {"id": "a", "title": "Change line two", "goal": "write a.txt: one\\nTWO-A\\nthree\\nfour", "worker": "w1", "owns": ["a.txt"], "size": "S"},
            {"id": "b", "title": "Add b", "goal": "write b.txt: bee\nwrite a.txt: one\\nTWO-B\\nthree\\nfour", "worker": "w2", "owns": ["b.txt"], "size": "S"},
            {"id": "c", "title": "Add c", "goal": "write c.txt: sea", "worker": "w1", "owns": ["c.txt"], "acceptance": accept_cmd("c.txt"), "size": "S"},
            {"id": "g", "title": "Add g", "goal": "write g.txt: BROKEN", "worker": "w3", "owns": ["g.txt"], "size": "S"}
        ]}},
        {"tool": "wait", "args": {"timeout_s": 20}}
    ]);
    let mut k = Kit::new();
    let mut p = lead_pane(&dir, &repo, fake_worker(seen.clone()), fake_text_lead(plan, 4, prompts.clone()));
    p.lead_cfg.gate = gate_cmd();
    k.render(&mut p, 150, 44);
    until(&mut k, &mut p, 5000, "repo", |p| p.repo.is_some());
    start(&mut k, &mut p, "Add three files and fix line two");
    until(&mut k, &mut p, 60_000, "the lead finishes", |p| !p.store.runs[0].state.active());
    let r = run0(&p);
    assert_eq!(r.state, store::RunState::Review, "run: {r:?}\nprompts: {:?}", prompts.lock().unwrap());
    assert_eq!(r.protocol, "text");
    assert!(r.branch.starts_with("oriel/lead-add-three-files"), "{}", r.branch);
    assert_eq!(r.merged, 4, "{:#?}", p.store.tasks);
    // the conflict and the gate failure went back to their workers and merged on the next try
    let (b, g) = (by_key(&p, "b"), by_key(&p, "g"));
    assert!(b.attempts >= 1 || by_key(&p, "a").attempts >= 1, "one of a/b conflicted");
    assert_eq!(g.attempts, 1, "g failed the gate once");
    assert!(p.store.tasks.iter().all(|t| t.status == Status::Done && t.outcome == "merged"));
    let log = sh(&repo, &["log", "--format=%s%n%b", &r.branch]);
    for key in ["a", "b", "c", "g"] {
        assert!(log.contains(&format!("Oriel-Task: {}", by_key(&p, key).id)), "{log}");
    }
    assert_eq!(sh(&repo, &["rev-list", "--count", &format!("{}..{}", r.base_sha, r.branch)]), "4", "one commit per task");
    let a_txt = sh(&repo, &["show", &format!("{}:a.txt", r.branch)]);
    assert!(a_txt.contains("TWO-A") && a_txt.contains("TWO-B") && !a_txt.contains("<<<<"), "{a_txt}");
    assert_eq!(sh(&repo, &["show", &format!("{}:g.txt", r.branch)]), "fixed");
    assert_eq!(sh(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "master", "the user's branch was never touched");
    assert!(!repo.join("b.txt").exists());
    let rec = &p.store.records;
    assert_eq!(rec["w3"].gate_fails, 1);
    assert!(rec.values().map(|r| r.conflicts).sum::<u32>() >= 1);
    assert_eq!(rec.values().map(|r| r.merged).sum::<u32>(), 4);
    assert!(seen.lock().unwrap().iter().any(|s| s.contains("conflict markers")) || seen.lock().unwrap().len() >= 5);
    assert!(k.notices().iter().any(|n| n.contains("lead finished")), "{:?}", k.notices());
    // what the lead heard: result cards, never transcripts
    let heard = prompts.lock().unwrap().join("\n");
    assert!(heard.contains("\"event\":\"finished\"") && heard.contains("\"event\":\"merged\"") && heard.contains("\"event\":\"bounced\""), "{heard}");
    assert!(heard.contains("\"summary\":\"did"), "cards carry the worker's summary");
    let board = k.render_html(&mut p, 150, 44, "target/snap/agents-lead.html");
    assert!(board.contains("lead") && board.contains("ready for review") && board.contains("4 merged"), "{board}");

    // the user reviews the integration branch and merges it into master
    k.key(&mut p, KeyCode::Up);
    assert!(p.lead_focus);
    k.key(&mut p, KeyCode::Char('d'));
    until(&mut k, &mut p, 8000, "run diff", |p| matches!(&p.mode, Mode::Diff(v) if v.data.is_some()));
    let diff = k.render(&mut p, 150, 44);
    assert!(diff.contains("merges cleanly into master") && diff.contains("g.txt"), "{diff}");
    k.key(&mut p, KeyCode::Char('m'));
    assert!(k.render(&mut p, 150, 44).contains("Squash-merge"));
    k.key(&mut p, KeyCode::Char('y'));
    until(&mut k, &mut p, 15_000, "merged into master", |p| p.store.runs[0].state == store::RunState::Merged || !p.store.runs[0].error.is_empty());
    assert_eq!(run0(&p).state, store::RunState::Merged, "{}", run0(&p).error);
    assert_eq!(std::fs::read_to_string(repo.join("c.txt")).unwrap().trim(), "sea");
    assert!(sh(&repo, &["log", "-1", "--format=%s"]).starts_with("Add three files"));
    assert!(!git::run(&repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{}", r.branch)]).ok, "integration branch deleted");
    assert!(!Path::new(&r.worktree).exists(), "the lead's checkout is gone");
    assert_eq!(sh(&repo, &["status", "--porcelain"]), "");
    let _ = std::fs::remove_dir_all(&dir);
}

/// An MCP lead end to end: its "CLI" reads the --mcp-config oriel wrote and speaks JSON-RPC through the real
/// stdio server loop → TCP broker → the board.
#[test]
fn agents_lead_mcp_bridge_end_to_end() {
    let dir = scratch("lead-mcp");
    let repo = temp_repo(&dir);
    let out: Arc<Mutex<String>> = Arc::default();
    let out2 = out.clone();
    let lead: run::Fake = Arc::new(move |spec: &run::Spec, _stop: &AtomicBool, on: &mut dyn FnMut(Ev)| -> run::Outcome {
        let i = spec.args.iter().position(|a| a == "--mcp-config").expect("claude gets --mcp-config");
        let cfg: Value = serde_json::from_str(&std::fs::read_to_string(&spec.args[i + 1]).unwrap()).unwrap();
        let a = &cfg["mcpServers"]["oriel"]["args"];
        assert_eq!(a[0], "mcp-lead");
        let (port, tok) = (a[1].as_str().unwrap().parse::<u16>().unwrap(), a[2].as_str().unwrap().to_string());
        assert!(spec.args.iter().any(|x| x == "--strict-mcp-config"));
        let calls = [
            json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "roster", "arguments": {}}}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "plan", "arguments": {"tasks": [{"id": "a", "title": "Add x", "goal": "write x.txt: ex", "worker": "w1", "owns": ["x.txt"], "size": "S"}, {"id": "b", "title": "Overlap", "goal": "write x.txt: nope", "worker": "w2", "owns": ["x.txt"]}]}}}),
            json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "plan", "arguments": {"tasks": [{"id": "a", "title": "Add x", "goal": "write x.txt: ex", "worker": "w1", "owns": ["x.txt"], "size": "S"}]}}}),
            json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "wait", "arguments": {"timeout_s": 20}, "_meta": {"progressToken": 5}}}),
            json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {"name": "task_diff", "arguments": {"id": "a"}}}),
            json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "merge", "arguments": {"id": "a"}}}),
            json!({"jsonrpc": "2.0", "id": 8, "method": "tools/call", "params": {"name": "wait", "arguments": {"timeout_s": 20}}}),
            json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {"name": "done", "arguments": {"summary": "x.txt added"}}}),
        ]
        .map(|v| v.to_string())
        .join("\n");
        let mut buf: Vec<u8> = vec![];
        mcp::serve(calls.as_bytes(), &mut buf, |tool, args| mcp::forward(port, &tok, tool, args));
        *out2.lock().unwrap() = String::from_utf8(buf).unwrap();
        on(Ev::Session("mcp-lead".into()));
        run::Outcome { session: "mcp-lead".into(), cost: 0.02, text: "x.txt added".into(), ..Default::default() }
    });
    let mut k = Kit::new();
    let mut p = lead_pane(&dir, &repo, fake_worker(Arc::default()), lead);
    p.lead_cfg.protocol = String::new(); // claude → MCP
    k.render(&mut p, 150, 44);
    until(&mut k, &mut p, 5000, "repo", |p| p.repo.is_some());
    start(&mut k, &mut p, "Add x");
    until(&mut k, &mut p, 30_000, "the lead finishes", |p| !p.store.runs[0].state.active());
    let r = run0(&p);
    assert_eq!((r.state, r.protocol.as_str()), (store::RunState::Review, "mcp"), "{r:?}");
    let replies: Vec<Value> = out.lock().unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let by_id = |i: i64| replies.iter().find(|v| v["id"] == i).cloned().unwrap_or_else(|| panic!("no reply {i}: {replies:?}"));
    let text = |i: i64| by_id(i)["result"]["content"][0]["text"].as_str().unwrap().to_string();
    assert_eq!(by_id(1)["result"]["tools"].as_array().unwrap().len(), mcp::TOOLS.len());
    let roster: Value = serde_json::from_str(&text(2)).unwrap();
    assert_eq!(roster["workers"].as_array().unwrap().len(), 3);
    assert_eq!(by_id(3)["result"]["isError"], true, "overlapping owns are refused: {}", text(3));
    assert!(text(3).contains("both own"), "{}", text(3));
    let plan: Value = serde_json::from_str(&text(4)).unwrap();
    assert_eq!(plan["tasks"][0]["key"], "a");
    let w1: Value = serde_json::from_str(&text(5)).unwrap();
    assert_eq!(w1["events"][0]["event"], "finished", "{w1}");
    assert_eq!(w1["events"][0]["files"][0]["path"], "x.txt");
    assert!(text(6).contains("+ex"), "{}", text(6));
    assert!(text(7).contains("queued for merging"), "{}", text(7));
    let w2: Value = serde_json::from_str(&text(8)).unwrap();
    assert_eq!(w2["events"][0]["event"], "merged", "{w2}");
    assert_eq!(sh(&repo, &["show", &format!("{}:x.txt", r.branch)]), "ex");
    assert_eq!(r.summary, "x.txt added");
    // the lead's own checkout moved to the new tip after the merge
    assert_eq!(std::fs::read_to_string(Path::new(&r.worktree).join("x.txt")).unwrap().trim(), "ex");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The run's budget stops new work: two workers at $0.10 against a $0.15 budget, the third never starts, and
/// the lead is told why when it asks for more.
#[test]
fn agents_lead_budget_cap_stops_spawning() {
    let dir = scratch("lead-budget");
    let repo = temp_repo(&dir);
    let prompts: Arc<Mutex<Vec<String>>> = Arc::default();
    let p2 = prompts.clone();
    let lead: run::Fake = Arc::new(move |spec: &run::Spec, _stop: &AtomicBool, _on: &mut dyn FnMut(Ev)| -> run::Outcome {
        let n = {
            let mut v = p2.lock().unwrap();
            v.push(spec.prompt.clone());
            v.len()
        };
        let actions = match n {
            1 => json!([{"tool": "plan", "args": {"tasks": [
                {"id": "a", "title": "A", "goal": "write a1.txt: a\ncost 0.10", "worker": "w1", "owns": ["a1.txt"], "size": "S"},
                {"id": "b", "title": "B", "goal": "write b1.txt: b\ncost 0.10", "worker": "w1", "owns": ["b1.txt"], "size": "S"},
                {"id": "c", "title": "C", "goal": "write c1.txt: c\ncost 0.10", "worker": "w1", "owns": ["c1.txt"], "size": "S"}
            ]}}, {"tool": "wait", "args": {"timeout_s": 10, "ids": ["a"]}}]),
            2 => json!([{"tool": "wait", "args": {"timeout_s": 10, "ids": ["b"]}}]),
            3 => json!([{"tool": "spawn_task", "args": {"worker": "w1", "title": "D", "goal": "write d.txt: d", "owns": ["d.txt"]}}]),
            _ => json!([{"tool": "done", "args": {"summary": "out of budget"}}]),
        };
        run::Outcome { session: "lead-b".into(), text: format!("```json\n{}\n```", json!({ "actions": actions })), ..Default::default() }
    });
    let mut k = Kit::new();
    let mut p = lead_pane(&dir, &repo, fake_worker(Arc::default()), lead);
    p.lead_cfg.max_parallel = 1;
    k.render(&mut p, 150, 44);
    until(&mut k, &mut p, 5000, "repo", |p| p.repo.is_some());
    k.key(&mut p, KeyCode::Char('L'));
    k.typ(&mut p, "Three small files");
    // budget field: tab to it, clear, type 0.15; one worker at a time
    for _ in 0..3 {
        k.key(&mut p, KeyCode::Tab);
    }
    k.key_mod(&mut p, KeyCode::Char('u'), KeyModifiers::CONTROL);
    k.typ(&mut p, "1");
    k.key(&mut p, KeyCode::Tab);
    k.key_mod(&mut p, KeyCode::Char('u'), KeyModifiers::CONTROL);
    k.typ(&mut p, "0.15");
    k.key_mod(&mut p, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert_eq!(run0(&p).budget_usd, 0.15);
    assert_eq!(run0(&p).max_parallel, 1);
    until(&mut k, &mut p, 30_000, "the lead finishes", |p| p.store.runs.first().is_some_and(|r| !r.state.active()));
    let (a, b, c) = (by_key(&p, "a").clone(), by_key(&p, "b").clone(), by_key(&p, "c").clone());
    assert_eq!((a.status, b.status), (Status::Review, Status::Review));
    assert_eq!(c.status, Status::Todo, "the third never started: {c:?}");
    assert!(c.worktree.is_empty());
    assert!((p.spend(&run0(&p).id) - 0.20).abs() < 1e-9);
    assert!(k.notices().iter().any(|n| n.contains("hit its budget")), "{:?}", k.notices());
    let heard = prompts.lock().unwrap().join("\n");
    assert!(heard.contains("budget is spent"), "the lead was told: {heard}");
    assert_eq!(p.store.tasks.len(), 3, "spawn_task was refused");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A worker that repeats itself gets nudged by the watchdog (stopped, then resumed in the same session with a
/// different-approach prompt); a finished headless task can be taken over in a terminal tab with `t`.
#[test]
fn agents_lead_watchdog_nudge_and_take_over() {
    let dir = scratch("lead-watch");
    let repo = temp_repo(&dir);
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let plan = json!([{"tool": "plan", "args": {"tasks": [{"id": "s", "title": "Spinner", "goal": "spin", "worker": "w1", "owns": ["s.txt"], "size": "S"}]}}, {"tool": "wait", "args": {"timeout_s": 20}}]);
    let lead: run::Fake = {
        let base = fake_text_lead(plan, 99, Arc::default());
        Arc::new(move |spec: &run::Spec, stop: &AtomicBool, on: &mut dyn FnMut(Ev)| {
            if spec.prompt.contains("\"event\":\"finished\"") {
                return run::Outcome { session: "lead-1".into(), text: "```json\n{\"actions\":[{\"tool\":\"done\",\"args\":{\"summary\":\"ok\"}}]}\n```".into(), ..Default::default() };
            }
            base(spec, stop, on)
        })
    };
    let mut k = Kit::new();
    let mut p = lead_pane(&dir, &repo, fake_worker(seen.clone()), lead);
    k.render(&mut p, 150, 44);
    until(&mut k, &mut p, 5000, "repo", |p| p.repo.is_some());
    start(&mut k, &mut p, "Spin a bit");
    until(&mut k, &mut p, 30_000, "the lead finishes", |p| !p.store.runs[0].state.active());
    let t = by_key(&p, "s").clone();
    assert_eq!(t.status, Status::Review, "{t:?}");
    assert_eq!(t.followups, 1, "one nudge");
    assert!(seen.lock().unwrap().iter().any(|s| s.contains("watchdog")), "{:?}", seen.lock().unwrap());
    let log: Vec<String> = p.live[&t.id].log.iter().map(|e| e.line()).collect();
    assert!(log.iter().any(|l| l.contains("watchdog: stuck")), "{log:?}");
    // the live transcript, then take it over in a tab
    p.select(&t.id);
    p.lead_focus = false;
    k.key(&mut p, KeyCode::Enter);
    assert!(matches!(&p.mode, Mode::Log(v) if v.target == LogTarget::Task(t.id.clone())));
    let tr = k.render(&mut p, 150, 44);
    assert!(tr.contains("Spinner") && tr.contains("watchdog"), "{tr}");
    k.key(&mut p, KeyCode::Char('t'));
    until(&mut k, &mut p, 5000, "taken over", |p| p.task(&t.id).is_some_and(|t| !t.headless()));
    assert!(k.actions.iter().any(|a| matches!(a, Action::OpenTagged { tag, .. } if *tag == t.tag())), "opens a terminal tab");
    assert_eq!(p.task(&t.id).unwrap().status, Status::Running);
    let _ = std::fs::remove_dir_all(&dir);
}

/// oriel closes mid-run: the run comes back stopped, and `r` resumes the lead's session with the current state.
#[test]
fn agents_lead_resume_after_restart() {
    let dir = scratch("lead-resume");
    let repo = temp_repo(&dir);
    let prompts: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let p2 = prompts.clone();
    let lead: run::Fake = Arc::new(move |spec: &run::Spec, stop: &AtomicBool, on: &mut dyn FnMut(Ev)| -> run::Outcome {
        p2.lock().unwrap().push((spec.prompt.clone(), spec.resume.clone()));
        on(Ev::Session("lead-r".into()));
        if spec.resume.is_empty() {
            // the first life: works until oriel goes away
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(10));
            }
            return run::Outcome { session: "lead-r".into(), stopped: true, ..Default::default() };
        }
        run::Outcome { session: "lead-r".into(), text: "```json\n{\"actions\":[{\"tool\":\"done\",\"args\":{\"summary\":\"resumed fine\"}}]}\n```".into(), ..Default::default() }
    });
    let mut k = Kit::new();
    let mut p = lead_pane(&dir, &repo, fake_worker(Arc::default()), lead.clone());
    k.render(&mut p, 150, 44);
    until(&mut k, &mut p, 5000, "repo", |p| p.repo.is_some());
    start(&mut k, &mut p, "Survive a restart");
    until(&mut k, &mut p, 10_000, "lead running", |p| p.store.runs[0].session_id == "lead-r");
    drop(p); // oriel closes
    std::thread::sleep(Duration::from_millis(200));
    let mut p = lead_pane(&dir, &repo, fake_worker(Arc::default()), lead);
    assert_eq!(run0(&p).state, store::RunState::Stopped);
    assert!(run0(&p).error.contains("r resumes"));
    k.render(&mut p, 150, 44);
    until(&mut k, &mut p, 5000, "repo", |p| p.repo.is_some());
    let board = k.render(&mut p, 150, 44);
    assert!(board.contains("stopped"), "{board}");
    k.key(&mut p, KeyCode::Up);
    assert!(p.lead_focus);
    k.key(&mut p, KeyCode::Char('r'));
    until(&mut k, &mut p, 10_000, "resumed and finished", |p| p.store.runs[0].state == store::RunState::Review);
    let last = prompts.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last.1, "lead-r", "the same session");
    assert!(last.0.contains("oriel restarted") && last.0.contains("Survive a restart"), "{}", last.0);
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ snapshots

/// A run in full swing for the screenshots: a lead panel, workers in every state, live transcripts.
fn demo_run(dir: &Path) -> Agents {
    let mut p = super::tests::demo(dir);
    p.store.tasks.retain(|t| t.status != Status::Done || t.outcome == "merged");
    p.roster = roster();
    p.roster[1].name = "codex".into();
    p.roster[0].name = "claude-haiku".into();
    p.roster[2].name = "claude-worker".into();
    p.roster.push(RosterEntry { name: "kimi".into(), agent: "kimi".into(), model: String::new(), tier: "cheap".into(), good_at: "bulk edits, docs, frontend".into(), max_turns: 40, budget_usd: 1.0, enabled: false });
    p.limits = roster::Limits {
        claude: vec![roster::Window { label: "5-hour".into(), pct: 34.0, resets_at: None }, roster::Window { label: "weekly".into(), pct: 12.0, resets_at: None }],
        codex: vec![roster::Window { label: "5-hour".into(), pct: 61.0, resets_at: None }, roster::Window { label: "weekly".into(), pct: 22.0, resets_at: None }],
    };
    let repo = p.repo_key();
    let now = store::now();
    let run = store::Run {
        id: "r1".into(),
        repo: repo.clone(),
        goal: "Dark mode: a theme toggle in the palette, persisted, with tests".into(),
        agent: "claude".into(),
        model: "opus".into(),
        protocol: "mcp".into(),
        state: store::RunState::Running,
        branch: "oriel/lead-dark-mode-a-theme-toggle-k3f9".into(),
        base_branch: "master".into(),
        cost_usd: 0.42,
        budget_usd: 8.0,
        lead_budget_usd: 2.0,
        max_parallel: 3,
        created: now - 460,
        merged: 1,
        log: vec![
            "integration branch oriel/lead-dark-mode-a-theme-toggle-k3f9 off master".into(),
            "“ Three tasks: the toggle command, persisting it, and tests. The config change goes first.".into(),
            "planned 4 tasks: cfg → claude-haiku, toggle → codex, persist → claude-worker, tests → claude-haiku".into(),
            "merged: Theme field in the config".into(),
            "bounced: Toggle command in the palette (conflict in src/app.rs, back to codex)".into(),
            "✎ config landed; toggle and persistence running in parallel".into(),
        ],
        ..Default::default()
    };
    p.store.runs.push(run);
    let mut l = lead::RunLive::new();
    let e = |label: &str, target: &str, summary: &str, status: &str| Entry { kind: 't', id: format!("{label}{target}"), label: label.into(), target: target.into(), summary: summary.into(), status: status.into(), ..Default::default() };
    l.log = vec![
        Entry::note("lead: claude · opus · MCP tools"),
        e("roster", "", "4 workers", "done"),
        e("Read", "src/panes/app.rs", "812 lines", "done"),
        Entry::say("Three tasks: the toggle command, persisting it, and tests. The config change goes first so the others build on it."),
        e("plan", "", "4 tasks", "done"),
        e("wait", "", "finished: Theme field", "done"),
        e("merge", "cfg", "queued", "done"),
        e("wait", "", "", "running"),
    ];
    p.runs_live.insert("r1".into(), l);
    let mut add = |id: &str, key: &str, title: &str, worker: &str, agent: &str, tier: &str, status: Status, f: &dyn Fn(&mut Task)| {
        let mut t = Task { id: id.into(), key: key.into(), repo: repo.clone(), title: title.into(), worker: worker.into(), agent: agent.into(), tier: tier.into(), run: "r1".into(), mode: "headless".into(), status, created: now - 400, started: now - 300, ..Default::default() };
        if status == Status::Running {
            t.worktree = format!("{repo}-wt-{id}");
        }
        f(&mut t);
        p.store.tasks.push(t);
    };
    add("q1", "cfg", "Theme field in the config", "claude-haiku", "claude", "cheap", Status::Done, &|t| {
        t.outcome = "merged".into();
        t.last = "merged into oriel/lead-dark-mode… as 4be21c0 (1 file changed, +6)".into();
        (t.added, t.removed, t.files, t.cost_usd) = (6, 0, 1, 0.04);
        t.finished = now - 200;
    });
    add("q2", "toggle", "Toggle command in the palette", "codex", "codex", "mid", Status::Running, &|t| {
        t.last = "Edit src/app.rs · +14 -2".into();
        (t.tokens, t.cost_usd, t.attempts) = (61_000, 0.31, 1);
        t.owns = vec!["src/app.rs".into()];
    });
    add("q3", "persist", "Persist the choice", "claude-worker", "claude", "premium", Status::Running, &|t| {
        t.last = "Bash cargo test config".into();
        (t.tokens, t.cost_usd) = (38_400, 0.22);
        t.started = now - 120;
    });
    add("q4", "tests", "Tests for the toggle", "claude-haiku", "claude", "cheap", Status::Todo, &|t| {
        t.queued = true;
        t.depends_on = vec!["toggle".into(), "persist".into()];
        t.prompt = "Add tests for the palette toggle and the persisted theme".into();
    });
    let live = |p: &mut Agents, id: &str, entries: Vec<Entry>| {
        p.live.entry(id.into()).or_default().log = entries;
    };
    let mut diff = e("Edit", "src/app.rs", "+14 -2", "running");
    diff.body = vec!["-    fn palette(&self) -> Vec<Cmd> {".into(), "+    fn palette(&self) -> Vec<Cmd> {".into(), "+        let mut v = self.base_cmds();".into(), "+        v.push(Cmd::new(\"toggle light/dark\", Action::ToggleTheme));".into()];
    live(&mut p, "q2", vec![Entry::note("codex started"), e("Run", "rg ToggleTheme src", "3 lines", "done"), e("Read", "src/app.rs", "", "done"), e("Edit", "src/app.rs", "+9 -1", "done"), Entry::note("merge bounced: conflict in src/app.rs — resolving"), e("Read", "src/app.rs", "conflict markers at 212", "done"), diff]);
    let mut out = e("Bash", "cargo test config", "", "running");
    out.body = vec![">   Compiling oriel v0.4.5".into(), ">    Running unittests src/main.rs".into()];
    live(&mut p, "q3", vec![Entry::note("claude-worker started"), e("Read", "src/config.rs", "212 lines", "done"), e("Edit", "src/config.rs", "+9 -1", "done"), Entry::say("Saving the theme on toggle; running the config tests."), out]);
    live(&mut p, "q1", vec![Entry::note("claude-haiku started"), e("Edit", "src/config.rs", "+6 -0", "done"), Entry::say("Added `theme_mode` to the config."), Entry::note("finished")]);
    p.store.records.insert("claude-haiku".into(), store::Record { tasks: 3, merged: 3, first_try: 3, cost_usd: 0.12, ..Default::default() });
    p.store.records.insert("codex".into(), store::Record { tasks: 2, merged: 1, first_try: 1, conflicts: 1, cost_usd: 0.55, ..Default::default() });
    p
}

#[test]
fn agents_lead_snapshots() {
    let dir = scratch("lead-snap");
    let mut k = Kit::new();
    let mut p = demo_run(&dir);
    p.lead_focus = true;
    let board = k.render_html(&mut p, 150, 44, "target/snap/agents-lead-board.html");
    println!("{board}");
    for s in ["lead · claude opus · MCP tools", "Dark mode", "running", "$", "oriel/lead-dark-mode", "codex", "premium"] {
        assert!(board.contains(s), "board missing {s:?}");
    }
    p.mode = Mode::Watch(WatchView { run: "r1".into(), sel: 1 });
    let watch = k.render_html(&mut p, 150, 44, "target/snap/agents-lead-watch.html");
    println!("{watch}");
    assert!(watch.contains("watch") && watch.contains("Toggle command") && watch.contains("cargo test config") && watch.contains("toggle light/") && watch.contains("starts once toggle + persist merged"));
    p.mode = Mode::Log(LogView { target: LogTarget::Task("q2".into()), scroll: 0, back: Some("r1".into()) });
    let log = k.render_html(&mut p, 150, 44, "target/snap/agents-lead-log.html");
    assert!(log.contains("take over") && log.contains("resolving"), "{log}");
    p.mode = Mode::Board;
    p.lead_focus = false;
    k.key(&mut p, KeyCode::Char('R'));
    let ros = k.render_html(&mut p, 150, 44, "target/snap/agents-lead-roster.html");
    println!("{ros}");
    assert!(ros.contains("roster") && ros.contains("claude-worker") && ros.contains("5h 61%") && ros.contains("3 merged of 3"));
    k.key(&mut p, KeyCode::Enter);
    let edit = k.render_html(&mut p, 150, 44, "target/snap/agents-lead-roster-edit.html");
    assert!(edit.contains("edit worker") && edit.contains("good at"));
    k.key(&mut p, KeyCode::Esc);
    k.key(&mut p, KeyCode::Esc);
    p.store.runs.clear();
    k.key(&mut p, KeyCode::Char('L'));
    k.typ(&mut p, "Split the settings page into tabs and add a search box");
    let form = k.render_html(&mut p, 150, 44, "target/snap/agents-lead-form.html");
    println!("{form}");
    assert!(form.contains("new lead run") && form.contains("MCP") && form.contains("workers at once"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Opt-in, costs a few cents: a real Claude lead (haiku) orchestrating a real Claude worker (haiku) through
/// oriel's MCP server in a temp repo. Needs `cargo build` first (the lead's CLI starts target/debug/oriel).
/// cargo test agents_lead_live -- --ignored --nocapture
#[test]
#[ignore]
fn agents_lead_live_claude_haiku() {
    let exe = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("debug").join(if cfg!(windows) { "oriel.exe" } else { "oriel" });
    assert!(exe.is_file(), "run cargo build first");
    let dir = scratch("lead-live");
    let repo = temp_repo(&dir);
    let mut k = Kit::new();
    let mut p = Agents::with_paths(Paths { agents: dir.join("agents"), wt: dir.join("wt") });
    p.start_dir = Some(repo.clone());
    p.mcp_exe = Some(exe);
    p.stagger = Duration::ZERO;
    p.lead_cfg.agent = "claude".into();
    p.lead_cfg.model = "haiku".into();
    p.lead_cfg.budget_usd = 0.60;
    p.lead_cfg.gate = "none".into();
    p.roster = vec![RosterEntry { name: "haiku".into(), agent: "claude".into(), model: "haiku".into(), tier: "cheap".into(), good_at: "small edits".into(), max_turns: 8, budget_usd: 0.30, enabled: true }];
    k.render(&mut p, 150, 44);
    until(&mut k, &mut p, 5000, "repo", |p| p.repo.is_some() && p.installed("claude").is_some());
    let id = {
        let mut cx = crate::pane::Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: &mut vec![], focused: true, time: 1.0 };
        p.start_run("Create a file hello.txt containing exactly the word hi. One task for the haiku worker (owns hello.txt, size S), merge it, then done.", "claude", "haiku", 1, 1.20, &mut cx).unwrap()
    };
    until(&mut k, &mut p, 600_000, "the live lead finishes", |p| p.run_ref(&id).is_some_and(|r| !r.state.active()));
    let r = p.run_ref(&id).unwrap().clone();
    println!("run: {:?} · protocol {} · lead ${:.4} · total ${:.4}", r.state, r.protocol, r.cost_usd, p.spend(&id));
    for l in &r.log {
        println!("  {l}");
    }
    for t in &p.store.tasks {
        println!("task {} {:?} {} ${:.4} {}", t.title, t.status, t.outcome, t.cost_usd, t.error);
    }
    let hello = sh(&repo, &["show", &format!("{}:hello.txt", r.branch)]);
    assert_eq!(hello.trim(), "hi");
    assert_eq!(r.protocol, "mcp");
    let _ = std::fs::remove_dir_all(&dir);
}
