//! Running your own tasks together, no lead: mark some on the board (space), enter, and they run as a run of
//! their own: in parallel (up to the run's limit) or one after another, highest priority first, each with the
//! agent and model you gave it and its own budget. Finished work goes through the same merge queue as lead runs
//! (conflict check + gate, fix rounds, one fresh retry), into one branch you review and merge at the end.

use super::lead::{self, RunLive};
use super::store::{self, Run, RunState, Status};
use super::{Agents, git};
use crate::pane::Cx;

impl Agents {
    /// Start a run of these hand-made TODO tasks (this repo's). `serial` = one after another, in the order given
    /// as far as their own "after" links allow. `gate` = the command every merge must pass ("" = none).
    pub(super) fn start_batch(&mut self, ids: &[String], serial: bool, gate: &str, cx: &mut Cx) -> Result<String, String> {
        let Some(repo) = self.repo.clone() else { return Err("open a repo first".into()) };
        let key = repo.root.display().to_string();
        let tasks: Vec<_> = ids.iter().filter_map(|id| self.task(id).cloned()).filter(|t| t.status == Status::Todo && t.run.is_empty() && t.repo == key).collect();
        if tasks.is_empty() {
            return Err("mark tasks in TODO first (space)".into());
        }
        if let Some(t) = tasks.iter().find(|t| !self.agent_ok(&t.agent)) {
            return Err(format!("\"{}\" uses {}, which isn't installed — edit it (e) to pick another agent", t.title, t.agent));
        }
        // a link to a task that isn't coming along would wait forever (merged ones are simply met)
        for t in &tasks {
            for d in t.depends_on.iter().filter(|d| !tasks.iter().any(|x| x.id == **d)) {
                match self.task(d) {
                    Some(x) if x.status == Status::Done && x.outcome == "merged" => {}
                    None => {}
                    Some(x) if x.status == Status::Done => return Err(format!("\"{}\" waits for \"{}\", which was discarded — edit the link (e)", t.title, x.title)),
                    Some(x) => return Err(format!("\"{}\" waits for \"{}\", which isn't marked — mark it too (space) or edit the link (e)", t.title, x.title)),
                }
            }
        }
        let order = lead::batch_order(&tasks)?;
        let tasks: Vec<store::Task> = order.iter().filter_map(|id| tasks.iter().find(|t| t.id == *id).cloned()).collect();
        let taken: Vec<store::Task> = self.store.runs.iter().map(|r| store::Task { id: r.id.clone(), ..Default::default() }).chain(self.store.tasks.iter().cloned()).collect();
        let id = store::new_id(&taken);
        let names: Vec<String> = tasks.iter().map(|t| t.title.clone()).collect();
        let goal = format!("{} {} task{}: {}", if serial { "in order," } else { "together," }, tasks.len(), if tasks.len() == 1 { "" } else { "s" }, names.join(" · "));
        let slug = store::slug(&names.first().cloned().unwrap_or_else(|| "batch".into()), &id);
        let wt = self.paths.wt.join(&repo.name).join(format!("batch-{slug}"));
        self.store.runs.push(Run {
            id: id.clone(),
            repo: repo.root.display().to_string(),
            goal,
            agent: "you".into(),
            state: RunState::Starting,
            budget_usd: self.lead_cfg.run_budget_usd,
            max_parallel: if serial { 1 } else { self.lead_cfg.max_parallel.clamp(1, 5) },
            created: store::now(),
            manual: true,
            serial,
            batch: tasks.iter().map(|t| t.id.clone()).collect(),
            gate: Some(gate.trim().to_string()),
            ..Default::default()
        });
        self.runs_live.insert(id.clone(), RunLive::new());
        self.remember_gate(gate);
        self.lead_focus = true;
        self.save();
        self.starting.insert(id.clone(), (repo.root.clone(), wt, format!("batch-{slug}")));
        let seen = self.dirty_seen.get(&key).cloned().unwrap_or_default();
        self.spawn_run_start(&id, git::Dirty::Ask(seen), cx);
        Ok(id)
    }

    /// The run's branch exists: hand the tasks to it and start what can start.
    pub(super) fn begin_batch(&mut self, run_id: &str, cx: &mut Cx) {
        let Some(run) = self.run_ref(run_id).cloned() else { return };
        let roster = self.roster();
        let mut prev: Option<String> = None;
        for tid in &run.batch {
            let Some(t) = self.task_mut(tid) else { continue };
            // what the roster says for this agent fills in the caps the task didn't set
            let w = roster.iter().find(|w| w.agent == t.agent && (t.model.is_empty() || w.model == t.model)).or_else(|| roster.iter().find(|w| w.agent == t.agent)).cloned().unwrap_or_default();
            t.run = run_id.to_string();
            t.repo = run.repo.clone();
            t.mode = "headless".into();
            t.queued = true;
            t.want_merge = true;
            t.key = t.id.clone();
            t.worker = if w.name.is_empty() { t.agent.clone() } else { w.name.clone() };
            t.tier = w.tier.clone();
            t.max_turns = if t.max_turns > 0 { t.max_turns } else { w.max_turns };
            t.budget_usd = if t.budget_usd > 0.0 { t.budget_usd } else { w.budget_usd };
            t.last = "queued".into();
            // your own "after" links, kept to the tasks in this run (start_batch refused links to open tasks
            // outside it, so only merged ones go); in order = each waits for the one before, in an order those
            // links allow (start_batch sorted the batch)
            let batch = run.batch.clone();
            t.depends_on.retain(|d| batch.contains(d));
            if run.serial {
                if let Some(p) = &prev {
                    if !t.depends_on.contains(p) {
                        t.depends_on.push(p.clone());
                    }
                }
            }
            prev = Some(tid.clone());
            self.live.insert(tid.clone(), Default::default());
        }
        if let Some(r) = self.run_mut(run_id) {
            r.log(&format!("{} tasks, {}", run.batch.len(), if run.serial { "one after another" } else { "up to the limit at once" }));
        }
        self.save();
        self.schedule(cx);
    }

    /// A run of yours (or a lead run whose lead is done) is ready to review once nothing is going any more and
    /// nothing will start by itself (see `working`): what didn't make it — blocked, failed, discarded, not started
    /// for lack of budget — is set aside and named in the summary. Called from sync for every such run, so no
    /// path can leave one running forever.
    pub(super) fn batch_check(&mut self, run_id: &str, cx: &mut Cx) {
        let Some(run) = self.run_ref(run_id).cloned() else { return };
        if !(run.manual || run.finishing) || run.state != RunState::Running || !self.working(run_id).is_empty() {
            return;
        }
        let tasks = self.run_tasks(run_id);
        let merged = tasks.iter().filter(|t| t.status == Status::Done && t.outcome == "merged").count();
        let total = tasks.len();
        let aside = self.set_aside(run_id);
        let extra = if aside.is_empty() { String::new() } else { format!(" · {}", aside.join(" · ")) };
        let (log, alert) = if run.manual {
            (format!("{merged} of {total} merged{extra} into {} · review it (d) and merge it (m)", run.branch), format!("your run is ready: {merged} of {total} tasks merged{extra} · d reviews, m merges"))
        } else {
            let m = format!("lead finished · review {} (d) and merge it (m){extra}", run.branch);
            (m.clone(), m)
        };
        if let Some(r) = self.run_mut(run_id) {
            r.state = RunState::Review;
            r.finished = store::now();
            r.log(&log);
            if r.manual {
                r.summary = log.clone();
            }
        }
        self.save();
        cx.alert(crate::alerts::Kind::AgentDone, alert);
    }
}
