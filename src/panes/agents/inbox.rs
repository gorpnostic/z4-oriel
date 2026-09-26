//! The agents app's part of the alerts app's "open now" list: tasks stuck waiting on you, finished tasks of your
//! own waiting for review (their result card is the peek), and a run whose branch is ready for you. A lead's (or
//! a batch's) workers aren't listed one by one: the run merges them itself, and says when it's done.

use super::store::{RunState, Status, Task};
use super::{Agents, Mode};
use crate::alerts::{Kind, Open, Reply};
use crate::pane::Cx;

/// The most summary lines and files a result card shows.
const LINES: usize = 6;

fn key(t: &Task) -> String {
    format!("task:{}", t.id)
}

/// The result card: who did it and what it cost, the merge gate, conflicts, the worker's own summary and
/// questions, and the files it changed most.
fn card(t: &Task) -> Vec<String> {
    let mut out = vec![];
    let model = if t.model.is_empty() { String::new() } else { format!(" {}", t.model) };
    let mut head = format!("{}{model} · +{} −{}", t.agent, t.added, t.removed);
    if t.files > 0 {
        head.push_str(&format!(" · {} file{}", t.files, if t.files == 1 { "" } else { "s" }));
    }
    if t.cost_usd > 0.0 {
        head.push_str(&format!(" · ${:.2}", t.cost_usd));
    }
    out.push(head);
    match t.gate.as_str() {
        "" => {}
        "pass" => out.push("merge gate: pass".into()),
        g => out.push(format!("merge gate failed: {}", g.lines().next().unwrap_or(""))),
    }
    if !t.conflicts.is_empty() {
        out.push(format!("conflicts with {}: {}", if t.base_branch.is_empty() { "its base" } else { &t.base_branch }, t.conflicts.join(", ")));
    }
    if !t.error.is_empty() {
        out.push(format!("error: {}", t.error.lines().next().unwrap_or("")));
    }
    let summary: Vec<&str> = t.summary.lines().filter(|l| !l.trim().is_empty()).collect();
    if !summary.is_empty() {
        out.push(String::new());
        out.extend(summary.iter().take(LINES).map(|l| l.to_string()));
        if summary.len() > LINES {
            out.push(format!("… {} more lines", summary.len() - LINES));
        }
    }
    for q in &t.questions {
        out.push(format!("asks: {q}"));
    }
    if !t.file_stats.is_empty() {
        out.push(String::new());
        let mut files = t.file_stats.clone();
        files.sort_by_key(|f| std::cmp::Reverse(f.1 + f.2));
        out.extend(files.iter().take(LINES).map(|(p, a, r)| format!("{p}  +{a} −{r}")));
    }
    out
}

impl Agents {
    /// What's waiting on you in this repo. Asked before every draw: one pass over the tasks, nothing slow.
    pub(super) fn open_items(&self) -> Vec<Open> {
        let repo = self.repo_key();
        let mut out = vec![];
        for t in self.store.tasks.iter().filter(|t| t.repo == repo) {
            match t.status {
                Status::Blocked => {
                    let q = if t.question.is_empty() { "waiting for you in its tab" } else { t.question.as_str() };
                    let mut o = Open::new(Kind::NeedsYou, key(t), format!("{} needs you: {q}", t.title));
                    o.detail.push(q.to_string());
                    if !t.recent.is_empty() {
                        o.detail.push(String::new());
                        o.detail.extend(t.recent.iter().rev().take(LINES).rev().cloned());
                    }
                    // its terminal tab shows the same thing: listed once, here
                    o.tag = (!t.headless()).then(|| t.tag());
                    out.push(o);
                }
                // a run's workers are the run's to merge
                Status::Review if t.run.is_empty() => {
                    let (kind, text) = if !t.error.is_empty() {
                        (Kind::NeedsYou, format!("{} failed: {}", t.title, t.error.lines().next().unwrap_or("")))
                    } else if t.blocked {
                        (Kind::NeedsYou, format!("{} is stuck: {}", t.title, t.questions.first().map(String::as_str).unwrap_or("see its summary")))
                    } else {
                        (Kind::AgentDone, format!("{} is ready for review", t.title))
                    };
                    let mut o = Open::new(kind, key(t), text);
                    o.detail = card(t);
                    out.push(o);
                }
                _ => {}
            }
        }
        if let Some(r) = self.current_run().filter(|r| r.state == RunState::Review) {
            let what = if r.manual { "your tasks are merged" } else { "lead run" };
            let mut o = Open::new(Kind::AgentDone, format!("run:{}", r.id), format!("{what}, ready for review: {}", r.goal));
            o.detail.push(format!("{} merged into {} · ${:.2}", r.merged, r.branch, self.spend(&r.id)));
            let summary: Vec<&str> = r.summary.lines().filter(|l| !l.trim().is_empty()).collect();
            if !summary.is_empty() {
                o.detail.push(String::new());
                o.detail.extend(summary.iter().take(LINES).map(|l| l.to_string()));
            }
            out.push(o);
        }
        out
    }

    /// enter in the alerts app (it has switched here): show that task or run as the board would.
    pub(super) fn answer_open(&mut self, key: &str, r: Reply, cx: &mut Cx) -> bool {
        if r != Reply::Go {
            return false;
        }
        // a form you're filling in isn't thrown away for it
        // (nor a line comment half typed in a diff)
        let typing = matches!(&self.mode, Mode::Diff(d) if d.typing.is_some());
        if typing || !matches!(self.mode, Mode::Board | Mode::Diff(_) | Mode::Log(_) | Mode::Watch(_)) {
            cx.notify("finish or esc what's open in agents first");
            return false;
        }
        if let Some(id) = key.strip_prefix("task:") {
            if self.task(id).is_none() {
                return false;
            }
            self.mode = Mode::Board;
            self.lead_focus = false;
            self.select(id);
            self.activate(cx); // its tab, its diff, or its transcript
            return true;
        }
        if let Some(id) = key.strip_prefix("run:") {
            if self.current_run().is_some_and(|r| r.id == id) {
                self.mode = Mode::Board;
                self.lead_focus = true;
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::super::git::RepoInfo;
    use super::super::store::{Paths, Run};
    use super::super::tests::scratch;
    use super::*;
    use crate::pane::Action;
    use crate::testkit::Kit;

    fn task(id: &str, repo: &str, status: Status) -> Task {
        Task { id: id.into(), repo: repo.into(), title: format!("task {id}"), agent: "claude".into(), status, ..Default::default() }
    }

    /// Stuck tasks and your own finished ones are listed with a result card; a run's workers aren't (the run
    /// merges them), other repos' tasks aren't, and enter shows the task as the board would.
    #[test]
    fn agents_open_now_lists_what_needs_you() {
        let dir = scratch("inbox");
        let mut p = Agents::with_paths(Paths { agents: dir.join("agents"), wt: dir.join("wt") });
        let root = dir.join("repo");
        let repo = root.display().to_string();
        p.repo = Some(RepoInfo { root: root.clone(), name: "repo".into(), branch: "main".into() });
        let mut stuck = task("t1", &repo, Status::Blocked);
        stuck.question = "Allow Bash: cargo test?".into();
        stuck.recent = vec!["Edit src/lib.rs".into(), "Bash cargo build".into()];
        let mut done = task("t2", &repo, Status::Review);
        (done.added, done.removed, done.files, done.cost_usd, done.gate) = (120, 8, 3, 0.42, "pass".into());
        done.summary = "Added the parser.\nTests pass.".into();
        done.file_stats = vec![("src/a.rs".into(), 10, 2), ("src/parser.rs".into(), 100, 5)];
        let mut worker = task("t3", &repo, Status::Review);
        worker.run = "r1".into();
        let elsewhere = task("t4", "/some/other/repo", Status::Blocked);
        let mut failed = task("t5", &repo, Status::Review);
        failed.error = "cargo test failed\nmore".into();
        p.store.tasks = vec![stuck, done, worker, elsewhere, failed, task("t6", &repo, Status::Running)];
        p.store.runs = vec![Run { id: "r1".into(), repo: repo.clone(), goal: "split the parser".into(), state: RunState::Review, branch: "oriel/lead-split".into(), merged: 2, ..Default::default() }];
        let items = p.open_items();
        let texts: Vec<&str> = items.iter().map(|o| o.text.as_str()).collect();
        assert_eq!(texts, ["task t1 needs you: Allow Bash: cargo test?", "task t2 is ready for review", "task t5 failed: cargo test failed", "lead run, ready for review: split the parser"]);
        assert_eq!(items[0].tag.as_deref(), Some("agent-task:t1"), "its terminal tab isn't listed again");
        assert!(items[0].needs_you() && !items[1].needs_you() && items[2].needs_you());
        let card = &items[1].detail;
        assert_eq!(card[0], "claude · +120 −8 · 3 files · $0.42");
        assert!(card.contains(&"merge gate: pass".to_string()) && card.contains(&"Tests pass.".to_string()));
        assert_eq!(card.iter().position(|l| l.starts_with("src/parser.rs")).unwrap() + 1, card.iter().position(|l| l.starts_with("src/a.rs")).unwrap(), "biggest change first");
        assert!(items[3].detail[0].starts_with("2 merged into oriel/lead-split"));
        // enter: the stuck one's tab (it has one open), the run's panel
        p.live("t1").pane = true;
        let k = Kit::new();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut actions = vec![];
        let mut cx = Cx { id: 1, theme: &k.theme, config: &k.config, tx: &tx, actions: &mut actions, focused: true, time: 0.0 };
        assert!(p.answer_open("task:t1", Reply::Go, &mut cx));
        assert_eq!(p.selected().as_deref(), Some("t1"));
        assert!(!p.answer_open("task:t1", Reply::Pick(0), &mut cx), "nothing here is answered with a number");
        assert!(p.answer_open("run:r1", Reply::Go, &mut cx) && p.lead_focus);
        assert!(!p.answer_open("task:gone", Reply::Go, &mut cx));
        // mid-form: left alone
        p.mode = Mode::Batch(super::super::BatchView { gate: super::super::Input::new("", false), editing: false });
        assert!(!p.answer_open("task:t2", Reply::Go, &mut cx));
        assert!(matches!(p.mode, Mode::Batch(_)));
        // a line comment being typed in a diff is left alone too
        let mut d = super::super::DiffView::new("t2");
        d.typing = Some(super::super::Input::new("half a thought", false));
        p.mode = Mode::Diff(d);
        assert!(!p.answer_open("task:t2", Reply::Go, &mut cx));
        assert!(matches!(&p.mode, Mode::Diff(d) if d.typing.as_ref().is_some_and(|t| t.text == "half a thought")));
        assert!(actions.iter().any(|a| matches!(a, Action::FocusTag(t) if t == "agent-task:t1")), "went to its tab");
    }
}
