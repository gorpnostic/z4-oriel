//! Just enough markdown for chat replies: headings, bold/italic, `inline code`, fenced code blocks, lists,
//! quotes, links. Produces ratatui lines already wrapped to a width (so the chat knows its exact height).

use crate::theme::Theme;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// Wrap styled spans to `width` columns, breaking at spaces where possible. `indent` is prepended to every
/// line (and counted in the width).
pub fn wrap(spans: Vec<Span<'static>>, width: usize, indent: &str, cont_indent: &str) -> Vec<Line<'static>> {
    let width = width.max(8);
    let mut out: Vec<Line<'static>> = vec![];
    let mut cur: Vec<Span<'static>> = vec![Span::raw(indent.to_string())];
    let mut used = indent.width();
    // split every span into words keeping trailing spaces, so styles survive wrapping
    for sp in spans {
        let style = sp.style;
        let text = sp.content.to_string();
        let mut word = String::new();
        let mut flush = |word: &mut String, cur: &mut Vec<Span<'static>>, used: &mut usize, out: &mut Vec<Line<'static>>| {
            if word.is_empty() {
                return;
            }
            let w = word.width();
            if *used + w.saturating_sub(trailing_spaces(word)) > width && *used > cont_indent.width() {
                out.push(Line::from(std::mem::take(cur)));
                cur.push(Span::raw(cont_indent.to_string()));
                *used = cont_indent.width();
                let trimmed = word.trim_start().to_string();
                *word = trimmed;
            }
            // a single word longer than the line: hard-break it
            let mut rest = std::mem::take(word);
            while *used + rest.width() > width && rest.chars().count() > 1 && rest.trim_end().width() > width - cont_indent.width() {
                let room = width.saturating_sub(*used).max(1);
                let mut head = String::new();
                let mut hw = 0;
                let mut chars = rest.chars().peekable();
                while let Some(&c) = chars.peek() {
                    let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                    if hw + cw > room {
                        break;
                    }
                    head.push(c);
                    hw += cw;
                    chars.next();
                }
                if head.is_empty() {
                    break;
                }
                cur.push(Span::styled(head, style));
                out.push(Line::from(std::mem::take(cur)));
                cur.push(Span::raw(cont_indent.to_string()));
                *used = cont_indent.width();
                rest = chars.collect();
            }
            *used += rest.width();
            cur.push(Span::styled(rest, style));
        };
        for c in text.chars() {
            if c == '\n' {
                flush(&mut word, &mut cur, &mut used, &mut out);
                out.push(Line::from(std::mem::take(&mut cur)));
                cur.push(Span::raw(cont_indent.to_string()));
                used = cont_indent.width();
                continue;
            }
            word.push(c);
            if c == ' ' {
                flush(&mut word, &mut cur, &mut used, &mut out);
            }
        }
        flush(&mut word, &mut cur, &mut used, &mut out);
    }
    out.push(Line::from(cur));
    out
}

fn trailing_spaces(s: &str) -> usize {
    s.len() - s.trim_end_matches(' ').len()
}

/// Inline markdown (**bold**, *italic*, `code`, [text](url)) to spans in `base` style.
pub fn inline(text: &str, base: Style, t: &Theme) -> Vec<Span<'static>> {
    let mut out = vec![];
    let mut buf = String::new();
    let (mut bold, mut italic) = (false, false);
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let style = |bold: bool, italic: bool| {
        let mut s = base;
        if bold {
            s = s.add_modifier(Modifier::BOLD);
        }
        if italic {
            s = s.add_modifier(Modifier::ITALIC);
        }
        s
    };
    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            if let Some(end) = chars[i + 1..].iter().position(|&x| x == '`') {
                if !buf.is_empty() {
                    out.push(Span::styled(std::mem::take(&mut buf), style(bold, italic)));
                }
                let code: String = chars[i + 1..i + 1 + end].iter().collect();
                out.push(Span::styled(code, Style::default().fg(t.inline)));
                i += end + 2;
                continue;
            }
        }
        if c == '*' || c == '_' {
            let double = i + 1 < chars.len() && chars[i + 1] == c;
            // underscores inside words (snake_case) are literal
            let word_inside = c == '_' && i > 0 && chars[i - 1].is_alphanumeric() && i + 1 < chars.len() && chars[i + 1].is_alphanumeric();
            if !word_inside {
                if !buf.is_empty() {
                    out.push(Span::styled(std::mem::take(&mut buf), style(bold, italic)));
                }
                if double {
                    bold = !bold;
                    i += 2;
                } else {
                    italic = !italic;
                    i += 1;
                }
                continue;
            }
        }
        if c == '[' {
            // [text](url) -> text (underlined)
            if let Some(close) = chars[i..].iter().position(|&x| x == ']') {
                let close = i + close;
                if close + 1 < chars.len() && chars[close + 1] == '(' {
                    if let Some(end) = chars[close..].iter().position(|&x| x == ')') {
                        if !buf.is_empty() {
                            out.push(Span::styled(std::mem::take(&mut buf), style(bold, italic)));
                        }
                        let label: String = chars[i + 1..close].iter().collect();
                        out.push(Span::styled(label, style(bold, italic).add_modifier(Modifier::UNDERLINED).fg(t.shine)));
                        i = close + end + 1;
                        continue;
                    }
                }
            }
        }
        buf.push(c);
        i += 1;
    }
    if !buf.is_empty() {
        out.push(Span::styled(buf, style(bold, italic)));
    }
    out
}

/// Render a whole markdown reply into wrapped lines, each prefixed with `indent`.
pub fn render(text: &str, width: usize, indent: &str, t: &Theme) -> Vec<Line<'static>> {
    let mut out = vec![];
    let mut in_code = false;
    let mut lang = String::new();
    let code_style = Style::default().fg(t.inline);
    let body = Style::default().fg(t.fg);
    for raw in text.split('\n') {
        let line = raw.trim_end_matches('\r');
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if in_code {
                in_code = false;
                out.push(Line::from(Span::styled(format!("{indent}└─"), Style::default().fg(t.frame))));
            } else {
                in_code = true;
                lang = trimmed.trim_start_matches('`').trim().to_string();
                let label = if lang.is_empty() { "code".to_string() } else { lang.clone() };
                out.push(Line::from(vec![
                    Span::styled(format!("{indent}┌─ "), Style::default().fg(t.frame)),
                    Span::styled(label, Style::default().fg(t.muted)),
                ]));
            }
            continue;
        }
        if in_code {
            // code keeps its spacing; long lines are cut, not wrapped
            let room = width.saturating_sub(indent.width() + 2);
            let shown = crate::ui::fit(&line.replace('\t', "    "), room);
            out.push(Line::from(vec![Span::styled(format!("{indent}│ "), Style::default().fg(t.frame)), Span::styled(shown, code_style)]));
            continue;
        }
        if trimmed.is_empty() {
            out.push(Line::raw(""));
            continue;
        }
        if let Some(h) = heading(trimmed) {
            let (level, text) = h;
            let st = Style::default().fg(t.accent).add_modifier(Modifier::BOLD);
            let prefix = if level <= 2 { "" } else { "" };
            out.extend(wrap(inline(&format!("{prefix}{text}"), st, t), width, indent, indent));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("> ") {
            let bar = format!("{indent}▎ ");
            out.extend(wrap(inline(rest, Style::default().fg(t.muted).add_modifier(Modifier::ITALIC), t), width, &bar, &bar));
            continue;
        }
        let lead = line.len() - trimmed.len();
        let pad = " ".repeat(lead.min(8));
        if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")).or_else(|| trimmed.strip_prefix("+ ")) {
            let first = format!("{indent}{pad}• ");
            let cont = format!("{indent}{pad}  ");
            let mut spans = vec![];
            spans.extend(inline(rest, body, t));
            let mut l = wrap(spans, width, &first, &cont);
            // colour the bullet
            if let Some(first_line) = l.first_mut() {
                if let Some(sp) = first_line.spans.first_mut() {
                    *sp = Span::styled(sp.content.to_string(), Style::default().fg(t.accent));
                }
            }
            out.extend(l);
            continue;
        }
        if let Some((num, rest)) = numbered(trimmed) {
            let first = format!("{indent}{pad}{num}. ");
            let cont = format!("{indent}{pad}{}", " ".repeat(num.len() + 2));
            out.extend(wrap(inline(rest, body, t), width, &first, &cont));
            continue;
        }
        if trimmed.chars().all(|c| c == '-' || c == '*' || c == '_') && trimmed.len() >= 3 {
            out.push(Line::from(Span::styled(format!("{indent}{}", "─".repeat(width.saturating_sub(indent.width()).min(40))), Style::default().fg(t.frame))));
            continue;
        }
        out.extend(wrap(inline(line, body, t), width, indent, indent));
    }
    if in_code {
        out.push(Line::from(Span::styled(format!("{indent}└─"), Style::default().fg(t.frame))));
    }
    // no trailing blank lines
    while out.last().map(|l| l.width() == 0 || l.spans.iter().all(|s| s.content.trim().is_empty())).unwrap_or(false) {
        out.pop();
    }
    out
}

fn heading(s: &str) -> Option<(usize, &str)> {
    let n = s.chars().take_while(|&c| c == '#').count();
    if n > 0 && n <= 6 && s[n..].starts_with(' ') { Some((n, s[n + 1..].trim())) } else { None }
}

fn numbered(s: &str) -> Option<(&str, &str)> {
    let n = s.chars().take_while(|c| c.is_ascii_digit()).count();
    if n > 0 && n < 4 && s[n..].starts_with(". ") { Some((&s[..n], &s[n + 2..])) } else { None }
}
