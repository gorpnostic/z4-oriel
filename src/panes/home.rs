//! The home screen: the logo, every app with its key, and the key bindings. An app from the sidebar opens in its
//! own tab (there's one music player, one calendar…) and the launcher steps aside; a terminal, Claude Code or
//! Codex takes the launcher's place.

use super::{APPS, available, open};
use crate::pane::{Action, Cx, Pane, Place};
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::time::{Duration, Instant};

/// How often the list is checked again (Claude Code or Codex installed since).
const RECHECK: Duration = Duration::from_secs(5);

pub struct Home {
    sel: usize,
    hits: Vec<(Rect, usize)>,
    /// The apps that can open here (claude and codex only when installed). Kept, not worked out per frame: a
    /// program that isn't installed costs a file check for every PATH folder, and the animated logo redraws
    /// eight times a second.
    avail: Vec<usize>,
    avail_at: Instant,
}

impl Home {
    pub fn new() -> Home {
        Home { sel: 0, hits: vec![], avail: Home::apps(), avail_at: Instant::now() }
    }
    fn apps() -> Vec<usize> {
        (0..APPS.len()).filter(|&i| available(APPS[i].0)).collect()
    }
    fn launch(&self, i: usize, cx: &mut Cx) {
        let name = APPS[i].0;
        // an app from the sidebar lives in its tab: go there (a second music player would play over the first)
        // and let this launcher go, except for help, which you come back from
        if name != "terminal" && crate::app::SIDEBAR.iter().any(|a| a.0 == name) {
            cx.act(Action::GotoApp(name));
            if name != "help" {
                cx.act(Action::Close);
            }
            return;
        }
        if let Some(p) = open(name, cx.config) {
            cx.act(Action::Open(p, Place::Replace));
        }
    }
}

impl Pane for Home {
    fn title(&self) -> String {
        "home".into()
    }
    fn icon(&self) -> &'static str {
        "home"
    }
    fn reopen(&self) -> Option<&'static str> {
        Some("home")
    }
    fn tick_every(&self) -> Option<Duration> {
        Some(RECHECK)
    }
    fn poll(&mut self, _cx: &mut Cx) {
        if self.avail_at.elapsed() >= RECHECK {
            self.avail = Home::apps();
            self.avail_at = Instant::now();
        }
    }
    fn hover(&self) -> usize {
        self.sel
    }

    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        let apps = &self.avail;
        self.sel = self.sel.min(apps.len().saturating_sub(1));
        let cols = if area.width >= 90 { 3 } else if area.width >= 60 { 2 } else { 1 };
        let rows = apps.len().div_ceil(cols) as u16;
        // a blank line between rows when there's room, else none: every app still fits in one narrow column
        let gap: u16 = if 10 + 2 + rows * 2 + 2 + 4 <= area.height { 2 } else { 1 };
        let block_h = 10 + 2 + rows * gap + 2 + 4;
        let top = area.y + area.height.saturating_sub(block_h) / 2;
        let mut y = top;
        let lh = ui::logo(f, Rect { y, height: area.height.saturating_sub(y - area.y), ..area }, t, cx.time);
        y += lh + 1;
        if y < area.bottom() {
            let tag = Paragraph::new(Line::from(Span::styled("a window onto everything", ui::muted(t)))).centered();
            f.render_widget(tag, Rect { x: area.x, y, width: area.width, height: 1 });
        }
        y += 2;
        // app grid: the key here, and the app's F-key from anywhere
        let cell_w: u16 = 26;
        let grid_w = cell_w * cols as u16;
        let x0 = area.x + area.width.saturating_sub(grid_w) / 2;
        self.hits.clear();
        for (n, &i) in apps.iter().enumerate() {
            let (name, key, icon, label) = APPS[i];
            let fkey = crate::app::SIDEBAR.iter().find(|a| a.0 == name).map(|a| a.3).unwrap_or("");
            let (r, c) = (n / cols, n % cols);
            let rect = Rect { x: x0 + c as u16 * cell_w, y: y + r as u16 * gap, width: cell_w, height: 1 };
            if rect.bottom() > area.bottom() {
                break;
            }
            // a pane narrower than a cell: the cell is cut at its right edge
            let rect = rect.intersection(area);
            if rect.is_empty() {
                continue;
            }
            let on = n == self.sel;
            let base = if on { Style::default().fg(t.accent).add_modifier(Modifier::BOLD) } else { Style::default() };
            let line = Line::from(vec![
                Span::styled(format!(" {key} "), Style::default().fg(t.accent).add_modifier(Modifier::BOLD | if on { Modifier::REVERSED } else { Modifier::empty() })),
                Span::raw("  "),
                Span::styled(ui::lead(icon), if on { base } else { Style::default().fg(t.accent) }),
                Span::styled(format!("{label:<12}"), base),
                Span::styled(format!("{fkey:>3}"), ui::muted(t)),
            ]);
            f.render_widget(Paragraph::new(line), rect);
            self.hits.push((rect, n));
        }
        y += rows * gap + 1;
        // bindings
        let pre = crate::config::prefix(cx.config).replace("ctrl+", "ctrl-"); // the one in effect
        let lines = vec![
            Line::from([ui::key_hint("alt ←↑↓→", "move", t), ui::key_hint("alt n", "new terminal", t), ui::key_hint("alt p", "palette", t)].concat()),
            Line::from([ui::key_hint("alt 1-9", "tabs", t), ui::key_hint("alt t", "new tab", t), ui::key_hint("alt z", "zoom", t), ui::key_hint("alt w", "close", t)].concat()),
            Line::from([ui::key_hint(&pre, "then | - split · hjkl move · ? help", t)].concat()),
        ];
        for (k, l) in lines.into_iter().enumerate() {
            let r = Rect { x: area.x, y: y + k as u16, width: area.width, height: 1 };
            if r.bottom() <= area.bottom() {
                f.render_widget(Paragraph::new(l).centered(), r);
            }
        }
    }

    fn key(&mut self, key: KeyEvent, cx: &mut Cx) -> bool {
        if key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) {
            return false;
        }
        let n = self.avail.len().max(1);
        match key.code {
            KeyCode::Char(c) => {
                if let Some(&i) = self.avail.iter().find(|&&i| APPS[i].1 == c) {
                    self.launch(i, cx);
                    return true;
                }
                match c {
                    'j' => self.sel = (self.sel + 1) % n,
                    'k' => self.sel = (self.sel + n - 1) % n,
                    _ => return false,
                }
            }
            KeyCode::Down | KeyCode::Right | KeyCode::Tab => self.sel = (self.sel + 1) % n,
            KeyCode::Up | KeyCode::Left | KeyCode::BackTab => self.sel = (self.sel + n - 1) % n,
            // the Enter that started oriel can arrive as a fresh keypress; don't let it open an app
            KeyCode::Enter if cx.time < 0.6 => {}
            KeyCode::Enter => {
                if let Some(&i) = self.avail.get(self.sel) {
                    self.launch(i, cx);
                }
            }
            _ => return false,
        }
        true
    }

    fn mouse(&mut self, ev: MouseEvent, _area: Rect, cx: &mut Cx) {
        let pos = ratatui::layout::Position { x: ev.column, y: ev.row };
        let Some(&(_, n)) = self.hits.iter().find(|(r, _)| r.contains(pos)) else { return };
        match ev.kind {
            MouseEventKind::Moved => self.sel = n,
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(&i) = self.avail.get(n) {
                    self.launch(i, cx);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Kit;

    fn goto(k: &Kit) -> Vec<String> {
        k.actions
            .iter()
            .map(|a| match a {
                Action::GotoApp(n) => format!("goto {n}"),
                Action::Close => "close".into(),
                Action::Open(_, Place::Replace) => "replace".into(),
                _ => "other".into(),
            })
            .collect()
    }

    #[test]
    fn home_goes_to_app_tabs() {
        let mut k = Kit::new();
        let mut h = Home::new();
        let s = k.render(&mut h, 120, 40);
        // every app, with its key here and its F-key from anywhere
        for want in ["agents", "your AIs", "calendar", "themes", "help", "F2", "F3", "F4", "F10"] {
            assert!(s.contains(want), "{want}: {s}");
        }
        assert!(s.lines().any(|l| l.contains(" m ") && l.contains("music") && l.contains("F4")), "{s}");
        // one narrow column still has room for all of them (rows close up instead of the last ones falling off)
        let s = k.render(&mut h, 50, 34);
        assert!(s.contains("themes") && s.contains("help") && s.contains("new tab"), "{s}");
        // music is the F4 tab, not a second player; the launcher steps aside
        k.key(&mut h, KeyCode::Char('m'));
        assert_eq!(goto(&k), ["goto music", "close"]);
        k.actions.clear();
        k.key(&mut h, KeyCode::Char('i'));
        assert_eq!(goto(&k), ["goto ais", "close"]);
        k.actions.clear();
        // help keeps the launcher to come back to
        k.key(&mut h, KeyCode::Char('?'));
        assert_eq!(goto(&k), ["goto help"]);
        k.actions.clear();
        // a terminal is a fresh shell right here
        k.key(&mut h, KeyCode::Char('t'));
        assert_eq!(goto(&k), ["replace"]);
    }

    #[test]
    fn home_checks_programs_every_few_seconds_not_every_frame() {
        let calls = || super::super::AVAILABLE_CALLS.with(|c| c.get());
        let mut k = Kit::new();
        let mut h = Home::new();
        let at_start = calls();
        // was: every frame and every mouse move asked for each app (a PATH search for claude and codex)
        for _ in 0..20 {
            let _ = k.render(&mut h, 120, 40);
        }
        for x in 0..50 {
            k.mouse(&mut h, MouseEvent { kind: MouseEventKind::Moved, column: x, row: 20, modifiers: KeyModifiers::NONE }, Rect::new(0, 0, 120, 40));
        }
        k.key(&mut h, KeyCode::Down);
        k.poll(&mut h);
        assert_eq!(calls(), at_start, "frames, keys and mouse moves use the kept list");
        // the timer checks again
        h.avail_at = Instant::now() - RECHECK;
        k.poll(&mut h);
        assert_eq!(calls(), at_start + APPS.len());
    }
}
