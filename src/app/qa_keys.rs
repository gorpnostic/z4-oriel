//! QA key-mash for the whole App (see src/qa_keys.rs): keys, mouse gestures, pastes, focus changes, background
//! alerts, an update notice, clipboard results for panes that may be gone, theme-file changes — with the real event
//! pump (wakes, reaping dead panes, agent tracking, the tour) and full-screen draws at many sizes in between.
//!
//! It runs in a child copy of the test binary: its own ORIEL_DATA_DIR and working folder (chats, notes, calendar,
//! alerts all land in scratch), and PATH = System32 only, so no claude / codex / git / pwsh can be found. Shells
//! are an inert ping (config.shell). The apps that could reach the real machine get stand-ins before the first key
//! and whenever their tab is closed: music (fake library, never plays), system (fake kill, fake data), files
//! (a scratch tree), your AIs (scratch paths), storage and agents (placeholders; they have their own mashes).
//! Held back by the guard: every other way to open a real music / system / files / storage pane (palette,
//! prefix keys, the home launcher), alt+v / ctrl+v (reads the real clipboard via PowerShell), `/` in the chat
//! (its slash menu can reach /save, which writes to Documents) and in help (see qa_keys_help's bug).
//! Stepped around so the mash can find the next bug (each has an #[ignore]d repro below or in its pane):
//! draws that would halve a split one cell thick (layout), draws that would give a terminal pane one row
//! (vt100), and sidebar clicks after a tab came or went since the last frame (stale hit boxes).

use super::*;
use crate::qa_keys::{Probe, Rng, SIZES, bounded, gesture, in_child, key, keys_for, paste_text, run_child, seed_for};
use crossterm::event::{KeyEventKind, KeyEventState};
use ratatui::{Terminal, backend::TestBackend};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Receiver;

/// Apps that must never be opened for real here.
const RISKY: &[&str] = &["music", "system", "files", "storage", "agents", "ais", "claude", "codex"];
/// The ones that get a safe stand-in tab.
const STUBS: &[&str] = &["music", "system", "files", "storage", "agents", "ais"];

/// Where storage / agents would be: they have their own mashes with fakes wired in.
struct Placeholder(&'static str);

impl Pane for Placeholder {
    fn title(&self) -> String {
        format!("{} (qa stand-in)", self.0)
    }
    fn icon(&self) -> &'static str {
        "window"
    }
    fn render(&mut self, f: &mut Frame, area: Rect, _cx: &mut Cx) {
        f.render_widget(Paragraph::new(format!("{} is mashed on its own", self.0)), area);
    }
    fn key(&mut self, _k: KeyEvent, _cx: &mut Cx) -> bool {
        false
    }
}

fn files_tree(d: &Path) {
    for sub in ["src", "docs/deep/er", "ünï 文件"] {
        std::fs::create_dir_all(d.join(sub)).unwrap();
    }
    std::fs::write(d.join("README.md"), "# qa\n\n**hi** `code`\n").unwrap();
    std::fs::write(d.join("src/main.rs"), "fn main() { println!(\"中文 🙂\"); }\n".repeat(50)).unwrap();
    std::fs::write(d.join("docs/deep/er/x.txt"), "deep").unwrap();
}

struct Env {
    root: PathBuf,
    files_title: String,
}

fn stub(env: &Env, name: &str) -> Box<dyn Pane> {
    match name {
        "music" => Box::new(crate::panes::music::qa_keys::fake(40, true)),
        "system" => Box::new(crate::panes::system::qa_keys::safe(true)),
        "files" => Box::new(crate::panes::files::Files::new(Some(env.root.join("files")))),
        "ais" => Box::new(crate::panes::ais::qa_keys::safe(&env.root.join("ais"))),
        "storage" => Box::new(Placeholder("storage")),
        _ => Box::new(Placeholder("agents")),
    }
}

/// Put a stand-in tab back for every risky app that has none (closing an app tab drops it; the next F-key would
/// open the real one).
fn ensure_stubs(app: &mut App, env: &Env) {
    for &name in STUBS {
        if app.tabs.iter().any(|t| t.app == Some(name)) {
            continue;
        }
        let id = app.add(stub(env, name));
        let order = |a: Option<&str>| a.and_then(|n| SIDEBAR.iter().position(|s| s.0 == n)).unwrap_or(usize::MAX);
        let me = order(Some(name));
        let at = app.tabs.iter().position(|t| order(t.app) > me).unwrap_or(app.tabs.len());
        app.tabs.insert(at, Tab { app: Some(name), name: None, root: Node::Leaf(id), focus: id, zoom: false });
        if at <= app.cur && app.tabs.len() > 1 {
            app.cur += 1;
        }
    }
}

/// What `App::run` does after each batch of events.
fn pump(app: &mut App, rx: &Receiver<Event>, env: &Env) {
    let mut n = 0;
    while let Ok(e) = rx.try_recv() {
        app.handle(e);
        n += 1;
        if n > 5000 {
            break;
        }
    }
    app.reap();
    app.track_agents();
    if app.onboard.is_some() {
        let probe = app.probe();
        let out = app.onboard.as_mut().unwrap().check(&probe);
        app.onboard_out(out);
    }
    app.quit = false; // quitting is fine; keep going
    ensure_stubs(app, env);
}

/// Would laying out this split tree in `area` hit the split_rect crash (qa_keys_layout_bug_*: halving a pane one
/// cell thick)? The mash then skips that draw / that click instead of stopping, so it can find the next bug.
fn too_thin(node: &Node, area: Rect) -> bool {
    match node {
        Node::Leaf(_) => false,
        Node::Split { dir, ratio, a, b } => {
            let thin = match dir {
                Dir::Right => area.width == 1,
                Dir::Down => area.height == 1,
            };
            if thin {
                return true;
            }
            let (ra, rb) = split_rect(area, *dir, *ratio);
            too_thin(a, ra) || too_thin(b, rb)
        }
    }
}

/// The body rect App::draw would use on a `w` x `h` screen.
fn body_for(app: &App, w: u16, h: u16) -> Rect {
    let side_w = if app.sidebar && w >= 70 { (w / 5).clamp(26, 36) } else { 0 };
    Rect { x: side_w, y: 0, width: w - side_w, height: h }
}

fn layout_crashes(app: &App, body: Rect) -> bool {
    let t = &app.tabs[app.cur];
    !t.zoom && too_thin(&t.root, body)
}

/// Would this frame squeeze a terminal pane to one row inside its border (panes::term::qa_keys: vt100 panics on
/// the pty reader thread when a line wraps there, and the poisoned lock takes the UI down)? Whether it bites
/// depends on when output arrives, so the mash never draws that frame. Only call after layout_crashes said no.
fn one_row_term(app: &App, body: Rect) -> bool {
    let t = &app.tabs[app.cur];
    let mut rects = vec![];
    if t.zoom {
        rects.push((t.focus, body));
    } else {
        t.root.rects(body, &mut rects);
    }
    rects.iter().any(|(id, r)| r.height == 3 && app.panes.get(id).is_some_and(|p| p.icon() == "term"))
}

fn focused_pane(app: &App) -> Option<&dyn Pane> {
    let t = app.tabs.get(app.cur)?;
    app.panes.get(&t.focus).map(|p| p.as_ref())
}

/// The guard: false = leave this key out (see the module comment for why each one).
fn vet_key(app: &App, env: &Env, k: &KeyEvent) -> bool {
    let (alt, ctrl) = (k.modifiers.contains(KeyModifiers::ALT), k.modifiers.contains(KeyModifiers::CONTROL));
    if matches!(k.code, KeyCode::Char('v' | 'V')) && (alt || ctrl) {
        return false; // clip::grab_image: PowerShell reading the real clipboard
    }
    if let (Some(p), KeyCode::Enter) = (&app.palette, k.code) {
        let m = App::palette_matches(p);
        if let Some(Cmd::Open(name, _)) = m.get(p.sel).map(|&i| &p.items[i].1) {
            if RISKY.contains(name) {
                return false;
            }
        }
    }
    if app.prefix_armed && !app.is_prefix(k) {
        if let KeyCode::Char(c) = k.code {
            if APPS.iter().any(|a| a.1 == c && a.0 != "terminal" && RISKY.contains(&a.0)) {
                return false;
            }
        }
    }
    let Some(p) = focused_pane(app) else { return true };
    let plain = !alt && !ctrl;
    match p.icon() {
        "home" if plain => match k.code {
            KeyCode::Enter => false, // launches the highlighted app, whichever it is
            KeyCode::Char(c) => !APPS.iter().any(|a| a.1 == c && RISKY.contains(&a.0)),
            _ => true,
        },
        "ai" | "search" => k.code != KeyCode::Char('/'),
        // your AIs: no popups (they crash below 64 columns) and no install view (it crashes below ~14)
        "gauge" if plain => !matches!(k.code, KeyCode::Char('c' | 'x' | 'a' | 'i' | 'l' | '1'..='4') | KeyCode::Enter | KeyCode::Tab | KeyCode::BackTab),
        "files" if plain => match k.code {
            KeyCode::Char('o' | 'p' | '~') => false, // Explorer, the real clipboard, the real home folder
            KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => p.title() != env.files_title,
            _ => true,
        },
        _ => true,
    }
}

/// Vetted right before it's handled (after the gesture's earlier events). `drawn_tabs`: how many tabs the last
/// drawn frame had, i.e. what the sidebar's hit boxes still describe.
fn vet_mouse(app: &App, m: &mut MouseEvent, drawn_tabs: usize) -> bool {
    let pos = Position { x: m.column, y: m.row };
    // a tab came or went since that frame: a click in the sidebar (everything left of the body) would use stale
    // tab numbers (qa_keys_app_bug_double_click_last_tab_close, qa_keys_app_bug_click_closed_tab_row)
    if pos.x < app.body.x && app.tabs.len() != drawn_tabs && matches!(m.kind, MouseEventKind::Down(_)) {
        return false;
    }
    if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) && layout_crashes(app, app.body) {
        return false; // the divider hit-test lays the tree out again (split_rect crash)
    }
    if let MouseEventKind::Down(MouseButton::Left) = m.kind {
        let under = app.outer.iter().find(|(_, r)| r.contains(pos)).and_then(|(id, _)| app.panes.get(id));
        if under.is_some_and(|p| p.icon() == "home") {
            m.kind = MouseEventKind::Moved; // a click on the launcher opens that app
        }
    }
    if let Some((_, r)) = app.side_area {
        // the files app's places are the real home, desktop and drives
        // (and your AIs' view list: see vet_key)
        if r.contains(pos) && matches!(app.tabs.get(app.cur).and_then(|t| t.app), Some("files" | "ais")) && matches!(m.kind, MouseEventKind::Down(_)) {
            return false;
        }
    }
    true
}

fn mash_app(name: &'static str, theme: &'static str, tour: bool, keys: usize) {
    bounded(name, Duration::from_secs(330), move |probe: Arc<Probe>| {
        let root = std::env::current_dir().unwrap(); // run_child's scratch folder
        crate::theme::TEST_DIR.with(|d| *d.borrow_mut() = Some(root.join("themes")));
        files_tree(&root.join("files"));
        std::fs::create_dir_all(root.join("ais")).unwrap();
        let env = Env { files_title: crate::panes::files::Files::new(Some(root.join("files"))).title(), root: root.clone() };
        let mut cfg = Config::default();
        cfg.theme = theme.into();
        cfg.shell = r"C:\Windows\System32\PING.EXE -n 8 127.0.0.1".into(); // an inert "shell": ignores typing, exits by itself
        cfg.music.folders = vec![root.join("music").to_string_lossy().into_owned()];
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(cfg, tx);
        if tour {
            app.start_tour();
        }
        ensure_stubs(&mut app, &env);
        let mut r = Rng::new(seed_for(name));
        let (mut w, mut h) = (150u16, 44u16);
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let slow = Duration::from_secs(5);
        let mut skipped = 0usize;
        let drawn_tabs = std::cell::Cell::new(app.tabs.len());
        let mut draw = |app: &mut App, term: &mut Terminal<TestBackend>, step: usize, probe: &Probe, w: u16, h: u16| {
            if layout_crashes(app, body_for(app, w, h)) {
                skipped += 1;
                probe.note(step, format!("(skipped draw {w}x{h}: a split one cell thick, the known split_rect crash)"));
                return;
            }
            if one_row_term(app, body_for(app, w, h)) {
                skipped += 1;
                probe.note(step, format!("(skipped draw {w}x{h}: a terminal pane one row tall, the known vt100 crash)"));
                return;
            }
            probe.note(step, format!("draw {w}x{h}"));
            let t = Instant::now();
            term.draw(|f| app.draw(f)).unwrap();
            drawn_tabs.set(app.tabs.len());
            assert!(t.elapsed() < slow, "SLOW: a draw at {w}x{h} took {:.1}s at step {step}", t.elapsed().as_secs_f64());
        };
        draw(&mut app, &mut term, 0, &probe, w, h);
        let (mut sent, mut step, mut dropped) = (0usize, 0usize, 0usize);
        while sent < keys {
            step += 1;
            if r.pct(3) {
                (w, h) = r.pick(SIZES);
                term = Terminal::new(TestBackend::new(w, h)).unwrap();
                draw(&mut app, &mut term, step, &probe, w, h);
            }
            let mut evs: Vec<Event> = vec![];
            match r.below(100) {
                0..=69 => {
                    let mut k = key(&mut r);
                    k.kind = match r.below(100) {
                        0..=2 => KeyEventKind::Repeat,
                        3..=4 => KeyEventKind::Release,
                        _ => KeyEventKind::Press,
                    };
                    k.state = KeyEventState::NONE;
                    if vet_key(&app, &env, &k) {
                        sent += 1;
                        evs.push(Event::Input(CEvent::Key(k)));
                    } else {
                        dropped += 1;
                        probe.note(step, format!("(guard dropped {k:?})"));
                    }
                }
                70..=85 => evs.extend(gesture(&mut r, 0, 0, w, h).into_iter().map(|m| Event::Input(CEvent::Mouse(m)))), // vetted below
                86..=87 => evs.push(Event::Input(CEvent::Paste(paste_text(&mut r, false)))),
                88 => evs.push(Event::Input(if r.pct(50) { CEvent::FocusLost } else { CEvent::FocusGained })),
                89 => {
                    let kinds = [crate::alerts::Kind::AgentDone, crate::alerts::Kind::NeedsYou, crate::alerts::Kind::Memory, crate::alerts::Kind::Usage, crate::alerts::Kind::Calendar];
                    evs.push(Event::Alert(r.pick(&kinds), format!("qa alert {step} 中文 🙂")));
                }
                90 => evs.push(Event::UpdateAvailable(format!("9.{}.0", r.below(9)))),
                91 => {
                    // a clipboard grab finishing for a pane that may be long gone
                    let id = if r.pct(60) { app.focused() } else { r.below(400) as u64 };
                    let got = if r.pct(70) { Some(vec![r"C:\qa\paste-1.png".to_string(), r"C:\qa dir\two files.txt".to_string()]) } else { None };
                    let k = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT);
                    evs.push(Event::Clipboard(id, got, k));
                }
                92 => evs.push(Event::ThemeFilesChanged),
                93..=96 => evs.push(Event::Tick),
                97 => evs.push(Event::Input(CEvent::Resize(r.below(300) as u16, r.below(100) as u16))),
                _ => {}
            }
            for mut e in evs {
                if let Event::Input(CEvent::Mouse(m)) = &mut e {
                    if !vet_mouse(&app, m, drawn_tabs.get()) {
                        probe.note(step, format!("(guard dropped {m:?})"));
                        continue;
                    }
                }
                probe.note(step, format!("{} @ {w}x{h} (tab {} of {}, panes {})", describe(&e), app.cur, app.tabs.len(), app.panes.len()));
                let t = Instant::now();
                app.handle(e);
                pump(&mut app, &rx, &env);
                assert!(t.elapsed() < slow, "SLOW: an event took {:.1}s at step {step}", t.elapsed().as_secs_f64());
            }
            if r.pct(60) {
                draw(&mut app, &mut term, step, &probe, w, h);
            }
        }
        for &(w, h) in SIZES {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            draw(&mut app, &mut term, step, &probe, w, h);
        }
        let mut term = Terminal::new(TestBackend::new(150, 44)).unwrap();
        draw(&mut app, &mut term, step, &probe, 150, 44);
        crate::testkit::save_html(term.backend().buffer(), &root.join(format!("app-{name}.html")).to_string_lossy());
        println!("---- qa {name}: {sent} keys sent, {dropped} held back, {skipped} draws skipped, {step} steps, {} tabs, {} panes", app.tabs.len(), app.panes.len());
    });
}

fn describe(e: &Event) -> String {
    match e {
        Event::Input(i) => format!("{i:?}"),
        Event::Wake(id) => format!("Wake({id})"),
        Event::ThemeFilesChanged => "ThemeFilesChanged".into(),
        Event::Tick => "Tick".into(),
        Event::Clipboard(id, got, _) => format!("Clipboard({id}, {got:?})"),
        Event::UpdateAvailable(v) => format!("UpdateAvailable({v})"),
        Event::Alert(k, s) => format!("Alert({k:?}, {s})"),
    }
}

const SYS32: &str = r"C:\Windows\System32";

/// qa_keys_help_bug_move_after_search_with_no_match through the real key path: F10, `/`, a query no topic
/// matches, enter (the box closes, the empty result stays), ↓.
#[test]
#[ignore = "fails: in the app, F10 / zzqqxx enter ↓ panics (index out of bounds, help.rs:414)"]
fn qa_keys_app_bug_help_search_no_match() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(Config::default(), tx);
    let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
    let keys = [KeyCode::F(10), KeyCode::Char('/')].into_iter().chain("zzqqxx".chars().map(KeyCode::Char)).chain([KeyCode::Enter, KeyCode::Down]);
    for code in keys {
        app.key(KeyEvent::new(code, KeyModifiers::NONE));
        term.draw(|f| app.draw(f)).unwrap();
    }
}

/// An App with three tabs of your own (home screens: nothing starts), drawn once at 120x40 so the sidebar's
/// hit boxes exist. Returns the app, the screen, and the last tab's sidebar row and its × (sidebar hit boxes).
fn three_tabs() -> (App, Terminal<TestBackend>, Rect, Rect) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::mem::forget(rx); // keep the channel open for the app's wakes
    let mut app = App::new(Config::default(), tx);
    app.new_tab(Box::new(crate::panes::home::Home::new()));
    app.new_tab(Box::new(crate::panes::home::Home::new()));
    let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
    term.draw(|f| app.draw(f)).unwrap();
    let last = app.tabs.len() - 1;
    let find = |want: fn(&SideHit, usize) -> bool| app.side_hits.iter().find(|(_, h)| want(h, last)).map(|(r, _)| *r);
    let row = find(|h, l| matches!(h, SideHit::Tab(i) if *i == l)).expect("the last tab's row in the sidebar");
    let x = find(|h, l| matches!(h, SideHit::CloseTab(i) if *i == l)).expect("the last tab's × in the sidebar");
    (app, term, row, x)
}

fn click(app: &mut App, r: Rect, b: MouseButton) {
    for kind in [MouseEventKind::Down(b), MouseEventKind::Up(b)] {
        app.handle(Event::Input(CEvent::Mouse(MouseEvent { kind, column: r.x, row: r.y, modifiers: KeyModifiers::NONE })));
    }
}

/// Found by qa_keys_app_tour (QA_SEED=13 QA_KEYS=20000). The sidebar's hit boxes come from the last frame, and
/// App::run handles a burst of queued input before it redraws (app.rs:469). A double-click on the last tab's ×
/// that arrives in one burst (oriel was busy for a moment) closes the tab, then closes "tab n" again with n now
/// past the end: close_tab indexes self.tabs[n] and oriel exits. Cmd::CloseTab checks the index (app.rs:1014);
/// the sidebar's × (app.rs:1229) and middle-click (app.rs:1208) call close_tab without that check. On any other
/// tab the same double-click silently closes a second tab, the one that slid into its place.
#[test]
#[ignore = "fails: two clicks on the last tab's sidebar × before a redraw panic (index out of bounds, app.rs:598): app.rs:1229/1208 skip the bound check of app.rs:1014"]
fn qa_keys_app_bug_double_click_last_tab_close() {
    let (mut app, _term, _row, x) = three_tabs();
    let n = app.tabs.len();
    click(&mut app, x, MouseButton::Left);
    assert_eq!(app.tabs.len(), n - 1, "the first click closed the tab");
    click(&mut app, x, MouseButton::Left); // same burst, no frame in between
}

/// The same stale hit boxes, selecting instead of closing: close the last tab with its ×, then click its old row
/// in the same burst. SideHit::Tab sets self.cur = n (app.rs:1224) with no check, and the next frame's first
/// self.tabs[self.cur] (draw_sidebar, app.rs:1433; app.rs:1328 without the sidebar) panics.
#[test]
#[ignore = "fails: closing the last tab then clicking its old sidebar row before a redraw sets cur past the end (app.rs:1224); the next draw panics (app.rs:1433)"]
fn qa_keys_app_bug_click_closed_tab_row() {
    let (mut app, mut term, row, x) = three_tabs();
    click(&mut app, x, MouseButton::Left);
    click(&mut app, row, MouseButton::Left);
    term.draw(|f| app.draw(f)).unwrap();
}

/// The layout crash (see qa_keys::tests::qa_keys_layout_bug_*) through the real key path: on an 80x24 screen,
/// ctrl+space then `-` five times. Each split-down opens a terminal (here the inert ping) in a pane half as tall;
/// the fifth halves a pane one row tall and the next frame panics.
#[test]
#[ignore = "fails: 5x ctrl+space - on 80x24 panics in layout::split_rect (clamp min > max, layout.rs:148)"]
fn qa_keys_app_bug_split_down_five_times() {
    if !cfg!(windows) {
        return;
    }
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut cfg = Config::default();
    cfg.shell = r"C:\Windows\System32\PING.EXE -n 3 127.0.0.1".into();
    let mut app = App::new(cfg, tx);
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| app.draw(f)).unwrap();
    for n in 1..=5 {
        app.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL));
        app.key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE));
        println!("split {n}: {} panes", app.panes.len());
        term.draw(|f| app.draw(f)).unwrap();
    }
}

#[test]
fn qa_keys_app() {
    if !cfg!(windows) {
        return; // the stand-ins and the restricted PATH are written for Windows
    }
    run_child("app::qa_keys::qa_keys_app_child", "app", &[("PATH", SYS32)], Duration::from_secs(420));
}

#[test]
#[ignore = "child process: run by qa_keys_app"]
fn qa_keys_app_child() {
    if !in_child() {
        return;
    }
    assert!(crate::config::which("claude").is_none() && crate::config::which("codex").is_none(), "PATH still finds an agent CLI");
    mash_app("app", "ultra", false, keys_for(3000));
}

#[test]
fn qa_keys_app_tour() {
    if !cfg!(windows) {
        return;
    }
    run_child("app::qa_keys::qa_keys_app_tour_child", "app-tour", &[("PATH", SYS32)], Duration::from_secs(420));
}

#[test]
#[ignore = "child process: run by qa_keys_app_tour"]
fn qa_keys_app_tour_child() {
    if !in_child() {
        return;
    }
    assert!(crate::config::which("claude").is_none() && crate::config::which("codex").is_none(), "PATH still finds an agent CLI");
    mash_app("app-tour", "oriel", true, keys_for(2500));
}
