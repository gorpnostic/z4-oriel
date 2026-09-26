//! The chat's diff view (/diff): every file a chat changed since a checkpoint, a file list with +/- counts on the
//! left and the selected file's hunks on the right. Drawn the way the agents app draws a task's diff (view.rs
//! draw_diff), from the same parsed `git::DiffFile`s.

use crate::panes::agents::git::{DiffFile, Kind};
use crate::theme::{Theme, mix};
use crate::ui;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_width::UnicodeWidthStr;

fn bold(c: Color) -> Style {
    Style::default().fg(c).add_modifier(Modifier::BOLD)
}

/// A colour faded towards the terminal black, for tinted backgrounds.
fn tint(c: Color, amount: f32) -> Color {
    match c {
        Color::Rgb(..) => mix(Color::Rgb(14, 14, 14), c, amount),
        _ => Color::Reset,
    }
}

/// Left and right text on one row of width `w`, the right side winning when it doesn't fit.
fn spread<'a>(left: Vec<Span<'a>>, right: Vec<Span<'a>>, w: usize) -> Line<'a> {
    let rw: usize = right.iter().map(|s| s.content.width()).sum();
    let room = w.saturating_sub(rw + if rw > 0 { 1 } else { 0 });
    let mut out = vec![];
    let mut used = 0;
    for s in left {
        if used >= room {
            break;
        }
        let text = ui::fit(&s.content, room - used);
        used += text.width();
        out.push(Span::styled(text, s.style));
    }
    out.push(Span::raw(" ".repeat(w.saturating_sub(used + rw))));
    out.extend(right);
    Line::from(out)
}

/// "+212 −40 · 6 files" for a set of files.
pub fn totals(files: &[DiffFile]) -> (u64, u64) {
    files.iter().fold((0, 0), |acc, f| (acc.0 + f.added, acc.1 + f.removed))
}

/// Draw the file list and the selected file's hunks in `main`. `file` and `scroll` are clamped; the file rows
/// come back as click targets.
pub fn draw(f: &mut Frame, main: Rect, files: &[DiffFile], file: &mut usize, scroll: &mut usize, t: &Theme) -> Vec<(Rect, usize)> {
    let mut hits = vec![];
    if files.is_empty() {
        f.render_widget(Paragraph::new(vec![Line::raw(""), Line::styled("no changes since then", ui::muted(t))]).centered(), main);
        return hits;
    }
    *file = (*file).min(files.len() - 1);
    let lw = (main.width / 3).clamp(24, 44).min(main.width.saturating_sub(20));
    let lr = Rect { width: lw, ..main };
    let rr = Rect { x: main.x + lw + 1, width: main.width.saturating_sub(lw + 1), ..main };
    // file list
    let li = ui::frame(f, lr, &format!("{}files", ui::lead("files")), Some(&format!("{}/{}", *file + 1, files.len())), false, t);
    let h = li.height as usize;
    let start = file.saturating_sub(h.saturating_sub(1));
    for (row, (i, df)) in files.iter().enumerate().skip(start).take(h).enumerate() {
        let r = Rect { y: li.y + row as u16, height: 1, ..li };
        let on = i == *file;
        let name = df.path.rsplit(['/', '\\']).next().unwrap_or(&df.path).to_string();
        let dir = df.path.strip_suffix(&name).unwrap_or("").to_string();
        let mark = match df.note.as_str() {
            "new" => "+ ",
            "deleted" => "− ",
            _ => "  ",
        };
        let mut line = spread(
            vec![Span::styled(mark, Style::default().fg(if mark == "− " { t.danger } else { t.good })), Span::styled(dir, ui::muted(t)), Span::styled(name, if on { bold(t.accent) } else { Style::default().fg(t.fg) })],
            vec![Span::styled(format!("+{}", df.added), Style::default().fg(t.good)), Span::styled(format!(" −{}", df.removed), Style::default().fg(t.danger))],
            r.width as usize,
        );
        if on {
            line = line.style(Style::default().bg(tint(t.accent, 0.18)));
        }
        f.render_widget(Paragraph::new(line), r);
        hits.push((r, i));
    }
    // hunks
    let df = &files[*file];
    let sub = if df.note.is_empty() { format!("+{} −{}", df.added, df.removed) } else { format!("{} · +{} −{}", df.note, df.added, df.removed) };
    let ri = ui::frame(f, rr, &format!("{}{}", ui::lead("code"), df.path), Some(&sub), true, t);
    let gw = df.lines.iter().filter_map(|l| l.old.max(l.new)).max().unwrap_or(1).to_string().len().max(3);
    let h = ri.height as usize;
    let max_scroll = df.lines.len().saturating_sub(h);
    *scroll = (*scroll).min(max_scroll);
    let (add_bg, del_bg) = (tint(t.good, 0.16), tint(t.danger, 0.16));
    let tw = (ri.width as usize).saturating_sub(gw * 2 + 4);
    let num = |n: Option<u32>| n.map(|x| format!("{x:>gw$}")).unwrap_or_else(|| " ".repeat(gw));
    let mut lines = vec![];
    for l in df.lines.iter().skip(*scroll).take(h) {
        let text = ui::fit(&l.text.replace('\t', "    "), tw);
        let pad = tw.saturating_sub(text.width());
        lines.push(match l.kind {
            Kind::Hunk => Line::from(vec![
                Span::styled(format!("{} ", "┄".repeat(gw * 2 + 1)), Style::default().fg(t.frame)),
                Span::styled(format!("@@ -{} +{} ", l.old.unwrap_or(0), l.new.unwrap_or(0)), ui::accent(t)),
                Span::styled(ui::fit(&l.text, tw.saturating_sub(14)), ui::muted(t)),
            ]),
            Kind::Note => Line::styled(format!("  {}", l.text), ui::muted(t)),
            Kind::Add => Line::from(vec![
                Span::styled(format!("{} {} ", num(None), num(l.new)), Style::default().fg(t.good).bg(add_bg)),
                Span::styled("+ ", bold(t.good).bg(add_bg)),
                Span::styled(format!("{text}{}", " ".repeat(pad)), Style::default().fg(t.good).bg(add_bg)),
            ]),
            Kind::Del => Line::from(vec![
                Span::styled(format!("{} {} ", num(l.old), num(None)), Style::default().fg(t.danger).bg(del_bg)),
                Span::styled("- ", bold(t.danger).bg(del_bg)),
                Span::styled(format!("{text}{}", " ".repeat(pad)), Style::default().fg(t.danger).bg(del_bg)),
            ]),
            Kind::Ctx => Line::from(vec![Span::styled(format!("{} {} ", num(l.old), num(l.new)), Style::default().fg(t.frame)), Span::raw("  "), Span::styled(text, Style::default().fg(t.fg))]),
        });
    }
    f.render_widget(Paragraph::new(lines), ri);
    if max_scroll > 0 {
        let pct = *scroll * 100 / max_scroll.max(1);
        let r = Rect { x: rr.x + 2, y: rr.bottom().saturating_sub(1), width: 12.min(rr.width.saturating_sub(4)), height: 1 };
        f.render_widget(Paragraph::new(Span::styled(format!(" {pct}% "), ui::muted(t))), r);
    }
    hits
}
