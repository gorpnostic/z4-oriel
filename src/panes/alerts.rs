//! alerts: the event center. What happened while you were busy, newest first; enter goes to where it happened.

use crate::alerts::{self, Alert, Kind};
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

pub struct Alerts {
    /// the picked row, newest first...
    sel: usize,
    /// ...and which alert that is: a new one arriving pushes every row down, and the pick must stay on the same
    /// alert, or a quick x would dismiss a different one
    pick: Option<alerts::Key>,
    rows: Vec<(Rect, usize)>,
}

impl Alerts {
    pub fn new() -> Alerts {
        Alerts { sel: 0, pick: None, rows: vec![] }
    }

    /// Find the picked alert again; if it's gone (dismissed), stay on the row that took its place.
    /// Rows are newest first: row r is all[n - 1 - r].
    fn settle(&mut self, all: &[Alert]) {
        let n = all.len();
        if let Some(i) = self.pick.as_ref().and_then(|k| all.iter().rposition(|a| alerts::key(a) == *k)) {
            self.sel = n - 1 - i;
        }
        self.sel = self.sel.min(n.saturating_sub(1));
        self.pick = (n > 0).then(|| alerts::key(&all[n - 1 - self.sel]));
    }

    /// The pick moved to row `sel`: remember which alert is there now.
    fn repick(&mut self) {
        self.pick = None;
        alerts::with(|all| self.settle(all));
    }
}

fn color(k: Kind, t: &crate::theme::Theme) -> Color {
    match k {
        Kind::AgentDone | Kind::Download | Kind::Update => t.good,
        Kind::NeedsYou | Kind::BuildFailed | Kind::Memory => t.danger,
        Kind::Approval => t.shine,
        Kind::Calendar => t.accent,
        Kind::Usage => t.inline,
    }
}

/// The app an alert jumps to, as the sidebar names it.
fn app_of(name: &str) -> Option<&'static str> {
    crate::app::SIDEBAR.iter().map(|a| a.0).chain(["updates"]).find(|a| *a == name)
}

impl Pane for Alerts {
    fn title(&self) -> String {
        "alerts".into()
    }
    fn icon(&self) -> &'static str {
        "bell"
    }
    fn badge(&self) -> Option<String> {
        let n = alerts::unread();
        (n > 0).then(|| format!("{n} new"))
    }

    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        let area = ui::hint_line(f, area, &[("↑↓", "pick"), ("enter", "go there"), ("x", "dismiss"), ("c", "clear all")], t);
        let body = Rect { x: area.x + 1, y: area.y + 1, width: area.width.saturating_sub(2), height: area.height.saturating_sub(1) };
        alerts::sync(); // another oriel window may have added some
        self.rows.clear();
        let any = alerts::with(|all| {
            if all.is_empty() {
                let lines = vec![
                    Line::from(Span::styled("nothing yet", ui::bold_accent(t))),
                    Line::raw(""),
                    Line::from(Span::styled("Agents finishing or needing you, approvals and questions, failed builds and merge checks, calendar", ui::muted(t))),
                    Line::from(Span::styled("reminders, finished installs, memory and AI-usage warnings and new versions land here.", ui::muted(t))),
                ];
                f.render_widget(Paragraph::new(lines), body);
                return false;
            }
            self.settle(all);
            let h = body.height as usize;
            let top = self.sel.saturating_sub(h.saturating_sub(1));
            for (row, i) in (0..all.len()).rev().enumerate().skip(top).take(h) {
                let a = &all[i];
                let (glyph, name) = a.kind.label();
                let on = row == self.sel;
                let c = color(a.kind, t);
                let text_st = if on { Style::default().add_modifier(Modifier::BOLD) } else if a.read { ui::muted(t) } else { Style::default() };
                let from = a.app.as_deref().map(|s| format!("  {s}")).unwrap_or_default();
                let w = (body.width as usize).saturating_sub(28 + from.len());
                let line = Line::from(vec![
                    Span::styled(if on { "▸ " } else { "  " }, ui::bold_accent(t)),
                    Span::styled(format!("{glyph} {name:<10}"), Style::default().fg(c).add_modifier(if a.read { Modifier::empty() } else { Modifier::BOLD })),
                    Span::styled(format!("{:>8}  ", alerts::ago(a.at)), ui::muted(t)),
                    Span::styled(ui::fit(&a.text, w), text_st),
                    Span::styled(from, ui::muted(t)),
                ]);
                let r = Rect { y: body.y + (row - top) as u16, height: 1, ..body };
                f.render_widget(Paragraph::new(line), r);
                self.rows.push((r, row));
            }
            true
        });
        // you've seen them now
        if any && cx.focused {
            alerts::mark_all_read();
        }
    }

    fn key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        let (n, picked) = alerts::with(|all| {
            self.settle(all);
            (all.len(), all.len().checked_sub(1 + self.sel).map(|i| all[i].clone()))
        });
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.sel = (self.sel + 1).min(n.saturating_sub(1));
                self.repick();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.sel = self.sel.saturating_sub(1);
                self.repick();
            }
            KeyCode::Enter => {
                if let Some(a) = picked {
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
            KeyCode::Char('x') | KeyCode::Delete => {
                if let Some(a) = picked {
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
}
