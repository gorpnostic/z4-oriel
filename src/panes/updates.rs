//! updates: what's new, update oriel in place, or roll back to the version before. The work itself is
//! crate::update; this is the screen for it. Render never touches files: the kept versions are listed when the
//! screen opens and again after a check, an update or a rollback.

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
    /// asking GitHub (kept apart from `busy`, so a check finishing can't hide an update that's still running)
    checking: bool,
    /// the update or rollback happening now ("downloading 0.6.2")
    busy: Option<String>,
    /// how the last update / rollback went
    result: Option<Result<String, String>>,
    /// older versions kept for rollback, newest first
    kept: Vec<String>,
}

impl St {
    fn list_kept(&mut self) {
        self.kept = update::older_kept(update::VERSION).into_iter().map(|(v, _)| v).collect();
    }
    /// Why an update or rollback can't start now. After a swap the file on disk is already another version, so
    /// a second one in this process would keep the wrong binary for rollback.
    fn blocked(&self) -> Option<&'static str> {
        if self.busy.is_some() {
            Some("one at a time: wait for this one to finish")
        } else if matches!(self.result, Some(Ok(_))) {
            Some("restart oriel first: quit (ctrl+space q) and start it again")
        } else {
            None
        }
    }
}

pub struct Updates {
    st: Arc<Mutex<St>>,
    asked: bool,
    confirm_rollback: bool,
    scroll: usize,
}

impl Updates {
    pub fn new() -> Updates {
        let mut st = St::default();
        st.list_kept();
        Updates { st: Arc::new(Mutex::new(st)), asked: false, confirm_rollback: false, scroll: 0 }
    }

    fn check(&mut self, force: bool, waker: Waker) {
        self.asked = true;
        let st = self.st.clone();
        st.lock().unwrap().checking = true;
        std::thread::spawn(move || {
            let r = if cfg!(test) { Err("no network in tests".into()) } else { update::check(force).map_err(|e| e.to_string()) };
            let mut s = st.lock().unwrap();
            s.releases = Some(r);
            s.checking = false;
            s.list_kept();
            drop(s);
            waker.wake();
        });
    }

    /// Run an update or rollback on a worker thread: `busy` while it runs, then `result`, then the kept list again.
    fn run(&self, doing: String, waker: Waker, work: impl FnOnce(&dyn Fn(&str)) -> Result<String, String> + Send + 'static) {
        let st = self.st.clone();
        st.lock().unwrap().busy = Some(doing);
        std::thread::spawn(move || {
            let say = |s: &str| {
                st.lock().unwrap().busy = Some(s.to_string());
                waker.wake();
            };
            let res = work(&say);
            let mut s = st.lock().unwrap();
            s.busy = None;
            s.result = Some(res);
            s.list_kept();
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
        let back = st.kept.first().map(|v| format!("roll back to {v}"));
        let mut hints: Vec<(&str, &str)> = vec![];
        if next.is_some() && st.blocked().is_none() {
            hints.push(("enter", "update now"));
        }
        if let (Some(b), None) = (&back, st.blocked()) {
            hints.push(("r", b));
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
        } else if st.checking && st.releases.is_some() {
            lines.push(Line::from(Span::styled("  checking GitHub for releases…", ui::muted(t))));
        }
        match &st.result {
            Some(Ok(v)) => lines.push(Line::from(Span::styled(format!("  ✓ {v} is installed: quit oriel ({} q) and start it again to use it", crate::config::prefix(cx.config)), Style::default().fg(t.good)))),
            Some(Err(e)) => lines.push(Line::from(Span::styled(format!("  ✗ {e}"), Style::default().fg(t.danger)))),
            None => {}
        }
        if self.confirm_rollback {
            if let Some(v) = st.kept.first() {
                lines.push(Line::from(Span::styled(format!("  press r again to go back to {v}"), Style::default().fg(t.danger).add_modifier(Modifier::BOLD))));
            }
        }
        if !st.kept.is_empty() {
            lines.push(Line::from(Span::styled(format!("  older versions kept for rollback: {}", st.kept.join(" · ")), ui::muted(t))));
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
            KeyCode::Enter | KeyCode::Char('r') => {
                let st = self.st.lock().unwrap();
                if let Some(why) = st.blocked() {
                    drop(st);
                    cx.notify(why);
                    return true;
                }
                if k.code == KeyCode::Enter {
                    let next = match &st.releases {
                        Some(Ok(rs)) => update::available(rs),
                        _ => None,
                    };
                    drop(st);
                    if let Some(r) = next {
                        self.run(format!("updating to {}", r.version), cx.waker(), move |say| {
                            if cfg!(test) { Err("no updating in tests".into()) } else { update::install(&r, say).map(|_| r.version.clone()).map_err(|e| e.to_string()) }
                        });
                    }
                } else {
                    let target = st.kept.first().cloned();
                    drop(st);
                    match target {
                        None => cx.notify("no older version kept yet (one is kept each time oriel updates itself)"),
                        Some(_) if !rollback_pending => self.confirm_rollback = true,
                        // running the old binary to check it, plus two copies: seconds under a virus scan, so off the UI thread
                        Some(v) => self.run(format!("rolling back to {v}"), cx.waker(), |_| {
                            let res = if cfg!(test) { Err("no rollback in tests".into()) } else { update::rollback() };
                            res.map(|v| format!("oriel {v} (rolled back)"))
                        }),
                    }
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

    fn screen() -> Updates {
        let p = Updates::new();
        let mut st = p.st.lock().unwrap();
        st.releases = Some(Ok(vec![
            Release { version: "9.1.0".into(), notes: "- a calendar\n- themes".into(), asset: "x".into(), ..Default::default() },
            Release { version: update::VERSION.into(), notes: "- this one".into(), asset: "x".into(), ..Default::default() },
        ]));
        st.kept = vec![];
        drop(st);
        Updates { asked: true, ..p }
    }

    #[test]
    fn updates_screen() {
        let mut k = Kit::new();
        let mut p = screen();
        let s = k.render_html(&mut p, 110, 30, "target/snap/updates.html");
        assert!(s.contains("9.1.0 is out") && s.contains("what's new since your version") && s.contains("a calendar") && s.contains("update now"), "{s}");
        assert!(!s.contains("this one"), "only what's newer");
        assert!(!s.contains("roll back"), "no r when nothing older is kept: {s}");
        // r names the older version it goes back to
        p.st.lock().unwrap().kept = vec!["0.6.1".into(), "0.6.0".into()];
        let s = k.render(&mut p, 110, 30);
        assert!(s.contains("roll back to 0.6.1") && s.contains("older versions kept for rollback: 0.6.1 · 0.6.0"), "{s}");
        k.key(&mut p, KeyCode::Char('r'));
        assert!(k.render_html(&mut p, 110, 30, "target/snap/updates-rollback.html").contains("press r again to go back to 0.6.1"));
    }

    #[test]
    fn updates_one_swap_per_run() {
        let mut k = Kit::new();
        let mut p = screen();
        p.st.lock().unwrap().kept = vec!["0.6.1".into()];
        // while an update runs, "check again" can't clear it, and enter / r do nothing but say why
        p.st.lock().unwrap().busy = Some("downloading 9.1.0".into());
        k.key(&mut p, KeyCode::Char('c'));
        let t0 = std::time::Instant::now();
        while p.st.lock().unwrap().checking && t0.elapsed() < std::time::Duration::from_secs(5) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(p.st.lock().unwrap().busy.as_deref(), Some("downloading 9.1.0"), "the check finishing left the update's state alone");
        let s = k.render(&mut p, 110, 30);
        assert!(!s.contains("update now") && !s.contains("roll back"), "{s}");
        k.key(&mut p, KeyCode::Enter);
        assert!(k.notices().last().unwrap().contains("one at a time"), "{:?}", k.notices());
        // after a successful swap, a second update or rollback waits for a restart
        let mut st = p.st.lock().unwrap();
        st.busy = None;
        st.result = Some(Ok("9.1.0".into()));
        drop(st);
        let s = k.render(&mut p, 110, 30);
        assert!(!s.contains("update now") && !s.contains("roll back to") && s.contains("✓ 9.1.0 is installed"), "{s}");
        for key in [KeyCode::Enter, KeyCode::Char('r'), KeyCode::Char('r')] {
            k.actions.clear();
            k.key(&mut p, key);
            assert!(k.notices().last().unwrap().contains("restart oriel first"), "{:?}", k.notices());
        }
        assert!(p.st.lock().unwrap().busy.is_none() && !p.confirm_rollback, "nothing started");
        // a rollback runs on a worker thread, like an update
        let mut st = p.st.lock().unwrap();
        st.result = None;
        st.kept = vec!["0.6.1".into()]; // the check above listed the real (test) folder
        drop(st);
        k.key(&mut p, KeyCode::Char('r'));
        k.key(&mut p, KeyCode::Char('r'));
        let t0 = std::time::Instant::now();
        while p.st.lock().unwrap().result.is_none() && t0.elapsed() < std::time::Duration::from_secs(5) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(p.st.lock().unwrap().result, Some(Err("no rollback in tests".into())));
    }
}
