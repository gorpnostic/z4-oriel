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
        let flush = |word: &mut String, cur: &mut Vec<Span<'static>>, used: &mut usize, out: &mut Vec<Line<'static>>| {
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
            // a single word longer than the line: hard-break it. One pass over the word: what's left of it is a
            // byte offset with its width, char count and trimmed width kept up to date (a streamed base64 blob
            // is re-wrapped every frame, so this must stay linear)
            let word = std::mem::take(word);
            let cont_w = cont_indent.width();
            let (mut start, mut rest_w, mut rest_n, mut rest_trim_w) = (0usize, word.width(), word.chars().count(), word.trim_end().width());
            while *used + rest_w > width && rest_n > 1 && rest_trim_w > width.saturating_sub(cont_w) {
                let room = width.saturating_sub(*used).max(1);
                let (mut end, mut hw, mut n) = (start, 0usize, 0usize);
                for c in word[start..].chars() {
                    let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                    if hw + cw > room {
                        break;
                    }
                    end += c.len_utf8();
                    hw += cw;
                    n += 1;
                }
                if end == start {
                    break;
                }
                cur.push(Span::styled(word[start..end].to_string(), style));
                out.push(Line::from(std::mem::take(cur)));
                cur.push(Span::raw(cont_indent.to_string()));
                *used = cont_w;
                // the trailing spaces are at the end, so the trimmed width loses exactly what was cut (or all of it)
                (start, rest_w, rest_n, rest_trim_w) = (end, rest_w.saturating_sub(hw), rest_n - n, rest_trim_w.saturating_sub(hw));
            }
            let rest = word[start..].to_string();
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
            let run = chars[i..].iter().take_while(|&&x| x == c).count();
            let n = run.min(2); // ** is bold, * italic
            let on = if n == 2 { bold } else { italic };
            let toggle = if on { closes(&chars, i, n) } else { opens(&chars, i, n) && closer(&chars, i + n, c, n).is_some() };
            if toggle {
                if !buf.is_empty() {
                    out.push(Span::styled(std::mem::take(&mut buf), style(bold, italic)));
                }
                if n == 2 {
                    bold = !bold;
                } else {
                    italic = !italic;
                }
                i += n;
            } else {
                // src/*.rs, 2 * 3, __init__.py, _private: just characters
                buf.extend(&chars[i..i + run]);
                i += run;
            }
            continue;
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

/// A `*`/`_` run of `n` at `i` can open emphasis: something other than a space follows it, and an underscore
/// doesn't sit inside a word.
fn opens(chars: &[char], i: usize, n: usize) -> bool {
    let next = chars.get(i + n);
    let prev = i.checked_sub(1).map(|p| chars[p]);
    next.is_some_and(|c| !c.is_whitespace()) && !(chars[i] == '_' && prev.is_some_and(|p| p.is_alphanumeric()))
}

/// ... and can close it: it follows something other than a space, and an underscore isn't followed by more of a
/// word (or a file extension: `__init__.py`).
fn closes(chars: &[char], i: usize, n: usize) -> bool {
    let prev = i.checked_sub(1).map(|p| chars[p]);
    let after = |k: usize| chars.get(i + n + k).copied();
    let wordy = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric());
    prev.is_some_and(|p| !p.is_whitespace()) && !(chars[i] == '_' && (wordy(after(0)) || (after(0) == Some('.') && wordy(after(1)))))
}

/// Where the run that would close emphasis opened with `n` × `c` is, from `from` on in the same text (code spans
/// skipped).
fn closer(chars: &[char], from: usize, c: char, n: usize) -> Option<usize> {
    let mut j = from;
    while j < chars.len() {
        if chars[j] == '`' {
            if let Some(end) = chars[j + 1..].iter().position(|&x| x == '`') {
                j += end + 2;
                continue;
            }
        }
        if chars[j] == c {
            let run = chars[j..].iter().take_while(|&&x| x == c).count();
            // *** closes either
            if (run == n || run >= 3) && closes(chars, j, n) {
                return Some(j);
            }
            j += run;
            continue;
        }
        j += 1;
    }
    None
}

/// Render a whole markdown reply into wrapped lines, each prefixed with `indent`.
pub fn render(text: &str, width: usize, indent: &str, t: &Theme) -> Vec<Line<'static>> {
    draw(text, width, indent, t, false).0
}

/// A fenced code block as the reply wrote it: its language ("" when none) and its lines, unwrapped.
#[derive(Clone, Debug, PartialEq)]
pub struct Code {
    pub lang: String,
    pub text: String,
}

/// The fenced code blocks in a reply, in order (an unclosed one runs to the end, as it's drawn).
pub fn fenced_blocks(text: &str) -> Vec<Code> {
    let mut out = vec![];
    let mut cur: Option<(String, Vec<&str>)> = None;
    for line in text.split('\n').map(|l| l.trim_end_matches('\r')) {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            match cur.take() {
                Some((lang, lines)) => out.push(Code { lang, text: lines.join("\n") }),
                None => cur = Some((trimmed.trim_start_matches('`').trim().to_string(), vec![])),
            }
            continue;
        }
        if let Some((_, lines)) = &mut cur {
            lines.push(line);
        }
    }
    if let Some((lang, mut lines)) = cur {
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        out.push(Code { lang, text: lines.join("\n") });
    }
    out
}

/// `render`, plus where each code block's `┌─ lang` header landed: (line index, the block). The headers say a
/// click copies them (the chat does that).
pub fn render_full(text: &str, width: usize, indent: &str, t: &Theme) -> (Vec<Line<'static>>, Vec<(usize, Code)>) {
    draw(text, width, indent, t, true)
}

fn draw(text: &str, width: usize, indent: &str, t: &Theme, copy_hint: bool) -> (Vec<Line<'static>>, Vec<(usize, Code)>) {
    let mut out = vec![];
    let mut heads: Vec<usize> = vec![];
    let mut in_code = false;
    let code_style = Style::default().fg(t.inline);
    let body = Style::default().fg(t.fg);
    let all: Vec<&str> = text.split('\n').map(|l| l.trim_end_matches('\r')).collect();
    let mut at = 0;
    while at < all.len() {
        let line = all[at];
        at += 1;
        let trimmed = line.trim_start();
        // a table: a row of cells, then its |---|---| line
        if !in_code && trimmed.starts_with('|') && all.get(at).is_some_and(|l| is_rule_row(l)) {
            let mut rows = vec![cells(trimmed)];
            let align = cells(all[at].trim()).iter().map(|c| (c.starts_with(':'), c.ends_with(':'))).collect::<Vec<_>>();
            at += 1;
            while at < all.len() && all[at].trim_start().starts_with('|') {
                rows.push(cells(all[at].trim()));
                at += 1;
            }
            table(&rows, &align, width, indent, t, &mut out);
            continue;
        }
        if trimmed.starts_with("```") {
            if in_code {
                in_code = false;
                out.push(Line::from(Span::styled(format!("{indent}└─"), Style::default().fg(t.frame))));
            } else {
                in_code = true;
                let lang = trimmed.trim_start_matches('`').trim().to_string();
                let label = if lang.is_empty() { "code".to_string() } else { lang };
                heads.push(out.len());
                out.push(Line::from(vec![
                    Span::styled(format!("{indent}┌─ "), Style::default().fg(t.frame)),
                    Span::styled(label, Style::default().fg(t.muted)),
                    Span::styled(if copy_hint { "  click to copy" } else { "" }, Style::default().fg(t.frame)),
                ]));
            }
            continue;
        }
        if in_code {
            // code keeps its spacing; a long line carries on under ↪ rather than being cut
            let room = width.saturating_sub(indent.width() + 2).max(4);
            for (n, piece) in hard_wrap(&line.replace('\t', "    "), room).into_iter().enumerate() {
                let gutter = if n == 0 { format!("{indent}│ ") } else { format!("{indent}│↪") };
                out.push(Line::from(vec![Span::styled(gutter, Style::default().fg(t.frame)), Span::styled(piece, code_style)]));
            }
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
            // a quiet bullet: the words are what matter
            if let Some(first_line) = l.first_mut() {
                if let Some(sp) = first_line.spans.first_mut() {
                    *sp = Span::styled(sp.content.to_string(), Style::default().fg(t.muted));
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
    let codes = if heads.is_empty() { vec![] } else { heads.into_iter().zip(fenced_blocks(text)).collect() };
    (out, codes)
}

/// Cut a line into pieces of at most `room` columns (code: no word breaks, every character kept).
fn hard_wrap(s: &str, room: usize) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut used = 0;
    for c in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > room && used > 0 {
            out.push(String::new());
            used = 0;
        }
        out.last_mut().unwrap().push(c);
        used += w;
    }
    out
}

/// `|---|:--:|` (the leading and trailing pipes optional): the line under a table's header.
fn is_rule_row(l: &str) -> bool {
    let l = l.trim();
    l.contains('-') && l.contains('|') && l.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t')) && cells(l).iter().all(|c| c.trim_matches(':').chars().all(|x| x == '-') && c.contains('-'))
}

/// A table row's cells, trimmed (`\|` and pipes inside `code` stay in the cell).
fn cells(row: &str) -> Vec<String> {
    let row = row.trim();
    let row = row.strip_prefix('|').unwrap_or(row);
    let row = row.strip_suffix('|').filter(|r| !r.ends_with('\\')).unwrap_or(row);
    let (mut out, mut cur, mut code, mut esc) = (vec![], String::new(), false, false);
    for c in row.chars() {
        match c {
            _ if esc => {
                cur.push(c);
                esc = false;
            }
            '\\' => esc = true,
            '`' => {
                code = !code;
                cur.push(c);
            }
            '|' if !code => out.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(c),
        }
    }
    out.push(cur.trim().to_string());
    out
}

/// A markdown table drawn with box lines. Columns keep their natural width when they fit; otherwise the wide
/// ones share the room and their cells wrap. Too narrow even for that: one "header: value" line per cell.
fn table(rows: &[Vec<String>], align: &[(bool, bool)], width: usize, indent: &str, t: &Theme, out: &mut Vec<Line<'static>>) {
    let ncol = rows.iter().map(|r| r.len()).max().unwrap_or(0).max(1);
    let frame = Style::default().fg(t.frame);
    let head = Style::default().fg(t.accent).add_modifier(Modifier::BOLD);
    let body = Style::default().fg(t.fg);
    let spans = |r: usize, c: usize| inline(rows[r].get(c).map(String::as_str).unwrap_or(""), if r == 0 { head } else { body }, t);
    let span_w = |s: &[Span]| s.iter().map(|x| x.content.width()).sum::<usize>();
    let natural: Vec<usize> = (0..ncol).map(|c| (0..rows.len()).map(|r| span_w(&spans(r, c))).max().unwrap_or(0).max(1)).collect();
    // │ a │ b │: three columns of frame per cell plus one
    let room = width.saturating_sub(indent.width() + 3 * ncol + 1);
    let mut w = natural.clone();
    if natural.iter().sum::<usize>() > room {
        // share out the room: narrow columns keep their width, the rest split what's left evenly
        let mut left: Vec<usize> = (0..ncol).collect();
        let mut free = room;
        loop {
            let fair = free / left.len().max(1);
            let (fits, wide): (Vec<usize>, Vec<usize>) = left.iter().partition(|&&c| natural[c] <= fair);
            if fits.is_empty() {
                for (k, &c) in wide.iter().enumerate() {
                    w[c] = fair + usize::from(k < free % wide.len().max(1));
                }
                break;
            }
            for c in fits {
                w[c] = natural[c];
                free -= natural[c];
            }
            left = wide;
            if left.is_empty() {
                break;
            }
        }
        if natural.iter().zip(&w).any(|(n, x)| x < n && *x < 8) {
            // no room for columns: each row as "header: value" lines
            for r in 1..rows.len() {
                for c in 0..ncol {
                    let mut sp = vec![Span::styled(format!("{}: ", rows[0].get(c).map(String::as_str).unwrap_or("")), Style::default().fg(t.muted))];
                    sp.extend(spans(r, c));
                    out.extend(wrap(sp, width, &format!("{indent}{}", if c == 0 { "• " } else { "  " }), &format!("{indent}    ")));
                }
            }
            return;
        }
    }
    let rule = |l: &str, m: &str, r: &str| Line::from(Span::styled(format!("{indent}{l}{}{r}", w.iter().map(|x| "─".repeat(x + 2)).collect::<Vec<_>>().join(m)), frame));
    out.push(rule("┌", "┬", "┐"));
    for r in 0..rows.len() {
        // each cell wrapped to its column, the row as tall as its tallest cell
        let cols: Vec<Vec<Line<'static>>> = (0..ncol)
            .map(|c| {
                if natural[c] <= w[c] {
                    return vec![Line::from(spans(r, c))];
                }
                // wrapping leaves the space after a line's last word: it mustn't push the border out
                let mut lines = wrap(spans(r, c), w[c], "", "");
                for l in &mut lines {
                    while l.spans.last().is_some_and(|s| s.content.trim_end().is_empty()) && l.spans.len() > 1 {
                        l.spans.pop();
                    }
                    if let Some(s) = l.spans.last_mut() {
                        *s = Span::styled(s.content.trim_end().to_string(), s.style);
                    }
                }
                lines
            })
            .collect();
        let tall = cols.iter().map(|c| c.len()).max().unwrap_or(1);
        for k in 0..tall {
            let mut sp = vec![Span::styled(format!("{indent}│"), frame)];
            for c in 0..ncol {
                let cell = cols[c].get(k).map(|l| l.spans.clone()).unwrap_or_default();
                let used = span_w(&cell);
                let pad = w[c].saturating_sub(used);
                let (l, rt) = match align.get(c) {
                    Some((true, true)) => (pad / 2, pad - pad / 2),
                    Some((false, true)) => (pad, 0),
                    _ => (0, pad),
                };
                sp.push(Span::raw(" ".repeat(l + 1)));
                sp.extend(cell);
                sp.push(Span::raw(" ".repeat(rt + 1)));
                sp.push(Span::styled("│", frame));
            }
            out.push(Line::from(sp));
        }
        if r == 0 {
            out.push(rule("├", "┼", "┤"));
        }
    }
    out.push(rule("└", "┴", "┘"));
}

fn heading(s: &str) -> Option<(usize, &str)> {
    let n = s.chars().take_while(|&c| c == '#').count();
    if n > 0 && n <= 6 && s[n..].starts_with(' ') { Some((n, s[n + 1..].trim())) } else { None }
}

fn numbered(s: &str) -> Option<(&str, &str)> {
    let n = s.chars().take_while(|c| c.is_ascii_digit()).count();
    if n > 0 && n < 4 && s[n..].starts_with(". ") { Some((&s[..n], &s[n + 2..])) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.content.to_string()).collect()
    }

    fn styled(spans: &[Span], m: Modifier) -> String {
        spans.iter().filter(|s| s.style.add_modifier.contains(m)).map(|s| s.content.to_string()).collect()
    }

    /// A lone * or _ is a character: paths, globs, maths and dunder names survive; real emphasis still works.
    #[test]
    fn chat_md_lone_stars_and_underscores() {
        let t = crate::theme::get("oriel");
        let base = Style::default();
        for s in ["run it on src/*.rs", "2 * 3 = 6", "edit __init__.py", "_private stays", "snake_case_name", "a ** b", "**not closed"] {
            let sp = inline(s, base, &t);
            assert_eq!(text(&sp), s, "{s}");
            assert!(styled(&sp, Modifier::ITALIC).is_empty() && styled(&sp, Modifier::BOLD).is_empty(), "{s}: {sp:?}");
        }
        let sp = inline("a **bold** and *it* and _this_ and __that__.", base, &t);
        assert_eq!(text(&sp), "a bold and it and this and that.");
        assert_eq!(styled(&sp, Modifier::BOLD), "boldthat");
        assert_eq!(styled(&sp, Modifier::ITALIC), "itthis");
        let sp = inline("*see `a*b` here*", base, &t);
        assert_eq!(text(&sp), "see a*b here", "a star in code doesn't close");
        let sp = inline("***both***", base, &t);
        assert_eq!((styled(&sp, Modifier::BOLD), styled(&sp, Modifier::ITALIC)), ("both".to_string(), "both".to_string()));
    }

    /// Tables draw with box lines at their natural size, wrap wide columns to fit, and fall back to one
    /// "header: value" line per cell when there's no room at all.
    #[test]
    fn chat_md_tables() {
        let t = crate::theme::get("oriel");
        let md = "Compared:\n\n| Tool | Speed | Notes |\n|------|:-----:|------:|\n| `rg` | fast | the default |\n| grep | ok | everywhere |\n\nDone.";
        let rows: Vec<String> = render(md, 40, "  ", &t).iter().map(|l| l.spans.iter().map(|s| s.content.to_string()).collect()).collect();
        println!("{}", rows.join("\n"));
        assert!(!rows.iter().any(|r| r.contains("|---") || r.contains("| Tool")), "no raw pipes");
        assert!(rows.iter().any(|r| r.contains("┌") && r.contains("┬")) && rows.iter().any(|r| r.contains("├")) && rows.iter().any(|r| r.contains("└")));
        let head = rows.iter().find(|r| r.contains("Tool")).unwrap();
        assert!(head.contains("│ Tool │") && head.contains("Speed") && head.contains("Notes"), "{head}");
        assert!(rows.iter().any(|r| r.contains("│ rg   │") && r.contains("the default │")), "code cell, right-aligned notes: {rows:?}");
        let w = |r: &String| r.width();
        let table: Vec<&String> = rows.iter().filter(|r| r.contains('│') || r.contains('─')).collect();
        assert!(table.iter().all(|r| w(r) == w(table[0]) && w(r) <= 40), "every row lines up: {table:?}");
        // too wide: the long column wraps inside its cell
        let wide = "| a | b |\n|---|---|\n| short | a much longer cell that has to wrap onto more lines |";
        let rows: Vec<String> = render(wide, 40, "", &t).iter().map(|l| l.spans.iter().map(|s| s.content.to_string()).collect()).collect();
        println!("{}", rows.join("\n"));
        assert!(rows.len() > 5 && rows.iter().all(|r| r.width() <= 40 && (r.starts_with('│') || r.starts_with('┌') || r.starts_with('├') || r.starts_with('└'))), "{rows:?}");
        assert!(rows.iter().all(|r| r.width() == rows[0].width()), "{rows:?}");
        // no room for columns at all
        let many = "| one | two | three | four |\n|---|---|---|---|\n| alpha beta | gamma delta | epsilon zeta | eta theta |";
        let rows: Vec<String> = render(many, 24, "", &t).iter().map(|l| l.spans.iter().map(|s| s.content.to_string()).collect()).collect();
        assert!(rows.iter().any(|r| r.contains("one: alpha beta")) && !rows.iter().any(|r| r.contains('│')), "{rows:?}");
    }

    /// A long code line carries on under ↪ with nothing cut; the blocks come back as written, with their headers.
    #[test]
    fn chat_md_code_wraps_and_blocks() {
        let t = crate::theme::get("oriel");
        let long = format!("let x = \"{}\";", "abcdefghij".repeat(6));
        let md = format!("Run:

```rust
{long}
short();
```

and

```
ls -la
");
        let (lines, codes) = render_full(&md, 40, "  ", &t);
        let rows: Vec<String> = lines.iter().map(|l| l.spans.iter().map(|s| s.content.to_string()).collect()).collect();
        println!("{}", rows.join("
"));
        assert!(rows.iter().all(|r| r.width() <= 40), "{rows:?}");
        assert!(!rows.iter().any(|r| r.contains('…')), "nothing cut: {rows:?}");
        let joined: String = rows.iter().filter(|r| r.starts_with("  │")).map(|r| r.trim_start_matches("  │ ").trim_start_matches("  │↪").to_string()).collect();
        assert!(joined.contains(&long), "every character is there: {joined}");
        assert!(rows.iter().filter(|r| r.starts_with("  │↪")).count() >= 1, "{rows:?}");
        assert_eq!(codes.len(), 2);
        assert!(rows[codes[0].0].contains("┌─ rust") && rows[codes[0].0].contains("click to copy"));
        assert_eq!(codes[0].1, Code { lang: "rust".into(), text: format!("{long}
short();") });
        assert_eq!(codes[1].1, Code { lang: String::new(), text: "ls -la".into() }, "an unclosed block runs to the end");
        assert!(!render(&md, 40, "", &t).iter().any(|l| l.spans.iter().any(|s| s.content.contains("click"))), "plain render: no hint");
    }
}
