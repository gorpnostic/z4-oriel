//! Shared drawing: the rounded frame with its title set into the border (the nest look), icons, the logo,
//! and small helpers every pane uses.

use crate::theme::{Theme, rainbow};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};
use std::sync::atomic::{AtomicBool, Ordering};

/// false = plain-text fallbacks for terminals without a Nerd Font.
pub static NERD: AtomicBool = AtomicBool::new(true);

// Nerd Font (Material Design) codepoints, from github.com/ryanoasis/nerd-fonts glyphnames.json
const ICONS: &[(&str, &str, &str)] = &[
    ("oriel", "\u{F05A8}", "*"),   // md-white_balance_sunny... replaced below by window
    ("window", "\u{F05AF}", "#"),  // md-window_maximize
    ("term", "\u{F018D}", ">"),    // md-console
    ("ai", "\u{F0674}", "*"),      // md-creation (sparkles)
    ("music", "\u{F075A}", "~"),   // md-music
    ("system", "\u{F061A}", "#"),  // md-chip
    ("files", "\u{F0256}", "/"),   // md-folder_outline
    ("notes", "\u{F0387}", "="),   // md-note... (music_note in nest; notebook here)
    ("storage", "\u{F02CA}", "%"), // md-harddisk
    ("home", "\u{F02DC}", "@"),    // md-home
    ("theme", "\u{F03D8}", "&"),   // md-palette
    ("calendar", "\u{F00ED}", "="), // md-calendar
    ("play", "\u{F040A}", ">"),
    ("pause", "\u{F03E4}", "||"),
    ("prev", "\u{F04AE}", "|<"),
    ("next", "\u{F04AD}", ">|"),
    ("shuffle", "\u{F049F}", "shuf"),
    ("repeat", "\u{F0456}", "rep"),
    ("volume", "\u{F057E}", "vol"),
    ("search", "\u{F0349}", "?"),
    ("gauge", "\u{F029A}", "#"),
    ("chart", "\u{F0128}", "#"),
    ("clock", "\u{F0150}", ""),
    ("robot", "\u{F06A9}", "@"),
    ("cloud", "\u{F0163}", "@"),
    ("claude", "\u{F0674}", "*"),
    ("split", "\u{F0E4D}", "|"),   // md-view_split_vertical... fallback bar
    ("close", "\u{F0156}", "x"),   // md-close
    ("tab", "\u{F04E9}", "+"),     // md-tab
    ("quit", "\u{F0206}", "q"),    // md-exit_to_app
    ("package", "\u{F03D3}", "pkg"),
    ("trash", "\u{F01B4}", "del"),
    ("file", "\u{F0214}", "-"),    // md-file
    ("doc", "\u{F0219}", "-"),     // md-file_document
    ("code", "\u{F0169}", "<>"),   // md-code_braces
    ("image", "\u{F021F}", "img"),
    ("audio", "\u{F0223}", "aud"),
    ("video", "\u{F022B}", "vid"),
    ("archive", "\u{F0225}", "zip"),
    ("up", "\u{F0737}", ".."),     // md-arrow_up_bold... folder up
    ("new", "\u{F1412}", "+"),     // md-chat_plus_outline
    ("you", "\u{F0004}", ""),      // md-account
];

pub const ICON_NAMES: &[&str] = &["term", "ai", "claude", "robot", "music", "system", "files", "notes", "storage", "home"];

/// The Nerd Font glyph regardless of the plain-icons setting (for the "can you see these?" check).
pub fn icon_nerd(name: &str) -> &'static str {
    ICONS.iter().find(|i| i.0 == name).map(|i| i.1).unwrap_or("")
}

pub fn icon(name: &str) -> &'static str {
    let nerd = NERD.load(Ordering::Relaxed);
    ICONS.iter().find(|i| i.0 == name).map(|i| if nerd { i.1 } else { i.2 }).unwrap_or("")
}

/// Icon + space, or nothing.
pub fn lead(name: &str) -> String {
    let g = icon(name);
    if g.is_empty() { String::new() } else { format!("{g} ") }
}

/// The nest-style frame: rounded hairline border, bold title set into the top edge, optional subtitle in the
/// bottom-right. Returns the inner rect.
pub fn frame(f: &mut Frame, area: Rect, title: &str, subtitle: Option<&str>, focused: bool, t: &Theme) -> Rect {
    let border = if focused { t.accent } else { t.frame };
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .title(Line::from(Span::styled(
            format!(" {title} "),
            Style::default().fg(if focused { t.accent } else { t.muted }).add_modifier(Modifier::BOLD),
        )));
    if let Some(s) = subtitle {
        block = block.title_bottom(Line::from(Span::styled(format!(" {s} "), Style::default().fg(t.muted))).right_aligned());
    }
    let inner = block.inner(area);
    f.render_widget(block, area);
    inner
}

/// A centered floating box (palette, dialogs). Clears what's under it.
pub fn popup(f: &mut Frame, screen: Rect, w: u16, h: u16, title: &str, t: &Theme) -> Rect {
    let w = w.min(screen.width.saturating_sub(4));
    let h = h.min(screen.height.saturating_sub(2));
    let r = Rect { x: screen.x + (screen.width - w) / 2, y: screen.y + (screen.height.saturating_sub(h)) / 3, width: w, height: h };
    f.render_widget(Clear, r);
    frame(f, r, title, None, true, t)
}

pub fn muted(t: &Theme) -> Style {
    Style::default().fg(t.muted)
}
pub fn accent(t: &Theme) -> Style {
    Style::default().fg(t.accent)
}
pub fn bold_accent(t: &Theme) -> Style {
    Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
}

/// Truncate to `w` display columns with an ellipsis.
pub fn fit(s: &str, w: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::new();
    let mut used = 0;
    let total: usize = s.chars().map(|c| c.width().unwrap_or(0)).sum();
    if total <= w {
        return s.to_string();
    }
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if used + cw + 1 > w {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push('…');
    out
}

pub fn human_bytes(n: u64) -> String {
    let mut v = n as f64;
    for unit in ["B", "K", "M", "G", "T"] {
        if v < 1024.0 || unit == "T" {
            return if unit == "B" { format!("{v:.0}{unit}") } else { format!("{v:.1}{unit}") };
        }
        v /= 1024.0;
    }
    unreachable!()
}

// ------------------------------------------------------------------ logo
// Omarchy-style block letters (from nest's font.py: 8 rows, 3-wide stems).
const O: [&str; 8] = ["  ▄█████▄ ", " ███   ███", " ███   ███", " ███   ███", " ███   ███", " ███   ███", " ███   ███", "  ▀█████▀ "];
const R: [&str; 8] = ["  ▄███████", " ███   ███", " ███   ███", "▄███▄▄▄██▀", "▀███▀▀▀▀  ", "██████████", " ███   ███", " ███   █▀ "];
const I: [&str; 8] = [" ▄█▄", " ███", " ███", " ███", " ███", " ███", " ███", " █▀ "];
const E: [&str; 8] = ["  ▄███████", " ███   ███", " ███   █▀ ", "▄███▄▄▄   ", "▀███▀▀▀   ", " ███   █▄ ", " ███   ███", " █████████"];
const L: [&str; 8] = [" ▄█      ", " ███     ", " ███     ", " ███     ", " ███     ", " ███   █▄", " ███   ███", " █████████"];

pub fn logo_lines() -> Vec<String> {
    (0..8).map(|r| [O[r], R[r], I[r], E[r], L[r]].join(" ")).collect()
}

/// Draw the big logo centered in `area` (skips it if there's no room). Rainbow on animated themes.
pub fn logo(f: &mut Frame, area: Rect, t: &Theme, time: f64) -> u16 {
    let lines = logo_lines();
    let w = lines[0].chars().count() as u16;
    if area.width < w + 2 || area.height < 9 {
        let p = Paragraph::new(Line::from(Span::styled("oriel", bold_accent(t)))).centered();
        f.render_widget(p, Rect { height: 1, ..area });
        return 1;
    }
    let x = area.x + (area.width - w) / 2;
    big_logo(f, &lines, x, area.y, t, time);
    8
}

/// Draw big-font rows at (x, y), shaded top to bottom from the theme accent to its highlight colour; on the
/// animated theme a soft rainbow drifts across it instead.
pub fn big_logo(f: &mut Frame, rows: &[String], x: u16, y: u16, t: &Theme, time: f64) {
    let n = rows.len().max(2) - 1;
    let area = f.area();
    for (r, line) in rows.iter().enumerate() {
        let yy = y + r as u16;
        if yy >= area.bottom() {
            break;
        }
        let spans: Vec<Span> = if t.animated {
            line.chars()
                .enumerate()
                .map(|(i, c)| Span::styled(c.to_string(), Style::default().fg(crate::theme::rainbow_at((i as f64 + r as f64 * 0.6) * 0.012 - time * 0.05, 0.5))))
                .collect()
        } else {
            vec![Span::styled(line.clone(), Style::default().fg(crate::theme::mix(t.accent, t.shine, r as f32 / n as f32)))]
        };
        let w = (line.chars().count() as u16).min(area.right().saturating_sub(x));
        f.render_widget(Paragraph::new(Line::from(spans)), Rect { x, y: yy, width: w, height: 1 });
    }
}

pub fn key_hint<'a>(key: &'a str, what: &'a str, t: &Theme) -> Vec<Span<'a>> {
    vec![
        Span::styled(key, Style::default().fg(t.accent).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {what}   "), Style::default().fg(t.muted)),
    ]
}

/// nest's hint line: "esc stop · ctrl+r regenerate · F1 ai" in the muted colour, keys a touch brighter.
/// Draws on the last row of `area` and returns the rect above it.
pub fn hint_line(f: &mut Frame, area: Rect, hints: &[(&str, &str)], t: &Theme) -> Rect {
    if area.height < 2 {
        return area;
    }
    let row = Rect { y: area.bottom() - 1, height: 1, ..area };
    let mut spans = vec![Span::raw(" ")];
    for (i, (k, what)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", Style::default().fg(t.muted)));
        }
        spans.push(Span::styled(k.to_string(), Style::default().fg(t.fg).add_modifier(Modifier::BOLD)));
        if !what.is_empty() {
            spans.push(Span::styled(format!(" {what}"), Style::default().fg(t.muted)));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), row);
    Rect { height: area.height - 1, ..area }
}

/// A sidebar-style list row: icon, label, right-aligned hint (like "F1" or a count). `on` = current.
pub fn side_row(f: &mut Frame, r: Rect, icon: &str, label: &str, right: &str, on: bool, t: &Theme) {
    let rw = unicode_width::UnicodeWidthStr::width(right) as u16;
    let lead = lead(icon);
    let style = if on { Style::default().fg(t.accent).add_modifier(Modifier::BOLD) } else { Style::default() };
    let left_w = r.width.saturating_sub(rw + 1) as usize;
    let text = fit(&format!("{lead}{label}"), left_w);
    let spans = vec![
        Span::styled(format!("{text:<left_w$}"), style),
        Span::styled(right.to_string(), if on { style } else { Style::default().fg(t.muted) }),
    ];
    f.render_widget(Paragraph::new(Line::from(spans)), r);
}

/// A thin horizontal rule in the frame colour.
pub fn rule(f: &mut Frame, r: Rect, t: &Theme) {
    f.render_widget(Paragraph::new(Span::styled("─".repeat(r.width as usize), Style::default().fg(t.frame))), Rect { height: 1, ..r });
}

pub fn fg(c: Color) -> Style {
    Style::default().fg(c)
}
