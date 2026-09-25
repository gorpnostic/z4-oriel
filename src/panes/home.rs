//! The home screen: the logo, every app with its key, and the key bindings. Picking an app replaces this pane.

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

pub struct Home {
    sel: usize,
    hits: Vec<(Rect, usize)>,
}

impl Home {
    pub fn new() -> Home {
        Home { sel: 0, hits: vec![] }
    }
    fn apps() -> Vec<usize> {
        (0..APPS.len()).filter(|&i| available(APPS[i].0)).collect()
    }
    fn launch(&self, i: usize, cx: &mut Cx) {
        if let Some(p) = open(APPS[i].0, cx.config) {
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
    fn tick_every(&self) -> Option<std::time::Duration> {
        None
    }

    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        let apps = Home::apps();
        self.sel = self.sel.min(apps.len().saturating_sub(1));
        let cols = if area.width >= 90 { 3 } else if area.width >= 60 { 2 } else { 1 };
        let rows = apps.len().div_ceil(cols) as u16;
        let block_h = 10 + 2 + rows * 2 + 2 + 4;
        let top = area.y + area.height.saturating_sub(block_h) / 2;
        let mut y = top;
        let lh = ui::logo(f, Rect { y, height: area.height.saturating_sub(y - area.y), ..area }, t, cx.time);
        y += lh + 1;
        let tag = Paragraph::new(Line::from(Span::styled("a window onto everything", ui::muted(t)))).centered();
        f.render_widget(tag, Rect { x: area.x, y, width: area.width, height: 1 });
        y += 2;
        // app grid
        let cell_w: u16 = 24;
        let grid_w = cell_w * cols as u16;
        let x0 = area.x + area.width.saturating_sub(grid_w) / 2;
        self.hits.clear();
        for (n, &i) in apps.iter().enumerate() {
            let (name, key, icon, label) = APPS[i];
            let _ = name;
            let (r, c) = (n / cols, n % cols);
            let rect = Rect { x: x0 + c as u16 * cell_w, y: y + r as u16 * 2, width: cell_w, height: 1 };
            if rect.bottom() > area.bottom() {
                break;
            }
            let on = n == self.sel;
            let base = if on { Style::default().fg(t.accent).add_modifier(Modifier::BOLD) } else { Style::default() };
            let line = Line::from(vec![
                Span::styled(format!(" {key} "), Style::default().fg(t.accent).add_modifier(Modifier::BOLD | if on { Modifier::REVERSED } else { Modifier::empty() })),
                Span::raw("  "),
                Span::styled(ui::lead(icon), if on { base } else { Style::default().fg(t.accent) }),
                Span::styled(label.to_string(), base),
            ]);
            f.render_widget(Paragraph::new(line), rect);
            self.hits.push((rect, n));
        }
        y += rows * 2 + 1;
        // bindings
        let pre = cx.config.prefix.replace("ctrl+", "ctrl-");
        let lines = vec![
            Line::from([ui::key_hint("alt ←↑↓→", "move", t), ui::key_hint("alt enter", "new terminal", t), ui::key_hint("alt p", "palette", t)].concat()),
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
        let apps = Home::apps();
        match key.code {
            KeyCode::Char(c) => {
                if let Some(&i) = apps.iter().find(|&&i| APPS[i].1 == c) {
                    self.launch(i, cx);
                    return true;
                }
                match c {
                    'j' => self.sel = (self.sel + 1) % apps.len(),
                    'k' => self.sel = (self.sel + apps.len() - 1) % apps.len(),
                    _ => return false,
                }
            }
            KeyCode::Down | KeyCode::Right | KeyCode::Tab => self.sel = (self.sel + 1) % apps.len(),
            KeyCode::Up | KeyCode::Left | KeyCode::BackTab => self.sel = (self.sel + apps.len() - 1) % apps.len(),
            KeyCode::Enter => self.launch(apps[self.sel], cx),
            _ => return false,
        }
        true
    }

    fn mouse(&mut self, ev: MouseEvent, _area: Rect, cx: &mut Cx) {
        let apps = Home::apps();
        for (r, n) in self.hits.clone() {
            if r.contains(ratatui::layout::Position { x: ev.column, y: ev.row }) {
                match ev.kind {
                    MouseEventKind::Moved => self.sel = n,
                    MouseEventKind::Down(MouseButton::Left) => self.launch(apps[n], cx),
                    _ => {}
                }
            }
        }
    }
}
