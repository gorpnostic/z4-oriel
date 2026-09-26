//! alerts: the event center. What happened while you were busy, newest first; enter goes to where it happened.

use crate::alerts::{self, Kind};
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
    sel: usize,
    rows: Vec<(Rect, usize)>,
}

impl Alerts {
    pub fn new() -> Alerts {
        Alerts { sel: 0, rows: vec![] }
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
        let mut all = alerts::CENTER.lock().unwrap();
        self.rows.clear();
        if all.is_empty() {
            let lines = vec![
                Line::from(Span::styled("nothing yet", ui::bold_accent(t))),
                Line::raw(""),
                Line::from(Span::styled("Agents finishing or needing you, approvals and questions, failed builds and merge checks, calendar", ui::muted(t))),
                Line::from(Span::styled("reminders, finished installs, memory and AI-usage warnings and new versions land here.", ui::muted(t))),
            ];
            f.render_widget(Paragraph::new(lines), body);
            return;
        }
        self.sel = self.sel.min(all.len() - 1);
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
        // you've seen them now
        if cx.focused {
            let mut changed = false;
            for a in all.iter_mut().filter(|a| !a.read) {
                a.read = true;
                changed = true;
            }
            if changed {
                alerts::save(&all);
            }
        }
    }

    fn key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        let mut all = alerts::CENTER.lock().unwrap();
        let n = all.len();
        // rows are newest first; row r is all[n - 1 - r]
        let idx = (n > 0).then(|| n - 1 - self.sel.min(n - 1));
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => self.sel = (self.sel + 1).min(n.saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => self.sel = self.sel.saturating_sub(1),
            KeyCode::Enter => {
                if let Some(i) = idx {
                    let a = &all[i];
                    if let Some(p) = a.pane {
                        cx.act(Action::FocusPane(p));
                    } else if let Some(app) = a.app.as_deref().and_then(app_of) {
                        cx.act(Action::GotoApp(app));
                    } else {
                        cx.notify("that one has nowhere to go");
                    }
                }
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                if let Some(i) = idx {
                    all.remove(i);
                    alerts::save(&all);
                }
            }
            KeyCode::Char('c') => {
                all.clear();
                alerts::save(&all);
                self.sel = 0;
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
                }
            }
            MouseEventKind::ScrollDown => self.sel += 1,
            MouseEventKind::ScrollUp => self.sel = self.sel.saturating_sub(1),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Kit;
    use crate::alerts::Alert;

    #[test]
    fn alerts_center() {
        {
            let mut c = alerts::CENTER.lock().unwrap();
            c.clear();
            for (k, s, app) in [(Kind::AgentDone, "claude code finished · fix tests", Some("ai")), (Kind::BuildFailed, "w1: add parser failed", Some("agents")), (Kind::Calendar, "in 10 min: 14:00 dentist", Some("calendar"))] {
                c.push(Alert { at: alerts::now() - 120, kind: k, text: s.into(), app: app.map(String::from), read: false, pane: None });
            }
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
        assert_eq!(alerts::CENTER.lock().unwrap().len(), 2);
        k.key(&mut p, KeyCode::Char('c'));
        assert!(alerts::CENTER.lock().unwrap().is_empty());
    }
}
