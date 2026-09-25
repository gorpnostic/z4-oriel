//! Rendered markdown for the notes preview (ctrl+e): headings in the accent, bullets and checkboxes, quotes,
//! rules, fenced code and `inline code` in the inline colour, **bold**, *italic*, [links](url).

use crate::theme::Theme;
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

pub fn render(text: &str, t: &Theme, width: u16) -> Vec<Line<'static>> {
    let mut out = vec![];
    let mut fence = false;
    for raw in text.replace("\r\n", "\n").split('\n') {
        let line = raw.replace('\t', "    ");
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fence = !fence;
            continue;
        }
        if fence {
            out.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(line.clone(), Style::default().fg(t.inline)),
            ]));
            continue;
        }
        if trimmed.starts_with('#') {
            let level = trimmed.chars().take_while(|c| *c == '#').count();
            let title = trimmed[level..].trim();
            let mut st = Style::default().fg(t.accent).add_modifier(Modifier::BOLD);
            if level == 1 {
                st = st.add_modifier(Modifier::UNDERLINED);
            } else if level >= 3 {
                st = Style::default().fg(t.shine).add_modifier(Modifier::BOLD);
            }
            out.push(Line::from(Span::styled(title.to_string(), st)));
            continue;
        }
        if matches!(trimmed.trim_end(), "---" | "***" | "___") {
            out.push(Line::from(Span::styled("─".repeat(width as usize), Style::default().fg(t.frame))));
            continue;
        }
        let pad = " ".repeat(indent);
        if let Some(r) = trimmed.strip_prefix("> ").or(if trimmed == ">" { Some("") } else { None }) {
            let mut spans = vec![Span::raw(pad), Span::styled("▎ ", Style::default().fg(t.accent))];
            spans.extend(inline(r, Style::default().fg(t.muted).add_modifier(Modifier::ITALIC), t));
            out.push(Line::from(spans));
            continue;
        }
        let checkbox = [("- [ ] ", "☐ "), ("- [x] ", "☑ "), ("- [X] ", "☑ ")].iter().find(|(p, _)| trimmed.starts_with(p));
        if let Some((p, glyph)) = checkbox {
            let done = glyph.starts_with('☑');
            let body = Style::default().add_modifier(if done { Modifier::CROSSED_OUT | Modifier::DIM } else { Modifier::empty() });
            let mut spans = vec![Span::raw(format!("{pad}  ")), Span::styled(glyph.to_string(), Style::default().fg(t.accent))];
            spans.extend(inline(&trimmed[p.len()..], body, t));
            out.push(Line::from(spans));
            continue;
        }
        if let Some(r) = ["- ", "* ", "+ "].iter().find_map(|b| trimmed.strip_prefix(b)) {
            let mut spans = vec![Span::raw(format!("{pad}  ")), Span::styled("• ", Style::default().fg(t.accent))];
            spans.extend(inline(r, Style::default(), t));
            out.push(Line::from(spans));
            continue;
        }
        let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() && trimmed[digits.len()..].starts_with(". ") {
            let mut spans = vec![Span::raw(format!("{pad}  ")), Span::styled(format!("{digits}. "), Style::default().fg(t.accent))];
            spans.extend(inline(&trimmed[digits.len() + 2..], Style::default(), t));
            out.push(Line::from(spans));
            continue;
        }
        let mut spans = vec![Span::raw(pad)];
        spans.extend(inline(trimmed, Style::default(), t));
        out.push(Line::from(spans));
    }
    out
}

/// Inline markup: `code`, **bold**, *italic* / _italic_, ~~strike~~, [text](url).
fn inline(s: &str, base: Style, t: &Theme) -> Vec<Span<'static>> {
    let c: Vec<char> = s.chars().collect();
    let mut out: Vec<Span<'static>> = vec![];
    let mut cur = String::new();
    let (mut bold, mut ital, mut strike) = (false, false, false);
    let style = |b: bool, i: bool, st: bool| {
        let mut s = base;
        if b {
            s = s.add_modifier(Modifier::BOLD);
        }
        if i {
            s = s.add_modifier(Modifier::ITALIC);
        }
        if st {
            s = s.add_modifier(Modifier::CROSSED_OUT);
        }
        s
    };
    let find = |from: usize, pat: &[char]| (from..c.len()).find(|&j| c[j..].starts_with(pat));
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        let flush = |cur: &mut String, out: &mut Vec<Span<'static>>, st: Style| {
            if !cur.is_empty() {
                out.push(Span::styled(std::mem::take(cur), st));
            }
        };
        if ch == '`' {
            if let Some(e) = find(i + 1, &['`']) {
                flush(&mut cur, &mut out, style(bold, ital, strike));
                out.push(Span::styled(c[i + 1..e].iter().collect::<String>(), Style::default().fg(t.inline)));
                i = e + 1;
                continue;
            }
        }
        if c[i..].starts_with(&['*', '*']) || c[i..].starts_with(&['_', '_']) {
            let pat = [ch, ch];
            if bold || find(i + 2, &pat).is_some() {
                flush(&mut cur, &mut out, style(bold, ital, strike));
                bold = !bold;
                i += 2;
                continue;
            }
        }
        if c[i..].starts_with(&['~', '~']) && (strike || find(i + 2, &['~', '~']).is_some()) {
            flush(&mut cur, &mut out, style(bold, ital, strike));
            strike = !strike;
            i += 2;
            continue;
        }
        if ch == '*' || ch == '_' {
            let prev_word = i > 0 && c[i - 1].is_alphanumeric();
            let next_word = c.get(i + 1).map(|n| n.is_alphanumeric()).unwrap_or(false);
            // snake_case and 2*3 stay literal
            let opens = !ital && !prev_word && c.get(i + 1).map(|n| !n.is_whitespace()).unwrap_or(false) && find(i + 1, &[ch]).is_some();
            let closes = ital && !(ch == '_' && next_word);
            if opens || closes {
                flush(&mut cur, &mut out, style(bold, ital, strike));
                ital = !ital;
                i += 1;
                continue;
            }
        }
        if ch == '[' {
            if let Some(close) = find(i + 1, &[']']) {
                if c.get(close + 1) == Some(&'(') {
                    if let Some(end) = find(close + 2, &[')']) {
                        flush(&mut cur, &mut out, style(bold, ital, strike));
                        let label: String = c[i + 1..close].iter().collect();
                        out.push(Span::styled(label, style(bold, ital, strike).fg(t.accent).add_modifier(Modifier::UNDERLINED)));
                        i = end + 1;
                        continue;
                    }
                }
            }
        }
        cur.push(ch);
        i += 1;
    }
    if !cur.is_empty() {
        out.push(Span::styled(cur, style(bold, ital, strike)));
    }
    out
}
