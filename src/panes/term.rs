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
    /// Tell the event center how this ended ("installing Firefox" -> "Firefox installed" / "failed").
    exit_alert: Option<String>,
    /// (program, why) when the requested program didn't start: the pane runs the default shell instead, under a
    /// red banner.
    failed: Option<(String, String)>,
    /// The program's exit status once it has ended (success?), set by the thread that waits on it.
    exit: Arc<Mutex<Option<bool>>>,
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
            exit_alert: None,
            failed,
            exit: Arc::new(Mutex::new(None)),
        }
    }

    /// Rows the failed-to-start banner takes at the top of the pane.
    fn banner_h(&self) -> u16 {
        self.failed.is_some() as u16
    }

    /// When the command ends, say so in the event center: `what` is e.g. "Firefox".
    pub fn alert_on_exit(mut self, what: &str) -> Term {
        self.exit_alert = Some(what.to_string());
        self
    }

    pub fn shell(cfg: &crate::config::Config, cwd: Option<std::path::PathBuf>) -> Term {
        let (prog, args) = crate::config::default_shell(cfg);
        let name = std::path::Path::new(&prog).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or(prog.clone());
        Term::new(&name, "term", &prog, args, cwd)
    }

    fn start_reader(&mut self, waker: Waker) {
        let mut reader = self.master.try_clone_reader().expect("pty reader");
        let waiter_waker = Waker { id: waker.id, tx: waker.tx.clone() };
        let parser = self.parser.clone();
        let writer = self.writer.clone();
        let exited = self.exited.clone();
        let (last_output, born) = (self.last_output.clone(), self.born);
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
                        last_output.store(born.elapsed().as_millis() as u64, Ordering::Relaxed);
                        waker.wake();
                    }
                }
            }
            exited.store(true, Ordering::SeqCst);
            waker.wake();
        });
        // ConPTY keeps the output open after the program ends (the reader above never sees EOF on Windows), so a
        // second thread waits on the process itself: the pane closes and the exit alert fires, with no polling
        if let Some(mut child) = self.child.take() {
            let (exit, exited) = (self.exit.clone(), self.exited.clone());
            std::thread::spawn(move || {
                if let Ok(s) = child.wait() {
                    *exit.lock().unwrap() = Some(s.success());
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
        let text = {
            let p = self.parser.lock().unwrap();
            let s = p.screen();
            let (_, cols) = s.size();
            let rows: Vec<String> = s.rows(0, cols).collect();
            let tail: Vec<&String> = rows.iter().rev().filter(|r| !r.trim().is_empty()).take(14).collect();
            tail.iter().rev().map(|r| r.to_lowercase()).collect::<Vec<_>>().join("\n")
        };
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
        if self.scroll > 0 { format!("{} · scrolled {}", self.title, self.scroll) } else { self.title.clone() }
    }
    fn icon(&self) -> &'static str {
        self.icon
    }
    fn is_terminal(&self) -> bool {
        true
    }
    fn exit_note(&mut self) -> Option<(crate::alerts::Kind, String)> {
        let what = self.exit_alert.take()?;
        if let Some((prog, e)) = &self.failed {
            // the shell that stood in for it exiting says nothing about the program: it never ran
            return Some((crate::alerts::Kind::BuildFailed, format!("{what}: couldn't start {prog} ({e})")));
        }
        // the output can close a moment before the waiting thread has the exit status: give it a little time
        let mut ok = *self.exit.lock().unwrap();
        for _ in 0..10 {
            if ok.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
            ok = *self.exit.lock().unwrap();
        }
        Some(match ok {
            Some(true) => (crate::alerts::Kind::Download, format!("{what}: done")),
            Some(false) => (crate::alerts::Kind::BuildFailed, format!("{what}: failed (see the terminal output next time with alt n)")),
            None => (crate::alerts::Kind::Download, format!("{what}: ended (couldn't tell whether it worked)")),
        })
    }

    fn alive(&self) -> bool {
        !self.exited.load(Ordering::SeqCst)
    }
    fn activity(&self) -> Option<Activity> {
        self.status
    }
    fn wants_mouse(&self) -> bool {
        self.parser.lock().map(|p| p.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None).unwrap_or(false)
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
        let row = ev.row.saturating_sub(area.y + self.banner_h()) + 1;
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

    /// A program that ends: the pane notices (on Windows too, where ConPTY never closes the output) and the exit
    /// alert tells success from failure.
    #[test]
    fn term_program_exit_closes_the_pane_with_its_result() {
        for (code, want) in [(3, crate::alerts::Kind::BuildFailed), (0, crate::alerts::Kind::Download)] {
            let (prog, args) = if cfg!(windows) { ("cmd.exe", vec!["/c".to_string(), format!("exit {code}")]) } else { ("sh", vec!["-c".to_string(), format!("exit {code}")]) };
            let mut k = crate::testkit::Kit::new();
            let mut t = Term::new("install thing", "package", prog, args, None).alert_on_exit("thing");
            assert!(t.failed.is_none());
            k.render(&mut t, 80, 10); // starts it
            let t0 = Instant::now();
            while t.alive() && t0.elapsed() < Duration::from_secs(10) {
                k.wait_wake(&mut t, 50);
            }
            assert!(!t.alive(), "exit {code}: the pane should close when the program ends");
            let (kind, text) = t.exit_note().unwrap();
            assert_eq!(kind, want, "exit {code}: {text}");
            assert_eq!(text.ends_with("done"), code == 0, "{text}");
        }
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
}
