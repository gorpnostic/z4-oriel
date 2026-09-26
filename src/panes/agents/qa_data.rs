//! QA: the orchestrator's saved data (store.rs): tasks.json and the status files hooks write, when they are
//! corrupt, from another version, full of odd text or extreme numbers. Loading must never panic, and one bad
//! field must not cost the user every task. Everything lives under target/test-scratch/qa-data; git only runs in
//! throwaway repos there; no agent is ever started (no start/merge keys are pressed).
//! Tests that document a real bug are `#[ignore = "fails: ..."]`; run them with `cargo test qa_agents -- --ignored`.

use super::tests::{noop_agent, temp_repo, until};
use super::*;
use crate::qa_data::{ReadOnly, controls, corrupt_json, nasty, scratch};
use crate::testkit::Kit;
use serde_json::{Value, json};

fn pane_at(dir: &Path, repo: &Path) -> Agents {
    let mut a = Agents::with_paths(Paths { agents: dir.join("agents"), wt: dir.join("wt") });
    a.start_dir = Some(repo.to_path_buf());
    a.fake_agent = Some(noop_agent());
    a
}

/// A scratch repo and the board's key for it (what Task::repo must hold to show on the board).
fn repo_and_key(name: &str) -> (PathBuf, PathBuf, String) {
    let d = scratch(name);
    let repo = temp_repo(&d);
    let mut k = Kit::new();
    let probe_dir = d.join("probe");
    let mut p = pane_at(&probe_dir, &repo);
    k.render(&mut p, 150, 44);
    until(&mut k, &mut p, 10_000, "repo detected", |p| p.repo.is_some());
    let key = p.repo_key();
    (d, repo, key)
}

fn task(id: &str, key: &str, title: &str, status: &str) -> Value {
    json!({"id": id, "repo": key, "title": title, "prompt": format!("do {title}"), "agent": "claude", "status": status, "created": 1_790_000_000i64})
}

fn write_tasks(dir: &Path, v: &Value) -> PathBuf {
    let p = dir.join("agents").join("tasks.json");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, serde_json::to_string_pretty(v).unwrap()).unwrap();
    p
}

/// Look at the board every way that doesn't start or merge anything.
fn look_around(k: &mut Kit, p: &mut Agents) -> String {
    let mut all = String::new();
    for (w, h) in [(150, 44), (100, 30), (60, 16), (30, 8)] {
        all.push_str(&k.render(p, w, h));
    }
    all.push_str(&k.render_side(p, 34, 24));
    for _ in 0..4 {
        for code in [KeyCode::Char('j'), KeyCode::Char('j'), KeyCode::Char('G'), KeyCode::Char('k'), KeyCode::Char('g')] {
            k.key(p, code);
        }
        all.push_str(&k.render(p, 150, 44));
        k.key(p, KeyCode::Tab);
    }
    k.key(p, KeyCode::Char('R'));
    all.push_str(&k.render(p, 150, 44));
    k.key(p, KeyCode::Esc);
    k.key(p, KeyCode::Char('w'));
    all.push_str(&k.render(p, 150, 44));
    k.key(p, KeyCode::Esc);
    all.push_str(&k.render(p, 150, 44));
    all
}

/// Wait for the saver thread to write tasks.json (or give up quietly after `ms`).
fn wait_for_write(path: &Path, before: &[u8], ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        if std::fs::read(path).map(|b| b != before).unwrap_or(false) {
            std::thread::sleep(Duration::from_millis(50));
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

// ------------------------------------------------------------------ tasks.json

#[test]
fn qa_agents_corrupt_tasks_json_never_panics() {
    let (d, repo, _) = repo_and_key("agents-corrupt");
    for (i, bytes) in corrupt_json().into_iter().enumerate() {
        let dir = d.join(format!("case{i}"));
        let path = dir.join("agents").join("tasks.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let mut k = Kit::new();
        let mut p = pane_at(&dir, &repo);
        assert!(p.store.tasks.is_empty(), "case {i}");
        k.render(&mut p, 150, 44);
        until(&mut k, &mut p, 10_000, "repo detected", |p| p.repo.is_some());
        let s = look_around(&mut k, &mut p);
        assert!(!s.is_empty());
    }
}

#[test]
#[ignore = "fails: one task tasks.json can't parse (a newer status, priority 300, a string cost) empties the whole board, and the next save overwrites every task, run and record"]
fn qa_agents_one_bad_task_wipes_every_task() {
    let (d, repo, key) = repo_and_key("agents-onebad");
    let bad_tasks = [
        ("a status from a newer oriel", json!({"status": "paused"})),
        ("priority out of i8 range", json!({"priority": 300})),
        ("cost as a string", json!({"cost_usd": "free"})),
        ("negative tokens", json!({"tokens": -1})),
        ("file_stats as objects", json!({"file_stats": [{"path": "a", "added": 1}]})),
    ];
    let mut lost = vec![];
    for (i, (what, patch)) in bad_tasks.iter().enumerate() {
        let dir = d.join(format!("case{i}"));
        let mut odd = task("odd1", &key, "the odd one", "todo");
        for (k, v) in patch.as_object().unwrap() {
            odd[k] = v.clone();
        }
        let store = json!({
            "repos": [key],
            "tasks": [task("keep1", &key, "keep one", "todo"), task("keep2", &key, "keep two", "review"), task("keep3", &key, "keep three", "done"), odd],
            "runs": [{"id": "run1", "repo": key, "goal": "keep this run", "state": "review"}],
            "records": {"worker-a": {"tasks": 9, "merged": 7}}
        });
        let path = write_tasks(&dir, &store);
        let before = std::fs::read(&path).unwrap();
        let mut k = Kit::new();
        let mut p = pane_at(&dir, &repo);
        let loaded = p.store.tasks.len();
        k.render(&mut p, 150, 44);
        until(&mut k, &mut p, 10_000, "repo detected", |p| p.repo.is_some());
        wait_for_write(&path, &before, 1500);
        let disk = std::fs::read_to_string(&path).unwrap();
        let kept = ["keep one", "keep two", "keep three", "keep this run", "worker-a"].iter().all(|s| disk.contains(s));
        if loaded < 3 || !kept {
            lost.push(format!("{what}: {loaded} of 4 tasks loaded; the other tasks/run/records still on disk after opening the board: {kept}"));
        }
    }
    assert!(lost.is_empty(), "one bad task:\n{}", lost.join("\n"));
}

#[test]
fn qa_agents_nasty_text_renders_clean() {
    let (d, repo, key) = repo_and_key("agents-nasty");
    let mut strings = nasty();
    strings.extend(["tab\there".to_string(), "line\nbreak".into(), "cr\rhere".into(), "esc \u{1b}[31mred\u{1b}[0m".into(), "bell\u{7}".into(), "osc \u{1b}]0;pwned\u{7}".into()]);
    let statuses = ["todo", "running", "blocked", "review", "done"];
    let tasks: Vec<Value> = strings
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let mut t = task(&format!("t{i}"), &key, s, statuses[i % 5]);
            for f in ["prompt", "last", "question", "error", "summary", "worker", "tier", "branch", "base_branch", "model", "outcome", "acceptance", "kind", "size", "gate"] {
                t[f] = json!(s);
            }
            t["touched"] = json!([s, s]);
            t["recent"] = json!([s]);
            t["questions"] = json!([s]);
            t["history"] = json!([s]);
            t["conflicts"] = json!([s]);
            t["owns"] = json!([s]);
            t["file_stats"] = json!([[s, 3, 1]]);
            t["added"] = json!(3);
            t["removed"] = json!(1);
            t["files"] = json!(1);
            t["mode"] = json!(if i % 2 == 0 { "headless" } else { "" });
            t
        })
        .collect();
    let runs: Vec<Value> = strings.iter().take(3).enumerate().map(|(i, s)| json!({"id": format!("r{i}"), "repo": key, "goal": s, "state": "review", "log": [s], "notes": [[1_790_000_000i64, s]], "summary": s, "error": s, "branch": s})).collect();
    let mut records = serde_json::Map::new();
    records.insert(strings[0].clone(), json!({"tasks": 1}));
    let path = write_tasks(&d, &json!({"repos": [key, strings[0], strings[1]], "tasks": tasks, "runs": runs, "records": records}));
    let mut k = Kit::new();
    let mut p = pane_at(&d, &repo);
    assert_eq!(p.store.tasks.len(), strings.len(), "every task loads from {}", path.display());
    k.render(&mut p, 150, 44);
    until(&mut k, &mut p, 10_000, "repo detected", |p| p.repo.is_some());
    let s = look_around(&mut k, &mut p);
    let bad = controls(&s);
    assert!(bad.is_empty(), "control characters from tasks.json reached the screen: {bad:?}");
}

/// One field at an extreme value per case: which ones panic when the board is shown and used?
#[test]
#[ignore = "fails: extreme numbers in tasks.json (run/task timestamps at i64::MIN/MAX, added = u64::MAX, priority 127 then +) panic the board with arithmetic overflow (debug builds)"]
fn qa_agents_extreme_numbers() {
    let (d, repo, key) = repo_and_key("agents-numbers");
    let none = json!({});
    // (what, task fields, run fields, records)
    let mut cases: Vec<(&str, Value, Value, Value)> = vec![
        ("run created = i64::MIN", none.clone(), json!({"created": i64::MIN, "finished": 0}), none.clone()),
        ("run finished = i64::MAX, created = i64::MIN", none.clone(), json!({"created": i64::MIN, "finished": i64::MAX}), none.clone()),
        ("run note at i64::MIN / i64::MAX", none.clone(), json!({"notes": [[i64::MIN, "n"], [i64::MAX, "m"]]}), none.clone()),
        ("run cost/turns/merged huge", none.clone(), json!({"cost_usd": 1e300, "budget_usd": 1e300, "lead_budget_usd": 1e300, "turns": u32::MAX, "merged": u32::MAX, "max_parallel": u32::MAX}), none.clone()),
        ("worker record extremes", none.clone(), none.clone(), json!({"w": {"tasks": u32::MAX, "merged": u32::MAX, "tokens": u64::MAX, "cost_usd": 1e300, "secs": i64::MIN}})),
    ];
    let task_cases: Vec<(&str, Value)> = vec![
        ("added = u64::MAX, removed = 1", json!({"added": u64::MAX, "removed": 1, "files": 1})),
        ("tokens = u64::MAX on two tasks", json!({"tokens": u64::MAX})),
        ("cost_usd = 1e300", json!({"cost_usd": 1e300, "budget_usd": 1e300})),
        ("priority = 127, then +", json!({"priority": 127})),
        ("priority = -128, then -", json!({"priority": -128})),
        ("created/started = i64::MIN", json!({"created": i64::MIN, "started": i64::MIN, "finished": i64::MIN})),
        ("created/started = i64::MAX", json!({"created": i64::MAX, "started": i64::MAX, "finished": i64::MAX})),
        ("followups/attempts = u32::MAX", json!({"followups": u32::MAX, "attempts": u32::MAX, "redispatches": u32::MAX, "max_turns": u32::MAX})),
        ("file_stats = u64::MAX", json!({"file_stats": [["a.rs", u64::MAX, u64::MAX], ["b.rs", u64::MAX, u64::MAX]]})),
        ("10k touched files", json!({"touched": (0..10_000).map(|i| format!("src/f{i}.rs")).collect::<Vec<_>>()})),
    ];
    cases.extend(task_cases.into_iter().map(|(w, t)| (w, t, none.clone(), none.clone())));
    let mut panics = vec![];
    for (i, (what, patch, run_patch, records)) in cases.iter().enumerate() {
        let dir = d.join(format!("case{i}"));
        let mut tasks = vec![];
        for (n, status) in ["todo", "running", "review", "done"].iter().enumerate() {
            let mut t = task(&format!("x{n}"), &key, &format!("task {n}"), status);
            for (k, v) in patch.as_object().unwrap() {
                t[k] = v.clone();
            }
            tasks.push(t);
        }
        let mut run = json!({"id": "r1", "repo": key, "goal": "g", "state": "review", "created": 1_790_000_000i64, "finished": 1_790_000_100i64});
        for (k, v) in run_patch.as_object().unwrap() {
            run[k] = v.clone();
        }
        write_tasks(&dir, &json!({"repos": [key], "tasks": tasks, "runs": [run], "records": records}));
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut k = Kit::new();
            let mut p = pane_at(&dir, &repo);
            assert_eq!(p.store.tasks.len(), 4, "loads");
            k.render(&mut p, 150, 44);
            until(&mut k, &mut p, 10_000, "repo detected", |p| p.repo.is_some());
            look_around(&mut k, &mut p);
            for c in 0..4 {
                p.col = c;
                k.key(&mut p, KeyCode::Char('+'));
                k.key(&mut p, KeyCode::Char('-'));
                k.key(&mut p, KeyCode::Char('-'));
                k.render(&mut p, 150, 44);
            }
        }));
        if let Err(e) = r {
            let msg = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
            panics.push(format!("{what}: {msg}"));
        }
    }
    assert!(panics.is_empty(), "tasks.json values that panic the board:\n{}", panics.join("\n"));
}

#[test]
fn qa_agents_many_tasks_stay_fast() {
    let (d, repo, key) = repo_and_key("agents-many");
    let statuses = ["todo", "running", "review", "done"];
    let tasks: Vec<Value> = (0..3000).map(|i| task(&format!("m{i}"), &key, &format!("task number {i} 🎉"), statuses[i % 4])).collect();
    write_tasks(&d, &json!({"repos": [key], "tasks": tasks}));
    let t0 = Instant::now();
    let mut k = Kit::new();
    let mut p = pane_at(&d, &repo);
    let load_ms = t0.elapsed().as_millis();
    k.render(&mut p, 150, 44);
    until(&mut k, &mut p, 10_000, "repo detected", |p| p.repo.is_some());
    let t1 = Instant::now();
    for _ in 0..10 {
        k.render(&mut p, 150, 44);
    }
    let render_ms = t1.elapsed().as_millis() / 10;
    let t2 = Instant::now();
    for _ in 0..20 {
        k.key(&mut p, KeyCode::Char('j'));
    }
    let key_ms = t2.elapsed().as_millis() / 20;
    println!("qa_agents_many: 3000 tasks: load {load_ms} ms, render {render_ms} ms/frame, j {key_ms} ms/key (debug build)");
    assert!(render_ms < 500, "a board frame with 3000 tasks took {render_ms} ms");
}

// ------------------------------------------------------------------ status files (hooks write these)

#[test]
fn qa_agents_status_files_corrupt_or_hostile() {
    let d = scratch("agents-status");
    let paths = Paths { agents: d.join("agents"), wt: d.join("wt") };
    std::fs::create_dir_all(paths.status_dir()).unwrap();
    for (i, bytes) in corrupt_json().into_iter().enumerate() {
        let p = paths.status_dir().join(format!("c{i}.json"));
        std::fs::write(&p, &bytes).unwrap();
        // never panics ("{}" and "[]" even parse, as an all-default status: serde reads a struct from a list)
        let _ = (i, store::read_status(&p));
    }
    for s in [r#"{"state":5,"ts":"now"}"#, r#"{"state":"running","ts":1e40}"#, r#"{"state":"running","ts":-1}"#, r#"{"state":["x"]}"#] {
        let p = paths.status_dir().join("w.json");
        std::fs::write(&p, s).unwrap();
        let _ = store::read_status(&p);
    }
    // `oriel report` from a hook: junk ids never write outside the status folder
    for id in ["../escape", "..\\escape", "a/b", "", "🎉", "con", "x.y", &"a".repeat(300)] {
        let args: Vec<String> = ["--task", id, "--state", "running"].iter().map(|s| s.to_string()).collect();
        assert_eq!(store::report(&paths, &args, Some(r#"{"tool_name":"Edit","tool_input":{"file_path":"a.rs"}}"#)), 0);
    }
    assert!(!paths.agents.join("escape.json").exists() && !d.join("escape.json").exists());
    // a long-but-legal id is a file name longer than Windows allows: report must stay quiet, not panic
    let args: Vec<String> = ["--task", &"a".repeat(300), "--state", "blocked"].iter().map(|s| s.to_string()).collect();
    assert_eq!(store::report(&paths, &args, Some("not json at all")), 0);
    // hook payloads with the wrong types everywhere
    for hook in [
        json!({"session_id": 5, "transcript_path": ["x"], "message": {"a": 1}, "tool_name": 7, "tool_input": "str"}),
        json!({"tool_name": "Bash", "tool_input": {"command": 42}}),
        json!({"tool_name": "Edit", "tool_input": {"file_path": ""}}),
        json!({"tool_name": "Edit", "tool_input": {"file_path": "C:\\"}}),
        json!({"tool_name": "🎉".repeat(500), "tool_input": {"command": "x\n".repeat(10_000)}}),
        json!(null),
        json!([1, 2]),
    ] {
        for state in ["running", "blocked", "idle", "session", "weird"] {
            let st = store::merge_report(StatusFile::default(), state, &hook, i64::MAX);
            assert!(st.last_tool.chars().count() <= 120);
        }
        let _ = store::tool_summary(&hook);
    }
}

#[test]
fn qa_agents_status_file_on_the_board() {
    // a task waiting in its tab, and status files with odd contents for it
    let (d, repo, key) = repo_and_key("agents-status-board");
    let mut t = task("s1", &key, "watch me", "running");
    t["worktree"] = json!(repo.display().to_string());
    t["started"] = json!(1_790_000_000i64);
    write_tasks(&d, &json!({"repos": [key], "tasks": [t]}));
    let mut k = Kit::new();
    let mut p = pane_at(&d, &repo);
    k.render(&mut p, 150, 44);
    until(&mut k, &mut p, 10_000, "repo detected", |p| p.repo.is_some());
    let status = p.paths.status("s1");
    std::fs::create_dir_all(status.parent().unwrap()).unwrap();
    let odd = nasty().into_iter().next().unwrap();
    for body in [json!({"state": "blocked", "ts": store::now(), "message": format!("{odd} \u{1b}[31m?")}), json!({"state": "running", "ts": store::now(), "last_tool": "x".repeat(5000)}), json!({"state": "mystery", "ts": 0})] {
        std::fs::write(&status, body.to_string()).unwrap();
        // the watcher looks every 500 ms
        let end = Instant::now() + Duration::from_millis(1300);
        while Instant::now() < end {
            k.poll(&mut p);
            std::thread::sleep(Duration::from_millis(50));
        }
        let s = k.render(&mut p, 150, 44);
        assert!(controls(&s).is_empty(), "control characters from a status file reached the board");
    }
}

#[test]
#[ignore = "fails: a status file with ts near i64::MAX panics the UI thread in on_status ('st.ts + 2', attempt to add with overflow, debug builds)"]
fn qa_agents_status_file_huge_ts() {
    let (d, repo, key) = repo_and_key("agents-status-ts");
    let t = task("s1", &key, "watch me", "running");
    write_tasks(&d, &json!({"repos": [key], "tasks": [t]}));
    let mut k = Kit::new();
    let mut p = pane_at(&d, &repo);
    k.render(&mut p, 150, 44);
    until(&mut k, &mut p, 10_000, "repo detected", |p| p.repo.is_some());
    let status = p.paths.status("s1");
    std::fs::create_dir_all(status.parent().unwrap()).unwrap();
    std::fs::write(&status, json!({"state": "blocked", "ts": i64::MAX}).to_string()).unwrap();
    let end = Instant::now() + Duration::from_millis(1300);
    while Instant::now() < end {
        k.poll(&mut p);
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ------------------------------------------------------------------ writing

#[test]
fn qa_agents_write_atomic_failures() {
    let d = scratch("agents-write");
    // read-only target: an error, and no temp file left behind
    let p = d.join("tasks.json");
    std::fs::write(&p, "{}").unwrap();
    let ro = ReadOnly::set(&p);
    assert!(store::write_atomic(&p, b"{\"tasks\":[]}").is_err(), "replacing a read-only tasks.json reports an error");
    drop(ro);
    let left: Vec<String> = std::fs::read_dir(&d).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n != "tasks.json").collect();
    assert!(left.is_empty(), "temp files left behind: {left:?}");
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "{}");
    // a file where the folder should be
    let blocker = d.join("agents");
    std::fs::write(&blocker, "a file").unwrap();
    assert!(store::write_atomic(&blocker.join("tasks.json"), b"{}").is_err());
    // missing folders are made
    let deep = d.join("x").join("y").join("tasks.json");
    store::write_atomic(&deep, b"{}").unwrap();
    assert_eq!(std::fs::read_to_string(&deep).unwrap(), "{}");
}

#[test]
fn qa_agents_slug_and_ids_from_odd_titles() {
    for title in nasty().iter().map(String::as_str).chain(["../../etc", "a/b\\c", "CON", "---", "Ünïcödé Fix", "\u{1b}[31m"]) {
        let s = store::slug(title, "k3f9abc");
        assert!(!s.is_empty() && s.len() <= 40, "slug({title:?}) = {s:?}");
        assert!(s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'), "slug({title:?}) = {s:?} is not branch/folder safe");
        assert!(!s.starts_with('-') && !s.ends_with('-'), "{s:?}");
    }
    assert_eq!(store::slug("", ""), "task-");
    let mut taken = vec![];
    for _ in 0..200 {
        let id = store::new_id(&taken);
        assert!(!taken.iter().any(|t: &Task| t.id == id));
        taken.push(Task { id, ..Default::default() });
    }
}
