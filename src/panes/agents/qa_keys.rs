//! QA key-mash for the agents app (see src/qa_keys.rs). Everything lives under scratch: its task store, its
//! worktrees and a throwaway git repo of its own (so it never finds the repo these tests run in). No agent is
//! ever found on this machine (the start-up search for claude / codex / kimi is skipped), so the planner can't
//! run Claude for real; tasks start a no-op command, and headless workers and leads are instant fakes.
//! The one thing held back: enter on a typed path in the repo picker (`~` or `.` would point it at a real repo).

use super::stream::{Entry, Ev as SEv};
use super::*;
use crate::qa_keys::{Ev, Opts, mash_pane, scratch};
use crate::testkit::Kit;
use serde_json::json;

fn git(dir: &Path, args: &[&str]) {
    let o = git::run(dir, args);
    assert!(o.ok, "git {args:?}: {}", o.stderr);
}

/// A fresh repo with one commit, inside `dir`.
fn repo(dir: &Path) -> PathBuf {
    let r = dir.join("repo");
    std::fs::create_dir_all(&r).unwrap();
    git(&r, &["init", "-q", "-b", "master"]);
    for (k, v) in [("user.name", "qa"), ("user.email", "qa@example.invalid"), ("commit.gpgsign", "false"), ("core.autocrlf", "false")] {
        git(&r, &["config", k, v]);
    }
    std::fs::write(r.join("a.txt"), "one\ntwo 中文\n").unwrap();
    git(&r, &["add", "-A"]);
    git(&r, &["commit", "-q", "-m", "init"]);
    r
}

fn fake_worker() -> run::Fake {
    Arc::new(|spec: &run::Spec, _stop: &AtomicBool, on: &mut dyn FnMut(SEv)| -> run::Outcome {
        on(SEv::Session("w".into()));
        on(SEv::Entry(Entry { kind: 't', id: "c1".into(), label: "Read".into(), target: "a.txt".into(), status: "done".into(), ..Default::default() }));
        on(SEv::Entry(Entry::say("done — nothing to change 🙂")));
        on(SEv::Cost(0.01));
        run::Outcome { session: "w".into(), cost: 0.01, text: format!("finished {}", spec.prompt.chars().take(20).collect::<String>()), ..Default::default() }
    })
}

fn fake_lead() -> run::Fake {
    Arc::new(|_spec: &run::Spec, _stop: &AtomicBool, on: &mut dyn FnMut(SEv)| -> run::Outcome {
        on(SEv::Entry(Entry::say("Looking.")));
        on(SEv::Cost(0.02));
        let actions = json!({"actions": [{"tool": "note", "args": {"text": "qa lead"}}, {"tool": "done", "args": {"summary": "nothing to do"}}]});
        run::Outcome { session: "lead".into(), cost: 0.02, text: format!("ok\n```json\n{actions}\n```"), ..Default::default() }
    })
}

/// An Agents pane that can't reach anything real (see the module comment).
fn safe(k: &Kit, dir: &Path, with_repo: bool) -> Agents {
    let mut a = Agents::with_paths(Paths { agents: dir.join("agents"), wt: dir.join("wt") });
    let r = repo(dir);
    a.start_dir = Some(r.clone());
    a.fake_agent = Some(if cfg!(windows) { ("cmd.exe".into(), vec!["/c".into(), "exit".into()]) } else { ("true".into(), vec![]) });
    a.fake_worker = Some(fake_worker());
    a.fake_lead = Some(fake_lead());
    a.stagger = Duration::ZERO;
    a.lead_cfg.agent = "claude".into();
    a.lead_cfg.protocol = "text".into();
    a.lead_cfg.gate = "none".into();
    a.roster = vec![
        crate::config::RosterEntry { name: "w1".into(), agent: "claude".into(), tier: "cheap".into(), ..Default::default() },
        crate::config::RosterEntry { name: "w2 中文".into(), agent: "codex".into(), tier: "premium".into(), enabled: false, ..Default::default() },
    ];
    a.booted = true; // skip the start-up search for installed agents (and the repo of the working folder)
    *a.waker.lock().unwrap() = Some(crate::pane::Waker { id: 1, tx: k.tx.clone() });
    a.repo_loading = false;
    if with_repo {
        a.set_repo(git::repo_info(&r).expect("scratch repo"));
    } else {
        a.mode = Mode::Repo(Picker { input: Input::new("", false), sel: 0, err: String::new(), checking: false });
    }
    // a few cards in every column to move between
    for (i, title) in ["fix the thing", "中文 task 🙂", "", "a very long title that goes on and on and on past the edge of any card"].iter().enumerate() {
        let id = a.add_task(title, &format!("do it {i}\nline two"), i % 3, "");
        if i == 1 {
            a.task_mut(&id).unwrap().status = Status::Review;
        }
    }
    a
}

fn guard(p: &mut Agents, ev: &mut Ev) -> bool {
    if let (Ev::Key(k), Mode::Repo(pk)) = (&*ev, &p.mode) {
        if k.code == KeyCode::Enter && pk.sel == 0 {
            return false; // a typed path could name a real repo (., .., ~)
        }
    }
    assert!(p.agents.iter().all(|a| a.1.is_none()), "an installed agent was found: the planner could run it");
    true
}

#[test]
fn qa_keys_agents() {
    let dir = scratch("agents");
    mash_pane("agents", Opts { snap: Some("agents".into()), ..Default::default() }, move |k| safe(k, &dir, true), guard);
}

#[test]
fn qa_keys_agents_no_repo() {
    let dir = scratch("agents-norepo");
    mash_pane("agents-norepo", Opts { keys: crate::qa_keys::keys_for(1500), ..Default::default() }, move |k| safe(k, &dir, false), guard);
}
