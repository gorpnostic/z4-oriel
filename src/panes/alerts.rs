//! alerts: the event center. On top, what's open right now and waiting on you (a chat's question or approval,
//! a stuck or finished task, a terminal agent at a prompt), answered from here: space peeks, 1-9 or y / n
//! answers, enter goes there. Under it, what happened while you were busy, newest first.

use crate::alerts::{self, Alert, Kind, Open, Reply};
use crate::pane::{Action, Cx, Pane};
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

/// A row, by what's on it rather than where: open items and alerts come and go, and the pick must stay on the
/// same one, or a quick x or 1 would land on a different one.
#[derive(Clone, PartialEq, Debug)]
enum Pick {
    Open(u64, String),
    Alert(alerts::Key),
}

pub struct Alerts {
    /// the picked row: the open items first, then the history newest first...
    sel: usize,
    /// ...and which one that is
    pick: Option<Pick>,
    /// space: the picked row shows its whole question or result card
    peek: bool,
    rows: Vec<(Rect, usize)>,
}

impl Alerts {
    pub fn new() -> Alerts {
        Alerts { sel: 0, pick: None, peek: false, rows: vec![] }
    }

    /// Find the picked row again; if it's gone (answered, dismissed), stay on the row that took its place.
    /// Row r is open[r] for the open items, then all[n - 1 - (r - m)] (the history is newest first).
    fn settle(&mut self, open: &[Open], all: &[Alert]) {
        let (m, n) = (open.len(), all.len());
        let found = match &self.pick {
            Some(Pick::Open(p, k)) => open.iter().position(|o| o.pane == *p && o.key == *k),
            Some(Pick::Alert(key)) => all.iter().rposition(|a| alerts::key(a) == *key).map(|i| m + n - 1 - i),
            None => None,
        };
        if let Some(r) = found {
            self.sel = r;
        }
        self.sel = self.sel.min((m + n).saturating_sub(1));
        self.pick = if self.sel < m {
            Some(Pick::Open(open[self.sel].pane, open[self.sel].key.clone()))
        } else {
            (self.sel - m < n).then(|| Pick::Alert(alerts::key(&all[n - 1 - (self.sel - m)])))
        };
    }

    /// The pick moved to row `sel`: remember which one is there now.
    fn repick(&mut self) {
        self.pick = None;
        both(|open, all| self.settle(open, all));
    }
}

/// The open items and the history together.
fn both<R>(f: impl FnOnce(&[Open], &[Alert]) -> R) -> R {
    alerts::with_open(|open| alerts::with(|all| f(open, all)))
}

/// Each kind's colour (the list here, and the toasts).
pub(crate) fn color(k: Kind, t: &crate::theme::Theme) -> Color {
    match k {
        Kind::AgentDone | Kind::Download | Kind::Update => t.good,
        Kind::NeedsYou | Kind::BuildFailed | Kind::Memory => t.danger,
        Kind::Approval => t.shine,
        Kind::Calendar => t.accent,
        Kind::Usage => t.inline,
    }
}

/// A coding agent's plan window nearly used up ("claude: 92% of your 5-hour limit used"): the other one, which
/// `h` hands the chat on screen to (/handoff in the chat).
fn handoff_to(a: &alerts::Alert) -> Option<&'static str> {
    if a.kind != Kind::Usage {
        return None;
    }
    match a.text.split(':').next()? {
        "claude" => Some("codex"),
        "codex" => Some("claude"),
        _ => None,
    }
}

/// The app an alert jumps to, as the sidebar names it.
fn app_of(name: &str) -> Option<&'static str> {
    crate::app::SIDEBAR.iter().map(|a| a.0).chain(["updates"]).find(|a| *a == name)
}

/// What answers an open item, for its row and its peek: ("1-3", "answer"), ("y / n", "allow · deny")...
fn answers(o: &Open) -> Option<(String, &'static str)> {
    let n = o.options.len().min(9);
    match (n, o.multi, o.yes_no) {
        (0, _, true) => Some(("y / n".into(), "allow · deny")),
        (0, _, false) => None,
        (1, false, _) => Some(("1".into(), "answer")),
        (_, false, _) => Some((format!("1-{n}"), "answer")),
        (_, true, _) => Some((format!("1-{n} tick · y"), "send")),
    }
}

/// Word-wrap to `w` columns (long words are cut).
fn wrap(s: &str, w: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthStr;
    let w = w.max(8);
    let mut out = vec![];
    let mut cur = String::new();
    for word in s.split(' ') {
        let (cw, ww) = (cur.width(), word.width());
        if cw > 0 && cw + 1 + ww > w {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
        while cur.width() > w {
            let cut: String = cur.chars().scan(0, |acc, c| {
                *acc += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                (*acc <= w).then_some(c)
            }).collect();
            let rest = cur[cut.len()..].to_string();
            out.push(cut);
            cur = rest;
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

/// A peek's lines under its row: the text behind a bar, then the choices, then how to answer.
fn peek_lines(o: &Open, w: usize, t: &crate::theme::Theme) -> Vec<Line<'static>> {
    const MOST: usize = 14;
    let bar = || Span::styled("    │ ", ui::muted(t));
    let mut out: Vec<Line> = vec![];
    let text: Vec<String> = o.detail.iter().flat_map(|l| if l.is_empty() { vec![String::new()] } else { wrap(l, w.saturating_sub(8)) }).collect();
    let cut = text.len() > MOST;
    for l in text.into_iter().take(MOST) {
        out.push(Line::from(vec![bar(), Span::raw(l)]));
    }
    if cut {
        out.push(Line::from(vec![bar(), Span::styled("… enter to see the rest", ui::muted(t))]));
    }
    for (i, opt) in o.options.iter().enumerate().take(9) {
        let tick = if o.multi { if o.ticked.get(i).copied().unwrap_or(false) { "[x] " } else { "[ ] " } } else { "" };
        out.push(Line::from(vec![bar(), Span::styled(format!("{} ", i + 1), ui::bold_accent(t)), Span::raw(format!("{tick}{}", ui::fit(opt, w.saturating_sub(14))))]));
    }
    let mut how = vec![bar()];
    if let Some((k, what)) = answers(o) {
        how.extend([Span::styled(k, ui::bold_accent(t)), Span::styled(format!(" {what} · "), ui::muted(t))]);
    }
    how.extend([Span::styled("enter", Style::default().fg(t.fg).add_modifier(Modifier::BOLD)), Span::styled(" go there", ui::muted(t))]);
    if !o.options.is_empty() {
        how.push(Span::styled(" (to answer in your own words)", ui::muted(t)));
    }
    out.push(Line::from(how));
    out
}

impl Pane for Alerts {
    fn title(&self) -> String {
        "alerts".into()
    }
    fn icon(&self) -> &'static str {
        "bell"
    }
    fn badge(&self) -> Option<String> {
        let waiting = alerts::with_open(|l| l.iter().filter(|o| o.needs_you()).count());
        let n = alerts::unread();
        if waiting > 0 {
            Some(format!("{waiting} need{} you", if waiting == 1 { "s" } else { "" }))
        } else {
            (n > 0).then(|| format!("{n} new"))
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        alerts::sync(); // another oriel window may have added some
        // the open item picked, or for a history row, who a usage alert's chat can be handed to (h)
        let (on_open, to) = both(|open, all| {
            self.settle(open, all);
            let m = open.len();
            let picked = if self.sel >= m { all.len().checked_sub(1 + self.sel - m).map(|i| &all[i]) } else { None };
            (open.get(self.sel).cloned(), picked.and_then(handoff_to))
        });
        let give = to.map(|to| format!("hand the chat to {to}"));
        let mut hints: Vec<(String, &str)> = vec![("↑↓".into(), "pick"), ("space".into(), if self.peek { "hide" } else { "peek" })];
        match &on_open {
            Some(o) => {
                hints.extend(answers(o));
                hints.push(("enter".into(), "go there"));
            }
            None => {
                hints.push(("enter".into(), "go there"));
                if let Some(g) = &give {
                    hints.push(("h".into(), g.as_str()));
                }
                hints.extend([("x".into(), "dismiss"), ("c".into(), "clear all")]);
            }
        }
        let hints: Vec<(&str, &str)> = hints.iter().map(|(k, w)| (k.as_str(), *w)).collect();
        let area = ui::hint_line(f, area, &hints, t);
        let body = Rect { x: area.x + 1, y: area.y + 1, width: area.width.saturating_sub(2), height: area.height.saturating_sub(1) };
        self.rows.clear();
        let w = body.width as usize;
        let any = both(|open, all| {
            if open.is_empty() && all.is_empty() {
                let lines = vec![
                    Line::from(Span::styled("nothing yet", ui::bold_accent(t))),
                    Line::raw(""),
                    Line::from(Span::styled("What's waiting on you right now (a chat's question or approval, a stuck agent, work to review) shows", ui::muted(t))),
                    Line::from(Span::styled("on top. Under it: agents finishing, failed builds and merge checks, calendar reminders, finished", ui::muted(t))),
                    Line::from(Span::styled("installs, memory and AI-usage warnings and new versions.", ui::muted(t))),
                ];
                f.render_widget(Paragraph::new(lines), body);
                return false;
            }
            // every line to draw, with the row it belongs to (for the pick and the mouse)
            let mut lines: Vec<(Line, Option<usize>)> = vec![];
            let m = open.len();
            if m > 0 {
                let need = open.iter().filter(|o| o.needs_you()).count();
                let mut counts = vec![];
                if need > 0 {
                    counts.push(format!("{need} need{} you", if need == 1 { "s" } else { "" }));
                }
                if m > need {
                    counts.push(format!("{} to review", m - need));
                }
                lines.push((Line::from(vec![Span::styled("open now", ui::bold_accent(t)), Span::styled(format!("  {}", counts.join(" · ")), ui::muted(t))]), None));
                for (row, o) in open.iter().enumerate() {
                    let (glyph, name) = o.label();
                    let on = row == self.sel;
                    let hint = answers(o).map(|(k, _)| format!("  {k}")).unwrap_or_default();
                    let tw = w.saturating_sub(30 + hint.chars().count());
                    lines.push((
                        Line::from(vec![
                            Span::styled(if on { "▸ " } else { "  " }, ui::bold_accent(t)),
                            Span::styled(format!("{glyph} {name:<10}"), Style::default().fg(color(o.kind, t)).add_modifier(Modifier::BOLD)),
                            // the tab it's in, where the history says how long ago
                            Span::styled(format!("  {:<10}  ", ui::fit(&o.from, 10)), ui::muted(t)),
                            Span::styled(ui::fit(&o.text, tw), if on { Style::default().add_modifier(Modifier::BOLD) } else { Style::default() }),
                            Span::styled(hint, ui::muted(t)),
                        ]),
                        Some(row),
                    ));
                    if on && self.peek {
                        lines.extend(peek_lines(o, w, t).into_iter().map(|l| (l, Some(row))));
                    }
                }
                if !all.is_empty() {
                    lines.push((Line::raw(""), None));
                    lines.push((Line::from(Span::styled("history", ui::bold_accent(t))), None));
                }
            }
            for (h, i) in (0..all.len()).rev().enumerate() {
                let (a, row) = (&all[i], m + h);
                let (glyph, name) = a.kind.label();
                let on = row == self.sel;
                let c = color(a.kind, t);
                let text_st = if on { Style::default().add_modifier(Modifier::BOLD) } else if a.read { ui::muted(t) } else { Style::default() };
                let from = a.app.as_deref().map(|s| format!("  {s}")).unwrap_or_default();
                let tw = w.saturating_sub(29 + from.len());
                lines.push((
                    Line::from(vec![
                        Span::styled(if on { "▸ " } else { "  " }, ui::bold_accent(t)),
                        Span::styled(format!("{glyph} {name:<10}"), Style::default().fg(c).add_modifier(if a.read { Modifier::empty() } else { Modifier::BOLD })),
                        Span::styled(format!(" {:>8}  ", alerts::ago(a.at)), ui::muted(t)),
                        Span::styled(ui::fit(&a.text, tw), text_st),
                        Span::styled(from, ui::muted(t)),
                    ]),
                    Some(row),
                ));
                if on && self.peek && ui::fit(&a.text, tw) != a.text {
                    // the whole text of one that didn't fit
                    for l in wrap(&a.text, w.saturating_sub(8)) {
                        lines.push((Line::from(vec![Span::styled("    │ ", ui::muted(t)), Span::raw(l)]), Some(row)));
                    }
                }
            }
            // scroll so the picked row (and its peek) is in view
            let h = body.height as usize;
            let first = lines.iter().position(|l| l.1 == Some(self.sel)).unwrap_or(0);
            let last = lines.iter().rposition(|l| l.1 == Some(self.sel)).unwrap_or(0);
            let top = if last - first + 1 > h { first } else { last.saturating_sub(h.saturating_sub(1)) };
            for (k, (line, row)) in lines.into_iter().skip(top).take(h).enumerate() {
                let r = Rect { y: body.y + k as u16, height: 1, ..body };
                f.render_widget(Paragraph::new(line), r);
                if let Some(row) = row {
                    self.rows.push((r, row));
                }
            }
            !all.is_empty()
        });
        // you've seen them now
        if any && cx.focused {
            alerts::mark_all_read();
        }
    }

    fn key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        let (n, open, picked) = both(|open, all| {
            self.settle(open, all);
            let m = open.len();
            let picked = if self.sel >= m { all.len().checked_sub(1 + self.sel - m).map(|i| all[i].clone()) } else { None };
            (m + all.len(), open.get(self.sel).cloned(), picked)
        });
        let respond = |cx: &mut Cx, o: &Open, r: Reply| cx.act(Action::Respond(o.pane, o.key.clone(), r));
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.sel = (self.sel + 1).min(n.saturating_sub(1));
                self.repick();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.sel = self.sel.saturating_sub(1);
                self.repick();
            }
            KeyCode::Char(' ') => self.peek = !self.peek,
            KeyCode::Char(c @ '1'..='9') => match &open {
                Some(o) if !o.options.is_empty() => {
                    let i = c as usize - '1' as usize;
                    if i < o.options.len() {
                        respond(cx, o, Reply::Pick(i));
                    } else {
                        cx.notify(format!("there are {} choices", o.options.len()));
                    }
                }
                Some(o) if o.yes_no => cx.notify("y allows · n denies"),
                _ => cx.notify("nothing to answer there · enter goes to it"),
            },
            KeyCode::Char(c @ ('y' | 'n')) => match &open {
                Some(o) if o.yes_no => respond(cx, o, if c == 'y' { Reply::Yes } else { Reply::No }),
                Some(o) if o.multi && c == 'y' => {
                    if o.ticked.iter().any(|t| *t) {
                        respond(cx, o, Reply::Yes);
                    } else {
                        cx.notify("tick a choice first (1-9)");
                    }
                }
                Some(o) if !o.options.is_empty() => cx.notify(format!("pick with 1-{}", o.options.len().min(9))),
                _ => cx.notify("nothing to answer there · enter goes to it"),
            },
            KeyCode::Enter => {
                if let Some(o) = &open {
                    respond(cx, o, Reply::Go);
                } else if let Some(a) = picked {
                    let app = a.app.as_deref().and_then(app_of);
                    match (a.pane, app) {
                        // its pane, or its app if that pane has closed since
                        (Some(p), Some(app)) => cx.act(Action::FocusPaneOr(p, app)),
                        (Some(p), None) => cx.act(Action::FocusPane(p)),
                        (None, Some(app)) => cx.act(Action::GotoApp(app)),
                        (None, None) => cx.notify("that one has nowhere to go"),
                    }
                }
            }
            // a usage alert: the chat carries on with the other coding agent (/handoff, in the box for you to send)
            KeyCode::Char('h') => match picked.as_ref().and_then(handoff_to) {
                Some(to) => cx.act(Action::AppPaste("ai", format!("/handoff {to}"))),
                None => return false,
            },
            KeyCode::Char('x') | KeyCode::Delete => {
                if open.is_some() {
                    cx.notify("it goes once it's answered or looked at · enter goes to it");
                } else if let Some(a) = picked {
                    alerts::dismiss(&alerts::key(&a));
                    self.repick(); // the row that took its place
                }
            }
            KeyCode::Char('c') => {
                alerts::clear();
                self.sel = 0;
                self.pick = None;
            }
            _ => return false,
        }
        true
    }

    fn mouse(&mut self, ev: MouseEvent, _area: Rect, _cx: &mut Cx) {
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let pos = Position { x: ev.column, y: ev.row };
                if let Some(&(_, r)) = self.rows.iter().find(|(rect, _)| rect.contains(pos)) {
                    self.sel = r;
                    self.repick();
                }
            }
            MouseEventKind::ScrollDown => {
                self.sel += 1;
                self.repick();
            }
            MouseEventKind::ScrollUp => {
                self.sel = self.sel.saturating_sub(1);
                self.repick();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Kit;

    fn push(at: i64, k: Kind, s: &str, app: Option<&str>, pane: Option<u64>) {
        alerts::push(Alert { at: alerts::now() - at, kind: k, text: s.into(), app: app.map(String::from), read: false, pane });
    }

    #[test]
    fn alerts_center() {
        alerts::clear();
        for (k, s, app) in [(Kind::AgentDone, "claude code finished · fix tests", Some("ai")), (Kind::BuildFailed, "w1: add parser failed", Some("agents")), (Kind::Calendar, "in 10 min: 14:00 dentist", Some("calendar"))] {
            push(120, k, s, app, None);
        }
        assert_eq!(alerts::unread(), 3);
        let mut k = Kit::new();
        let mut p = Alerts::new();
        let s = k.render_html(&mut p, 120, 20, "target/snap/alerts.html");
        assert!(s.contains("dentist") && s.contains("failed") && s.contains("2m"), "{s}");
        assert!(!s.contains("open now"), "nothing open: just the history, as before");
        let first = s.find("dentist").unwrap();
        assert!(first < s.find("fix tests").unwrap(), "newest first");
        assert_eq!(alerts::unread(), 0, "looking at them marks them read");
        k.key(&mut p, KeyCode::Enter);
        assert!(k.actions.iter().any(|a| matches!(a, Action::GotoApp("calendar"))));
        k.key(&mut p, KeyCode::Char('x'));
        assert_eq!(alerts::with(|l| l.len()), 2);
        k.key(&mut p, KeyCode::Char('c'));
        assert_eq!(alerts::with(|l| l.len()), 0);
    }

    #[test]
    fn alerts_pick_follows_the_alert() {
        alerts::clear();
        push(300, Kind::AgentDone, "oldest", None, None);
        push(200, Kind::BuildFailed, "middle", Some("agents"), Some(42));
        push(100, Kind::Calendar, "newest", None, None);
        let mut k = Kit::new();
        let mut p = Alerts::new();
        k.render(&mut p, 120, 20);
        k.key(&mut p, KeyCode::Down); // "middle"
        // an agent finishes: every row shifts down one
        push(0, Kind::AgentDone, "brand new", None, None);
        k.render(&mut p, 120, 20);
        // enter goes to middle's pane, or its app if the pane has closed
        k.key(&mut p, KeyCode::Enter);
        assert!(k.actions.iter().any(|a| matches!(a, Action::FocusPaneOr(42, "agents"))));
        // x dismisses middle, not whatever row 1 is now
        k.key(&mut p, KeyCode::Char('x'));
        let left = alerts::with(|l| l.iter().map(|a| a.text.clone()).collect::<Vec<_>>());
        assert_eq!(left, ["oldest", "newest", "brand new"]);
        // and the pick moves to the row that took its place
        k.key(&mut p, KeyCode::Char('x'));
        assert_eq!(alerts::with(|l| l.iter().map(|a| a.text.clone()).collect::<Vec<_>>()), ["newest", "brand new"]);
    }

    fn open_item(kind: Kind, pane: u64, key: &str, text: &str, from: &str) -> Open {
        let mut o = Open::new(kind, key, text);
        o.pane = pane;
        o.from = from.into();
        o
    }

    fn responses(k: &Kit) -> Vec<(u64, String, Reply)> {
        k.actions.iter().filter_map(|a| if let Action::Respond(p, key, r) = a { Some((*p, key.clone(), *r)) } else { None }).collect()
    }

    /// The open-now list: live items on top of the history, peeked with space, answered with 1-9 or y / n, and
    /// enter goes to them. The pick stays on the item it was on as others come and go.
    #[test]
    fn alerts_open_now_answers_in_place() {
        alerts::clear();
        push(600, Kind::Calendar, "in 10 min: standup", Some("calendar"), None);
        let mut q = open_item(Kind::Approval, 7, "q:Which crate for the parser?", "claude asks: Which crate for the parser?", "ai");
        q.detail = vec!["Which crate for the parser?".into(), "It's for the config loader; both are maintained.".into()];
        q.options = vec!["nom".into(), "winnow".into(), "hand-written".into()];
        let mut ask = open_item(Kind::Approval, 7, "ask:Bash:cargo publish", "claude wants to: Bash cargo publish", "ai");
        ask.yes_no = true;
        let mut review = open_item(Kind::AgentDone, 9, "task:t1", "add parser is ready for review", "agents");
        review.detail = vec!["claude · +120 −8 · 3 files · $0.42".into(), "gate: pass".into()];
        let term = open_item(Kind::NeedsYou, 12, "term", "pwsh is waiting for you", "pwsh");
        alerts::set_open(vec![review.clone(), q.clone(), ask.clone(), term.clone()]);
        let mut k = Kit::new();
        let mut p = Alerts::new();
        let s = k.render_html(&mut p, 120, 24, "target/snap/alerts-open.html");
        assert!(s.contains("open now") && s.contains("3 need you · 1 to review") && s.contains("history"), "{s}");
        let y = |s: &str, needle: &str| s.lines().position(|l| l.contains(needle)).unwrap_or_else(|| panic!("no {needle} in\n{s}"));
        assert!(y(&s, "Which crate") < y(&s, "cargo publish") && y(&s, "pwsh is waiting") < y(&s, "ready for review") && y(&s, "ready for review") < y(&s, "standup"), "waiting on you, then review, then history: {s}");
        assert!(s.contains("1-3") && s.contains("y / n"), "each row says how it's answered: {s}");
        // space peeks: the whole question and its choices
        k.key(&mut p, KeyCode::Char(' '));
        let s = k.render_html(&mut p, 120, 24, "target/snap/alerts-peek.html");
        assert!(s.contains("both are maintained") && s.contains("2 winnow") && s.contains("go there"), "{s}");
        // a narrow pane: cut short, never a panic, the picked row still in view
        let s = k.render(&mut p, 34, 8);
        assert!(s.contains("asking"), "{s}");
        // a number answers it, through the pane that holds it
        k.key(&mut p, KeyCode::Char('2'));
        assert_eq!(responses(&k), [(7, "q:Which crate for the parser?".to_string(), Reply::Pick(1))]);
        k.key(&mut p, KeyCode::Char('7'));
        assert!(k.notices().last().unwrap().contains("3 choices"));
        // it's answered: gone from the list, and the pick moves to what took its place (the approval)
        alerts::set_open(vec![review.clone(), ask.clone(), term.clone()]);
        k.render(&mut p, 120, 24);
        k.actions.clear();
        k.key(&mut p, KeyCode::Char('n'));
        assert_eq!(responses(&k), [(7, "ask:Bash:cargo publish".to_string(), Reply::No)]);
        // a new question arrives above it: the pick stays on the approval
        alerts::set_open(vec![review.clone(), q.clone(), ask.clone(), term.clone()]);
        k.render(&mut p, 120, 24);
        k.actions.clear();
        k.key(&mut p, KeyCode::Char('y'));
        assert_eq!(responses(&k), [(7, "ask:Bash:cargo publish".to_string(), Reply::Yes)]);
        // enter on the review goes there; x on an open item doesn't pretend to dismiss it
        k.key(&mut p, KeyCode::Down);
        k.key(&mut p, KeyCode::Down);
        k.actions.clear();
        k.key(&mut p, KeyCode::Enter);
        assert_eq!(responses(&k), [(9, "task:t1".to_string(), Reply::Go)]);
        k.key(&mut p, KeyCode::Char('x'));
        assert!(k.notices().last().unwrap().contains("once it's answered"));
        k.key(&mut p, KeyCode::Char('1'));
        assert!(k.notices().last().unwrap().contains("nothing to answer"), "a review has no choices");
        // down past the open items: the history works as before
        k.key(&mut p, KeyCode::Down);
        k.actions.clear();
        k.key(&mut p, KeyCode::Enter);
        assert!(k.actions.iter().any(|a| matches!(a, Action::GotoApp("calendar"))));
        alerts::set_open(vec![]);
    }

    /// Several can be ticked: numbers tick (the chat keeps the ticks), y sends them.
    #[test]
    fn alerts_open_now_multi_choice() {
        alerts::clear();
        let mut q = open_item(Kind::Approval, 3, "q:Which are Copy?", "claude asks: Which are Copy?", "ai");
        q.options = vec!["i32".into(), "String".into(), "bool".into()];
        q.multi = true;
        q.ticked = vec![false; 3];
        alerts::set_open(vec![q.clone()]);
        let mut k = Kit::new();
        let mut p = Alerts::new();
        k.key(&mut p, KeyCode::Char('y'));
        assert!(responses(&k).is_empty() && k.notices().last().unwrap().contains("tick a choice first"));
        k.key(&mut p, KeyCode::Char('1'));
        assert_eq!(responses(&k), [(3, "q:Which are Copy?".to_string(), Reply::Pick(0))]);
        q.ticked = vec![true, false, false];
        alerts::set_open(vec![q]);
        k.key(&mut p, KeyCode::Char(' '));
        let s = k.render(&mut p, 100, 16);
        assert!(s.contains("[x] i32") && s.contains("[ ] String") && s.contains("1-3 tick · y send"), "{s}");
        k.actions.clear();
        k.key(&mut p, KeyCode::Char('y'));
        assert_eq!(responses(&k), [(3, "q:Which are Copy?".to_string(), Reply::Yes)]);
        alerts::set_open(vec![]);
    }

    #[test]
    fn alerts_wrap_long_text() {
        assert_eq!(wrap("the quick brown fox jumps", 10), ["the quick", "brown fox", "jumps"]);
        assert_eq!(wrap("abcdefghijklmnop", 8), ["abcdefgh", "ijklmnop"]);
        assert_eq!(wrap("", 10), [""]);
    }

    /// h on a usage alert about one coding agent puts /handoff to the other one in the chat's box; on any other
    /// alert it does nothing.
    #[test]
    fn alerts_usage_hands_off() {
        alerts::clear();
        push(60, Kind::AgentDone, "claude code finished", Some("ai"), None);
        push(0, Kind::Usage, "claude: 92% of your 5-hour limit used, resets in 1h 5m", Some("ais"), None);
        let mut k = Kit::new();
        let mut p = Alerts::new();
        let s = k.render(&mut p, 120, 12);
        assert!(s.contains("hand the chat to codex"), "{s}");
        k.key(&mut p, KeyCode::Char('h'));
        assert!(k.actions.iter().any(|a| matches!(a, Action::AppPaste("ai", t) if t == "/handoff codex")), "{:?}", k.notices());
        k.actions.clear();
        k.key(&mut p, KeyCode::Down);
        k.key(&mut p, KeyCode::Char('h'));
        assert!(k.actions.is_empty(), "not a usage alert: nothing to hand off");
        alerts::clear();
    }
}
