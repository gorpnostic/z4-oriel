//! QA: first run and profiles. Drives the onboarding flow (onboard.rs) page by page and step by step with keys,
//! mouse clicks and synthetic probes, draws every stage headlessly at many sizes, round-trips config.toml
//! through the same serde path `config::load` / `config::save` use, and checks where an ORIEL_DATA_DIR profile
//! keeps things (in a child copy of this test binary, so the shared process's environment is never touched).
//! App-level onboarding tests (which need the App's private state) live in qa_first_run_app.rs, a child module
//! of app.rs.
//!
//! Everything is headless: TestBackend only, theme files come from a scratch folder (theme::TEST_DIR), the
//! music/notes folders point into target/test-scratch/qa-first-run/, the Ollama probe points at a closed port,
//! and nothing is saved (config::save and onboard::mark_done are no-ops under cfg(test)).

#![cfg(test)]

use crate::config::{Config, LeadConfig, RosterEntry};
use crate::onboard::{Onboard, Out, Probe, Stage};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};
use std::path::{Path, PathBuf};

// ------------------------------------------------------------------ helpers

/// A fresh scratch folder for one test (wiped first).
pub(crate) fn scratch(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-scratch").join("qa-first-run").join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A config that keeps everything the onboarding touches inside `dir`: an empty themes folder (so the theme list
/// is just the built-ins), a music folder with three songs, a notes folder, no real Ollama, and two API "keys" so
/// the default-AI row always has at least two choices.
pub(crate) fn cfg_in(dir: &Path) -> Config {
    let themes = dir.join("themes");
    std::fs::create_dir_all(&themes).unwrap();
    crate::theme::TEST_DIR.with(|d| *d.borrow_mut() = Some(themes));
    let music = dir.join("music");
    std::fs::create_dir_all(music.join("sub")).unwrap();
    for f in ["a.mp3", "b.FLAC", "notes.txt", "sub/c.ogg"] {
        std::fs::write(music.join(f), b"x").unwrap();
    }
    std::fs::create_dir_all(dir.join("notes")).unwrap();
    let mut c = Config::default();
    c.theme = "ultra".into();
    c.startup = "home".into();
    c.music.folders = vec![music.to_string_lossy().to_string()];
    c.notes_folder = dir.join("notes").to_string_lossy().to_string();
    c.ai.ollama_url = "http://127.0.0.1:1".into(); // closed port: no real Ollama is probed
    c.ai.openai_key = "test-key".into();
    c.ai.anthropic_key = "test-key".into();
    c
}

fn new_ob(name: &str) -> (PathBuf, Config, Onboard) {
    let d = scratch(name);
    let cfg = cfg_in(&d);
    let ob = Onboard::new(&cfg.theme, &cfg);
    (d, cfg, ob)
}

fn k(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn press(ob: &mut Onboard, code: KeyCode) -> (bool, String) {
    let (used, out) = ob.key(k(code), &Probe::default());
    (used, out_str(&out))
}

fn out_str(o: &Out) -> String {
    match o {
        Out::None => "none".into(),
        Out::Theme(n, save) => format!("theme {n} save={save}"),
        Out::Icons(b) => format!("icons {b}"),
        Out::MusicFolder(p) => format!("music {p}"),
        Out::NotesFolder(p) => format!("notes {p}"),
        Out::Defaults { startup, provider } => format!("defaults {startup} {provider}"),
        Out::Finished => "finished".into(),
    }
}

fn stage(ob: &Onboard) -> String {
    match &ob.stage {
        Stage::Welcome => "welcome".into(),
        Stage::Theme { sel } => format!("theme {sel}"),
        Stage::Icons => "icons".into(),
        Stage::Music { .. } => "music".into(),
        Stage::Notes { .. } => "notes".into(),
        Stage::Defaults { row, start, ai } => format!("defaults {row} {start} {ai}"),
        Stage::Ais => "ais".into(),
        Stage::Tour { step, .. } => format!("tour {step}"),
    }
}

fn step(ob: &Onboard) -> Option<usize> {
    if let Stage::Tour { step, .. } = &ob.stage { Some(*step) } else { None }
}

fn tour_at(ob: &mut Onboard, i: usize, start: &Probe) {
    ob.stage = Stage::Tour { step: i, start: start.clone() };
}

/// Draw the onboarding alone at w x h; returns the screen as text (rows trimmed) and the buffer.
fn draw(ob: &mut Onboard, w: u16, h: u16) -> (String, Buffer) {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    let t = crate::theme::get("ultra");
    term.draw(|f| {
        let a = f.area();
        ob.draw(f, a, &t, 1.0)
    })
    .unwrap();
    let buf = term.backend().buffer().clone();
    (text(&buf), buf)
}

fn text(buf: &Buffer) -> String {
    (0..buf.area.height).map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n")
}

/// Where `pat` (single-width chars) starts on screen.
fn find(buf: &Buffer, pat: &str) -> Option<(u16, u16)> {
    let pc: Vec<String> = pat.chars().map(|c| c.to_string()).collect();
    for y in 0..buf.area.height {
        let row: Vec<&str> = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        'x: for x0 in 0..row.len() {
            if x0 + pc.len() > row.len() {
                break;
            }
            for (i, c) in pc.iter().enumerate() {
                if row[x0 + i] != c {
                    continue 'x;
                }
            }
            return Some((x0 as u16, y));
        }
    }
    None
}

fn click(ob: &mut Onboard, x: u16, y: u16) -> (bool, String) {
    let (used, out) = ob.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: KeyModifiers::NONE }, &Probe::default());
    (used, out_str(&out))
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ------------------------------------------------------------------ setup pages

/// Enter all the way through: every page shows, each page's choice comes out, and the tour starts.
#[test]
fn setup_forward_with_enter_on_every_page() {
    let (d, cfg, mut ob) = new_ob("forward");
    assert_eq!(stage(&ob), "welcome");
    assert!(ob.is_modal() && ob.animating(), "the welcome is modal and animates");
    assert!(draw(&mut ob, 150, 42).0.contains("take the tour"));

    assert_eq!(press(&mut ob, KeyCode::Enter), (true, "none".into()));
    let themes = crate::theme::names();
    let ultra = themes.iter().position(|t| t == "ultra").unwrap();
    assert_eq!(stage(&ob), format!("theme {ultra}"), "the theme page starts on the theme in use");
    assert!(draw(&mut ob, 150, 42).0.contains("setup · pick a look"));
    assert!(!ob.animating(), "only the welcome animates");

    assert_eq!(press(&mut ob, KeyCode::Enter), (true, "theme ultra save=true".into()));
    assert_eq!(stage(&ob), "icons");
    assert!(draw(&mut ob, 150, 42).0.contains("little pictures"));

    assert_eq!(press(&mut ob, KeyCode::Enter), (true, "icons true".into()));
    assert_eq!(stage(&ob), "music");
    let s = draw(&mut ob, 150, 42).0;
    assert!(s.contains("setup · your music"), "{s}");
    assert!(s.contains("3 songs found"), "a.mp3, b.FLAC and sub/c.ogg count, notes.txt doesn't:\n{s}");

    let music = cfg.music.folders[0].clone();
    assert_eq!(press(&mut ob, KeyCode::Enter), (true, format!("music {music}")));
    assert_eq!(stage(&ob), "notes");
    let s = draw(&mut ob, 200, 42).0;
    assert!(s.contains("setup · your notes"), "{s}");

    // the default notes folder is kept as is: nothing to save
    assert_eq!(press(&mut ob, KeyCode::Enter), (true, "none".into()));
    assert!(stage(&ob).starts_with("defaults 0 0 0"), "{}", stage(&ob));
    assert!(draw(&mut ob, 150, 42).0.contains("open oriel on"));

    let avail = crate::panes::chat::providers::available(&cfg.ai);
    assert!(avail.len() >= 2, "two API keys make at least two AIs available: {avail:?}");
    assert_eq!(press(&mut ob, KeyCode::Enter), (true, format!("defaults ai {}", avail[0])));
    assert_eq!(stage(&ob), "ais");
    let s = draw(&mut ob, 150, 42).0;
    assert!(s.contains("setup · your AIs") && s.contains("Anthropic API"), "{s}");

    let (used, out) = press(&mut ob, KeyCode::Enter);
    assert!(used && out == "none");
    assert_eq!(step(&ob), Some(0), "enter on the last page starts the tour");
    assert!(!ob.is_modal(), "the tour lets the app have the keys");
    let _ = d;
}

/// Esc on each page moves on without saving anything (and the theme preview is undone).
#[test]
fn setup_esc_on_every_page_keeps_everything() {
    let (_d, _cfg, mut ob) = new_ob("esc");
    press(&mut ob, KeyCode::Enter);
    // preview two themes down, then esc: back to the one we started with, not saved
    let (_, o) = press(&mut ob, KeyCode::Down);
    assert!(o.ends_with("save=false"), "moving previews without saving: {o}");
    press(&mut ob, KeyCode::Down);
    assert_eq!(press(&mut ob, KeyCode::Esc), (true, "theme ultra save=false".into()));
    assert_eq!(stage(&ob), "icons");
    assert_eq!(press(&mut ob, KeyCode::Esc), (true, "none".into()));
    assert_eq!(stage(&ob), "music");
    assert_eq!(press(&mut ob, KeyCode::Esc), (true, "none".into()));
    assert_eq!(stage(&ob), "notes");
    assert_eq!(press(&mut ob, KeyCode::Esc), (true, "none".into()));
    assert!(stage(&ob).starts_with("defaults"));
    assert_eq!(press(&mut ob, KeyCode::Esc), (true, "none".into()));
    assert_eq!(stage(&ob), "ais");
    assert_eq!(press(&mut ob, KeyCode::Esc), (true, "finished".into()), "esc on the last page ends the setup without the tour");
}

/// The welcome: s or esc skip everything, enter goes on, other keys (and ctrl+s) do nothing but are swallowed.
#[test]
fn welcome_skip_and_stray_keys() {
    for code in [KeyCode::Char('s'), KeyCode::Esc] {
        let (_d, _c, mut ob) = new_ob("welcome-skip");
        assert_eq!(press(&mut ob, code), (true, "finished".into()));
    }
    let (_d, _c, mut ob) = new_ob("welcome-stray");
    for code in [KeyCode::Char('x'), KeyCode::Char('q'), KeyCode::Up, KeyCode::F(1), KeyCode::Tab, KeyCode::Backspace] {
        assert_eq!(press(&mut ob, code), (true, "none".into()), "stray keys are swallowed while the welcome is up");
        assert_eq!(stage(&ob), "welcome");
    }
    let (used, out) = ob.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL), &Probe::default());
    assert!(used && matches!(out, Out::None), "ctrl+s is not the skip key");
    let (used, out) = ob.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::ALT), &Probe::default());
    assert!(used && matches!(out, Out::None), "alt+esc is not the skip key");
    // the ais page's skip has the same rule
    ob.stage = Stage::Ais;
    let (_, out) = ob.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL), &Probe::default());
    assert!(matches!(out, Out::None));
    assert_eq!(press(&mut ob, KeyCode::Char('s')), (true, "finished".into()));
}

/// The theme list: up at the top and down at the bottom clamp, j/k/tab work, enter saves the highlighted one.
#[test]
fn theme_page_moves_clamps_and_saves() {
    let (_d, _c, mut ob) = new_ob("theme");
    let themes = crate::theme::names();
    ob.stage = Stage::Theme { sel: 0 };
    assert_eq!(press(&mut ob, KeyCode::Up), (true, format!("theme {} save=false", themes[0])), "up at the top stays put");
    assert_eq!(press(&mut ob, KeyCode::Char('k')), (true, format!("theme {} save=false", themes[0])));
    assert_eq!(press(&mut ob, KeyCode::Char('j')), (true, format!("theme {} save=false", themes[1])));
    assert_eq!(press(&mut ob, KeyCode::Tab), (true, format!("theme {} save=false", themes[2])));
    for _ in 0..themes.len() + 3 {
        press(&mut ob, KeyCode::Down);
    }
    assert_eq!(stage(&ob), format!("theme {}", themes.len() - 1), "down at the bottom stays put");
    let s = draw(&mut ob, 150, 42).0;
    for t in &themes {
        assert!(s.contains(t.as_str()), "every theme is listed ({t}):\n{s}");
    }
    assert_eq!(press(&mut ob, KeyCode::Enter), (true, format!("theme {} save=true", themes[themes.len() - 1])));
    assert_eq!(stage(&ob), "icons");
}

#[test]
fn icons_page_yes_no() {
    let (_d, _c, mut ob) = new_ob("icons");
    ob.stage = Stage::Icons;
    assert_eq!(press(&mut ob, KeyCode::Char('x')), (true, "none".into()));
    assert_eq!(stage(&ob), "icons");
    assert_eq!(press(&mut ob, KeyCode::Char('n')), (true, "icons false".into()));
    assert_eq!(stage(&ob), "music");
    ob.stage = Stage::Icons;
    assert_eq!(press(&mut ob, KeyCode::Char('y')), (true, "icons true".into()));
    assert_eq!(stage(&ob), "music");
}

/// Typing on the music page re-counts songs, ctrl+u clears, tab completes a folder, a missing folder is refused.
#[test]
fn music_page_typing_counting_and_completion() {
    let (d, cfg, mut ob) = new_ob("music");
    let music = cfg.music.folders[0].clone();
    // start from the full path plus one stray letter so only the scratch folder is ever counted, never a parent
    ob.stage = Stage::Music { input: format!("{music}x"), found: None };
    press(&mut ob, KeyCode::Backspace);
    match &ob.stage {
        Stage::Music { input, found } => {
            assert_eq!(input, &music);
            assert_eq!(*found, Some(3));
        }
        _ => panic!("left the music page"),
    }
    // a folder that isn't there
    press(&mut ob, KeyCode::Char('z'));
    assert!(matches!(&ob.stage, Stage::Music { found: None, .. }));
    assert!(draw(&mut ob, 150, 42).0.contains("that folder doesn't exist"));
    // ctrl+u clears the field
    ob.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL), &Probe::default());
    assert!(matches!(&ob.stage, Stage::Music { input, .. } if input.is_empty()));
    // alt+letters are not typed
    ob.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT), &Probe::default());
    assert!(matches!(&ob.stage, Stage::Music { input, .. } if input.is_empty()));
    // tab completion: "<scratch>\mu" -> "<scratch>\music\" (the only folder starting with "mu")
    let sep = std::path::MAIN_SEPARATOR;
    ob.stage = Stage::Music { input: format!("{}{sep}MU", d.display()), found: None };
    press(&mut ob, KeyCode::Tab);
    match &ob.stage {
        Stage::Music { input, found } => {
            assert_eq!(input, &format!("{}{sep}music{sep}", d.display()), "completes case-insensitively and adds the separator");
            assert_eq!(*found, Some(3));
        }
        _ => panic!(),
    }
    // two folders sharing a prefix complete to the common part only
    std::fs::create_dir_all(d.join("music").join("rock-old")).unwrap();
    std::fs::create_dir_all(d.join("music").join("rock-new")).unwrap();
    ob.stage = Stage::Music { input: format!("{music}{sep}ro"), found: None };
    press(&mut ob, KeyCode::Tab);
    assert!(matches!(&ob.stage, Stage::Music { input, .. } if *input == format!("{music}{sep}rock-")), "{}", match &ob.stage { Stage::Music { input, .. } => input.clone(), _ => String::new() });
    // enter on a folder that doesn't exist moves on but saves nothing
    ob.stage = Stage::Music { input: format!("{music}-missing"), found: None };
    assert_eq!(press(&mut ob, KeyCode::Enter), (true, "none".into()));
    assert_eq!(stage(&ob), "notes");
    // surrounding spaces are trimmed off a good folder
    ob.stage = Stage::Music { input: format!("  {music}  "), found: None };
    assert_eq!(press(&mut ob, KeyCode::Enter), (true, format!("music {music}")));
}

#[test]
fn notes_page_custom_folder() {
    let (d, cfg, mut ob) = new_ob("notes");
    ob.stage = Stage::Notes { input: cfg.notes_folder.clone() };
    ob.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL), &Probe::default());
    let want = d.join("vault").to_string_lossy().to_string();
    for c in want.chars() {
        press(&mut ob, KeyCode::Char(c));
    }
    let s = draw(&mut ob, 150, 42).0;
    assert!(s.contains("vault"), "{s}");
    assert_eq!(press(&mut ob, KeyCode::Enter), (true, format!("notes {want}")));
    // an emptied field keeps the default rather than saving ""
    ob.stage = Stage::Notes { input: String::new() };
    assert_eq!(press(&mut ob, KeyCode::Enter), (true, "none".into()));
}

/// The defaults page: both rows cycle and wrap, and every "open oriel on" choice is something the app can open.
#[test]
fn defaults_page_rows_cycle_and_wrap() {
    let (_d, cfg, mut ob) = new_ob("defaults");
    let avail = crate::panes::chat::providers::available(&cfg.ai);
    ob.stage = Stage::Defaults { row: 0, start: 0, ai: 0 };
    press(&mut ob, KeyCode::Left);
    assert_eq!(stage(&ob), "defaults 0 4 0", "left from the first start choice wraps to the last");
    press(&mut ob, KeyCode::Char('l'));
    assert_eq!(stage(&ob), "defaults 0 0 0");
    press(&mut ob, KeyCode::Down);
    press(&mut ob, KeyCode::Right);
    assert_eq!(stage(&ob), "defaults 1 0 1");
    press(&mut ob, KeyCode::Tab);
    assert_eq!(stage(&ob), "defaults 0 0 1", "tab toggles the row back");
    press(&mut ob, KeyCode::Up);
    for _ in 0..avail.len() {
        press(&mut ob, KeyCode::Char('h'));
    }
    assert_eq!(stage(&ob), "defaults 1 0 1", "a full lap of the AI row comes back round");
    assert_eq!(press(&mut ob, KeyCode::Enter), (true, format!("defaults ai {}", avail[1])));
    // every start choice
    let mut starts = vec![];
    for i in 0..5 {
        ob.stage = Stage::Defaults { row: 0, start: 0, ai: 0 };
        for _ in 0..i {
            press(&mut ob, KeyCode::Right);
        }
        let (_, o) = press(&mut ob, KeyCode::Enter);
        starts.push(o.split(' ').nth(1).unwrap().to_string());
    }
    assert_eq!(starts, ["ai", "agents", "home", "terminal", "music"]);
    for s in &starts {
        let ok = crate::app::SIDEBAR.iter().any(|a| a.0 == s) || s == "home";
        assert!(ok, "startup {s} must be an app App::new can open");
    }
    let s = draw(&mut ob, 150, 42).0;
    let _ = s;
}

/// Replaying the setup (palette → take the tour, `oriel --tour`) and pressing enter through the defaults page
/// should keep what you chose last time, like the theme page keeps your theme.
#[test]
#[ignore = "fails: onboard.rs:376-377 (and :382 on esc) always start the defaults page on chat + the first AI, so replaying the setup resets startup/provider"]
fn defaults_page_preselects_current_config() {
    let d = scratch("defaults-preselect");
    let mut cfg = cfg_in(&d);
    cfg.startup = "music".into();
    cfg.ai.provider = "anthropic".into();
    // walk welcome -> theme -> icons -> music -> notes -> defaults with enter (esc on welcome would skip)
    let mut ob = Onboard::new(&cfg.theme, &cfg);
    for _ in 0..10 {
        if stage(&ob).starts_with("defaults") {
            break;
        }
        press(&mut ob, KeyCode::Enter);
    }
    assert!(stage(&ob).starts_with("defaults"), "{}", stage(&ob));
    let (_, out) = press(&mut ob, KeyCode::Enter);
    assert_eq!(out, "defaults music anthropic", "enter on an untouched defaults page should keep the current startup and AI");
}

/// A setup wizard should let you go back a page (e.g. you pressed enter on the theme page too early).
#[test]
#[ignore = "fails: onboard.rs:316-415 has no back key on any setup page; esc means skip-forward, so a wrong enter can only be undone by replaying everything"]
fn setup_pages_can_go_back() {
    let (_d, _c, mut ob) = new_ob("back");
    let mut went_back = vec![];
    for code in [KeyCode::BackTab, KeyCode::Left, KeyCode::Backspace] {
        ob.stage = Stage::Icons;
        press(&mut ob, code);
        if stage(&ob).starts_with("theme") {
            went_back.push(format!("{code:?}"));
        }
    }
    assert!(!went_back.is_empty(), "no key goes from icons back to the theme page");
}

// ------------------------------------------------------------------ the tour

/// Every tour step, done for real (as the app's probe would see it): each one waits for its own action, ignores
/// unrelated ones, and the last one finishes.
#[test]
fn tour_every_step_waits_for_its_action() {
    let (_d, _c, mut ob) = new_ob("tour-all");
    let mut p = Probe { app: Some("ai"), user_tabs: 0, panes_in_tab: 1, focus: 1, ..Default::default() };
    tour_at(&mut ob, 0, &p);
    let titles = ["switch apps", "chat with any AI", "your AIs", "a team of agents", "the palette", "a tab of your own", "split it",
        "move between panes", "name the tab", "right-click", "copy, paste, close", "agents look after themselves", "the help screen", "you're set"];
    let mut shown = vec![];
    let check = |ob: &mut Onboard, p: &Probe| out_str(&ob.check(p));
    for (i, title) in titles.iter().enumerate() {
        let s = draw(&mut ob, 150, 42).0;
        assert!(s.contains(&format!("tour · {}/{} · {title}", i + 1, titles.len())), "step {i}:\n{s}");
        shown.push(*title);
        assert_eq!(check(&mut ob, &p), "none", "step {i} ({title}) must wait");
        assert_eq!(step(&ob), Some(i));
        match i {
            0 => {
                p.app = Some("files"); // another app isn't music
                assert_eq!(check(&mut ob, &p), "none");
                p.app = Some("music");
                p.focus = 2;
            }
            1 => {
                p.app = Some("ai");
                p.focus = 1;
            }
            2 => {
                p.app = Some("ais");
                p.focus = 3;
            }
            3 => {
                p.app = Some("agents");
                p.focus = 4;
            }
            4 => {
                p.palette_seen += 1;
                p.palette_open = true;
                assert_eq!(check(&mut ob, &p), "none", "still open: wait for esc");
                assert_eq!(step(&ob), Some(4));
                p.palette_open = false;
            }
            5 => {
                p.focus = 9; // moving focus is not a new tab
                assert_eq!(check(&mut ob, &p), "none");
                p.app = None;
                p.user_tabs += 1;
                p.focus = 5;
            }
            6 => {
                p.tab_named = false;
                p.panes_in_tab = 2;
                p.focus = 6;
            }
            7 => {
                p.focus = 5;
            }
            8 => {
                p.ctx_seen += 1; // a right-click is not a rename
                assert_eq!(check(&mut ob, &p), "none");
                p.tab_named = true;
            }
            9 => {
                p.ctx_seen += 1;
            }
            10 | 11 => {
                assert_eq!(press(&mut ob, KeyCode::Char('x')), (false, "none".into()), "other keys go to the app");
                assert_eq!(press(&mut ob, KeyCode::Enter), (true, "none".into()), "enter reads on");
                continue;
            }
            12 => {
                // F10 here is the app's own key: the tour lets it through and the app opens help
                assert_eq!(press(&mut ob, KeyCode::F(10)), (false, "none".into()));
                p.app = Some("help");
            }
            13 => {
                assert_eq!(press(&mut ob, KeyCode::Enter), (true, "finished".into()), "enter on the last step finishes");
                continue;
            }
            _ => unreachable!(),
        }
        assert_eq!(check(&mut ob, &p), "none", "step {i} ({title}) done: moves on");
        assert_eq!(step(&ob), Some(i + 1), "step {i} ({title}) should have advanced");
    }
    assert_eq!(shown.len(), 14);
}

/// F10 skips every step but the help one (where it is the app's own key); F11 ends the tour from anywhere;
/// enter only reads on for the read-only steps.
#[test]
fn tour_skip_end_and_enter_keys() {
    let (_d, _c, mut ob) = new_ob("tour-keys");
    let p = Probe::default();
    tour_at(&mut ob, 0, &p);
    let mut skipped = 0;
    loop {
        let before = step(&ob).unwrap();
        let (used, out) = press(&mut ob, KeyCode::F(10));
        if !used {
            assert_eq!(before, 12, "only the help step passes F10 through");
            break;
        }
        assert_eq!(out, "none");
        assert_eq!(step(&ob), Some(before + 1));
        skipped += 1;
        assert!(skipped < 20);
    }
    assert_eq!(skipped, 12);
    for i in 0..14 {
        tour_at(&mut ob, i, &p);
        assert_eq!(press(&mut ob, KeyCode::F(11)), (true, "finished".into()), "F11 ends the tour on step {i}");
    }
    for i in [0, 4, 8, 12] {
        tour_at(&mut ob, i, &p);
        assert_eq!(press(&mut ob, KeyCode::Enter), (false, "none".into()), "enter on an action step ({i}) belongs to the app");
        assert_eq!(step(&ob), Some(i));
    }
    // F10 on the second-to-last step, then on the last one: the last skip finishes
    tour_at(&mut ob, 13, &p);
    assert_eq!(press(&mut ob, KeyCode::F(10)), (true, "finished".into()));
}

/// The tour's first step says "Open music: press F4". If oriel already shows music when the tour starts
/// (startup = music, then `oriel --tour`), the step is done before it's ever drawn.
#[test]
#[ignore = "fails: onboard.rs:41 (also :47, :89, :113) done() only looks at the current probe, so a step that is already true passes before it is drawn"]
fn tour_steps_wait_even_if_already_true() {
    let (_d, _c, mut ob) = new_ob("tour-already");
    let on_music = Probe { app: Some("music"), focus: 1, panes_in_tab: 1, ..Default::default() };
    tour_at(&mut ob, 0, &on_music);
    let out = out_str(&ob.check(&on_music));
    assert_eq!((out.as_str(), step(&ob)), ("none", Some(0)), "switch apps completed without pressing anything");
}

/// oriel starts on chat by default. Skipping the first step ("switch apps", F10) while you're on chat must not
/// also skip the second one ("chat with any AI", the only place the tour explains /provider, /model, /perms):
/// App::run checks the probe right after the skip, and "go back to chat" is already true.
#[test]
#[ignore = "fails: onboard.rs:47 step 2's done() is `now.app == ai` with no change since the step began, so it passes the instant step 1 is skipped from chat"]
fn tour_skipping_step_one_from_chat_keeps_step_two() {
    let (_d, _c, mut ob) = new_ob("tour-skip-chat");
    let on_chat = Probe { app: Some("ai"), focus: 1, panes_in_tab: 1, ..Default::default() };
    tour_at(&mut ob, 0, &on_chat);
    let (used, _) = ob.key(k(KeyCode::F(10)), &on_chat);
    assert!(used);
    assert_eq!(step(&ob), Some(1));
    let out = out_str(&ob.check(&on_chat)); // what App::run does after the event
    assert_eq!((out.as_str(), step(&ob)), ("none", Some(1)), "'chat with any AI' was skipped without ever being drawn");
}

// ------------------------------------------------------------------ mouse

#[test]
fn mouse_welcome_buttons() {
    let (_d, _c, mut ob) = new_ob("mouse-welcome");
    let (_, buf) = draw(&mut ob, 150, 42);
    // a click on nothing is swallowed
    assert_eq!(click(&mut ob, 0, 0), (true, "none".into()));
    assert_eq!(stage(&ob), "welcome");
    let (x, y) = find(&buf, "enter  take the tour").expect("tour button drawn");
    assert_eq!(click(&mut ob, x + 2, y), (true, "none".into()));
    assert!(stage(&ob).starts_with("theme"), "the tour button starts the setup");
    // the setup pages have no buttons: a click is swallowed and changes nothing
    draw(&mut ob, 150, 42);
    assert_eq!(click(&mut ob, 75, 21), (true, "none".into()));
    assert!(stage(&ob).starts_with("theme"));

    let (_d, _c, mut ob) = new_ob("mouse-welcome-skip");
    let (_, buf) = draw(&mut ob, 150, 42);
    let (x, y) = find(&buf, "s  skip").expect("skip button drawn");
    assert_eq!(click(&mut ob, x + 1, y), (true, "finished".into()));
}

#[test]
fn mouse_tour_buttons() {
    let (_d, _c, mut ob) = new_ob("mouse-tour");
    let p = Probe::default();
    tour_at(&mut ob, 0, &p);
    let (_, buf) = draw(&mut ob, 150, 42);
    // outside the box the click belongs to the app
    assert_eq!(click(&mut ob, 5, 5), (false, "none".into()));
    let (x, y) = find(&buf, "F10  skip step").expect("skip button");
    assert_eq!(click(&mut ob, x + 1, y), (true, "none".into()));
    assert_eq!(step(&ob), Some(1));
    tour_at(&mut ob, 10, &p);
    let (_, buf) = draw(&mut ob, 150, 42);
    let (x, y) = find(&buf, "enter  next").expect("next button on a read step");
    click(&mut ob, x + 1, y);
    assert_eq!(step(&ob), Some(11));
    tour_at(&mut ob, 12, &p);
    let (_, buf) = draw(&mut ob, 150, 42);
    assert!(find(&buf, "F10  skip step").is_none() && find(&buf, "skip step").is_some(), "the help step's skip isn't labelled F10");
    tour_at(&mut ob, 13, &p);
    let (_, buf) = draw(&mut ob, 150, 42);
    let (x, y) = find(&buf, "enter  finish").expect("finish button on the last step");
    assert_eq!(click(&mut ob, x + 1, y), (true, "finished".into()));
    tour_at(&mut ob, 5, &p);
    let (_, buf) = draw(&mut ob, 150, 42);
    let (x, y) = find(&buf, "F11  end tour").expect("end button");
    assert_eq!(click(&mut ob, x + 1, y), (true, "finished".into()));
}

// ------------------------------------------------------------------ drawing

/// Every stage and every tour step draws at every size down to 1x1 without panicking.
#[test]
fn every_stage_draws_at_any_size() {
    let (d, cfg, mut ob) = new_ob("sizes");
    let long = format!("{}{}", cfg.music.folders[0], "\\very-long-folder-name".repeat(12));
    let mut stages: Vec<(String, Stage)> = vec![
        ("welcome".into(), Stage::Welcome),
        ("theme".into(), Stage::Theme { sel: 3 }),
        ("icons".into(), Stage::Icons),
        ("music".into(), Stage::Music { input: cfg.music.folders[0].clone(), found: Some(3) }),
        ("music-missing".into(), Stage::Music { input: long.clone(), found: None }),
        ("music-empty".into(), Stage::Music { input: String::new(), found: Some(0) }),
        ("notes".into(), Stage::Notes { input: long }),
        ("defaults".into(), Stage::Defaults { row: 1, start: 4, ai: 1 }),
        ("ais".into(), Stage::Ais),
    ];
    for i in 0..14 {
        stages.push((format!("tour{i}"), Stage::Tour { step: i, start: Probe::default() }));
    }
    let sizes = [(150u16, 42u16), (200, 60), (80, 24), (60, 20), (40, 12), (24, 8), (12, 4), (5, 2), (1, 1)];
    let mut fails = vec![];
    for (name, st) in stages {
        ob.stage = st;
        for (w, h) in sizes {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                draw(&mut ob, w, h);
            }));
            if let Err(e) = r {
                let msg = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                fails.push(format!("{name} at {w}x{h}: {msg}"));
            }
        }
        // keep a coloured snapshot of each at the usual size to look at
        let mut term = Terminal::new(TestBackend::new(150, 42)).unwrap();
        let t = crate::theme::get("ultra");
        term.draw(|f| {
            let a = f.area();
            ob.draw(f, a, &t, 1.0)
        })
        .unwrap();
        crate::testkit::save_html(term.backend().buffer(), &d.join(format!("snap-{name}.html")).to_string_lossy());
    }
    assert!(fails.is_empty(), "draw panicked:\n{}", fails.join("\n"));
}

/// At the sizes terminals open at (80x24 is the classic default), every screen shows its title, its content and
/// its key hints: nothing important falls off the bottom.
#[test]
fn every_page_fits_common_terminal_sizes() {
    let (_d, cfg, mut ob) = new_ob("fit");
    let pages: Vec<(Stage, &[&str])> = vec![
        (Stage::Welcome, &["a window onto everything", "enter  take the tour", "s  skip"]),
        (Stage::Theme { sel: 0 }, &["setup · pick a look", "enter use it"]),
        (Stage::Icons, &["setup · icons", "little pictures", "n boxes: plain text"]),
        (Stage::Music { input: cfg.music.folders[0].clone(), found: Some(3) }, &["setup · your music", "3 songs found", "esc skip"]),
        (Stage::Notes { input: cfg.notes_folder.clone() }, &["setup · your notes", "esc keep the default"]),
        (Stage::Defaults { row: 0, start: 0, ai: 0 }, &["setup · defaults", "open oriel on", "default AI for chat", "enter next"]),
        (Stage::Ais, &["setup · your AIs", "Anthropic API", "s skip it"]),
    ];
    let mut bad = vec![];
    for (st, want) in pages {
        ob.stage = st;
        for (w, h) in [(80u16, 24u16), (100, 30), (120, 30), (150, 42)] {
            let (s, _) = draw(&mut ob, w, h);
            for p in want {
                if !squash(&s).contains(&squash(p)) {
                    bad.push(format!("{w}x{h}: '{p}' missing on {}", stage(&ob)));
                }
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// Theme files you've made join the list on the theme page. However many there are, the one you've moved to
/// must stay on screen.
#[test]
#[ignore = "fails: onboard.rs:504-523 lists every theme in two columns with no scrolling, so past ~24 themes at 80x24 the highlighted row is below the page"]
fn theme_page_keeps_the_highlight_on_screen() {
    let d = scratch("many-themes");
    let _ = cfg_in(&d);
    for i in 0..24 {
        std::fs::write(d.join("themes").join(format!("mine-{i:02}.toml")), "base = \"ocean\"\n").unwrap();
    }
    let cfg = cfg_in(&d); // keeps the files: cfg_in only creates folders
    let mut ob = Onboard::new(&cfg.theme, &cfg);
    let themes = crate::theme::names();
    assert!(themes.iter().any(|t| t == "mine-23"), "custom themes are listed: {themes:?}");
    let half = themes.len().div_ceil(2);
    let mut hidden = vec![];
    for sel in [half - 1, themes.len() - 1] {
        ob.stage = Stage::Theme { sel };
        let (s, _) = draw(&mut ob, 80, 24);
        if !s.contains(&format!("▌ ■■ {}", themes[sel])) {
            hidden.push(format!("'{}' (row {} of {half})", themes[sel], sel % half + 1));
        }
    }
    assert!(hidden.is_empty(), "highlighted theme not on screen at 80x24: {}", hidden.join(", "));
}

/// The tour box has to fit its whole text and the "try" line at common terminal widths.
#[test]
fn tour_box_shows_whole_text() {
    let (_d, _c, mut ob) = new_ob("tour-text");
    let bodies = tour_texts(&mut ob);
    let mut cut = vec![];
    for (i, (title, body, keys)) in bodies.iter().enumerate() {
        for w in [150u16, 100, 80, 64, 50, 44] {
            tour_at(&mut ob, i, &Probe::default());
            let (_, buf) = draw(&mut ob, w, 40);
            let inside = squash(&tour_box_text(&buf));
            if !inside.contains(&squash(body)) {
                cut.push(format!("step {} '{title}' at width {w}: body cut, box shows: {inside}", i + 1));
            } else if !keys.is_empty() && !inside.contains(&squash(&format!("try {keys}"))) {
                cut.push(format!("step {} '{title}' at width {w}: 'try {keys}' line hidden, box shows: {inside}", i + 1));
            }
        }
    }
    assert!(cut.is_empty(), "{}", cut.join("\n"));
}

/// Each step's (title, body, keys) as the tour box draws them at a big size (read back off the screen).
fn tour_texts(ob: &mut Onboard) -> Vec<(String, String, String)> {
    let mut v = vec![];
    for i in 0..14 {
        tour_at(ob, i, &Probe::default());
        let mut term = Terminal::new(TestBackend::new(400, 60)).unwrap();
        let t = crate::theme::get("ultra");
        term.draw(|f| {
            let a = f.area();
            ob.draw(f, a, &t, 1.0)
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        let inside = tour_box_rows(&buf);
        let title = text(&buf).lines().find_map(|l| l.split(" · ").nth(2).map(|s| s.trim_end_matches(|c: char| c == '╮' || c == '─' || c == ' ').to_string())).unwrap_or_default();
        // rows: body..., "", "try keys", buttons
        let rows: Vec<String> = inside.iter().map(|r| r.trim().to_string()).collect();
        let body_end = rows.iter().position(|r| r.is_empty() || r.starts_with("try ") || r.contains("skip step") || r.contains("enter  ")).unwrap_or(rows.len());
        let body = rows[..body_end].join(" ");
        let keys = rows.iter().find_map(|r| r.strip_prefix("try ").map(|s| s.trim().to_string())).unwrap_or_default();
        v.push((title, body, keys));
    }
    v
}

/// The rows inside the tour box's border.
fn tour_box_rows(buf: &Buffer) -> Vec<String> {
    let Some((x0, y0)) = find(buf, "╭") else { return vec![] };
    let mut x1 = x0 + 1;
    while x1 < buf.area.width && buf[(x1, y0)].symbol() != "╮" {
        x1 += 1;
    }
    let mut rows = vec![];
    let mut y = y0 + 1;
    while y < buf.area.height && buf[(x0, y)].symbol() != "╰" {
        rows.push((x0 + 1..x1).map(|x| buf[(x, y)].symbol()).collect::<String>());
        y += 1;
    }
    rows
}

fn tour_box_text(buf: &Buffer) -> String {
    tour_box_rows(buf).join(" ")
}

/// The folder boxes on the music and notes pages: the right edge of the text row lines up with the corners,
/// whatever the path's length. (A path long enough to be cut with … happens to come out square; the usual short
/// default, e.g. %APPDATA%\oriel\notes, doesn't.)
#[test]
#[ignore = "fails: onboard.rs:717-724 field() pads a short path's row one column short, so its right │ sits left of the ╮/╯ corners"]
fn input_field_box_is_square() {
    let (_d, cfg, mut ob) = new_ob("field-box");
    let mut bad = vec![];
    for (what, input) in [("short", r"C:\notes".to_string()), ("empty", String::new()), ("long", cfg.notes_folder.repeat(3))] {
        for w in [150u16, 100, 60] {
            ob.stage = Stage::Notes { input: input.clone() };
            let (s, buf) = draw(&mut ob, w, 42);
            let (x0, y0) = find(&buf, "╭─").expect("field top");
            let right_corner = (x0..buf.area.width).find(|&x| buf[(x, y0)].symbol() == "╮").expect("╮");
            let right_bar = (x0 + 1..buf.area.width).rev().find(|&x| buf[(x, y0 + 1)].symbol() == "│").expect("│");
            if right_bar != right_corner {
                bad.push(format!("{what} path at width {w}: row ends at column {right_bar}, corners at {right_corner}\n{}", s.lines().skip(y0 as usize).take(3).collect::<Vec<_>>().join("\n")));
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// Typing a long path: the end you're typing has to stay visible.
#[test]
#[ignore = "fails: onboard.rs:716 field() uses ui::fit, which keeps the start of the path and cuts the end, so what you type disappears"]
fn input_field_shows_the_end_of_a_long_path() {
    let (_d, cfg, mut ob) = new_ob("field-long");
    let long = format!("{}{}END", cfg.notes_folder, "\\deeper".repeat(20));
    ob.stage = Stage::Notes { input: long };
    let (s, _) = draw(&mut ob, 150, 42);
    assert!(s.contains("END▏"), "the end of the typed path (and the cursor after it) should show:\n{s}");
}

/// The music page's song count gives up after 20,000 folder entries. When it does, the count has to say it's
/// partial ("+"), and a keystroke must not stall the UI thread counting.
#[test]
#[ignore = "fails: onboard.rs:190-192 stops at 20,000 entries but :548 only adds + at 20,000 songs; and :368-370 re-walks the folder on the UI thread on every keystroke"]
fn music_count_is_marked_partial_and_quick() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-scratch").join("qa-first-run").join("big-music");
    // 20,100 entries (100 songs), built once and kept between runs
    let marker = dir.join(".built-20100");
    if !marker.exists() {
        let _ = std::fs::remove_dir_all(&dir);
        for b in 0..20 {
            let sub = dir.join(format!("d{b:02}"));
            std::fs::create_dir_all(&sub).unwrap();
            for i in 0..1005 {
                let name = if i < 5 { format!("song{i}.mp3") } else { format!("doc{i}.txt") };
                std::fs::write(sub.join(name), b"").unwrap();
            }
        }
        std::fs::write(&marker, b"").unwrap();
    }
    let d = scratch("big-music-cfg");
    let cfg = cfg_in(&d);
    let mut ob = Onboard::new(&cfg.theme, &cfg);
    let path = dir.to_string_lossy().to_string();
    ob.stage = Stage::Music { input: format!("{path}x"), found: None };
    let t0 = std::time::Instant::now();
    press(&mut ob, KeyCode::Backspace);
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    let found = match &ob.stage {
        Stage::Music { found, .. } => *found,
        _ => None,
    };
    let (s, _) = draw(&mut ob, 150, 42);
    let line = s.lines().find(|l| l.contains("songs found")).unwrap_or("").trim().to_string();
    println!("one keystroke on a 20,100-entry folder: {ms:.0} ms, found {found:?}, shows '{line}'");
    let mut problems = vec![];
    if found.unwrap_or(0) < 100 && !line.contains('+') {
        problems.push(format!("count stopped early at {found:?} of 100 songs but shows '{line}' with no '+'"));
    }
    if ms > 50.0 {
        problems.push(format!("a single keystroke took {ms:.0} ms on the UI thread"));
    }
    assert!(problems.is_empty(), "{}", problems.join("; "));
}

// ------------------------------------------------------------------ config round trips

/// What `config::load` does with a file's text (config.rs:182-186), without touching the real config file.
fn parse_like_load(s: &str) -> Config {
    let mut c: Config = toml::from_str(s).ok().unwrap_or_default();
    if c.theme.is_empty() {
        c.theme = if crate::theme::omarchy_dir().is_some() { "omarchy".into() } else { "ultra".into() };
    }
    c
}

fn dbg(c: &Config) -> String {
    format!("{c:?}")
}

fn default_theme() -> &'static str {
    if crate::theme::omarchy_dir().is_some() { "omarchy" } else { "ultra" }
}

#[test]
fn config_defaults_are_sane() {
    let c = Config::default();
    assert_eq!(c.theme, "", "empty = resolved by load()");
    assert_eq!(parse_like_load("").theme, default_theme(), "no config file: ultra (omarchy on Omarchy)");
    assert_eq!(c.ai.perms, "edits");
    assert_eq!(c.prefix, "ctrl+space");
    assert_eq!(c.startup, "ai");
    assert!(crate::app::SIDEBAR.iter().any(|a| a.0 == c.startup), "the default startup is an app");
    assert!(!c.plain_icons && c.desktop_notifications);
    assert!(c.ai.provider.is_empty() && c.ai.effort.is_empty() && c.ai.models.is_empty());
    assert!(c.ai.openai_key.is_empty() && c.ai.anthropic_key.is_empty(), "no keys by default");
    assert!(c.ai.ollama_url.starts_with("http://127.0.0.1:"), "{}", c.ai.ollama_url);
    assert!(c.ai.openai_url.starts_with("https://"));
    assert!(!c.ai.ollama_model.is_empty() && !c.ai.openai_model.is_empty() && !c.ai.anthropic_model.is_empty());
    assert!(c.music.folders.is_empty() && c.notes_folder.is_empty() && c.roster.is_empty());
    let l = &c.lead;
    assert!((1..=5).contains(&l.max_parallel), "max_parallel {} outside the documented 1-5", l.max_parallel);
    assert!(l.budget_usd > 0.0 && l.run_budget_usd >= l.budget_usd, "the run cap covers at least the lead's own cap");
    assert!(l.gate_timeout_s >= 60 && l.stagger_s < 60);
    let r = RosterEntry::default();
    assert_eq!((r.agent.as_str(), r.tier.as_str(), r.enabled), ("claude", "mid", true));
    assert!(r.max_turns > 0 && r.budget_usd > 0.0 && r.budget_usd <= l.run_budget_usd);
    // the default theme exists and is the animated one
    let t = crate::theme::get(default_theme());
    assert_eq!(t.name, default_theme());
    if default_theme() == "ultra" {
        assert!(t.animated, "ultra is the animated theme");
    }
}

/// A theme name that isn't there (a deleted theme file, a typo, "") falls back to the default look.
#[test]
#[ignore = "fails: theme.rs:100 falls back to PALETTES[0] (\"oriel\"), not the documented default \"ultra\""]
fn unknown_theme_falls_back_to_the_default() {
    let d = scratch("theme-fallback");
    let _ = cfg_in(&d);
    for name in ["", "my-deleted-theme", "Ultra"] {
        assert_eq!(crate::theme::get(name).name, default_theme(), "theme::get({name:?})");
    }
}

#[test]
fn config_round_trips_through_save_format() {
    // the defaults
    let c = Config::default();
    let s = toml::to_string_pretty(&c).expect("defaults serialize");
    assert_eq!(dbg(&toml::from_str::<Config>(&s).unwrap()), dbg(&c), "{s}");
    // every field set, with the awkward values real configs have: Windows paths, quotes, unicode, a per-AI model
    // map (a table in the middle of [ai]), an array of roster tables, fractional budgets
    let mut c = Config::default();
    c.theme = "sakura".into();
    c.shell = r#"pwsh.exe -NoLogo -Command "& { echo 'hi' }""#.into();
    c.prefix = "ctrl+b".into();
    c.startup = "music".into();
    c.plain_icons = true;
    c.desktop_notifications = false;
    c.notes_folder = r"C:\Users\someone\OneDrive\Notes — ünïcödé 📝".into();
    c.ai.provider = "codex".into();
    c.ai.perms = "bypass".into();
    c.ai.effort = "ultracode".into();
    c.ai.models.insert("claude".into(), "opus".into());
    c.ai.models.insert("open ai/odd.key".into(), "gpt \"mini\"".into());
    c.ai.openai_key = r"sk-test\with\backslashes".into();
    c.ai.ollama_url = "http://192.0.2.1:11434".into();
    c.music.folders = vec![r"D:\Music".into(), "/home/x/Music".into(), r"\\nas\share\songs".into()];
    c.lead = LeadConfig { agent: "kimi".into(), model: "k2".into(), budget_usd: 0.1, run_budget_usd: 12.35, max_parallel: 5, protocol: "mcp".into(), gate: "cargo test --all".into(), gate_timeout_s: 60, stagger_s: 0 };
    c.roster = vec![
        RosterEntry { name: "codex".into(), agent: "codex".into(), model: String::new(), tier: "cheap".into(), good_at: "refactors, \"tests\"".into(), max_turns: 0, budget_usd: 0.0, enabled: false },
        RosterEntry::default(),
    ];
    let s = toml::to_string_pretty(&c).expect("a full config must serialize, or config::save silently writes nothing");
    let back: Config = toml::from_str(&s).unwrap_or_else(|e| panic!("{e}\n{s}"));
    assert_eq!(dbg(&back), dbg(&c), "{s}");
    assert_eq!(toml::to_string_pretty(&back).unwrap(), s, "saving twice gives the same file");
    // Windows line endings (a config edited in Notepad)
    let crlf = s.replace('\n', "\r\n");
    assert_eq!(dbg(&toml::from_str::<Config>(&crlf).unwrap()), dbg(&c));
}

#[test]
fn config_missing_fields_take_defaults() {
    let c = parse_like_load("[ai]\nprovider = \"codex\"\n");
    assert_eq!(c.ai.provider, "codex");
    assert_eq!(c.ai.perms, "edits", "a partial [ai] keeps the default perms");
    assert_eq!(c.ai.ollama_url, "http://127.0.0.1:11434");
    assert_eq!(c.prefix, "ctrl+space");
    assert_eq!(c.theme, default_theme());
    assert!(c.desktop_notifications, "a missing bool takes its default (true), not false");

    let c = parse_like_load("[lead]\nmax_parallel = 2\n");
    assert_eq!(c.lead.max_parallel, 2);
    assert_eq!((c.lead.budget_usd, c.lead.run_budget_usd, c.lead.gate_timeout_s, c.lead.stagger_s), (2.0, 8.0, 900, 5));

    let c = parse_like_load("[[roster]]\nname = \"w1\"\n\n[[roster]]\nname = \"w2\"\nagent = \"kimi\"\n");
    assert_eq!(c.roster.len(), 2);
    assert_eq!(c.roster[0], RosterEntry { name: "w1".into(), ..RosterEntry::default() });
    assert_eq!((c.roster[1].agent.as_str(), c.roster[1].enabled, c.roster[1].max_turns), ("kimi", true, 40));

    let c = parse_like_load("theme = \"\"\nstartup = \"notes\"\n");
    assert_eq!(c.theme, default_theme(), "an empty theme resolves like a missing one");
    assert_eq!(c.startup, "notes");
}

#[test]
fn config_unknown_and_old_fields_are_ignored() {
    // fields an older oriel wrote and this one no longer has (two [ai] fields were dropped in 1664f59: a url list
    // and a mode string); perms "full" / "read" are the pre-/perms names
    let old = "theme = \"oriel\"\n\n[ai]\nold_urls = [\"http://example.invalid\"]\nold_mode = \"auto\"\nperms = \"full\"\nprovider = \"claude\"\n";
    let c = parse_like_load(old);
    assert_eq!((c.theme.as_str(), c.ai.perms.as_str(), c.ai.provider.as_str()), ("oriel", "full", "claude"), "an old config still loads");
    // things from a newer oriel
    let newer = "theme = \"ember\"\nfuture_flag = true\n\n[future]\nx = 1\n\n[ai]\nnew_thing = [1, 2]\nprovider = \"ollama\"\n\n[ai.models]\nclaude = \"opus\"\n\n[[roster]]\nname = \"w\"\ncolor = \"red\"\n";
    let c = parse_like_load(newer);
    assert_eq!(c.theme, "ember");
    assert_eq!(c.ai.provider, "ollama");
    assert_eq!(c.ai.models.get("claude").map(String::as_str), Some("opus"));
    assert_eq!(c.roster.len(), 1);
}

#[test]
fn config_empty_and_garbage_files_give_defaults() {
    let d = dbg(&parse_like_load(""));
    for s in ["", "   \n\n", "# just a comment\n", "\n# theme = \"ember\"\n"] {
        assert_eq!(dbg(&parse_like_load(s)), d, "{s:?}");
    }
    let bytes: Vec<u8> = (0..400u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    let noise = String::from_utf8_lossy(&bytes).to_string();
    for s in ["not toml at all {{{", "\0\0\0", "[ai\nprovider =", "theme = ", "[[ai]]\nprovider = 'x'", "ai = 5", "= = =", "[a.b.c.d.e.f.g]\n[a.b.c.d.e.f.g]", noise.as_str()] {
        let c = std::panic::catch_unwind(|| parse_like_load(s)).unwrap_or_else(|_| panic!("parsing {s:?} panicked"));
        assert_eq!(c.theme, default_theme(), "{s:?}");
        assert_eq!(c.ai.perms, "edits", "{s:?}");
    }
}

/// Windows Notepad (and PowerShell 5's `Set-Content -Encoding UTF8`) save UTF-8 with a byte-order mark.
#[test]
fn config_with_a_utf8_bom_still_loads() {
    let c = parse_like_load("\u{feff}theme = \"ember\"\nprefix = \"ctrl+a\"\n");
    assert_eq!((c.theme.as_str(), c.prefix.as_str()), ("ember", "ctrl+a"), "a BOM at the start must not reset the whole config");
}

// ------------------------------------------------------------------ profiles (ORIEL_DATA_DIR)

/// Runs in a child copy of this test binary (see profile_paths): prints where this process's profile keeps the
/// config and the first-run marker. Does nothing in a normal run.
#[test]
#[ignore = "helper: only does something in the child process profile_paths starts"]
fn profile_paths_child() {
    if std::env::var_os("ORIEL_QA_CHILD").is_none() {
        return;
    }
    println!("QA-PATHS config={} marker={} data={}", crate::config::path().display(), crate::onboard::marker().display(), crate::config::data_dir().display());
}

/// Start this test binary again with ORIEL_DATA_DIR set (never set_var in the shared test process) and read back
/// (config path, first-run marker path, data dir). The child's working folder is a scratch folder.
fn profile_paths(data_dir: &str, name: &str) -> (String, String, String) {
    let cwd = scratch(name);
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["qa_first_run::profile_paths_child", "--exact", "--ignored", "--nocapture", "--test-threads=1"])
        .env("ORIEL_QA_CHILD", "1")
        .env("ORIEL_DATA_DIR", data_dir)
        .current_dir(&cwd)
        .output()
        .expect("start the child test process");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    // the harness prints "test <name> ... " on the same line first
    let line = text.lines().find_map(|l| l.find("QA-PATHS ").map(|i| l[i + 9..].trim_end().to_string())).unwrap_or_else(|| panic!("no paths from the child:\n{text}\n{}", String::from_utf8_lossy(&out.stderr)));
    let (config, rest) = line.strip_prefix("config=").and_then(|s| s.split_once(" marker=")).expect("config=");
    let (marker, data) = rest.split_once(" data=").expect("data=");
    (config.to_string(), marker.to_string(), data.to_string())
}

/// ORIEL_DATA_DIR is "a separate profile (demos, screenshots, testing)". The first-run marker lives in the profile,
/// so a fresh profile runs the setup, and everything the setup saves (theme, icons, music, notes, startup, AI)
/// has to land in that profile too, not in the main profile's config.toml.
#[test]
#[ignore = "fails: config.rs:177 path() ignores ORIEL_DATA_DIR, so a fresh profile's first-run setup overwrites the main config.toml"]
fn profile_keeps_its_own_config() {
    let prof = scratch("profile-a");
    let (config, marker, data) = profile_paths(&prof.to_string_lossy(), "profile-a-cwd");
    assert!(Path::new(&data).starts_with(&prof), "data dir {data} is the profile");
    assert!(Path::new(&marker).starts_with(&prof), "first-run marker {marker} is per profile");
    assert!(Path::new(&config).starts_with(&prof), "the config the profile's setup saves to ({config}) is outside the profile {}", prof.display());
}

/// An empty ORIEL_DATA_DIR (`ORIEL_DATA_DIR= oriel`, or a launcher that sets it to "") must not turn the current
/// folder into the profile: chats, notes and the first-run marker would be written wherever oriel was started.
#[test]
#[ignore = "fails: config.rs:169-172 treats an empty ORIEL_DATA_DIR as the relative path \"\", so data lands in the working folder"]
fn empty_data_dir_env_is_ignored() {
    let (_config, marker, data) = profile_paths("", "profile-empty-cwd");
    assert!(Path::new(&data).is_absolute() && Path::new(&marker).is_absolute(), "data dir {data:?} / marker {marker:?} are relative to the working folder");
}

/// One value of the wrong type (a hand edit like `max_parallel = "3"`) must not throw away every other setting:
/// load() then hands back pure defaults, and the next save (any theme change, the onboarding) writes those
/// defaults over the user's file.
#[test]
#[ignore = "fails: config.rs:182 load() parses all-or-nothing; one bad value resets every setting (and save() then overwrites the file)"]
fn config_one_bad_value_keeps_the_rest() {
    let s = "theme = \"ember\"\nstartup = \"music\"\n\n[ai]\nprovider = \"codex\"\n\n[lead]\nmax_parallel = \"3\"\n";
    let c = parse_like_load(s);
    assert_eq!((c.theme.as_str(), c.startup.as_str(), c.ai.provider.as_str()), ("ember", "music", "codex"), "the good values survive a bad one");
}
