//! A real terminal inside a pane: a shell (or any command, e.g. `claude`) on a pseudo-terminal
//! (ConPTY on Windows, a Unix pty elsewhere), parsed by vt100 and drawn cell by cell.

use crate::pane::{Activity, Cx, Pane, Waker};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::sync::{Arc, Mutex};

pub struct Term {
    title: String,
    icon: &'static str,
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    /// The program until it starts running (first render): then a thread waits on it.
    child: Option<Box<dyn Child + Send + Sync>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    exited: Arc<AtomicBool>,
    size: (u16, u16), // rows, cols
    scroll: usize,    // lines scrolled back (0 = live)
    started: bool,
    /// ms since `born` when the program last printed something (set by the reader thread)
    last_output: Arc<AtomicU64>,
    born: Instant,
    /// Some once this pane is known to run a coding agent
    status: Option<Activity>,
    agent: bool,
    last_scan: Instant,
    /// While it's waiting on you: the bottom of its screen (the question and its choices), for the alerts app.
    prompt: Vec<String>,
    /// Tell the event center how this ended ("installing Firefox" -> "Firefox installed" / "failed").
    exit_alert: Option<String>,
    /// (program, why) when the requested program didn't start: the pane runs the default shell instead, under a
    /// red banner.
    failed: Option<(String, String)>,
    /// The program's exit code once it has ended (0 = success), set by the thread that waits on it.
    exit: Arc<Mutex<Option<u32>>>,
    /// What was asked for, to run it again (r on a pane whose program failed).
    spec: Spec,
    /// The failure has been reported (alert) and the pane is being kept so its output can be read.
    announced: bool,
    /// Keep the pane when the program fails. Not for your own shell: `exit` after a failed command (bash hands
    /// back its status) is you closing it, not an error to read.
    keep_failed: bool,
    /// The folder it started in, and the one the shell last said it's in (OSC 7 / Windows Terminal's OSC 9;9,
    /// sent by prompts like starship and oh-my-posh), set by the reader thread.
    dir: Option<std::path::PathBuf>,
    osc_dir: Arc<Mutex<Option<std::path::PathBuf>>>,
    /// What it comes back as when oriel restarts: "terminal" for a shell, "claude" / "codex"; None for the rest
    /// (installs, an orchestrator's workers).
    reopen: Option<&'static str>,
}

/// How a Term was started: enough to start it again.
#[derive(Clone)]
struct Spec {
    title: String,
    prog: String,
    args: Vec<String>,
    cwd: Option<std::path::PathBuf>,
    alert: Option<String>,
}

/// Programs that are coding agents (by executable name), so their panes get status dots from the start.
const AGENTS: &[&str] = &["claude", "codex", "opencode", "kimi", "aider", "cursor-agent", "copilot", "droid", "amp", "goose"];

impl Term {
    /// `prog` + `args` on a pty, started lazily at the first render (when the pane size is known).
    pub fn new(title: &str, icon: &'static str, prog: &str, args: Vec<String>, cwd: Option<std::path::PathBuf>) -> Term {
        let pty = native_pty_system();
        let pair = pty.openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }).expect("openpty");
        let mut cmd = CommandBuilder::new(prog);
        cmd.args(&args);
        let dir = cwd.clone().or_else(|| std::env::current_dir().ok());
        if let Some(d) = &dir {
            cmd.cwd(d);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env_remove("NO_COLOR"); // Claude Code sets it; shells in panes should have colour
        for k in crate::panes::chat::providers::CLAUDE_SESSION_ENV {
            cmd.env_remove(k);
        }
        cmd.env("ORIEL", "1");
        // oriel started by its full path: `oriel` still works in its own terminals
        if let Some(path) = std::env::current_exe().ok().and_then(|e| with_dir_on_path(e.parent()?, cmd.get_env("PATH"))) {
            cmd.env("PATH", path);
        }
        let (child, failed) = match pair.slave.spawn_command(cmd) {
            Ok(c) => (c, None),
            Err(e) => {
                // fall back to the default shell so the pane still works; the banner and the title say what failed
                // (never eprintln: the UI owns the screen, the text would land on top of it and vanish)
                let (sh, a) = crate::config::default_shell(&crate::config::Config::default());
                let mut c = CommandBuilder::new(sh);
                c.args(a);
                if let Some(d) = &dir {
                    c.cwd(d);
                }
                // Windows says `CreateProcessW "<prog>\0" failed: <reason> (os error 2)`: the reason is the useful part
                let e = e.to_string();
                let why = e.rsplit_once("failed: ").map_or(e.as_str(), |(_, why)| why);
                let why = why.split(" (os error").next().unwrap_or(why).trim().trim_end_matches('.').to_string();
                (pair.slave.spawn_command(c).expect("spawn shell"), Some((prog.to_string(), why)))
            }
        };
        drop(pair.slave);
        let writer = pair.master.take_writer().expect("pty writer");
        let killer = child.clone_killer();
        Term {
            title: if failed.is_some() { format!("{title} · failed to start") } else { title.to_string() },
            icon,
            parser: Arc::new(Mutex::new(vt100::Parser::new(24, 80, 10_000))),
            writer: Arc::new(Mutex::new(writer)),
            master: pair.master,
            child: Some(child),
            killer,
            exited: Arc::new(AtomicBool::new(false)),
            size: (24, 80),
            scroll: 0,
            started: false,
            last_output: Arc::new(AtomicU64::new(0)),
            born: Instant::now(),
            status: None,
            agent: {
                let words = std::iter::once(prog).chain(args.iter().map(String::as_str));
                words.map(|w| std::path::Path::new(w).file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default()).any(|n| AGENTS.contains(&n.as_str()))
            },
            last_scan: Instant::now(),
            prompt: vec![],
            exit_alert: None,
            failed,
            exit: Arc::new(Mutex::new(None)),
            spec: Spec { title: title.to_string(), prog: prog.to_string(), args, cwd, alert: None },
            announced: false,
            keep_failed: true,
            dir,
            osc_dir: Arc::default(),
            reopen: None,
        }
    }

    /// The exit code, once the program has ended with an error: the pane stays open so you can read why.
    fn failed_code(&self) -> Option<u32> {
        if !self.keep_failed || !self.exited.load(Ordering::SeqCst) {
            return None;
        }
        self.exit.lock().unwrap().filter(|&c| c != 0)
    }

    /// Start the same program again in this pane (r on a failed one).
    fn rerun(&mut self) {
        let s = self.spec.clone();
        let mut t = Term::new(&s.title, self.icon, &s.prog, s.args, s.cwd);
        t.reopen = self.reopen;
        t.keep_failed = self.keep_failed;
        if let Some(what) = s.alert {
            t = t.alert_on_exit(&what);
        }
        *self = t; // the old one has ended: dropping it kills nothing
    }

    /// Move the scrollback view to `lines` back (clamped to what exists).
    fn scroll_to(&mut self, lines: usize) {
        let mut p = self.parser.lock().unwrap();
        p.screen_mut().set_scrollback(lines);
        self.scroll = p.screen().scrollback();
    }

    /// Rows the failed-to-start banner takes at the top of the pane.
    fn banner_h(&self) -> u16 {
        self.failed.is_some() as u16
    }

    /// Brought back as `name` (a panes::open name) when oriel restarts where you left off.
    pub fn reopen_as(mut self, name: &'static str) -> Term {
        self.reopen = Some(name);
        self
    }

    /// When the command ends, say so in the event center: `what` is e.g. "Firefox".
    pub fn alert_on_exit(mut self, what: &str) -> Term {
        self.exit_alert = Some(what.to_string());
        self.spec.alert = Some(what.to_string());
        self
    }

    pub fn shell(cfg: &crate::config::Config, cwd: Option<std::path::PathBuf>) -> Term {
        let (prog, args) = crate::config::default_shell(cfg);
        let name = std::path::Path::new(&prog).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or(prog.clone());
        let mut t = Term::new(&name, "term", &prog, args, cwd).reopen_as("terminal");
        t.keep_failed = false;
        t
    }

    fn start_reader(&mut self, waker: Waker) {
        let mut reader = self.master.try_clone_reader().expect("pty reader");
        let waiter_waker = Waker { id: waker.id, tx: waker.tx.clone() };
        let parser = self.parser.clone();
        let writer = self.writer.clone();
        let (last_output, born) = (self.last_output.clone(), self.born);
        let osc_dir = self.osc_dir.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 16384];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let data = &buf[..n];
                        let mut p = parser.lock().unwrap();
                        p.process(data);
                        // Device Status Report: ConPTY (and some programs) ask where the cursor is and wait for
                        // the answer. A real terminal replies; so must we.
                        if contains(data, b"\x1b[6n") {
                            let (r, c) = p.screen().cursor_position();
                            let _ = writer.lock().unwrap().write_all(format!("\x1b[{};{}R", r + 1, c + 1).as_bytes());
                        }
                        drop(p);
                        if let Some(d) = osc_cwd(data) {
                            *osc_dir.lock().unwrap() = Some(d);
                        }
                        last_output.store(born.elapsed().as_millis() as u64, Ordering::Relaxed);
                        waker.wake();
                    }
                }
            }
            // the end of the output isn't the end of the program: the thread below says when it has ended (with
            // its exit code, which decides whether the pane closes)
            waker.wake();
        });
        // ConPTY keeps the output open after the program ends (the reader above never sees EOF on Windows), so a
        // second thread waits on the process itself: the pane closes and the exit alert fires, with no polling
        if let Some(mut child) = self.child.take() {
            let (exit, exited) = (self.exit.clone(), self.exited.clone());
            std::thread::spawn(move || {
                if let Ok(s) = child.wait() {
                    *exit.lock().unwrap() = Some(if s.success() { 0 } else { s.exit_code().max(1) });
                }
                exited.store(true, Ordering::SeqCst);
                waiter_waker.wake();
            });
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.scroll = 0;
        let mut w = self.writer.lock().unwrap();
        let _ = w.write_all(bytes);
        let _ = w.flush();
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        if (rows, cols) == self.size || rows == 0 || cols == 0 {
            return;
        }
        self.size = (rows, cols);
        // the screen first, under one lock (so the reader can't slip output in between), then tell the program
        set_size_keep_bottom(&mut self.parser.lock().unwrap(), rows, cols);
        let _ = self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
    }
}

/// Resize the screen the way real terminals do: when it gets shorter than the cursor row, the top rows scroll
/// into scrollback so the prompt and the newest output stay on screen (vt100 alone cuts rows off the bottom,
/// and they'd be lost). The alternate screen just truncates: full-screen programs redraw on resize. A program's
/// own scroll region keeps vt100's behaviour (it never feeds scrollback).
fn set_size_keep_bottom(p: &mut vt100::Parser, rows: u16, cols: u16) {
    let s = p.screen();
    let (old_rows, _) = s.size();
    let (r, c) = s.cursor_position();
    if rows < old_rows && r >= rows && !s.alternate_screen() {
        let n = r + 1 - rows;
        // CSI n S scrolls the whole screen up n rows (into scrollback); then put the cursor back on its line
        p.process(format!("\x1b[{n}S\x1b[{};{}H", r + 1 - n, c + 1).as_bytes());
    }
    p.screen_mut().set_size(rows, cols);
}

impl Term {
    /// Read the bottom of the screen for an agent's tell-tale lines: "esc to interrupt" while it works,
    /// a permission prompt or question when it's waiting on you.
    fn scan(&mut self) {
        let tail: Vec<String> = {
            let p = self.parser.lock().unwrap();
            let s = p.screen();
            let (_, cols) = s.size();
            let rows: Vec<String> = s.rows(0, cols).collect();
            let mut tail: Vec<String> = rows.into_iter().rev().filter(|r| !r.trim().is_empty()).take(14).collect();
            tail.reverse();
            tail
        };
        let text = tail.iter().map(|r| r.to_lowercase()).collect::<Vec<_>>().join("\n");
        let working = ["esc to interrupt", "esc to cancel", "ctrl+c to interrupt", "ctrl-c to interrupt"].iter().any(|m| text.contains(m));
        let blocked = [
            "do you want to",
            "do you want me to",
            "allow this",
            "allow once",
            "approve this",
            "(y/n)",
            "[y/n]",
            "❯ 1. yes",
            "› 1. yes",
            "press enter to confirm",
            "waiting for your approval",
        ]
        .iter()
        .any(|m| text.contains(m));
        // an agent's idle prompt (Claude Code's footer, Codex's key line): so shift+enter is a new line from the start
        let idle = ["? for shortcuts", "⏎ send"].iter().any(|m| text.contains(m));
        if working || blocked || idle {
            self.agent = true;
        }
        if !self.agent {
            return;
        }
        // what it's asking, as the screen shows it (a prompt box's borders trimmed off)
        let unboxed = |r: &String| r.trim_end().trim_matches(|c| matches!(c, '│' | '╭' | '╮' | '╰' | '╯' | '─' | ' ')).to_string();
        self.prompt = if blocked { tail.iter().map(unboxed).filter(|r| !r.is_empty()).collect() } else { vec![] };
        let quiet_ms = (self.born.elapsed().as_millis() as u64).saturating_sub(self.last_output.load(Ordering::Relaxed));
        self.status = Some(if blocked {
            Activity::Blocked
        } else if working || quiet_ms < 1500 {
            Activity::Working
        } else {
            Activity::Idle
        });
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// The last folder a shell reported in this output: OSC 7 (`ESC ] 7 ; file://host/path BEL`, percent-encoded)
/// or Windows Terminal's OSC 9;9 (`ESC ] 9 ; 9 ; "C:\path" BEL`), ended by BEL or ESC \. Only folders that exist.
fn osc_cwd(data: &[u8]) -> Option<std::path::PathBuf> {
    let mut found = None;
    let mut i = 0;
    while let Some(at) = data[i..].windows(2).position(|w| w == b"\x1b]").map(|p| p + i) {
        let body = &data[at + 2..];
        let end = body.iter().position(|&b| b == 0x07 || b == 0x1b).unwrap_or(body.len());
        let s = String::from_utf8_lossy(&body[..end]);
        let path = if let Some(url) = s.strip_prefix("7;") {
            // file://host/path: drop the host; /C:/x on Windows is C:/x
            let rest = url.strip_prefix("file://").unwrap_or(url);
            let p = &rest[rest.find('/').unwrap_or(rest.len())..];
            let p = percent_decode(p);
            let bytes = p.as_bytes();
            if cfg!(windows) && bytes.len() >= 3 && bytes[0] == b'/' && bytes[2] == b':' { Some(p[1..].to_string()) } else { Some(p) }
        } else {
            s.strip_prefix("9;9;").map(|p| p.trim_matches('"').to_string())
        };
        if let Some(p) = path.filter(|p| !p.is_empty()).map(std::path::PathBuf::from).filter(|p| p.is_dir()) {
            found = Some(p);
        }
        i = at + 2 + end;
    }
    found
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        // only while it runs: once it has ended the pid could belong to something else (the unix killer is a bare pid)
        if self.exit.lock().unwrap().is_none() {
            let _ = self.killer.kill();
        }
    }
}

impl Pane for Term {
    fn title(&self) -> String {
        let t = match self.failed_code() {
            Some(c) => format!("{} · exited with {}", self.title, code_text(c)),
            None => self.title.clone(),
        };
        if self.scroll > 0 { format!("{t} · scrolled {}", self.scroll) } else { t }
    }
    fn icon(&self) -> &'static str {
        self.icon
    }
    fn is_terminal(&self) -> bool {
        true
    }
    fn cwd(&self) -> Option<std::path::PathBuf> {
        self.osc_dir.lock().ok().and_then(|d| d.clone()).or_else(|| self.dir.clone())
    }
    fn reopen(&self) -> Option<&'static str> {
        self.reopen
    }
    fn exit_note(&mut self) -> Option<(crate::alerts::Kind, String)> {
        let what = self.exit_alert.take()?;
        if let Some((prog, e)) = &self.failed {
            // the shell that stood in for it exiting says nothing about the program: it never ran
            return Some((crate::alerts::Kind::BuildFailed, format!("{what}: couldn't start {prog} ({e})")));
        }
        // the waiting thread sets the exit code before it says the program ended
        Some(match *self.exit.lock().unwrap() {
            Some(0) => (crate::alerts::Kind::Download, format!("{what}: done")),
            Some(c) => (crate::alerts::Kind::BuildFailed, format!("{what}: failed with exit code {} · its output is still in the pane", code_text(c))),
            None => (crate::alerts::Kind::Download, format!("{what}: ended (couldn't tell whether it worked)")),
        })
    }

    /// A program that ended cleanly closes its pane; one that failed keeps it (dimmed, with a footer) so the error
    /// can be read. Enter or esc closes it then, r runs it again.
    fn alive(&self) -> bool {
        !self.exited.load(Ordering::SeqCst) || self.failed_code().is_some()
    }
    fn activity(&self) -> Option<Activity> {
        if self.failed_code().is_some() { None } else { self.status }
    }
    fn open_now(&self) -> Vec<crate::alerts::Open> {
        if self.status != Some(Activity::Blocked) {
            return vec![];
        }
        // seen from the alerts app, answered here: the keys are Claude Code's or Codex's, not ours to guess at
        let mut o = crate::alerts::Open::new(crate::alerts::Kind::NeedsYou, "term", format!("{} is waiting for you", self.title));
        o.detail = self.prompt.clone();
        vec![o]
    }
    fn wants_mouse(&self) -> bool {
        self.parser.lock().map(|p| p.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None).unwrap_or(false)
    }
    fn wants_fkeys(&self) -> bool {
        // full-screen programs (htop, mc, nano, vim) use the alternate screen or the mouse; a shell prompt doesn't.
        // Coding agents (opencode is full-screen too) have no use for F-keys, and switching apps from them matters.
        !self.agent && self.parser.lock().map(|p| p.screen().alternate_screen() || p.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None).unwrap_or(false)
    }
    fn tick_every(&self) -> Option<Duration> {
        // agents get re-checked so "working" turns into "idle/done" when they go quiet
        if self.agent && self.failed_code().is_none() { Some(Duration::from_millis(400)) } else { None }
    }
    fn poll(&mut self, cx: &mut Cx) {
        if let Some(c) = self.failed_code() {
            // it ended with an error: say so once (the pane stays, so the output can be read)
            if !self.announced {
                self.announced = true;
                if let Some(note) = self.exit_note() {
                    cx.alert(note.0, note.1);
                } else if self.agent {
                    cx.alert(crate::alerts::Kind::BuildFailed, format!("{} exited with {}", self.title, code_text(c)));
                }
            }
            return;
        }
        if self.last_scan.elapsed() < Duration::from_millis(200) {
            return;
        }
        self.last_scan = Instant::now();
        self.scan();
    }

    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        if !self.started {
            self.started = true;
            self.start_reader(cx.waker());
        }
        let area = match &self.failed {
            Some((prog, e)) if area.height > 1 => {
                // the reason first: a narrow pane cuts the end of the line
                let line = format!(" oriel: couldn't start {prog}: {e} · this is a plain shell");
                let st = Style::default().fg(cx.theme.danger).add_modifier(Modifier::BOLD);
                f.render_widget(ratatui::widgets::Paragraph::new(ratatui::text::Span::styled(crate::ui::fit(&line, area.width as usize), st)), Rect { height: 1, ..area });
                Rect { y: area.y + 1, height: area.height - 1, ..area }
            }
            _ => area,
        };
        let failed = self.failed_code();
        let area = match failed {
            Some(c) if area.height > 1 => {
                // the footer takes the last row; the screen shrinks (its top scrolls back), so the error stays in view
                let line = format!(" exited with {} · enter or esc closes · r reruns", code_text(c));
                let st = Style::default().fg(cx.theme.danger).add_modifier(Modifier::BOLD);
                let r = Rect { y: area.bottom() - 1, height: 1, ..area };
                f.render_widget(ratatui::widgets::Paragraph::new(ratatui::text::Span::styled(crate::ui::fit(&line, area.width as usize), st)), r);
                Rect { height: area.height - 1, ..area }
            }
            _ => area,
        };
        self.resize(area.height, area.width);
        let mut p = self.parser.lock().unwrap();
        p.screen_mut().set_scrollback(self.scroll);
        self.scroll = p.screen().scrollback(); // clamped to what exists
        let screen = p.screen();
        let buf = f.buffer_mut();
        for row in 0..area.height {
            for col in 0..area.width {
                let Some(cell) = screen.cell(row, col) else { continue };
                if cell.is_wide_continuation() {
                    continue;
                }
                let mut style = Style::default().fg(color(cell.fgcolor())).bg(color(cell.bgcolor()));
                let mut m = Modifier::empty();
                if cell.bold() {
                    m |= Modifier::BOLD;
                }
                if cell.dim() {
                    m |= Modifier::DIM;
                }
                if cell.italic() {
                    m |= Modifier::ITALIC;
                }
                if cell.underline() {
                    m |= Modifier::UNDERLINED;
                }
                if cell.inverse() {
                    m |= Modifier::REVERSED;
                }
                if failed.is_some() {
                    m |= Modifier::DIM; // it has ended: the output is there to read, not to type into
                }
                style = style.add_modifier(m);
                let s = cell.contents();
                if let Some(bc) = buf.cell_mut(Position { x: area.x + col, y: area.y + row }) {
                    bc.set_symbol(if s.is_empty() { " " } else { s });
                    bc.set_style(style);
                }
            }
        }
        if cx.focused && !screen.hide_cursor() && self.scroll == 0 && failed.is_none() {
            let (r, c) = screen.cursor_position();
            if r < area.height && c < area.width {
                f.set_cursor_position(Position { x: area.x + c, y: area.y + r });
            }
        }
    }

    fn key(&mut self, key: KeyEvent, cx: &mut Cx) -> bool {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT) && !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        // scrollback from the keyboard (xterm's keys): never sent to the program
        let page = (self.size.0 as usize).saturating_sub(1).max(1);
        let back = match key.code {
            KeyCode::PageUp if shift => Some(self.scroll + page),
            KeyCode::PageDown if shift => Some(self.scroll.saturating_sub(page)),
            KeyCode::Home if shift => Some(usize::MAX),
            KeyCode::End if shift => Some(0),
            _ => None,
        };
        if let Some(n) = back {
            self.scroll_to(n);
            return true;
        }
        if self.failed_code().is_some() {
            // the program has ended: enter/esc close the pane, r starts it again
            match key.code {
                KeyCode::Enter | KeyCode::Esc => cx.act(crate::pane::Action::Close),
                KeyCode::Char('r') | KeyCode::Char('R') => self.rerun(),
                _ => {}
            }
            return true;
        }
        if self.agent && newline_key(&key) {
            // shift+enter in Claude Code / Codex: a new line, not "send" (they read ESC CR as alt+enter)
            self.send(b"\x1b\r");
            return true;
        }
        let app_cursor = self.parser.lock().unwrap().screen().application_cursor();
        if let Some(bytes) = encode_key(key, app_cursor) {
            self.send(&bytes);
        }
        true
    }

    fn paste(&mut self, text: &str, _cx: &mut Cx) {
        let bracketed = self.parser.lock().unwrap().screen().bracketed_paste();
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        if bracketed {
            self.send(format!("\x1b[200~{text}\x1b[201~").as_bytes());
        } else {
            self.send(text.as_bytes());
        }
    }

    fn mouse(&mut self, ev: MouseEvent, area: Rect, _cx: &mut Cx) {
        let (mode, enc, alt_screen, app_cursor) = {
            let p = self.parser.lock().unwrap();
            let s = p.screen();
            (s.mouse_protocol_mode(), s.mouse_protocol_encoding(), s.alternate_screen(), s.application_cursor())
        };
        let col = ev.column.saturating_sub(area.x) + 1;
        let row = ev.row.saturating_sub(area.y + self.banner_h()) + 1;
        if mode == vt100::MouseProtocolMode::None {
            let up = match ev.kind {
                MouseEventKind::ScrollUp => true,
                MouseEventKind::ScrollDown => false,
                _ => return,
            };
            if alt_screen && self.failed_code().is_none() {
                // a full-screen program without mouse mode (less, man, git's pager): the alternate screen has no
                // scrollback, so the wheel sends arrow keys, like xterm's "alternate scroll"
                let arrow: &[u8] = match (up, app_cursor) {
                    (true, true) => b"\x1bOA",
                    (true, false) => b"\x1b[A",
                    (false, true) => b"\x1bOB",
                    (false, false) => b"\x1b[B",
                };
                self.send(&arrow.repeat(3));
            } else if up {
                // the program doesn't want the mouse: the wheel scrolls our scrollback
                self.scroll_to(self.scroll + 3);
            } else {
                self.scroll_to(self.scroll.saturating_sub(3));
            }
            return;
        }
        let (code, release) = match ev.kind {
            MouseEventKind::Down(b) => (btn(b), false),
            MouseEventKind::Up(b) => (btn(b), true),
            MouseEventKind::Drag(b) if mode != vt100::MouseProtocolMode::Press => (btn(b) + 32, false),
            MouseEventKind::Moved if mode == vt100::MouseProtocolMode::AnyMotion => (35, false),
            MouseEventKind::ScrollUp => (64, false),
            MouseEventKind::ScrollDown => (65, false),
            _ => return,
        };
        let bytes = if enc == vt100::MouseProtocolEncoding::Sgr {
            format!("\x1b[<{};{};{}{}", code, col, row, if release { 'm' } else { 'M' }).into_bytes()
        } else {
            let c = if release { 3 } else { code };
            vec![0x1b, b'[', b'M', 32 + c as u8, (32 + col.min(223)) as u8, (32 + row.min(223)) as u8]
        };
        self.send(&bytes);
    }
}

/// Shift+enter / ctrl+enter: the keys coding agents take for a new line in their prompt.
fn newline_key(k: &KeyEvent) -> bool {
    k.code == KeyCode::Enter && k.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL) && !k.modifiers.contains(KeyModifiers::ALT)
}

/// An exit code for people: Windows crash codes (0xC0000005...) read better in hex.
fn code_text(c: u32) -> String {
    if c > 0xFFFF { format!("0x{c:08X}") } else { c.to_string() }
}

/// PATH with `dir` in front, unless it's already on it (None then, or when it can't be joined).
fn with_dir_on_path(dir: &std::path::Path, path: Option<&std::ffi::OsStr>) -> Option<std::ffi::OsString> {
    let norm = |p: &std::path::Path| {
        let s = p.to_string_lossy().trim_end_matches(['/', '\\']).to_string();
        if cfg!(windows) { s.to_lowercase().replace('/', "\\") } else { s }
    };
    let parts: Vec<std::path::PathBuf> = path.map(|p| std::env::split_paths(p).collect()).unwrap_or_default();
    if parts.iter().any(|p| norm(p) == norm(dir)) {
        return None;
    }
    std::env::join_paths(std::iter::once(dir.to_path_buf()).chain(parts)).ok()
}

fn btn(b: MouseButton) -> u16 {
    match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// Key -> the bytes an xterm would send.
pub fn encode_key(key: KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    // a typed char, AltGr ones included (ctrl+alt+'@' on Windows is AltGr+2): the char itself, no ctrl/ESC
    if let Some(c) = crate::ui::typed_char(&key) {
        return Some(c.to_string().into_bytes());
    }
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    // xterm modifier parameter: 1 + shift(1) + alt(2) + ctrl(4)
    let m = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
    let csi = |final_: &str, n: Option<u8>| -> Vec<u8> {
        match (n, m) {
            (None, 1) => format!("\x1b[{final_}").into_bytes(),
            (None, m) => format!("\x1b[1;{m}{final_}").into_bytes(),
            (Some(n), 1) => format!("\x1b[{n}~").into_bytes(),
            (Some(n), m) => format!("\x1b[{n};{m}~").into_bytes(),
        }
    };
    let arrow = |c: char| -> Vec<u8> {
        if m == 1 {
            if app_cursor { format!("\x1bO{c}").into_bytes() } else { format!("\x1b[{c}").into_bytes() }
        } else {
            format!("\x1b[1;{m}{c}").into_bytes()
        }
    };
    let mut out = match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let c = c.to_ascii_lowercase();
                match c {
                    'a'..='z' => vec![c as u8 - b'a' + 1],
                    ' ' | '@' | '2' => vec![0],
                    '[' | '3' => vec![27],
                    '\\' | '4' => vec![28],
                    ']' | '5' => vec![29],
                    '^' | '6' => vec![30],
                    '_' | '/' | '7' => vec![31],
                    '8' => vec![127],
                    _ => c.to_string().into_bytes(),
                }
            } else {
                c.to_string().into_bytes()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => {
            if ctrl { vec![8] } else { vec![127] }
        }
        KeyCode::Esc => vec![27],
        KeyCode::Up => arrow('A'),
        KeyCode::Down => arrow('B'),
        KeyCode::Right => arrow('C'),
        KeyCode::Left => arrow('D'),
        KeyCode::Home => arrow('H'),
        KeyCode::End => arrow('F'),
        KeyCode::Insert => csi("", Some(2)),
        KeyCode::Delete => csi("", Some(3)),
        KeyCode::PageUp => csi("", Some(5)),
        KeyCode::PageDown => csi("", Some(6)),
        KeyCode::F(n) => match n {
            1..=4 => {
                let c = [b'P', b'Q', b'R', b'S'][n as usize - 1] as char;
                if m == 1 { format!("\x1bO{c}").into_bytes() } else { format!("\x1b[1;{m}{c}").into_bytes() }
            }
            5 => csi("", Some(15)),
            6 => csi("", Some(17)),
            7 => csi("", Some(18)),
            8 => csi("", Some(19)),
            9 => csi("", Some(20)),
            10 => csi("", Some(21)),
            11 => csi("", Some(23)),
            12 => csi("", Some(24)),
            _ => return None,
        },
        _ => return None,
    };
    // Alt+char / Alt+Enter etc: ESC prefix (arrows and function keys already carry the modifier)
    if alt && matches!(key.code, KeyCode::Char(_) | KeyCode::Enter | KeyCode::Backspace | KeyCode::Tab | KeyCode::Esc) {
        out.insert(0, 27);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char, m: KeyModifiers) -> Vec<u8> {
        encode_key(KeyEvent::new(KeyCode::Char(c), m), false).unwrap()
    }

    #[test]
    fn term_altgr_chars_reach_the_shell() {
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        // AltGr on Windows arrives as ctrl+alt+char: the shell gets the char, not ESC NUL / ESC '{'
        assert_eq!(key('@', altgr), b"@");
        assert_eq!(key('{', altgr), b"{");
        assert_eq!(key('\\', altgr), b"\\");
        assert_eq!(key('€', altgr), "€".as_bytes());
        // real shortcuts are unchanged
        assert_eq!(key('a', KeyModifiers::CONTROL), [1]);
        assert_eq!(key(' ', KeyModifiers::CONTROL), [0]);
        assert_eq!(key('b', KeyModifiers::ALT), [27, b'b']);
        assert_eq!(key('c', altgr), [27, 3]);
        assert_eq!(key('A', KeyModifiers::SHIFT), b"A");
    }

    fn row(p: &vt100::Parser, r: u16) -> String {
        let (_, cols) = p.screen().size();
        p.screen().rows(0, cols).nth(r as usize).unwrap_or_default().trim_end().to_string()
    }

    fn scrollback_rows(p: &mut vt100::Parser) -> usize {
        p.screen_mut().set_scrollback(usize::MAX);
        let n = p.screen().scrollback();
        p.screen_mut().set_scrollback(0);
        n
    }

    #[test]
    fn term_shrinking_keeps_the_prompt_and_feeds_scrollback() {
        let mut p = vt100::Parser::new(40, 80, 1000);
        for i in 0..39 {
            p.process(format!("line {i}\r\n").as_bytes());
        }
        p.process(b"PS> ");
        assert_eq!(p.screen().cursor_position(), (39, 4));
        set_size_keep_bottom(&mut p, 20, 80);
        assert_eq!(row(&p, 19), "PS>", "the prompt stays on the last row");
        assert_eq!(row(&p, 0), "line 20", "the newest output is still on screen");
        assert_eq!(p.screen().cursor_position(), (19, 4));
        assert_eq!(scrollback_rows(&mut p), 20, "the top 20 rows went into scrollback, not away");
        // growing back doesn't touch scrollback
        set_size_keep_bottom(&mut p, 30, 80);
        assert_eq!(scrollback_rows(&mut p), 20);

        // a short screen with the cursor near the top: nothing to push
        let mut p = vt100::Parser::new(40, 80, 1000);
        p.process(b"one\r\ntwo\r\n$ ");
        set_size_keep_bottom(&mut p, 20, 80);
        assert_eq!((row(&p, 0), row(&p, 2)), ("one".into(), "$".into()));
        assert_eq!(scrollback_rows(&mut p), 0);

        // full-screen programs (alternate screen) redraw themselves: plain truncation
        let mut p = vt100::Parser::new(40, 80, 1000);
        p.process(b"\x1b[?1049h\x1b[40;1Hbottom");
        set_size_keep_bottom(&mut p, 20, 80);
        assert_eq!(scrollback_rows(&mut p), 0);
    }

    fn shell_cmd(win: &str, unix: &str) -> (&'static str, Vec<String>) {
        if cfg!(windows) { ("cmd.exe", vec!["/c".into(), win.into()]) } else { ("sh", vec!["-c".into(), unix.into()]) }
    }

    /// Start the pane's program and wait for it to end (on Windows too, where ConPTY never closes the output).
    fn run_to_end(k: &mut crate::testkit::Kit, t: &mut Term) {
        k.render(t, 80, 10); // starts it
        let t0 = Instant::now();
        while !t.exited.load(Ordering::SeqCst) && t0.elapsed() < Duration::from_secs(10) {
            k.wait_wake(t, 50);
        }
        assert!(t.exited.load(Ordering::SeqCst), "the program should have ended");
    }

    #[test]
    fn term_clean_exit_closes_the_pane() {
        let (prog, args) = shell_cmd("exit 0", "exit 0");
        let mut k = crate::testkit::Kit::new();
        let mut t = Term::new("install thing", "package", prog, args, None).alert_on_exit("thing");
        assert!(t.failed.is_none());
        run_to_end(&mut k, &mut t);
        assert!(!t.alive(), "exit 0: the pane closes");
        let (kind, text) = t.exit_note().unwrap();
        assert_eq!(kind, crate::alerts::Kind::Download);
        assert!(text.ends_with("done"), "{text}");
    }

    /// A program that fails (Claude Code not signed in, a crashed worker, a failed install) keeps its pane, so the
    /// error can be read; enter closes it, r runs it again.
    #[test]
    fn term_failed_program_keeps_its_pane() {
        let (prog, args) = shell_cmd("echo not signed in & exit 3", "echo not signed in; exit 3");
        let mut k = crate::testkit::Kit::new();
        let mut t = Term::new("claude code", "claude", prog, args, None).alert_on_exit("thing");
        run_to_end(&mut k, &mut t);
        assert!(t.alive(), "a failed program keeps its pane");
        assert_eq!(t.failed_code(), Some(3));
        assert!(t.title().ends_with("exited with 3"), "{}", t.title());
        // the output is still there, above the footer
        let mut s = String::new();
        for _ in 0..60 {
            s = k.render(&mut t, 80, 10);
            if s.contains("not signed in") {
                break;
            }
            k.wait_wake(&mut t, 50);
        }
        let _ = k.render_html(&mut t, 80, 10, "target/snap/term-exited.html");
        assert!(s.contains("not signed in"), "{s}");
        assert!(s.lines().nth(9).unwrap_or_default().contains("exited with 3 · enter or esc closes · r reruns"), "{s}");
        // one alert, with the exit code in it
        k.poll(&mut t);
        k.poll(&mut t);
        let notes = k.notices();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].starts_with("thing: failed with exit code 3"), "{notes:?}");
        assert_eq!((t.activity(), t.tick_every()), (None, None));
        // typing goes nowhere; r runs it again in the same pane
        assert!(k.key(&mut t, KeyCode::Char('x')));
        k.key(&mut t, KeyCode::Char('r'));
        assert!(!t.exited.load(Ordering::SeqCst) && t.failed_code().is_none());
        assert_eq!(t.title(), "claude code");
        run_to_end(&mut k, &mut t);
        assert_eq!(t.failed_code(), Some(3), "it ran again");
        k.actions.clear();
        k.key(&mut t, KeyCode::Enter);
        assert!(matches!(k.actions.last(), Some(crate::pane::Action::Close)), "enter closes it");
        assert_eq!(code_text(0xC0000005), "0xC0000005");
        // your own shell ending with an error (bash's `exit` after a failed command) is you closing it: it goes
        let (prog, args) = shell_cmd("exit 3", "exit 3");
        let mut t = Term::new("shell", "term", prog, args, None);
        t.keep_failed = false; // what Term::shell sets
        run_to_end(&mut k, &mut t);
        assert!(!t.alive(), "a shell you exit closes, whatever its last status");
    }

    /// Captures what a pane sends to its program.
    #[derive(Clone, Default)]
    struct Sent(Arc<Mutex<Vec<u8>>>);
    impl Write for Sent {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    fn take(s: &Sent) -> Vec<u8> {
        std::mem::take(&mut *s.0.lock().unwrap())
    }

    /// A pane that's never drawn (its program's output isn't read) and whose keys land in `Sent`.
    fn quiet_term() -> (Term, Sent) {
        let (prog, args) = shell_cmd("pause", "sleep 30");
        let mut t = Term::new("quiet", "term", prog, args, None);
        let sent = Sent::default();
        t.writer = Arc::new(Mutex::new(Box::new(sent.clone())));
        (t, sent)
    }

    #[test]
    fn term_keyboard_scrollback_and_alternate_scroll() {
        let mut k = crate::testkit::Kit::new();
        let (mut t, sent) = quiet_term();
        {
            let mut p = t.parser.lock().unwrap();
            for i in 0..100 {
                p.process(format!("line {i}\r\n").as_bytes());
            }
        }
        let sh = KeyModifiers::SHIFT;
        k.key_mod(&mut t, KeyCode::PageUp, sh);
        assert_eq!(t.scroll, 23, "a page is the screen less a line");
        assert!(t.title().ends_with("scrolled 23"), "{}", t.title());
        k.key_mod(&mut t, KeyCode::Home, sh);
        assert_eq!(t.scroll, 77, "the top of the scrollback");
        k.key_mod(&mut t, KeyCode::PageDown, sh);
        assert_eq!(t.scroll, 54);
        k.key_mod(&mut t, KeyCode::End, sh);
        assert_eq!(t.scroll, 0);
        assert!(take(&sent).is_empty(), "scrollback keys never reach the program");
        // the wheel scrolls back on the normal screen; a key goes to the program and back to live
        let area = Rect::new(0, 0, 80, 24);
        let wheel = |kind| MouseEvent { kind, column: 5, row: 5, modifiers: KeyModifiers::NONE };
        k.mouse(&mut t, wheel(MouseEventKind::ScrollUp), area);
        assert_eq!(t.scroll, 3);
        k.key(&mut t, KeyCode::Char('a'));
        assert_eq!((t.scroll, take(&sent)), (0, b"a".to_vec()));
        // a full-screen program without mouse mode (less, man, git log): the wheel sends arrows
        t.parser.lock().unwrap().process(b"\x1b[?1049h");
        k.mouse(&mut t, wheel(MouseEventKind::ScrollUp), area);
        k.mouse(&mut t, wheel(MouseEventKind::ScrollDown), area);
        assert_eq!(t.scroll, 0);
        assert_eq!(take(&sent), b"\x1b[A\x1b[A\x1b[A\x1b[B\x1b[B\x1b[B");
        t.parser.lock().unwrap().process(b"\x1b[?1h"); // application cursor keys
        k.mouse(&mut t, wheel(MouseEventKind::ScrollUp), area);
        assert_eq!(take(&sent), b"\x1bOA\x1bOA\x1bOA");
    }

    #[test]
    fn term_shift_enter_is_a_newline_in_agents() {
        let mut k = crate::testkit::Kit::new();
        let (mut t, sent) = quiet_term();
        k.key_mod(&mut t, KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(take(&sent), b"\r", "a shell gets a plain enter");
        t.agent = true;
        k.key_mod(&mut t, KeyCode::Enter, KeyModifiers::SHIFT);
        k.key_mod(&mut t, KeyCode::Enter, KeyModifiers::CONTROL);
        assert_eq!(take(&sent), b"\x1b\r\x1b\r", "Claude Code / Codex read ESC CR as a new line");
        k.key(&mut t, KeyCode::Enter);
        assert_eq!(take(&sent), b"\r", "enter still sends");
        // Claude Code's idle footer marks a pane as an agent before it has done anything
        let (mut t, _) = quiet_term();
        assert!(!t.agent);
        t.parser.lock().unwrap().process("╭────╮\r\n│ >  │\r\n╰────╯\r\n  ? for shortcuts".as_bytes());
        t.scan();
        assert!(t.agent && t.status.is_some(), "gets a status dot too");
    }

    #[test]
    fn term_puts_oriels_folder_on_path() {
        use std::ffi::OsStr;
        use std::path::Path;
        let dir = if cfg!(windows) { Path::new(r"C:\Tools\oriel") } else { Path::new("/opt/oriel") };
        let path = if cfg!(windows) { r"C:\Windows;C:\bin" } else { "/usr/bin:/bin" };
        let got = with_dir_on_path(dir, Some(OsStr::new(path))).unwrap();
        let parts: Vec<_> = std::env::split_paths(&got).collect();
        assert_eq!((parts[0].as_path(), parts.len()), (dir, 3), "{got:?}");
        // already on it (any case, a trailing slash): left alone
        let has = if cfg!(windows) { r"C:\Windows;c:\tools\ORIEL\" } else { "/usr/bin:/opt/oriel/" };
        assert!(with_dir_on_path(dir, Some(OsStr::new(has))).is_none());
        assert!(with_dir_on_path(dir, None).is_some());
    }

    #[test]
    fn term_failed_launch_says_so() {
        let mut k = crate::testkit::Kit::new();
        let mut t = Term::new("install thing", "package", "oriel-no-such-program-4242", vec![], None).alert_on_exit("thing");
        assert_eq!(t.title(), "install thing · failed to start");
        let s = k.render(&mut t, 130, 6);
        let _ = k.render_html(&mut t, 130, 8, "target/snap/term-failed.html");
        let first = s.lines().next().unwrap_or_default();
        assert!(first.contains("oriel: couldn't start oriel-no-such-program-4242") && first.contains("plain shell"), "{s}");
        // the stand-in shell ending isn't the install succeeding
        let (kind, text) = t.exit_note().unwrap();
        assert_eq!(kind, crate::alerts::Kind::BuildFailed);
        assert!(text.starts_with("thing: couldn't start"), "{text}");
    }

    #[test]
    fn term_fkeys_go_to_full_screen_programs() {
        let (prog, args): (&str, Vec<String>) = if cfg!(windows) { ("cmd.exe", vec!["/c".into(), "exit".into()]) } else { ("sh", vec!["-c".into(), "true".into()]) };
        let mut t = Term::new("htop", "term", prog, args, None);
        assert!(!t.wants_fkeys(), "at a shell prompt F-keys switch apps");
        t.parser.lock().unwrap().process(b"\x1b[?1049h"); // the program goes full-screen (alternate screen)
        assert!(t.wants_fkeys());
        t.agent = true; // a coding agent that's full-screen (opencode) still leaves F-keys to switch apps
        assert!(!t.wants_fkeys());
    }

    #[test]
    fn term_knows_its_folder() {
        let d = std::path::absolute("target/test-scratch/shell/term dir").unwrap();
        let sub = d.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let (prog, args): (&str, Vec<String>) = if cfg!(windows) { ("cmd.exe", vec!["/c".into(), "exit".into()]) } else { ("sh", vec!["-c".into(), "true".into()]) };
        let t = Term::new("sh", "term", prog, args, Some(d.clone()));
        assert_eq!(t.cwd(), Some(d.clone()), "where it started");
        assert_eq!(t.reopen(), None, "a plain command isn't reopened");
        assert_eq!(t.reopen_as("terminal").reopen(), Some("terminal"));
        // the shell reports where it cd'd to: OSC 7 (percent-encoded, with a host) and Windows Terminal's 9;9
        let url = format!("file://host{}{}", if cfg!(windows) { "/" } else { "" }, sub.display().to_string().replace('\\', "/").replace(' ', "%20"));
        assert_eq!(osc_cwd(format!("prompt\x1b]7;{url}\x07$ ").as_bytes()), Some(sub.clone()), "{url}");
        assert_eq!(osc_cwd(format!("\x1b]9;9;\"{}\"\x1b\\", d.display()).as_bytes()), Some(d.clone()));
        // the last one wins; a folder that doesn't exist and other OSCs (titles) are ignored
        let two = format!("\x1b]9;9;{}\x07\x1b]0;title\x07\x1b]9;9;{}\x07", d.display(), sub.display());
        assert_eq!(osc_cwd(two.as_bytes()), Some(sub.clone()));
        assert_eq!(osc_cwd(b"\x1b]9;9;/no/such/folder\x07"), None);
        assert_eq!(osc_cwd(b"\x1b]0;just a title\x07"), None);
    }

    /// A coding agent at a permission prompt: its screen's question and choices are what the alerts app peeks at,
    /// and it drops off the open list once it's working again.
    #[test]
    fn term_waiting_agent_says_what_it_asks() {
        let (prog, args): (&str, Vec<String>) = if cfg!(windows) { ("cmd.exe", vec!["/c".into(), "exit".into()]) } else { ("sh", vec!["-c".into(), "true".into()]) };
        let mut t = Term::new("claude", "claude", prog, args, None);
        let screen = [
            "╭──────────────────────────────╮",
            "│ Bash command                 │",
            "│   cargo publish              │",
            "│ Do you want to proceed?      │",
            "│ ❯ 1. Yes                     │",
            "│   2. No, and tell Claude     │",
            "╰──────────────────────────────╯",
        ];
        t.parser.lock().unwrap().process(format!("\x1b[2J\x1b[H{}\r\n", screen.join("\r\n")).as_bytes());
        t.scan();
        assert_eq!(t.activity(), Some(Activity::Blocked));
        let open = t.open_now();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].text, "claude is waiting for you");
        assert_eq!(open[0].detail, ["Bash command", "cargo publish", "Do you want to proceed?", "❯ 1. Yes", "2. No, and tell Claude"], "the box's borders are trimmed off");
        assert!(open[0].options.is_empty() && !open[0].yes_no, "not answered from alerts: the keys are the agent's");
        t.parser.lock().unwrap().process(b"\x1b[2J\x1b[H* Publishing... (esc to interrupt)\r\n");
        t.scan();
        assert!(t.open_now().is_empty() && t.prompt.is_empty());
    }
}
