//! your AIs: every AI coding CLI you have (or could have) in one place — plan limits with reset countdowns,
//! local token usage with API-equivalent cost, one-key installs and sign-ins, and token-saving presets.
//!
//! Views (sidebar section, keys 1-4 / tab): overview · install · usage · token saver.
//!
//! Safety rules:
//!   * render never touches files or runs commands; detection, log parsing, HTTP and file edits all run on
//!     background threads and report back through a channel
//!   * installs and sign-ins run in a visible terminal pane, after a y/esc confirmation naming the command
//!   * config edits (Claude settings.json, Codex config.toml) show a diff and ask first, change only their own
//!     keys, keep a timestamped backup, and refuse if the file changed since the diff was shown
//!   * credential files are only checked for existence, never read; usage parsing keeps numbers only
//!   * under `cargo test` nothing is launched, and all paths point into a temp dir (util::Paths::under)

mod catalog;
mod draw;
mod limits;
mod ollama;
mod saver;
#[cfg(test)]
mod tests;
mod usage;
mod util;

#[cfg(not(test))]
use crate::pane::{Action, Place};
use crate::pane::{Cx, Waker};
use catalog::{CLIS, Found, Os};
use limits::Limits;
use ratatui::layout::Rect;
use saver::{Plan, Readouts};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};
use usage::{SrcSum, Stats};
use util::Paths;

/// `oriel usage-sink` — Claude Code's statusLine command (reads its JSON on stdin). Returns the exit code.
pub fn cli(args: &[String]) -> i32 {
    limits::cli(args)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum View {
    Overview,
    Install,
    Usage,
    Saver,
}

const VIEWS: [View; 4] = [View::Overview, View::Install, View::Usage, View::Saver];

impl View {
    fn i(self) -> usize {
        self as usize
    }
}

/// How often the usage logs are re-read while the app is open (incremental, so it's cheap).
const REFRESH: Duration = Duration::from_secs(30);

enum Msg {
    Found(usize, Found),
    Usage { sums: Vec<SrcSum>, stats: Stats, codex: Option<Limits>, claude: Option<Limits> },
    Ollama(ollama::Info),
    Readouts(Readouts),
    Plan(Result<saver::Connect, String>),
    Applied(Result<String, String>),
}

enum Ask {
    Install { cli: usize, cmd: String, ready: bool },
    Edit(Plan),
}

/// A message box (no action): e.g. how to chain an existing statusLine.
struct Note {
    title: String,
    lines: Vec<String>,
}

pub struct Ais {
    paths: Paths,
    os: Os,
    off: i64,
    view: View,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    waker: Option<Waker>,
    started: bool,
    /// run real detection (PATH, --version, Ollama) — off in tests
    live: bool,
    worker: Option<Sender<()>>,
    scanning: bool,
    last_scan: Option<Instant>,
    // data
    found: Vec<Option<Found>>,
    picks: Vec<Option<(String, bool)>>,
    usage: Option<Vec<SrcSum>>,
    stats: Option<Stats>,
    first_stats: Option<Stats>,
    claude_lim: Option<Limits>,
    codex_lim: Option<Limits>,
    ollama: Option<ollama::Info>,
    readouts: Option<Readouts>,
    // ui
    sel: [usize; 4],
    scroll: [usize; 4],
    usrc: usize,
    preset: usize,
    ask: Option<Ask>,
    ask_scroll: usize,
    note: Option<Note>,
    busy: bool,
    side_hits: Vec<(Rect, View)>,
    row_hits: Vec<(Rect, usize)>,
    #[cfg(test)]
    launched: Vec<String>,
}

impl Ais {
    pub fn new(cfg: &crate::config::Config) -> Self {
        Self::with(Paths::real(cfg), true)
    }

    fn with(paths: Paths, live: bool) -> Self {
        let (tx, rx) = channel();
        let os = Os::detect();
        let picks = CLIS.iter().map(|c| catalog::pick(c, os, &|t| crate::config::which(t).is_some())).collect();
        Ais {
            paths,
            os,
            off: util::local_offset(),
            view: View::Overview,
            tx,
            rx,
            waker: None,
            started: false,
            live,
            worker: None,
            scanning: false,
            last_scan: None,
            found: vec![None; CLIS.len()],
            picks,
            usage: None,
            stats: None,
            first_stats: None,
            claude_lim: None,
            codex_lim: None,
            ollama: None,
            readouts: None,
            sel: [0; 4],
            scroll: [0; 4],
            usrc: 0,
            preset: 1,
            ask: None,
            ask_scroll: 0,
            note: None,
            busy: false,
            side_hits: vec![],
            row_hits: vec![],
            #[cfg(test)]
            launched: vec![],
        }
    }

    // ------------------------------------------------------------------ background work

    fn send(tx: &Sender<Msg>, waker: &Option<Waker>, m: Msg) {
        if tx.send(m).is_ok() {
            if let Some(w) = waker {
                w.wake();
            }
        }
    }

    fn ensure(&mut self, cx: &Cx) {
        if self.started {
            return;
        }
        self.started = true;
        self.waker = Some(cx.waker());
        self.start_worker();
        self.rescan_usage();
        self.detect();
        self.load_readouts();
    }

    /// One long-lived thread owns the usage cache; each request brings it up to date and sends a summary.
    fn start_worker(&mut self) {
        let (req_tx, req_rx) = channel::<()>();
        let (paths, tx, waker, off) = (self.paths.clone(), self.tx.clone(), self.waker.clone(), self.off);
        std::thread::spawn(move || {
            let cache_file = paths.cache_file();
            let mut cache = usage::Cache::load(&cache_file);
            while req_rx.recv().is_ok() {
                while req_rx.try_recv().is_ok() {} // coalesce
                let stats = usage::refresh(&mut cache, &paths);
                let sums = usage::summarize(&cache, util::now(), off);
                let codex = usage::codex_limits(&cache).map(|(t, v)| limits::codex_from(t, &v));
                let claude = limits::read_claude(&paths);
                if stats.changed > 0 {
                    cache.save(&cache_file);
                }
                Ais::send(&tx, &waker, Msg::Usage { sums, stats, codex, claude });
            }
        });
        self.worker = Some(req_tx);
    }

    fn rescan_usage(&mut self) {
        if let Some(w) = &self.worker {
            if w.send(()).is_ok() {
                self.scanning = true;
                self.last_scan = Some(Instant::now());
            }
        }
    }

    fn detect(&mut self) {
        if !self.live {
            return;
        }
        for (i, c) in CLIS.iter().enumerate() {
            self.found[i] = None;
            let (paths, tx, waker) = (self.paths.clone(), self.tx.clone(), self.waker.clone());
            std::thread::spawn(move || Ais::send(&tx, &waker, Msg::Found(i, catalog::detect(c, &paths))));
        }
        let (base, tx, waker) = (self.paths.ollama.clone(), self.tx.clone(), self.waker.clone());
        std::thread::spawn(move || Ais::send(&tx, &waker, Msg::Ollama(ollama::probe(&base))));
    }

    fn load_readouts(&mut self) {
        let (paths, tx, waker) = (self.paths.clone(), self.tx.clone(), self.waker.clone());
        std::thread::spawn(move || Ais::send(&tx, &waker, Msg::Readouts(saver::readouts(&paths))));
    }

    fn drain(&mut self, cx: &mut Cx) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Found(i, f) => self.found[i] = Some(f),
                Msg::Usage { sums, stats, codex, claude } => {
                    self.scanning = false;
                    if self.first_stats.is_none() {
                        self.first_stats = Some(stats.clone());
                    }
                    self.usage = Some(sums);
                    self.stats = Some(stats);
                    self.codex_lim = codex;
                    self.claude_lim = claude;
                }
                Msg::Ollama(o) => self.ollama = Some(o),
                Msg::Readouts(r) => self.readouts = Some(r),
                Msg::Plan(r) => {
                    self.busy = false;
                    match r {
                        Ok(saver::Connect::Plan(p)) => {
                            if p.diff.iter().all(|d| d.0 == ' ') {
                                cx.notify("already set — nothing to change");
                            } else {
                                self.ask_scroll = 0;
                                self.ask = Some(Ask::Edit(p));
                            }
                        }
                        Ok(saver::Connect::Already) => cx.notify("limits are already connected (statusLine runs oriel usage-sink)"),
                        Ok(saver::Connect::Chain { current, suggestion }) => {
                            self.note = Some(Note {
                                title: "you already have a status line".into(),
                                lines: vec![
                                    "oriel won't replace it. Your statusLine command is:".into(),
                                    format!("  {current}"),
                                    String::new(),
                                    "To keep it and feed oriel too, change it (in ~/.claude/settings.json) to:".into(),
                                    format!("  {suggestion}"),
                                    String::new(),
                                    "usage-sink saves the limits, then runs your command with the same input and prints".into(),
                                    "its output, so your status line looks exactly as before.".into(),
                                ],
                            })
                        }
                        Err(e) => cx.notify(e),
                    }
                }
                Msg::Applied(r) => {
                    self.busy = false;
                    match r {
                        Ok(s) => cx.notify(s),
                        Err(e) => cx.notify(e),
                    }
                    self.load_readouts();
                }
            }
        }
    }

    // ------------------------------------------------------------------ lookups

    fn sum(&self, src: usage::Src) -> Option<&SrcSum> {
        self.usage.as_ref()?.iter().find(|s| s.src == src)
    }

    fn installed_count(&self) -> (usize, usize) {
        (self.found.iter().filter(|f| f.as_ref().is_some_and(|f| f.installed())).count(), self.found.iter().filter(|f| f.is_some()).count())
    }

    fn today_all(&self) -> usage::Tot {
        let mut t = usage::Tot::default();
        for s in self.usage.iter().flatten() {
            t.add(&s.today);
        }
        t
    }

    /// Sources with usage, for the usage view's switcher.
    fn usage_srcs(&self) -> Vec<usage::Src> {
        self.usage.iter().flatten().filter(|s| s.all.n > 0).map(|s| s.src).collect()
    }

    // ------------------------------------------------------------------ actions

    fn set_view(&mut self, v: View) {
        self.view = v;
        if v == View::Saver {
            self.load_readouts();
        }
    }

    fn step(&mut self, d: isize) {
        let n = match self.view {
            View::Install => CLIS.len(),
            View::Saver => saver::PRESETS.len(),
            View::Usage => self.usage_srcs().get(self.usrc).and_then(|s| self.sum(*s)).map(|s| s.days.len()).unwrap_or(0),
            View::Overview => 0,
        };
        if n == 0 {
            return;
        }
        let v = self.view.i();
        if self.view == View::Usage {
            self.scroll[v] = (self.scroll[v] as isize + d).clamp(0, n as isize - 1) as usize;
            return;
        }
        self.sel[v] = (self.sel[v] as isize + d).clamp(0, n as isize - 1) as usize;
        if self.view == View::Saver {
            self.preset = self.sel[v];
        }
    }

    fn ask_install(&mut self, cx: &mut Cx) {
        let i = self.sel[View::Install.i()];
        let c = &CLIS[i];
        let Some((cmd, ready)) = self.picks[i].clone() else {
            cx.notify(format!("{}: no installer for {} ({}) — o opens the docs", c.name, self.os.label(), if c.note.is_empty() { "not packaged" } else { c.note }));
            return;
        };
        self.ask_scroll = 0;
        self.ask = Some(Ask::Install { cli: i, cmd, ready });
    }

    fn sign_in(&mut self, cx: &mut Cx) {
        let i = self.sel[View::Install.i()];
        let c = &CLIS[i];
        let Some(login) = c.login else {
            cx.notify(format!("{} {} — see the docs (o)", c.name, if c.note.is_empty() { "has no sign-in" } else { c.note }));
            return;
        };
        let f = self.found[i].as_ref();
        if f.is_some_and(|f| !f.installed()) {
            cx.notify(format!("{} isn't installed yet — enter installs it", c.name));
            return;
        }
        // run the binary we found by its full path when it isn't on PATH (e.g. ~/.kimi-code/bin)
        let cmdline = match f.and_then(|f| f.path.clone()) {
            Some(p) if crate::config::which(c.bin).is_none() => {
                let rest = login.strip_prefix(c.bin).unwrap_or("");
                if cfg!(windows) { format!("& '{}'{rest}", p.display()) } else { format!("'{}'{rest}", p.display()) }
            }
            _ => login.to_string(),
        };
        self.run_in_terminal(&format!("sign in: {}", c.name), &cmdline, cx);
        cx.notify(format!("{}: follow the prompts in the new pane, then press r here to recheck", c.name));
    }

    fn run_in_terminal(&mut self, title: &str, cmdline: &str, cx: &mut Cx) {
        let (prog, args) = util::host_command(cmdline);
        #[cfg(test)]
        {
            let _ = (title, &cx, &prog, &args);
            self.launched.push(format!("term: {cmdline}"));
        }
        #[cfg(not(test))]
        {
            let term = crate::panes::term::Term::new(title, "package", &prog, args, None);
            cx.act(Action::Open(Box::new(term), Place::Split));
        }
    }

    fn open_url(&mut self, url: &str, cx: &mut Cx) {
        cx.notify(format!("opening {url}"));
        #[cfg(test)]
        {
            self.launched.push(format!("open: {url}"));
        }
        #[cfg(not(test))]
        {
            let url = url.to_string();
            std::thread::spawn(move || {
                #[cfg(windows)]
                let mut c = {
                    use std::os::windows::process::CommandExt;
                    let mut c = std::process::Command::new("rundll32.exe");
                    c.args(["url.dll,FileProtocolHandler", &url]).creation_flags(0x0800_0000);
                    c
                };
                #[cfg(not(windows))]
                let mut c = {
                    let mut c = std::process::Command::new("xdg-open");
                    c.arg(&url);
                    c
                };
                c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
                let _ = c.status();
            });
        }
    }

    /// Build a config edit on a background thread; the diff comes back as Msg::Plan.
    fn plan(&mut self, what: &'static str, cx: &mut Cx) {
        if self.busy {
            return;
        }
        self.busy = true;
        let (paths, tx, waker, preset) = (self.paths.clone(), self.tx.clone(), self.waker.clone(), self.preset);
        let _ = cx;
        std::thread::spawn(move || {
            let p = &saver::PRESETS[preset];
            let r = match what {
                "claude" => saver::plan_claude(&paths, p).map(saver::Connect::Plan),
                "codex" => saver::plan_codex(&paths, p).map(saver::Connect::Plan),
                _ => saver::plan_connect(&paths),
            };
            Ais::send(&tx, &waker, Msg::Plan(r));
        });
    }

    fn confirm_yes(&mut self, cx: &mut Cx) {
        match self.ask.take() {
            Some(Ask::Install { cli, cmd, .. }) => {
                let c = &CLIS[cli];
                self.run_in_terminal(&format!("install {}", c.name), &cmd, cx);
                cx.notify(format!("installing {} — press r here when it's done to recheck", c.name));
            }
            Some(Ask::Edit(plan)) => {
                self.busy = true;
                let (tx, waker) = (self.tx.clone(), self.waker.clone());
                std::thread::spawn(move || {
                    let r = saver::apply(&plan, &saver::backup_stamp()).map(|b| match b {
                        Some(b) => format!("{} · backup: {}", plan.done, b.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
                        None => plan.done.clone(),
                    });
                    Ais::send(&tx, &waker, Msg::Applied(r));
                });
            }
            None => {}
        }
    }

    fn refresh_all(&mut self, cx: &mut Cx) {
        self.rescan_usage();
        self.detect();
        self.load_readouts();
        cx.notify("rechecking your AIs…");
    }
}
