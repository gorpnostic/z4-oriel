//! system: cpu / gpu / memory · disk / network · disk io cards with history sparklines, Wren's trainer when it
//! runs, and a sortable, filterable process table with kill (nest's system app, ported).
//!
//! All sampling happens on background threads (see sampler.rs); the UI thread only swaps in the latest
//! snapshot and draws it.

mod sampler;

use crate::pane::{Cx, Pane};
use crate::theme::Theme;
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use sampler::{GpuState, Proc, Shared, Snap};
use std::sync::Arc;
use std::time::Duration;

const SPARK: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const WARN: Color = Color::Rgb(0xe0, 0xa0, 0x40);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sort {
    Cpu,
    Mem,
    Name,
}

impl Sort {
    fn label(self) -> &'static str {
        match self {
            Sort::Cpu => "cpu",
            Sort::Mem => "memory",
            Sort::Name => "name",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum View {
    Overview,
    Sort(Sort),
    Training,
}

/// Sidebar rows: icon, label, view.
const SIDE: [(&str, &str, View); 5] = [
    ("chart", "overview", View::Overview),
    ("system", "sort by cpu", View::Sort(Sort::Cpu)),
    ("gauge", "sort by memory", View::Sort(Sort::Mem)),
    ("search", "sort by name", View::Sort(Sort::Name)),
    ("wren", "training", View::Training),
];

pub struct System {
    shared: Arc<Shared>,
    started: bool,
    snap: Arc<Snap>,
    seq: u64,
    gpu: GpuState,
    gpu_hist: Vec<f32>,
    sort: Sort,
    view: View,
    filter: String,
    filtering: bool,
    /// Indices into `snap.procs`, filtered + sorted.
    rows: Vec<usize>,
    sel: usize,
    sel_pid: Option<u32>,
    /// The user moved the cursor: follow that process through re-sorts. Until then the cursor stays on row 0.
    pinned: bool,
    scroll: usize,
    pending_kill: Option<(u32, String)>,
    killer: fn(u32) -> Result<(), String>,
    // hit areas from the last render
    table_body: Rect,
    filter_box: Rect,
    side_hits: Vec<(Rect, View)>,
}

impl System {
    pub fn new() -> Self {
        Self {
            shared: Shared::new(),
            started: false,
            snap: Arc::new(Snap::default()),
            seq: 0,
            gpu: GpuState::Pending,
            gpu_hist: vec![],
            sort: Sort::Cpu,
            view: View::Overview,
            filter: String::new(),
            filtering: false,
            rows: vec![],
            sel: 0,
            sel_pid: None,
            pinned: false,
            scroll: 0,
            pending_kill: None,
            killer: sampler::kill,
            table_body: Rect::default(),
            filter_box: Rect::default(),
            side_hits: vec![],
        }
    }

    fn ensure_started(&mut self, cx: &Cx) {
        self.shared.touch();
        if !self.started {
            self.started = true;
            sampler::start(self.shared.clone(), cx.waker());
        }
    }

    /// Take the newest snapshot (a pointer swap) and rebuild the filtered/sorted row list.
    fn pull(&mut self, cx: &mut Cx) {
        let (snap, msgs) = {
            let mut g = self.shared.lock();
            if !matches!(g.gpu, GpuState::Pending) || !matches!(self.gpu, GpuState::Pending) {
                self.gpu = g.gpu.clone();
                self.gpu_hist.clear();
                self.gpu_hist.extend(g.gpu_hist.iter().copied());
            }
            (g.snap.clone(), std::mem::take(&mut g.msgs))
        };
        for m in msgs {
            cx.notify(m);
        }
        if let Some(s) = snap
            && s.seq != self.seq
        {
            self.seq = s.seq;
            self.snap = s;
            self.rebuild();
        }
    }

    fn rebuild(&mut self) {
        let q = self.filter.trim().to_lowercase();
        let procs = &self.snap.procs;
        self.rows.clear();
        self.rows.extend((0..procs.len()).filter(|&i| q.is_empty() || procs[i].name.to_lowercase().contains(&q) || procs[i].pid.to_string() == q));
        match self.sort {
            Sort::Cpu => self.rows.sort_by(|&a, &b| procs[b].cpu.total_cmp(&procs[a].cpu).then(procs[a].pid.cmp(&procs[b].pid))),
            Sort::Mem => self.rows.sort_by(|&a, &b| procs[b].mem.cmp(&procs[a].mem).then(procs[a].pid.cmp(&procs[b].pid))),
            Sort::Name => self.rows.sort_by_cached_key(|&i| (procs[i].name.to_lowercase(), procs[i].pid)),
        }
        // keep the cursor on the same process
        if self.pinned
            && let Some(pid) = self.sel_pid
            && let Some(i) = self.rows.iter().position(|&r| procs[r].pid == pid)
        {
            self.sel = i;
        }
        self.sel = self.sel.min(self.rows.len().saturating_sub(1));
        self.sel_pid = self.selected().map(|p| p.pid);
    }

    fn selected(&self) -> Option<&Proc> {
        self.rows.get(self.sel).map(|&i| &self.snap.procs[i])
    }

    fn move_sel(&mut self, d: isize) {
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        self.sel = (self.sel as isize + d).clamp(0, n as isize - 1) as usize;
        self.pinned = self.sel > 0;
        self.sel_pid = self.selected().map(|p| p.pid);
    }

    fn set_sort(&mut self, s: Sort) {
        self.sort = s;
        if matches!(self.view, View::Sort(_)) {
            self.view = View::Sort(s);
        }
        self.rebuild();
    }

    fn set_view(&mut self, v: View) {
        self.view = v;
        if let View::Sort(s) = v {
            self.sort = s;
            self.rebuild();
        }
    }

    fn ask_kill(&mut self) {
        if let Some(p) = self.selected() {
            self.pending_kill = Some((p.pid, p.name.clone()));
        }
    }

    fn confirm_kill(&mut self, yes: bool, cx: &mut Cx) {
        let Some((pid, name)) = self.pending_kill.take() else { return };
        if !yes {
            cx.notify("left it alone");
            return;
        }
        let (shared, waker, killer) = (self.shared.clone(), cx.waker(), self.killer);
        std::thread::spawn(move || {
            let msg = match killer(pid) {
                Ok(()) => format!("sent terminate to {name} (pid {pid})"),
                Err(e) => format!("couldn't kill {name} (pid {pid}): {e}"),
            };
            shared.lock().msgs.push(msg);
            waker.wake();
        });
    }
}

impl Drop for System {
    fn drop(&mut self) {
        self.shared.stop();
    }
}

// ------------------------------------------------------------------ drawing helpers

fn spark<T: Copy + Into<f64>>(vals: impl DoubleEndedIterator<Item = T> + ExactSizeIterator, width: usize, top: Option<f64>) -> String {
    let v: Vec<f64> = vals.rev().take(width).map(Into::into).collect::<Vec<_>>().into_iter().rev().collect();
    let top = top.unwrap_or_else(|| v.iter().cloned().fold(1e-9, f64::max));
    let mut s = " ".repeat(width.saturating_sub(v.len()));
    for x in v {
        s.push(if x > 0.0 { SPARK[((8.0 * x / top) as usize).clamp(1, 8)] } else { ' ' });
    }
    s
}

fn meter(frac: f64, width: usize, fill: Color, empty: Color) -> Vec<Span<'static>> {
    let n = ((frac.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    vec![Span::styled("█".repeat(n), Style::default().fg(fill)), Span::styled("░".repeat(width - n), Style::default().fg(empty))]
}

fn gib(n: u64) -> f64 {
    n as f64 / (1u64 << 30) as f64
}

fn rate(v: f64) -> String {
    format!("{}/s", ui::human_bytes(v as u64))
}

fn card(f: &mut Frame, r: Rect, icon: &str, title: &str, lines: Vec<Line>, t: &Theme) {
    if r.width < 4 || r.height < 2 {
        return;
    }
    let inner = ui::frame(f, r, &format!("{}{title}", ui::lead(icon)), None, false, t);
    let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
    f.render_widget(Paragraph::new(lines), inner);
}

fn split_h(r: Rect) -> (Rect, Rect) {
    let lw = r.width / 2;
    (Rect { width: lw, ..r }, Rect { x: r.x + lw + 1, width: r.width.saturating_sub(lw + 1), ..r })
}

impl System {
    fn cpu_lines(&self, w: usize, t: &Theme) -> Vec<Line<'static>> {
        let s = &self.snap;
        let cur = s.cpu_hist.back().copied().unwrap_or(0.0);
        let mut info = format!("{} threads", s.cores.len());
        if s.ghz > 0.0 {
            info.push_str(&format!(" · {:.1} GHz", s.ghz));
        }
        let mut lines = vec![
            Line::from(vec![Span::styled(format!("{cur:5.1}% "), ui::bold_accent(t)), Span::styled(info, ui::muted(t))]),
            Line::styled(spark(s.cpu_hist.iter().copied(), w, Some(100.0)), ui::accent(t)),
        ];
        // per-core mini bars, two rows like nest (more if the card is narrow)
        let per_row = (w / 2).max(1);
        let rows = s.cores.len().div_ceil(2).min(per_row).max(1);
        for chunk in s.cores.chunks(rows).take(2) {
            let spans: Vec<Span> = chunk
                .iter()
                .map(|&v| {
                    let c = if v > 1.0 { SPARK[((8.0 * v / 100.0) as usize).clamp(1, 8)] } else { '·' };
                    Span::styled(format!("{c} "), if v > 50.0 { ui::accent(t) } else { ui::muted(t) })
                })
                .collect();
            lines.push(Line::from(spans));
        }
        lines.push(Line::styled("per core", ui::muted(t)));
        lines
    }

    fn gpu_lines(&self, w: usize, t: &Theme) -> Vec<Line<'static>> {
        let g = match &self.gpu {
            GpuState::Ok(g) => g,
            GpuState::Pending => return vec![Line::styled("asking nvidia-smi…", ui::muted(t))],
            _ => return vec![Line::styled("no NVIDIA GPU / nvidia-smi", ui::muted(t))],
        };
        let name = g.name.replace("NVIDIA GeForce ", "").replace("NVIDIA ", "");
        let vram = format!(" {:.1}/{:.0} GB", g.used / 1024.0, g.total / 1024.0);
        let mw = w.saturating_sub(6 + vram.len()).max(4);
        let hot = if g.temp >= 83.0 { t.danger } else if g.temp >= 75.0 { WARN } else { t.accent };
        let mut vl = vec![Span::styled("vram  ", ui::muted(t))];
        vl.extend(meter(g.used / g.total.max(1.0), mw, t.accent, t.frame));
        vl.push(Span::styled(vram, ui::muted(t)));
        vec![
            Line::from(vec![Span::styled(format!("{:5.0}% ", g.util), ui::bold_accent(t)), Span::styled(name, ui::muted(t))]),
            Line::styled(spark(self.gpu_hist.iter().copied(), w, Some(100.0)), ui::accent(t)),
            Line::from(vl),
            Line::from(vec![
                Span::styled("temp  ", ui::muted(t)),
                Span::styled(format!("{:.0}°C", g.temp), Style::default().fg(hot).add_modifier(Modifier::BOLD)),
                Span::styled(format!("   power {:.0}/{:.0} W   fan {:.0}%", g.power, g.limit, g.fan), ui::muted(t)),
            ]),
        ]
    }

    fn mem_lines(&self, w: usize, t: &Theme) -> Vec<Line<'static>> {
        let s = &self.snap;
        let lw = s.disks.iter().map(|d| d.label.chars().count()).max().unwrap_or(0).clamp(5, 12) + 1;
        let ram = format!(" {:.1}/{:.0} GB", gib(s.ram_used), gib(s.ram_total));
        let mw = w.saturating_sub(lw + 16).max(4);
        let mut rl = vec![Span::styled(format!("{:<lw$}", "ram"), ui::muted(t))];
        rl.extend(meter(s.ram_used as f64 / s.ram_total.max(1) as f64, mw, t.accent, t.frame));
        rl.push(Span::styled(ram, ui::muted(t)));
        let mut lines = vec![
            Line::from(rl),
            Line::from(vec![Span::raw(" ".repeat(lw)), Span::styled(spark(s.ram_hist.iter().copied(), mw, Some(100.0)), ui::accent(t))]),
        ];
        for d in &s.disks {
            let pct = 100.0 * (1.0 - d.free as f64 / d.total.max(1) as f64);
            let col = if pct >= 95.0 { t.danger } else if pct >= 85.0 { WARN } else { t.accent };
            let mut l = vec![Span::styled(format!("{:<lw$}", ui::fit(&d.label, lw - 1)), ui::muted(t))];
            l.extend(meter(pct / 100.0, mw, col, t.frame));
            l.push(Span::styled(
                format!(" {:.1} GB free", gib(d.free)),
                if pct >= 95.0 { Style::default().fg(col).add_modifier(Modifier::BOLD) } else { ui::muted(t) },
            ));
            lines.push(Line::from(l));
        }
        lines
    }

    fn io_lines(&self, w: usize, t: &Theme) -> Vec<Line<'static>> {
        let s = &self.snap;
        let sw = w.saturating_sub(6 + 11).max(4);
        [("down", &s.net_down), ("up", &s.net_up), ("read", &s.disk_r), ("write", &s.disk_w)]
            .into_iter()
            .map(|(label, q)| {
                let cur = q.back().copied().unwrap_or(0.0);
                Line::from(vec![
                    Span::styled(format!("{label:<6}"), ui::muted(t)),
                    Span::styled(spark(q.iter().copied(), sw, None), ui::accent(t)),
                    Span::styled(format!(" {:>10}", rate(cur)), if cur > 1e6 { Style::default().add_modifier(Modifier::BOLD) } else { ui::muted(t) }),
                ])
            })
            .collect()
    }

    fn train_lines(&self, w: usize, big: bool, t: &Theme) -> Vec<Line<'static>> {
        let Some(st) = &self.snap.train else {
            return vec![Line::styled(
                format!("not training right now · `python train{}train.py pretrain` in {}", std::path::MAIN_SEPARATOR, sampler::wren_root().display()),
                ui::muted(t),
            )];
        };
        let mut head = vec![Span::styled(
            format!("{} {}", if st.running { "● running" } else { "○ stopped" }, st.mode),
            if st.running { ui::bold_accent(t) } else { ui::muted(t) },
        )];
        if let Some(pid) = st.pid {
            head.push(Span::styled(format!("  pid {pid}"), ui::muted(t)));
        }
        let mut lines = vec![];
        let bar_w = w.saturating_sub(8).min(60).max(4);
        if let Some(pr) = &st.progress {
            head.push(Span::styled(format!("   step {}/{}", pr.step, pr.total), Style::default().add_modifier(Modifier::BOLD)));
            head.push(Span::styled(format!("   loss {:.3}   {:.1}k tok/s   eta {:.1} h", pr.loss, pr.tok_s / 1000.0, pr.eta / 3600.0), ui::muted(t)));
            lines.push(Line::from(head));
            let frac = pr.step as f64 / pr.total.max(1) as f64;
            let mut l = meter(frac, bar_w, t.accent, t.frame);
            l.push(Span::raw(format!(" {:.1}%", 100.0 * frac)));
            lines.push(Line::from(l));
        } else if let Some(last) = st.evals.last() {
            head.push(Span::styled(format!("   last eval at step {}", last.step), Style::default().add_modifier(Modifier::BOLD)));
            let n = st.evals.len();
            let total = if st.mode == "pretrain" { 8392 } else { 0 };
            if n >= 2 && st.running && total > 0 {
                let (a, b) = (&st.evals[n - 2], &st.evals[n - 1]);
                let r = (b.step as f64 - a.step as f64) / (b.time - a.time).max(1.0);
                if r > 0.0 {
                    head.push(Span::styled(format!("   ~{:.1} h left", (total as f64 - b.step as f64) / r / 3600.0), ui::muted(t)));
                }
            }
            lines.push(Line::from(head));
            if total > 0 {
                let frac = last.step as f64 / total as f64;
                let mut l = meter(frac, bar_w, t.accent, t.frame);
                l.push(Span::raw(format!(" {:.1}%", 100.0 * frac)));
                lines.push(Line::from(l));
            }
        } else {
            lines.push(Line::from(head));
        }
        if !st.evals.is_empty() {
            let vals: Vec<f64> = st.evals.iter().map(|e| e.val).collect();
            let lo = vals.iter().cloned().fold(f64::MAX, f64::min);
            let hi = vals[if vals.len() > 1 { 1 } else { 0 }..].iter().cloned().fold(f64::MIN, f64::max);
            let rng = (hi - lo).max(1e-6);
            let n = w.saturating_sub(24).clamp(4, 60);
            let bars: String = vals[vals.len().saturating_sub(n)..].iter().map(|v| SPARK[((8.0 * (hi - v) / rng) as usize + 1).clamp(1, 8)]).collect();
            lines.push(Line::from(vec![
                Span::styled("val loss ", ui::muted(t)),
                Span::styled(bars, ui::accent(t)),
                Span::styled(format!("  {:.2} → {:.3}", vals[0], vals[vals.len() - 1]), ui::muted(t)),
            ]));
        }
        if big && !st.samples.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::styled("latest samples", ui::bold_accent(t)));
            for (p, out) in st.samples.iter().take(4) {
                let out: String = out.replace('\n', " ").chars().take(150).collect();
                lines.push(Line::from(vec![
                    Span::styled(format!("  {p}"), Style::default().add_modifier(Modifier::BOLD)),
                    Span::styled(ui::fit(&out, w.saturating_sub(p.chars().count() + 2)), ui::muted(t)),
                ]));
            }
        }
        lines
    }

    fn draw_filter(&mut self, f: &mut Frame, r: Rect, t: &Theme, focused: bool) {
        self.filter_box = r;
        if let Some((pid, name)) = &self.pending_kill {
            let inner = ui::frame(f, r, "kill", None, true, &Theme { accent: t.danger, ..t.clone() });
            let l = Line::from(vec![
                Span::styled(format!(" kill {name} (pid {pid})?  "), Style::default().fg(t.danger).add_modifier(Modifier::BOLD)),
                Span::styled("y", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(" = yes · ", ui::muted(t)),
                Span::styled("esc", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(" = no", ui::muted(t)),
            ]);
            f.render_widget(Paragraph::new(l), inner);
            return;
        }
        let border = if self.filtering && focused { t.accent } else { t.frame };
        let block = ratatui::widgets::Block::bordered().border_type(ratatui::widgets::BorderType::Rounded).border_style(Style::default().fg(border));
        let inner = block.inner(r);
        f.render_widget(block, r);
        let l = if self.filter.is_empty() && !self.filtering {
            Line::styled("  filter processes…   (/)", ui::muted(t))
        } else {
            let mut v = vec![Span::raw(format!("  {}", self.filter))];
            if self.filtering {
                v.push(Span::styled(" ", Style::default().add_modifier(Modifier::REVERSED)));
            }
            Line::from(v)
        };
        f.render_widget(Paragraph::new(l), inner);
    }

    fn draw_table(&mut self, f: &mut Frame, r: Rect, t: &Theme, focused: bool) {
        if r.height < 2 {
            return;
        }
        let r = Rect { x: r.x + 1, width: r.width.saturating_sub(2), ..r };
        // column widths: pid, name, cpu, memory, threads, user (name gets the slack, capped)
        let fixed = 7 + 8 + 10 + 9 + 3;
        let spare = (r.width as usize).saturating_sub(fixed + 14);
        let name_w = spare.clamp(12, 40);
        let user_w = (r.width as usize).saturating_sub(fixed + name_w).clamp(0, 24);
        let row = |pid: &str, name: &str, cpu: &str, mem: &str, thr: &str, user: &str| {
            format!(
                "{pid:<7}{name:<name_w$} {cpu:>7} {mem:>9}   {thr:<8}{user}",
                name = ui::fit(name, name_w),
                user = ui::fit(user, user_w)
            )
        };
        let head = row("pid", "name", "cpu %", "memory", "threads", "user");
        f.render_widget(Paragraph::new(Line::styled(head, Style::default().add_modifier(Modifier::BOLD))), Rect { height: 1, ..r });
        let body = Rect { y: r.y + 1, height: r.height - 1, width: r.width.saturating_sub(1), ..r };
        self.table_body = body;
        let h = body.height as usize;
        if self.sel < self.scroll {
            self.scroll = self.sel;
        } else if self.sel >= self.scroll + h {
            self.scroll = self.sel + 1 - h;
        }
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(h));
        let procs = &self.snap.procs;
        let mut lines = Vec::with_capacity(h);
        for (n, &i) in self.rows.iter().enumerate().skip(self.scroll).take(h) {
            let p = &procs[i];
            let text = row(&p.pid.to_string(), &p.name, &format!("{:5.1}", p.cpu), &ui::human_bytes(p.mem), &p.threads.to_string(), &p.user);
            let style = if n == self.sel {
                let s = Style::default().bg(t.frame).add_modifier(Modifier::BOLD);
                if focused { s.fg(t.accent) } else { s }
            } else {
                Style::default()
            };
            lines.push(Line::styled(format!("{text:<w$}", w = body.width as usize), style));
        }
        if self.rows.is_empty() {
            let msg = if self.snap.seq == 0 { "sampling…" } else { "no process matches" };
            lines.push(Line::styled(msg, ui::muted(t)));
        }
        f.render_widget(Paragraph::new(lines), body);
        // scrollbar
        let total = self.rows.len();
        if total > h && h > 0 {
            let x = r.right() - 1;
            let th = ((h * h) / total).max(1);
            let ty = (self.scroll * (h - th)) / (total - h).max(1);
            let bar: Vec<Line> = (0..h).map(|y| if y >= ty && y < ty + th { Line::styled("█", ui::accent(t)) } else { Line::styled("│", ui::fg(t.frame)) }).collect();
            f.render_widget(Paragraph::new(bar), Rect { x, y: body.y, width: 1, height: body.height });
        }
    }
}

impl Pane for System {
    fn title(&self) -> String {
        "system".into()
    }
    fn icon(&self) -> &'static str {
        "system"
    }
    fn subtitle(&self) -> Option<String> {
        Some(format!("{} processes · sorted by {}", self.rows.len(), self.sort.label()))
    }
    fn badge(&self) -> Option<String> {
        self.snap.cpu_hist.back().map(|c| format!("cpu {c:.0}%"))
    }
    fn tick_every(&self) -> Option<Duration> {
        Some(Duration::from_secs(1))
    }

    fn poll(&mut self, cx: &mut Cx) {
        self.ensure_started(cx);
        self.pull(cx);
    }

    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        self.ensure_started(cx);
        let t = cx.theme;
        let hints: &[(&str, &str)] = if self.pending_kill.is_some() {
            &[("y", "kill it"), ("esc", "leave it")]
        } else if self.filtering {
            &[("enter", "keep filter"), ("esc", "clear"), ("↑↓", "move")]
        } else {
            &[("c/m/n", "sort cpu·memory·name"), ("k", "kill"), ("/", "filter")]
        };
        let body = ui::hint_line(f, area, hints, t);
        let train_shown = self.snap.train.is_some();

        if self.view == View::Training {
            let lines = self.train_lines(body.width.saturating_sub(4) as usize, true, t);
            card(f, body, "wren", "wren training", lines, t);
            self.table_body = Rect::default();
            return;
        }

        let has_gpu = !matches!(self.gpu, GpuState::Missing);
        let (w, mut y) = (body.width, body.y);
        let (lw, rw) = { let (a, b) = split_h(body); (a.width.saturating_sub(4) as usize, b.width.saturating_sub(4) as usize) };
        let top_h: u16 = 7;
        let mid_h: u16 = 2 + (2 + self.snap.disks.len() as u16).max(4);
        let train_lines = if train_shown { self.train_lines(w.saturating_sub(4) as usize, false, t) } else { vec![] };
        let train_h = if train_shown { train_lines.len() as u16 + 2 } else { 0 };
        let avail = body.height;
        let min_table = 6;
        let show_top = avail >= top_h + 3 + min_table;
        let show_mid = avail >= top_h + mid_h + 3 + min_table;
        let show_train = train_shown && avail >= top_h + mid_h + train_h + 3 + min_table;

        if show_top {
            let r = Rect { y, height: top_h, ..body };
            if has_gpu {
                let (a, b) = split_h(r);
                card(f, a, "system", "cpu", self.cpu_lines(lw, t), t);
                card(f, b, "gauge", "gpu", self.gpu_lines(rw, t), t);
            } else {
                card(f, r, "system", "cpu", self.cpu_lines(w.saturating_sub(4) as usize, t), t);
            }
            y += top_h;
        }
        if show_mid {
            let r = Rect { y, height: mid_h, ..body };
            let (a, b) = split_h(r);
            card(f, a, "chart", "memory · disk", self.mem_lines(lw, t), t);
            card(f, b, "cloud", "network · disk io", self.io_lines(rw, t), t);
            y += mid_h;
        }
        if show_train {
            card(f, Rect { y, height: train_h, ..body }, "wren", "wren training", train_lines, t);
            y += train_h;
        }
        if body.bottom() >= y + 3 {
            self.draw_filter(f, Rect { y, height: 3, ..body }, t, cx.focused);
            y += 3;
        }
        let table = Rect { y, height: body.bottom().saturating_sub(y), ..body };
        self.draw_table(f, table, t, cx.focused);
    }

    fn key(&mut self, key: KeyEvent, cx: &mut Cx) -> bool {
        if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            return false;
        }
        if self.pending_kill.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.confirm_kill(true, cx),
                _ => self.confirm_kill(false, cx),
            }
            return true;
        }
        if self.filtering {
            match key.code {
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.rebuild();
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.rebuild();
                }
                KeyCode::Enter => self.filtering = false,
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                    self.rebuild();
                }
                KeyCode::Up => self.move_sel(-1),
                KeyCode::Down => self.move_sel(1),
                _ => return false,
            }
            return true;
        }
        let page = self.table_body.height.max(1) as isize;
        match key.code {
            KeyCode::Char('c') => self.set_sort(Sort::Cpu),
            KeyCode::Char('m') => self.set_sort(Sort::Mem),
            KeyCode::Char('n') => self.set_sort(Sort::Name),
            KeyCode::Char('/') => {
                self.filtering = true;
                if self.view == View::Training {
                    self.view = View::Overview;
                }
            }
            KeyCode::Char('k') | KeyCode::Delete => self.ask_kill(),
            KeyCode::Char('t') => self.set_view(if self.view == View::Training { View::Overview } else { View::Training }),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up => self.move_sel(-1),
            KeyCode::PageDown => self.move_sel(page),
            KeyCode::PageUp => self.move_sel(-page),
            KeyCode::Home | KeyCode::Char('g') => self.move_sel(-(self.rows.len() as isize)),
            KeyCode::End | KeyCode::Char('G') => self.move_sel(self.rows.len() as isize),
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.rebuild();
            }
            KeyCode::Esc if self.view == View::Training => self.view = View::Overview,
            _ => return false,
        }
        true
    }

    fn mouse(&mut self, ev: MouseEvent, _area: Rect, _cx: &mut Cx) {
        let pos = ratatui::layout::Position { x: ev.column, y: ev.row };
        match ev.kind {
            MouseEventKind::ScrollDown => self.move_sel(3),
            MouseEventKind::ScrollUp => self.move_sel(-3),
            MouseEventKind::Down(MouseButton::Left) => {
                if self.table_body.contains(pos) {
                    let i = self.scroll + (ev.row - self.table_body.y) as usize;
                    if i < self.rows.len() {
                        self.sel = i;
                        self.pinned = i > 0;
                        self.sel_pid = self.selected().map(|p| p.pid);
                    }
                } else if self.filter_box.contains(pos) && self.pending_kill.is_none() {
                    self.filtering = true;
                } else if self.filtering {
                    self.filtering = false;
                }
            }
            _ => {}
        }
    }

    fn side(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        self.side_hits.clear();
        for (n, &(icon, label, view)) in SIDE.iter().enumerate() {
            if n as u16 >= area.height {
                break;
            }
            let r = Rect { y: area.y + n as u16, height: 1, ..area };
            let right = match view {
                View::Training => match &self.snap.train {
                    Some(tr) if tr.running => "●".to_string(),
                    _ => String::new(),
                },
                _ => String::new(),
            };
            ui::side_row(f, r, icon, label, &right, self.view == view, t);
            self.side_hits.push((r, view));
        }
    }

    fn side_mouse(&mut self, ev: MouseEvent, _area: Rect, _cx: &mut Cx) {
        if let MouseEventKind::Down(MouseButton::Left) = ev.kind {
            let pos = ratatui::layout::Position { x: ev.column, y: ev.row };
            if let Some(&(_, v)) = self.side_hits.iter().find(|(r, _)| r.contains(pos)) {
                self.set_view(v);
            }
        }
    }
}

#[cfg(test)]
mod tests;
