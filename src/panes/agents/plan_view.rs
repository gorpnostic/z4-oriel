//! The lead's plan, for you to approve before any worker starts (the lead form's "approve the plan first"): its
//! tasks as cards, checked by plan.rs, with what the check says next to them. x drops a task, e edits one (title,
//! goal, what it owns, its acceptance command, its worker), a starts the workers; c sends the plan back to the lead
//! with a note. The lead hears either as an event through `wait` (plan_approved / plan_rejected).

use super::input::Input;
use super::plan;
use super::{Agents, Mode};
use crate::pane::Cx;
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};
use serde_json::json;

/// The plan view.
pub struct PlanView {
    pub run: String,
    pub sel: usize,
    pub edit: Option<PlanEdit>,
    /// c: the note that sends the plan back
    pub note: Option<Input>,
}

/// e: one task of the plan, being edited.
pub struct PlanEdit {
    pub key: String,
    pub field: usize, // 0 title, 1 goal, 2 owns, 3 acceptance, 4 worker, 5 save
    pub title: Input,
    pub goal: Input,
    /// globs, comma separated
    pub owns: Input,
    pub acceptance: Input,
    /// index into the roster names
    pub worker: usize,
    pub err: String,
}

const FIELDS: usize = 6;

fn bold(c: Color) -> Style {
    Style::default().fg(c).add_modifier(Modifier::BOLD)
}

impl Agents {
    /// enter on the lead panel while its plan waits: the cards.
    pub(super) fn open_plan_review(&mut self, run: &str) {
        self.mode = Mode::PlanReview(PlanView { run: run.to_string(), sel: 0, edit: None, note: None });
    }

    /// What plan.rs says about the plan as it stands (dropped tasks left out): its notes, or what's wrong.
    fn plan_check(&self, run: &str) -> Result<plan::Checked, Vec<String>> {
        let Some(r) = self.run_ref(run) else { return Err(vec!["the run is gone".into()]) };
        let kept: Vec<plan::Item> = r.held.iter().filter(|i| !r.dropped.contains(&i.key)).cloned().collect();
        if kept.is_empty() {
            return Err(vec!["you dropped every task — c sends the plan back to the lead with a note instead".into()]);
        }
        let title = |k: &str| r.held.iter().find(|i| i.key == k).map(|i| i.title.clone()).unwrap_or_else(|| k.to_string());
        let waits: Vec<String> = kept.iter().flat_map(|it| it.depends_on.iter().filter(|d| r.dropped.contains(d)).map(move |d| (it, d))).map(|(it, d)| format!("{:?} waits for {:?}, which you dropped — drop it too (x), or keep that one", it.title, title(d))).collect();
        if !waits.is_empty() {
            return Err(waits);
        }
        plan::check(kept, &self.plan_existing(run), &self.plan_worker_names())
    }

    /// a: the plan as it stands goes to work (if it passes the check), and the lead hears what you changed.
    fn approve_plan(&mut self, run: &str, cx: &mut Cx) {
        let checked = match self.plan_check(run) {
            Ok(c) => c,
            Err(e) => {
                cx.notify(format!("not yet: {}", e.first().cloned().unwrap_or_default()));
                return;
            }
        };
        let Some(r) = self.run_ref(run).cloned() else { return };
        let titles = |keys: &[String]| -> Vec<String> { keys.iter().filter_map(|k| r.held.iter().find(|i| i.key == *k)).map(|i| i.title.clone()).collect() };
        let (dropped, edited) = (titles(&r.dropped), titles(&r.edited.iter().filter(|k| !r.dropped.contains(k)).cloned().collect::<Vec<_>>()));
        if let Some(x) = self.run_mut(run) {
            x.held.clear();
            x.held_notes.clear();
            x.dropped.clear();
            x.edited.clear();
        }
        let n = checked.items.len();
        let (serial, notes) = (checked.serial, checked.notes.clone());
        // a run that went to review while its plan waited (its lead called done) takes the tasks in again
        self.revive(run);
        let out = self.spawn_plan(&r, checked.items, cx);
        let reply = self.plan_reply(&out, serial, &notes);
        let mut card = json!({"event": "plan_approved", "tasks": reply["tasks"], "action": "the tasks are queued and start as worker slots free up: wait for their results"});
        if !dropped.is_empty() {
            card["dropped"] = json!(dropped);
        }
        if !edited.is_empty() {
            card["edited_by_the_user"] = json!(edited);
        }
        if !notes.is_empty() {
            card["notes"] = json!(notes);
        }
        let mut line = format!("you approved the plan: {n} task{}", if n == 1 { "" } else { "s" });
        if !dropped.is_empty() {
            line.push_str(&format!(" (dropped {})", dropped.join(", ")));
        }
        if let Some(x) = self.run_mut(run) {
            x.log(&line);
        }
        self.run_event(run, card);
        self.mode = Mode::Board;
        self.lead_focus = true;
        self.save();
        cx.notify(format!("plan approved: {n} task{} queued", if n == 1 { "" } else { "s" }));
    }

    /// c: the plan goes back to the lead with your note; it plans again.
    fn reject_plan(&mut self, run: &str, note: &str, cx: &mut Cx) {
        let note = note.trim().to_string();
        if note.is_empty() {
            return;
        }
        if let Some(x) = self.run_mut(run) {
            x.held.clear();
            x.held_notes.clear();
            x.dropped.clear();
            x.edited.clear();
            x.log(&format!("you sent the plan back: {note}"));
        }
        self.run_event(run, json!({"event": "plan_rejected", "note": note, "action": "nothing was started. Submit a new plan that takes the note into account"}));
        self.mode = Mode::Board;
        self.lead_focus = true;
        self.save();
        cx.notify("the plan went back to the lead with your note");
    }

    fn plan_edit(&self, it: &plan::Item) -> PlanEdit {
        let names = self.plan_worker_names();
        PlanEdit {
            key: it.key.clone(),
            field: 1,
            title: Input::new(&it.title, false),
            goal: Input::new(&it.goal, true),
            owns: Input::new(&it.owns.join(", "), false),
            acceptance: Input::new(&it.acceptance, false),
            worker: names.iter().position(|n| n.eq_ignore_ascii_case(&it.worker)).unwrap_or(0),
            err: String::new(),
        }
    }

    /// Save an edited card into the held plan (the check runs again on what you see).
    fn save_plan_edit(&mut self, run: &str, e: &mut PlanEdit) -> bool {
        let (title, goal) = (e.title.text.trim().to_string(), e.goal.text.trim().to_string());
        if title.is_empty() || goal.is_empty() {
            e.err = "it needs a title and a goal".into();
            e.field = if title.is_empty() { 0 } else { 1 };
            return false;
        }
        let owns: Vec<String> = e.owns.text.split(',').map(|s| s.trim().replace('\\', "/")).filter(|s| !s.is_empty()).collect();
        if owns.is_empty() {
            e.err = "say which files it may edit (globs like src/cli/**)".into();
            e.field = 2;
            return false;
        }
        let worker = self.plan_worker_names().get(e.worker).cloned().unwrap_or_default();
        let Some(r) = self.run_mut(run) else { return false };
        let Some(it) = r.held.iter_mut().find(|i| i.key == e.key) else { return false };
        let before = it.clone();
        it.title = title;
        it.goal = goal;
        it.owns = owns;
        it.acceptance = e.acceptance.text.trim().to_string();
        if !worker.is_empty() {
            it.worker = worker;
        }
        if *it != before && !r.edited.contains(&e.key) {
            r.edited.push(e.key.clone());
        }
        self.save();
        true
    }

    pub(super) fn plan_review_key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        let Mode::PlanReview(mut v) = std::mem::replace(&mut self.mode, Mode::Board) else { return false };
        let Some(r) = self.run_ref(&v.run).cloned() else { return true };
        if r.held.is_empty() {
            return true; // approved or sent back meanwhile
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        // the note that sends it back
        if let Some(mut inp) = v.note.take() {
            match k.code {
                KeyCode::Esc => {}
                KeyCode::Enter if !k.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) => {
                    if !inp.text.trim().is_empty() {
                        let run = v.run.clone();
                        self.mode = Mode::PlanReview(v);
                        self.reject_plan(&run, &inp.text, cx);
                        return true;
                    }
                    v.note = Some(inp);
                }
                _ => {
                    inp.key(k);
                    v.note = Some(inp);
                }
            }
            self.mode = Mode::PlanReview(v);
            return true;
        }
        // editing a card
        if let Some(mut e) = v.edit.take() {
            e.err.clear();
            let names = self.plan_worker_names().len().max(1);
            match k.code {
                KeyCode::Esc => {}
                KeyCode::Char('s') if ctrl => {
                    if !self.save_plan_edit(&v.run.clone(), &mut e) {
                        v.edit = Some(e);
                    }
                }
                KeyCode::Tab => {
                    e.field = (e.field + 1) % FIELDS;
                    v.edit = Some(e);
                }
                KeyCode::BackTab => {
                    e.field = (e.field + FIELDS - 1) % FIELDS;
                    v.edit = Some(e);
                }
                _ => {
                    let used = match e.field {
                        0 => e.title.key(k),
                        1 => e.goal.key(k),
                        2 => e.owns.key(k),
                        3 => e.acceptance.key(k),
                        _ => false,
                    };
                    if !used {
                        match k.code {
                            KeyCode::Enter if e.field == 5 => {
                                if !self.save_plan_edit(&v.run.clone(), &mut e) {
                                    v.edit = Some(e);
                                }
                                self.mode = Mode::PlanReview(v);
                                return true;
                            }
                            KeyCode::Left if e.field == 4 => e.worker = (e.worker + names - 1) % names,
                            KeyCode::Right | KeyCode::Char(' ') if e.field == 4 => e.worker = (e.worker + 1) % names,
                            KeyCode::Enter | KeyCode::Down => e.field = (e.field + 1).min(FIELDS - 1),
                            KeyCode::Up => e.field = e.field.saturating_sub(1),
                            _ => {}
                        }
                    }
                    v.edit = Some(e);
                }
            }
            self.mode = Mode::PlanReview(v);
            return true;
        }
        let n = r.held.len();
        v.sel = v.sel.min(n.saturating_sub(1));
        let key = r.held[v.sel].key.clone();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return true,
            KeyCode::Up | KeyCode::Char('k') => v.sel = v.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => v.sel = (v.sel + 1).min(n - 1),
            KeyCode::Char('x') | KeyCode::Delete => {
                if let Some(x) = self.run_mut(&v.run) {
                    if let Some(i) = x.dropped.iter().position(|d| *d == key) {
                        x.dropped.remove(i);
                    } else {
                        x.dropped.push(key);
                    }
                }
                self.save();
            }
            KeyCode::Char('e') | KeyCode::Enter => v.edit = Some(self.plan_edit(&r.held[v.sel])),
            KeyCode::Char('c') => v.note = Some(Input::new("", true)),
            KeyCode::Char('a') => {
                let run = v.run.clone();
                self.mode = Mode::PlanReview(v);
                self.approve_plan(&run, cx);
                return true;
            }
            _ => {}
        }
        self.mode = Mode::PlanReview(v);
        true
    }

    /// Text pasted while the plan view is open: into the field or the note being typed.
    pub(super) fn plan_review_paste(&mut self, text: &str) {
        let Mode::PlanReview(v) = &mut self.mode else { return };
        if let Some(n) = &mut v.note {
            n.insert(text);
        } else if let Some(e) = &mut v.edit {
            match e.field {
                0 => e.title.insert(text),
                1 => e.goal.insert(text),
                2 => e.owns.insert(text),
                3 => e.acceptance.insert(text.trim()),
                _ => {}
            }
        }
    }

    // ------------------------------------------------------------------ drawing

    pub(super) fn draw_plan_review(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        let Mode::PlanReview(v) = &self.mode else { return };
        let (run_id, sel, editing, noting) = (v.run.clone(), v.sel, v.edit.is_some(), v.note.is_some());
        let Some(r) = self.run_ref(&run_id).cloned().filter(|r| !r.held.is_empty()) else {
            // approved or sent back meanwhile, or the run is gone
            self.mode = Mode::Board;
            return;
        };
        let hints: &[(&str, &str)] = if noting {
            &[("enter", "send the plan back with this"), ("esc", "cancel")]
        } else if editing {
            &[("tab", "next field"), ("ctrl+s", "save"), ("esc", "cancel")]
        } else {
            &[("↑↓", "choose"), ("a", "approve & start"), ("e", "edit"), ("x", "drop / keep"), ("c", "send it back with a note"), ("esc", "board")]
        };
        let body = ui::hint_line(f, area, hints, t);
        let body = Rect { x: body.x + 1, width: body.width.saturating_sub(2), ..body };
        let w = body.width as usize;
        let kept = r.held.iter().filter(|i| !r.dropped.contains(&i.key)).count();
        let head = Line::from(vec![
            Span::styled("⚑ the lead's plan  ", bold(t.accent)),
            Span::styled(format!("{kept} of {} task{} · nothing starts until you approve it", r.held.len(), if r.held.len() == 1 { "" } else { "s" }), Style::default().fg(t.fg)),
            Span::styled(format!("   {}", ui::fit(r.goal.lines().next().unwrap_or(""), w.saturating_sub(60))), ui::muted(t)),
        ]);
        f.render_widget(Paragraph::new(head), Rect { height: 1, ..body });
        // what plan.rs says: its notes (hotspots, one at a time), or why it can't start as it is
        let check = self.plan_check(&run_id);
        let (msgs, bad): (Vec<String>, bool) = match &check {
            Ok(c) => (c.notes.clone(), false),
            Err(e) => (e.clone(), true),
        };
        let mut y = body.y + 2;
        for m in msgs.iter().take(4) {
            if y >= body.bottom() {
                break;
            }
            let st = if bad { Style::default().fg(t.danger) } else { Style::default().fg(t.shine) };
            f.render_widget(Paragraph::new(Line::styled(ui::fit(&format!("{} {m}", if bad { "✗" } else { "·" }), w), st)), Rect { y, height: 1, ..body });
            y += 1;
        }
        if !msgs.is_empty() {
            y += 1;
        }
        // the cards, scrolled so the chosen one shows
        let card_h = 5u16;
        let room = body.bottom().saturating_sub(y).saturating_sub(if noting { 3 } else { 0 });
        let fit = (room / card_h).max(1) as usize;
        let start = sel.saturating_sub(fit.saturating_sub(1));
        let roster = self.roster();
        for (i, it) in r.held.iter().enumerate().skip(start).take(fit) {
            let cr = Rect { y: y + ((i - start) as u16) * card_h, height: card_h.min(body.bottom().saturating_sub(y + ((i - start) as u16) * card_h)), ..body };
            if cr.height < 3 {
                break;
            }
            let on = i == sel;
            let dropped = r.dropped.contains(&it.key);
            let edited = r.edited.contains(&it.key);
            let w_entry = roster.iter().find(|x| x.name.eq_ignore_ascii_case(&it.worker));
            let who = match w_entry {
                Some(x) => format!("{} ({} {} · {})", x.name, x.agent, if x.model.is_empty() { "default" } else { &x.model }, x.tier),
                None => it.worker.clone(),
            };
            let border = if dropped { t.frame } else if on { t.accent } else { t.frame };
            let title = format!(" {} · {} ", it.key, it.title);
            let mut right = format!(" {who} · {}{} ", if it.size.is_empty() { "M" } else { &it.size }, match it.priority {
                2 => " · urgent",
                1 => " · high",
                -1 => " · low",
                _ => "",
            });
            if edited {
                right = format!(" edited ·{right}");
            }
            if dropped {
                right = " ✗ dropped · x keeps it ".into();
            }
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(if on { bold(border) } else { Style::default().fg(border) })
                .title(Line::from(Span::styled(ui::fit(&title, (cr.width as usize).saturating_sub(right.chars().count() + 6)), if dropped { ui::muted(t) } else { bold(if on { t.accent } else { t.fg }) })))
                .title(Line::from(Span::styled(right, if dropped { Style::default().fg(t.danger) } else if edited { Style::default().fg(t.shine) } else { ui::muted(t) })).right_aligned());
            let inner = block.inner(cr);
            f.render_widget(block, cr);
            let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
            let iw = inner.width as usize;
            let dim = |s: Style| if dropped { ui::muted(t) } else { s };
            let goal = it.goal.split_whitespace().collect::<Vec<_>>().join(" ");
            let mut lines = vec![Line::styled(ui::fit(&goal, iw), dim(Style::default().fg(t.fg)))];
            let mut facts = format!("owns {}", if it.owns.is_empty() { "—".into() } else { it.owns.join(", ") });
            if !it.depends_on.is_empty() {
                facts.push_str(&format!(" · after {}", it.depends_on.join(", ")));
            }
            facts.push_str(&format!(" · accept: {}", if it.acceptance.is_empty() { "—" } else { &it.acceptance }));
            lines.push(Line::styled(ui::fit(&facts, iw), dim(ui::muted(t))));
            // what the check says about this one
            let quoted = format!("{:?}", it.title);
            if let Some(m) = msgs.iter().find(|m| m.contains(&quoted)).filter(|_| !dropped) {
                lines.push(Line::styled(ui::fit(&format!("{} {m}", if bad { "✗" } else { "·" }), iw), if bad { Style::default().fg(t.danger) } else { Style::default().fg(t.shine) }));
            }
            f.render_widget(Paragraph::new(lines), inner);
        }
        if r.held.len() > start + fit {
            let rr = Rect { y: body.bottom().saturating_sub(1), height: 1, ..body };
            f.render_widget(Paragraph::new(Span::styled(format!("↓ {} more", r.held.len() - start - fit), ui::muted(t))).right_aligned(), rr);
        }
        let Mode::PlanReview(v) = &self.mode else { return };
        if let Some(n) = &v.note {
            let br = Rect { y: body.bottom().saturating_sub(3), height: 3.min(body.height), ..body };
            f.render_widget(ratatui::widgets::Clear, br);
            let bi = ui::frame(f, br, "send the plan back to the lead · what should change?", None, true, t);
            super::view::draw_input(f, bi, n, "e.g. \"one task is enough\", \"leave the CLI alone\"", true, t);
        }
        if let Some(e) = &v.edit {
            self.draw_plan_edit(f, area, e, cx);
        }
    }

    fn draw_plan_edit(&self, f: &mut Frame, area: Rect, e: &PlanEdit, cx: &Cx) {
        let t = cx.theme;
        let inner = ui::popup(f, area, 90, 26, &format!("edit task {}", e.key), t);
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
        let bottom = inner.bottom();
        let mut y = inner.y + 1;
        let field = |f: &mut Frame, label: &str, h: u16, idx: usize, y: &mut u16| -> Rect {
            let r = Rect { y: *y, height: h.min(bottom.saturating_sub(*y)), ..inner };
            *y += h;
            ui::frame(f, r, label, None, e.field == idx, t)
        };
        let r = field(f, "title", 3, 0, &mut y);
        super::view::draw_input(f, r, &e.title, "short and imperative", e.field == 0, t);
        let r = field(f, "goal (what the worker is told)", 8, 1, &mut y);
        super::view::draw_input(f, r, &e.goal, "complete instructions a worker with no other context can follow", e.field == 1, t);
        let r = field(f, "owns (the files it may edit: globs, comma separated)", 3, 2, &mut y);
        super::view::draw_input(f, r, &e.owns, "e.g. src/cli/**, docs/cli.md", e.field == 2, t);
        let r = field(f, "acceptance (a command that proves it works; runs in the merge gate)", 3, 3, &mut y);
        super::view::draw_input(f, r, &e.acceptance, "none", e.field == 3, t);
        let r = field(f, "worker", 3, 4, &mut y);
        let names = self.plan_worker_names();
        let mut spans = vec![];
        for (i, n) in names.iter().enumerate() {
            let st = if i == e.worker { Style::default().fg(Color::Black).bg(t.accent).add_modifier(Modifier::BOLD) } else { Style::default().fg(t.fg) };
            spans.push(Span::styled(format!(" {n} "), st));
            spans.push(Span::raw("  "));
        }
        if e.field == 4 {
            spans.push(Span::styled("←/→ choose", ui::muted(t)));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), r);
        if y + 1 < bottom {
            let btn = if e.field == 5 { Span::styled(" save ", Style::default().fg(Color::Black).bg(t.accent).add_modifier(Modifier::BOLD)) } else { Span::styled("[save]", bold(t.accent)) };
            let mut spans = vec![btn];
            if !e.err.is_empty() {
                spans.push(Span::styled(format!("   {}", e.err), Style::default().fg(t.danger)));
            }
            f.render_widget(Paragraph::new(Line::from(spans)).centered(), Rect { y: y + 1, height: 1, ..inner });
        }
    }
}
