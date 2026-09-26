//! Drawing for the agents app: the board, its cards, the diff view, the popups (new task, comment, plan,
//! confirm, repo picker) and the sidebar section.

use super::git::Kind;
use super::store::{Status, Task};
use super::{Agents, Hit, KINDS, Mode, Pending};
use crate::pane::Cx;
use crate::theme::{Theme, mix};
use crate::ui;
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};
use unicode_width::UnicodeWidthStr;

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const COLS: [&str; 4] = ["TODO", "RUNNING", "REVIEW", "DONE"];
const CARD_H: u16 = 6;

fn bold(c: Color) -> Style {
    Style::default().fg(c).add_modifier(Modifier::BOLD)
}

/// A colour faded towards the terminal black, for tinted backgrounds (diff lines, selected rows).
fn tint(c: Color, amount: f32) -> Color {
    match c {
        Color::Rgb(..) => mix(Color::Rgb(14, 14, 14), c, amount),
        _ => Color::Reset,
    }
}

fn col_color(c: usize, t: &Theme) -> Color {
    match c {
        0 => t.fg,
        1 => t.accent,
        2 => t.shine,
        _ => t.good,
    }
}

/// "45s", "4m", "1h 12m", "3d"
pub fn dur(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d", s / 86400),
    }
}

fn money(usd: f64) -> String {
    if usd <= 0.0 {
        String::new()
    } else if usd < 0.01 {
        "<$0.01".into()
    } else {
        format!("${usd:.2}")
    }
}

fn tokens(n: u64) -> String {
    match n {
        0 => String::new(),
        1..=999 => format!("{n} tok"),
        1000..=999_999 => format!("{}k tok", n / 1000),
        _ => format!("{:.1}M tok", n as f64 / 1e6),
    }
}

/// Left and right text on one row of width `w`, the right side winning when it doesn't fit.
fn spread<'a>(left: Vec<Span<'a>>, right: Vec<Span<'a>>, w: usize) -> Line<'a> {
    let rw: usize = right.iter().map(|s| s.content.width()).sum();
    let room = w.saturating_sub(rw + if rw > 0 { 1 } else { 0 });
    let mut out = vec![];
    let mut used = 0;
    for s in left {
        if used >= room {
            break;
        }
        let text = ui::fit(&s.content, room - used);
        used += text.width();
        out.push(Span::styled(text, s.style));
    }
    out.push(Span::raw(" ".repeat(w.saturating_sub(used + rw))));
    out.extend(right);
    Line::from(out)
}

impl Agents {
    pub(super) fn draw(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        self.hits.clear();
        let t = cx.theme;
        if area.width < 20 || area.height < 8 {
            f.render_widget(Paragraph::new(Span::styled("agents: make the pane bigger", ui::muted(t))), area);
            return;
        }
        match self.mode {
            Mode::Diff(_) => self.draw_diff(f, area, cx),
            Mode::PlanReview(_) => self.draw_plan_review(f, area, cx),
            Mode::Watch(_) => self.draw_watch(f, area, cx),
            Mode::Log(_) => self.draw_log(f, area, cx),
            _ => {
                let hints = self.board_hints();
                let body = ui::hint_line(f, area, &fit_hints(&hints, area.width as usize), t);
                let body = Rect { x: body.x + 1, width: body.width.saturating_sub(2), ..body };
                self.draw_header(f, Rect { height: 1, ..body }, cx);
                let mut cols = Rect { y: body.y + 2, height: body.height.saturating_sub(2), ..body };
                // the lead run sits above the columns
                if self.current_run().is_some() && cols.height > super::lead_view::LEAD_H + 8 {
                    self.draw_lead_panel(f, Rect { height: super::lead_view::LEAD_H, ..cols }, cx);
                    cols = Rect { y: cols.y + super::lead_view::LEAD_H + 1, height: cols.height - super::lead_view::LEAD_H - 1, ..cols };
                } else if self.current_run().is_none() {
                    self.lead_focus = false;
                }
                self.draw_board(f, cols, cx);
            }
        }
        // popups on top
        match &self.mode {
            Mode::Form(_) => self.draw_form(f, area, cx),
            Mode::Confirm(_) => self.draw_confirm(f, area, cx),
            Mode::Comment(..) | Mode::Plan(_) => self.draw_prompt_box(f, area, cx),
            Mode::Repo(_) => self.draw_picker(f, area, cx),
            Mode::LeadForm(_) => self.draw_lead_form(f, area, cx),
            Mode::Roster(_) => self.draw_roster(f, area, cx),
            Mode::Batch(_) => self.draw_batch(f, area, cx),
            Mode::Dirty(_) => self.draw_dirty(f, area, cx),
            Mode::Try(_) => self.draw_try(f, area, cx),
            Mode::SaveTemplate(_) => self.draw_save_template(f, area, cx),
            Mode::Templates(_) => self.draw_templates(f, area, cx),
            _ => {}
        }
        if self.prompt_pick.is_some() {
            self.draw_prompt_pick(f, area, cx);
        }
    }

    /// Uncommitted changes in your checkout: commit them, take them along, or go without.
    fn draw_dirty(&self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        let Mode::Dirty(d) = &self.mode else { return };
        let repo = self.repo.as_ref().map(|r| r.name.clone()).unwrap_or_else(|| "the repo".into());
        let files = super::git::porcelain_files(&d.status);
        let shown: Vec<String> = files.iter().take(4).cloned().collect();
        let more = if files.len() > 4 { format!(" and {} more", files.len() - 4) } else { String::new() };
        let what_files = if files.is_empty() { String::new() } else { format!("{}{more}", shown.join(", ")) };
        let key = |k: &str, c: Color| Span::styled(format!(" {k} "), Style::default().fg(Color::Black).bg(c).add_modifier(Modifier::BOLD));
        let (title, body, keys): (&str, Vec<String>, Vec<Line>) = match &d.then {
            super::DirtyThen::Merge(_) | super::DirtyThen::MergeRun(_) => (
                "uncommitted changes",
                vec![format!("{repo} has changes that aren't committed, so nothing was merged."), "Commit them (git add -A) and the merge goes ahead right after.".into()],
                vec![Line::from(vec![key("c", t.accent), Span::styled("  commit your changes and merge", Style::default().fg(t.fg))])],
            ),
            then => {
                let what = match then {
                    super::DirtyThen::Task(id) => format!("\"{}\"", self.task(id).map(|x| x.title.clone()).unwrap_or_default()),
                    _ => "the run".into(),
                };
                (
                    "uncommitted changes",
                    vec![
                        format!("{} file{} in {repo} {} committed{}", files.len(), if files.len() == 1 { "" } else { "s" }, if files.len() == 1 { "isn't" } else { "aren't" }, if what_files.is_empty() { String::new() } else { format!(": {what_files}") }),
                        "A chat's edits? A new branch starts from your last commit, so".to_string(),
                        format!("{what} wouldn't see them."),
                    ],
                    vec![
                        Line::from(vec![key("c", t.accent), Span::styled("  commit them first (git add -A), then start", Style::default().fg(t.fg))]),
                        Line::from(vec![key("i", t.shine), Span::styled("  take them along: start from a snapshot (your checkout isn't touched)", Style::default().fg(t.fg))]),
                        Line::from(vec![key("enter", t.frame), Span::styled("  start without them", Style::default().fg(t.fg))]),
                    ],
                )
            }
        };
        let h = (body.len() + keys.len()) as u16 + 7;
        let inner = ui::popup(f, area, 80, h, title, t);
        let inner = Rect { x: inner.x + 2, width: inner.width.saturating_sub(4), y: inner.y + 1, height: inner.height.saturating_sub(1) };
        let mut lines: Vec<Line> = body.iter().enumerate().map(|(i, s)| Line::styled(ui::fit(s, inner.width as usize), if i == 0 { bold(t.fg) } else { ui::muted(t) })).collect();
        lines.push(Line::raw(""));
        lines.extend(keys);
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![Span::styled("esc", bold(t.fg)), Span::styled(" cancel", ui::muted(t))]));
        f.render_widget(Paragraph::new(lines), inner);
    }

    /// T: which checkout, and what to start in it.
    fn draw_try(&self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        let Mode::Try(v) = &self.mode else { return };
        let setup = !v.setup.trim().is_empty();
        let inner = ui::popup(f, area, 80, if setup { 12 } else { 11 }, &format!("{}try it", ui::lead("term")), t);
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
        let w = inner.width as usize;
        // (the port first: a long path gets cut)
        let port = if v.port > 0 { format!("ORIEL_PORT {} · ", v.port) } else { String::new() };
        let mut head = vec![Line::styled(ui::fit(&v.title, w), bold(t.fg)), Line::styled(ui::fit(&format!("{port}a terminal in {}", v.dir.display()), w), ui::muted(t))];
        if setup {
            head.push(Line::styled(ui::fit(&format!("runs the repo's setup first: {}", v.setup.trim()), w), Style::default().fg(t.shine)));
        }
        let hh = head.len() as u16;
        f.render_widget(Paragraph::new(head), Rect { height: hh, ..inner });
        let r = Rect { y: inner.y + hh + 1, height: 3, ..inner };
        let ri = ui::frame(f, r, "run there (remembered for this repo · $ORIEL_PORT is its port)", None, true, t);
        draw_input(f, ri, &v.input, "nothing — just a shell · or e.g. pnpm dev --port $ORIEL_PORT (.oriel/project.toml `run`)", true, t);
        let hr = Rect { y: inner.bottom().saturating_sub(1), height: 1, ..inner };
        f.render_widget(Paragraph::new(Line::from(vec![Span::styled("enter", bold(t.fg)), Span::styled(" open", ui::muted(t)), Span::styled(" · ", ui::muted(t)), Span::styled("esc", bold(t.fg)), Span::styled(" cancel", ui::muted(t))])).centered(), hr);
    }

    /// ctrl+t: your saved prompts.
    fn draw_prompt_pick(&self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        let Some(p) = &self.prompt_pick else { return };
        let rows = p.rows();
        let h = (rows.len() as u16 + 9).clamp(11, 24);
        let inner = ui::popup(f, area, 76, h, &format!("{}saved prompts", ui::lead("new")), t);
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
        let w = inner.width as usize;
        let r = Rect { height: 3, ..inner };
        let ri = ui::frame(f, r, "", None, true, t);
        draw_input(f, ri, &p.filter, "type to filter · or a name to save the field under", true, t);
        let mut y = inner.y + 4;
        if p.items.is_empty() {
            f.render_widget(Paragraph::new(Span::styled(ui::fit(&format!("none yet — they live in {}", super::prompts::path().display()), w), ui::muted(t))), Rect { y, height: 1, ..inner });
            y += 2;
        }
        let room = inner.bottom().saturating_sub(1).saturating_sub(y) as usize;
        let start = p.sel.saturating_sub(room.saturating_sub(1));
        for (i, row) in rows.iter().enumerate().skip(start).take(room) {
            let on = i == p.sel;
            let bg = if on { Style::default().bg(tint(t.accent, 0.18)) } else { Style::default() };
            let line = match row {
                super::PickRow::Prompt(k) => {
                    let pr = &p.items[*k];
                    spread(vec![Span::styled(format!("{} {}", if on { "›" } else { " " }, pr.name), if on { bold(t.accent) } else { bold(t.fg) }), Span::styled(format!("  {}", pr.text.replace('\n', " ")), ui::muted(t))], vec![], w)
                }
                super::PickRow::Save(name) => spread(vec![Span::styled(format!("{} + save what's in the field as \"{name}\"", if on { "›" } else { " " }), if on { bold(t.shine) } else { Style::default().fg(t.shine) })], vec![], w),
            };
            f.render_widget(Paragraph::new(line.style(bg)), Rect { y, height: 1, ..inner });
            y += 1;
        }
        let hr = Rect { y: inner.bottom().saturating_sub(1), height: 1, ..inner };
        f.render_widget(Paragraph::new(Span::styled("enter insert · ↑↓ choose · del delete · esc close · chat: /p <name>", ui::muted(t))).centered(), hr);
    }

    /// Run the marked tasks together: which, how, and where their work ends up.
    fn draw_batch(&self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        let n = self.marked.len();
        let Mode::Batch(v) = &self.mode else { return };
        let inner = ui::popup(f, area, 80, (n as u16 + 16).min(30), &format!("{}run {n} task{} together", ui::lead("robot"), if n == 1 { "" } else { "s" }), t);
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
        let mut lines = vec![];
        for (i, id) in self.marked.iter().enumerate() {
            let Some(task) = self.task(id) else { continue };
            let model = if task.model.is_empty() { "default" } else { &task.model };
            let pri = match task.priority {
                2 => " ‼ urgent",
                1 => " ↑ high",
                -1 => " ↓ low",
                _ => "",
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{:>2}. ", i + 1), bold(t.accent)),
                Span::styled(ui::fit(&task.title, 40), Style::default().fg(t.fg).add_modifier(Modifier::BOLD)),
                Span::styled(format!("  {} · {model}{pri}", task.agent), ui::muted(t)),
            ]));
        }
        lines.push(Line::raw(""));
        let par = self.lead_cfg.max_parallel.clamp(1, 5);
        lines.push(Line::from(vec![Span::styled(" p ", Style::default().fg(Color::Black).bg(t.accent).add_modifier(Modifier::BOLD)), Span::styled(format!("  all at once: up to {par} at a time, highest priority first"), Style::default().fg(t.fg))]));
        lines.push(Line::from(vec![Span::styled(" s ", Style::default().fg(Color::Black).bg(t.shine).add_modifier(Modifier::BOLD)), Span::styled("  one after another, in the order you marked them (after-links first)", Style::default().fg(t.fg))]));
        lines.push(Line::raw(""));
        let top = (inner.y + lines.len() as u16).min(inner.bottom());
        f.render_widget(Paragraph::new(lines), Rect { height: top - inner.y, ..inner });
        // the gate row: what every merged result must pass (g edits it)
        let (prefill, why) = self.gate_prefill();
        let gate_r = Rect { y: top, height: 3.min(inner.bottom().saturating_sub(top)), ..inner };
        let note = (!v.gate.text.is_empty() && v.gate.text == prefill).then_some(why.as_str());
        let ri = ui::frame(f, gate_r, if v.editing { "gate · enter done" } else { "gate · g edits" }, note, v.editing, t);
        let ph = if prefill.is_empty() { why.clone() } else { "none — merges get only a conflict check".to_string() };
        draw_input(f, ri, &v.gate, &ph, v.editing, t);
        // what happens, with what actually runs
        let budget = self.lead_cfg.run_budget_usd;
        let gate = v.gate.text.trim();
        let w = inner.width as usize;
        let mut tail: Vec<Line> = [
            "Each runs in its own worktree with its own AI and model. Finished work is".to_string(),
            if gate.is_empty() { "merged one at a time into a new branch after a conflict check only:".to_string() } else { "merged one at a time into a new branch after a conflict check and".to_string() },
            if gate.is_empty() { "no gate, so nothing builds or tests it (g sets one).".to_string() } else { format!("`{}` on the merged result (a failure goes back to it).", ui::fit(gate, w.saturating_sub(40))) },
            format!("Budget ${budget:.2} for the run. When they're all in: d reviews, m merges."),
        ]
        .into_iter()
        .map(|l| Line::from(Span::styled(ui::fit(&l, w), ui::muted(t))))
        .collect();
        tail.push(Line::from(Span::styled("esc cancels", ui::muted(t))));
        let ty = (top + 4).min(inner.bottom());
        f.render_widget(Paragraph::new(tail), Rect { y: ty, height: inner.bottom() - ty, ..inner });
    }

    pub(super) fn board_hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.repo.is_none() {
            return vec![("o", "open a repo")];
        }
        if !self.marked.is_empty() {
            return vec![("space", "mark more"), ("enter", "run the marked together"), ("esc", "unmark")];
        }
        if self.lead_focus {
            if let Some(r) = self.current_run() {
                if r.state == super::store::RunState::Stopped && r.branch.is_empty() {
                    // it never started
                    return vec![("r", "retry with the same goal"), ("x", "discard"), ("↓", "cards")];
                }
                let mut h = if r.held.is_empty() { vec![("enter", "lead transcript"), ("w", "watch")] } else { vec![("enter", "review the lead's plan"), ("w", "watch")] };
                if r.state.active() {
                    h.extend([("s", "stop run"), ("d", "diff so far")]);
                } else {
                    if r.state == super::store::RunState::Stopped {
                        h.push(("r", "resume"));
                    }
                    if self.feedback_block(r).is_none() {
                        h.push(("c", "feedback to the lead"));
                    }
                    h.extend([("d", "review"), ("V", "second opinion"), ("T", "try it"), ("m", "merge into your branch"), ("x", "discard run")]);
                }
                if r.manual || !r.state.active() {
                    h.push(("S", "save as template"));
                }
                h.extend([("↓", "cards"), ("R", "roster")]);
                return h;
            }
        }
        let sel = self.selected().and_then(|id| self.task(&id).cloned());
        let mut h = vec![("n", "new task"), ("L", "lead run")];
        let headless = sel.as_ref().is_some_and(|t| t.headless());
        match sel.as_ref().map(|t| t.status) {
            Some(Status::Todo) if headless => h.extend([("enter", "transcript"), ("x", "discard")]),
            Some(Status::Todo) => h.extend([("enter", "start"), ("space", "mark"), ("e", "edit"), ("x", "delete")]),
            Some(Status::Running | Status::Blocked) if headless => h.extend([("enter", "live transcript"), ("t", "take over"), ("d", "diff"), ("x", "discard")]),
            Some(Status::Running | Status::Blocked) => h.extend([("enter", "open agent"), ("d", "diff"), ("c", "comment"), ("m", "merge"), ("x", "discard")]),
            Some(Status::Review) if headless => h.extend([("enter", "transcript"), ("d", "diff"), ("V", "second opinion"), ("T", "try it"), ("m", "merge"), ("c", "follow-up"), ("t", "take over"), ("x", "discard")]),
            Some(Status::Review) => h.extend([("d", "diff"), ("V", "second opinion"), ("T", "try it"), ("m", "merge"), ("c", "comment"), ("x", "discard"), ("r", "retry")]),
            Some(Status::Done) if sel.as_ref().map(|t| t.outcome != "merged" && t.run.is_empty()).unwrap_or(false) => h.push(("r", "retry")),
            _ => {}
        }
        if self.deleted.as_ref().is_some_and(|(_, at)| at.elapsed() < super::UNDO_FOR) {
            h.insert(0, ("u", "undo delete"));
        }
        if self.current_run().is_some() {
            h.push(("w", "watch run"));
        }
        // planning only reads the repo: fine next to a run
        if self.installed("claude").is_some() {
            h.push(("P", "plan"));
        }
        h.extend([("W", "templates"), ("R", "roster"), ("o", "repo"), ("←→↑↓", "move")]);
        h
    }

    fn draw_header(&self, f: &mut Frame, r: Rect, cx: &Cx) {
        let t = cx.theme;
        let nerd = ui::NERD.load(std::sync::atomic::Ordering::Relaxed);
        let mut left = vec![];
        match &self.repo {
            Some(repo) => {
                left.push(Span::styled(format!("{}{}", ui::lead("files"), repo.name), bold(t.fg)));
                left.push(Span::styled(format!("  {}{}", if nerd { "\u{E0A0} " } else { "on " }, repo.branch), ui::accent(t)));
                left.push(Span::styled(format!("   {}", repo.root.display()), ui::muted(t)));
            }
            None if self.repo_loading => left.push(Span::styled("looking for a git repo…", ui::muted(t))),
            None => left.push(Span::styled("no repo open", ui::muted(t))),
        }
        let key = self.repo_key();
        let mine: Vec<&Task> = self.store.tasks.iter().filter(|x| x.repo == key).collect();
        let n = |s: Status| mine.iter().filter(|x| x.status == s).count();
        let mut right: Vec<Span> = vec![];
        let sep = |right: &mut Vec<Span>| {
            if !right.is_empty() {
                right.push(Span::styled(" · ", ui::muted(t)));
            }
        };
        if n(Status::Running) > 0 {
            sep(&mut right);
            right.push(Span::styled(format!("{} running", n(Status::Running)), ui::accent(t)));
        }
        if n(Status::Blocked) > 0 {
            sep(&mut right);
            right.push(Span::styled(format!("{} blocked", n(Status::Blocked)), bold(t.danger)));
        }
        if n(Status::Review) > 0 {
            sep(&mut right);
            right.push(Span::styled(format!("{} to review", n(Status::Review)), Style::default().fg(t.shine)));
        }
        if self.planning {
            sep(&mut right);
            right.push(Span::styled(format!("{} planning", SPIN[(cx.time * 10.0) as usize % 10]), ui::accent(t)));
        }
        sep(&mut right);
        right.push(Span::styled(format!("${:.2} today", self.today()), bold(t.fg)));
        f.render_widget(Paragraph::new(spread(left, right, r.width as usize)), r);
    }

    fn draw_board(&mut self, f: &mut Frame, area: Rect, cx: &Cx) {
        let t = cx.theme;
        let gap = 2u16;
        let cw = (area.width.saturating_sub(gap * 3)) / 4;
        for c in 0..4 {
            let x = area.x + c as u16 * (cw + gap);
            let w = if c == 3 { area.right().saturating_sub(x) } else { cw };
            let r = Rect { x, width: w, ..area };
            self.hits.push((r, Hit::Column(c)));
            let list = self.column(c);
            let on = self.col == c;
            let color = col_color(c, t);
            // header: name + count, then a rule in the column's colour
            let blocked = if c == 1 { list.iter().filter(|&&i| self.store.tasks[i].status == Status::Blocked).count() } else { 0 };
            let mut right = vec![Span::styled(list.len().to_string(), if on { bold(color) } else { ui::muted(t) })];
            if blocked > 0 {
                right.insert(0, Span::styled(format!("{blocked} ⚠  "), bold(t.danger)));
            }
            let head = spread(vec![Span::styled(format!(" {}", COLS[c]), if on { bold(color) } else { bold(t.muted) })], right, w as usize);
            f.render_widget(Paragraph::new(head), Rect { height: 1, ..r });
            let rule_c = if on { color } else { t.frame };
            f.render_widget(Paragraph::new(Span::styled("━".repeat(w as usize), Style::default().fg(rule_c))), Rect { y: r.y + 1, height: 1, ..r });
            let body = Rect { y: r.y + 2, height: r.height.saturating_sub(2), ..r };
            if list.is_empty() {
                let msg: &[&str] = match c {
                    0 => &["no tasks yet", "", "n  new task", "P  plan one with claude"],
                    1 => &["nothing running", "", "enter starts a task"],
                    2 => &["finished work lands here"],
                    _ => &["merged and discarded"],
                };
                let lines: Vec<Line> = std::iter::once(Line::raw(""))
                    .chain(msg.iter().map(|m| {
                        if let Some((k, rest)) = m.split_once("  ") {
                            Line::from(vec![Span::styled(k.to_string(), bold(t.fg)), Span::styled(format!("  {rest}"), ui::muted(t))])
                        } else {
                            Line::styled(m.to_string(), ui::muted(t))
                        }
                    }))
                    .collect();
                if c > 0 || self.repo.is_some() {
                    f.render_widget(Paragraph::new(lines).centered(), body);
                }
                continue;
            }
            // cards, scrolled so the selection stays visible
            let fit = ((body.height + 0) / CARD_H).max(1) as usize;
            let sel = self.row[c].min(list.len() - 1);
            self.row[c] = sel;
            if sel < self.scroll[c] {
                self.scroll[c] = sel;
            } else if sel >= self.scroll[c] + fit {
                self.scroll[c] = sel + 1 - fit;
            }
            self.scroll[c] = self.scroll[c].min(list.len().saturating_sub(fit));
            for (k, &i) in list.iter().enumerate().skip(self.scroll[c]).take(fit) {
                let y = body.y + ((k - self.scroll[c]) as u16) * CARD_H;
                let cr = Rect { y, height: CARD_H.min(body.bottom().saturating_sub(y)), ..body };
                if cr.height < 3 {
                    break;
                }
                let task = self.store.tasks[i].clone();
                self.draw_card(f, cr, &task, on && k == sel, cx);
                self.hits.push((cr, Hit::Card(c, k)));
            }
            // more below / above
            let hidden_below = list.len().saturating_sub(self.scroll[c] + fit);
            if hidden_below > 0 {
                let rr = Rect { y: body.bottom().saturating_sub(1), height: 1, ..body };
                f.render_widget(Paragraph::new(Span::styled(format!("↓ {hidden_below} more"), ui::muted(t))).right_aligned(), rr);
            }
        }
    }

    fn draw_card(&self, f: &mut Frame, r: Rect, task: &Task, sel: bool, cx: &Cx) {
        let t = cx.theme;
        let live = self.live.get(&task.id);
        let busy = live.map(|l| l.busy).unwrap_or(false);
        let blocked = task.status == Status::Blocked;
        let border = if blocked {
            t.danger
        } else if sel {
            t.accent
        } else {
            t.frame
        };
        let mut block = Block::default().borders(Borders::ALL).border_type(if sel { BorderType::Thick } else { BorderType::Rounded }).border_style(Style::default().fg(border));
        if sel && !blocked {
            block = block.border_type(BorderType::Rounded).border_style(bold(border));
        }
        let inner = block.inner(r);
        f.render_widget(block, r);
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
        let w = inner.width as usize;
        let now = super::store::now();
        let spin = SPIN[(cx.time * 10.0) as usize % 10];
        // 1: status glyph + title
        let (glyph, gstyle) = match task.status {
            Status::Todo => ("○", ui::muted(t)),
            Status::Running if busy => (spin, ui::muted(t)),
            Status::Running => (spin, bold(t.accent)),
            Status::Blocked => ("▲", bold(t.danger)),
            Status::Review if busy => (spin, Style::default().fg(t.shine)),
            Status::Review => ("◆", bold(t.shine)),
            Status::Done if task.outcome == "merged" => ("✓", bold(t.good)),
            Status::Done => ("✗", ui::muted(t)),
        };
        let title_style = match task.status {
            Status::Done => Style::default().fg(t.muted),
            _ if sel => bold(t.fg),
            _ => Style::default().fg(t.fg).add_modifier(Modifier::BOLD),
        };
        let (glyph, gstyle) = if task.headless() && task.status == Status::Review && (task.blocked || !task.error.is_empty()) { ("▲", bold(t.danger)) } else { (glyph, gstyle) };
        let mark = self.marked.iter().position(|m| *m == task.id);
        let pri = match task.priority {
            2 => Some(("‼ ", bold(t.danger))),
            1 => Some(("↑ ", bold(t.shine))),
            -1 => Some(("↓ ", ui::muted(t))),
            _ => None,
        };
        let mut head = vec![];
        if let Some(i) = mark {
            head.push(Span::styled(format!("[{}] ", i + 1), Style::default().fg(Color::Black).bg(t.accent).add_modifier(Modifier::BOLD)));
        }
        head.push(Span::styled(format!("{glyph} "), gstyle));
        if let Some((g, st)) = pri {
            head.push(Span::styled(g, st));
        }
        let used: usize = head.iter().map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref())).sum();
        head.push(Span::styled(ui::fit(&task.title, w.saturating_sub(used)), title_style));
        let mut lines = vec![Line::from(head)];
        // 2: agent · model (a lead run's worker: ⚑ worker · tier), and time on the right
        let model = if task.model.is_empty() { "default" } else { &task.model };
        let icon = KINDS.iter().find(|k| k.0 == task.agent).map(|k| k.1).unwrap_or("robot");
        let when = match task.status {
            Status::Todo if task.queued => Span::styled("queued", ui::muted(t)),
            Status::Todo => Span::styled(format!("added {}", dur(now - task.created)), ui::muted(t)),
            Status::Running => Span::styled(dur(now - task.started), ui::accent(t)),
            Status::Blocked => Span::styled("BLOCKED", bold(t.danger)),
            Status::Review if task.want_merge && !self.run_accepting(&task.run) => Span::styled("merge on hold", ui::muted(t)),
            Status::Review if task.want_merge => Span::styled("merging", bold(t.shine)),
            Status::Review => Span::styled(format!("took {}", dur(task.finished - task.started)), ui::muted(t)),
            Status::Done => Span::styled(format!("{} {}", task.outcome, crate::panes::files::clock::stamp(task.finished).get(0..6).unwrap_or("")), ui::muted(t)),
        };
        let mut right = vec![when];
        if task.followups > 0 {
            right.insert(0, Span::styled(format!("↻{} ", task.followups), ui::muted(t)));
        }
        let mut who = if task.run.is_empty() {
            vec![Span::styled(format!("{}{} · {model}", ui::lead(icon), task.agent), ui::muted(t))]
        } else {
            let tier_c = match task.tier.as_str() {
                "cheap" => t.good,
                "premium" => t.shine,
                _ => t.accent,
            };
            let mut v = vec![Span::styled("⚑ ", Style::default().fg(t.shine)), Span::styled(format!("{}{}", ui::lead(icon), task.worker), Style::default().fg(t.fg)), Span::styled(format!(" · {}", task.tier), Style::default().fg(tier_c))];
            if !task.headless() {
                v.push(Span::styled(" · tab", ui::accent(t)));
            }
            v
        };
        // its checkout's own port, in a repo whose project.toml sets checkouts up
        if task.port > 0 && !task.worktree.is_empty() && task.status != Status::Done && self.projects.get(&task.repo).is_some_and(|p| p.is_set()) {
            who.push(Span::styled(format!(" · :{}", task.port), Style::default().fg(t.accent)));
        }
        lines.push(spread(who, right, w));
        // 3-4: what it's doing / asking / the error, and the numbers
        let stat_spans = |t: &Theme| -> Vec<Span<'static>> {
            let mut v = vec![];
            if task.added + task.removed > 0 || task.files > 0 {
                v.push(Span::styled(format!("+{}", task.added), Style::default().fg(t.good)));
                v.push(Span::styled(format!(" −{}", task.removed), Style::default().fg(t.danger)));
                v.push(Span::styled(format!(" · {} file{}", task.files, if task.files == 1 { "" } else { "s" }), ui::muted(t)));
            }
            v
        };
        let cost = |t: &Theme| -> Vec<Span<'static>> {
            let m = money(task.cost_usd);
            if m.is_empty() { vec![] } else { vec![Span::styled(m, bold(t.fg))] }
        };
        if !task.error.is_empty() {
            // (a setup's or gate's failure has several lines: they run on)
            let (a, b) = two_lines(&format!("✗ {}", task.error.split('\n').map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" · ")), w);
            lines.push(Line::styled(a, Style::default().fg(t.danger)));
            lines.push(Line::styled(b, Style::default().fg(t.danger)));
        } else {
            match task.status {
                Status::Todo if task.queued && !task.depends_on.is_empty() => {
                    let (a, _) = two_lines(&task.prompt.replace('\n', " "), w);
                    lines.push(Line::styled(a, ui::muted(t)));
                    // waiting on something that won't merge by itself: say so, rather than wait forever
                    match self.waiting_for(task).iter().find_map(|d| self.dead_dep(&task.run, d)) {
                        Some((title, why)) => lines.push(Line::styled(ui::fit(&format!("stuck: {title} {why} · x drops it"), w), Style::default().fg(t.danger))),
                        None => lines.push(Line::styled(ui::fit(&format!("after {}", task.depends_on.join(", ")), w), Style::default().fg(t.frame))),
                    }
                }
                Status::Todo => {
                    let (a, b) = two_lines(&task.prompt.replace('\n', " "), w);
                    lines.push(Line::styled(a, ui::muted(t)));
                    lines.push(Line::styled(b, ui::muted(t)));
                }
                Status::Blocked => {
                    let (a, b) = two_lines(&task.question, w);
                    lines.push(Line::styled(a, Style::default().fg(t.danger)));
                    lines.push(spread(vec![Span::styled(b, Style::default().fg(t.danger))], cost(t), w));
                }
                Status::Running => {
                    let last = if task.last.is_empty() { "working…".to_string() } else { task.last.clone() };
                    lines.push(Line::styled(ui::fit(&format!("› {last}"), w), Style::default().fg(t.fg)));
                    let mut left = stat_spans(t);
                    let tk = tokens(task.tokens);
                    if left.is_empty() && !tk.is_empty() {
                        left.push(Span::styled(tk, ui::muted(t)));
                    }
                    lines.push(spread(left, cost(t), w));
                }
                Status::Review if task.blocked => {
                    let q = task.questions.first().cloned().unwrap_or_else(|| "blocked — needs a decision".into());
                    lines.push(Line::styled(ui::fit(&format!("? {q}"), w), Style::default().fg(t.danger)));
                    lines.push(spread(stat_spans(t), cost(t), w));
                }
                Status::Review | Status::Done => {
                    let st = if task.status == Status::Review && !busy { Style::default().fg(t.shine) } else { ui::muted(t) };
                    let last = if task.status == Status::Review && !task.summary.is_empty() && !task.want_merge && !busy { task.summary.clone() } else { task.last.clone() };
                    // where its review stands, at the end of the line (it wins over the summary)
                    let chip = if task.status == Status::Review { self.review_chip(task, spin, t) } else { vec![] };
                    lines.push(spread(vec![Span::styled(last, st)], chip, w));
                    lines.push(spread(stat_spans(t), cost(t), w));
                }
            }
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    /// Where a finished card's review stands: a second opinion being written, comments waiting to go, or (a big
    /// or hotspot diff, where research says another vendor's eyes pay off) a nudge to ask for one.
    fn review_chip(&self, task: &Task, spin: &str, t: &Theme) -> Vec<Span<'static>> {
        if let Some(by) = self.reviewing.get(&task.id) {
            return vec![Span::styled(format!("{spin} {by}"), ui::accent(t))];
        }
        let notes = self.notes_of(&task.id);
        if !notes.is_empty() {
            let blocking = notes.iter().filter(|n| n.finding == Some(true)).count();
            let on = notes.iter().filter(|n| n.on).count();
            return vec![Span::styled(if blocking > 0 { format!("● {blocking} blocking") } else { format!("● {on} to send") }, bold(if blocking > 0 { t.danger } else { t.accent }))];
        }
        let hot = task.file_stats.iter().map(|f| f.0.as_str()).chain(task.touched.iter().map(String::as_str)).any(super::plan::is_hotspot);
        if !task.worktree.is_empty() && (task.added + task.removed > 150 || hot || task.size == "M") {
            return vec![Span::styled("V 2nd opinion?", Style::default().fg(t.muted))];
        }
        vec![]
    }

    // ------------------------------------------------------------------ diff

    fn draw_diff(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        let Mode::Diff(v) = &self.mode else { return };
        let id = v.id.clone();
        let typing = v.typing.is_some();
        // a lead run's review diff shows as a task: its goal, integration branch → the user's branch
        let task = self.store.tasks.iter().find(|x| x.id == id).cloned().or_else(|| {
            self.store.runs.iter().find(|r| r.id == id).map(|r| Task { id: r.id.clone(), title: format!("⚑ {}", r.goal.lines().next().unwrap_or("")), branch: r.branch.clone(), base_branch: r.base_branch.clone(), error: r.error.clone(), ..Default::default() })
        });
        let task = task.unwrap_or_default();
        let is_run = self.run_ref(&id).is_some();
        let block = self.comment_block(&id);
        let notes: Vec<super::review::Note> = self.notes_of(&id).to_vec();
        let ticked = notes.iter().filter(|n| n.on).count();
        let reviewing = self.reviewing.get(&id).cloned();
        // keys only where they work: c and enter when feedback can go (a run: its lead; a task: its session)
        let mut hints: Vec<(String, String)> = vec![];
        let mut h = |k: &str, d: &str| hints.push((k.to_string(), d.to_string()));
        if typing {
            h("enter", "add the comment");
            h("esc", "drop it");
        } else {
            h("↑↓", "line");
            h("←→", "file");
            if block.is_none() {
                h("c", "comment on the line");
                if ticked > 0 {
                    h("enter", &format!("send {ticked}"));
                }
            }
            if !notes.is_empty() {
                h("space", "tick");
                h("n", "next comment");
            }
            if reviewing.is_none() {
                h("V", "second opinion");
            }
            if block.is_none() {
                h("C", if is_run { "feedback to the lead" } else { "comment on it all" });
            }
            for (k, d) in [("m", "merge"), ("T", "try it"), ("x", "discard"), ("R", "reload"), ("esc", "board")] {
                h(k, d);
            }
        }
        let hint_refs: Vec<(&str, &str)> = hints.iter().map(|(k, d)| (k.as_str(), d.as_str())).collect();
        let body = ui::hint_line(f, area, &fit_hints(&hint_refs, area.width as usize), t);
        let body = Rect { x: body.x + 1, width: body.width.saturating_sub(2), ..body };
        let Mode::Diff(v) = &mut self.mode else { return };
        // header: title, branch → target, totals, merge check
        let mut right: Vec<Span> = vec![];
        let mut left = vec![Span::styled("◆ ", bold(t.shine)), Span::styled(task.title.clone(), bold(t.fg)), Span::styled(format!("   {} → {}", task.branch, task.base_branch), ui::muted(t))];
        if let Some(by) = &reviewing {
            right.push(Span::styled(format!("{} {by} is reviewing…   ", SPIN[(cx.time * 10.0) as usize % 10]), ui::accent(t)));
        }
        match &v.data {
            None => right.push(Span::styled(format!("{} reading diff", SPIN[(cx.time * 10.0) as usize % 10]), ui::muted(t))),
            Some(Err(e)) => right.push(Span::styled(e.clone(), Style::default().fg(t.danger))),
            Some(Ok(d)) => {
                let (a, r) = d.files.iter().fold((0, 0), |acc, f| (acc.0 + f.added, acc.1 + f.removed));
                right.push(Span::styled(format!("+{a}"), bold(t.good)));
                right.push(Span::styled(format!(" −{r}"), bold(t.danger)));
                right.push(Span::styled(format!(" · {} file{}   ", d.files.len(), if d.files.len() == 1 { "" } else { "s" }), ui::muted(t)));
                match &d.conflicts {
                    Some(c) if c.is_empty() => right.push(Span::styled(format!(" ✓ merges cleanly into {} ", d.target), Style::default().fg(t.good))),
                    Some(c) => right.push(Span::styled(format!(" ✗ conflicts with {} in {} file{} ", d.target, c.len(), if c.len() == 1 { "" } else { "s" }), bold(t.danger))),
                    None => right.push(Span::styled("merge check unavailable", ui::muted(t))),
                }
            }
        }
        if !task.error.is_empty() {
            left.push(Span::styled(format!("   ✗ {}", task.error.lines().next().unwrap_or("")), Style::default().fg(t.danger)));
        }
        f.render_widget(Paragraph::new(spread(left, right, body.width as usize)), Rect { height: 1, ..body });
        let main = Rect { y: body.y + 2, height: body.height.saturating_sub(2), ..body };
        let Some(Ok(d)) = &v.data else {
            return;
        };
        if d.files.is_empty() {
            f.render_widget(Paragraph::new(vec![Line::raw(""), Line::styled("no changes yet", ui::muted(t))]).centered(), main);
            return;
        }
        v.file = v.file.min(d.files.len() - 1);
        let lw = (main.width / 3).clamp(24, 44).min(main.width.saturating_sub(20));
        let lr = Rect { width: lw, ..main };
        let rr = Rect { x: main.x + lw + 1, width: main.width.saturating_sub(lw + 1), ..main };
        // the left column: the files, and under them your comments and the findings, when there are any
        let nh = if notes.is_empty() { 0 } else { (notes.len() as u16 + 2).min(lr.height / 2).max(4.min(lr.height)) };
        let fr = Rect { height: lr.height.saturating_sub(nh), ..lr };
        let li = ui::frame(f, fr, &format!("{}files", ui::lead("files")), Some(&format!("{}/{}", v.file + 1, d.files.len())), false, t);
        let conflicted: Vec<&String> = d.conflicts.iter().flatten().collect();
        let h = li.height as usize;
        let start = v.file.saturating_sub(h.saturating_sub(1));
        for (row, (i, file)) in d.files.iter().enumerate().skip(start).take(h).enumerate() {
            let y = li.y + row as u16;
            let r = Rect { y, height: 1, ..li };
            let on = i == v.file;
            let name = file.path.rsplit(['/', '\\']).next().unwrap_or(&file.path).to_string();
            let dir = file.path.strip_suffix(&name).unwrap_or("").to_string();
            let commented = notes.iter().any(|n| n.path == file.path);
            let mark = if conflicted.iter().any(|c| **c == file.path) {
                "✗ "
            } else if commented {
                "● "
            } else if file.note == "new" {
                "+ "
            } else if file.note == "deleted" {
                "− "
            } else {
                "  "
            };
            let mark_st = match mark {
                "✗ " => bold(t.danger),
                "● " => bold(t.accent),
                _ => Style::default().fg(t.good),
            };
            let mut line = spread(
                vec![Span::styled(mark, mark_st), Span::styled(dir, ui::muted(t)), Span::styled(name, if on { bold(t.accent) } else { Style::default().fg(t.fg) })],
                vec![Span::styled(format!("+{}", file.added), Style::default().fg(t.good)), Span::styled(format!(" −{}", file.removed), Style::default().fg(t.danger))],
                r.width as usize,
            );
            if on {
                line = line.style(Style::default().bg(tint(t.accent, 0.18)));
            }
            f.render_widget(Paragraph::new(line), r);
            self.hits.push((r, Hit::File(i)));
        }
        let file = &d.files[v.file];
        if nh > 0 {
            let nr = Rect { y: fr.bottom(), height: nh, ..lr };
            let sub = format!("{ticked} to send");
            let ni = ui::frame(f, nr, "comments", Some(&sub), false, t);
            let here: Vec<&super::review::Note> = file.lines.get(v.cursor).map(|l| notes.iter().filter(|n| n.at(&file.path, l)).collect()).unwrap_or_default();
            let mut lines = vec![];
            for n in notes.iter().take(ni.height as usize) {
                let (tag, c) = note_style(n, t);
                let at_cursor = here.contains(&n);
                let st = if at_cursor { Style::default().bg(tint(t.accent, 0.18)) } else { Style::default() };
                let text = format!("{}: {}", n.anchor().rsplit(['/', '\\']).next().unwrap_or(""), n.text);
                lines.push(
                    Line::from(vec![
                        Span::styled(if n.on { "● " } else { "○ " }, bold(c)),
                        Span::styled(tag, Style::default().fg(c)),
                        Span::styled(ui::fit(&text, (ni.width as usize).saturating_sub(2 + tag.width())), if n.on { Style::default().fg(t.fg) } else { ui::muted(t) }),
                    ])
                    .style(st),
                );
            }
            f.render_widget(Paragraph::new(lines), ni);
        }
        // hunks, with the cursor's line lit and a mark by every line that has a comment
        let sub = if file.note.is_empty() { format!("+{} −{}", file.added, file.removed) } else { format!("{} · +{} −{}", file.note, file.added, file.removed) };
        let ri = ui::frame(f, rr, &format!("{}{}", ui::lead("code"), file.path), Some(&sub), true, t);
        self.hits.push((rr, Hit::DiffBody));
        // typing a comment: its box sits at the bottom
        let ri_lines = if typing && ri.height > 6 { Rect { height: ri.height - 3, ..ri } } else { ri };
        let gw = file.lines.iter().filter_map(|l| l.old.max(l.new)).max().unwrap_or(1).to_string().len().max(3);
        let h = ri_lines.height as usize;
        let max_scroll = file.lines.len().saturating_sub(h);
        // keep the cursor in view, with a little context around it
        let margin = 2.min(h / 4);
        if v.cursor < v.scroll + margin {
            v.scroll = v.cursor.saturating_sub(margin);
        } else if v.cursor + margin + 1 > v.scroll + h {
            v.scroll = (v.cursor + margin + 1).saturating_sub(h);
        }
        v.scroll = v.scroll.min(max_scroll);
        let (add_bg, del_bg, cur_bg) = (tint(t.good, 0.16), tint(t.danger, 0.16), tint(t.accent, 0.30));
        let tw = (ri_lines.width as usize).saturating_sub(gw * 2 + 5);
        let mut lines = vec![];
        for (i, l) in file.lines.iter().enumerate().skip(v.scroll) {
            if lines.len() >= h {
                break;
            }
            let num = |n: Option<u32>| n.map(|x| format!("{x:>gw$}")).unwrap_or_else(|| " ".repeat(gw));
            let text = ui::fit(&l.text.replace('\t', "    "), tw);
            let here: Vec<&super::review::Note> = notes.iter().filter(|n| n.at(&file.path, l)).collect();
            let cursor = i == v.cursor;
            let mark = match here.first().map(|n| note_style(n, t).1) {
                _ if cursor => Span::styled("›", bold(t.accent)),
                Some(c) => Span::styled("●", bold(c)),
                None => Span::raw(" "),
            };
            let mut line = match l.kind {
                Kind::Hunk => Line::from(vec![
                    Span::raw(" "),
                    Span::styled(format!("{} ", "┄".repeat(gw * 2 + 1)), Style::default().fg(t.frame)),
                    Span::styled(format!("@@ -{} +{} ", l.old.unwrap_or(0), l.new.unwrap_or(0)), ui::accent(t)),
                    Span::styled(ui::fit(&l.text, tw.saturating_sub(14)), ui::muted(t)),
                ]),
                Kind::Note => Line::styled(format!("   {}", l.text), ui::muted(t)),
                Kind::Add => {
                    let pad = tw.saturating_sub(text.width());
                    Line::from(vec![
                        mark,
                        Span::styled(format!("{} {} ", num(None), num(l.new)), Style::default().fg(t.good).bg(add_bg)),
                        Span::styled("+ ", bold(t.good).bg(add_bg)),
                        Span::styled(format!("{text}{}", " ".repeat(pad)), Style::default().fg(t.good).bg(add_bg)),
                    ])
                }
                Kind::Del => {
                    let pad = tw.saturating_sub(text.width());
                    Line::from(vec![
                        mark,
                        Span::styled(format!("{} {} ", num(l.old), num(None)), Style::default().fg(t.danger).bg(del_bg)),
                        Span::styled("- ", bold(t.danger).bg(del_bg)),
                        Span::styled(format!("{text}{}", " ".repeat(pad)), Style::default().fg(t.danger).bg(del_bg)),
                    ])
                }
                Kind::Ctx => {
                    let pad = tw.saturating_sub(text.width());
                    Line::from(vec![mark, Span::styled(format!("{} {} ", num(l.old), num(l.new)), Style::default().fg(t.frame)), Span::raw("  "), Span::styled(format!("{text}{}", " ".repeat(pad)), Style::default().fg(t.fg))])
                }
            };
            if cursor {
                for s in line.spans.iter_mut().skip(1) {
                    s.style = s.style.bg(cur_bg);
                }
            }
            let row = Rect { y: ri_lines.y + lines.len() as u16, height: 1, ..ri_lines };
            self.hits.push((row, Hit::DiffLine(i)));
            lines.push(line);
            // the comments (and findings) on this line, right under it
            for n in &here {
                let (tag, c) = note_style(n, t);
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(gw * 2 + 3)),
                    Span::styled(format!("{} {tag}", if n.on { "▸" } else { "▹" }), bold(c)),
                    Span::styled(ui::fit(&n.text, tw.saturating_sub(4 + tag.width())), Style::default().fg(c)),
                ]));
            }
        }
        lines.truncate(h);
        f.render_widget(Paragraph::new(lines), ri_lines);
        if max_scroll > 0 && !typing {
            let pct = v.scroll * 100 / max_scroll.max(1);
            let r = Rect { x: rr.x + 2, y: rr.bottom().saturating_sub(1), width: 12.min(rr.width.saturating_sub(4)), height: 1 };
            f.render_widget(Paragraph::new(Span::styled(format!(" {pct}% "), ui::muted(t))), r);
        }
        if let (Some(inp), Some(l)) = (&v.typing, file.lines.get(v.cursor)) {
            let at = if l.kind == Kind::Del { format!("{}:{} (removed)", file.path, l.old.unwrap_or(0)) } else { format!("{}:{}", file.path, l.new.unwrap_or(0)) };
            let br = Rect { y: ri.bottom().saturating_sub(3), height: 3.min(ri.height), ..ri };
            f.render_widget(Clear, br);
            let bi = ui::frame(f, br, &format!("comment on {at} · enter adds it · esc drops it"), None, true, t);
            draw_input(f, bi, inp, "what's wrong here, or what should change?", true, t);
        }
    }

    // ------------------------------------------------------------------ popups

    fn draw_form(&self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        let Mode::Form(form) = &self.mode else { return };
        let title = if form.editing.is_some() { "edit task" } else { "new task" };
        let inner = ui::popup(f, area, 90, 37, &format!("{}{title}", ui::lead("new")), t);
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
        let repo = self.repo.as_ref().map(|r| format!("{} · a new branch off {}", r.name, r.branch)).unwrap_or_default();
        f.render_widget(Paragraph::new(Span::styled(repo, ui::muted(t))), Rect { height: 1, ..inner });
        let mut y = inner.y + 2;
        let bottom = inner.bottom();
        let field = |f: &mut Frame, label: &str, h: u16, idx: usize, y: &mut u16| -> Rect {
            let r = Rect { y: *y, height: h.min(bottom.saturating_sub(*y)), ..inner };
            *y += h;
            ui::frame(f, r, label, None, form.field == idx, t)
        };
        // title
        let r = field(f, "title", 3, 0, &mut y);
        draw_input(f, r, &form.title, "what should it be called?", form.field == 0, t);
        // prompt
        let ph = (bottom.saturating_sub(y + 21)).clamp(3, 12);
        let r = field(f, "prompt", ph + 2, 1, &mut y);
        draw_input(f, r, &form.prompt, "what should the agent do? (enter = new line · ↑ earlier prompts · ctrl+t saved prompts)", form.field == 1, t);
        // agent chooser
        let r = field(f, "agent", 3, 2, &mut y);
        let mut spans = vec![];
        for (i, (id, icon)) in KINDS.iter().enumerate() {
            let installed = self.installed(id).is_some();
            let on = form.agent == i;
            let label = format!(" {}{id} ", ui::lead(icon));
            let st = if on {
                Style::default().fg(Color::Black).bg(t.accent).add_modifier(Modifier::BOLD)
            } else if installed {
                Style::default().fg(t.fg)
            } else {
                Style::default().fg(t.frame)
            };
            spans.push(Span::styled(label, st));
            if !installed {
                spans.push(Span::styled(" not installed", Style::default().fg(t.frame)));
            }
            spans.push(Span::raw("   "));
        }
        if form.field == 2 {
            spans.push(Span::styled("←/→ choose", ui::muted(t)));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), r);
        // model
        let r = field(f, "model", 3, 3, &mut y);
        let ph = match KINDS[form.agent].0 {
            "claude" => "default · or sonnet, opus, haiku, fable",
            "codex" => "default · or gpt-5.6-terra, gpt-5.6-sol…",
            _ => "default",
        };
        draw_input(f, r, &form.model, ph, form.field == 3, t);
        // priority
        let r = field(f, "priority", 3, 4, &mut y);
        let mut spans = vec![];
        for (i, (_, name)) in super::PRIORITIES.iter().enumerate() {
            let on = form.priority == i;
            let st = if on { Style::default().fg(Color::Black).bg(t.accent).add_modifier(Modifier::BOLD) } else { Style::default().fg(t.fg) };
            spans.push(Span::styled(format!(" {name} "), st));
            spans.push(Span::raw("  "));
        }
        if form.field == 4 {
            spans.push(Span::styled("←/→ choose · waiting tasks start highest first", ui::muted(t)));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), r);
        // after
        let r = field(f, "after (starts once these are merged)", 3, 5, &mut y);
        draw_input(f, r, &form.after, "task ids or the start of their titles, comma separated · empty = no wait", form.field == 5, t);
        // budget
        let r = field(f, "budget $ (stops it past this)", 3, 6, &mut y);
        draw_input(f, r, &form.budget, "empty = none (run together: the roster's cap) · kimi tabs can't be capped", form.field == 6, t);
        // buttons
        y += 1;
        if y < bottom {
            let btn = |label: &str, on: bool| {
                if on && form.field == 7 {
                    Span::styled(format!(" {label} "), Style::default().fg(Color::Black).bg(t.accent).add_modifier(Modifier::BOLD))
                } else if on {
                    Span::styled(format!("[{label}]"), bold(t.accent))
                } else {
                    Span::styled(format!(" {label} "), Style::default().fg(t.muted))
                }
            };
            let mut spans = vec![btn("add to todo", form.button == 0), Span::raw("   "), btn("add & start", form.button == 1)];
            if !form.err.is_empty() {
                spans.push(Span::styled(format!("   {}", form.err), Style::default().fg(t.danger)));
            }
            f.render_widget(Paragraph::new(Line::from(spans)).centered(), Rect { y, height: 1, ..inner });
        }
        if bottom > inner.y + 1 {
            let hr = Rect { y: bottom - 1, height: 1, ..inner };
            if self.restored {
                f.render_widget(Paragraph::new(Span::styled("restored your draft · ctrl+u clears", bold(t.accent))).centered(), hr);
                return;
            }
            let hints = [("tab", "next field"), ("ctrl+s", "add to todo"), ("enter", "on the buttons"), ("esc", "cancel (kept)")];
            let mut spans = vec![];
            for (i, (k, w)) in hints.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(" · ", ui::muted(t)));
                }
                spans.push(Span::styled(k.to_string(), bold(t.fg)));
                spans.push(Span::styled(format!(" {w}"), ui::muted(t)));
            }
            f.render_widget(Paragraph::new(Line::from(spans)).centered(), hr);
        }
    }

    fn draw_confirm(&self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        let Mode::Confirm(c) = &self.mode else { return };
        if let Some(run) = self.run_ref(&c.id) {
            let goal: String = run.goal.lines().next().unwrap_or("").chars().take(50).collect();
            let workers = self.run_tasks(&run.id).iter().filter(|x| matches!(x.status, Status::Running | Status::Blocked)).count();
            let left = self.run_tasks(&run.id).iter().filter(|x| x.status != Status::Done).count();
            let (title, body, yes, danger): (&str, Vec<String>, &str, bool) = match c.what {
                Pending::StopRun => (
                    "stop the lead run",
                    vec![
                        format!("Stop \"{goal}\"?"),
                        format!("The lead and {workers} running worker(s) stop now, and nothing new merges."),
                        format!("What's merged stays on {} for you to review or merge;", run.branch),
                        "r resumes the run where it stopped.".into(),
                    ],
                    "stop",
                    true,
                ),
                Pending::MergeRun => {
                    let mut body = vec![format!("Squash-merge {} into {}?", run.branch, run.base_branch), format!("Everything the run merged ({} task(s)) becomes one commit on {}.", run.merged, run.base_branch), "Then the integration branch and the lead's checkout are removed.".into()];
                    if left > 0 {
                        body.push(format!("{left} task(s) that didn't make it in go too (worktrees and all)."));
                    }
                    ("merge the lead run", body, "merge", false)
                }
                _ => ("discard the lead run", vec![format!("Throw away \"{goal}\"?"), format!("Stops everything and deletes {} and every worker's worktree.", run.branch), "Nothing reaches your branch.".into()], "discard", true),
            };
            let h = body.len() as u16 + 6;
            let inner = ui::popup(f, area, 76, h, title, t);
            let inner = Rect { x: inner.x + 2, width: inner.width.saturating_sub(4), y: inner.y + 1, height: inner.height.saturating_sub(1) };
            let mut lines: Vec<Line> = body.iter().enumerate().map(|(i, s)| Line::styled(s.clone(), if i == 0 { bold(t.fg) } else { ui::muted(t) })).collect();
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled(format!(" y {yes} "), Style::default().fg(Color::Black).bg(if danger { t.danger } else { t.accent }).add_modifier(Modifier::BOLD)),
                Span::raw("   "),
                Span::styled("esc", bold(t.fg)),
                Span::styled(" cancel", ui::muted(t)),
            ]));
            f.render_widget(Paragraph::new(lines), inner);
            return;
        }
        let Some(task) = self.task(&c.id) else { return };
        let (title, body, yes, danger): (&str, Vec<String>, &str, bool) = match c.what {
            Pending::Merge if !task.run.is_empty() => (
                "merge into the run",
                vec![
                    format!("Queue \"{}\" for the run's integration branch?", task.title),
                    "It's conflict-checked and gated (build/tests) on the merged result".into(),
                    format!("first; nothing reaches {} until you merge the run.", self.run_ref(&task.run).map(|r| r.base_branch.clone()).unwrap_or_default()),
                ],
                "queue merge",
                false,
            ),
            Pending::StopRun | Pending::MergeRun | Pending::DiscardRun => return,
            Pending::Merge => (
                "merge",
                vec![
                    format!("Squash-merge \"{}\" into {}?", task.title, if task.base_branch.is_empty() { "the repo" } else { &task.base_branch }),
                    "Leftover changes get committed first; then the worktree and".into(),
                    format!("the branch {} are removed.", task.branch),
                ],
                "merge",
                false,
            ),
            Pending::Discard => ("discard", vec![format!("Throw away \"{}\"?", task.title), format!("Deletes its worktree and the branch {}.", task.branch), "Its changes are gone for good.".into()], "discard", true),
            Pending::Retry => ("retry", vec![format!("Start \"{}\" over?", task.title), "Its current worktree and changes are thrown away and".into(), "the agent runs again from the latest commit.".into()], "retry", true),
            Pending::Delete => ("delete", vec![format!("Delete the task \"{}\"?", task.title)], "delete", true),
        };
        let h = body.len() as u16 + 6;
        let inner = ui::popup(f, area, 68, h, title, t);
        let inner = Rect { x: inner.x + 2, width: inner.width.saturating_sub(4), y: inner.y + 1, height: inner.height.saturating_sub(1) };
        let mut lines: Vec<Line> = body.iter().enumerate().map(|(i, s)| Line::styled(s.clone(), if i == 0 { bold(t.fg) } else { ui::muted(t) })).collect();
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(format!(" y {yes} "), Style::default().fg(Color::Black).bg(if danger { t.danger } else { t.accent }).add_modifier(Modifier::BOLD)),
            Span::raw("   "),
            Span::styled("esc", bold(t.fg)),
            Span::styled(" cancel", ui::muted(t)),
        ]));
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_prompt_box(&self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        let mut extra: Option<(String, Style)> = None;
        let (title, head, inp, ph) = match &self.mode {
            // feedback for a lead run: the lead picks it up again
            Mode::Comment(id, i) if self.run_ref(id).is_some() => {
                let r = self.run_ref(id).cloned().unwrap_or_default();
                let goal = ui::fit(r.goal.lines().next().unwrap_or(""), 30);
                let spend = self.spend(id);
                if r.budget_usd > 0.0 && spend >= r.budget_usd {
                    extra = Some((format!("the run's budget is spent (${spend:.2} of ${:.2}) — ctrl+b adds $2, or the lead can only merge and answer", r.budget_usd), Style::default().fg(t.danger)));
                } else if r.budget_usd > 0.0 {
                    extra = Some((format!("${spend:.2} of ${:.2} spent · ctrl+b adds $2", r.budget_usd), ui::muted(t)));
                }
                ("feedback to the lead", format!("\"{goal}\" · the lead picks it up again on {}", r.branch), i, "what should change or come next? (\"also add tests\", \"the toggle icon is wrong\")")
            }
            Mode::Comment(id, i) => {
                let task = self.task(id);
                let name = task.map(|x| x.title.clone()).unwrap_or_default();
                let how = match task.map(|x| x.agent.as_str()) {
                    Some("codex") => "codex resume --last",
                    Some("kimi") => "kimi -c",
                    _ => "claude --continue",
                };
                ("comment", format!("feedback for \"{name}\" · continues its session ({how})"), i, "what should change?")
            }
            Mode::Plan(i) => ("plan", "claude reads the repo (read-only) and splits the goal into TODO cards".to_string(), i, "what's the goal? (↑ earlier goals)"),
            _ => return,
        };
        // your ticked line comments and findings go along with it
        let notes: Vec<&super::review::Note> = match &self.mode {
            Mode::Comment(id, _) => self.notes_of(id).iter().filter(|n| n.on).collect(),
            _ => vec![],
        };
        let shown = notes.len().min(6);
        let more = if notes.len() > shown { 1 } else { 0 };
        let ph = if notes.is_empty() { ph } else { "anything to add? (optional) — enter sends it with the comments above" };
        let h = if extra.is_some() { 13 } else { 12 } + if notes.is_empty() { 0 } else { shown as u16 + more + 1 };
        let inner = ui::popup(f, area, 84, h, &format!("{}{title}", ui::lead(if title == "plan" { "claude" } else { "new" })), t);
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
        let w = inner.width as usize;
        let mut head_lines = vec![Line::styled(ui::fit(&head, w), ui::muted(t))];
        if let Some((s, st)) = &extra {
            head_lines.push(Line::styled(ui::fit(s, w), *st));
        }
        if !notes.is_empty() {
            head_lines.push(Line::styled(format!("with {} comment{}:", notes.len(), if notes.len() == 1 { "" } else { "s" }), bold(t.fg)));
            for n in notes.iter().take(shown) {
                let (tag, c) = note_style(n, t);
                head_lines.push(Line::from(vec![Span::styled(format!("● {tag}"), Style::default().fg(c)), Span::styled(ui::fit(&format!("{}: {}", n.anchor(), n.text), w.saturating_sub(2 + tag.width())), Style::default().fg(t.fg))]));
            }
            if more > 0 {
                head_lines.push(Line::styled(format!("  … and {} more", notes.len() - shown), ui::muted(t)));
            }
        }
        let hh = head_lines.len() as u16;
        f.render_widget(Paragraph::new(head_lines), Rect { height: hh, ..inner });
        let r = Rect { y: inner.y + hh + 1, height: inner.height.saturating_sub(hh + 3), ..inner };
        let ri = ui::frame(f, r, "", None, true, t);
        draw_input(f, ri, inp, ph, true, t);
        let hr = Rect { y: inner.bottom().saturating_sub(1), height: 1, ..inner };
        if self.restored {
            f.render_widget(Paragraph::new(Span::styled("restored your draft · ctrl+u clears", bold(t.accent))).centered(), hr);
            return;
        }
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("enter", bold(t.fg)),
                Span::styled(if title == "plan" { " plan" } else { " send" }, ui::muted(t)),
                Span::styled(" · ", ui::muted(t)),
                Span::styled("ctrl+t", bold(t.fg)),
                Span::styled(" saved prompts", ui::muted(t)),
                Span::styled(" · ", ui::muted(t)),
                Span::styled("esc", bold(t.fg)),
                Span::styled(" cancel (kept)", ui::muted(t)),
            ]))
            .centered(),
            hr,
        );
    }

    fn draw_picker(&self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        let Mode::Repo(p) = &self.mode else { return };
        let rows = self.picker_rows();
        let chats = rows.iter().filter(|r| r.1).count();
        let h = (rows.len() as u16 + 11 + if chats > 0 { 2 } else { 0 }).min(26);
        let inner = ui::popup(f, area, 78, h, &format!("{}open a repo", ui::lead("files")), t);
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
        f.render_widget(Paragraph::new(Span::styled("agents work in git worktrees of a repo — which one?", ui::muted(t))), Rect { height: 1, ..inner });
        let r = Rect { y: inner.y + 2, height: 3, ..inner };
        let ri = ui::frame(f, r, "path", None, p.sel == 0, t);
        draw_input(f, ri, &p.input, "type a folder path, e.g. ~/code/project", p.sel == 0, t);
        let mut y = inner.y + 6;
        if !self.store.repos.is_empty() && y < inner.bottom() {
            f.render_widget(Paragraph::new(Span::styled("recent", bold(t.muted))), Rect { y, height: 1, ..inner });
            y += 1;
        }
        for (i, (path, from_chat)) in rows.iter().enumerate() {
            if y + 1 >= inner.bottom() {
                break;
            }
            if *from_chat && (i == 0 || !rows[i - 1].1) {
                // the folders your chats work in
                y += if i > 0 { 1 } else { 0 };
                if y + 1 >= inner.bottom() {
                    break;
                }
                f.render_widget(Paragraph::new(Span::styled("from your chats", bold(t.muted))), Rect { y, height: 1, ..inner });
                y += 1;
                if y + 1 >= inner.bottom() {
                    break;
                }
            }
            let on = p.sel == i + 1;
            let name = std::path::Path::new(path).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let st = if on { Style::default().fg(t.accent).add_modifier(Modifier::BOLD).bg(tint(t.accent, 0.15)) } else { Style::default().fg(t.fg) };
            let line = spread(vec![Span::styled(format!("{} {name}", if on { "›" } else { " " }), st), Span::styled(format!("  {path}"), ui::muted(t).patch(if on { Style::default().bg(tint(t.accent, 0.15)) } else { Style::default() }))], vec![], inner.width as usize);
            f.render_widget(Paragraph::new(line.style(if on { Style::default().bg(tint(t.accent, 0.15)) } else { Style::default() })), Rect { y, height: 1, ..inner });
            y += 1;
        }
        let msg = if p.checking {
            Span::styled("checking…", ui::muted(t))
        } else if !p.err.is_empty() {
            Span::styled(p.err.clone(), Style::default().fg(t.danger))
        } else {
            Span::styled("enter open · ↑↓ choose · del forget · esc close", ui::muted(t))
        };
        f.render_widget(Paragraph::new(msg), Rect { y: inner.bottom().saturating_sub(1), height: 1, ..inner });
    }

    // ------------------------------------------------------------------ sidebar

    pub(super) fn draw_side(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        self.side_hits.clear();
        let mut y = area.y;
        let row = |f: &mut Frame, y: &mut u16, r: Rect| -> Option<Rect> {
            if *y >= r.bottom() {
                return None;
            }
            let rr = Rect { y: *y, height: 1, ..r };
            *y += 1;
            let _ = f;
            Some(rr)
        };
        if let Some(r) = row(f, &mut y, area) {
            ui::side_row(f, r, "new", "new task", "n", false, t);
            self.side_hits.push((r, Hit::NewTask));
        }
        y += 1;
        let cur = self.repo_key();
        if !self.store.repos.is_empty() {
            if let Some(r) = row(f, &mut y, area) {
                f.render_widget(Paragraph::new(Span::styled("repos", bold(t.muted))), r);
            }
        }
        for path in self.store.repos.clone().iter().take(6) {
            let Some(r) = row(f, &mut y, area) else { break };
            let name = std::path::Path::new(path).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| path.clone());
            let open = self.store.tasks.iter().filter(|x| x.repo == *path && x.status != Status::Done).count();
            ui::side_row(f, r, "files", &name, &if open > 0 { open.to_string() } else { String::new() }, *path == cur, t);
            self.side_hits.push((r, Hit::Repo(path.clone())));
        }
        if self.repo.is_none() {
            return;
        }
        y += 1;
        let mine: Vec<&Task> = self.store.tasks.iter().filter(|x| x.repo == cur).collect();
        for (c, name) in ["todo", "running", "review", "done"].iter().enumerate() {
            let Some(r) = row(f, &mut y, area) else { break };
            let n = mine.iter().filter(|x| x.status.column() == c).count();
            let blocked = mine.iter().filter(|x| x.status == Status::Blocked).count();
            let glyph = ["○", "●", "◆", "✓"][c];
            let color = if n == 0 { t.frame } else { col_color(c, t) };
            let mut right = vec![Span::styled(n.to_string(), if n > 0 { Style::default().fg(t.fg) } else { ui::muted(t) })];
            if c == 1 && blocked > 0 {
                right.insert(0, Span::styled(format!("{blocked} ⚠ "), bold(t.danger)));
            }
            let line = spread(vec![Span::styled(format!("{glyph} "), Style::default().fg(color)), Span::styled(name.to_string(), if self.col == c { bold(t.accent) } else { Style::default().fg(t.fg) })], right, r.width as usize);
            f.render_widget(Paragraph::new(line), r);
            self.side_hits.push((r, Hit::Column(c)));
        }
        if let Some(run) = self.current_run().cloned() {
            y += 1;
            if let Some(r) = row(f, &mut y, area) {
                let spin = SPIN[(cx.time * 10.0) as usize % 10];
                let (g, st) = if run.state.active() { (spin, bold(t.accent)) } else { ("⚑", bold(t.shine)) };
                let spend = self.spend(&run.id);
                f.render_widget(Paragraph::new(spread(vec![Span::styled(format!("{g} "), st), Span::styled("lead run", if self.lead_focus { bold(t.accent) } else { Style::default().fg(t.fg) })], vec![Span::styled(format!("${spend:.2}"), ui::muted(t))], r.width as usize)), r);
                self.side_hits.push((r, Hit::LeadPanel));
            }
        }
        let today = self.today();
        if today > 0.0 {
            y += 1;
            if let Some(r) = row(f, &mut y, area) {
                f.render_widget(Paragraph::new(spread(vec![Span::styled("today", ui::muted(t))], vec![Span::styled(format!("${today:.2}"), bold(t.fg))], r.width as usize)), r);
            }
        }
    }
}

/// How a comment shows: (tag, colour). Yours in the accent, a reviewer's blocking finding in red, an optional one
/// in yellow.
fn note_style(n: &super::review::Note, t: &Theme) -> (&'static str, Color) {
    match n.finding {
        Some(true) => ("blocking · ", t.danger),
        Some(false) => ("optional · ", t.shine),
        None => ("", t.accent),
    }
}

/// Split text into two rows of width `w` (the second ellipsised).
fn two_lines(s: &str, w: usize) -> (String, String) {
    let s = s.trim();
    if s.width() <= w {
        return (s.to_string(), String::new());
    }
    // break at the last space that fits
    let mut cut = 0;
    let mut used = 0;
    for (i, c) in s.char_indices() {
        used += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used > w {
            break;
        }
        if c == ' ' {
            cut = i;
        }
        if cut == 0 && used == w {
            cut = i + c.len_utf8();
        }
    }
    if cut == 0 {
        cut = s.char_indices().take_while(|(i, _)| s[..*i].width() < w).last().map(|(i, _)| i).unwrap_or(0);
    }
    (s[..cut].trim_end().to_string(), ui::fit(s[cut..].trim_start(), w))
}

fn fit_hints<'a>(hints: &[(&'a str, &'a str)], w: usize) -> Vec<(&'a str, &'a str)> {
    let mut n = hints.len();
    while n > 1 && hints[..n].iter().map(|(k, d)| k.width() + d.width() + 4).sum::<usize>() > w {
        n -= 1;
    }
    hints[..n].to_vec()
}

/// A text field inside `r`: wrapped text (or a muted placeholder) and the cursor when focused.
pub(super) fn draw_input(f: &mut Frame, r: Rect, inp: &super::input::Input, placeholder: &str, focused: bool, t: &Theme) {
    if r.width < 2 || r.height == 0 {
        return;
    }
    let r = Rect { x: r.x + 1, width: r.width - 1, ..r };
    let w = r.width.saturating_sub(1) as usize;
    if inp.text.is_empty() {
        f.render_widget(Paragraph::new(Span::styled(ui::fit(placeholder, w), Style::default().fg(t.frame))), r);
        if focused {
            f.set_cursor_position(Position { x: r.x, y: r.y });
        }
        return;
    }
    let (rows, (cr, cc)) = if inp.multi { inp.wrapped(w) } else { single_row(inp, w) };
    let h = r.height as usize;
    let top = if cr >= h { cr + 1 - h } else { 0 };
    let lines: Vec<Line> = rows.iter().skip(top).take(h).map(|s| Line::styled(s.clone(), Style::default().fg(t.fg))).collect();
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(lines), r);
    if focused {
        f.set_cursor_position(Position { x: r.x + cc as u16, y: r.y + (cr - top) as u16 });
    }
}

/// A one-line field scrolled horizontally so the cursor stays visible.
fn single_row(inp: &super::input::Input, w: usize) -> (Vec<String>, (usize, usize)) {
    let chars: Vec<char> = inp.text.chars().collect();
    let start = inp.cur.saturating_sub(w.saturating_sub(1));
    let s: String = chars.iter().skip(start).take(w).collect();
    (vec![s], (0, inp.cur - start))
}
