//! updates: what's new, update oriel in place, or roll back to the version before. The work itself is
//! crate::update; this is the screen for it.

use crate::pane::{Cx, Pane, Waker};
use crate::update::{self, Release};
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct St {
    releases: Option<Result<Vec<Release>, String>>,
    /// what's happening now ("downloading 0.6.2")
    busy: Option<String>,
    /// how the last update / rollback went
    result: Option<Result<String, String>>,
}

pub struct Updates {
    st: Arc<Mutex<St>>,
    asked: bool,
    confirm_rollback: bool,
    scroll: usize,
}

impl Updates {
    pub fn new() -> Updates {
        Updates { st: Arc::default(), asked: false, confirm_rollback: false, scroll: 0 }
    }

    fn check(&mut self, force: bool, waker: Waker) {
        self.asked = true;
        let st = self.st.clone();
        st.lock().unwrap().busy = Some("checking GitHub for releases".into());
        std::thread::spawn(move || {
            let r = if cfg!(test) { Err("no network in tests".into()) } else { update::check(force) };
            let mut s = st.lock().unwrap();
            s.releases = Some(r);
            s.busy = None;
            drop(s);
            waker.wake();
        });
    }
}

impl Pane for Updates {
    fn title(&self) -> String {
        "updates".into()
    }
    fn icon(&self) -> &'static str {
        "package"
    }

    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        if !self.asked {
            self.check(false, cx.waker());
        }
        let t = cx.theme;
        let st = self.st.lock().unwrap();
        let rs: &[Release] = match &st.releases {
            Some(Ok(rs)) => rs,
            _ => &[],
        };
        let next = update::available(rs);
        let kept: Vec<String> = {
            let mut v: Vec<String> = update::backups().into_iter().map(|(v, _)| v).filter(|v| v != update::VERSION).collect();
            v.sort_by_key(|v| std::cmp::Reverse(update::parse(v)));
            v
        };
        let mut hints: Vec<(&str, &str)> = vec![];
        if next.is_some() && st.busy.is_none() && !matches!(st.result, Some(Ok(_))) {
            hints.push(("enter", "update now"));
        }
        if !kept.is_empty() {
            hints.push(("r", "roll back"));
        }
        hints.extend([("c", "check again"), ("↑↓", "scroll")]);
        let area = ui::hint_line(f, area, &hints, t);
        let body = Rect { x: area.x + 2, y: area.y + 1, width: area.width.saturating_sub(4), height: area.height.saturating_sub(1) };
        let mut lines: Vec<Line> = vec![];
        // ---- where you are
        let state = match (&st.releases, &next) {
            (None, _) => Span::styled("checking…", ui::muted(t)),
            (Some(Err(e)), _) => Span::styled(format!("couldn't check: {e}"), Style::default().fg(t.danger)),
            (Some(Ok(_)), Some(n)) => Span::styled(format!("{} is out", n.version), Style::default().fg(t.good).add_modifier(Modifier::BOLD)),
            (Some(Ok(_)), None) => Span::styled("you have the newest version", Style::default().fg(t.good)),
        };
        lines.push(Line::from(vec![Span::styled(format!("{}oriel {}", ui::lead("package"), update::VERSION), ui::bold_accent(t)), Span::raw("   "), state]));
        if let Some(b) = &st.busy {
            lines.push(Line::from(Span::styled(format!("  {}…", b), Style::default().fg(t.shine))));
        }
        match &st.result {
            Some(Ok(v)) => lines.push(Line::from(Span::styled(format!("  ✓ {v} is installed: quit oriel (ctrl+space q) and start it again to use it"), Style::default().fg(t.good)))),
            Some(Err(e)) => lines.push(Line::from(Span::styled(format!("  ✗ {e}"), Style::default().fg(t.danger)))),
            None => {}
        }
        if self.confirm_rollback {
            if let Some(v) = kept.first() {
                lines.push(Line::from(Span::styled(format!("  press r again to go back to {v}"), Style::default().fg(t.danger).add_modifier(Modifier::BOLD))));
            }
        }
        if !kept.is_empty() {
            lines.push(Line::from(Span::styled(format!("  kept for rollback: {}", kept.join(" · ")), ui::muted(t))));
        }
        lines.push(Line::raw(""));
        // ---- what's new: coming up if there's an update, else what this version brought
        let (head, notes) = match &next {
            Some(_) => ("what's new since your version".to_string(), update::notes_since(rs, update::VERSION)),
            None => {
                let cur = rs.iter().find(|r| r.version == update::VERSION);
                ("what's in this version".to_string(), cur.map(|r| update::notes_since(std::slice::from_ref(r), "0.0.0")).unwrap_or_default())
            }
        };
        lines.push(Line::from(Span::styled(head, Style::default().fg(t.shine).add_modifier(Modifier::BOLD))));
        lines.push(Line::raw(""));
        if notes.is_empty() {
            lines.push(Line::from(Span::styled(if st.releases.is_none() { "…" } else { "no notes for this one" }, ui::muted(t))));
        }
        drop(st);
        let md = crate::panes::chat::render_markdown(&notes, body.width as usize, t);
        lines.extend(md);
        let h = body.height as usize;
        self.scroll = self.scroll.min(lines.len().saturating_sub(h));
        f.render_widget(Paragraph::new(lines.into_iter().skip(self.scroll).take(h).collect::<Vec<_>>()), body);
    }

    fn key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        let rollback_pending = std::mem::take(&mut self.confirm_rollback);
        match k.code {
            KeyCode::Enter => {
                let next = match &self.st.lock().unwrap().releases {
                    Some(Ok(rs)) => update::available(rs),
                    _ => None,
                };
                if let Some(r) = next {
                    let (st, waker) = (self.st.clone(), cx.waker());
                    st.lock().unwrap().busy = Some(format!("updating to {}", r.version));
                    std::thread::spawn(move || {
                        let say = |s: &str| {
                            st.lock().unwrap().busy = Some(s.to_string());
                            waker.wake();
                        };
                        let res = if cfg!(test) { Err("no updating in tests".into()) } else { update::install(&r, &say).map(|_| r.version.clone()) };
                        let mut s = st.lock().unwrap();
                        s.busy = None;
                        s.result = Some(res);
                        drop(s);
                        waker.wake();
                    });
                }
            }
            KeyCode::Char('r') => {
                if !rollback_pending {
                    self.confirm_rollback = true;
                } else {
                    let res = if cfg!(test) { Err("no rollback in tests".into()) } else { update::rollback() };
                    self.st.lock().unwrap().result = Some(res.map(|v| format!("oriel {v} (rolled back)")));
                }
            }
            KeyCode::Char('c') => self.check(true, cx.waker()),
            KeyCode::Down | KeyCode::Char('j') => self.scroll += 1,
            KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageDown => self.scroll += 10,
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Kit;

    #[test]
    fn updates_screen() {
        let mut k = Kit::new();
        let mut p = Updates::new();
        p.asked = true;
        p.st.lock().unwrap().releases = Some(Ok(vec![
            Release { version: "9.1.0".into(), notes: "- a calendar\n- themes".into(), asset: "x".into(), ..Default::default() },
            Release { version: update::VERSION.into(), notes: "- this one".into(), asset: "x".into(), ..Default::default() },
        ]));
        let s = k.render_html(&mut p, 110, 30, "target/snap/updates.html");
        assert!(s.contains("9.1.0 is out") && s.contains("what's new since your version") && s.contains("a calendar") && s.contains("update now"), "{s}");
        assert!(!s.contains("this one"), "only what's newer");
    }
}
