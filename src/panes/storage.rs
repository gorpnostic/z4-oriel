//! storage: drives, smart cleanup, a big-folder explorer, installed apps (uninstall) and package installs — a port
//! of nest's storage app (wren/storage_view.py).
//!
//! Safety rules:
//!   * nothing is deleted, uninstalled or installed without an in-app y/esc confirmation naming the exact target
//!   * "review" rows (your data) never delete: enter opens them in the files app
//!   * uninstall/install and anything needing root run in a terminal pane, so you see and answer every prompt
//!   * sizes are measured on background threads and never follow links/junctions (see storage/scan.rs)
//!   * under `cargo test` the delete/run paths only record what they would have done

mod draw;
mod scan;
mod sys;
#[cfg(test)]
mod tests;

use crate::pane::{Action, Cx, Place, Waker};
use ratatui::layout::Rect;
use scan::{Cmd, Drive, Find, How, Kind, Target};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use sys::{App, Found};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum View {
    Cleanup,
    Folders,
    Apps,
    Install,
}

/// sel value meaning "nothing picked yet: the top row" (it follows the list while sizes re-sort it).
const TOP: usize = usize::MAX;

const VIEWS: [View; 4] = [View::Cleanup, View::Folders, View::Apps, View::Install];

impl View {
    fn i(self) -> usize {
        self as usize
    }
}

enum Msg {
    Drives(Vec<Drive>),
    Targets(u64, Vec<Target>),
    Paths(u64, &'static str, Vec<PathBuf>),
    Size { g: u64, id: &'static str, bytes: u64, done: bool },
    Cleaned { label: String, freed: u64 },
    Listing { g: u64, rows: Vec<FRow> },
    FSize { g: u64, idx: usize, bytes: u64, done: bool },
    FDone { g: u64, big: Vec<(u64, PathBuf)> },
    FErr(u64, String),
    Apps(Result<Vec<App>, String>),
    Found { g: u64, res: Result<Vec<Found>, String> },
}

#[derive(Clone, Debug)]
struct FRow {
    name: String,
    path: PathBuf,
    dir: bool,
    size: u64,
    done: bool,
}

enum Pending {
    Clean(&'static str),
    Uninstall(usize),
    Install(usize),
}

struct Confirm {
    question: String,
    lines: Vec<String>,
    what: Pending,
}

/// Background threads send through this; wakes are throttled to ~10/s except for "done" messages.
#[derive(Clone)]
struct Out {
    tx: Sender<Msg>,
    waker: Option<Waker>,
    last: Arc<AtomicU64>,
    t0: Instant,
}

impl Out {
    fn send(&self, m: Msg, urgent: bool) {
        if self.tx.send(m).is_err() {
            return;
        }
        let now = self.t0.elapsed().as_millis() as u64;
        let last = self.last.load(Ordering::Relaxed);
        if urgent || now.saturating_sub(last) > 100 {
            self.last.store(now, Ordering::Relaxed);
            if let Some(w) = &self.waker {
                w.wake();
            }
        }
    }
}

pub struct Storage {
    view: View,
    out: Out,
    rx: Receiver<Msg>,
    started: bool,
    drives: Option<Vec<Drive>>,
    /// per view: the selected item (an index into that view's own list) and the table's scroll offset
    sel: [usize; 4],
    off: [usize; 4],
    // cleanup
    tgen: u64,
    tstop: Arc<AtomicBool>,
    targets: Vec<Target>,
    sizes: HashMap<&'static str, (u64, bool)>,
    // big folders
    froot: PathBuf,
    fgen: u64,
    fstop: Arc<AtomicBool>,
    frows: Vec<FRow>,
    fbig: Vec<(u64, PathBuf)>,
    fdone: bool,
    fstarted: bool,
    ferr: Option<String>,
    /// after going up: the folder we came out of, selected once it's listed
    pending_select: Option<PathBuf>,
    // installed apps
    apps: Option<Result<Vec<App>, String>>,
    filter: String,
    // install
    query: String,
    found_q: String,
    found: Option<Result<Vec<Found>, String>>,
    sgen: u64,
    searching: bool,
    /// the filter/search box has the keyboard
    input: bool,
    confirm: Option<Confirm>,
    table_hit: Option<(Rect, usize)>,
    side_hits: Vec<(Rect, View)>,
    install_via: &'static str,
    #[cfg(test)]
    launched: Vec<String>,
}

impl Storage {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        let install_via = if cfg!(windows) {
            "winget"
        } else if crate::config::which("yay").is_some() {
            "pacman + AUR"
        } else {
            "pacman"
        };
        Storage {
            view: View::Cleanup,
            out: Out { tx, waker: None, last: Arc::new(AtomicU64::new(0)), t0: Instant::now() },
            rx,
            started: false,
            drives: None,
            sel: [TOP; 4],
            off: [0; 4],
            tgen: 0,
            tstop: Arc::new(AtomicBool::new(false)),
            targets: vec![],
            sizes: HashMap::new(),
            froot: dirs::home_dir().unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "C:\\" } else { "/" })),
            fgen: 0,
            fstop: Arc::new(AtomicBool::new(false)),
            frows: vec![],
            fbig: vec![],
            fdone: false,
            fstarted: false,
            ferr: None,
            pending_select: None,
            apps: None,
            filter: String::new(),
            query: String::new(),
            found_q: String::new(),
            found: None,
            sgen: 0,
            searching: false,
            input: false,
            confirm: None,
            table_hit: None,
            side_hits: vec![],
            install_via,
            #[cfg(test)]
            launched: vec![],
        }
    }

    // ------------------------------------------------------------------ background work

    /// First contact with the app: remember how to wake it and start measuring.
    fn ensure(&mut self, cx: &Cx) {
        if self.started {
            return;
        }
        self.started = true;
        self.out.waker = Some(cx.waker());
        self.refresh_drives();
        self.scan_targets();
    }

    fn refresh_drives(&self) {
        let out = self.out.clone();
        std::thread::spawn(move || out.send(Msg::Drives(scan::drives()), true));
    }

    fn scan_targets(&mut self) {
        self.tstop.store(true, Ordering::Relaxed);
        self.tstop = Arc::new(AtomicBool::new(false));
        self.tgen += 1;
        self.sizes.clear();
        let (g, stop, out) = (self.tgen, self.tstop.clone(), self.out.clone());
        std::thread::spawn(move || {
            let list = scan::targets();
            out.send(Msg::Targets(g, list.clone()), true);
            for tg in list {
                let (stop, out) = (stop.clone(), out.clone());
                std::thread::spawn(move || {
                    let paths = scan::find_paths(&tg, &stop);
                    out.send(Msg::Paths(g, tg.id, paths.clone()), false);
                    let id = tg.id;
                    let mut bytes = 0;
                    if matches!(tg.find, Find::Journal) {
                        bytes = sys::run("journalctl", &["--disk-usage"]).and_then(|s| scan::parse_journal(&s)).unwrap_or(0);
                    }
                    if bytes == 0 {
                        let mut base = 0;
                        for p in &paths {
                            let o = out.clone();
                            base += scan::dir_size(p, &stop, None, &mut |n| o.send(Msg::Size { g, id, bytes: base + n, done: false }, false));
                        }
                        bytes = base;
                    }
                    if !stop.load(Ordering::Relaxed) {
                        out.send(Msg::Size { g, id, bytes, done: true }, true);
                    }
                });
            }
        });
    }

    fn scan_folder(&mut self, root: PathBuf) {
        self.fstop.store(true, Ordering::Relaxed);
        self.fstop = Arc::new(AtomicBool::new(false));
        self.fgen += 1;
        self.fstarted = true;
        self.froot = root.clone();
        self.frows.clear();
        self.fbig.clear();
        self.fdone = false;
        self.ferr = None;
        self.sel[View::Folders.i()] = TOP;
        self.off[View::Folders.i()] = 0;
        let (g, stop, out) = (self.fgen, self.fstop.clone(), self.out.clone());
        std::thread::spawn(move || {
            let rd = match std::fs::read_dir(&root) {
                Ok(rd) => rd,
                Err(e) => return out.send(Msg::FErr(g, format!("{}: {e}", root.display())), true),
            };
            let mut rows = vec![];
            for e in rd.flatten() {
                let Ok(md) = e.metadata() else { continue };
                let path = e.path();
                if scan::is_link(&md) || scan::skip_root_child(&path) {
                    continue;
                }
                let dir = md.is_dir();
                rows.push(FRow { name: e.file_name().to_string_lossy().into_owned(), path, dir, size: if dir { 0 } else { md.len() }, done: !dir });
            }
            let dirs: Vec<(usize, PathBuf)> = rows.iter().enumerate().filter(|(_, r)| r.dir).map(|(i, r)| (i, r.path.clone())).collect();
            let mut big: Vec<(u64, PathBuf)> = rows.iter().filter(|r| !r.dir && r.size > scan::BIG_FILE).map(|r| (r.size, r.path.clone())).collect();
            out.send(Msg::Listing { g, rows }, true);
            let next = AtomicUsize::new(0);
            let found = Mutex::new(vec![]);
            std::thread::scope(|s| {
                for _ in 0..dirs.len().clamp(1, 8) {
                    s.spawn(|| {
                        let mut mine = vec![];
                        loop {
                            let k = next.fetch_add(1, Ordering::Relaxed);
                            let Some((idx, p)) = dirs.get(k) else { break };
                            let idx = *idx;
                            let o = out.clone();
                            let n = scan::dir_size(p, &stop, Some(&mut mine), &mut |b| o.send(Msg::FSize { g, idx, bytes: b, done: false }, false));
                            if stop.load(Ordering::Relaxed) {
                                return;
                            }
                            out.send(Msg::FSize { g, idx, bytes: n, done: true }, true);
                        }
                        found.lock().unwrap().extend(mine);
                    });
                }
            });
            if !stop.load(Ordering::Relaxed) {
                big.extend(found.into_inner().unwrap());
                big.sort_by(|a, b| b.0.cmp(&a.0));
                big.truncate(15);
                out.send(Msg::FDone { g, big }, true);
            }
        });
    }

    fn load_apps(&mut self) {
        self.apps = None;
        let out = self.out.clone();
        std::thread::spawn(move || out.send(Msg::Apps(sys::installed_apps()), true));
    }

    fn search(&mut self) {
        let q = self.query.trim().to_string();
        if q.is_empty() {
            return;
        }
        self.sgen += 1;
        self.searching = true;
        self.found_q = q.clone();
        let (g, out) = (self.sgen, self.out.clone());
        std::thread::spawn(move || out.send(Msg::Found { g, res: sys::search(&q) }, true));
    }

    fn drain(&mut self, cx: &mut Cx) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Drives(d) => self.drives = Some(d),
                Msg::Targets(g, list) if g == self.tgen => {
                    for tg in &list {
                        self.sizes.entry(tg.id).or_insert((0, false));
                    }
                    self.targets = list;
                }
                Msg::Paths(g, id, paths) if g == self.tgen => {
                    if let Some(tg) = self.targets.iter_mut().find(|t| t.id == id) {
                        tg.paths = paths;
                    }
                }
                Msg::Size { g, id, bytes, done } if g == self.tgen => {
                    let e = self.sizes.entry(id).or_insert((0, false));
                    if !e.1 {
                        *e = (bytes, done);
                    }
                }
                Msg::Cleaned { label, freed } => {
                    cx.notify(format!("{label}: freed {}", human(freed)));
                    self.scan_targets();
                    self.refresh_drives();
                }
                Msg::Listing { g, rows } if g == self.fgen => {
                    if let Some(p) = self.pending_select.take() {
                        if let Some(i) = rows.iter().position(|r| r.path == p) {
                            self.sel[View::Folders.i()] = i;
                        }
                    }
                    self.frows = rows;
                }
                Msg::FSize { g, idx, bytes, done } if g == self.fgen => {
                    if let Some(r) = self.frows.get_mut(idx) {
                        if !r.done {
                            r.size = bytes;
                            r.done = done;
                        }
                    }
                }
                Msg::FDone { g, big } if g == self.fgen => {
                    self.fbig = big;
                    self.fdone = true;
                }
                Msg::FErr(g, e) if g == self.fgen => {
                    self.ferr = Some(e);
                    self.fdone = true;
                }
                Msg::Apps(a) => {
                    if let Err(e) = &a {
                        cx.notify(e.clone());
                    }
                    self.apps = Some(a);
                }
                Msg::Found { g, res } if g == self.sgen => {
                    self.searching = false;
                    self.sel[View::Install.i()] = TOP;
                    self.off[View::Install.i()] = 0;
                    self.found = Some(res);
                }
                _ => {} // from a scan that was replaced
            }
        }
    }

    // ------------------------------------------------------------------ lists

    fn size_of(&self, id: &str) -> (u64, bool) {
        self.sizes.get(id).copied().unwrap_or((0, false))
    }

    /// Indices into the view's own list, in display order.
    fn order(&self, v: View) -> Vec<usize> {
        match v {
            View::Cleanup => {
                let mut o: Vec<usize> = (0..self.targets.len()).collect();
                o.sort_by_key(|&i| (self.targets[i].kind != Kind::Clean, std::cmp::Reverse(self.size_of(self.targets[i].id).0)));
                o
            }
            View::Folders => {
                let mut o: Vec<usize> = (0..self.frows.len()).collect();
                o.sort_by_key(|&i| std::cmp::Reverse(self.frows[i].size));
                o.truncate(300);
                o
            }
            View::Apps => {
                let q = self.filter.trim().to_lowercase();
                match &self.apps {
                    Some(Ok(a)) => (0..a.len()).filter(|&i| q.is_empty() || format!("{} {}", a[i].name, a[i].publisher).to_lowercase().contains(&q)).collect(),
                    _ => vec![],
                }
            }
            View::Install => match &self.found {
                Some(Ok(f)) => (0..f.len()).collect(),
                _ => vec![],
            },
        }
    }

    /// The selected item's index in its list, if the list has it.
    fn selected(&self) -> Option<usize> {
        let o = self.order(self.view);
        let s = self.sel[self.view.i()];
        if o.contains(&s) { Some(s) } else { o.first().copied() }
    }

    fn step(&mut self, d: isize) {
        let o = self.order(self.view);
        if o.is_empty() {
            return;
        }
        let pos = o.iter().position(|&i| i == self.sel[self.view.i()]).unwrap_or(0) as isize;
        let np = (pos + d).clamp(0, o.len() as isize - 1) as usize;
        self.sel[self.view.i()] = o[np];
    }

    fn set_view(&mut self, v: View) {
        self.view = v;
        self.input = false;
        match v {
            View::Folders if !self.fstarted => self.scan_folder(self.froot.clone()),
            View::Apps if self.apps.is_none() => self.load_apps(),
            View::Install if self.found.is_none() && !self.searching => self.input = true,
            _ => {}
        }
    }

    fn rescan(&mut self) {
        match self.view {
            View::Cleanup => self.scan_targets(),
            View::Folders => self.scan_folder(self.froot.clone()),
            View::Apps => self.load_apps(),
            View::Install => self.search(),
        }
        self.refresh_drives();
    }

    fn open_files(&mut self, p: PathBuf, cx: &mut Cx) {
        cx.act(Action::Open(Box::new(crate::panes::files::Files::new(Some(p))), Place::Split));
    }

    /// Open a terminal pane running `cmd` so the user sees (and answers) every prompt.
    fn run_in_terminal(&mut self, title: &str, icon: &'static str, cmd: &Cmd, cx: &mut Cx) {
        #[cfg(test)]
        {
            let _ = (title, icon, &cx);
            self.launched.push(format!("term: {}", cmd.text()));
        }
        #[cfg(not(test))]
        {
            let prog = crate::config::which(&cmd.prog).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| cmd.prog.clone());
            let term = crate::panes::term::Term::new(title, icon, &prog, cmd.args.clone(), None);
            cx.act(Action::Open(Box::new(term), Place::Split));
        }
    }

    // ------------------------------------------------------------------ actions (each asks first)

    fn ask_clean(&mut self, cx: &mut Cx) {
        let Some(i) = self.selected().filter(|_| self.view == View::Cleanup) else { return };
        let tg = &self.targets[i];
        if tg.kind == Kind::Review {
            cx.notify(format!("{} is your data — press enter to review it in files", tg.label));
            return;
        }
        let (size, done) = self.size_of(tg.id);
        let size = if done { human(size) } else { format!("{}, still measuring", human(size)) };
        let mut lines = vec![tg.note.clone()];
        let question = match &tg.how {
            How::Term(c) => {
                lines.push(format!("runs `{}` in a terminal (asks for your password)", c.text()));
                format!("clean {} ({size})?", tg.label)
            }
            How::Pnpm => {
                lines.push("runs `pnpm store prune`".into());
                format!("prune the {} ({size})?", tg.label)
            }
            How::Recycle => format!("empty the recycle bin ({size})?"),
            _ => {
                lines.extend(tg.paths.iter().take(3).map(|p| p.display().to_string()));
                format!("empty {} ({size})?", tg.label)
            }
        };
        self.confirm = Some(Confirm { question, lines, what: Pending::Clean(tg.id) });
    }

    fn ask_uninstall(&mut self) {
        let Some(i) = self.selected() else { return };
        let Some(Ok(apps)) = &self.apps else { return };
        let a = &apps[i];
        let size = if a.size > 0 { human(a.size) } else { "size unknown".into() };
        self.confirm = Some(Confirm {
            question: format!("uninstall {} {} ({size})?", a.name, a.version),
            lines: vec![format!("runs `{}` in a terminal", a.uninstall.text())],
            what: Pending::Uninstall(i),
        });
    }

    fn ask_install(&mut self) {
        let Some(i) = self.selected() else { return };
        let Some(Ok(found)) = &self.found else { return };
        let f = &found[i];
        self.confirm = Some(Confirm {
            question: format!("install {} {}?", f.name, f.version),
            lines: vec![format!("runs `{}` in a terminal", f.install.text())],
            what: Pending::Install(i),
        });
    }

    fn execute(&mut self, what: Pending, cx: &mut Cx) {
        match what {
            Pending::Clean(id) => {
                let Some(tg) = self.targets.iter().find(|t| t.id == id).cloned() else { return };
                if let How::Term(c) = &tg.how {
                    self.run_in_terminal(&format!("clean {}", tg.label), "trash", c, cx);
                    cx.notify("press r when it's done to rescan");
                } else {
                    self.start_clean(tg, cx);
                }
            }
            Pending::Uninstall(i) => {
                let Some(Ok(apps)) = &self.apps else { return };
                let a = apps[i].clone();
                self.run_in_terminal(&format!("uninstall {}", a.name), "trash", &a.uninstall, cx);
                cx.notify(format!("started {}'s uninstaller — press r when done", a.name));
            }
            Pending::Install(i) => {
                let Some(Ok(found)) = &self.found else { return };
                let f = found[i].clone();
                self.run_in_terminal(&format!("install {}", f.name), "package", &f.install, cx);
            }
        }
    }

    /// Empty a confirmed "clean" row on a background thread.
    fn start_clean(&mut self, tg: Target, cx: &mut Cx) {
        cx.notify(format!("cleaning {}…", tg.label));
        #[cfg(test)]
        {
            self.launched.push(format!("clean: {}", tg.id));
        }
        #[cfg(not(test))]
        {
            let before = self.size_of(tg.id).0;
            let out = self.out.clone();
            std::thread::spawn(move || {
                let measure = |ps: &[PathBuf]| ps.iter().map(|p| scan::dir_size(p, &AtomicBool::new(false), None, &mut |_| {})).sum::<u64>();
                let hidden = |prog: &str, args: &[&str]| {
                    let mut c = std::process::Command::new(prog);
                    c.args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
                    #[cfg(windows)]
                    {
                        use std::os::windows::process::CommandExt;
                        c.creation_flags(0x0800_0000);
                    }
                    let _ = c.status();
                };
                let freed = match tg.how {
                    How::Recycle => {
                        hidden("powershell.exe", &["-NoProfile", "-Command", "Clear-RecycleBin -Force -ErrorAction SilentlyContinue"]);
                        before.saturating_sub(measure(&tg.paths))
                    }
                    How::Pnpm => {
                        let b = measure(&tg.paths);
                        if cfg!(windows) { hidden("cmd.exe", &["/c", "pnpm", "store", "prune"]) } else { hidden("pnpm", &["store", "prune"]) }
                        b.saturating_sub(measure(&tg.paths))
                    }
                    How::Dir => tg.paths.iter().map(|p| scan::clear_dir(p)).sum(),
                    How::Term(_) | How::Nothing => 0,
                };
                out.send(Msg::Cleaned { label: tg.label.clone(), freed }, true);
            });
        }
    }

    fn enter(&mut self, cx: &mut Cx) {
        match self.view {
            View::Cleanup => {
                if let Some(i) = self.selected() {
                    let tg = &self.targets[i];
                    match tg.open.clone().or_else(|| tg.paths.first().cloned()) {
                        Some(p) => self.open_files(p, cx),
                        None => cx.notify(format!("nothing to show for {}", tg.label)),
                    }
                }
            }
            View::Folders => {
                if let Some(i) = self.selected() {
                    let r = self.frows[i].clone();
                    if r.dir { self.scan_folder(r.path) } else { self.open_files(self.froot.clone(), cx) }
                }
            }
            View::Apps => self.ask_uninstall(),
            View::Install => self.ask_install(),
        }
    }

    fn up(&mut self) {
        if let Some(parent) = self.froot.parent().map(Path::to_path_buf) {
            let child = self.froot.clone();
            self.scan_folder(parent);
            self.pending_select = Some(child);
        }
    }
}

fn human(n: u64) -> String {
    let mut v = n as f64;
    for u in ["B", "KB", "MB", "GB", "TB"] {
        if v < 1024.0 || u == "TB" {
            return if u == "B" { format!("{v:.0} {u}") } else { format!("{v:.1} {u}") };
        }
        v /= 1024.0;
    }
    unreachable!()
}
