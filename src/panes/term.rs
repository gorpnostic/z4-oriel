//! A real terminal inside a pane: a shell (or any command, e.g. `claude`) on a pseudo-terminal
//! (ConPTY on Windows, a Unix pty elsewhere), parsed by vt100 and drawn cell by cell.

use crate::pane::{Activity, Cx, Pane, Waker};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
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
    child: Box<dyn Child + Send + Sync>,
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
    /// The folder it started in, and the one the shell last said it's in (OSC 7 / Windows Terminal's OSC 9;9,
    /// sent by prompts like starship and oh-my-posh), set by the reader thread.
    dir: Option<std::path::PathBuf>,
    osc_dir: Arc<Mutex<Option<std::path::PathBuf>>>,
    /// What it comes back as when oriel restarts: "terminal" for a shell, "claude" / "codex"; None for the rest
    /// (installs, an orchestrator's workers).
    reopen: Option<&'static str>,
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
        let child = pair.slave.spawn_command(cmd).unwrap_or_else(|e| {
            // fall back to the default shell so the pane still works, with the error visible
            eprintln!("oriel: couldn't start {prog}: {e}");
            let (sh, a) = crate::config::default_shell(&crate::config::Config::default());
            let mut c = CommandBuilder::new(sh);
            c.args(a);
            pair.slave.spawn_command(c).expect("spawn shell")
        });
        drop(pair.slave);
        let writer = pair.master.take_writer().expect("pty writer");
        Term {
            title: title.to_string(),
            icon,
            parser: Arc::new(Mutex::new(vt100::Parser::new(24, 80, 10_000))),
            writer: Arc::new(Mutex::new(writer)),
            master: pair.master,
            child,
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
            dir,
            osc_dir: Arc::default(),
            reopen: None,
        }
    }

    /// Brought back as `name` (a panes::open name) when oriel restarts where you left off.
    pub fn reopen_as(mut self, name: &'static str) -> Term {
        self.reopen = Some(name);
        self
    }

    /// When the command ends, say so in the event center: `what` is e.g. "Firefox".
    pub fn alert_on_exit(mut self, what: &str) -> Term {
        self.exit_alert = Some(what.to_string());
        self
    }

    pub fn shell(cfg: &crate::config::Config, cwd: Option<std::path::PathBuf>) -> Term {
        let (prog, args) = crate::config::default_shell(cfg);
        let name = std::path::Path::new(&prog).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or(prog.clone());
        Term::new(&name, "term", &prog, args, cwd).reopen_as("terminal")
    }

    fn start_reader(&mut self, waker: Waker) {
        let mut reader = self.master.try_clone_reader().expect("pty reader");
        let parser = self.parser.clone();
        let writer = self.writer.clone();
        let exited = self.exited.clone();
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
            exited.store(true, Ordering::SeqCst);
            waker.wake();
        });
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
        let _ = self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        self.parser.lock().unwrap().screen_mut().set_size(rows, cols);
    }
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
        if working || blocked {
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
        let _ = self.child.kill();
    }
}

impl Pane for Term {
    fn title(&self) -> String {
        if self.scroll > 0 { format!("{} · scrolled {}", self.title, self.scroll) } else { self.title.clone() }
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
        let ok = self.child.try_wait().ok().flatten().map(|s| s.success()).unwrap_or(true);
        Some(if ok { (crate::alerts::Kind::Download, format!("{what}: done")) } else { (crate::alerts::Kind::BuildFailed, format!("{what}: failed (see the terminal output next time with alt n)")) })
    }

    fn alive(&self) -> bool {
        !self.exited.load(Ordering::SeqCst)
    }
    fn activity(&self) -> Option<Activity> {
        self.status
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
        if self.agent { Some(Duration::from_millis(400)) } else { None }
    }
    fn poll(&mut self, _cx: &mut Cx) {
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
                style = style.add_modifier(m);
                let s = cell.contents();
                if let Some(bc) = buf.cell_mut(Position { x: area.x + col, y: area.y + row }) {
                    bc.set_symbol(if s.is_empty() { " " } else { s });
                    bc.set_style(style);
                }
            }
        }
        if cx.focused && !screen.hide_cursor() && self.scroll == 0 {
            let (r, c) = screen.cursor_position();
            if r < area.height && c < area.width {
                f.set_cursor_position(Position { x: area.x + c, y: area.y + r });
            }
        }
    }

    fn key(&mut self, key: KeyEvent, _cx: &mut Cx) -> bool {
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
        let (mode, enc) = {
            let p = self.parser.lock().unwrap();
            (p.screen().mouse_protocol_mode(), p.screen().mouse_protocol_encoding())
        };
        let col = ev.column.saturating_sub(area.x) + 1;
        let row = ev.row.saturating_sub(area.y) + 1;
        if mode == vt100::MouseProtocolMode::None {
            // the program doesn't want the mouse: the wheel scrolls our scrollback
            match ev.kind {
                MouseEventKind::ScrollUp => self.scroll += 3,
                MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_sub(3),
                _ => {}
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

fn btn(b: MouseButton) -> u16 {
    match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// Key -> the bytes an xterm would send.
pub fn encode_key(key: KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
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
