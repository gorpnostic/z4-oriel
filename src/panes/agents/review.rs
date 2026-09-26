//! Review in the diff view: a line cursor, comments anchored to `path:line`, a second opinion from another
//! vendor's AI whose findings land in the same list (blocking ones ticked, optional ones not), and one message
//! with everything ticked, sent through the task's usual feedback path (its session carries on) or to a run's
//! lead. docs/research-orchestration.md #9: codex reviews Claude's work, Claude reviews everyone else's.

use super::git::{self, Kind};
use super::input::Input;
use super::{Agents, DiffView, Mode, Msg, run};
use crate::pane::{Action, Cx};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// What one second opinion may cost.
pub const REVIEW_BUDGET: f64 = 1.0;
/// How much of a diff a reviewer is shown (the rest it can read in the checkout).
const REVIEW_CHARS: usize = 60_000;

/// The findings a reviewer returns (claude --json-schema, codex --output-schema, kimi: a ```json block).
pub const FINDINGS_SCHEMA: &str = r#"{"type":"object","properties":{"blocking":{"type":"array","items":{"type":"object","properties":{"path":{"type":"string"},"line":{"type":"integer"},"why":{"type":"string"},"repro":{"type":"string"}},"required":["path","line","why","repro"],"additionalProperties":false}},"optional":{"type":"array","items":{"type":"object","properties":{"path":{"type":"string"},"line":{"type":"integer"},"why":{"type":"string"},"repro":{"type":"string"}},"required":["path","line","why","repro"],"additionalProperties":false}}},"required":["blocking","optional"],"additionalProperties":false}"#;

/// A comment on one line of a diff, waiting to go out with the next send: yours, or a second opinion's finding.
#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    pub path: String,
    /// Its line: in the new version, or (`old`) a removed line's number before. 0 = the whole file.
    pub line: u32,
    pub old: bool,
    /// The code on that line, quoted in the message (line numbers move once the agent edits).
    pub code: String,
    pub text: String,
    /// None = yours; Some(true) = a reviewer's blocking finding, Some(false) = an optional one.
    pub finding: Option<bool>,
    /// Goes out with the next send (space toggles it).
    pub on: bool,
}

impl Note {
    pub fn anchor(&self) -> String {
        match (self.line, self.old) {
            (0, _) => self.path.clone(),
            (n, true) => format!("{}:{n} (a removed line)", self.path),
            (n, false) => format!("{}:{n}", self.path),
        }
    }

    /// Does it sit on this line of file `path`?
    pub fn at(&self, path: &str, l: &git::DLine) -> bool {
        self.path == path
            && self.line > 0
            && match l.kind {
                Kind::Del => self.old && l.old == Some(self.line),
                Kind::Add | Kind::Ctx => !self.old && l.new == Some(self.line),
                _ => false,
            }
    }
}

/// The message your ticked notes make, after whatever you typed in the box.
pub fn message(notes: &[Note], text: &str) -> String {
    let on: Vec<&Note> = notes.iter().filter(|n| n.on).collect();
    let text = text.trim();
    if on.is_empty() {
        return text.to_string();
    }
    let mut s = String::new();
    if !text.is_empty() {
        s.push_str(text);
        s.push_str("\n\n");
    }
    s.push_str("Review comments on your changes (path:line in the new version, then the line as it was):\n");
    for n in on {
        let tag = match n.finding {
            Some(true) => "[blocking] ",
            Some(false) => "[optional] ",
            None => "",
        };
        s.push_str(&format!("- {}: {tag}{}\n", n.anchor(), n.text.trim()));
        if !n.code.trim().is_empty() {
            s.push_str(&format!("    > {}\n", crate::ui::fit(n.code.trim(), 160)));
        }
    }
    s.push_str("Deal with each of them, then finish as usual.");
    s
}

/// One finding from a reviewer.
#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    pub path: String,
    pub line: u32,
    pub why: String,
    pub repro: String,
    pub blocking: bool,
}

/// Everything a second opinion needs, off the UI thread.
pub struct ReviewJob {
    wt: PathBuf,
    base: String,
    author: String,
    what: String,
    agent: String,
    model: String,
    pub by: String,
    bin: PathBuf,
    fake: Option<run::Fake>,
    tmp: PathBuf,
}

impl ReviewJob {
    /// Show the reviewer the diff (cut to REVIEW_CHARS; it can read the rest) and collect its findings. Blocking.
    pub fn run(&self) -> Result<Review, String> {
        let mut diff = git::diff_text(&self.wt, &self.base, "", 1, 1_000_000)?;
        if diff.len() > REVIEW_CHARS {
            let cut = (0..=REVIEW_CHARS).rev().find(|&i| diff.is_char_boundary(i)).unwrap_or(0);
            diff.truncate(cut);
            diff.push_str("\n… (the diff goes on: read the changed files in the checkout)");
        }
        let spec = run::review_spec(&self.agent, &self.bin, &self.model, &review_prompt(&self.author, &self.what, &diff), &self.wt, &self.tmp, REVIEW_BUDGET);
        let o = run::run(&spec, &Arc::new(AtomicBool::new(false)), self.fake.as_ref(), &mut |_| {});
        match findings_of(&o) {
            Some(findings) => Ok(Review { by: self.by.clone(), findings, cost: o.cost }),
            None => Err(o.error.unwrap_or_else(|| "its answer had no findings list".into())),
        }
    }
}

/// A second opinion, back from the reviewer.
pub struct Review {
    /// "codex" / "claude · sonnet"
    pub by: String,
    pub findings: Vec<Finding>,
    pub cost: f64,
}

/// `{blocking: [...], optional: [...]}` (from structured output, or the last JSON object in its answer).
pub fn parse_findings(v: &Value) -> Option<Vec<Finding>> {
    if !v["blocking"].is_array() && !v["optional"].is_array() {
        return None;
    }
    let mut out = vec![];
    for (key, blocking) in [("blocking", true), ("optional", false)] {
        for f in v[key].as_array().into_iter().flatten() {
            let s = |k: &str| f[k].as_str().unwrap_or("").trim().to_string();
            let why = if s("why").is_empty() { s("reason") } else { s("why") };
            if why.is_empty() {
                continue;
            }
            let line = f["line"].as_u64().or_else(|| f["line"].as_str().and_then(|x| x.trim().parse().ok())).unwrap_or(0) as u32;
            out.push(Finding { path: s("path").trim_start_matches("./").replace('\\', "/"), line, why, repro: s("repro"), blocking });
        }
    }
    Some(out)
}

fn findings_of(o: &run::Outcome) -> Option<Vec<Finding>> {
    if let Some(f) = o.report.as_ref().and_then(parse_findings) {
        return Some(f);
    }
    // a JSON object at the end of its answer (or in a ```json block)
    let t = o.text.trim();
    let body = match t.rfind("```") {
        Some(end) if end > 0 => t[..end].rfind("```").map(|start| &t[start + 3..end]).unwrap_or(t),
        _ => t,
    };
    let (a, b) = (body.find('{')?, body.rfind('}')?);
    parse_findings(&serde_json::from_str(body.get(a..=b)?).ok()?)
}

/// What the reviewer is asked.
pub fn review_prompt(author: &str, what: &str, diff: &str) -> String {
    format!(
        "You are reviewing a change another AI ({author}) made, before it's merged. You didn't write it: look for what is actually wrong with it.\n\n\
         THE TASK IT WAS GIVEN:\n{}\n\n\
         Report only real problems: bugs, crashes, security holes, data loss, what the task asked for that's missing or broken, tests that don't test anything. No style or naming nits, no praise.\n\
         - blocking: must be fixed before this merges.\n\
         - optional: worth a look, fine to leave.\n\
         For each: path (as in the diff), line (in the new version; 0 for the whole file), why (one or two sentences), repro (how to see it go wrong, or \"\").\n\
         You may read the files in this checkout for context; don't change anything. Reply with only this JSON: {{\"blocking\": [...], \"optional\": [...]}} (empty lists when it's fine).\n\n\
         THE CHANGE (git diff against where it started):\n{diff}",
        what.trim()
    )
}

fn code_line(l: &git::DLine) -> bool {
    matches!(l.kind, Kind::Add | Kind::Del | Kind::Ctx)
}

impl DiffView {
    pub fn new(id: &str) -> DiffView {
        DiffView { id: id.to_string(), data: None, file: 0, scroll: 0, cursor: 0, typing: None }
    }

    pub fn files(&self) -> &[git::DiffFile] {
        self.data.as_ref().and_then(|d| d.as_ref().ok()).map(|d| d.files.as_slice()).unwrap_or(&[])
    }

    /// The code line under the cursor, and its file.
    pub fn at(&self) -> Option<(&git::DiffFile, &git::DLine)> {
        let f = self.files().get(self.file)?;
        f.lines.get(self.cursor).filter(|l| code_line(l)).map(|l| (f, l))
    }

    /// Move the cursor `by` lines, onto a code line (hunk headers are skipped).
    pub fn step(&mut self, by: isize) {
        let Some(f) = self.files().get(self.file) else { return };
        let n = f.lines.len();
        if n == 0 {
            return;
        }
        let want = (self.cursor as isize + by).clamp(0, n as isize - 1) as usize;
        let fwd = (want..n).find(|&i| code_line(&f.lines[i]));
        let back = (0..=want).rev().find(|&i| code_line(&f.lines[i]));
        self.cursor = if by >= 0 { fwd.or(back) } else { back.or(fwd) }.unwrap_or(want);
    }

    pub fn set_file(&mut self, i: usize) {
        let n = self.files().len();
        if n == 0 {
            return;
        }
        self.file = i.min(n - 1);
        self.cursor = 0;
        self.scroll = 0;
        self.step(0);
    }

    /// The first code line of the next (`fwd`) or previous hunk.
    fn hunk(&mut self, fwd: bool) {
        let Some(f) = self.files().get(self.file) else { return };
        let heads: Vec<usize> = f.lines.iter().enumerate().filter(|(_, l)| l.kind == Kind::Hunk).map(|(i, _)| i).collect();
        let to = if fwd { heads.iter().find(|&&h| h > self.cursor).copied() } else { heads.iter().rev().find(|&&h| h + 1 < self.cursor).copied() };
        if let Some(h) = to {
            self.cursor = h;
            self.step(1);
        }
    }
}

impl Agents {
    /// Your comments and findings on this task or run.
    pub(super) fn notes_of(&self, id: &str) -> &[Note] {
        self.notes.get(id).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Ticked ones (what enter sends).
    pub(super) fn ticked(&self, id: &str) -> usize {
        self.notes_of(id).iter().filter(|n| n.on).count()
    }

    /// The box that sends your ticked comments (with anything else you want to say) through the usual path.
    fn open_compose(&mut self, id: &str) {
        self.open_comment(id.to_string());
        self.comment_back = Some(id.to_string());
    }

    /// Keys in the diff view.
    pub(super) fn diff_key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        let Mode::Diff(v) = &self.mode else { return false };
        let id = v.id.clone();
        if v.typing.is_some() {
            return self.note_key(k);
        }
        // the same guard as on the board: no second git step (or a follow-up) for a task while one is running
        if matches!(k.code, KeyCode::Char('m' | 'x' | 'c' | 'C' | 'V')) && self.task(&id).is_some() {
            if let Err(why) = self.can_act(&id) {
                cx.notify(why);
                return true;
            }
        }
        let block = self.comment_block(&id);
        let pane = self.live.get(&id).is_some_and(|l| l.pane);
        let ticked = self.ticked(&id);
        let is_run = self.run_ref(&id).is_some();
        let Mode::Diff(v) = &mut self.mode else { return false };
        let on_line: Vec<usize> = match v.at() {
            Some((f, l)) => self.notes.get(&id).map(|ns| ns.iter().enumerate().filter(|(_, n)| n.at(&f.path, l)).map(|(i, _)| i).collect()).unwrap_or_default(),
            None => vec![],
        };
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('d') => self.mode = Mode::Board,
            KeyCode::Down | KeyCode::Char('j') => v.step(1),
            KeyCode::Up | KeyCode::Char('k') => v.step(-1),
            KeyCode::PageDown => v.step(20),
            KeyCode::PageUp => v.step(-20),
            // space ticks what's on this line, else pages on
            KeyCode::Char(' ') if !on_line.is_empty() => {
                if let Some(ns) = self.notes.get_mut(&id) {
                    let on = !on_line.iter().all(|&i| ns[i].on);
                    for &i in &on_line {
                        ns[i].on = on;
                    }
                }
            }
            KeyCode::Char(' ') => v.step(20),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => v.set_file(v.file + 1),
            KeyCode::Left | KeyCode::Char('h') | KeyCode::BackTab => v.set_file(v.file.saturating_sub(1)),
            KeyCode::Char('J') => v.hunk(true),
            KeyCode::Char('K') => v.hunk(false),
            KeyCode::Home | KeyCode::Char('g') => {
                v.cursor = 0;
                v.step(0);
            }
            KeyCode::End | KeyCode::Char('G') => {
                v.cursor = usize::MAX / 2;
                v.step(-1);
            }
            KeyCode::Char('n') | KeyCode::Char('N') => self.jump_note(&id, k.code == KeyCode::Char('n')),
            KeyCode::Delete | KeyCode::Backspace if !on_line.is_empty() => {
                if let Some(ns) = self.notes.get_mut(&id) {
                    for &i in on_line.iter().rev() {
                        ns.remove(i);
                    }
                }
            }
            // c: a comment on this line (on the whole thing where there's no line: the diff isn't in yet)
            KeyCode::Char('c') => match block {
                Some(why) => cx.notify(why),
                None if v.at().is_some() => v.typing = Some(Input::new("", false)),
                None => self.open_compose(&id),
            },
            KeyCode::Char('C') => match block {
                Some(why) => cx.notify(why),
                None => self.open_compose(&id),
            },
            KeyCode::Char('V') => self.second_opinion(&id, cx),
            KeyCode::Enter if ticked > 0 => match block {
                Some(why) => cx.notify(why),
                None => self.open_compose(&id),
            },
            KeyCode::Enter if pane => {
                let tag = self.task(&id).map(|t| t.tag()).unwrap_or_default();
                cx.act(Action::FocusTag(tag));
            }
            KeyCode::Enter => cx.notify(if block.is_some() { "nothing to send" } else { "c comments on the line you're on · C sends a comment on the whole thing · V asks another AI" }),
            KeyCode::Char('R') => {
                if is_run {
                    self.open_run_diff(&id, cx);
                } else {
                    self.open_diff(&id, cx);
                }
            }
            KeyCode::Char('m') => {
                let what = if is_run { super::Pending::MergeRun } else { super::Pending::Merge };
                self.mode = Mode::Confirm(super::Confirm { id, what });
            }
            KeyCode::Char('x') => {
                let what = if is_run { super::Pending::DiscardRun } else { super::Pending::Discard };
                self.mode = Mode::Confirm(super::Confirm { id, what });
            }
            KeyCode::Char('T') => self.open_try(&id, cx),
            _ => return false,
        }
        true
    }

    /// Typing a line comment: enter adds it to the list, esc drops it.
    fn note_key(&mut self, k: KeyEvent) -> bool {
        let Mode::Diff(v) = &mut self.mode else { return false };
        let Some(inp) = v.typing.as_mut() else { return false };
        match k.code {
            KeyCode::Esc => v.typing = None,
            KeyCode::Enter if !k.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) => {
                let text = inp.text.trim().to_string();
                let note = v.at().filter(|_| !text.is_empty()).map(|(f, l)| Note {
                    path: f.path.clone(),
                    line: if l.kind == Kind::Del { l.old.unwrap_or(0) } else { l.new.unwrap_or(0) },
                    old: l.kind == Kind::Del,
                    code: l.text.clone(),
                    text,
                    finding: None,
                    on: true,
                });
                v.typing = None;
                let id = v.id.clone();
                if let Some(n) = note {
                    self.notes.entry(id).or_default().push(n);
                }
            }
            _ => {
                inp.key(k);
            }
        }
        true
    }

    /// n / N: the cursor to the next (or previous) comment or finding, across files.
    fn jump_note(&mut self, id: &str, fwd: bool) {
        let notes = self.notes_of(id).to_vec();
        let Mode::Diff(v) = &mut self.mode else { return };
        // every (file, line) that has a note, in diff order; notes off the diff's lines sit at the top of their file
        let mut spots: Vec<(usize, usize)> = vec![];
        for (fi, f) in v.files().iter().enumerate() {
            for n in notes.iter().filter(|n| n.path == f.path) {
                let li = f.lines.iter().position(|l| n.at(&f.path, l)).unwrap_or(0);
                if !spots.contains(&(fi, li)) {
                    spots.push((fi, li));
                }
            }
        }
        spots.sort();
        let here = (v.file, v.cursor);
        let to = if fwd { spots.iter().find(|s| **s > here).or(spots.first()) } else { spots.iter().rev().find(|s| **s < here).or(spots.last()) };
        if let Some(&(fi, li)) = to {
            if fi != v.file {
                v.set_file(fi);
            }
            v.cursor = li;
            v.step(0);
        }
    }

    // ------------------------------------------------------------------ the second opinion

    /// Who gives the second opinion on `author`'s work: another vendor's AI when one is installed (codex for
    /// Claude's work, Claude for everyone else's), else the same one; on its strongest roster model.
    pub(super) fn reviewer_for(&self, author: &str) -> Option<(String, String)> {
        let order: &[&str] = if author == "claude" { &["codex", "claude", "kimi"] } else { &["claude", "codex", "kimi"] };
        let agent = order.iter().find(|a| self.fake_reviewer.is_some() || self.installed(a).is_some())?;
        let rank = |t: &str| super::roster::TIERS.iter().position(|x| *x == t).unwrap_or(1);
        let model = self.roster().into_iter().filter(|w| w.agent == *agent && w.enabled).max_by_key(|w| rank(&w.tier)).map(|w| w.model).unwrap_or_default();
        Some((agent.to_string(), model))
    }

    /// V: another AI reviews this task's (or run's) diff, read-only, in the background; its findings join your
    /// comments (see on_reviewed).
    pub(super) fn second_opinion(&mut self, id: &str, cx: &mut Cx) {
        if let Some(by) = self.reviewing.get(id) {
            cx.notify(format!("{by} is already reviewing it"));
            return;
        }
        // a task's worktree against its base; a run's integration branch, in the lead's checkout (kept at its tip)
        let (wt, base, author, what) = if let Some(r) = self.run_ref(id) {
            let mut n: std::collections::HashMap<&str, usize> = Default::default();
            for t in self.run_tasks(id).into_iter().filter(|t| t.outcome == "merged") {
                *n.entry(t.agent.as_str()).or_default() += 1;
            }
            let author = n.into_iter().max_by_key(|x| x.1).map(|x| x.0.to_string()).unwrap_or_else(|| r.agent.clone());
            (r.worktree.clone(), r.base_sha.clone(), author, r.goal.clone())
        } else if let Some(t) = self.task(id) {
            (t.worktree.clone(), t.base_sha.clone(), t.agent.clone(), format!("{}\n{}", t.title, t.prompt))
        } else {
            return;
        };
        if wt.is_empty() || base.is_empty() || !Path::new(&wt).is_dir() {
            cx.notify("nothing to review: its checkout is gone");
            return;
        }
        let Some((agent, model)) = self.reviewer_for(&author) else {
            cx.notify("a second opinion needs claude, codex or kimi installed");
            return;
        };
        let job = self.review_job(PathBuf::from(wt), base, author, what, agent, model);
        cx.notify(format!("{} is reviewing it (read-only, up to ${REVIEW_BUDGET:.2}) — its findings show up here", job.by));
        self.reviewing.insert(id.to_string(), job.by.clone());
        let id = id.to_string();
        self.spawn(cx, move |send| send(Msg::Reviewed(id, job.run())));
    }

    /// A second opinion to run on a background thread (the reviewer's binary, or the tests' fake).
    fn review_job(&self, wt: PathBuf, base: String, author: String, what: String, agent: String, model: String) -> ReviewJob {
        let by = if model.is_empty() { agent.clone() } else { format!("{agent} · {model}") };
        let bin = self.installed(&agent).unwrap_or_else(|| PathBuf::from(&agent));
        ReviewJob { wt, base, author, what, agent, model, by, bin, fake: self.fake_reviewer.clone(), tmp: self.paths.agents.join("tmp") }
    }

    /// The lead's `review` tool: a second opinion on one of its tasks, answered to the lead (not queued for you):
    /// its blocking and optional findings, to send back with send_followup before merging (research #9).
    pub(super) fn call_review(&mut self, run: &super::store::Run, id: &str, reply: super::lead::Reply, cx: &mut Cx) {
        let t = match self.task_in(run, id) {
            Ok(t) => t,
            Err(e) => {
                let _ = reply.send(Err(e));
                return;
            }
        };
        let busy = self.live.get(&t.id).is_some_and(|l| l.stop.is_some() || l.busy || l.checking);
        let why = if matches!(t.status, super::store::Status::Running | super::store::Status::Blocked) || busy {
            Some(format!("{} is still working — review it once it's finished", t.id))
        } else if t.worktree.is_empty() || t.base_sha.is_empty() || !Path::new(&t.worktree).is_dir() {
            Some(format!("{} has no worktree any more", t.id))
        } else {
            None
        };
        let who = self.reviewer_for(&t.agent);
        let (Some((agent, model)), None) = (who.clone(), why.clone()) else {
            let _ = reply.send(Err(why.unwrap_or_else(|| "no reviewer is installed (claude, codex or kimi)".into())));
            return;
        };
        let job = self.review_job(PathBuf::from(&t.worktree), t.base_sha.clone(), t.agent.clone(), format!("{}\n{}", t.title, t.prompt), agent, model);
        let (run_id, tid) = (run.id.clone(), t.id.clone());
        self.spawn(cx, move |send| {
            let r = job.run();
            let cost = r.as_ref().map(|rv| rv.cost).unwrap_or(0.0);
            let answer = r.map(|rv| {
                let list = |b: bool| -> Vec<Value> { rv.findings.iter().filter(|f| f.blocking == b).map(|f| serde_json::json!({"path": f.path, "line": f.line, "why": f.why, "repro": f.repro})).collect() };
                serde_json::json!({"task": tid, "reviewer": rv.by, "blocking": list(true), "optional": list(false), "next": "send the blocking ones back with send_followup (quote path:line), then merge once it's fixed; the optional ones are your call"}).to_string()
            });
            let line = match &answer {
                Ok(_) => format!("second opinion on {tid} from {}", job.by),
                Err(e) => format!("second opinion on {tid} failed: {e}"),
            };
            let _ = reply.send(answer);
            send(Msg::ReviewSpent(run_id, cost, line));
        });
    }

    pub(super) fn on_reviewed(&mut self, id: &str, r: Result<Review, String>, cx: &mut Cx) {
        let by = self.reviewing.remove(id).unwrap_or_default();
        let rv = match r {
            Ok(rv) => rv,
            Err(e) => {
                cx.notify(format!("the second opinion ({by}) failed: {e}"));
                return;
            }
        };
        // what it cost counts today (a task's own figure is its agent's)
        if let Some(t) = self.task_mut(id) {
            t.spent_before += rv.cost;
        } else if let Some(r) = self.run_mut(id) {
            r.cost_usd += rv.cost;
            r.log(&format!("second opinion from {}: {} blocking, {} optional", rv.by, rv.findings.iter().filter(|f| f.blocking).count(), rv.findings.iter().filter(|f| !f.blocking).count()));
        }
        // the lines they point at, quoted from the diff when it's open
        let files: Vec<git::DiffFile> = match &self.mode {
            Mode::Diff(v) if v.id == id => v.files().to_vec(),
            _ => vec![],
        };
        let code = |path: &str, line: u32| files.iter().find(|f| f.path == path).and_then(|f| f.lines.iter().find(|l| l.kind != Kind::Del && l.new == Some(line) && code_line(l))).map(|l| l.text.clone()).unwrap_or_default();
        let notes = self.notes.entry(id.to_string()).or_default();
        // a new opinion replaces the last one's findings; your own comments stay
        notes.retain(|n| n.finding.is_none());
        for f in &rv.findings {
            let text = if f.repro.is_empty() { f.why.clone() } else { format!("{} (repro: {})", f.why, f.repro) };
            notes.push(Note { path: f.path.clone(), line: f.line, old: false, code: code(&f.path, f.line), text, finding: Some(f.blocking), on: f.blocking });
        }
        let (b, o) = (rv.findings.iter().filter(|f| f.blocking).count(), rv.findings.iter().filter(|f| !f.blocking).count());
        let name = self.task(id).map(|t| t.title.clone()).or_else(|| self.run_ref(id).map(|r| r.goal.lines().next().unwrap_or("").to_string())).unwrap_or_default();
        let msg = if b + o == 0 { format!("second opinion on \"{name}\" ({}): nothing to fix", rv.by) } else { format!("second opinion on \"{name}\" ({}): {b} blocking, {o} optional — the blocking ones are ticked; d shows them, enter sends", rv.by) };
        cx.alert(crate::alerts::Kind::AgentDone, msg);
        self.save();
    }
}
