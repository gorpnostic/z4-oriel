//! Runs saved as templates, to run again: `S` on a run of your tasks (or a finished lead run) writes its tasks to
//! `<repo>/.oriel/templates/<name>.toml`, a mechanical dump since they're already typed. Change literals into
//! {{args}} by hand (`e` in the list edits the file right there). `W` lists the repo's templates; enter asks for
//! the args, drops the tasks into TODO and opens the run popup (p together, s in order).
//!
//!     name = "fix-issue"
//!     description = "fix an issue and add a test for it"
//!
//!     [[task]]
//!     key = "t1"
//!     title = "Fix issue {{issue}}"
//!     prompt = "..."
//!     agent = "claude"
//!     model = "sonnet"
//!     priority = 1
//!     after = []
//!     budget_usd = 1.5
//!     acceptance = "cargo test"
//!     owns = ["src/**"]

use super::input::Input;
use super::store::{Status, Task};
use super::{Agents, KINDS, Mode, PRIORITIES};
use crate::pane::Cx;
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Template {
    pub name: String,
    pub description: String,
    #[serde(rename = "task")]
    pub tasks: Vec<TaskT>,
}

fn is_zero(n: &i8) -> bool {
    *n == 0
}

fn no_budget(n: &f64) -> bool {
    *n <= 0.0
}

/// One task of a template.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct TaskT {
    /// what `after` refers to
    pub key: String,
    pub title: String,
    pub prompt: String,
    pub agent: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub priority: i8,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    #[serde(skip_serializing_if = "no_budget")]
    pub budget_usd: f64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub acceptance: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub owns: Vec<String>,
}

pub fn dir(repo: &Path) -> PathBuf {
    repo.join(".oriel").join("templates")
}

/// A file-safe name: "Fix issue #12 fast!" → "fix-issue-12-fast".
pub fn name_for(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().flat_map(|c| c.to_lowercase()) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 32 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "template".into() } else { out }
}

/// A template file's text: what it is, then the tasks.
pub fn parse(text: &str) -> Result<Template, String> {
    let t: Template = toml::from_str(text).map_err(|e| e.to_string().lines().find(|l| !l.trim().is_empty()).unwrap_or("doesn't parse").trim().to_string())?;
    if t.tasks.is_empty() {
        return Err("it has no [[task]]".into());
    }
    if let Some(i) = t.tasks.iter().position(|x| x.title.trim().is_empty() && x.prompt.trim().is_empty()) {
        return Err(format!("task {} has neither a title nor a prompt", i + 1));
    }
    Ok(t)
}

pub fn to_text(t: &Template) -> Result<String, String> {
    let body = toml::to_string_pretty(t).map_err(|e| e.to_string())?;
    Ok(format!("# oriel template: agents → W runs it. {{{{name}}}} in a title, prompt or command is asked for when it runs.\n{body}"))
}

/// The repo's templates, by file name (ones that don't parse say why).
pub fn list(repo: &Path) -> Vec<(PathBuf, Result<Template, String>)> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir(repo)).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "toml")).collect();
    v.sort();
    v.into_iter().map(|p| {
        let t = std::fs::read_to_string(&p).map_err(|e| e.to_string()).and_then(|s| parse(&s));
        (p, t)
    }).collect()
}

/// Write it under a free name (`name`, then `name-2`…): the path it went to.
pub fn save(repo: &Path, t: &Template) -> Result<PathBuf, String> {
    let d = dir(repo);
    std::fs::create_dir_all(&d).map_err(|e| format!("couldn't make {}: {e}", d.display()))?;
    let base = name_for(&t.name);
    let path = (1..100).map(|n| d.join(if n == 1 { format!("{base}.toml") } else { format!("{base}-{n}.toml") })).find(|p| !p.exists()).ok_or("too many templates with that name")?;
    let mut t = t.clone();
    t.name = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or(base);
    super::store::write_atomic(&path, to_text(&t)?.as_bytes()).map_err(|e| format!("couldn't save {}: {e}", path.display()))?;
    Ok(path)
}

/// The {{args}} it asks for, in the order they first appear.
pub fn args(t: &Template) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for x in &t.tasks {
        let texts = [&x.title, &x.prompt, &x.acceptance, &x.model].into_iter().cloned().chain(x.owns.iter().cloned());
        for s in texts {
            let mut rest = s.as_str();
            while let Some(a) = rest.find("{{") {
                let Some(b) = rest[a + 2..].find("}}") else { break };
                let name = rest[a + 2..a + 2 + b].trim().to_string();
                if !name.is_empty() && !out.contains(&name) {
                    out.push(name);
                }
                rest = &rest[a + 2 + b + 2..];
            }
        }
    }
    out
}

/// The template with its {{args}} filled in.
pub fn fill(t: &Template, vals: &[(String, String)]) -> Template {
    let sub = |s: &str| {
        let mut s = s.to_string();
        for (k, v) in vals {
            s = s.replace(&format!("{{{{{k}}}}}"), v).replace(&format!("{{{{ {k} }}}}"), v);
        }
        s
    };
    let mut t = t.clone();
    for x in &mut t.tasks {
        x.title = sub(&x.title);
        x.prompt = sub(&x.prompt);
        x.acceptance = sub(&x.acceptance);
        x.model = sub(&x.model);
        x.owns = x.owns.iter().map(|o| sub(o)).collect();
    }
    t
}

/// A run's tasks as a template: title, prompt, agent, model, priority, what each comes after, budget, acceptance
/// and owned files. Discarded ones are left out.
pub fn from_run(name: &str, description: &str, tasks: &[&Task]) -> Template {
    let keys: Vec<(String, String)> = tasks.iter().enumerate().map(|(i, t)| (t.id.clone(), format!("t{}", i + 1))).collect();
    let key_of = |dep: &str| -> Option<String> {
        // a lead's own key, or a task id
        tasks.iter().position(|t| t.id == dep || (!t.key.is_empty() && t.key == dep)).map(|i| keys[i].1.clone())
    };
    let tasks = tasks
        .iter()
        .zip(&keys)
        .map(|(t, (_, k))| TaskT {
            key: k.clone(),
            title: t.title.clone(),
            prompt: t.prompt.clone(),
            agent: t.agent.clone(),
            model: t.model.clone(),
            priority: t.priority,
            after: t.depends_on.iter().filter_map(|d| key_of(d)).collect(),
            budget_usd: t.budget_usd,
            acceptance: t.acceptance.clone(),
            owns: t.owns.clone(),
        })
        .collect();
    Template { name: name.to_string(), description: description.to_string(), tasks }
}

// ------------------------------------------------------------------ the board's side

/// S: save a run as a template (its name).
pub struct SaveView {
    pub run: String,
    pub name: Input,
    pub err: String,
}

/// W: the repo's templates.
pub struct TplView {
    pub items: Vec<(PathBuf, Result<Template, String>)>,
    pub sel: usize,
    /// enter on one that has {{args}}: a field each
    pub args: Option<ArgsForm>,
    /// e: the file's text, edited here
    pub edit: Option<(PathBuf, Input, String)>,
}

pub struct ArgsForm {
    pub names: Vec<String>,
    pub inputs: Vec<Input>,
    pub field: usize,
    pub err: String,
}

impl Agents {
    /// S on the lead panel: a run of yours any time, a lead's once its plan is settled.
    pub(super) fn open_save_template(&mut self, run: &str, cx: &mut Cx) {
        let Some(r) = self.run_ref(run) else { return };
        if !r.manual && r.state.active() {
            cx.notify("the lead is still at it — save the run as a template once it's done (its tasks can still change)");
            return;
        }
        if self.template_tasks(run).is_empty() {
            cx.notify("that run has no tasks to save");
            return;
        }
        let first = if r.manual { self.template_tasks(run).first().map(|t| t.title.clone()).unwrap_or_default() } else { r.goal.clone() };
        self.mode = Mode::SaveTemplate(SaveView { run: run.to_string(), name: Input::new(&name_for(first.lines().next().unwrap_or("")), false), err: String::new() });
    }

    /// The run's tasks that go into a template: all but the discarded, in their order.
    fn template_tasks(&self, run: &str) -> Vec<&Task> {
        let Some(r) = self.run_ref(run) else { return vec![] };
        let keep = |t: &&Task| !(t.status == Status::Done && t.outcome != "merged");
        if r.manual {
            r.batch.iter().filter_map(|id| self.task(id)).filter(keep).collect()
        } else {
            let mut v: Vec<&Task> = self.run_tasks(run).into_iter().filter(keep).collect();
            v.sort_by_key(|t| t.created);
            v
        }
    }

    pub(super) fn save_template_key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        let Mode::SaveTemplate(mut v) = std::mem::replace(&mut self.mode, Mode::Board) else { return false };
        v.err.clear();
        match k.code {
            KeyCode::Esc => {}
            KeyCode::Enter => {
                let Some(r) = self.run_ref(&v.run).cloned() else { return true };
                let name = v.name.text.trim().to_string();
                if name.is_empty() {
                    v.err = "give it a name".into();
                    self.mode = Mode::SaveTemplate(v);
                    return true;
                }
                let tasks = self.template_tasks(&v.run);
                let desc = if r.manual { tasks.iter().map(|t| t.title.clone()).collect::<Vec<_>>().join(" · ") } else { r.goal.lines().next().unwrap_or("").to_string() };
                let tpl = from_run(&name, &desc, &tasks);
                match save(Path::new(&r.repo), &tpl) {
                    Ok(p) => {
                        let rel = p.strip_prefix(&r.repo).map(|x| x.display().to_string()).unwrap_or_else(|_| p.display().to_string());
                        cx.notify(format!("saved {rel} ({} tasks) · W runs it · change literals to {{{{args}}}} with e there", tpl.tasks.len()));
                    }
                    Err(e) => {
                        v.err = e;
                        self.mode = Mode::SaveTemplate(v);
                    }
                }
            }
            _ => {
                v.name.key(k);
                self.mode = Mode::SaveTemplate(v);
            }
        }
        true
    }

    /// W: this repo's templates.
    pub(super) fn open_templates(&mut self) {
        let items = self.repo.as_ref().map(|r| list(&r.root)).unwrap_or_default();
        self.mode = Mode::Templates(TplView { items, sel: 0, args: None, edit: None });
    }

    /// Drop a template's tasks into TODO (their after-links and all), marked, and open the run popup.
    fn run_template(&mut self, t: &Template, cx: &mut Cx) {
        let mut made: Vec<(String, String)> = vec![];
        for x in &t.tasks {
            let agent = KINDS.iter().position(|k| k.0 == x.agent.trim()).unwrap_or(self.default_agent());
            let id = self.add_task(&x.title, if x.prompt.trim().is_empty() { &x.title } else { &x.prompt }, agent, &x.model);
            if let Some(tm) = self.task_mut(&id) {
                tm.priority = x.priority.clamp(PRIORITIES[0].0, PRIORITIES[PRIORITIES.len() - 1].0);
                tm.budget_usd = x.budget_usd.max(0.0);
                tm.acceptance = x.acceptance.trim().to_string();
                tm.owns = x.owns.clone();
            }
            made.push((x.key.clone(), id));
        }
        for (x, (_, id)) in t.tasks.iter().zip(made.clone()) {
            let deps: Vec<String> = x.after.iter().filter_map(|a| made.iter().find(|(k, _)| !k.is_empty() && k == a).map(|(_, i)| i.clone())).collect();
            if let Some(tm) = self.task_mut(&id) {
                tm.depends_on = deps;
            }
        }
        self.save();
        self.marked = made.into_iter().map(|x| x.1).collect();
        cx.notify(format!("{} task{} from \"{}\" in TODO · p runs them together, s in order", t.tasks.len(), if t.tasks.len() == 1 { "" } else { "s" }, t.name));
        self.open_batch();
    }

    pub(super) fn templates_key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        let Mode::Templates(mut v) = std::mem::replace(&mut self.mode, Mode::Board) else { return false };
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        // e: the file's text
        if let Some((path, mut inp, _)) = v.edit.take() {
            match k.code {
                KeyCode::Esc => {}
                KeyCode::Char('s') if ctrl => match parse(&inp.text) {
                    Ok(_) => match super::store::write_atomic(&path, inp.text.as_bytes()) {
                        Ok(()) => {
                            cx.notify(format!("saved {}", path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()));
                            let root = self.repo.as_ref().map(|r| r.root.clone()).unwrap_or_default();
                            v.items = list(&root);
                        }
                        Err(e) => v.edit = Some((path, inp, e.to_string())),
                    },
                    Err(e) => v.edit = Some((path, inp, e)),
                },
                _ => {
                    inp.key(k);
                    v.edit = Some((path, inp, String::new()));
                }
            }
            self.mode = Mode::Templates(v);
            return true;
        }
        // its {{args}}
        if let Some(mut a) = v.args.take() {
            a.err.clear();
            match k.code {
                KeyCode::Esc => {}
                KeyCode::Tab | KeyCode::Down => {
                    a.field = (a.field + 1) % a.inputs.len().max(1);
                    v.args = Some(a);
                }
                KeyCode::BackTab | KeyCode::Up => {
                    a.field = (a.field + a.inputs.len().max(1) - 1) % a.inputs.len().max(1);
                    v.args = Some(a);
                }
                KeyCode::Enter if a.field + 1 < a.inputs.len() => {
                    a.field += 1;
                    v.args = Some(a);
                }
                KeyCode::Enter => {
                    if let Some(i) = a.inputs.iter().position(|x| x.text.trim().is_empty()) {
                        a.err = format!("{} is empty", a.names[i]);
                        a.field = i;
                        v.args = Some(a);
                    } else if let Some((_, Ok(t))) = v.items.get(v.sel) {
                        let vals: Vec<(String, String)> = a.names.iter().cloned().zip(a.inputs.iter().map(|x| x.text.trim().to_string())).collect();
                        let t = fill(t, &vals);
                        self.run_template(&t, cx);
                        return true;
                    }
                }
                _ => {
                    if let Some(x) = a.inputs.get_mut(a.field) {
                        x.key(k);
                    }
                    v.args = Some(a);
                }
            }
            self.mode = Mode::Templates(v);
            return true;
        }
        let n = v.items.len();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('W') => return true,
            KeyCode::Up | KeyCode::Char('k') => v.sel = v.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => v.sel = (v.sel + 1).min(n.saturating_sub(1)),
            KeyCode::Enter => match v.items.get(v.sel) {
                Some((_, Ok(t))) => {
                    let names = args(t);
                    if names.is_empty() {
                        let t = t.clone();
                        self.run_template(&t, cx);
                        return true;
                    }
                    v.args = Some(ArgsForm { inputs: names.iter().map(|_| Input::new("", false)).collect(), names, field: 0, err: String::new() });
                }
                Some((p, Err(e))) => cx.notify(format!("{}: {e} — e fixes it", p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default())),
                None => {}
            },
            KeyCode::Char('e') => {
                if let Some((p, _)) = v.items.get(v.sel) {
                    let text = std::fs::read_to_string(p).unwrap_or_default().replace("\r\n", "\n");
                    let mut inp = Input::new(&text, true);
                    inp.cur = 0;
                    v.edit = Some((p.clone(), inp, String::new()));
                }
            }
            _ => {}
        }
        self.mode = Mode::Templates(v);
        true
    }

    /// Text pasted into the name, an arg or the file being edited.
    pub(super) fn templates_paste(&mut self, text: &str) {
        match &mut self.mode {
            Mode::SaveTemplate(v) => v.name.insert(text.trim()),
            Mode::Templates(v) => {
                if let Some((_, inp, _)) = &mut v.edit {
                    inp.insert(text);
                } else if let Some(a) = &mut v.args {
                    if let Some(x) = a.inputs.get_mut(a.field) {
                        x.insert(text.trim());
                    }
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------ drawing

    pub(super) fn draw_save_template(&self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        let Mode::SaveTemplate(v) = &self.mode else { return };
        let n = self.template_tasks(&v.run).len();
        let inner = ui::popup(f, area, 80, 12, &format!("{}save the run as a template", ui::lead("new")), t);
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
        let w = inner.width as usize;
        let head = vec![
            Line::styled(ui::fit(&format!("its {n} task{} (title, prompt, AI, model, priority, order, budget, acceptance, files) go to", if n == 1 { "" } else { "s" }), w), ui::muted(t)),
            Line::styled(ui::fit(&format!(".oriel/templates/{}.toml — W in agents runs it again", name_for(&v.name.text)), w), ui::muted(t)),
        ];
        f.render_widget(Paragraph::new(head), Rect { height: 2, ..inner });
        let r = Rect { y: inner.y + 3, height: 3, ..inner };
        let ri = ui::frame(f, r, "name", None, true, t);
        super::view::draw_input(f, ri, &v.name, "e.g. fix-issue", true, t);
        let hr = Rect { y: inner.bottom().saturating_sub(1), height: 1, ..inner };
        let msg = if v.err.is_empty() { Line::styled("enter save · esc cancel · then change literals to {{args}} (W, e)", ui::muted(t)) } else { Line::styled(v.err.clone(), Style::default().fg(t.danger)) };
        f.render_widget(Paragraph::new(msg).centered(), hr);
    }

    pub(super) fn draw_templates(&self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        let Mode::Templates(v) = &self.mode else { return };
        let h = (v.items.len() as u16 * 2 + 9).clamp(12, 30);
        let inner = ui::popup(f, area, 96, h, &format!("{}templates", ui::lead("new")), t);
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
        let w = inner.width as usize;
        let root = self.repo.as_ref().map(|r| dir(&r.root).display().to_string()).unwrap_or_default();
        f.render_widget(Paragraph::new(Span::styled(ui::fit(&format!("runs saved to run again · {root}"), w), ui::muted(t))), Rect { height: 1, ..inner });
        let mut y = inner.y + 2;
        if v.items.is_empty() {
            let lines = vec![Line::styled("none yet", bold_fg(t)), Line::styled("S on the lead panel saves a run of your tasks (or a finished lead run) as one.", ui::muted(t))];
            f.render_widget(Paragraph::new(lines), Rect { y, height: 2, ..inner });
        }
        for (i, (p, tpl)) in v.items.iter().enumerate() {
            if y + 1 >= inner.bottom().saturating_sub(1) {
                break;
            }
            let on = i == v.sel;
            let bg = if on { Style::default().bg(tint(t.accent, 0.15)) } else { Style::default() };
            let file = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let (l1, l2) = match tpl {
                Ok(x) => {
                    let a = args(x);
                    let args = if a.is_empty() { String::new() } else { format!(" · asks for {}", a.iter().map(|n| format!("{{{{{n}}}}}")).collect::<Vec<_>>().join(" ")) };
                    let agents: Vec<String> = {
                        let mut v: Vec<String> = vec![];
                        for x in &x.tasks {
                            if !v.contains(&x.agent) {
                                v.push(x.agent.clone());
                            }
                        }
                        v
                    };
                    (
                        Line::from(vec![Span::styled(format!("{} {file}", if on { "›" } else { " " }), if on { bold_accent(t) } else { bold_fg(t) }), Span::styled(format!("  {} task{} · {}{args}", x.tasks.len(), if x.tasks.len() == 1 { "" } else { "s" }, agents.join(", ")), ui::muted(t))]),
                        Line::styled(ui::fit(&format!("    {}", x.description), w), ui::muted(t)),
                    )
                }
                Err(e) => (Line::from(vec![Span::styled(format!("{} {file}", if on { "›" } else { " " }), if on { bold_accent(t) } else { bold_fg(t) })]), Line::styled(ui::fit(&format!("    ✗ {e}"), w), Style::default().fg(t.danger))),
            };
            f.render_widget(Paragraph::new(vec![l1.style(bg), l2.style(bg)]), Rect { y, height: 2, ..inner });
            y += 2;
        }
        let hr = Rect { y: inner.bottom().saturating_sub(1), height: 1, ..inner };
        f.render_widget(Paragraph::new(Span::styled("enter run it · e edit the file · ↑↓ choose · esc close", ui::muted(t))).centered(), hr);
        if let Some(a) = &v.args {
            let name = v.items.get(v.sel).and_then(|(_, x)| x.as_ref().ok()).map(|x| x.name.clone()).unwrap_or_default();
            let ih = (a.names.len() as u16 * 3 + 6).min(area.height);
            let pi = ui::popup(f, area, 72, ih, &format!("run {name}"), t);
            let pi = Rect { x: pi.x + 1, width: pi.width.saturating_sub(2), ..pi };
            let mut y = pi.y + 1;
            for (i, (n, inp)) in a.names.iter().zip(&a.inputs).enumerate() {
                if y + 3 > pi.bottom() {
                    break;
                }
                let r = ui::frame(f, Rect { y, height: 3, ..pi }, &format!("{{{{{n}}}}}"), None, i == a.field, t);
                super::view::draw_input(f, r, inp, "", i == a.field, t);
                y += 3;
            }
            let msg = if a.err.is_empty() { Line::styled("enter next / run · tab next · esc back", ui::muted(t)) } else { Line::styled(a.err.clone(), Style::default().fg(t.danger)) };
            f.render_widget(Paragraph::new(msg).centered(), Rect { y: pi.bottom().saturating_sub(1), height: 1, ..pi });
        }
        if let Some((p, inp, err)) = &v.edit {
            let pi = ui::popup(f, area, 110, area.height.saturating_sub(2), &format!("edit {}", p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()), t);
            let pi = Rect { x: pi.x + 1, width: pi.width.saturating_sub(2), ..pi };
            let r = Rect { height: pi.height.saturating_sub(1), ..pi };
            super::view::draw_input(f, r, inp, "", true, t);
            let msg = if err.is_empty() { Line::styled("ctrl+s save · esc cancel · {{name}} in a title, prompt or command is asked for when it runs", ui::muted(t)) } else { Line::styled(format!("✗ {err}"), Style::default().fg(t.danger)) };
            f.render_widget(Paragraph::new(msg).centered(), Rect { y: pi.bottom().saturating_sub(1), height: 1, ..pi });
        }
    }
}

/// A colour faded towards the terminal black (selected rows).
fn tint(c: ratatui::style::Color, amount: f32) -> ratatui::style::Color {
    match c {
        ratatui::style::Color::Rgb(..) => crate::theme::mix(ratatui::style::Color::Rgb(14, 14, 14), c, amount),
        _ => ratatui::style::Color::Reset,
    }
}

fn bold_fg(t: &crate::theme::Theme) -> Style {
    Style::default().fg(t.fg).add_modifier(Modifier::BOLD)
}

fn bold_accent(t: &crate::theme::Theme) -> Style {
    Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
}
