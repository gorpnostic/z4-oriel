//! Lead mode on screen: the lead panel at the top of the board, the new-run form, the roster editor, the watch
//! view (the lead and every worker side by side, live — a multiplexer for the run) and one agent's transcript.

use super::input::Input;
use super::roster::TIERS;
use super::store::{Run, RunState, Status, Task};
use super::stream::Entry;
use super::{Agents, Hit, KINDS, LeadForm, LogTarget, LogView, Mode, RosterEdit, WatchView};
use crate::config::RosterEntry;
use crate::pane::Cx;
use crate::theme::{Theme, mix};
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};
use unicode_width::UnicodeWidthStr;

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub const LEAD_H: u16 = 9;

fn bold(c: Color) -> Style {
    Style::default().fg(c).add_modifier(Modifier::BOLD)
}

fn tint(c: Color, amount: f32) -> Color {
    match c {
        Color::Rgb(..) => mix(Color::Rgb(14, 14, 14), c, amount),
        _ => Color::Reset,
    }
}

fn money(v: f64) -> String {
    if v <= 0.0 { "$0.00".into() } else if v < 0.01 { "<$0.01".into() } else { format!("${v:.2}") }
}

/// A spend bar: ▰▰▰▱▱▱
fn bar(frac: f64, n: usize) -> (String, String) {
    let on = ((frac.clamp(0.0, 1.0)) * n as f64).round() as usize;
    ("▰".repeat(on), "▱".repeat(n - on))
}

/// Left and right text on one row, the right side winning when it doesn't fit.
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

fn state_label(r: &Run) -> &'static str {
    match r.state {
        RunState::Starting => "starting",
        RunState::Running => "running",
        RunState::Review => "ready for review",
        RunState::Merged => "merged",
        RunState::Stopped => "stopped",
        RunState::Discarded => "discarded",
    }
}

fn agent_icon(agent: &str) -> &'static str {
    KINDS.iter().find(|k| k.0 == agent).map(|k| k.1).unwrap_or("robot")
}

/// Transcript lines for one entry. `full` = with diff/output bodies and every line of what it said.
fn entry_lines(e: &Entry, w: usize, full: bool, t: &Theme, spin: &str) -> Vec<Line<'static>> {
    let mut out = vec![];
    match e.kind {
        't' => {
            let (g, gs) = match e.status.as_str() {
                "running" => (spin.to_string(), bold(t.accent)),
                "error" => ("✗".into(), bold(t.danger)),
                _ => ("✓".into(), Style::default().fg(t.good)),
            };
            let label_style = if e.label.contains('_') || e.label == "roster" || e.label == "plan" || e.label == "wait" { bold(t.shine) } else { bold(t.fg) };
            let mut spans = vec![Span::styled(format!("{g} "), gs), Span::styled(e.label.clone(), label_style)];
            if !e.target.is_empty() {
                spans.push(Span::styled(format!(" {}", e.target), Style::default().fg(t.fg)));
            }
            let right = if e.summary.is_empty() { vec![] } else { vec![Span::styled(e.summary.clone(), if e.status == "error" { Style::default().fg(t.danger) } else { ui::muted(t) })] };
            out.push(spread(spans, right, w));
            if full {
                for b in &e.body {
                    let (k, rest) = b.split_at(b.chars().next().map(|c| c.len_utf8()).unwrap_or(0));
                    let text = ui::fit(&rest.replace('\t', "    "), w.saturating_sub(4));
                    let pad = " ".repeat(w.saturating_sub(4 + text.width()));
                    out.push(match k {
                        "+" => Line::from(vec![Span::raw("  "), Span::styled(format!("+ {text}{pad}"), Style::default().fg(t.good).bg(tint(t.good, 0.14)))]),
                        "-" => Line::from(vec![Span::raw("  "), Span::styled(format!("- {text}{pad}"), Style::default().fg(t.danger).bg(tint(t.danger, 0.14)))]),
                        "!" => Line::from(vec![Span::raw("  "), Span::styled(format!("│ {text}"), Style::default().fg(t.danger))]),
                        _ => Line::from(vec![Span::raw("  "), Span::styled(format!("│ {text}"), ui::muted(t))]),
                    });
                }
            }
        }
        's' => {
            let lines: Vec<&str> = e.label.lines().filter(|l| !l.trim().is_empty()).collect();
            let take = if full { 40 } else { 1 };
            for (i, l) in lines.iter().take(take).enumerate() {
                let lead = if i == 0 { "› " } else { "  " };
                out.push(Line::from(vec![Span::styled(lead, Style::default().fg(t.accent)), Span::styled(ui::fit(l.trim(), w.saturating_sub(2)), Style::default().fg(t.fg).add_modifier(Modifier::ITALIC))]));
            }
            if full && lines.len() > take {
                out.push(Line::styled(format!("  … {} more lines", lines.len() - take), ui::muted(t)));
            }
        }
        'e' => out.push(Line::styled(ui::fit(&format!("✗ {}", e.label.lines().next().unwrap_or("")), w), Style::default().fg(t.danger))),
        _ => out.push(Line::styled(ui::fit(&format!("· {}", e.label.lines().next().unwrap_or("")), w), ui::muted(t))),
    }
    out
}

impl Agents {
    // ---------------------------------------------------------------- the lead panel

    /// The run's panel at the top of the board: goal, spend, branch, what the lead did lately.
    pub(super) fn draw_lead_panel(&mut self, f: &mut Frame, r: Rect, cx: &Cx) {
        let t = cx.theme;
        let Some(run) = self.current_run().cloned() else { return };
        let focus = self.lead_focus;
        let spin = SPIN[(cx.time * 10.0) as usize % 10];
        let proto = if run.protocol == "mcp" { "MCP tools" } else { "text protocol" };
        let title = format!("⚑ lead · {}{} · {}", run.agent, if run.model.is_empty() { String::new() } else { format!(" {}", run.model) }, proto);
        let hints = if !focus {
            "↑ select".to_string()
        } else if run.state.active() {
            "enter transcript · w watch · s stop · d diff so far".to_string()
        } else if run.state == RunState::Stopped && run.error.contains("r resumes") {
            "r resume · d review · m merge · x discard".to_string()
        } else {
            "enter transcript · w watch · d review · m merge · x discard".to_string()
        };
        let border = if focus { t.accent } else if run.state == RunState::Review { t.shine } else { t.frame };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(if focus { bold(border) } else { Style::default().fg(border) })
            .title(Line::from(vec![Span::styled(format!(" {} ", title), bold(if focus { t.accent } else { t.fg }))]))
            .title_bottom(Line::from(Span::styled(format!(" {hints} "), if focus { Style::default().fg(t.fg) } else { ui::muted(t) })).right_aligned());
        let inner = block.inner(r);
        f.render_widget(Clear, r);
        f.render_widget(block, r);
        self.hits.push((r, Hit::LeadPanel));
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
        let w = inner.width as usize;
        let now = super::store::now();
        let spend = self.spend(&run.id);
        let frac = if run.budget_usd > 0.0 { spend / run.budget_usd } else { 0.0 };
        let (on, off) = bar(frac, 12);
        let over = run.budget_usd > 0.0 && spend >= run.budget_usd;
        let (glyph, gs) = match run.state {
            RunState::Starting | RunState::Running => (spin, bold(t.accent)),
            RunState::Review => ("◆", bold(t.shine)),
            RunState::Merged => ("✓", bold(t.good)),
            _ => ("■", ui::muted(t)),
        };
        let elapsed = if run.finished > 0 { run.finished - run.created } else { now - run.created };
        let mut lines = vec![spread(
            vec![Span::styled(format!("{glyph} "), gs), Span::styled(run.goal.lines().next().unwrap_or("").to_string(), bold(t.fg))],
            vec![
                Span::styled(format!("{} {}  ", state_label(&run), super::view::dur(elapsed)), if run.state == RunState::Review { bold(t.shine) } else { ui::muted(t) }),
                Span::styled(money(spend), bold(if over { t.danger } else { t.fg })),
                Span::styled(format!(" of {}  ", money(run.budget_usd)), ui::muted(t)),
                Span::styled(on, Style::default().fg(if over { t.danger } else if frac > 0.75 { t.shine } else { t.accent })),
                Span::styled(off, Style::default().fg(t.frame)),
            ],
            w,
        )];
        let tasks = self.run_tasks(&run.id);
        let n = |f: &dyn Fn(&&Task) -> bool| tasks.iter().filter(|x| f(x)).count();
        let running = n(&|x| matches!(x.status, Status::Running | Status::Blocked));
        let queued = n(&|x| x.status == Status::Todo && x.queued);
        let merged = n(&|x| x.status == Status::Done && x.outcome == "merged");
        let review = n(&|x| x.status == Status::Review && !x.want_merge);
        let merging = n(&|x| x.status == Status::Review && x.want_merge);
        let mut facts = vec![Span::styled(if run.branch.is_empty() { "making the integration branch…".to_string() } else { format!("{} ← {}", run.branch, run.base_branch) }, ui::accent(t))];
        let mut add = |s: String, st: Style| {
            facts.push(Span::styled(" · ", ui::muted(t)));
            facts.push(Span::styled(s, st));
        };
        add(format!("{running}/{} running", run.max_parallel), if running > 0 { ui::accent(t) } else { ui::muted(t) });
        if queued > 0 {
            add(format!("{queued} queued"), ui::muted(t));
        }
        if review > 0 {
            add(format!("{review} to review"), Style::default().fg(t.shine));
        }
        if merging > 0 {
            add(format!("{merging} merging"), Style::default().fg(t.shine));
        }
        add(format!("{merged} merged"), if merged > 0 { Style::default().fg(t.good) } else { ui::muted(t) });
        let lead_cost = vec![Span::styled(format!("lead {}", money(run.cost_usd)), ui::muted(t))];
        lines.push(spread(facts, lead_cost, w));
        if !run.error.is_empty() {
            lines.push(Line::styled(ui::fit(&format!("✗ {}", run.error), w), Style::default().fg(t.danger)));
        } else if !run.summary.is_empty() && !run.state.active() && !run.log.iter().rev().take(4).any(|l| l.starts_with("done: ")) {
            lines.push(Line::styled(ui::fit(&format!("done: {}", run.summary), w), Style::default().fg(t.shine)));
        }
        let room = (inner.height as usize).saturating_sub(lines.len());
        for l in run.log.iter().rev().take(room).collect::<Vec<_>>().into_iter().rev() {
            let st = if l.contains(" ✗ ") || l.starts_with("lead stopped") || l.starts_with("failed") || l.starts_with("blocked") || l.starts_with("plan refused") || l.starts_with("budget") {
                Style::default().fg(t.danger)
            } else if l.starts_with("merged") || l.contains(" ✓ merged") {
                Style::default().fg(t.good)
            } else if l.starts_with("bounced") || l.starts_with("redispatched") {
                Style::default().fg(t.shine)
            } else if l.starts_with('“') {
                ui::muted(t).add_modifier(Modifier::ITALIC)
            } else if l.starts_with('✎') {
                ui::accent(t)
            } else {
                Style::default().fg(t.fg)
            };
            lines.push(Line::from(vec![Span::styled("› ", ui::muted(t)), Span::styled(ui::fit(l, w.saturating_sub(2)), st)]));
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    // ---------------------------------------------------------------- new run form

    pub(super) fn new_lead_form(&self) -> LeadForm {
        let installed = |a: &str| self.installed(a).is_some();
        // the last lead used, else the first installed agent
        let agent = KINDS.iter().position(|k| k.0 == self.lead_cfg.agent && (installed(k.0) || self.fake_lead.is_some())).or_else(|| KINDS.iter().position(|k| installed(k.0))).unwrap_or(0);
        LeadForm {
            field: 0,
            goal: Input::new("", true),
            agent,
            model: Input::new(&self.lead_cfg.model, false),
            parallel: Input::new(&self.lead_cfg.max_parallel.clamp(1, 5).to_string(), false),
            budget: Input::new(&format!("{:.2}", self.lead_cfg.run_budget_usd), false),
            err: String::new(),
        }
    }

    pub(super) fn lead_form_key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        let Mode::LeadForm(mut form) = std::mem::replace(&mut self.mode, Mode::Board) else { return false };
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        form.err.clear();
        let submit = |s: &mut Self, form: LeadForm, cx: &mut Cx| {
            let agent = KINDS[form.agent].0;
            let par = form.parallel.text.trim().parse::<u32>().unwrap_or(3).clamp(1, 5);
            let budget = form.budget.text.trim().trim_start_matches('$').parse::<f64>().unwrap_or(s.lead_cfg.run_budget_usd).max(0.0);
            match s.start_run(&form.goal.text, agent, &form.model.text, par, budget, cx) {
                Ok(_) => {}
                Err(e) => {
                    let mut form = form;
                    form.err = e;
                    s.mode = Mode::LeadForm(form);
                }
            }
        };
        match k.code {
            KeyCode::Esc => return true,
            KeyCode::Char('s') if ctrl => {
                submit(self, form, cx);
                return true;
            }
            KeyCode::Tab => form.field = (form.field + 1) % 6,
            KeyCode::BackTab => form.field = (form.field + 5) % 6,
            _ => {
                let used = match form.field {
                    0 => form.goal.key(k),
                    2 => form.model.key(k),
                    3 => matches!(k.code, KeyCode::Char(c) if !c.is_ascii_digit() && !ctrl) || form.parallel.key(k),
                    4 => matches!(k.code, KeyCode::Char(c) if !(c.is_ascii_digit() || c == '.') && !ctrl) || form.budget.key(k),
                    _ => false,
                };
                if !used {
                    match k.code {
                        KeyCode::Enter if form.field == 5 => {
                            submit(self, form, cx);
                            return true;
                        }
                        KeyCode::Enter | KeyCode::Down => form.field = (form.field + 1).min(5),
                        KeyCode::Up => form.field = form.field.saturating_sub(1),
                        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if form.field == 1 => {
                            let any = self.agents.iter().any(|a| a.1.is_some()) && self.fake_lead.is_none();
                            let step = if k.code == KeyCode::Left { KINDS.len() - 1 } else { 1 };
                            for _ in 0..KINDS.len() {
                                form.agent = (form.agent + step) % KINDS.len();
                                if !any || self.installed(KINDS[form.agent].0).is_some() {
                                    break;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        self.mode = Mode::LeadForm(form);
        true
    }

    pub(super) fn draw_lead_form(&self, f: &mut Frame, area: Rect, cx: &Cx) {
        let t = cx.theme;
        let Mode::LeadForm(form) = &self.mode else { return };
        let inner = ui::popup(f, area, 92, 25, "⚑ new lead run", t);
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
        let repo = self.repo.as_ref().map(|r| format!("{} · a lead agent plans the goal and hands it to workers on a new branch off {}", r.name, r.branch)).unwrap_or_default();
        f.render_widget(Paragraph::new(Span::styled(ui::fit(&repo, inner.width as usize), ui::muted(t))), Rect { height: 1, ..inner });
        let bottom = inner.bottom();
        let mut y = inner.y + 2;
        let field = |f: &mut Frame, label: &str, h: u16, idx: usize, y: &mut u16, r: Rect| -> Rect {
            let rr = Rect { y: *y, height: h.min(bottom.saturating_sub(*y)), ..r };
            *y += h;
            ui::frame(f, rr, label, None, form.field == idx, t)
        };
        let r = field(f, "goal", 7, 0, &mut y, inner);
        super::view::draw_input(f, r, &form.goal, "what should the team get done? (enter = new line)", form.field == 0, t);
        // who leads
        let r = field(f, "lead", 4, 1, &mut y, inner);
        let mut spans = vec![];
        for (i, (id, icon)) in KINDS.iter().enumerate() {
            let installed = self.installed(id).is_some() || self.fake_lead.is_some();
            let on = form.agent == i;
            let st = if on {
                Style::default().fg(Color::Black).bg(t.accent).add_modifier(Modifier::BOLD)
            } else if installed {
                Style::default().fg(t.fg)
            } else {
                Style::default().fg(t.frame)
            };
            spans.push(Span::styled(format!(" {}{id} ", ui::lead(icon)), st));
            spans.push(Span::styled(if installed { "   " } else { " not installed   " }, Style::default().fg(t.frame)));
        }
        if form.field == 1 {
            spans.push(Span::styled("←/→ choose", ui::muted(t)));
        }
        let agent = KINDS[form.agent].0;
        let how = match agent {
            "claude" => "calls oriel's tools over MCP (--mcp-config) · read-only: can't edit or run commands",
            "codex" => "calls oriel's tools over MCP (-c mcp_servers.oriel…) · read-only sandbox",
            "kimi" => "calls oriel's tools over MCP (a .kimi-code/mcp.json in its own checkout) · falls back to JSON actions",
            _ => "answers with JSON actions each turn",
        };
        f.render_widget(Paragraph::new(vec![Line::from(spans), Line::styled(ui::fit(how, r.width as usize), ui::muted(t))]), r);
        // model · parallel · budget in a row
        let third = inner.width / 3;
        let row = Rect { y, height: 3.min(bottom.saturating_sub(y)), ..inner };
        let (a, b, c) = (Rect { width: third.saturating_sub(1), ..row }, Rect { x: row.x + third, width: third.saturating_sub(1), ..row }, Rect { x: row.x + third * 2, width: row.width.saturating_sub(third * 2), ..row });
        let ph = match agent {
            "claude" => "default · opus, sonnet…",
            "codex" => "default · gpt-5.6-sol…",
            _ => "default",
        };
        let ri = ui::frame(f, a, "model", None, form.field == 2, t);
        super::view::draw_input(f, ri, &form.model, ph, form.field == 2, t);
        let ri = ui::frame(f, b, "workers at once (1-5)", None, form.field == 3, t);
        super::view::draw_input(f, ri, &form.parallel, "3", form.field == 3, t);
        let ri = ui::frame(f, c, "run budget $ (all agents)", None, form.field == 4, t);
        super::view::draw_input(f, ri, &form.budget, "8.00", form.field == 4, t);
        y += 4;
        // the roster, briefly
        if y + 1 < bottom {
            let roster = self.roster();
            let mut spans = vec![Span::styled("workers  ", bold(t.muted))];
            for (i, w) in roster.iter().filter(|w| w.enabled).enumerate() {
                if i > 0 {
                    spans.push(Span::styled(" · ", ui::muted(t)));
                }
                spans.push(Span::styled(w.name.clone(), Style::default().fg(t.fg)));
                spans.push(Span::styled(format!(" {}", w.tier), ui::muted(t)));
            }
            if roster.iter().all(|w| !w.enabled) {
                spans.push(Span::styled("none — press R on the board to set up the roster", Style::default().fg(t.danger)));
            }
            spans.push(Span::styled("   (R edits)", Style::default().fg(t.frame)));
            f.render_widget(Paragraph::new(Line::from(spans)), Rect { y, height: 1, ..inner });
            y += 2;
        }
        if y < bottom {
            let btn = if form.field == 5 { Span::styled(" start lead run ", Style::default().fg(Color::Black).bg(t.accent).add_modifier(Modifier::BOLD)) } else { Span::styled("[start lead run]", bold(t.accent)) };
            let mut spans = vec![btn];
            if !form.err.is_empty() {
                spans.push(Span::styled(format!("   {}", form.err), Style::default().fg(t.danger)));
            }
            f.render_widget(Paragraph::new(Line::from(spans)).centered(), Rect { y, height: 1, ..inner });
        }
        if bottom > inner.y + 1 {
            let hr = Rect { y: bottom - 1, height: 1, ..inner };
            let hints = [("tab", "next field"), ("ctrl+s", "start"), ("esc", "cancel")];
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

    // ---------------------------------------------------------------- the roster

    fn roster_edit(&self, idx: Option<usize>) -> RosterEdit {
        let w = idx.and_then(|i| self.roster().get(i).cloned()).unwrap_or_default();
        RosterEdit {
            idx,
            field: 0,
            name: Input::new(&w.name, false),
            agent: KINDS.iter().position(|k| k.0 == w.agent).unwrap_or(0),
            model: Input::new(&w.model, false),
            tier: TIERS.iter().position(|x| *x == w.tier).unwrap_or(1),
            good_at: Input::new(&w.good_at, false),
            turns: Input::new(&w.max_turns.to_string(), false),
            budget: Input::new(&format!("{:.2}", w.budget_usd), false),
            enabled: w.enabled,
            err: String::new(),
        }
    }

    /// Roster edits are saved to the config right away (the lead reads them on its next roster call).
    fn set_roster(&mut self, r: Vec<RosterEntry>) {
        self.roster = r;
        self.save_config();
    }

    pub(super) fn roster_key(&mut self, k: KeyEvent) -> bool {
        let Mode::Roster(mut v) = std::mem::replace(&mut self.mode, Mode::Board) else { return false };
        let mut roster = self.roster();
        if let Some(mut e) = v.edit.take() {
            e.err.clear();
            let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
            let save = |s: &mut Self, e: &mut RosterEdit, roster: &mut Vec<RosterEntry>| -> bool {
                let name = e.name.text.trim().to_string();
                if name.is_empty() {
                    e.err = "give it a name".into();
                    e.field = 0;
                    return false;
                }
                if roster.iter().enumerate().any(|(i, w)| w.name.eq_ignore_ascii_case(&name) && Some(i) != e.idx) {
                    e.err = format!("{name} is already on the roster");
                    return false;
                }
                let w = RosterEntry {
                    name,
                    agent: KINDS[e.agent].0.into(),
                    model: e.model.text.trim().into(),
                    tier: TIERS[e.tier].into(),
                    good_at: e.good_at.text.trim().into(),
                    max_turns: e.turns.text.trim().parse().unwrap_or(40),
                    budget_usd: e.budget.text.trim().trim_start_matches('$').parse().unwrap_or(1.5),
                    enabled: e.enabled,
                };
                match e.idx {
                    Some(i) if i < roster.len() => roster[i] = w,
                    _ => roster.push(w),
                }
                s.set_roster(roster.clone());
                true
            };
            match k.code {
                KeyCode::Esc => {
                    self.mode = Mode::Roster(v);
                    return true;
                }
                KeyCode::Char('s') if ctrl => {
                    if save(self, &mut e, &mut roster) {
                        v.sel = e.idx.unwrap_or(roster.len() - 1);
                        self.mode = Mode::Roster(v);
                        return true;
                    }
                }
                KeyCode::Tab => e.field = (e.field + 1) % 8,
                KeyCode::BackTab => e.field = (e.field + 7) % 8,
                _ => {
                    let used = match e.field {
                        0 => e.name.key(k),
                        2 => e.model.key(k),
                        4 => e.good_at.key(k),
                        5 => matches!(k.code, KeyCode::Char(c) if !c.is_ascii_digit() && !ctrl) || e.turns.key(k),
                        6 => matches!(k.code, KeyCode::Char(c) if !(c.is_ascii_digit() || c == '.') && !ctrl) || e.budget.key(k),
                        _ => false,
                    };
                    if !used {
                        match k.code {
                            KeyCode::Enter if e.field == 7 => {
                                if save(self, &mut e, &mut roster) {
                                    v.sel = e.idx.unwrap_or(roster.len() - 1);
                                    self.mode = Mode::Roster(v);
                                    return true;
                                }
                            }
                            KeyCode::Enter | KeyCode::Down => e.field = (e.field + 1).min(7),
                            KeyCode::Up => e.field = e.field.saturating_sub(1),
                            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if e.field == 1 => {
                                e.agent = if k.code == KeyCode::Left { (e.agent + KINDS.len() - 1) % KINDS.len() } else { (e.agent + 1) % KINDS.len() };
                            }
                            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if e.field == 3 => {
                                e.tier = if k.code == KeyCode::Left { (e.tier + TIERS.len() - 1) % TIERS.len() } else { (e.tier + 1) % TIERS.len() };
                            }
                            _ => {}
                        }
                    }
                }
            }
            v.edit = Some(e);
            self.mode = Mode::Roster(v);
            return true;
        }
        let n = roster.len();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('R') => return true,
            KeyCode::Up | KeyCode::Char('k') => v.sel = v.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => v.sel = (v.sel + 1).min(n.saturating_sub(1)),
            KeyCode::Char(' ') if n > 0 => {
                roster[v.sel].enabled = !roster[v.sel].enabled;
                self.set_roster(roster);
            }
            KeyCode::Enter | KeyCode::Char('e') if n > 0 => v.edit = Some(self.roster_edit(Some(v.sel))),
            KeyCode::Char('a') | KeyCode::Char('n') => v.edit = Some(self.roster_edit(None)),
            KeyCode::Char('x') | KeyCode::Delete if n > 0 => {
                roster.remove(v.sel);
                v.sel = v.sel.min(roster.len().saturating_sub(1));
                self.set_roster(roster);
            }
            KeyCode::Char('J') if v.sel + 1 < n => {
                roster.swap(v.sel, v.sel + 1);
                v.sel += 1;
                self.set_roster(roster);
            }
            KeyCode::Char('K') if v.sel > 0 => {
                roster.swap(v.sel, v.sel - 1);
                v.sel -= 1;
                self.set_roster(roster);
            }
            _ => {}
        }
        self.mode = Mode::Roster(v);
        true
    }

    pub(super) fn draw_roster(&self, f: &mut Frame, area: Rect, cx: &Cx) {
        let t = cx.theme;
        let Mode::Roster(v) = &self.mode else { return };
        let roster = self.roster();
        let h = (roster.len() as u16 * 2 + 7).clamp(14, 34);
        let inner = ui::popup(f, area, 110, h, "⚑ roster · the workers a lead can use", t);
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
        let w = inner.width as usize;
        let src = if self.roster.is_empty() { "defaults from what's installed — any edit saves them to the config".to_string() } else { "saved in oriel's config.toml under [[roster]] (oriel --config prints where)".to_string() };
        f.render_widget(Paragraph::new(Span::styled(ui::fit(&src, w), ui::muted(t))), Rect { height: 1, ..inner });
        let mut y = inner.y + 2;
        if roster.is_empty() {
            f.render_widget(Paragraph::new(Span::styled("no workers — a adds one", ui::muted(t))), Rect { y, height: 1, ..inner });
        }
        for (i, wk) in roster.iter().enumerate() {
            if y + 2 > inner.bottom().saturating_sub(1) {
                break;
            }
            let on = v.sel == i;
            let installed = self.installed(&wk.agent).is_some() || self.fake_worker.is_some();
            let bg = if on { Style::default().bg(tint(t.accent, 0.15)) } else { Style::default() };
            let check = if wk.enabled { "●" } else { "○" };
            let tier_c = match wk.tier.as_str() {
                "cheap" => t.good,
                "premium" => t.shine,
                _ => t.accent,
            };
            let limits = self.limits.text(&wk.agent);
            let paused = self.paused.get(&wk.agent).is_some_and(|u| *u > super::store::now());
            let right = vec![
                Span::styled(if paused { "paused (rate limit)  ".to_string() } else { format!("{limits}  ") }, if paused { Style::default().fg(t.danger) } else { ui::muted(t) }),
                Span::styled(format!("${:.2}/task · {} turns", wk.budget_usd, wk.max_turns), ui::muted(t)),
            ];
            let left = vec![
                Span::styled(format!("{} {check} ", if on { "›" } else { " " }), if wk.enabled { bold(t.accent) } else { Style::default().fg(t.frame) }),
                Span::styled(format!("{:<16}", ui::fit(&wk.name, 16)), if wk.enabled { bold(t.fg) } else { Style::default().fg(t.muted) }),
                Span::styled(format!("{}{}", ui::lead(agent_icon(&wk.agent)), wk.agent), Style::default().fg(if installed { t.fg } else { t.frame })),
                Span::styled(if wk.model.is_empty() { " · default".to_string() } else { format!(" · {}", wk.model) }, ui::muted(t)),
                Span::styled(format!("  {}", wk.tier), bold(tier_c)),
                Span::styled(if installed { String::new() } else { "  not installed".into() }, Style::default().fg(t.danger)),
            ];
            f.render_widget(Paragraph::new(spread(left, right, w).style(bg)), Rect { y, height: 1, ..inner });
            let rec = self.store.records.get(&wk.name).filter(|r| r.tasks > 0).map(|r| format!("   · {} merged of {} ({} first try)", r.merged, r.tasks, r.first_try)).unwrap_or_default();
            let good = if wk.good_at.is_empty() { "general coding".to_string() } else { wk.good_at.clone() };
            f.render_widget(Paragraph::new(Line::from(vec![Span::raw("     "), Span::styled(ui::fit(&format!("{good}{rec}"), w.saturating_sub(5)), ui::muted(t))]).style(bg)), Rect { y: y + 1, height: 1, ..inner });
            y += 2;
        }
        let hint = "↑↓ choose · enter edit · space on/off · a add · x remove · J/K reorder · esc close";
        f.render_widget(Paragraph::new(Span::styled(hint, ui::muted(t))).centered(), Rect { y: inner.bottom().saturating_sub(1), height: 1, ..inner });
        if let Some(e) = &v.edit {
            self.draw_roster_edit(f, area, e, t);
        }
    }

    fn draw_roster_edit(&self, f: &mut Frame, area: Rect, e: &RosterEdit, t: &Theme) {
        let inner = ui::popup(f, area, 72, 22, if e.idx.is_some() { "edit worker" } else { "add a worker" }, t);
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
        let bottom = inner.bottom();
        let mut y = inner.y + 1;
        let row = |f: &mut Frame, y: &mut u16, label: &str, idx: usize, draw: &dyn Fn(&mut Frame, Rect)| {
            if *y + 3 > bottom {
                return;
            }
            let r = Rect { y: *y, height: 3, ..inner };
            let ri = ui::frame(f, r, label, None, e.field == idx, t);
            draw(f, ri);
            *y += 3;
        };
        row(f, &mut y, "name", 0, &|f, r| super::view::draw_input(f, r, &e.name, "e.g. codex-fast", e.field == 0, t));
        let cycle = |items: Vec<&str>, on: usize, focus: bool| -> Line<'static> {
            let mut spans = vec![];
            for (i, it) in items.iter().enumerate() {
                spans.push(if i == on { Span::styled(format!(" {it} "), Style::default().fg(Color::Black).bg(t.accent).add_modifier(Modifier::BOLD)) } else { Span::styled(format!(" {it} "), Style::default().fg(t.fg)) });
                spans.push(Span::raw("  "));
            }
            if focus {
                spans.push(Span::styled("←/→", ui::muted(t)));
            }
            Line::from(spans)
        };
        let agents: Vec<&str> = KINDS.iter().map(|k| k.0).collect();
        row(f, &mut y, "agent", 1, &|f, r| f.render_widget(Paragraph::new(cycle(agents.clone(), e.agent, e.field == 1)), r));
        let half = inner.width / 2;
        if y + 3 <= bottom {
            let a = Rect { y, height: 3, width: half.saturating_sub(1), ..inner };
            let b = Rect { y, height: 3, x: inner.x + half, width: inner.width - half, ..inner };
            let ri = ui::frame(f, a, "model", None, e.field == 2, t);
            super::view::draw_input(f, ri, &e.model, "default", e.field == 2, t);
            let ri = ui::frame(f, b, "tier", None, e.field == 3, t);
            f.render_widget(Paragraph::new(cycle(TIERS.to_vec(), e.tier, e.field == 3)), ri);
            y += 3;
        }
        row(f, &mut y, "good at", 4, &|f, r| super::view::draw_input(f, r, &e.good_at, "what the lead should hand it", e.field == 4, t));
        if y + 3 <= bottom {
            let a = Rect { y, height: 3, width: half.saturating_sub(1), ..inner };
            let b = Rect { y, height: 3, x: inner.x + half, width: inner.width - half, ..inner };
            let ri = ui::frame(f, a, "max turns (claude)", None, e.field == 5, t);
            super::view::draw_input(f, ri, &e.turns, "40", e.field == 5, t);
            let ri = ui::frame(f, b, "budget $ per task", None, e.field == 6, t);
            super::view::draw_input(f, ri, &e.budget, "1.50", e.field == 6, t);
            y += 3;
        }
        if y < bottom {
            let btn = if e.field == 7 { Span::styled(" save ", Style::default().fg(Color::Black).bg(t.accent).add_modifier(Modifier::BOLD)) } else { Span::styled("[save]", bold(t.accent)) };
            let mut spans = vec![btn];
            if !e.err.is_empty() {
                spans.push(Span::styled(format!("   {}", e.err), Style::default().fg(t.danger)));
            }
            f.render_widget(Paragraph::new(Line::from(spans)).centered(), Rect { y: y + 1, height: 1, ..inner });
        }
        f.render_widget(Paragraph::new(Span::styled("tab next · ctrl+s save · esc cancel", ui::muted(t))).centered(), Rect { y: bottom.saturating_sub(1), height: 1, ..inner });
    }

    // ---------------------------------------------------------------- watch: the run, tiled

    /// The watch view's tiles: the lead, then its workers (running first).
    pub(super) fn watch_tiles(&self, run: &str) -> Vec<LogTarget> {
        let mut tasks: Vec<&Task> = self.run_tasks(run).into_iter().filter(|t| !(t.status == Status::Done && t.outcome == "discarded")).collect();
        let rank = |t: &Task| match t.status {
            Status::Running | Status::Blocked => 0,
            Status::Review => 1,
            Status::Todo => 2,
            Status::Done => 3,
        };
        tasks.sort_by_key(|t| (rank(t), std::cmp::Reverse(t.started), t.created));
        std::iter::once(LogTarget::Lead(run.to_string())).chain(tasks.into_iter().take(8).map(|t| LogTarget::Task(t.id.clone()))).collect()
    }

    pub(super) fn open_tile(&mut self, i: usize) {
        let Mode::Watch(w) = &self.mode else { return };
        let run = w.run.clone();
        if let Some(target) = self.watch_tiles(&run).get(i).cloned() {
            self.mode = Mode::Log(LogView { target, scroll: 0, back: Some(run) });
        }
    }

    fn grid(n: usize, area: Rect) -> Vec<Rect> {
        let cols = match n {
            0..=1 => 1,
            2..=4 => 2,
            _ => 3,
        };
        let cols = if area.width < 90 { cols.min(2) } else { cols };
        let rows = n.div_ceil(cols).max(1);
        // the first row (the lead and the busiest workers) gets more height than the rest
        let heights: Vec<u16> = match rows {
            1 => vec![area.height],
            2 => {
                let a = area.height * 3 / 5;
                vec![a, area.height - a]
            }
            _ => {
                let a = area.height * 2 / 5;
                let rest = (area.height - a) / (rows as u16 - 1);
                std::iter::once(a).chain((1..rows).map(|r| if r == rows - 1 { area.height - a - rest * (rows as u16 - 2) } else { rest })).collect()
            }
        };
        let mut out = vec![];
        for i in 0..n {
            let (r, c) = (i / cols, i % cols);
            // the last row stretches when it isn't full
            let in_row = if r == rows - 1 { n - r * cols } else { cols };
            let cw = area.width / in_row as u16;
            let x = area.x + c as u16 * cw;
            let y = area.y + heights[..r].iter().sum::<u16>();
            let w = if c == in_row - 1 { area.right() - x } else { cw };
            out.push(Rect { x, y, width: w, height: heights[r] });
        }
        out
    }

    pub(super) fn watch_key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        let Mode::Watch(w) = &mut self.mode else { return false };
        let run = w.run.clone();
        let n = self.watch_tiles(&run).len();
        let Mode::Watch(w) = &mut self.mode else { return false };
        let cols = match n {
            0..=1 => 1,
            2..=4 => 2,
            _ => 3,
        };
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('w') => self.mode = Mode::Board,
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => w.sel = (w.sel + 1) % n.max(1),
            KeyCode::Left | KeyCode::Char('h') | KeyCode::BackTab => w.sel = (w.sel + n.max(1) - 1) % n.max(1),
            KeyCode::Down | KeyCode::Char('j') => w.sel = (w.sel + cols).min(n.saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => w.sel = w.sel.saturating_sub(cols),
            KeyCode::Enter => {
                let i = w.sel;
                self.open_tile(i);
            }
            KeyCode::Char('t') | KeyCode::Char('d') => {
                let i = w.sel;
                if let Some(LogTarget::Task(id)) = self.watch_tiles(&run).get(i).cloned() {
                    if k.code == KeyCode::Char('t') {
                        self.take_over(&id, cx);
                    } else {
                        self.open_diff(&id, cx);
                    }
                }
            }
            KeyCode::Char('s') => {
                if self.run_ref(&run).is_some_and(|r| r.state.active()) {
                    self.mode = Mode::Confirm(super::Confirm { id: run, what: super::Pending::StopRun });
                }
            }
            _ => {}
        }
        true
    }

    /// (title, subtitle, entries, focus colour) for a tile or transcript.
    fn target_info(&self, target: &LogTarget, cx: &Cx) -> (String, String, Vec<Entry>, Color) {
        let t = cx.theme;
        let spin = SPIN[(cx.time * 10.0) as usize % 10];
        match target {
            LogTarget::Lead(run) => {
                let r = self.run_ref(run).cloned().unwrap_or_default();
                let log = self.runs_live.get(run).map(|l| l.log.clone()).unwrap_or_default();
                let glyph = if r.state.active() { spin } else { "⚑" };
                (
                    format!("{glyph} lead · {} {}", r.agent, if r.model.is_empty() { "" } else { &r.model }),
                    format!("{} · {}", state_label(&r), money(r.cost_usd)),
                    if log.is_empty() { r.log.iter().map(|l| Entry::note(l)).collect() } else { log },
                    t.shine,
                )
            }
            LogTarget::Task(id) => {
                let Some(task) = self.task(id) else { return (id.clone(), String::new(), vec![], t.frame) };
                let l = self.live.get(id);
                let glyph = match task.status {
                    Status::Running | Status::Blocked if task.headless() => spin,
                    Status::Running | Status::Blocked => "⧉",
                    Status::Review if task.blocked || !task.error.is_empty() => "▲",
                    Status::Review => "◆",
                    Status::Todo => "○",
                    Status::Done if task.outcome == "merged" => "✓",
                    Status::Done => "✗",
                };
                let color = match task.status {
                    Status::Running | Status::Blocked => t.accent,
                    Status::Review if task.blocked || !task.error.is_empty() => t.danger,
                    Status::Review => t.shine,
                    Status::Done => t.good,
                    Status::Todo => t.muted,
                };
                let name = if task.worker.is_empty() { task.agent.clone() } else { task.worker.clone() };
                let tier = if task.tier.is_empty() { String::new() } else { format!(" · {}", task.tier) };
                let state = super::lead::state_name(task);
                let mut log = l.map(|l| l.log.clone()).unwrap_or_default();
                if log.is_empty() {
                    log = task.recent.iter().map(|r| Entry::note(r)).collect();
                    if log.is_empty() && task.status == Status::Todo {
                        let why = if !task.depends_on.is_empty() { format!("queued · starts once {} merged", task.depends_on.join(" + ")) } else { "queued · waiting for a free worker slot".into() };
                        log.push(Entry::note(&why));
                        for l in task.prompt.lines().filter(|l| !l.trim().is_empty()).take(6) {
                            log.push(Entry::say(l));
                        }
                    }
                }
                (format!("{glyph} {name}{tier} · {}", task.title), format!("{state} · {}", money(task.cost_usd)), log, color)
            }
        }
    }

    pub(super) fn draw_watch(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        let Mode::Watch(w) = &self.mode else { return };
        let (run_id, sel) = (w.run.clone(), w.sel);
        let Some(run) = self.run_ref(&run_id).cloned() else {
            self.mode = Mode::Board;
            return;
        };
        let hints = [("←→↑↓", "move"), ("enter", "transcript"), ("t", "take over"), ("d", "diff"), ("s", "stop run"), ("esc", "board")];
        let body = ui::hint_line(f, area, &hints, t);
        let body = Rect { x: body.x + 1, width: body.width.saturating_sub(2), ..body };
        let spend = self.spend(&run.id);
        let head = spread(
            vec![Span::styled("⚑ watch  ", bold(t.accent)), Span::styled(run.goal.lines().next().unwrap_or("").to_string(), bold(t.fg))],
            vec![Span::styled(format!("{}  ", state_label(&run)), ui::muted(t)), Span::styled(money(spend), bold(t.fg)), Span::styled(format!(" of {}", money(run.budget_usd)), ui::muted(t))],
            body.width as usize,
        );
        f.render_widget(Paragraph::new(head), Rect { height: 1, ..body });
        let tiles = self.watch_tiles(&run.id);
        let sel = sel.min(tiles.len().saturating_sub(1));
        if let Mode::Watch(w) = &mut self.mode {
            w.sel = sel;
        }
        let grid_area = Rect { y: body.y + 2, height: body.height.saturating_sub(2), ..body };
        let spin = SPIN[(cx.time * 10.0) as usize % 10];
        for (i, (target, r)) in tiles.iter().zip(Self::grid(tiles.len(), grid_area)).enumerate() {
            let (title, sub, log, color) = self.target_info(target, cx);
            let on = i == sel;
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(if on { t.accent } else { mix(t.frame, color, 0.45) }))
                .title(Line::from(Span::styled(format!(" {} ", ui::fit(&title, r.width.saturating_sub(4) as usize)), bold(if on { t.accent } else { color }))))
                .title_bottom(Line::from(Span::styled(format!(" {sub} "), ui::muted(t))).right_aligned());
            let inner = block.inner(r);
            f.render_widget(block, r);
            self.hits.push((r, Hit::Tile(i)));
            let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
            let w = inner.width as usize;
            // newest at the bottom; the last entry gets its body (the diff or output it's on right now)
            let mut lines: Vec<Line> = vec![];
            let n = log.len();
            for (k, e) in log.iter().enumerate() {
                lines.extend(entry_lines(e, w, k + 1 == n && e.kind == 't', t, spin));
            }
            let h = inner.height as usize;
            let skip = lines.len().saturating_sub(h);
            f.render_widget(Paragraph::new(lines.into_iter().skip(skip).collect::<Vec<_>>()), inner);
        }
    }

    // ---------------------------------------------------------------- one transcript

    pub(super) fn log_key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        let Mode::Log(v) = &mut self.mode else { return false };
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = match v.back.take() {
                    Some(run) => Mode::Watch(WatchView { sel: 0, run }),
                    None => Mode::Board,
                }
            }
            KeyCode::Up | KeyCode::Char('k') => v.scroll += 1,
            KeyCode::Down | KeyCode::Char('j') => v.scroll = v.scroll.saturating_sub(1),
            KeyCode::PageUp => v.scroll += 20,
            KeyCode::PageDown | KeyCode::Char(' ') => v.scroll = v.scroll.saturating_sub(20),
            KeyCode::End | KeyCode::Char('G') => v.scroll = 0,
            KeyCode::Char('t') => {
                if let LogTarget::Task(id) = v.target.clone() {
                    self.take_over(&id, cx);
                }
            }
            KeyCode::Char('d') => match v.target.clone() {
                LogTarget::Task(id) => self.open_diff(&id, cx),
                LogTarget::Lead(run) => self.open_run_diff(&run, cx),
            },
            KeyCode::Char('c') => {
                if let LogTarget::Task(id) = v.target.clone() {
                    let ok = self.task(&id).is_some_and(|t| !t.worktree.is_empty() && t.status == Status::Review);
                    if ok {
                        self.mode = Mode::Comment(id, Input::new("", true));
                    }
                }
            }
            _ => {}
        }
        true
    }

    pub(super) fn draw_log(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        let Mode::Log(v) = &self.mode else { return };
        let (target, scroll) = (v.target.clone(), v.scroll);
        let (title, sub, log, color) = self.target_info(&target, cx);
        let task = if let LogTarget::Task(id) = &target { self.task(id).cloned() } else { None };
        let hints: Vec<(&str, &str)> = match &task {
            Some(tk) if tk.headless() && !tk.worktree.is_empty() => vec![("↑↓", "scroll"), ("t", "take over in a tab"), ("d", "diff"), ("c", "follow-up"), ("esc", "back")],
            Some(_) => vec![("↑↓", "scroll"), ("d", "diff"), ("esc", "back")],
            None => vec![("↑↓", "scroll"), ("d", "integration diff"), ("esc", "back")],
        };
        let body = ui::hint_line(f, area, &hints, t);
        let body = Rect { x: body.x + 1, width: body.width.saturating_sub(2), ..body };
        let spin = SPIN[(cx.time * 10.0) as usize % 10];
        let inner = ui::frame(f, body, &title, Some(&sub), true, t);
        let _ = color;
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
        let w = inner.width as usize;
        let mut lines: Vec<Line> = vec![];
        // what it was asked, first
        match (&target, &task) {
            (LogTarget::Task(_), Some(tk)) => {
                let mut facts = vec![Span::styled(format!("{}{} ", ui::lead(agent_icon(&tk.agent)), tk.agent), bold(t.fg))];
                if !tk.model.is_empty() {
                    facts.push(Span::styled(format!("{} ", tk.model), ui::muted(t)));
                }
                if !tk.owns.is_empty() {
                    facts.push(Span::styled(format!("· owns {} ", tk.owns.join(", ")), ui::muted(t)));
                }
                if !tk.acceptance.is_empty() {
                    facts.push(Span::styled(format!("· accept: {}", tk.acceptance), ui::muted(t)));
                }
                lines.push(Line::from(facts));
                for l in tk.prompt.lines().filter(|l| !l.trim().is_empty()).take(4) {
                    lines.push(Line::styled(ui::fit(l, w), ui::muted(t).add_modifier(Modifier::ITALIC)));
                }
                if !tk.gate.is_empty() && tk.gate != "pass" {
                    lines.push(Line::styled(ui::fit(&format!("gate: {}", tk.gate.lines().next().unwrap_or("")), w), Style::default().fg(t.danger)));
                }
                lines.push(Line::styled("─".repeat(w), Style::default().fg(t.frame)));
            }
            (LogTarget::Lead(run), _) => {
                if let Some(r) = self.run_ref(run) {
                    lines.push(Line::styled(ui::fit(&r.goal, w), bold(t.fg)));
                    lines.push(Line::styled("─".repeat(w), Style::default().fg(t.frame)));
                }
            }
            _ => {}
        }
        for e in &log {
            lines.extend(entry_lines(e, w, true, t, spin));
        }
        let h = inner.height as usize;
        let max = lines.len().saturating_sub(h);
        let scroll = scroll.min(max);
        if let Mode::Log(v) = &mut self.mode {
            v.scroll = scroll;
        }
        let start = max - scroll;
        f.render_widget(Paragraph::new(lines.into_iter().skip(start).take(h).collect::<Vec<_>>()), inner);
        if scroll > 0 {
            let r = Rect { x: body.x + 2, y: body.bottom().saturating_sub(1), width: 24.min(body.width), height: 1 };
            f.render_widget(Paragraph::new(Span::styled(format!(" ↓ {scroll} newer lines "), ui::accent(t))), r);
        }
    }
}
