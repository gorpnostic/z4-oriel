//! Running your own tasks together, no lead: mark some on the board (space), enter, and they run as a run of
//! their own: in parallel (up to the run's limit) or one after another, highest priority first, each with the
//! agent and model you gave it and its own budget. Finished work goes through the same merge queue as lead runs
//! (conflict check + gate, fix rounds, one fresh retry), into one branch you review and merge at the end.

use super::store::{self, Run, RunState, Status};
use super::lead::RunLive;
use super::{Agents, Msg, git};
use crate::pane::Cx;

impl Agents {
    /// Start a run of these hand-made TODO tasks. `serial` = one after another, in the order given.
    pub(super) fn start_batch(&mut self, ids: &[String], serial: bool, cx: &mut Cx) -> Result<String, String> {
        let Some(repo) = self.repo.clone() else { return Err("open a repo first".into()) };
        let tasks: Vec<_> = ids.iter().filter_map(|id| self.task(id).cloned()).filter(|t| t.status == Status::Todo && t.run.is_empty()).collect();
        if tasks.is_empty() {
            return Err("mark tasks in TODO first (space)".into());
        }
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
            ..Default::default()
        });
        self.runs_live.insert(id.clone(), RunLive::new());
        self.lead_focus = true;
        self.save();
        let (root, id2) = (repo.root.clone(), id.clone());
        self.spawn(cx, move |send| send(Msg::RunStarted(id2, git::start_run(&root, &wt, &format!("batch-{slug}")))));
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
            // your own "after" links, kept to the tasks in this run; in order = each waits for the one before
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

    /// A task in a run of yours settled: when every one is merged (or set aside), the run is ready to review.
    pub(super) fn batch_check(&mut self, run_id: &str, cx: &mut Cx) {
        let Some(run) = self.run_ref(run_id).cloned() else { return };
        if !run.manual || run.state != RunState::Running {
            return;
        }
        let tasks = self.run_tasks(run_id);
        if tasks.is_empty() || tasks.iter().any(|t| t.status != Status::Done) {
            return;
        }
        let merged = tasks.iter().filter(|t| t.outcome == "merged").count();
        let total = tasks.len();
        if let Some(r) = self.run_mut(run_id) {
            r.state = RunState::Review;
            r.finished = store::now();
            let msg = format!("{merged} of {total} merged into {} · review it (d) and merge it (m)", r.branch);
            r.log(&msg);
            r.summary = msg.clone();
        }
        self.save();
        cx.alert(crate::alerts::Kind::AgentDone, format!("your run is ready: {merged} of {total} tasks merged · d reviews, m merges"));
    }
}
