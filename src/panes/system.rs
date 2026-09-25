//! system: a small task manager in the spirit of TMOG. Seven views, switched from the sidebar, with 1-7 or tab:
//!   summary      cpu / gpu / memory · disk / network · disk io cards and the top processes
//!   processes    the whole table (flat or as a tree), sortable by cpu, memory, disk read/write, name; kill
//!   performance  big cpu / gpu / memory history graphs and one small graph per logical processor
//!   startup      Run keys + Startup folders (Windows), autostart .desktop files + systemd --user (Linux)
//!   services     the service manager's list with state, start type and description
//!   connections  netstat / ss, mapped to process names
//!   system info  OS, CPU, memory, GPUs, board, disks (collected once)
//!
//! All sampling happens on background threads (sampler.rs every second, probes.rs while a view needs it); the UI
//! thread only swaps in the latest results and draws them.

mod probes;
mod sampler;
mod views;

use crate::pane::{Cx, Pane};
use crate::theme::Theme;
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use probes::{Conn, Info, Kind, Probes, Service, StartupItem};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use sampler::{GpuState, Proc, Shared, Snap};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use views::{Cell, Col, col};

const SPARK: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const WARN: Color = Color::Rgb(0xe0, 0xa0, 0x40);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sort {
    Cpu,
    Mem,
    Read,
    Write,
    Name,
}

impl Sort {
    fn label(self) -> &'static str {
        match self {
            Sort::Cpu => "cpu",
            Sort::Mem => "memory",
            Sort::Read => "disk read",
            Sort::Write => "disk write",
            Sort::Name => "name",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum View {
    Summary,
    Processes,
    Performance,
    Startup,
    Services,
    Connections,
    Info,
}

/// Sidebar rows / number keys: view, icon, label.
const VIEWS: [(View, &str, &str); 7] = [
    (View::Summary, "chart", "summary"),
    (View::Processes, "system", "processes"),
    (View::Performance, "gauge", "performance"),
    (View::Startup, "play", "startup"),
    (View::Services, "package", "services"),
    (View::Connections, "cloud", "connections"),
    (View::Info, "doc", "system info"),
];

impl View {
    fn index(self) -> usize {
        VIEWS.iter().position(|v| v.0 == self).unwrap_or(0)
    }
    /// Which of the three plain lists (startup, services, connections) this view is.
    fn list(self) -> Option<usize> {
        match self {
            View::Startup => Some(0),
            View::Services => Some(1),
            View::Connections => Some(2),
            _ => None,
        }
    }
    fn has_procs(self) -> bool {
        matches!(self, View::Summary | View::Processes)
    }
}

/// The process table columns: pid, name, cpu, memory, read/s, write/s, threads, user.
const PROC_COLS: [Col; 8] = [
    col("pid", 6, 0, false),
    col("name", 16, 44, false),
    col("cpu %", 6, 0, true),
    col("memory", 8, 0, true),
    col("read/s", 8, 0, true),
    col("write/s", 8, 0, true),
    col("threads", 7, 0, true),
    col("user", 8, 22, false),
];
const PROC_SORT_COL: [(usize, Sort); 5] = [(1, Sort::Name), (2, Sort::Cpu), (3, Sort::Mem), (4, Sort::Read), (5, Sort::Write)];

/// Tree mode: how each row is drawn.
#[derive(Clone, Debug)]
struct TreeRow {
    guide: String,
    depth: usize,
    kids: bool,
    folded: bool,
    /// Descendants hidden under a folded row.
    hidden: usize,
}

/// Selection, scroll and filter of one of the plain lists.
#[derive(Default)]
struct List {
    sel: usize,
    scroll: usize,
    filter: String,
    /// Indices into the data, filtered.
    rows: Vec<usize>,
    body: Rect,
}

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
    /// Indices into `snap.procs`, filtered + sorted (tree order in tree mode).
    rows: Vec<usize>,
    tree: bool,
    /// Parallel to `rows` in tree mode.
    tree_rows: Vec<TreeRow>,
    /// pids folded in the tree.
    folded: HashSet<u32>,
    sel: usize,
    sel_pid: Option<u32>,
    /// The user moved the cursor: follow that process through re-sorts. Until then the cursor stays on row 0.
    pinned: bool,
    scroll: usize,
    pending_kill: Option<(u32, String)>,
    killer: fn(u32) -> Result<(), String>,
    // startup / services / connections / info, collected on demand
    probes: Arc<Probes>,
    /// false in tests: never start a probe thread.
    probes_live: bool,
    probe_seq: u64,
    startup: Option<Arc<Vec<StartupItem>>>,
    services: Option<Arc<Vec<Service>>>,
    conns: Option<Arc<Vec<Conn>>>,
    info: Option<Arc<Info>>,
    lists: [List; 3],
    // hit areas from the last render
    table_body: Rect,
    table_head: Rect,
    head_hits: Vec<(u16, u16, Sort)>,
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
            view: View::Summary,
            filter: String::new(),
            filtering: false,
            rows: vec![],
            tree: false,
            tree_rows: vec![],
            folded: HashSet::new(),
            sel: 0,
            sel_pid: None,
            pinned: false,
            scroll: 0,
            pending_kill: None,
            killer: sampler::kill,
            probes: Arc::new(Probes::default()),
            probes_live: true,
            probe_seq: 0,
            startup: None,
            services: None,
            conns: None,
            info: None,
            lists: Default::default(),
            table_body: Rect::default(),
            table_head: Rect::default(),
            head_hits: vec![],
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

    /// Ask for the current view's slow data (a no-op while it's fresh or already being collected).
    fn want(&mut self, cx: &Cx) {
        if !self.probes_live {
            return;
        }
        let (kind, every) = match self.view {
            View::Startup => (Kind::Startup, Some(Duration::from_secs(30))),
            View::Services => (Kind::Services, Some(Duration::from_secs(5))),
            View::Connections => (Kind::Conns, Some(Duration::from_secs(2))),
            View::Info => (Kind::Info, None),
            _ => return,
        };
        self.probes.want(kind, every, self.snap.clone(), cx.waker());
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
        let changed = {
            let d = self.probes.lock();
            (d.seq != self.probe_seq).then(|| (d.seq, d.startup.clone(), d.services.clone(), d.conns.clone(), d.info.clone()))
        };
        if let Some((seq, st, sv, cn, inf)) = changed {
            self.probe_seq = seq;
            self.startup = st;
            self.services = sv;
            self.conns = cn;
            self.info = inf;
            for i in 0..3 {
                self.rebuild_list(i);
            }
        }
    }

    // ------------------------------------------------------------------ processes

    fn cmp(&self, a: usize, b: usize) -> Ordering {
        let p = &self.snap.procs;
        let (x, y) = (&p[a], &p[b]);
        let by = match self.sort {
            Sort::Cpu => y.cpu.total_cmp(&x.cpu),
            Sort::Mem => y.mem.cmp(&x.mem),
            Sort::Read => y.read.total_cmp(&x.read),
            Sort::Write => y.write.total_cmp(&x.write),
            Sort::Name => x.name.to_lowercase().cmp(&y.name.to_lowercase()),
        };
        by.then(x.pid.cmp(&y.pid))
    }

    fn rebuild(&mut self) {
        let q = self.filter.trim().to_lowercase();
        let snap = self.snap.clone();
        let procs = &snap.procs;
        let hit = |p: &Proc| q.is_empty() || p.name.to_lowercase().contains(&q) || p.pid.to_string() == q;
        self.tree_rows.clear();
        if !self.tree {
            let mut rows: Vec<usize> = (0..procs.len()).filter(|&i| hit(&procs[i])).collect();
            match self.sort {
                Sort::Name => rows.sort_by_cached_key(|&i| (procs[i].name.to_lowercase(), procs[i].pid)),
                _ => rows.sort_by(|&a, &b| self.cmp(a, b)),
            }
            self.rows = rows;
        } else {
            self.rows = self.tree_order(&q);
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

    /// Rows in tree order: children under their parent, siblings sorted. While filtering, matches keep their
    /// ancestors (for context) and nothing is folded. Fills `tree_rows`.
    fn tree_order(&mut self, q: &str) -> Vec<usize> {
        let snap = self.snap.clone();
        let procs = &snap.procs;
        let n = procs.len();
        let at: HashMap<u32, usize> = procs.iter().enumerate().map(|(i, p)| (p.pid, i)).collect();
        // a parent only counts if it started before the child: pids get reused
        let parent: Vec<Option<usize>> = procs
            .iter()
            .map(|p| if p.ppid == 0 || p.ppid == p.pid { None } else { at.get(&p.ppid).copied().filter(|&j| procs[j].start <= p.start) })
            .collect();
        let mut vis = vec![q.is_empty(); n];
        if !q.is_empty() {
            for i in 0..n {
                let p = &procs[i];
                if p.name.to_lowercase().contains(q) || p.pid.to_string() == q {
                    let (mut j, mut hops) = (Some(i), 0);
                    while let Some(k) = j {
                        if vis[k] || hops > 64 {
                            break;
                        }
                        vis[k] = true;
                        j = parent[k];
                        hops += 1;
                    }
                }
            }
        }
        let mut kids: Vec<Vec<usize>> = vec![vec![]; n];
        let mut roots = vec![];
        for i in (0..n).filter(|&i| vis[i]) {
            match parent[i] {
                Some(p) if vis[p] => kids[p].push(i),
                _ => roots.push(i),
            }
        }
        roots.sort_by(|&a, &b| self.cmp(a, b));
        for k in kids.iter_mut() {
            if k.len() > 1 {
                k.sort_by(|&a, &b| self.cmp(a, b));
            }
        }
        let folding = q.is_empty();
        let mut rows = Vec::with_capacity(n);
        let mut meta = Vec::with_capacity(n);
        let mut seen = vec![false; n];
        struct Walk<'a> {
            procs: &'a [Proc],
            kids: &'a [Vec<usize>],
            folded: &'a HashSet<u32>,
            folding: bool,
            seen: &'a mut [bool],
            rows: &'a mut Vec<usize>,
            meta: &'a mut Vec<TreeRow>,
        }
        /// Hide everything under a folded row: count it and mark it seen.
        fn hide(w: &mut Walk, i: usize, guard: usize) -> usize {
            let (mut n, kids) = (0, w.kids);
            for &c in &kids[i] {
                if !w.seen[c] && guard < 64 {
                    w.seen[c] = true;
                    n += 1 + hide(w, c, guard + 1);
                }
            }
            n
        }
        fn walk(w: &mut Walk, i: usize, depth: usize, prefix: &str, last: bool) {
            w.seen[i] = true;
            let guide = if depth == 0 { String::new() } else { format!("{prefix}{}", if last { "└─" } else { "├─" }) };
            let kids = !w.kids[i].is_empty();
            let folded = kids && w.folding && w.folded.contains(&w.procs[i].pid);
            let hidden = if folded { hide(w, i, 0) } else { 0 };
            w.rows.push(i);
            w.meta.push(TreeRow { guide, depth, kids, folded, hidden });
            if folded || depth > 64 {
                return;
            }
            let next = if depth == 0 { String::new() } else { format!("{prefix}{}", if last { "  " } else { "│ " }) };
            let list = w.kids[i].clone();
            let n = list.len();
            for (k, c) in list.into_iter().enumerate() {
                if !w.seen[c] {
                    walk(w, c, depth + 1, &next, k + 1 == n);
                }
            }
        }
        let mut w = Walk { procs, kids: &kids, folded: &self.folded, folding, seen: &mut seen, rows: &mut rows, meta: &mut meta };
        for &r in &roots {
            walk(&mut w, r, 0, "", true);
        }
        // anything caught in a parent cycle (shouldn't happen, pids are checked) still gets shown
        for i in 0..n {
            if vis[i] && !w.seen[i] {
                walk(&mut w, i, 0, "", true);
            }
        }
        self.tree_rows = meta;
        rows
    }

    fn selected(&self) -> Option<&Proc> {
        self.rows.get(self.sel).map(|&i| &self.snap.procs[i])
    }

    fn move_sel(&mut self, d: isize) {
        if let Some(li) = self.view.list() {
            let l = &mut self.lists[li];
            if !l.rows.is_empty() {
                l.sel = (l.sel as isize + d).clamp(0, l.rows.len() as isize - 1) as usize;
            }
            return;
        }
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
        self.rebuild();
    }

    fn set_view(&mut self, v: View, cx: &Cx) {
        self.view = v;
        self.filtering = false;
        self.want(cx);
    }

    fn cycle_view(&mut self, d: isize, cx: &Cx) {
        let i = (self.view.index() as isize + d).rem_euclid(VIEWS.len() as isize) as usize;
        self.set_view(VIEWS[i].0, cx);
    }

    fn toggle_tree(&mut self) {
        self.tree = !self.tree;
        self.pinned = self.sel > 0;
        self.rebuild();
    }

    /// Fold / unfold the selected tree row (`Some(true)` = fold). Returns false if there was nothing to do.
    fn fold(&mut self, fold: Option<bool>) -> bool {
        let Some(m) = self.tree_rows.get(self.sel).cloned() else { return false };
        let Some(pid) = self.selected().map(|p| p.pid) else { return false };
        let want = fold.unwrap_or(!m.folded);
        if m.kids && want != m.folded && self.filter.trim().is_empty() {
            if want {
                self.folded.insert(pid);
            } else {
                self.folded.remove(&pid);
            }
            self.pinned = true;
            self.sel_pid = Some(pid);
            self.rebuild();
            return true;
        }
        if fold == Some(true) && m.depth > 0 {
            // already folded or a leaf: jump to the parent
            if let Some(i) = (0..self.sel).rev().find(|&i| self.tree_rows[i].depth + 1 == m.depth) {
                self.sel = i;
                self.pinned = true;
                self.sel_pid = self.selected().map(|p| p.pid);
            }
            return true;
        }
        if fold == Some(false) && m.kids && !m.folded && self.sel + 1 < self.rows.len() {
            self.move_sel(1);
            return true;
        }
        false
    }

    fn ask_kill(&mut self) {
        if self.view == View::Connections {
            let l = &self.lists[2];
            if let Some(c) = l.rows.get(l.sel).and_then(|&i| self.conns.as_ref()?.get(i))
                && c.pid > 4
            {
                self.pending_kill = Some((c.pid, if c.process.is_empty() { "?".into() } else { c.process.clone() }));
            }
            return;
        }
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

    // ------------------------------------------------------------------ the plain lists

    fn list_len(&self, li: usize) -> usize {
        match li {
            0 => self.startup.as_ref().map_or(0, |v| v.len()),
            1 => self.services.as_ref().map_or(0, |v| v.len()),
            _ => self.conns.as_ref().map_or(0, |v| v.len()),
        }
    }

    /// Does row `i` of list `li` match the (lowercase) filter?
    fn list_hit(&self, li: usize, i: usize, q: &str) -> bool {
        let has = |s: &str| s.to_lowercase().contains(q);
        match li {
            0 => self.startup.as_ref().is_some_and(|v| {
                let s = &v[i];
                has(&s.name) || has(&s.command) || has(&s.location) || (q == "off" && s.enabled == Some(false))
            }),
            1 => self.services.as_ref().is_some_and(|v| {
                let s = &v[i];
                has(&s.name) || has(&s.display) || has(&s.desc) || s.state == q || s.start == q || s.pid.to_string() == q
            }),
            _ => self.conns.as_ref().is_some_and(|v| {
                let c = &v[i];
                has(&c.process) || has(&c.local) || has(&c.remote) || c.state == q || c.proto == q || c.pid.to_string() == q
            }),
        }
    }

    fn rebuild_list(&mut self, li: usize) {
        let q = self.lists[li].filter.trim().to_lowercase();
        let rows: Vec<usize> = (0..self.list_len(li)).filter(|&i| q.is_empty() || self.list_hit(li, i, &q)).collect();
        let l = &mut self.lists[li];
        l.rows = rows;
        l.sel = l.sel.min(l.rows.len().saturating_sub(1));
    }

    fn filter_mut(&mut self) -> &mut String {
        match self.view.list() {
            Some(li) => &mut self.lists[li].filter,
            None => &mut self.filter,
        }
    }

    fn refilter(&mut self) {
        match self.view.list() {
            Some(li) => self.rebuild_list(li),
            None => self.rebuild(),
        }
    }

    fn filterable(&self) -> bool {
        self.view.has_procs() || self.view.list().is_some()
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

    /// The filter box (or the kill question, which takes its place).
    fn draw_filter(&mut self, f: &mut Frame, r: Rect, t: &Theme, focused: bool, text: &str, placeholder: &str) {
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
        let l = if text.is_empty() && !self.filtering {
            Line::styled(format!("  {placeholder}   (/)"), ui::muted(t))
        } else {
            let mut v = vec![Span::raw(format!("  {text}"))];
            if self.filtering {
                v.push(Span::styled(" ", Style::default().add_modifier(Modifier::REVERSED)));
            }
            Line::from(v)
        };
        f.render_widget(Paragraph::new(l), inner);
    }

    fn proc_cells(&self, n: usize, t: &Theme) -> Vec<Cell> {
        let p = &self.snap.procs[self.rows[n]];
        let io = |v: f64| if v >= 1.0 { Cell::plain(ui::human_bytes(v as u64)) } else { Cell::new("·", ui::fg(t.frame)) };
        let mut name = Cell::plain(&p.name);
        if let Some(m) = self.tree_rows.get(n).filter(|_| self.tree) {
            let mark = match (m.kids, m.folded, m.depth) {
                (true, true, _) => "▸ ",
                (true, false, _) => "▾ ",
                (false, _, 0) => "  ",
                (false, _, _) => "─ ",
            };
            name = name.with_pre(format!("{}{mark}", m.guide), ui::muted(t));
            if m.folded && m.hidden > 0 {
                name.text = format!("{}  +{}", p.name, m.hidden);
            }
        }
        let cpu_style = if p.cpu >= 25.0 { ui::bold_accent(t) } else if p.cpu >= 1.0 { Style::default() } else { ui::muted(t) };
        vec![
            Cell::new(p.pid.to_string(), ui::muted(t)),
            name,
            Cell::new(format!("{:.1}", p.cpu), cpu_style),
            Cell::plain(ui::human_bytes(p.mem)),
            io(p.read),
            io(p.write),
            Cell::new(p.threads.to_string(), ui::muted(t)),
            Cell::new(p.user.to_string(), ui::muted(t)),
        ]
    }

    fn draw_table(&mut self, f: &mut Frame, r: Rect, t: &Theme, focused: bool) {
        if r.height < 2 {
            return;
        }
        let r = Rect { x: r.x + 1, width: r.width.saturating_sub(2), ..r };
        let total = r.width.saturating_sub(1) as usize; // the last column is the scrollbar
        let w = views::widths(&PROC_COLS, total);
        let sorted = PROC_SORT_COL.iter().find(|(_, s)| *s == self.sort).map(|(i, _)| *i);
        f.render_widget(Paragraph::new(views::head_line(&PROC_COLS, &w, sorted, total, t)), Rect { height: 1, ..r });
        // header hit areas, for click-to-sort
        self.table_head = Rect { height: 1, ..r };
        self.head_hits.clear();
        let mut x = r.x;
        for (i, cw) in w.iter().enumerate() {
            if let Some(&(_, s)) = PROC_SORT_COL.iter().find(|(c, _)| *c == i) {
                self.head_hits.push((x, x + *cw as u16, s));
            }
            x += (*cw + views::GAP) as u16;
        }
        let body = Rect { y: r.y + 1, height: r.height - 1, width: total as u16, ..r };
        self.table_body = body;
        let h = body.height as usize;
        views::clamp_scroll(self.sel, &mut self.scroll, h, self.rows.len());
        let sel = views::sel_style(t, focused);
        let mut lines = Vec::with_capacity(h);
        for n in (self.scroll..self.rows.len()).take(h) {
            lines.push(views::row_line(self.proc_cells(n, t), &PROC_COLS, &w, (n == self.sel).then_some(sel), total));
        }
        if self.rows.is_empty() {
            let msg = if self.snap.seq == 0 { "sampling…" } else { "no process matches" };
            lines.push(Line::styled(msg, ui::muted(t)));
        }
        f.render_widget(Paragraph::new(lines), body);
        views::scrollbar(f, r.right() - 1, body, self.scroll, self.rows.len(), t);
    }

    /// A plain list view: summary line, filter box, table, and a detail line about the selected row.
    fn draw_list(&mut self, f: &mut Frame, body: Rect, li: usize, t: &Theme, focused: bool) {
        let (cols, what): (&[Col], &str) = match li {
            0 => (&views::STARTUP_COLS, "startup entries"),
            1 => (&views::SERVICE_COLS, "services"),
            _ => (&views::CONN_COLS, "connections"),
        };
        let loaded = match li {
            0 => self.startup.is_some(),
            1 => self.services.is_some(),
            _ => self.conns.is_some(),
        };
        let mut y = body.y;
        f.render_widget(Paragraph::new(self.list_summary(li, t)), Rect { y, height: 1, ..body });
        y += 1;
        if body.bottom() >= y + 3 {
            let filter = self.lists[li].filter.clone();
            self.draw_filter(f, Rect { y, height: 3, ..body }, t, focused, &filter, &format!("filter {what}…"));
            y += 3;
        }
        let detail = self.list_detail(li);
        let table_bottom = if detail.is_some() && body.bottom() > y + 3 { body.bottom() - 1 } else { body.bottom() };
        let r = Rect { x: body.x + 1, y, width: body.width.saturating_sub(2), height: table_bottom.saturating_sub(y) };
        if r.height < 2 {
            return;
        }
        let total = r.width.saturating_sub(1) as usize;
        let w = views::widths(cols, total);
        f.render_widget(Paragraph::new(views::head_line(cols, &w, None, total, t)), Rect { height: 1, ..r });
        let tb = Rect { y: r.y + 1, height: r.height - 1, width: total as u16, ..r };
        let h = tb.height as usize;
        let (sel, n) = {
            let l = &mut self.lists[li];
            l.body = tb;
            views::clamp_scroll(l.sel, &mut l.scroll, h, l.rows.len());
            (l.sel, l.rows.len())
        };
        let l = &self.lists[li];
        let ss = views::sel_style(t, focused);
        let mut lines = Vec::with_capacity(h);
        for k in (l.scroll..n).take(h) {
            let i = l.rows[k];
            let cells = match li {
                0 => views::startup_cells(&self.startup.as_ref().unwrap()[i], t),
                1 => views::service_cells(&self.services.as_ref().unwrap()[i], t),
                _ => views::conn_cells(&self.conns.as_ref().unwrap()[i], t),
            };
            lines.push(views::row_line(cells, cols, &w, (k == sel).then_some(ss), total));
        }
        if n == 0 {
            let msg = if !loaded { format!("reading {what}…") } else if l.filter.is_empty() { format!("no {what} found") } else { format!("no {what} match") };
            lines.push(Line::styled(msg, ui::muted(t)));
        }
        let scroll = l.scroll;
        f.render_widget(Paragraph::new(lines), tb);
        views::scrollbar(f, r.right() - 1, tb, scroll, n, t);
        if let Some(d) = detail
            && table_bottom < body.bottom()
        {
            let row = Rect { y: body.bottom() - 1, height: 1, ..body };
            f.render_widget(Paragraph::new(Line::styled(format!(" {}", ui::fit(&d, body.width.saturating_sub(2) as usize)), ui::muted(t))), row);
        }
    }

    fn list_summary(&self, li: usize, t: &Theme) -> Line<'static> {
        let n = |v: usize| v.to_string();
        match li {
            0 => {
                let v = self.startup.as_ref().map_or(&[][..], |v| v.as_slice());
                let off = v.iter().filter(|s| s.enabled == Some(false)).count();
                views::summary_line(&[(n(v.len()), "startup entries"), (n(v.len() - off), "on"), (n(off), "off")], t)
            }
            1 => {
                let v = self.services.as_ref().map_or(&[][..], |v| v.as_slice());
                let running = v.iter().filter(|s| s.state == "running").count();
                let failed = v.iter().filter(|s| s.state == "failed").count();
                let mut parts = vec![(n(v.len()), "services"), (n(running), "running"), (n(v.len() - running - failed), "stopped")];
                if failed > 0 {
                    parts.push((n(failed), "failed"));
                }
                views::summary_line(&parts, t)
            }
            _ => {
                let v = self.conns.as_ref().map_or(&[][..], |v| v.as_slice());
                let est = v.iter().filter(|c| c.state == "established").count();
                let listen = v.iter().filter(|c| c.state == "listening").count();
                let mut per: HashMap<&str, usize> = HashMap::new();
                for c in v.iter().filter(|c| c.state == "established" && !c.process.is_empty()) {
                    *per.entry(c.process.as_str()).or_default() += 1;
                }
                let mut top: Vec<(&str, usize)> = per.into_iter().collect();
                top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
                let mut line = views::summary_line(&[(n(v.len()), "connections"), (n(est), "established"), (n(listen), "listening")], t);
                if !top.is_empty() {
                    line.spans.push(Span::styled("   most active  ", ui::muted(t)));
                    for (i, (name, c)) in top.iter().take(4).enumerate() {
                        if i > 0 {
                            line.spans.push(Span::styled(" · ", ui::muted(t)));
                        }
                        line.spans.push(Span::styled(name.to_string(), ui::accent(t)));
                        line.spans.push(Span::styled(format!(" {c}"), ui::muted(t)));
                    }
                }
                line
            }
        }
    }

    /// The whole of what the selected row can't fit: full command, description, endpoints.
    fn list_detail(&self, li: usize) -> Option<String> {
        let l = &self.lists[li];
        let i = *l.rows.get(l.sel)?;
        match li {
            0 => self.startup.as_ref().map(|v| v[i].command.clone()).filter(|s| !s.is_empty()),
            1 => self.services.as_ref().map(|v| {
                let s = &v[i];
                if s.desc.is_empty() { s.display.clone() } else { format!("{} — {}", s.display, s.desc) }
            }),
            _ => self.conns.as_ref().map(|v| {
                let c = &v[i];
                format!("{} {} → {}  {}  {} (pid {})", c.proto, c.local, c.remote, c.state, if c.process.is_empty() { "?" } else { &c.process }, c.pid)
            }),
        }
    }

    fn draw_summary(&mut self, f: &mut Frame, body: Rect, t: &Theme, focused: bool) {
        let has_gpu = !matches!(self.gpu, GpuState::Missing);
        let (w, mut y) = (body.width, body.y);
        let (lw, rw) = {
            let (a, b) = split_h(body);
            (a.width.saturating_sub(4) as usize, b.width.saturating_sub(4) as usize)
        };
        let top_h: u16 = 7;
        let mid_h: u16 = 2 + (2 + self.snap.disks.len() as u16).max(4);
        let avail = body.height;
        let min_table = 6;
        if avail >= top_h + 3 + min_table {
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
        if avail >= top_h + mid_h + 3 + min_table {
            let r = Rect { y, height: mid_h, ..body };
            let (a, b) = split_h(r);
            card(f, a, "chart", "memory · disk", self.mem_lines(lw, t), t);
            card(f, b, "cloud", "network · disk io", self.io_lines(rw, t), t);
            y += mid_h;
        }
        self.draw_procs(f, Rect { y, height: body.bottom().saturating_sub(y), ..body }, t, focused);
    }

    fn draw_procs(&mut self, f: &mut Frame, r: Rect, t: &Theme, focused: bool) {
        let mut y = r.y;
        if r.bottom() >= y + 3 {
            let filter = self.filter.clone();
            self.draw_filter(f, Rect { y, height: 3, ..r }, t, focused, &filter, "filter processes…");
            y += 3;
        }
        self.draw_table(f, Rect { y, height: r.bottom().saturating_sub(y), ..r }, t, focused);
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
        let n = |li: usize| self.lists[li].rows.len();
        Some(match self.view {
            View::Summary | View::Processes => {
                format!("{} processes · sorted by {}{}", self.rows.len(), self.sort.label(), if self.tree { " · tree" } else { "" })
            }
            View::Performance => format!("{} logical processors", self.snap.cores.len()),
            View::Startup => format!("{} startup entries", n(0)),
            View::Services => format!("{} services", n(1)),
            View::Connections => format!("{} connections", n(2)),
            View::Info => "system info".into(),
        })
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
        self.want(cx);
    }

    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        self.ensure_started(cx);
        let t = cx.theme;
        let mut hints: Vec<(&str, &str)> = vec![];
        if self.pending_kill.is_some() {
            hints.extend([("y", "kill it"), ("esc", "leave it")]);
        } else if self.filtering {
            hints.extend([("enter", "keep filter"), ("esc", "clear"), ("↑↓", "move")]);
        } else {
            match self.view {
                View::Summary | View::Processes => {
                    hints.extend([("c/m/r/w/n", "sort cpu·memory·read·write·name"), ("t", if self.tree { "flat list" } else { "tree" })]);
                    if self.tree {
                        hints.push(("←→", "fold"));
                    }
                    hints.extend([("k", "kill"), ("/", "filter")]);
                }
                View::Connections => hints.extend([("/", "filter"), ("k", "kill process")]),
                View::Startup | View::Services => hints.push(("/", "filter")),
                View::Performance | View::Info => {}
            }
            hints.push(("1-7 tab", "views"));
        }
        let body = ui::hint_line(f, area, &hints, t);
        self.table_body = Rect::default();
        match self.view {
            View::Summary => self.draw_summary(f, body, t, cx.focused),
            View::Processes => self.draw_procs(f, body, t, cx.focused),
            View::Performance => self.draw_performance(f, body, t),
            View::Info => self.draw_info(f, body, t),
            v => self.draw_list(f, body, v.list().unwrap_or(0), t, cx.focused),
        }
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
                    self.filter_mut().push(c);
                    self.refilter();
                }
                KeyCode::Backspace => {
                    self.filter_mut().pop();
                    self.refilter();
                }
                KeyCode::Enter => self.filtering = false,
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter_mut().clear();
                    self.refilter();
                }
                KeyCode::Up => self.move_sel(-1),
                KeyCode::Down => self.move_sel(1),
                _ => return false,
            }
            return true;
        }
        // views
        match key.code {
            KeyCode::Char(c @ '1'..='7') => {
                self.set_view(VIEWS[c as usize - '1' as usize].0, cx);
                return true;
            }
            KeyCode::Tab => {
                self.cycle_view(1, cx);
                return true;
            }
            KeyCode::BackTab => {
                self.cycle_view(-1, cx);
                return true;
            }
            _ => {}
        }
        let procs = self.view.has_procs();
        let page = match self.view.list() {
            Some(li) => self.lists[li].body.height.max(1) as isize,
            None => self.table_body.height.max(1) as isize,
        };
        let len = match self.view.list() {
            Some(li) => self.lists[li].rows.len(),
            None => self.rows.len(),
        } as isize;
        let moves = procs || self.view.list().is_some();
        match key.code {
            KeyCode::Char('c') if procs => self.set_sort(Sort::Cpu),
            KeyCode::Char('m') if procs => self.set_sort(Sort::Mem),
            KeyCode::Char('r') if procs => self.set_sort(Sort::Read),
            KeyCode::Char('w') if procs => self.set_sort(Sort::Write),
            KeyCode::Char('n') if procs => self.set_sort(Sort::Name),
            KeyCode::Char('t') if procs => self.toggle_tree(),
            KeyCode::Left if procs && self.tree => return self.fold(Some(true)),
            KeyCode::Right if procs && self.tree => return self.fold(Some(false)),
            KeyCode::Enter if procs && self.tree => return self.fold(None),
            KeyCode::Char('k') | KeyCode::Delete if procs || self.view == View::Connections => self.ask_kill(),
            KeyCode::Char('/') if self.filterable() => self.filtering = true,
            KeyCode::Down | KeyCode::Char('j') if moves => self.move_sel(1),
            KeyCode::Up if moves => self.move_sel(-1),
            KeyCode::PageDown if moves => self.move_sel(page),
            KeyCode::PageUp if moves => self.move_sel(-page),
            KeyCode::Home | KeyCode::Char('g') if moves => self.move_sel(-len),
            KeyCode::End | KeyCode::Char('G') if moves => self.move_sel(len),
            KeyCode::Esc if self.filterable() && !self.filter_mut().is_empty() => {
                self.filter_mut().clear();
                self.refilter();
            }
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
                if let Some(li) = self.view.list() {
                    let l = &mut self.lists[li];
                    if l.body.contains(pos) {
                        let i = l.scroll + (ev.row - l.body.y) as usize;
                        if i < l.rows.len() {
                            l.sel = i;
                        }
                        return;
                    }
                } else if self.view.has_procs() && self.table_head.contains(pos) {
                    if let Some(&(_, _, s)) = self.head_hits.iter().find(|(a, b, _)| ev.column >= *a && ev.column < *b) {
                        self.set_sort(s);
                    }
                    return;
                } else if self.view.has_procs() && self.table_body.contains(pos) {
                    let i = self.scroll + (ev.row - self.table_body.y) as usize;
                    if i < self.rows.len() {
                        if i == self.sel && self.tree {
                            self.fold(None);
                        }
                        self.sel = i;
                        self.pinned = i > 0;
                        self.sel_pid = self.selected().map(|p| p.pid);
                    }
                    return;
                }
                if self.filterable() && self.filter_box.contains(pos) && self.pending_kill.is_none() {
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
        for (n, &(view, icon, label)) in VIEWS.iter().enumerate() {
            if n as u16 >= area.height {
                break;
            }
            let r = Rect { y: area.y + n as u16, height: 1, ..area };
            ui::side_row(f, r, icon, label, &(n + 1).to_string(), self.view == view, t);
            self.side_hits.push((r, view));
        }
    }

    fn side_mouse(&mut self, ev: MouseEvent, _area: Rect, cx: &mut Cx) {
        if let MouseEventKind::Down(MouseButton::Left) = ev.kind {
            let pos = ratatui::layout::Position { x: ev.column, y: ev.row };
            if let Some(&(_, v)) = self.side_hits.iter().find(|(r, _)| r.contains(pos)) {
                self.set_view(v, cx);
            }
        }
    }
}

#[cfg(test)]
mod tests;
