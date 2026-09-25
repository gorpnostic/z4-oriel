//! Drawing, keys and mouse for the storage pane (the state and background work live in storage.rs).

use super::catalog::{self, CATALOG, CATS, Src};
use super::{Kind, NV, Storage, TOP, VIEWS, View, human};
use crate::pane::{Cx, Pane};
use crate::theme::Theme;
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::sync::atomic::Ordering;

const AMBER: Color = Color::Rgb(0xe0, 0xa0, 0x40);

/// One table cell: styled segments (the size bar is two colours).
type Cell = Vec<(String, Style)>;
type Row = Vec<Cell>;

fn cell(s: impl Into<String>, st: Style) -> Cell {
    vec![(s.into(), st)]
}

struct Col {
    title: &'static str,
    /// 0 = takes what's left
    w: u16,
    right: bool,
}

fn col(title: &'static str, w: u16) -> Col {
    Col { title, w, right: false }
}

/// A nest DataTable: bold header, one line per row, the selected row highlighted, scrolled to keep it in view.
/// Returns the body rect (for mouse hits).
fn table(f: &mut Frame, r: Rect, cols: &[Col], rows: &[Row], sel: Option<usize>, off: &mut usize, focused: bool, t: &Theme) -> Rect {
    if r.height < 2 || r.width < 4 {
        return Rect::default();
    }
    let fixed: u16 = cols.iter().filter(|c| c.w > 0).map(|c| c.w + 1).sum();
    let nflex = cols.iter().filter(|c| c.w == 0).count().max(1) as u16;
    let flex_w = r.width.saturating_sub(fixed + nflex) / nflex;
    let widths: Vec<usize> = cols.iter().map(|c| if c.w == 0 { flex_w } else { c.w } as usize).collect();
    let line = |cells: &[Cell], base: Option<Style>| -> Line<'static> {
        let fix = |st: Style| match base {
            Some(b) => {
                // muted text would vanish on the highlight: lift it to the body colour
                let st = if st.fg == Some(t.muted) || st.fg == Some(t.frame) { st.fg(Color::Reset) } else { st };
                st.patch(b)
            }
            None => st,
        };
        let gap = fix(Style::default());
        let mut spans = vec![];
        for (k, c) in cells.iter().enumerate() {
            let w = widths.get(k).copied().unwrap_or(0);
            if w == 0 {
                continue;
            }
            let mut segs = vec![];
            let mut left = w;
            for (s, st) in c {
                if left == 0 {
                    break;
                }
                let sw = unicode_width::UnicodeWidthStr::width(s.as_str());
                let s = if sw > left { ui::fit(s, left) } else { s.clone() };
                left = left.saturating_sub(unicode_width::UnicodeWidthStr::width(s.as_str()));
                segs.push(Span::styled(s, fix(*st)));
            }
            let pad = Span::styled(" ".repeat(left), gap);
            if cols[k].right {
                spans.push(pad);
                spans.extend(segs);
            } else {
                spans.extend(segs);
                spans.push(pad);
            }
            spans.push(Span::styled(" ", gap));
        }
        Line::from(spans)
    };
    let head: Row = cols.iter().map(|c| cell(c.title, Style::default().add_modifier(Modifier::BOLD))).collect();
    f.render_widget(Paragraph::new(line(&head, None)), Rect { height: 1, ..r });
    let body = Rect { y: r.y + 1, height: r.height - 1, ..r };
    let h = body.height as usize;
    if let Some(s) = sel {
        if s < *off {
            *off = s;
        } else if s >= *off + h {
            *off = s + 1 - h;
        }
    }
    *off = (*off).min(rows.len().saturating_sub(h));
    let hi = if focused { Style::default().bg(t.frame).add_modifier(Modifier::BOLD) } else { Style::default().add_modifier(Modifier::BOLD) };
    for (k, row) in rows.iter().skip(*off).take(h).enumerate() {
        let on = sel == Some(*off + k);
        f.render_widget(Paragraph::new(line(row, on.then_some(hi))), Rect { y: body.y + k as u16, height: 1, ..body });
    }
    body
}

fn empty_note(f: &mut Frame, r: Rect, text: &str, t: &Theme) {
    if r.height > 1 {
        f.render_widget(Paragraph::new(Span::styled(text.to_string(), ui::muted(t))), Rect { y: r.y + 1, height: 1, ..r });
    }
}

impl Storage {
    fn view_label(&self, v: View) -> (&'static str, String) {
        match v {
            View::Cleanup => ("gauge", "cleanup".into()),
            View::Folders => ("files", "big folders".into()),
            View::Apps => ("package", "installed apps".into()),
            View::Catalog => ("cloud", "get apps".into()),
            View::Install => ("search", format!("search {}", self.search_tool())),
        }
    }

    fn search_tool(&self) -> &'static str {
        match self.env.os {
            catalog::Os::Windows => "winget",
            catalog::Os::Arch => self.env.aur_helper.unwrap_or("pacman"),
            _ => "apt",
        }
    }

    /// Catalog entries shown on this OS, and how many of them are installed.
    fn catalog_counts(&self) -> (usize, usize) {
        let shown = self.cat_count(0);
        let inst = self.installed.as_ref().map(|inst| (0..CATALOG.len()).filter(|&i| self.picks[i].is_some() && inst.has(&CATALOG[i])).count()).unwrap_or(0);
        (shown, inst)
    }

    fn cleanable(&self) -> u64 {
        self.targets.iter().filter(|t| t.kind == Kind::Clean).map(|t| self.size_of(t.id).0).sum()
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.confirm.is_some() {
            return vec![("y", "yes"), ("esc", "no")];
        }
        if self.input {
            return match self.view {
                View::Install => vec![("enter", "search"), ("esc", "back to the results")],
                View::Catalog => vec![("type", "to filter"), ("enter", "done (or search, if nothing matches)"), ("esc", "clear")],
                _ => vec![("type", "to filter"), ("enter", "done"), ("esc", "clear")],
            };
        }
        match self.view {
            View::Catalog => vec![("enter", "install"), ("tab / 1-9", "category"), ("/", "filter"), ("s", "search for anything else"), ("o", "homepage"), ("r", "recheck")],
            View::Cleanup => vec![("x", "clean the selected item"), ("enter", "review it in files"), ("r", "rescan"), ("tab", "next view")],
            View::Folders => vec![("enter", "open folder"), ("backspace", "up"), ("o", "show in files"), ("r", "rescan")],
            View::Apps => vec![("u", "uninstall"), ("/", "filter"), ("r", "reload"), ("tab", "next view")],
            View::Install => vec![("/", "search"), ("enter", "install the selected one"), ("tab", "next view")],
        }
    }

    fn draw_drives(&self, f: &mut Frame, r: Rect, t: &Theme) {
        let inner = ui::frame(f, r, &format!("{}drives", ui::lead("chart")), None, false, t);
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
        let Some(drives) = &self.drives else {
            f.render_widget(Paragraph::new(Span::styled("reading drives…", ui::muted(t))), Rect { height: 1, ..inner });
            return;
        };
        let nw = drives.iter().map(|d| d.name.chars().count()).max().unwrap_or(2).clamp(2, 16);
        for (k, d) in drives.iter().enumerate() {
            let y = inner.y + k as u16;
            if y >= inner.bottom() {
                break;
            }
            let pct = if d.total > 0 { 100.0 * (d.total - d.free.min(d.total)) as f64 / d.total as f64 } else { 0.0 };
            let colr = if pct >= 95.0 { t.danger } else if pct >= 85.0 { AMBER } else { t.accent };
            let text = format!("{} free of {}", human(d.free), human(d.total));
            let full = pct >= 95.0;
            let bar_w = (inner.width as usize).saturating_sub(nw + 4 + text.len() + 13).clamp(6, 40);
            let n = (((pct / 100.0) * bar_w as f64).round() as usize).min(bar_w);
            let mut spans = vec![
                Span::styled(format!("{:<nw$}  ", ui::fit(&d.name, nw)), Style::default().add_modifier(Modifier::BOLD)),
                Span::styled("█".repeat(n), Style::default().fg(colr)),
                Span::styled("░".repeat(bar_w - n), Style::default().fg(t.frame)),
                Span::styled(format!("  {text}"), if full { Style::default().fg(colr).add_modifier(Modifier::BOLD) } else { ui::muted(t) }),
            ];
            if full {
                spans.push(Span::styled("  almost full", Style::default().fg(colr).add_modifier(Modifier::BOLD)));
            } else if pct >= 85.0 {
                spans.push(Span::styled(format!("  {pct:.0}% used"), Style::default().fg(colr)));
            }
            f.render_widget(Paragraph::new(Line::from(spans)), Rect { y, height: 1, ..inner });
        }
    }

    fn draw_box(&self, f: &mut Frame, r: Rect, t: &Theme) {
        let (title, value, ph) = match self.view {
            View::Apps => ("filter", &self.filter, "filter installed apps…   (/)".to_string()),
            View::Catalog => ("filter", &self.cfilter, format!("filter the catalog…   (/)      not here? press s to search {} for anything", self.search_tool())),
            _ => ("search", &self.query, format!("search {}, press enter…   (/)", self.install_via)),
        };
        let inner = ui::frame(f, r, &format!("{}{title}", ui::lead("search")), None, self.input, t);
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
        let caret = Span::styled("▏", ui::accent(t));
        let line = match (value.is_empty(), self.input) {
            (true, false) => Line::from(Span::styled(ph, ui::muted(t))),
            (true, true) => Line::from(vec![caret, Span::styled(ph, ui::muted(t))]),
            (false, false) => Line::from(Span::raw(value.clone())),
            (false, true) => Line::from(vec![Span::raw(value.clone()), caret]),
        };
        f.render_widget(Paragraph::new(line), Rect { height: 1, ..inner });
    }

    fn draw_table(&mut self, f: &mut Frame, r: Rect, focused: bool, t: &Theme) {
        let order = self.order(self.view);
        let sel_pos = self.selected().and_then(|s| order.iter().position(|&i| i == s));
        let muted = ui::muted(t);
        let plain = Style::default();
        let (cols, rows, empty): (Vec<Col>, Vec<Row>, Option<String>) = match self.view {
            View::Cleanup => {
                let cols = vec![col("what", 26), col("size", 11), col("", 7), col("why", 0)];
                let rows = order
                    .iter()
                    .map(|&i| {
                        let tg = &self.targets[i];
                        let (b, done) = self.size_of(tg.id);
                        let size = if done { cell(human(b), plain) } else if b > 0 { cell(format!("{}…", human(b)), muted) } else { cell("…", muted) };
                        let kind = if tg.kind == Kind::Clean { cell("clean", ui::bold_accent(t)) } else { cell("review", muted) };
                        vec![cell(tg.label.clone(), plain), size, kind, cell(tg.note.clone(), muted)]
                    })
                    .collect();
                (cols, rows, self.targets.is_empty().then(|| "looking for things to clean…".to_string()))
            }
            View::Folders => {
                let cols = vec![col("", 20), Col { title: "size", w: 10, right: true }, col("folder / file", 0)];
                let top = order.first().map(|&i| self.frows[i].size).unwrap_or(1).max(1);
                let rows = order
                    .iter()
                    .map(|&i| {
                        let fr = &self.frows[i];
                        let n = ((20 * fr.size as u128) / top as u128) as usize;
                        let fill = Style::default().fg(if fr.dir { t.accent } else { t.muted });
                        let bar = vec![("█".repeat(n.min(20)), fill), ("░".repeat(20 - n.min(20)), Style::default().fg(t.frame))];
                        let size = if fr.done { cell(human(fr.size), plain) } else { cell(format!("{}…", human(fr.size)), muted) };
                        let name = if fr.dir { cell(format!("{}{}", ui::lead("files"), fr.name), plain) } else { cell(format!("  {}", fr.name), muted) };
                        vec![bar, size, name]
                    })
                    .collect();
                let empty = if let Some(e) = &self.ferr {
                    Some(e.clone())
                } else if self.frows.is_empty() {
                    Some(if self.fdone { "this folder is empty".into() } else { format!("measuring {} …", self.froot.display()) })
                } else {
                    None
                };
                (cols, rows, empty)
            }
            View::Apps => {
                let who = if cfg!(windows) { "publisher" } else { "description" };
                let cols = vec![
                    col("app", 0),
                    col("version", 16),
                    col(who, if cfg!(windows) { 26 } else { 40 }),
                    Col { title: "size", w: 10, right: true },
                    col("installed", 11),
                ];
                let (rows, empty) = match &self.apps {
                    None => (vec![], Some("reading installed apps…".to_string())),
                    Some(Err(e)) => (vec![], Some(e.clone())),
                    Some(Ok(a)) => {
                        let rows: Vec<Row> = order
                            .iter()
                            .map(|&i| {
                                let a = &a[i];
                                vec![
                                    cell(a.name.clone(), plain),
                                    cell(a.version.clone(), muted),
                                    cell(a.publisher.clone(), muted),
                                    cell(if a.size > 0 { human(a.size) } else { String::new() }, plain),
                                    cell(a.date.clone(), muted),
                                ]
                            })
                            .collect();
                        let e = rows.is_empty().then(|| format!("no apps match “{}”", self.filter));
                        (rows, e)
                    }
                };
                (cols, rows, empty)
            }
            View::Catalog => {
                let show_cat = self.cat == 0 || !self.cfilter.trim().is_empty();
                let mut cols = vec![col("app", 20), col("what it is", 0)];
                if show_cat {
                    cols.push(col("category", 14));
                }
                cols.extend([col("via", 8), col("", 12)]);
                let mut prev = None;
                let rows = order
                    .iter()
                    .map(|&i| {
                        let e = &CATALOG[i];
                        // the category label only on the first row of each group
                        let first = prev != Some(e.cat);
                        prev = Some(e.cat);
                        let p = self.picks[i].as_ref().expect("order only lists picked entries");
                        let inst = self.installed.as_ref().map(|inst| inst.has(e));
                        let usable = p.install.is_some() || p.src == Src::Web;
                        let name_st = if usable { plain } else { muted };
                        let via_st = match p.src {
                            _ if !usable => muted,
                            Src::Web => muted,
                            _ => ui::accent(t),
                        };
                        let status = match (&p.missing, inst) {
                            (Some(m), _) => cell(m.split(" (").next().unwrap_or(m).to_string(), Style::default().fg(AMBER)),
                            (None, Some(true)) => cell("✓ installed", ui::bold_accent(t)),
                            (None, None) => cell("…", muted),
                            (None, Some(false)) if p.src == Src::Web => cell("opens page", muted),
                            (None, Some(false)) => cell("", muted),
                        };
                        let mut row = vec![cell(e.name, name_st), cell(e.desc, muted)];
                        if show_cat {
                            row.push(cell(if first { e.cat.label() } else { "" }, muted));
                        }
                        row.extend([cell(p.src.label(), via_st), status]);
                        row
                    })
                    .collect();
                let empty = order.is_empty().then(|| {
                    if self.cfilter.trim().is_empty() {
                        format!("nothing in this category is packaged for {}", self.env.os.label())
                    } else {
                        format!("no catalog app matches “{}” — press enter to search {} for it", self.cfilter.trim(), self.search_tool())
                    }
                });
                (cols, rows, empty)
            }
            View::Install => {
                let cols = if cfg!(windows) {
                    vec![col("name", 0), col("winget id", 40), col("version", 16), col("source", 8)]
                } else {
                    vec![col("package", 30), col("version", 18), col("repo", 12), col("description", 0)]
                };
                let (rows, empty) = match &self.found {
                    _ if self.searching => (vec![], Some(format!("searching {} for “{}”…", self.install_via, self.found_q))),
                    None => (vec![], Some(format!("type what you're looking for and press enter — results come from {}", self.install_via))),
                    Some(Err(e)) => (vec![], Some(e.clone())),
                    Some(Ok(v)) => {
                        let rows: Vec<Row> = v
                            .iter()
                            .map(|x| {
                                if cfg!(windows) {
                                    vec![cell(x.name.clone(), plain), cell(x.id.clone(), muted), cell(x.version.clone(), plain), cell(x.source.clone(), muted)]
                                } else {
                                    vec![cell(x.name.clone(), plain), cell(x.version.clone(), muted), cell(x.source.clone(), ui::accent(t)), cell(x.desc.clone(), muted)]
                                }
                            })
                            .collect();
                        let e = rows.is_empty().then(|| format!("nothing found for “{}”", self.found_q));
                        (rows, e)
                    }
                };
                (cols, rows, empty)
            }
        };
        let vi = self.view.i();
        let mut off = self.off[vi];
        let body = table(f, r, &cols, &rows, sel_pos, &mut off, focused && !self.input, t);
        self.off[vi] = off;
        self.table_hit = Some((body, off));
        if let Some(e) = empty {
            empty_note(f, r, &e, t);
        }
    }

    /// The catalog's category column: "all" then each category with how many apps it offers here.
    fn draw_cats(&mut self, f: &mut Frame, r: Rect, focused: bool, t: &Theme) {
        self.cat_hits.clear();
        let inner = ui::frame(f, r, "categories", None, false, t);
        let filtering = !self.cfilter.trim().is_empty();
        for c in 0..=CATS.len() {
            let y = inner.y + c as u16;
            if y >= inner.bottom() {
                break;
            }
            let row = Rect { y, height: 1, ..inner };
            let label = if c == 0 { "all" } else { CATS[c - 1].label() };
            let n = self.cat_count(c);
            let on = self.cat == c && !filtering;
            let w = row.width as usize;
            let key = format!("{} ", c + 1);
            let left = format!(" {key}{label}");
            let right = format!("{n} ");
            let pad = w.saturating_sub(unicode_width::UnicodeWidthStr::width(left.as_str()) + right.len());
            let (base, keyst, numst) = if on {
                let b = if focused { Style::default().bg(t.frame) } else { Style::default() };
                (b.fg(t.accent).add_modifier(Modifier::BOLD), b.fg(t.accent), b.fg(t.accent))
            } else if n == 0 {
                (ui::muted(t), Style::default().fg(t.frame), Style::default().fg(t.frame))
            } else {
                (Style::default(), ui::muted(t), ui::muted(t))
            };
            let line = Line::from(vec![
                Span::styled(format!(" {key}"), keyst),
                Span::styled(ui::fit(label, w.saturating_sub(right.len() + 4)), base),
                Span::styled(" ".repeat(pad), base),
                Span::styled(right, numst),
            ]);
            f.render_widget(Paragraph::new(line), row);
            self.cat_hits.push((row, c));
        }
        // how many are installed, under the list
        let (shown, inst) = self.catalog_counts();
        let y = inner.y + CATS.len() as u16 + 2;
        if y < inner.bottom() {
            let text = match &self.installed {
                Some(_) => format!(" {inst} of {shown} installed"),
                None => " checking installed…".into(),
            };
            f.render_widget(Paragraph::new(Span::styled(ui::fit(&text, inner.width as usize), ui::muted(t))), Rect { y, height: 1, ..inner });
        }
        let hidden = CATALOG.len() - shown;
        if hidden > 0 && y + 1 < inner.bottom() {
            let text = format!(" {hidden} not for {}", self.env.os.label());
            f.render_widget(Paragraph::new(Span::styled(ui::fit(&text, inner.width as usize), Style::default().fg(t.frame))), Rect { y: y + 1, height: 1, ..inner });
        }
    }

    fn draw_confirm(&self, f: &mut Frame, area: Rect, t: &Theme) {
        let Some(c) = &self.confirm else { return };
        let longest = c.lines.iter().chain([&c.question]).map(|l| unicode_width::UnicodeWidthStr::width(l.as_str())).max().unwrap_or(0) as u16;
        let w = (longest + 4).max(84).min(area.width.saturating_sub(4)).max(20);
        let h = c.lines.len() as u16 + 6;
        let inner = ui::popup(f, area, w, h, "are you sure?", t);
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
        let iw = inner.width as usize;
        let mut lines = vec![Line::from(Span::styled(ui::fit(&c.question, iw), Style::default().fg(t.danger).add_modifier(Modifier::BOLD))), Line::raw("")];
        for l in &c.lines {
            lines.push(Line::from(Span::styled(ui::fit(l, iw), ui::muted(t))));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from([ui::key_hint("y", "yes", t), ui::key_hint("esc", "no", t)].concat()));
        f.render_widget(Paragraph::new(lines), inner);
    }
}

impl Pane for Storage {
    fn title(&self) -> String {
        "storage".into()
    }
    fn icon(&self) -> &'static str {
        "storage"
    }

    fn subtitle(&self) -> Option<String> {
        Some(match self.view {
            View::Cleanup => {
                let left = self.targets.iter().filter(|t| !self.size_of(t.id).1).count();
                if self.targets.is_empty() {
                    "scanning…".into()
                } else if left > 0 {
                    format!("scanning… {left} left")
                } else {
                    format!("{} safe to clean · select a row and press x", human(self.cleanable()))
                }
            }
            View::Folders => {
                let total: u64 = self.frows.iter().map(|r| r.size).sum();
                format!("{} · {}{}", self.froot.display(), human(total), if self.fdone { "" } else { " · measuring…" })
            }
            View::Apps => match &self.apps {
                Some(Ok(a)) => format!("{} of {} apps · {} reported", self.order(View::Apps).len(), a.len(), human(a.iter().map(|a| a.size).sum())),
                Some(Err(_)) => "couldn't read installed apps".into(),
                None => "reading installed apps…".into(),
            },
            View::Catalog => {
                let (shown, inst) = self.catalog_counts();
                let what = if self.cfilter.trim().is_empty() {
                    if self.cat == 0 { "all".to_string() } else { CATS[self.cat - 1].label().to_string() }
                } else {
                    format!("“{}”", self.cfilter.trim())
                };
                let inst = if self.installed.is_some() { format!("{inst} installed") } else { "checking what's installed…".into() };
                format!("{shown} popular apps for {} · {what} · {inst} · enter installs (asks first)", self.env.os.label())
            }
            View::Install => match &self.found {
                _ if self.searching => format!("searching {} for “{}”…", self.install_via, self.found_q),
                Some(Ok(v)) => format!("{} results for “{}” · select one and press enter", v.len(), self.found_q),
                _ => format!("install from {}", self.install_via),
            },
        })
    }

    fn badge(&self) -> Option<String> {
        let d = self.drives.as_ref()?;
        let sys = d.iter().find(|d| d.name == "C:" || d.name == "/").or(d.first())?;
        Some(format!("{} free", ui::human_bytes(sys.free)))
    }

    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        self.ensure(cx);
        let t = cx.theme;
        let body = ui::hint_line(f, area, &self.hints(), t);
        let body = Rect { height: body.height.saturating_sub(1), ..body }; // breathing room above the hints
        let mut y = body.y;
        let n = self.drives.as_ref().map(|d| d.len()).unwrap_or(1).max(1) as u16;
        let card_h = n + 2;
        if body.height >= card_h + 5 {
            self.draw_drives(f, Rect { y, height: card_h, ..body }, t);
            y += card_h + 1;
        }
        if matches!(self.view, View::Apps | View::Catalog | View::Install) && body.bottom() > y + 5 {
            self.draw_box(f, Rect { y, height: 3, ..body }, t);
            y += 3;
        }
        let mut bottom = body.bottom();
        if self.view == View::Folders && self.fdone && !self.fbig.is_empty() && bottom > y + 4 {
            bottom -= 2;
            let names: Vec<String> = self
                .fbig
                .iter()
                .take(5)
                .map(|(s, p)| format!("{} {}", p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), human(*s)))
                .collect();
            let line = Line::from(vec![Span::styled(" biggest files in here: ", ui::bold_accent(t)), Span::styled(names.join("  ·  "), ui::muted(t))]);
            f.render_widget(Paragraph::new(line), Rect { y: bottom + 1, height: 1, ..body });
        }
        let mut tr = Rect { x: body.x + 1, y, width: body.width.saturating_sub(2), height: bottom.saturating_sub(y) };
        if self.view == View::Catalog && tr.width >= 80 && tr.height >= 4 {
            let cw = 24;
            self.draw_cats(f, Rect { x: body.x, width: cw, ..tr }, cx.focused && !self.input, t);
            tr = Rect { x: body.x + cw + 2, width: tr.width.saturating_sub(cw + 1), ..tr };
        } else {
            self.cat_hits.clear();
        }
        self.draw_table(f, tr, cx.focused, t);
        self.draw_confirm(f, area, t);
    }

    fn key(&mut self, key: KeyEvent, cx: &mut Cx) -> bool {
        self.ensure(cx);
        if key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) {
            return false;
        }
        if self.confirm.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if let Some(c) = self.confirm.take() {
                        self.execute(c.what, cx);
                    }
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.confirm = None;
                    cx.notify("cancelled");
                }
                _ => {}
            }
            return true; // nothing else happens while a question is open
        }
        if self.input && self.view == View::Catalog {
            match key.code {
                KeyCode::Char(c) => self.cfilter.push(c),
                KeyCode::Backspace => {
                    self.cfilter.pop();
                }
                KeyCode::Esc => {
                    self.cfilter.clear();
                    self.input = false;
                }
                KeyCode::Enter => {
                    self.input = false;
                    let q = self.cfilter.trim().to_string();
                    if !q.is_empty() && self.order(View::Catalog).is_empty() {
                        self.cfilter.clear();
                        self.search_for(q);
                    }
                }
                KeyCode::Up => self.step(-1),
                KeyCode::Down => self.step(1),
                KeyCode::Tab => self.input = false,
                _ => {}
            }
            if !matches!(key.code, KeyCode::Up | KeyCode::Down) {
                self.sel[View::Catalog.i()] = TOP;
                self.off[View::Catalog.i()] = 0;
            }
            return true;
        }
        if self.input {
            let apps = self.view == View::Apps;
            let text = if apps { &mut self.filter } else { &mut self.query };
            match key.code {
                KeyCode::Char(c) => text.push(c),
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Esc => {
                    if apps {
                        self.filter.clear();
                    }
                    self.input = false;
                }
                KeyCode::Enter => {
                    self.input = false;
                    if !apps {
                        self.search();
                    }
                }
                KeyCode::Up => self.step(-1),
                KeyCode::Down => self.step(1),
                KeyCode::Tab => self.input = false,
                _ => {}
            }
            if apps && !matches!(key.code, KeyCode::Up | KeyCode::Down) {
                self.sel[View::Apps.i()] = self.order(View::Apps).first().copied().unwrap_or(0);
            }
            return true;
        }
        if self.view == View::Catalog {
            let n = CATS.len();
            match key.code {
                // tab walks the categories, then on to the next view (backtab: back out the other side)
                KeyCode::Tab if self.cat < n => self.set_cat(self.cat + 1),
                KeyCode::BackTab if self.cat > 0 => self.set_cat(self.cat - 1),
                KeyCode::Right | KeyCode::Char('l') => self.set_cat((self.cat + 1) % (n + 1)),
                KeyCode::Left | KeyCode::Char('h') => self.set_cat((self.cat + n) % (n + 1)),
                KeyCode::Char(c @ '1'..='9') => self.set_cat(c as usize - '1' as usize),
                KeyCode::Char('s') => self.search_for(self.cfilter.trim().to_string()),
                KeyCode::Char('o') => {
                    if let Some(i) = self.selected() {
                        self.open_url(CATALOG[i].home, cx);
                    }
                }
                KeyCode::Char('i') => self.ask_get(cx),
                _ => return self.key_common(key, cx),
            }
            return true;
        }
        self.key_common(key, cx)
    }

    fn mouse(&mut self, ev: MouseEvent, _area: Rect, cx: &mut Cx) {
        match ev.kind {
            MouseEventKind::ScrollDown => self.step(3),
            MouseEventKind::ScrollUp => self.step(-3),
            MouseEventKind::Down(MouseButton::Left) if self.confirm.is_none() => {
                let at = Position { x: ev.column, y: ev.row };
                if let Some(&(_, c)) = self.cat_hits.iter().find(|(r, _)| r.contains(at)) {
                    self.set_cat(c);
                    return;
                }
                if let Some((body, off)) = self.table_hit {
                    if body.contains(at) {
                        let pos = off + (ev.row - body.y) as usize;
                        if let Some(&i) = self.order(self.view).get(pos) {
                            self.sel[self.view.i()] = i;
                            // double-click on a catalog app = enter (which still asks before installing)
                            let now = std::time::Instant::now();
                            let double = self.last_click.is_some_and(|(t, j)| j == i && now.duration_since(t).as_millis() < 450);
                            self.last_click = Some((now, i));
                            if double && self.view == View::Catalog {
                                self.last_click = None;
                                self.ask_get(cx);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn poll(&mut self, cx: &mut Cx) {
        self.ensure(cx);
        self.drain(cx);
    }

    fn side(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        self.ensure(cx);
        self.side_hits.clear();
        for (k, &v) in VIEWS.iter().enumerate() {
            let y = area.y + k as u16;
            if y >= area.bottom() {
                break;
            }
            let r = Rect { y, height: 1, ..area };
            let (icon, label) = self.view_label(v);
            let right = match v {
                View::Cleanup if !self.targets.is_empty() => ui::human_bytes(self.cleanable()),
                View::Apps => match &self.apps {
                    Some(Ok(a)) => a.len().to_string(),
                    _ => String::new(),
                },
                View::Catalog => self.cat_count(0).to_string(),
                _ => String::new(),
            };
            ui::side_row(f, r, icon, &label, &right, v == self.view, cx.theme);
            self.side_hits.push((r, v));
        }
    }

    fn side_mouse(&mut self, ev: MouseEvent, _area: Rect, cx: &mut Cx) {
        if let MouseEventKind::Down(MouseButton::Left) = ev.kind {
            if let Some(&(_, v)) = self.side_hits.iter().find(|(r, _)| r.contains(Position { x: ev.column, y: ev.row })) {
                self.ensure(cx);
                self.set_view(v);
            }
        }
    }
}

impl Storage {
    /// Keys every view shares (after the catalog has had first go at tab / digits).
    fn key_common(&mut self, key: KeyEvent, cx: &mut Cx) -> bool {
        match key.code {
            KeyCode::Tab => self.set_view(VIEWS[(self.view.i() + 1) % NV]),
            KeyCode::BackTab => {
                self.set_view(VIEWS[(self.view.i() + NV - 1) % NV]);
                if self.view == View::Catalog {
                    self.set_cat(CATS.len()); // coming back in from the right: the last category
                }
            }
            KeyCode::Char(c @ '1'..='5') => self.set_view(VIEWS[c as usize - '1' as usize]),
            KeyCode::Up | KeyCode::Char('k') => self.step(-1),
            KeyCode::Down | KeyCode::Char('j') => self.step(1),
            KeyCode::PageUp => self.step(-10),
            KeyCode::PageDown => self.step(10),
            KeyCode::Home | KeyCode::Char('g') => self.step(-1_000_000),
            KeyCode::End | KeyCode::Char('G') => self.step(1_000_000),
            KeyCode::Char('r') => self.rescan(),
            KeyCode::Char('/') if matches!(self.view, View::Apps | View::Catalog | View::Install) => self.input = true,
            KeyCode::Char('x') if self.view == View::Cleanup => self.ask_clean(cx),
            KeyCode::Char('u') if self.view == View::Apps => self.ask_uninstall(),
            KeyCode::Char('o') if self.view == View::Folders => {
                let p = self.selected().map(|i| self.frows[i].clone()).filter(|r| r.dir).map(|r| r.path).unwrap_or_else(|| self.froot.clone());
                self.open_files(p, cx);
            }
            KeyCode::Backspace if self.view == View::Folders => self.up(),
            KeyCode::Enter => self.enter(cx),
            _ => return false,
        }
        true
    }
}

impl Drop for Storage {
    fn drop(&mut self) {
        self.tstop.store(true, Ordering::Relaxed);
        self.fstop.store(true, Ordering::Relaxed);
    }
}
