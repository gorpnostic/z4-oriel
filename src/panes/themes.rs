//! The themes app: make your own theme. Every colour is listed next to a live preview; arrows, [ ] and - =
//! change hue, lightness and saturation, and each change saves to `themes/<name>.toml` and recolours oriel at
//! once. Editing a built-in theme makes your own copy on the first change.

use crate::pane::{Action, Cx, Pane};
use crate::theme::{self, Theme};
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

enum Input {
    Hex(String),
    Name(String),
}

pub struct Themes {
    sel: usize,
    input: Option<Input>,
    msg: Option<String>,
    rows: Vec<(Rect, usize)>,
    side_hits: Vec<(Rect, String)>,
}

/// The colours, then the rainbow switch.
fn row_count() -> usize {
    theme::COLOR_KEYS.len() + 1
}

impl Themes {
    pub fn new() -> Themes {
        Themes { sel: 0, input: None, msg: None, rows: vec![], side_hits: vec![] }
    }

    /// Change the live theme and save it: to its own file if it's yours, else to a new copy of the built-in.
    fn change(&mut self, cx: &mut Cx, f: impl FnOnce(&mut Theme)) {
        let mut th = cx.theme.clone();
        f(&mut th);
        let (name, base) = if theme::is_custom(&th.name) {
            (th.name.clone(), theme::base_of(&th.name))
        } else {
            let n = theme::free_name(&format!("my-{}", th.name));
            self.msg = Some(format!("made your own copy of {}: {n} (the original stays as it was)", th.name));
            (n, th.name.clone())
        };
        th.name = name.clone();
        match theme::save_custom(&name, &base, &th) {
            Ok(_) => cx.act(Action::ApplyTheme(name)),
            Err(e) => self.msg = Some(format!("couldn't save the theme: {e}")),
        }
    }

    /// Nudge the selected colour in HSL.
    fn nudge(&mut self, cx: &mut Cx, dh: f32, ds: f32, dl: f32) {
        let Some(&(key, _)) = theme::COLOR_KEYS.get(self.sel) else { return };
        let fallback = if key == "background" { (16, 16, 20) } else { (220, 220, 220) };
        let (h, mut s, l) = theme::to_hsl(theme::approx_rgb(theme::field(cx.theme, key), fallback));
        // a grey has no hue to turn: give it a little colour first
        if dh != 0.0 && s < 0.05 {
            s = 0.35;
        }
        let c = theme::from_hsl(h + dh, s + ds, l + dl);
        self.change(cx, |t| theme::set_field(t, key, c));
    }

    fn preview(&self, f: &mut Frame, area: Rect, t: &Theme) {
        let inner = ui::frame(f, area, &format!("{}preview", ui::lead("theme")), Some("how it looks"), true, t);
        let fg = |c: Color| Style::default().fg(c);
        let text = if matches!(t.fg, Color::Reset) { Style::default() } else { fg(t.fg) };
        let (add, del) = (crate::panes::chat::diff_band(t, true), crate::panes::chat::diff_band(t, false));
        let on = |st: Style, b: Option<(Color, Color)>, word: bool| match b {
            Some((line, w)) => st.bg(if word { w } else { line }),
            None => st,
        };
        let w = inner.width.saturating_sub(2) as usize;
        let pad = |used: usize| " ".repeat(w.saturating_sub(used));
        let mut lines: Vec<Line> = vec![
            Line::from(vec![Span::styled("❯ ", fg(t.muted)), Span::styled("make the counter start at one", text)]),
            Line::raw(""),
            Line::from(vec![Span::styled("● ", fg(t.good)), Span::styled("Update", text.add_modifier(Modifier::BOLD)), Span::styled("(src/counter.rs)", text)]),
            Line::from(vec![Span::styled("  ⎿  ", fg(t.muted)), Span::styled("Added 1 line, removed 1 line", text)]),
            Line::from(vec![Span::styled("     3  ", fg(t.muted)), Span::styled("fn ", fg(t.accent)), Span::styled("start", fg(t.shine)), Span::styled("() -> u32 {", text)]),
        ];
        let del_line = vec![
            Span::styled("     4 -", on(fg(t.danger), del, false)),
            Span::styled("    let ", on(fg(t.accent), del, false)),
            Span::styled("n = ", on(text, del, false)),
            Span::styled("0", on(fg(theme::mix(t.inline, t.accent, 0.5)), del, true)),
            Span::styled(";", on(text, del, false)),
            Span::styled(pad(19), on(text, del, false)),
        ];
        let add_line = vec![
            Span::styled("     4 +", on(fg(t.good), add, false)),
            Span::styled("    let ", on(fg(t.accent), add, false)),
            Span::styled("n = ", on(text, add, false)),
            Span::styled("1", on(fg(theme::mix(t.inline, t.accent, 0.5)), add, true)),
            Span::styled(";", on(text, add, false)),
            Span::styled(pad(19), on(text, add, false)),
        ];
        lines.push(Line::from(del_line));
        lines.push(Line::from(add_line));
        lines.extend([
            Line::from(vec![Span::styled("     5      ", fg(t.muted)), Span::styled("log", fg(t.shine)), Span::styled("(", text), Span::styled("\"started\"", fg(t.inline)), Span::styled("); ", text), Span::styled("// once", fg(t.muted).add_modifier(Modifier::ITALIC))]),
            Line::raw(""),
            Line::from(vec![Span::styled("● ", text), Span::styled("Done: the counter starts at ", text), Span::styled("1", fg(t.inline)), Span::styled(" now.", text)]),
            Line::raw(""),
            Line::from(vec![Span::styled("✻ ", fg(t.accent)), Span::styled("Cooked for 12s · 1.2k tokens", fg(t.muted))]),
            Line::raw(""),
            Line::from(vec![
                Span::styled("● ", fg(t.good)),
                Span::styled("done   ", fg(t.muted)),
                Span::styled("● ", fg(t.danger)),
                Span::styled("needs you   ", fg(t.muted)),
                Span::styled("◐ ", fg(t.accent)),
                Span::styled("working", fg(t.muted)),
            ]),
            Line::raw(""),
            Line::from(vec![Span::styled("╭─ ", fg(t.user)), Span::styled("you ", fg(t.muted)), Span::styled("──────────╮", fg(t.user)), Span::styled("   ╭─ ", fg(t.frame)), Span::styled("a pane you're not in", fg(t.muted)), Span::styled(" ─╮", fg(t.frame))]),
            Line::from(vec![Span::styled("╰────────────────╯", fg(t.user)), Span::styled("   ╰─────────────────────────╯", fg(t.frame))]),
            Line::raw(""),
            Line::from(vec![Span::styled("enter", text.add_modifier(Modifier::BOLD)), Span::styled(" send · ", fg(t.muted)), Span::styled("esc", text.add_modifier(Modifier::BOLD)), Span::styled(" stop · ", fg(t.muted)), Span::styled("F10", text.add_modifier(Modifier::BOLD)), Span::styled(" help", fg(t.muted))]),
        ]);
        f.render_widget(Paragraph::new(lines), Rect { x: inner.x + 1, width: inner.width.saturating_sub(1), ..inner });
    }
}

impl Pane for Themes {
    fn title(&self) -> String {
        "themes".into()
    }
    fn icon(&self) -> &'static str {
        "theme"
    }

    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        let hints: &[(&str, &str)] = match self.input {
            Some(Input::Hex(_)) => &[("type", "#rrggbb, a name like bright-blue, or terminal"), ("enter", "set"), ("esc", "cancel")],
            Some(Input::Name(_)) => &[("type", "a name"), ("enter", "make it"), ("esc", "cancel")],
            None => &[("↑↓", "colour"), ("←→", "hue"), ("[ ]", "darker/lighter"), ("- =", "less/more colour"), ("enter", "type it"), ("r", "reset"), ("n", "new"), ("o", "open file")],
        };
        let area = ui::hint_line(f, area, hints, t);
        let body = Rect { x: area.x + 1, y: area.y + 1, width: area.width.saturating_sub(2), height: area.height.saturating_sub(1) };
        let mine = theme::is_custom(&t.name);
        let mut y = body.y;
        let head = if mine {
            vec![
                Span::styled(format!("{}{}", ui::lead("theme"), t.name), ui::bold_accent(t)),
                Span::styled(format!("  yours · starts from {} · saved as you go", theme::base_of(&t.name)), ui::muted(t)),
            ]
        } else {
            vec![Span::styled(format!("{}{}", ui::lead("theme"), t.name), ui::bold_accent(t)), Span::styled("  built in · change anything and it becomes your own copy", ui::muted(t))]
        };
        f.render_widget(Paragraph::new(Line::from(head)), Rect { y, height: 1, ..body });
        y += 2;
        // ---- the colours
        let wide = body.width >= 112;
        let list_w = if wide { 58 } else { body.width };
        self.rows.clear();
        for i in 0..row_count() {
            if y >= body.bottom() {
                break;
            }
            let r = Rect { x: body.x, y, width: list_w, height: 1 };
            let on = i == self.sel;
            let mark = Span::styled(if on { "▸ " } else { "  " }, ui::bold_accent(t));
            let spans = if let Some(&(key, what)) = theme::COLOR_KEYS.get(i) {
                let c = theme::field(t, key);
                let swatch = if matches!(c, Color::Reset) { Span::styled(" term ", ui::muted(t)) } else { Span::styled("██████", Style::default().fg(c)) };
                let name_st = if on { ui::bold_accent(t) } else { Style::default() };
                let mut v = vec![mark, Span::styled(format!("{key:<11}"), name_st), swatch, Span::styled(format!("  {:<14}", theme::color_name(c)), ui::muted(t))];
                if on || wide {
                    v.push(Span::styled(ui::fit(what, (list_w as usize).saturating_sub(36)), ui::muted(t)));
                }
                v
            } else {
                let name_st = if on { ui::bold_accent(t) } else { Style::default() };
                vec![mark, Span::styled(format!("{:<11}", "rainbow"), name_st), Span::styled(if t.animated { "on    " } else { "off   " }, ui::accent(t)), Span::styled("  the animated logo · enter switches it", ui::muted(t))]
            };
            f.render_widget(Paragraph::new(Line::from(spans)), r);
            self.rows.push((r, i));
            y += 1;
        }
        y += 1;
        // ---- typing, or what just happened
        if y < body.bottom() {
            let r = Rect { x: body.x, y, width: list_w, height: 1 };
            match &self.input {
                Some(Input::Hex(s)) => f.render_widget(Paragraph::new(Line::from(vec![Span::styled("colour › ", ui::bold_accent(t)), Span::raw(s.clone()), Span::styled("▏", ui::accent(t))])), r),
                Some(Input::Name(s)) => f.render_widget(Paragraph::new(Line::from(vec![Span::styled("new theme called › ", ui::bold_accent(t)), Span::raw(s.clone()), Span::styled("▏", ui::accent(t))])), r),
                None => {
                    let problem = if mine { theme::problems(&t.name).into_iter().next() } else { None };
                    if let Some(p) = problem {
                        f.render_widget(Paragraph::new(Span::styled(ui::fit(&format!("⚠ {p}"), list_w as usize), Style::default().fg(t.danger))), r);
                    } else if let Some(m) = &self.msg {
                        f.render_widget(Paragraph::new(Span::styled(ui::fit(m, list_w as usize), Style::default().fg(t.good))), r);
                    }
                }
            }
            y += 2;
        }
        if mine && y < body.bottom() {
            let path = theme::themes_dir().join(format!("{}.toml", t.name));
            f.render_widget(Paragraph::new(Span::styled(ui::fit(&format!("file: {}", path.display()), list_w as usize), ui::muted(t))), Rect { x: body.x, y, width: list_w, height: 1 });
        }
        // ---- the preview
        let pv = if wide {
            Rect { x: body.x + list_w + 2, y: body.y + 2, width: body.width.saturating_sub(list_w + 2), height: body.height.saturating_sub(2).min(22) }
        } else {
            let top = y + 2;
            Rect { x: body.x, y: top, width: body.width, height: body.bottom().saturating_sub(top).min(22) }
        };
        if pv.height >= 8 && pv.width >= 30 {
            self.preview(f, pv, t);
        }
    }

    fn key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        if let Some(input) = &mut self.input {
            let buf = match input {
                Input::Hex(s) | Input::Name(s) => s,
            };
            match k.code {
                KeyCode::Esc => self.input = None,
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => buf.push(c),
                KeyCode::Enter => match self.input.take() {
                    Some(Input::Hex(s)) => match (theme::color(&s), theme::COLOR_KEYS.get(self.sel)) {
                        (Some(c), Some(&(key, _))) => self.change(cx, |t| theme::set_field(t, key, c)),
                        _ => self.msg = Some(format!("'{s}' isn't a colour: write it like #b48cff")),
                    },
                    Some(Input::Name(s)) => {
                        let name = theme::free_name(&s);
                        let mut th = cx.theme.clone();
                        let base = if theme::is_custom(&th.name) { theme::base_of(&th.name) } else { th.name.clone() };
                        th.name = name.clone();
                        match theme::save_custom(&name, &base, &th) {
                            Ok(_) => {
                                self.msg = Some(format!("made {name}: change its colours here"));
                                cx.act(Action::ApplyTheme(name));
                            }
                            Err(e) => self.msg = Some(format!("couldn't make it: {e}")),
                        }
                    }
                    None => {}
                },
                _ => {}
            }
            return true;
        }
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        let on_rainbow = self.sel == theme::COLOR_KEYS.len();
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.sel = (self.sel + 1).min(row_count() - 1),
            KeyCode::Left | KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') if on_rainbow => self.change(cx, |t| t.animated = !t.animated),
            KeyCode::Left => self.nudge(cx, if shift { -2.0 } else { -8.0 }, 0.0, 0.0),
            KeyCode::Right => self.nudge(cx, if shift { 2.0 } else { 8.0 }, 0.0, 0.0),
            KeyCode::Char('[') => self.nudge(cx, 0.0, 0.0, -0.04),
            KeyCode::Char(']') => self.nudge(cx, 0.0, 0.0, 0.04),
            KeyCode::Char('-') => self.nudge(cx, 0.0, -0.06, 0.0),
            KeyCode::Char('=') | KeyCode::Char('+') => self.nudge(cx, 0.0, 0.06, 0.0),
            KeyCode::Enter => {
                let cur = theme::COLOR_KEYS.get(self.sel).map(|&(key, _)| theme::color_name(theme::field(cx.theme, key))).unwrap_or_default();
                self.input = Some(Input::Hex(cur));
            }
            KeyCode::Char('r') => {
                if let Some(&(key, _)) = theme::COLOR_KEYS.get(self.sel) {
                    let orig = theme::field(&theme::get(&theme::base_of(&cx.theme.name)), key);
                    self.change(cx, |t| theme::set_field(t, key, orig));
                    self.msg = Some(format!("{key} is back to {}'s", theme::base_of(&cx.theme.name)));
                }
            }
            KeyCode::Char('n') => self.input = Some(Input::Name(theme::free_name("my-theme"))),
            KeyCode::Char('o') => {
                if theme::is_custom(&cx.theme.name) {
                    let path = theme::themes_dir().join(format!("{}.toml", cx.theme.name));
                    self.msg = Some(match open_in_editor(&path) {
                        Ok(()) => "opened the file: save it and oriel recolours".into(),
                        Err(e) => format!("couldn't open it: {e}"),
                    });
                } else {
                    self.msg = Some("a built-in has no file: change a colour (or n) to make your own".into());
                }
            }
            _ => return false,
        }
        true
    }

    fn mouse(&mut self, ev: MouseEvent, _area: Rect, _cx: &mut Cx) {
        if let MouseEventKind::Down(MouseButton::Left) = ev.kind {
            let pos = Position { x: ev.column, y: ev.row };
            if let Some(&(_, i)) = self.rows.iter().find(|(r, _)| r.contains(pos)) {
                self.sel = i;
            }
        }
    }

    /// Every theme in the sidebar: click one to use it (and edit it here).
    fn side(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        self.side_hits.clear();
        let mut y = area.y;
        for name in theme::names() {
            if y >= area.bottom() {
                break;
            }
            let r = Rect { y, height: 1, ..area };
            let right = if theme::is_custom(&name) { "yours" } else { "" };
            ui::side_row(f, r, "theme", &name, right, name == t.name, t);
            self.side_hits.push((r, name));
            y += 1;
        }
    }

    fn side_mouse(&mut self, ev: MouseEvent, _area: Rect, cx: &mut Cx) {
        if let MouseEventKind::Down(MouseButton::Left) = ev.kind {
            let pos = Position { x: ev.column, y: ev.row };
            if let Some((_, name)) = self.side_hits.iter().find(|(r, _)| r.contains(pos)) {
                cx.act(Action::ApplyTheme(name.clone()));
                self.msg = None;
            }
        }
    }
}

/// Open a text file to edit: Notepad on Windows (a .toml may have no app), the system's default elsewhere.
fn open_in_editor(path: &std::path::Path) -> Result<(), String> {
    if cfg!(test) {
        return Ok(());
    }
    if cfg!(windows) {
        std::process::Command::new("notepad.exe").arg(path).spawn().map(|_| ()).map_err(|e| e.to_string())
    } else {
        crate::panes::files::preview::open_external(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Kit;

    fn with_dir(name: &str) -> std::path::PathBuf {
        let d = std::path::absolute(format!("target/test-scratch/themes-{name}")).unwrap();
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        theme::TEST_DIR.with(|t| *t.borrow_mut() = Some(d.clone()));
        d
    }

    /// Kit applies nothing, so do what the app does with ApplyTheme.
    fn apply(k: &mut Kit) {
        for a in std::mem::take(&mut k.actions) {
            if let Action::ApplyTheme(n) = a {
                k.theme = theme::get(&n);
            }
        }
    }

    #[test]
    fn themes_file_format() {
        let d = with_dir("format");
        std::fs::write(d.join("mine.toml"), "base = \"dracula\"\naccent = \"#ff0000\"\ngood = \"bright-green\"\nshine = \"#0f0\"\nacent = \"x\"\nrainbow = true\n").unwrap();
        assert!(theme::names().contains(&"mine".to_string()));
        let t = theme::get("mine");
        assert_eq!(t.accent, Color::Rgb(255, 0, 0));
        assert_eq!(t.shine, Color::Rgb(0, 255, 0));
        assert_eq!(t.good, Color::LightGreen);
        assert_eq!(t.inline, theme::get("dracula").inline, "the rest comes from the base");
        assert!(t.animated);
        assert!(theme::problems("mine").iter().any(|p| p.contains("'acent' isn't a setting")));
        // a typo mid-edit keeps the last good version
        std::fs::write(d.join("mine.toml"), "base = \"dracula\"\naccent = \"#ff0000\n").unwrap();
        assert_eq!(theme::get("mine").accent, Color::Rgb(255, 0, 0));
        assert!(!theme::problems("mine").is_empty());
        // round trip
        theme::save_custom("copy", "ocean", &theme::get("ultra")).unwrap();
        let c = theme::get("copy");
        assert_eq!((c.accent, c.good, c.danger), (theme::get("ultra").accent, theme::get("ultra").good, theme::get("ultra").danger));
        assert!(theme::problems("copy").is_empty(), "{:?}", theme::problems("copy"));
    }

    #[test]
    fn themes_editor() {
        let d = with_dir("editor");
        let mut k = Kit::new();
        k.theme = theme::get("ultra");
        let mut p = Themes::new();
        let s = k.render_html(&mut p, 150, 40, "target/snap/themes-editor.html");
        assert!(s.contains("accent") && s.contains("#b48cff") && s.contains("preview") && s.contains("built in"), "{s}");
        // the first change of a built-in makes your own copy, saved and applied
        k.key(&mut p, KeyCode::Right);
        apply(&mut k);
        assert_eq!(k.theme.name, "my-ultra");
        assert!(d.join("my-ultra.toml").is_file());
        assert_ne!(k.theme.accent, theme::get("ultra").accent, "the hue turned");
        // then it edits that copy
        k.key(&mut p, KeyCode::Down);
        k.key(&mut p, KeyCode::Char(']'));
        apply(&mut k);
        assert_eq!(k.theme.name, "my-ultra");
        assert_ne!(k.theme.shine, theme::get("ultra").shine);
        // type a colour
        k.key(&mut p, KeyCode::Enter);
        for _ in 0..10 {
            k.key(&mut p, KeyCode::Backspace);
        }
        k.typ(&mut p, "#123456");
        k.key(&mut p, KeyCode::Enter);
        apply(&mut k);
        assert_eq!(k.theme.shine, Color::Rgb(0x12, 0x34, 0x56));
        // reset it
        k.key(&mut p, KeyCode::Char('r'));
        apply(&mut k);
        assert_eq!(k.theme.shine, theme::get("ultra").shine);
        let s = k.render_html(&mut p, 150, 40, "target/snap/themes-editor-mine.html");
        assert!(s.contains("yours · starts from ultra"), "{s}");
        let side = k.render_side(&mut p, 30, 20);
        assert!(side.contains("my-ultra") && side.contains("yours"), "{side}");
        theme::TEST_DIR.with(|t| *t.borrow_mut() = None);
    }
}
