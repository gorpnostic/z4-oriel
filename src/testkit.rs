//! Headless testing for panes: render into an off-screen buffer and get plain text back, feed keys and mouse
//! events, no real terminal or window involved. Use from a `#[cfg(test)] mod tests` in any pane file:
//!
//!     let mut k = crate::testkit::Kit::new();
//!     let mut p = Music::new(&k.config);
//!     k.key(&mut p, KeyCode::Char('j'));
//!     println!("{}", k.render(&mut p, 120, 40));   // cargo test music -- --nocapture
//!     println!("{}", k.render_side(&mut p, 34, 20)); // the pane's sidebar section

#![cfg(test)]

use crate::config::Config;
use crate::pane::{Action, Cx, Event, Pane};
use crate::theme::{self, Theme};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::{Terminal, backend::TestBackend, layout::Rect};
use std::sync::mpsc::{Receiver, Sender, channel};

pub struct Kit {
    pub config: Config,
    pub theme: Theme,
    pub tx: Sender<Event>,
    pub rx: Receiver<Event>,
    /// Actions the pane asked for (open, notify, ...), newest last.
    pub actions: Vec<Action>,
    pub time: f64,
}

impl Kit {
    pub fn new() -> Kit {
        let (tx, rx) = channel();
        Kit { config: Config::default(), theme: theme::get("oriel"), tx, rx, actions: vec![], time: 1.0 }
    }

    fn cx(&mut self) -> Cx<'_> {
        Cx { id: 1, theme: &self.theme, config: &self.config, tx: &self.tx, actions: &mut self.actions, focused: true, time: self.time }
    }

    /// Render the pane's inside at w x h and return it as text, one line per row.
    pub fn render(&mut self, p: &mut dyn Pane, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut cx_actions = std::mem::take(&mut self.actions);
        term.draw(|f| {
            let mut cx = Cx { id: 1, theme: &self.theme, config: &self.config, tx: &self.tx, actions: &mut cx_actions, focused: true, time: self.time };
            p.render(f, Rect::new(0, 0, w, h), &mut cx);
        })
        .unwrap();
        self.actions = cx_actions;
        dump(term.backend())
    }

    /// Render like `render` and also save a coloured HTML snapshot (see save_html).
    pub fn render_html(&mut self, p: &mut dyn Pane, w: u16, h: u16, path: &str) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut cx_actions = std::mem::take(&mut self.actions);
        term.draw(|f| {
            let mut cx = Cx { id: 1, theme: &self.theme, config: &self.config, tx: &self.tx, actions: &mut cx_actions, focused: true, time: self.time };
            let inner = crate::ui::frame(f, Rect::new(0, 0, w, h), &format!("{}{}", crate::ui::lead(p.icon()), p.title()), p.subtitle().as_deref(), true, &self.theme);
            p.render(f, inner, &mut cx);
        })
        .unwrap();
        self.actions = cx_actions;
        save_html(term.backend().buffer(), path);
        dump(term.backend())
    }

    /// Render the pane's sidebar section at w x h.
    pub fn render_side(&mut self, p: &mut dyn Pane, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut cx_actions = std::mem::take(&mut self.actions);
        term.draw(|f| {
            let mut cx = Cx { id: 1, theme: &self.theme, config: &self.config, tx: &self.tx, actions: &mut cx_actions, focused: true, time: self.time };
            p.side(f, Rect::new(0, 0, w, h), &mut cx);
        })
        .unwrap();
        self.actions = cx_actions;
        dump(term.backend())
    }

    pub fn key(&mut self, p: &mut dyn Pane, code: KeyCode) -> bool {
        self.key_mod(p, code, KeyModifiers::NONE)
    }

    pub fn key_mod(&mut self, p: &mut dyn Pane, code: KeyCode, m: KeyModifiers) -> bool {
        let mut cx = self.cx();
        p.key(KeyEvent::new(code, m), &mut cx)
    }

    pub fn typ(&mut self, p: &mut dyn Pane, text: &str) {
        for c in text.chars() {
            self.key(p, KeyCode::Char(c));
        }
    }

    pub fn mouse(&mut self, p: &mut dyn Pane, ev: MouseEvent, area: Rect) {
        let mut cx = self.cx();
        p.mouse(ev, area, &mut cx);
    }

    /// Call `poll` (what the app does after a Wake or tick). Drains pending wakes first.
    pub fn poll(&mut self, p: &mut dyn Pane) {
        while self.rx.try_recv().is_ok() {}
        let mut cx = self.cx();
        p.poll(&mut cx);
    }

    /// Wait up to `ms` for background work to wake the pane, polling as it does.
    pub fn wait_wake(&mut self, p: &mut dyn Pane, ms: u64) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        let mut woke = false;
        while std::time::Instant::now() < deadline {
            if let Ok(Event::Wake(_)) = self.rx.recv_timeout(std::time::Duration::from_millis(20)) {
                woke = true;
                let mut cx = self.cx();
                p.poll(&mut cx);
            }
        }
        woke
    }

    pub fn notices(&self) -> Vec<String> {
        // alerts are toasts too (and also go to the event center)
        self.actions.iter().filter_map(|a| match a { Action::Notify(s) | Action::Alert(_, s) => Some(s.clone()), _ => None }).collect()
    }
}

/// Write a rendered buffer as a coloured HTML page (terminal black, Cascadia-ish mono). Turn it into a PNG with
/// `pwsh tools\snap.ps1 <file.html>` to look at it.
pub fn save_html(buf: &ratatui::buffer::Buffer, path: &str) {
    use ratatui::style::{Color, Modifier};
    fn css(c: Color, fallback: &str) -> String {
        const ANSI: [&str; 16] = ["#1e1e1e", "#e05a5a", "#6fbf73", "#e6b673", "#4aa8d4", "#bd93f9", "#56c8d8", "#cfcfcf",
            "#6e6e6e", "#ff7b72", "#a6e3a1", "#ffd580", "#7fd0ff", "#d6acff", "#8be9fd", "#ffffff"];
        match c {
            Color::Reset => fallback.into(),
            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
            Color::Indexed(i) if i < 16 => ANSI[i as usize].into(),
            Color::Indexed(i) if i >= 232 => { let v = 8 + (i - 232) * 10; format!("#{v:02x}{v:02x}{v:02x}") }
            Color::Indexed(i) => { let i = i - 16; let f = |x: u8| if x == 0 { 0 } else { 55 + x * 40 }; format!("#{:02x}{:02x}{:02x}", f(i / 36), f((i / 6) % 6), f(i % 6)) }
            Color::Black => ANSI[0].into(), Color::Red => ANSI[1].into(), Color::Green => ANSI[2].into(), Color::Yellow => ANSI[3].into(),
            Color::Blue => ANSI[4].into(), Color::Magenta => ANSI[5].into(), Color::Cyan => ANSI[6].into(), Color::Gray => ANSI[7].into(),
            Color::DarkGray => ANSI[8].into(), Color::LightRed => ANSI[9].into(), Color::LightGreen => ANSI[10].into(),
            Color::LightYellow => ANSI[11].into(), Color::LightBlue => ANSI[12].into(), Color::LightMagenta => ANSI[13].into(),
            Color::LightCyan => ANSI[14].into(), Color::White => ANSI[15].into(),
        }
    }
    let mut h = String::from("<!doctype html><meta charset=utf-8><style>body{margin:0;background:#0c0c0c}pre{margin:0;padding:8px;font:15px/1.25 'Cascadia Mono NF','Cascadia Mono',Consolas,monospace;color:#d8d8d8}span{white-space:pre}</style><pre>");
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let c = &buf[(x, y)];
            let (mut fg, mut bg) = (css(c.fg, "#d8d8d8"), css(c.bg, "transparent"));
            if c.modifier.contains(Modifier::REVERSED) {
                std::mem::swap(&mut fg, &mut bg);
                if fg == "transparent" { fg = "#0c0c0c".into(); }
            }
            let w = if c.modifier.contains(Modifier::BOLD) { "font-weight:bold;" } else { "" };
            let sym = c.symbol().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
            h.push_str(&format!("<span style=\"color:{fg};background:{bg};{w}\">{sym}</span>"));
        }
        h.push('\n');
    }
    h.push_str("</pre>");
    let _ = std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap());
    std::fs::write(path, h).unwrap();
}

fn dump(b: &TestBackend) -> String {
    let buf = b.buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        let mut line = String::new();
        for x in 0..buf.area.width {
            line.push_str(buf[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}
