//! QA key-mash: deterministic pseudo-random keys, mouse gestures and pastes thrown at every pane and at the App,
//! with renders at many sizes (down to 1x1) in between. Nothing may panic or hang.
//!
//! Headless and silent: panes render into `testkit::Kit` / ratatui's TestBackend, nothing touches the desktop.
//! Anything that could reach the real machine is either faked (the panes' own qa modules) or filtered out of the
//! stream by a per-pane guard (e.g. the files app never gets `o`, which opens Explorer). Tests that need
//! ORIEL_DATA_DIR, or that touch process-wide state (the alerts centre), run in a child copy of this test binary.
//!
//!   cargo test qa_keys                          every mash (they're also in panes::*::qa_keys and app::qa_keys)
//!   $env:QA_SEED=7;     cargo test qa_keys      another seed stream (the default seeds are fixed per test)
//!   $env:QA_KEYS=20000; cargo test qa_keys      more keys per test
//!
//!   cargo test _bug_ -- --ignored              the bugs the mashes found, one small repro each (they fail)
//!   cargo test qa_keys_home_frame_cost -- --ignored --nocapture    why the home mash is the slow one
//!
//! Scratch files live in target/test-scratch/qa-keys/ only.

use crate::pane::{Cx, Pane};
use crate::testkit::Kit;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ------------------------------------------------------------------ randomness

/// xorshift64: tiny, seeded, the same stream on every machine.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Rng {
        Rng((seed ^ 0x9E37_79B9_7F4A_7C15).wrapping_mul(0xBF58_476D_1CE4_E5B9) | 1)
    }
    pub(crate) fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    pub(crate) fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
    pub(crate) fn pct(&mut self, p: usize) -> bool {
        self.below(100) < p
    }
    pub(crate) fn pick<T: Clone>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len())].clone()
    }
}

/// The seed for a test: fixed per test name, mixed with $QA_SEED when set.
pub(crate) fn seed_for(name: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
    }
    let extra: u64 = std::env::var("QA_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    h ^ extra.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// Keys per test: $QA_KEYS, else `default`.
pub(crate) fn keys_for(default: usize) -> usize {
    std::env::var("QA_KEYS").ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

const LETTERS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
const PUNCT: &str = " /.,-=+[]?:;'\"~!@#$%^&*()_{}|\\<>`";
/// Text that has broken byte-indexed string code before: accents, wide CJK, emoji, combining marks, RTL, ZWJ.
const EXOTIC: &[char] = &['é', 'ß', 'ø', 'Ω', '中', '日', '本', '한', '🙂', '👍', '\u{301}', '\u{200b}', '\u{200d}', 'ש', 'ع', '€', '…', '→', '█', '\u{a0}', '\u{fe0f}'];

/// One random key, spread over what a person could press: letters, digits, punctuation, unicode, arrows and paging,
/// enter/esc/tab/backtab/backspace/delete/insert, F1-F24, with ctrl / alt / shift combos.
pub(crate) fn key(r: &mut Rng) -> KeyEvent {
    let code = match r.below(100) {
        0..=29 => KeyCode::Char(r.pick(&LETTERS.chars().collect::<Vec<_>>())),
        30..=37 => KeyCode::Char((b'0' + r.below(10) as u8) as char),
        38..=46 => KeyCode::Char(r.pick(&PUNCT.chars().collect::<Vec<_>>())),
        47..=50 => KeyCode::Char(r.pick(EXOTIC)),
        51..=64 => r.pick(&[KeyCode::Up, KeyCode::Down, KeyCode::Left, KeyCode::Right, KeyCode::Home, KeyCode::End, KeyCode::PageUp, KeyCode::PageDown]),
        65..=80 => r.pick(&[KeyCode::Enter, KeyCode::Esc, KeyCode::Tab, KeyCode::BackTab, KeyCode::Backspace, KeyCode::Delete, KeyCode::Insert, KeyCode::Enter, KeyCode::Esc, KeyCode::Backspace]),
        81..=85 => KeyCode::F(1 + r.below(12) as u8),
        86 => KeyCode::F(13 + r.below(12) as u8),
        87..=90 => KeyCode::Char(' '),
        91 => r.pick(&[KeyCode::Null, KeyCode::CapsLock, KeyCode::Menu, KeyCode::KeypadBegin, KeyCode::PrintScreen, KeyCode::Pause]),
        // the keys apps actually bind, so modes get entered and left often
        _ => KeyCode::Char(r.pick(&['j', 'k', 'q', 'y', 'n', 'x', 'd', 'e', 's', 'r', 't', 'g', 'G', 'L', 'P', '/', '?', '1', '2', '3'])),
    };
    let mods = match r.below(100) {
        0..=67 => KeyModifiers::NONE,
        68..=78 => KeyModifiers::CONTROL,
        79..=85 => KeyModifiers::ALT,
        86..=92 => KeyModifiers::SHIFT,
        93..=95 => KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        96..=97 => KeyModifiers::ALT | KeyModifiers::SHIFT,
        _ => KeyModifiers::CONTROL | KeyModifiers::ALT,
    };
    KeyEvent::new(code, mods)
}

fn button(r: &mut Rng) -> MouseButton {
    match r.below(10) {
        0..=6 => MouseButton::Left,
        7..=8 => MouseButton::Right,
        _ => MouseButton::Middle,
    }
}

/// A mouse gesture at random places inside `w` x `h` (offset by `x0`,`y0`): a click, a double click, a drag,
/// wheel turns, hovering. Several events in order.
pub(crate) fn gesture(r: &mut Rng, x0: u16, y0: u16, w: u16, h: u16) -> Vec<MouseEvent> {
    let at = |r: &mut Rng| (x0.saturating_add(r.below(w.max(1) as usize) as u16), y0.saturating_add(r.below(h.max(1) as usize) as u16));
    let mods = match r.below(100) {
        0..=86 => KeyModifiers::NONE,
        87..=94 => KeyModifiers::SHIFT,
        95..=97 => KeyModifiers::CONTROL,
        _ => KeyModifiers::ALT,
    };
    let ev = |kind, (column, row): (u16, u16)| MouseEvent { kind, column, row, modifiers: mods };
    let p = at(r);
    match r.below(100) {
        0..=29 => {
            let b = button(r);
            vec![ev(MouseEventKind::Down(b), p), ev(MouseEventKind::Up(b), p)]
        }
        30..=41 => vec![
            ev(MouseEventKind::Down(MouseButton::Left), p),
            ev(MouseEventKind::Up(MouseButton::Left), p),
            ev(MouseEventKind::Down(MouseButton::Left), p),
            ev(MouseEventKind::Up(MouseButton::Left), p),
        ],
        42..=56 => {
            let b = if r.pct(85) { MouseButton::Left } else { button(r) };
            let mut v = vec![ev(MouseEventKind::Down(b), p)];
            for _ in 0..1 + r.below(4) {
                v.push(ev(MouseEventKind::Drag(b), at(r)));
            }
            v.push(ev(MouseEventKind::Up(b), at(r)));
            v
        }
        57..=71 => (0..1 + r.below(3)).map(|_| ev(MouseEventKind::Moved, at(r))).collect(),
        72..=83 => (0..1 + r.below(4)).map(|_| ev(MouseEventKind::ScrollDown, p)).collect(),
        84..=95 => (0..1 + r.below(4)).map(|_| ev(MouseEventKind::ScrollUp, p)).collect(),
        96..=97 => vec![ev(MouseEventKind::ScrollLeft, p), ev(MouseEventKind::ScrollRight, p)],
        // a lone press or release (the other half landed outside the window)
        _ => vec![ev(if r.pct(50) { MouseEventKind::Down(button(r)) } else { MouseEventKind::Up(button(r)) }, p)],
    }
}

/// Pasted text: words, newlines (both kinds), tabs, unicode, sometimes long.
pub(crate) fn paste_text(r: &mut Rng, allow_slash: bool) -> String {
    let n = if r.pct(10) { 200 + r.below(800) } else { r.below(40) };
    let mut s = String::new();
    for _ in 0..n {
        let c = match r.below(100) {
            0..=69 => r.pick(&LETTERS.chars().collect::<Vec<_>>()),
            70..=79 => ' ',
            80..=83 => '\n',
            84 => '\r',
            85 => '\t',
            86..=91 => r.pick(EXOTIC),
            _ => r.pick(&PUNCT.chars().collect::<Vec<_>>()),
        };
        s.push(c);
    }
    if !allow_slash {
        s = s.replace('/', "|");
    }
    s
}

/// Screen sizes to render at: roomy, typical, cramped, and absurd (1x1, 250x3, 6x50).
pub(crate) const SIZES: &[(u16, u16)] =
    &[(120, 40), (80, 24), (200, 60), (150, 44), (100, 30), (60, 20), (40, 12), (24, 8), (12, 4), (6, 3), (3, 2), (2, 2), (1, 1), (250, 3), (6, 50), (33, 17), (70, 10)];

// ------------------------------------------------------------------ the bounded runner

/// What a running mash reports back, so a panic or a hang says where it happened.
#[derive(Default)]
pub(crate) struct Probe {
    pub step: AtomicUsize,
    pub log: Mutex<VecDeque<String>>,
}

impl Probe {
    pub(crate) fn note(&self, step: usize, what: String) {
        self.step.store(step, Ordering::Relaxed);
        let mut l = self.log.lock().unwrap_or_else(|e| e.into_inner());
        l.push_back(format!("#{step} {what}"));
        while l.len() > 30 {
            l.pop_front();
        }
    }
    fn tail(&self) -> String {
        let l = self.log.lock().unwrap_or_else(|e| e.into_inner());
        l.iter().cloned().collect::<Vec<_>>().join("\n    ")
    }
}

/// Run `body` on its own thread with a deadline. A panic in it fails the test with the panic message, the step
/// and the last events; running past `budget` fails it as a hang.
pub(crate) fn bounded(name: &str, budget: Duration, body: impl FnOnce(Arc<Probe>) + Send + 'static) {
    let probe = Arc::new(Probe::default());
    let p2 = probe.clone();
    let h = std::thread::Builder::new().name(format!("qa-{name}")).stack_size(16 << 20).spawn(move || body(p2)).unwrap();
    let t0 = Instant::now();
    while !h.is_finished() {
        if t0.elapsed() > budget {
            panic!(
                "HANG: qa {name} still running after {:.0}s at step {}\n  last events:\n    {}",
                t0.elapsed().as_secs_f64(),
                probe.step.load(Ordering::Relaxed),
                probe.tail()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    if let Err(e) = h.join() {
        let msg = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_else(|| "(non-string panic)".into());
        panic!("PANIC in qa {name} at step {}: {msg}\n  last events:\n    {}", probe.step.load(Ordering::Relaxed), probe.tail());
    }
}

// ------------------------------------------------------------------ one pane

/// One thing the mash does to a pane. A guard may change it or return false to leave it out. Renders go past
/// the guard too (as `Render` / `RenderSide`), so a mash can step around a known crash at some size.
#[derive(Debug, Clone)]
#[allow(dead_code)] // a render's size is only read by the guards that step around a size crash
pub(crate) enum Ev {
    Key(KeyEvent),
    Mouse(MouseEvent),
    SideMouse(MouseEvent),
    Paste(String),
    Render(u16, u16),
    RenderSide(u16, u16),
}

pub(crate) struct Opts {
    pub keys: usize,
    /// render the pane's sidebar section too, and click in it
    pub side: bool,
    pub budget: Duration,
    /// any single event or render slower than this counts as a hang
    pub slow: Duration,
    /// save a final HTML snapshot here (under target/test-scratch/qa-keys)
    pub snap: Option<String>,
    /// render sizes (default SIZES); a mash that has to step around a known small-size crash narrows them
    pub sizes: &'static [(u16, u16)],
}

impl Default for Opts {
    fn default() -> Self {
        Opts { keys: keys_for(2500), side: true, budget: Duration::from_secs(150), slow: Duration::from_secs(5), snap: None, sizes: SIZES }
    }
}

fn side_cx<'a>(k: &'a mut Kit) -> Cx<'a> {
    Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: &mut k.actions, focused: true, time: k.time }
}

/// Mash one pane: `make` builds it (on the mash thread, so it needn't be Send), `guard` vets every event
/// (return false to drop it; it may also rewrite it). Renders at many sizes, polls, lets time pass.
pub(crate) fn mash_pane<P: Pane + 'static>(
    name: &'static str,
    opts: Opts,
    make: impl FnOnce(&mut Kit) -> P + Send + 'static,
    mut guard: impl FnMut(&mut P, &mut Ev) -> bool + Send + 'static,
) {
    let budget = opts.budget;
    bounded(name, budget, move |probe| {
        let mut r = Rng::new(seed_for(name));
        let mut k = Kit::new();
        k.time = 10.0;
        let mut p = make(&mut k);
        let (mut w, mut h) = (120u16, 40u16);
        let (mut sw, mut sh) = (30u16, 20u16);
        let slow = opts.slow;
        let timed = |what: &str, step: usize, t: Instant| {
            let dt = t.elapsed();
            assert!(dt < slow, "SLOW: {what} took {:.1}s at step {step} (counts as a hang)", dt.as_secs_f64());
        };
        probe.note(0, format!("render {w}x{h}"));
        if guard(&mut p, &mut Ev::Render(w, h)) {
            k.render(&mut p, w, h);
        }
        let mut sent = 0usize;
        let mut step = 0usize;
        while sent < opts.keys {
            step += 1;
            if r.pct(3) {
                (w, h) = r.pick(opts.sizes);
            }
            let roll = r.below(100);
            let mut evs: Vec<Ev> = match roll {
                0..=69 => vec![Ev::Key(key(&mut r))],
                70..=81 => gesture(&mut r, 0, 0, w, h).into_iter().map(Ev::Mouse).collect(),
                82..=85 if opts.side => gesture(&mut r, 0, 0, sw, sh).into_iter().map(Ev::SideMouse).collect(),
                86..=87 => vec![Ev::Paste(paste_text(&mut r, true))],
                _ => vec![],
            };
            for ev in evs.iter_mut() {
                if !guard(&mut p, ev) {
                    probe.note(step, format!("(guard dropped {ev:?})"));
                    continue;
                }
                probe.note(step, format!("{ev:?} @ {w}x{h}"));
                let t = Instant::now();
                match ev.clone() {
                    Ev::Key(ke) => {
                        sent += 1;
                        k.key_mod(&mut p, ke.code, ke.modifiers);
                    }
                    Ev::Mouse(m) => k.mouse(&mut p, m, Rect::new(0, 0, w, h)),
                    Ev::SideMouse(m) => {
                        let mut cx = side_cx(&mut k);
                        p.side_mouse(m, Rect::new(0, 0, sw, sh), &mut cx);
                    }
                    Ev::Paste(s) => {
                        let mut cx = side_cx(&mut k);
                        p.paste(&s, &mut cx);
                    }
                    Ev::Render(..) | Ev::RenderSide(..) => {}
                }
                timed("event", step, t);
            }
            match roll {
                88..=93 => {}
                94..=95 if opts.side => {
                    (sw, sh) = if r.pct(80) { (22 + r.below(15) as u16, 6 + r.below(30) as u16) } else { r.pick(opts.sizes) };
                    if guard(&mut p, &mut Ev::RenderSide(sw, sh)) {
                        probe.note(step, format!("render side {sw}x{sh}"));
                        let t = Instant::now();
                        k.render_side(&mut p, sw, sh);
                        timed("side render", step, t);
                    }
                }
                96..=97 => {
                    probe.note(step, "poll".into());
                    let t = Instant::now();
                    k.poll(&mut p);
                    timed("poll", step, t);
                }
                98..=99 => {
                    k.time += 0.5 + r.below(30) as f64 / 3.0;
                    probe.note(step, format!("time -> {:.1}s, poll", k.time));
                    k.poll(&mut p);
                }
                _ => {}
            }
            // most events are followed by a frame, like the app does
            if (roll >= 88 && roll <= 93 || r.pct(35)) && guard(&mut p, &mut Ev::Render(w, h)) {
                probe.note(step, format!("render {w}x{h}"));
                let t = Instant::now();
                k.render(&mut p, w, h);
                timed("render", step, t);
            }
            if step % 50 == 0 {
                k.actions.clear(); // drop the panes it asked to open (a Term kills its process on drop)
            }
        }
        // settle: every size once, then the sidebar
        for &(w, h) in opts.sizes {
            if guard(&mut p, &mut Ev::Render(w, h)) {
                probe.note(step, format!("final render {w}x{h}"));
                k.render(&mut p, w, h);
            }
        }
        if opts.side && guard(&mut p, &mut Ev::RenderSide(30, 20)) {
            k.render_side(&mut p, 30, 20);
        }
        if let Some(path) = &opts.snap {
            let _ = k.render_html(&mut p, 150, 44, &scratch_path(&format!("snap/{path}.html")).to_string_lossy());
        }
        // what it ended up looking at (cargo test qa_keys_<name> -- --nocapture)
        println!("---- qa {name}: {sent} keys, {step} steps; final frame:\n{}", k.render(&mut p, 100, 30));
        k.actions.clear();
    });
}

// ------------------------------------------------------------------ scratch + child processes

/// target/test-scratch/qa-keys/<rel> inside this worktree (absolute).
pub(crate) fn scratch_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-scratch").join("qa-keys").join(rel)
}

/// A fresh, empty scratch folder.
pub(crate) fn scratch(rel: &str) -> PathBuf {
    let d = scratch_path(rel);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// True inside a child started by `run_child` (the child tests are #[ignore]d and do nothing on their own).
pub(crate) fn in_child() -> bool {
    std::env::var_os("QA_KEYS_CHILD").is_some()
}

/// Run one #[ignore]d test of this binary in a child process: its own ORIEL_DATA_DIR and working folder under
/// scratch (so process-wide state and data files stay out of this process and out of the real profile), plus
/// `env`. Fails with the child's output if it fails, and as a hang if it outlives `budget`.
pub(crate) fn run_child(test: &str, tag: &str, env: &[(&str, &str)], budget: Duration) {
    let root = scratch(&format!("child-{tag}"));
    let (data, cwd) = (root.join("data"), root.join("cwd"));
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(cwd.join("target").join("test-scratch")).unwrap();
    let out_path = root.join("output.txt");
    let out = std::fs::File::create(&out_path).unwrap();
    let err = out.try_clone().unwrap();
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args([test, "--exact", "--ignored", "--nocapture", "--test-threads=1"])
        .current_dir(&cwd)
        .env("QA_KEYS_CHILD", "1")
        .env("ORIEL_DATA_DIR", &data)
        .env("RUST_BACKTRACE", "0")
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err);
    for (k, v) in env {
        cmd.env(k, v);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // no console window
    }
    let mut child = cmd.spawn().expect("start the child test");
    let t0 = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if t0.elapsed() > budget {
            let _ = child.kill();
            let _ = child.wait();
            panic!("HANG: child {test} still running after {:.0}s\n{}", t0.elapsed().as_secs_f64(), tail_of(&out_path));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let text = std::fs::read_to_string(&out_path).unwrap_or_default();
    assert!(text.contains("1 passed"), "child {test} didn't run its test (exit {status:?}):\n{}", tail_of(&out_path));
    assert!(status.success(), "child {test} failed (exit {status:?}):\n{}", tail_of(&out_path));
}

fn tail_of(p: &Path) -> String {
    let s = std::fs::read_to_string(p).unwrap_or_default();
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(60)..].join("\n")
}

// ------------------------------------------------------------------ the panes that need no private access

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panes;

    /// A small tree of files to browse: folders, code, markdown, an image, a binary, unicode names, a deep path.
    fn files_fixture(d: &Path) {
        for sub in ["src", "docs", "assets", "a/b/c/d", "ünïcödé 文件夹", "empty"] {
            std::fs::create_dir_all(d.join(sub)).unwrap();
        }
        std::fs::write(d.join("README.md"), "# fixture\n\n**bold** and `code`\n\n- one\n- two\n\n```rust\nfn main() {}\n```\n").unwrap();
        std::fs::write(d.join("src/main.rs"), (0..600).map(|i| format!("fn f{i}() {{ let s = \"中文 {i}\"; }}\n")).collect::<String>()).unwrap();
        std::fs::write(d.join("docs/wide.txt"), format!("{}\n\ttabs\there\n🙂🙂🙂 emoji line\n", "x".repeat(5000))).unwrap();
        std::fs::write(d.join("blob.bin"), (0..=255u8).collect::<Vec<_>>()).unwrap();
        std::fs::write(d.join("ünïcödé 文件夹/名前.txt"), "こんにちは\n").unwrap();
        std::fs::write(d.join("a/b/c/d/deep.json"), "{\"k\": [1, 2, 3]}").unwrap();
        std::fs::write(d.join(".hidden"), "shh").unwrap();
        std::fs::write(d.join("empty.txt"), "").unwrap();
        let img = image::RgbImage::from_fn(97, 31, |x, y| image::Rgb([(x * 2) as u8, (y * 8) as u8, 99]));
        img.save(d.join("assets/pic.png")).unwrap();
        let tiny = image::RgbImage::from_fn(1, 1, |_, _| image::Rgb([1, 2, 3]));
        tiny.save(d.join("assets/tiny.png")).unwrap();
        std::fs::write(d.join("assets/broken.png"), b"\x89PNG not really").unwrap();
    }

    #[test]
    fn qa_keys_rng_is_deterministic() {
        let (mut a, mut b) = (Rng::new(5), Rng::new(5));
        let ka: Vec<String> = (0..200).map(|_| format!("{:?}", key(&mut a))).collect();
        let kb: Vec<String> = (0..200).map(|_| format!("{:?}", key(&mut b))).collect();
        assert_eq!(ka, kb);
        assert_ne!(Rng::new(5).next(), Rng::new(6).next());
    }

    /// Every key encoding the terminal pane can be asked for, including F13-F255, odd KeyCodes and every modifier
    /// combination: encode_key must never panic.
    #[test]
    fn qa_keys_term_encode_key_fuzz() {
        let mut r = Rng::new(seed_for("encode"));
        let all_mods = [KeyModifiers::NONE, KeyModifiers::SHIFT, KeyModifiers::CONTROL, KeyModifiers::ALT, KeyModifiers::SUPER, KeyModifiers::all()];
        for i in 0..60_000u32 {
            let code = match i % 7 {
                0 => KeyCode::F((i % 256) as u8),
                1 => KeyCode::Char(char::from_u32(i % 0x3000).unwrap_or('x')),
                2 => KeyCode::Char(char::from_u32(r.below(0x10FFFF) as u32).unwrap_or('y')),
                _ => key(&mut r).code,
            };
            let m = all_mods[r.below(all_mods.len())] | if r.pct(20) { KeyModifiers::SHIFT } else { KeyModifiers::NONE };
            let _ = panes::term::encode_key(KeyEvent::new(code, m), r.pct(50));
        }
    }

    #[test]
    fn qa_keys_home() {
        // no launching: every app key and enter would open a real app (a shell, the music library...)
        mash_pane("home", Opts { side: false, ..Default::default() }, |_| panes::home::Home::new(), |_p, ev| match ev {
            Ev::Key(k) => {
                let launches = matches!(k.code, KeyCode::Enter) || matches!(k.code, KeyCode::Char(c) if panes::APPS.iter().any(|a| a.1 == c));
                !launches || k.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
            }
            Ev::Mouse(m) => {
                if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
                    m.kind = MouseEventKind::Moved; // a click on an app launches it
                }
                true
            }
            _ => true,
        });
    }

    /// The mash sidesteps the bug below (it tracks the search box itself and holds back topic moves while a query
    /// is up), so the rest of help keeps getting exercised.
    #[test]
    fn qa_keys_help() {
        let (mut searching, mut query) = (false, String::new());
        mash_pane("help", Opts::default(), |_| panes::help::Help::new(), move |_, ev| {
            let Ev::Key(k) = ev else { return true };
            let plain = !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
            if searching {
                match k.code {
                    KeyCode::Esc => (searching, query) = (false, String::new()),
                    KeyCode::Enter => searching = false,
                    KeyCode::Backspace => {
                        query.pop();
                    }
                    KeyCode::Char(c) if plain => query.push(c),
                    _ => {}
                }
                return true;
            }
            match k.code {
                KeyCode::Char('/') => (searching, query) = (true, String::new()),
                KeyCode::Esc => query.clear(),
                KeyCode::Down | KeyCode::Up | KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('j' | 'k') if !query.is_empty() => return false,
                _ => {}
            }
            true
        });
    }

    /// Why qa_keys_home is the slowest mash: every Home render, key and mouse event calls Home::apps(), which
    /// calls panes::available() for each app, and for claude / codex that's config::which(): a PATH search on
    /// disk (4 extensions x every PATH folder until a hit). Help renders the same kind of screen without it.
    #[test]
    #[ignore = "fails: home 36-43 ms/frame vs help 6.5: Home::apps() runs two PATH searches (18-28 ms, 89 PATH folders) per render/key/mouse move (home.rs:25, mod.rs:41)"]
    fn qa_keys_home_frame_cost() {
        let mut k = Kit::new();
        let (mut home, mut help) = (panes::home::Home::new(), panes::help::Help::new());
        let n = 200;
        let time = |k: &mut Kit, p: &mut dyn Pane| {
            k.render(p, 120, 40); // warm up
            let t = Instant::now();
            for _ in 0..n {
                k.render(p, 120, 40);
            }
            t.elapsed().as_secs_f64() * 1000.0 / n as f64
        };
        let (home_ms, help_ms) = (time(&mut k, &mut home), time(&mut k, &mut help));
        let t = Instant::now();
        for _ in 0..n {
            let _ = panes::available("claude");
            let _ = panes::available("codex");
        }
        let which_ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        let dirs = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).count()).unwrap_or(0);
        println!("home {home_ms:.2} ms/frame, help {help_ms:.2} ms/frame, available(claude)+available(codex) {which_ms:.2} ms ({dirs} PATH folders)");
        assert!(which_ms < 0.5, "two PATH searches cost {which_ms:.2} ms, paid on every home frame, key and mouse move (home {home_ms:.2} ms vs help {help_ms:.2} ms per frame)");
    }

    /// Found by qa_keys_help. In the app: F10, `/`, type something no topic has, enter, then ↓ — oriel exits.
    #[test]
    #[ignore = "fails: help panics (index out of bounds, help.rs:414) on ↓/↑/tab after a search that matches nothing"]
    fn qa_keys_help_bug_move_after_search_with_no_match() {
        let mut k = Kit::new();
        let mut p = panes::help::Help::new();
        k.key(&mut p, KeyCode::Char('/'));
        k.typ(&mut p, "zzqqxx");
        k.key(&mut p, KeyCode::Enter); // search box closes, the query (and its empty result) stays
        k.render(&mut p, 120, 40);
        k.key(&mut p, KeyCode::Down); // shown[0] on an empty list
    }

    /// Found by qa_keys_app. In the app on an 80x24 terminal: ctrl+space - five times (split down, each time in the
    /// new pane) and oriel exits: the fifth split halves a pane that is one row tall.
    #[test]
    #[ignore = "fails: layout::split_rect panics (clamp min > max, layout.rs:148) splitting a 1-row/1-col area: 5 split-downs on 80x24"]
    fn qa_keys_layout_bug_split_a_one_row_pane() {
        use crate::layout::{Dir, Node};
        let mut root = Node::Leaf(1);
        for id in 2..=6 {
            assert!(root.split(id - 1, id, Dir::Down)); // ctrl+space - : the new pane gets the focus
        }
        let mut out = vec![];
        root.rects(Rect::new(26, 0, 54, 24), &mut out); // what App::draw does with the body of an 80x24 screen
    }

    /// Same crash, smaller: any split on a terminal one row tall (or one column wide).
    #[test]
    #[ignore = "fails: layout::split_rect panics (clamp min > max, layout.rs:148/143) for an area 1 cell thick"]
    fn qa_keys_layout_bug_split_rect_one_cell() {
        let _ = crate::layout::split_rect(Rect::new(0, 0, 80, 1), crate::layout::Dir::Down, 0.5);
    }

    /// Found by qa_keys_app_tour with QA_SEED=1 (a 12x4 screen, now and then: it depends on when the pty's output
    /// arrives). The root cause, in the vt100 crate: once the screen is one row tall, any line that wraps panics
    /// in Grid::col_wrap (`prev_pos.row -= scrolled` with row 0, scrolled 1). Term::resize (term.rs:145) refuses
    /// 0 rows but lets 1 through. What that does to the app: panes::term::qa_keys.
    #[test]
    #[ignore = "fails: vt100 0.16.2 panics (subtract with overflow, grid.rs:683) when a line wraps on a 1-row screen, which term.rs:145 allows"]
    fn qa_keys_term_bug_vt100_wrap_on_one_row() {
        let mut p = vt100::Parser::new(24, 80, 0);
        p.screen_mut().set_size(1, 30); // what Term::resize does for a pane one row tall and 30 wide
        p.process(b"Pinging 127.0.0.1 with 32 bytes of data:\r\n");
    }

    #[test]
    fn qa_keys_updates() {
        // cfg(test) keeps it off the network and from updating or rolling back
        mash_pane("updates", Opts { side: false, ..Default::default() }, |_| panes::updates::Updates::new(), |_, _| true);
    }

    #[test]
    fn qa_keys_calendar() {
        let dir = scratch("calendar");
        std::fs::write(
            dir.join("calendar.json"),
            r#"[{"day":20000,"at":600,"text":"dentist 中文 🙂"},{"day":20000,"text":"all day thing"},{"day":20001,"at":1439,"text":"late"},{"day":0,"at":0,"text":"epoch"}]"#,
        )
        .unwrap();
        let path = dir.join("calendar.json");
        mash_pane("calendar", Opts { snap: Some("calendar".into()), ..Default::default() }, move |_| panes::calendar::Calendar::open_at(path), |_, _| true);
    }

    #[test]
    fn qa_keys_notes() {
        let dir = scratch("notes");
        std::fs::write(dir.join("shopping.md"), "# shopping\n\n- milk\n- 中文 eggs 🙂\n\ttabbed\n").unwrap();
        std::fs::write(dir.join("long.md"), (0..400).map(|i| format!("line {i} {}\n", "word ".repeat(i % 40))).collect::<String>()).unwrap();
        std::fs::write(dir.join("crlf.md"), "one\r\ntwo\r\nthree é\r\n").unwrap();
        std::fs::write(dir.join("empty.md"), "").unwrap();
        mash_pane("notes", Opts { snap: Some("notes".into()), ..Default::default() }, move |_| panes::notes::Notes::open_in(dir, None), |_, _| true);
    }

    #[test]
    fn qa_keys_themes() {
        let dir = scratch("themes");
        mash_pane(
            "themes",
            Opts::default(),
            move |_| {
                crate::theme::TEST_DIR.with(|d| *d.borrow_mut() = Some(dir)); // theme files stay in scratch
                panes::themes::Themes::new()
            },
            |_, _| true,
        );
    }

    #[test]
    fn qa_keys_files() {
        let root = scratch("files");
        files_fixture(&root);
        let root_title = panes::files::Files::new(Some(root.clone())).title();
        mash_pane(
            "files",
            Opts { snap: Some("files".into()), ..Default::default() },
            move |_| panes::files::Files::new(Some(root)),
            move |p, ev| match ev {
                Ev::Key(k) => {
                    if k.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) {
                        return true; // files ignores these
                    }
                    match k.code {
                        // o opens Explorer, p writes the real clipboard, ~ goes to the real home folder, t starts a shell
                        KeyCode::Char('o' | 'p' | '~' | 't') => false,
                        // never climb out of the fixture
                        KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => p.title() != root_title,
                        _ => true,
                    }
                }
                // the sidebar's places are the real home, desktop and drives: scroll it, don't click it
                Ev::SideMouse(m) => matches!(m.kind, MouseEventKind::ScrollDown | MouseEventKind::ScrollUp | MouseEventKind::Moved),
                _ => true,
            },
        );
    }

    #[test]
    fn qa_keys_term() {
        // an inert program on the pty: ping ignores everything typed at it (no shell to run what the mash types)
        let prog = if cfg!(windows) { r"C:\Windows\System32\PING.EXE" } else { "/bin/sleep" };
        if !Path::new(prog).exists() {
            eprintln!("no {prog}: skipping");
            return;
        }
        let args: Vec<String> = if cfg!(windows) { vec!["-n".into(), "25".into(), "127.0.0.1".into()] } else { vec!["25".into()] };
        // never one row tall: a line wrapping there panics in vt100 (qa_keys_term_bug_*), depending on pty timing
        mash_pane("term", Opts { side: false, keys: keys_for(2000), ..Default::default() }, move |_| panes::term::Term::new("inert", "term", prog, args, None), |_, ev| {
            !matches!(ev, Ev::Render(_, 1))
        });
    }

    /// The alerts centre is one list for the whole process, so this runs in a child (the alerts tests use it too).
    #[test]
    fn qa_keys_alerts() {
        run_child("qa_keys::tests::qa_keys_alerts_child", "alerts", &[], Duration::from_secs(200));
    }

    #[test]
    #[ignore = "child process: run by qa_keys_alerts"]
    fn qa_keys_alerts_child() {
        if !in_child() {
            return;
        }
        use crate::alerts::{self, Alert, Kind};
        {
            let mut c = alerts::CENTER.lock().unwrap();
            let kinds = [Kind::AgentDone, Kind::NeedsYou, Kind::Approval, Kind::BuildFailed, Kind::Calendar, Kind::Download, Kind::Memory, Kind::Usage, Kind::Update];
            for i in 0..40 {
                c.push(Alert {
                    at: alerts::now() - i * 997,
                    kind: kinds[i as usize % kinds.len()],
                    text: format!("alert {i} {}", if i % 3 == 0 { "中文 🙂 a long one ".repeat(6) } else { String::new() }),
                    app: [None, Some("ai".to_string()), Some("agents".to_string()), Some("nope".to_string())][i as usize % 4].clone(),
                    read: i % 2 == 0,
                    pane: if i % 5 == 0 { Some(i as u64) } else { None },
                });
            }
        }
        mash_pane("alerts", Opts { side: false, ..Default::default() }, |_| panes::alerts::Alerts::new(), |_, ev| {
            // refill now and then so x / c don't leave it empty for the rest of the run
            if matches!(ev, Ev::Key(k) if k.code == KeyCode::F(7)) {
                let mut c = alerts::CENTER.lock().unwrap();
                for i in 0..5 {
                    c.push(Alert { at: alerts::now(), kind: Kind::AgentDone, text: format!("refill {i}"), app: Some("music".into()), read: false, pane: None });
                }
            }
            true
        });
    }
}
