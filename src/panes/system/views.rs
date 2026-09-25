//! Drawing for the system app's non-process views (performance, startup, services, connections, system info)
//! plus the table and graph helpers every view shares. Nothing here collects data: it all comes from the
//! sampler's snapshot or the probes' last result.

use super::probes::{Conn, Service, StartupItem};
use super::sampler::{GpuState, HISTORY};
use super::{SPARK, System, WARN, card, gib, meter, split_h};
use crate::theme::Theme;
use crate::ui;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_width::UnicodeWidthStr;

// ------------------------------------------------------------------ graphs

/// A filled area graph `h` rows tall and `w` columns wide over the last `window` samples (newest on the
/// right). With more samples than columns each column averages its share; missing history stays blank.
pub fn graph<T: Copy + Into<f64>>(vals: &std::collections::VecDeque<T>, window: usize, w: usize, h: usize, top: f64, t: &Theme) -> Vec<Line<'static>> {
    if w == 0 || h == 0 {
        return vec![];
    }
    let n = vals.len();
    let window = window.max(1);
    // per column: Some(value) or None (no data yet)
    let cols: Vec<Option<f64>> = (0..w)
        .map(|c| {
            if window <= w {
                // one sample per column, right-aligned
                let back = w - 1 - c; // 0 = newest
                (back < window && back < n).then(|| vals[n - 1 - back].into())
            } else {
                let (a, b) = (c * window / w, ((c + 1) * window / w).max(c * window / w + 1));
                // sample index `k` of the window counts from the oldest; window start = n - window
                let (mut sum, mut cnt) = (0.0, 0);
                for k in a..b {
                    if let Some(i) = (n + k).checked_sub(window) {
                        sum += vals[i].into();
                        cnt += 1;
                    }
                }
                (cnt > 0).then(|| sum / cnt as f64)
            }
        })
        .collect();
    let top = top.max(1e-9);
    // height of each column in eighths of a row
    let levels: Vec<usize> = cols
        .iter()
        .map(|v| match v {
            Some(v) if *v > 0.0 => ((v / top * (h * 8) as f64).round() as usize).clamp(1, h * 8),
            _ => 0,
        })
        .collect();
    // phosphor look on tall graphs: a bright band along the top, the area under it dimmed. Small graphs stay
    // solid (a one-row band would be most of them).
    let (bright, body) = (ui::accent(t), if h >= 5 { Style::default().fg(dim(t)) } else { ui::accent(t) });
    (0..h)
        .map(|r| {
            let fb = h - 1 - r; // rows from the bottom
            let mut spans: Vec<Span<'static>> = vec![];
            let mut run = String::new();
            let mut run_style = Style::default();
            for &l in &levels {
                // a band one row thick along the top of each column is bright, the rest of the column dim
                let (c, st) = if l <= fb * 8 {
                    (' ', Style::default())
                } else {
                    let c = if (l - 1) / 8 == fb { SPARK[l - fb * 8] } else { '█' };
                    (c, if (fb + 1) * 8 > l.saturating_sub(8) { bright } else { body })
                };
                if st != run_style && !run.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut run), run_style));
                }
                run_style = st;
                run.push(c);
            }
            spans.push(Span::styled(run, run_style));
            Line::from(spans)
        })
        .collect()
}

/// The accent at ~40% brightness, for graph fills (the frame colour on themes without RGB colours).
pub fn dim(t: &Theme) -> ratatui::style::Color {
    match t.accent {
        ratatui::style::Color::Rgb(r, g, b) => {
            let f = |x: u8| (x as f32 * 0.42) as u8;
            ratatui::style::Color::Rgb(f(r), f(g), f(b))
        }
        _ => t.frame,
    }
}

// ------------------------------------------------------------------ tables

#[derive(Clone, Copy)]
pub struct Col {
    pub head: &'static str,
    pub min: usize,
    /// Extra width this column may take (0 = fixed). Slack goes to columns left to right.
    pub max: usize,
    pub right: bool,
}

pub const fn col(head: &'static str, min: usize, max: usize, right: bool) -> Col {
    Col { head, min, max, right }
}

pub const GAP: usize = 2;

/// Column widths for `total` columns of space: every column gets its minimum, slack is handed out in order.
pub fn widths(cols: &[Col], total: usize) -> Vec<usize> {
    let mut w: Vec<usize> = cols.iter().map(|c| c.min).collect();
    let used: usize = w.iter().sum::<usize>() + GAP * cols.len().saturating_sub(1);
    let mut spare = total.saturating_sub(used);
    for (i, c) in cols.iter().enumerate() {
        let add = c.max.saturating_sub(c.min).min(spare);
        w[i] += add;
        spare -= add;
    }
    // anything left goes to the last flexible column
    if spare > 0
        && let Some(i) = cols.iter().rposition(|c| c.max > c.min)
    {
        w[i] += spare;
    }
    w
}

#[derive(Clone, Default)]
pub struct Cell {
    pub pre: String,
    pub pre_style: Style,
    pub text: String,
    pub style: Style,
}

impl Cell {
    pub fn new(text: impl Into<String>, style: Style) -> Cell {
        Cell { text: text.into(), style, ..Default::default() }
    }
    pub fn plain(text: impl Into<String>) -> Cell {
        Cell::new(text, Style::default())
    }
    pub fn with_pre(mut self, pre: impl Into<String>, style: Style) -> Cell {
        self.pre = pre.into();
        self.pre_style = style;
        self
    }
}

/// One table row as a Line exactly `total` columns wide. `sel` (the selection style) is laid over every cell.
pub fn row_line(cells: Vec<Cell>, cols: &[Col], w: &[usize], sel: Option<Style>, total: usize) -> Line<'static> {
    let over = |s: Style| match sel {
        Some(o) => s.patch(o),
        None => s,
    };
    let mut spans = Vec::with_capacity(cells.len() * 3);
    let mut used = 0;
    for (i, c) in cells.into_iter().enumerate().take(cols.len()) {
        let cw = w[i];
        if cw == 0 {
            continue;
        }
        if used > 0 {
            spans.push(Span::styled(" ".repeat(GAP), over(Style::default())));
            used += GAP;
        }
        if used + cw > total {
            break;
        }
        let pw = c.pre.width().min(cw);
        let text = ui::fit(&c.text, cw - pw);
        let pad = (cw - pw).saturating_sub(text.width());
        if cols[i].right && pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), over(Style::default())));
        }
        if pw > 0 {
            spans.push(Span::styled(ui::fit(&c.pre, pw), over(c.pre_style)));
        }
        spans.push(Span::styled(text, over(c.style)));
        if !cols[i].right && pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), over(Style::default())));
        }
        used += cw;
    }
    if used < total {
        spans.push(Span::styled(" ".repeat(total - used), over(Style::default())));
    }
    Line::from(spans)
}

pub fn head_line(cols: &[Col], w: &[usize], sorted: Option<usize>, total: usize, t: &Theme) -> Line<'static> {
    let cells = cols
        .iter()
        .enumerate()
        .map(|(i, c)| {
            if sorted == Some(i) {
                Cell::new(format!("{}▾", c.head), ui::bold_accent(t))
            } else {
                Cell::new(c.head, Style::default().add_modifier(Modifier::BOLD))
            }
        })
        .collect();
    row_line(cells, cols, w, None, total)
}

pub fn sel_style(t: &Theme, focused: bool) -> Style {
    let s = Style::default().bg(t.frame).add_modifier(Modifier::BOLD);
    if focused { s.fg(t.accent) } else { s }
}

/// Keep `sel` inside the `h` visible rows starting at `scroll`.
pub fn clamp_scroll(sel: usize, scroll: &mut usize, h: usize, n: usize) {
    if sel < *scroll {
        *scroll = sel;
    } else if h > 0 && sel >= *scroll + h {
        *scroll = sel + 1 - h;
    }
    *scroll = (*scroll).min(n.saturating_sub(h));
}

pub fn scrollbar(f: &mut Frame, x: u16, body: Rect, scroll: usize, total: usize, t: &Theme) {
    let h = body.height as usize;
    if total <= h || h == 0 {
        return;
    }
    let th = ((h * h) / total).max(1);
    let ty = (scroll * (h - th)) / (total - h).max(1);
    let bar: Vec<Line> = (0..h).map(|y| if y >= ty && y < ty + th { Line::styled("█", ui::accent(t)) } else { Line::styled("│", ui::fg(t.frame)) }).collect();
    f.render_widget(Paragraph::new(bar), Rect { x, y: body.y, width: 1, height: body.height });
}

/// "12 things · 3 bad" style summary: numbers bold, words muted.
pub fn summary_line(parts: &[(String, &str)], t: &Theme) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, (n, what)) in parts.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", ui::muted(t)));
        }
        spans.push(Span::styled(n.clone(), Style::default().add_modifier(Modifier::BOLD)));
        if !what.is_empty() {
            spans.push(Span::styled(format!(" {what}"), ui::muted(t)));
        }
    }
    Line::from(spans)
}

// ------------------------------------------------------------------ list views: startup, services, connections

pub const STARTUP_COLS: [Col; 4] = [col("name", 16, 34, false), col("status", 6, 0, false), col("where", 18, 26, false), col("command", 20, 400, false)];
pub const SERVICE_COLS: [Col; 5] = [col("name", 14, 30, false), col("state", 9, 0, false), col("start", 14, 0, false), col("pid", 6, 0, true), col("description", 20, 400, false)];
pub const CONN_COLS: [Col; 6] = [col("proto", 5, 0, false), col("local", 18, 30, false), col("remote", 18, 30, false), col("state", 12, 0, false), col("pid", 6, 0, true), col("process", 12, 400, false)];

pub fn startup_cells(s: &StartupItem, t: &Theme) -> Vec<Cell> {
    let (status, st) = match s.enabled {
        Some(true) => ("on", ui::fg(t.good)),
        Some(false) => ("off", ui::muted(t)),
        None => ("", ui::muted(t)),
    };
    let name_style = if s.enabled == Some(false) { ui::muted(t) } else { Style::default() };
    vec![Cell::new(&s.name, name_style), Cell::new(status, st), Cell::new(&s.location, ui::muted(t)), Cell::new(&s.command, ui::muted(t))]
}

pub fn state_style(state: &str, t: &Theme) -> Style {
    match state {
        "running" | "established" => ui::fg(t.good),
        "failed" => Style::default().fg(t.danger).add_modifier(Modifier::BOLD),
        "starting" | "stopping" | "pausing" | "resuming" | "activating" => ui::fg(WARN),
        "listening" => ui::accent(t),
        _ => ui::muted(t),
    }
}

pub fn service_cells(s: &Service, t: &Theme) -> Vec<Cell> {
    let start_style = if s.start == "disabled" || s.start == "masked" { ui::fg(t.danger) } else { ui::muted(t) };
    vec![
        Cell::plain(&s.name),
        Cell::new(&s.state, state_style(&s.state, t)),
        Cell::new(&s.start, start_style),
        Cell::new(if s.pid > 0 { s.pid.to_string() } else { String::new() }, ui::muted(t)),
        Cell::plain(&s.display),
    ]
}

pub fn conn_cells(c: &Conn, t: &Theme) -> Vec<Cell> {
    vec![
        Cell::new(&c.proto, ui::muted(t)),
        Cell::plain(&c.local),
        Cell::new(&c.remote, if c.state == "established" { Style::default() } else { ui::muted(t) }),
        Cell::new(&c.state, state_style(&c.state, t)),
        Cell::new(if c.pid > 0 { c.pid.to_string() } else { String::new() }, ui::muted(t)),
        Cell::new(&c.process, Style::default()),
    ]
}

// ------------------------------------------------------------------ performance + system info

fn pct_style(v: f32, t: &Theme) -> Style {
    if v >= 90.0 {
        Style::default().fg(t.danger).add_modifier(Modifier::BOLD)
    } else if v >= 50.0 {
        ui::bold_accent(t)
    } else {
        Style::default()
    }
}

pub fn uptime(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, (secs / 3600) % 24, (secs / 60) % 60);
    if d > 0 { format!("{d}d {h}h {m}m") } else if h > 0 { format!("{h}h {m}m") } else { format!("{m}m") }
}

impl System {
    pub(super) fn draw_performance(&mut self, f: &mut Frame, body: Rect, t: &Theme) {
        let s = self.snap.clone();
        let has_gpu = matches!(self.gpu, GpuState::Ok(_));
        let top_h: u16 = if body.height >= 34 { 10 } else { 8 };
        let mid_h: u16 = if body.height >= 34 { 8 } else { 6 };
        let (mut y, bottom) = (body.y, body.bottom());

        // cpu + gpu (or memory) side by side: big history graphs
        let r = Rect { y, height: top_h.min(bottom - y), ..body };
        let (a, b) = split_h(r);
        let gh = top_h.saturating_sub(3) as usize;
        let cur = s.cpu_hist.back().copied().unwrap_or(0.0);
        let aw = a.width.saturating_sub(4) as usize;
        let mut info = format!("{} cpus", s.cores.len());
        if s.ghz > 0.0 {
            info.push_str(&format!(" · {:.2} GHz", s.ghz));
        }
        info.push_str(&format!(" · {} processes · {} threads", s.procs.len(), s.threads));
        let mut lines = vec![Line::from(vec![Span::styled(format!("{cur:5.1}% "), ui::bold_accent(t)), Span::styled(ui::fit(&info, aw.saturating_sub(7)), ui::muted(t))])];
        lines.extend(graph(&s.cpu_hist, HISTORY, aw, gh, 100.0, t));
        card(f, a, "system", "cpu · 2 min", lines, t);

        let bw = b.width.saturating_sub(4) as usize;
        if has_gpu {
            let GpuState::Ok(g) = &self.gpu else { unreachable!() };
            let name = g.name.replace("NVIDIA GeForce ", "").replace("NVIDIA ", "");
            let hot = if g.temp >= 83.0 { t.danger } else if g.temp >= 75.0 { WARN } else { t.accent };
            let mut lines = vec![Line::from(vec![
                Span::styled(format!("{:5.0}% ", g.util), ui::bold_accent(t)),
                Span::styled(format!("{name} · "), ui::muted(t)),
                Span::styled(format!("{:.0}°C", g.temp), Style::default().fg(hot).add_modifier(Modifier::BOLD)),
                Span::styled(format!(" · {:.0}/{:.0} W · vram {:.1}/{:.0} GB", g.power, g.limit, g.used / 1024.0, g.total / 1024.0), ui::muted(t)),
            ])];
            let hist: std::collections::VecDeque<f32> = self.gpu_hist.iter().copied().collect();
            lines.extend(graph(&hist, HISTORY / 2, bw, gh, 100.0, t)); // nvidia-smi is asked every 2 s
            card(f, b, "gauge", "gpu · 2 min", lines, t);
        } else {
            self.mem_card(f, b, gh, t);
        }
        y += r.height;

        // memory + io
        if bottom >= y + mid_h {
            let r = Rect { y, height: mid_h, ..body };
            let (a, b) = split_h(r);
            if has_gpu {
                self.mem_card(f, a, mid_h.saturating_sub(3) as usize, t);
            } else {
                card(f, a, "cloud", "network · disk io", self.io_lines(a.width.saturating_sub(4) as usize, t), t);
            }
            let io = self.io_lines(b.width.saturating_sub(4) as usize, t);
            if has_gpu {
                card(f, b, "cloud", "network · disk io", io, t);
            } else {
                self.disk_card(f, b, t);
            }
            y += mid_h;
        }

        // one small graph per logical processor
        if bottom >= y + 4 {
            self.draw_cores(f, Rect { y, height: bottom - y, ..body }, t);
        }
    }

    fn mem_card(&self, f: &mut Frame, r: Rect, gh: usize, t: &Theme) {
        let s = &self.snap;
        let w = r.width.saturating_sub(4) as usize;
        let pct = 100.0 * s.ram_used as f64 / s.ram_total.max(1) as f64;
        let mut l = vec![Span::styled(format!("{pct:5.1}% "), ui::bold_accent(t)), Span::styled(format!("{:.1} of {:.1} GB in use  ", gib(s.ram_used), gib(s.ram_total)), ui::muted(t))];
        let mw = w.saturating_sub(40).clamp(0, 40);
        if mw >= 6 {
            l.extend(meter(pct / 100.0, mw, t.accent, t.frame));
        }
        let mut lines = vec![Line::from(l)];
        lines.extend(graph(&s.ram_hist, HISTORY, w, gh, 100.0, t));
        card(f, r, "chart", "memory · 2 min", lines, t);
    }

    fn disk_card(&self, f: &mut Frame, r: Rect, t: &Theme) {
        let lines = self.mem_lines(r.width.saturating_sub(4) as usize, t).into_iter().skip(2).collect();
        card(f, r, "storage", "disks", lines, t);
    }

    fn draw_cores(&self, f: &mut Frame, r: Rect, t: &Theme) {
        let s = &self.snap;
        let n = s.cores.len();
        let title = format!("logical processors · {n} · 60 s");
        let inner = ui::frame(f, r, &format!("{}{title}", ui::lead("system")), None, false, t);
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
        if n == 0 || inner.height == 0 {
            f.render_widget(Paragraph::new(Line::styled("sampling…", ui::muted(t))), inner);
            return;
        }
        let (iw, ih) = (inner.width as usize, inner.height as usize);
        // biggest cells that still fit: prefer 2 graph rows + a gap row, then fewer
        let mut pick = None;
        'outer: for (graph_rows, gap) in [(3usize, 1usize), (2, 1), (2, 0), (1, 0)] {
            let cell_h = 1 + graph_rows + gap;
            for cols in 1..=n {
                let rows = n.div_ceil(cols);
                let cw = (iw + 2) / cols;
                if cw < 14 {
                    break;
                }
                if rows * cell_h - gap <= ih && cw <= 64 {
                    pick = Some((cols, graph_rows, cell_h));
                    break 'outer;
                }
            }
        }
        let Some((cols, graph_rows, cell_h)) = pick.or(Some(((iw + 2) / 14, 1, 2))) else { return };
        let cols = cols.max(1);
        let cw = (iw + 2) / cols;
        let mhz_varies = s.core_mhz.iter().filter(|&&m| m > 0).collect::<std::collections::HashSet<_>>().len() > 1;
        for i in 0..n {
            let (cx, cy) = (i % cols, i / cols);
            let y = inner.y as usize + cy * cell_h;
            if y + 1 + graph_rows > inner.bottom() as usize {
                break;
            }
            let x = inner.x + (cx * cw) as u16;
            let w = cw.saturating_sub(2).max(4);
            let v = s.cores[i];
            let mut label = vec![Span::styled(format!("cpu {i}"), ui::muted(t))];
            let mut right = format!("{v:.0}%");
            if mhz_varies && let Some(&m) = s.core_mhz.get(i).filter(|&&m| m > 0) {
                right = format!("{:.1} GHz  {right}", m as f64 / 1000.0);
            }
            let lw = format!("cpu {i}").width();
            let pad = w.saturating_sub(lw + right.width());
            label.push(Span::raw(" ".repeat(pad)));
            label.push(Span::styled(right, pct_style(v, t)));
            f.render_widget(Paragraph::new(Line::from(label)), Rect { x, y: y as u16, width: w as u16, height: 1 });
            let empty = std::collections::VecDeque::new();
            let hist = s.core_hist.get(i).unwrap_or(&empty);
            let g = graph(hist, 60, w, graph_rows, 100.0, t);
            f.render_widget(Paragraph::new(g), Rect { x, y: y as u16 + 1, width: w as u16, height: graph_rows as u16 });
        }
    }

    pub(super) fn draw_info(&mut self, f: &mut Frame, body: Rect, t: &Theme) {
        let Some(info) = self.info.clone() else {
            f.render_widget(Paragraph::new(Line::styled(" collecting system info…", ui::muted(t))), body);
            return;
        };
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let mut sections = info.sections.clone();
        // the live bits come from the sampler, not the one-off collection
        if let Some(os) = sections.iter_mut().find(|s| s.title == "os")
            && info.boot > 0
        {
            os.rows.push(("uptime".into(), uptime(now.saturating_sub(info.boot))));
        }
        if let Some(m) = sections.iter_mut().find(|s| s.title == "memory")
            && self.snap.ram_total > 0
        {
            m.rows.push(("in use".into(), format!("{:.1} GB ({:.0}%)", gib(self.snap.ram_used), 100.0 * self.snap.ram_used as f64 / self.snap.ram_total as f64)));
        }
        // two columns, each section into the shorter one
        let (left, right) = split_h(body);
        let (mut ly, mut ry) = (body.y, body.y);
        for sec in &sections {
            let h = sec.rows.len() as u16 + 2;
            let (col, y) = if ly <= ry { (left, &mut ly) } else { (right, &mut ry) };
            if *y + h > body.bottom() {
                continue;
            }
            let r = Rect { y: *y, height: h, ..col };
            let w = r.width.saturating_sub(4) as usize;
            let kw = sec.rows.iter().map(|(k, _)| k.width()).max().unwrap_or(0).min(w / 2).max(6);
            let lines: Vec<Line> = sec
                .rows
                .iter()
                .map(|(k, v)| {
                    Line::from(vec![
                        Span::styled(format!("{:<kw$}  ", ui::fit(k, kw)), ui::muted(t)),
                        Span::raw(ui::fit(v, w.saturating_sub(kw + 2))),
                    ])
                })
                .collect();
            card(f, r, sec.icon, &sec.title, lines, t);
            *y += h;
        }
    }
}
