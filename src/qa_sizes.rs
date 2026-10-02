//! QA pass: sizes. Every pane `panes::open` makes, rendered at many sizes (tiny to huge, zero-sized too), in
//! three states (fresh, loaded, scrolled to the end) and through a resize sequence, plus each pane's sidebar
//! section. Nothing may panic, nothing may be drawn outside the rect the pane was given, and no wide glyph may
//! be cut in half at the right edge. The whole app (sidebar, splits, zoom, popups, onboarding) is in
//! `qa_sizes_app.rs`, which needs App's private fields.
//!
//! Panes that would read this machine when rendered (chat's saved chats, the music library, the agents board)
//! run in a child copy of this test binary with ORIEL_DATA_DIR / APPDATA / CODEX_HOME pointed at a scratch
//! folder under target/test-scratch/qa-sizes. Storage and "your AIs" scan the real home folder whatever the
//! environment says, so they're swept from fixture-backed instances in their own modules
//! (panes/storage/qa_sizes.rs, panes/ais/qa_sizes.rs). The claude/codex entries of panes::open start the real
//! CLI, so they're stood in for by a terminal pane running a harmless shell under the same title and icon.
//!
//!   cargo test qa_sizes -- --nocapture

#![cfg(test)]

use crate::pane::{Cx, Pane};
use crate::testkit::Kit;
use crossterm::event::KeyCode;
use ratatui::{
    Terminal,
    backend::TestBackend,
    buffer::Buffer,
    layout::{Position, Rect},
};
use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;

/// The sizes from the brief.
pub(crate) const SIZES: &[(u16, u16)] = &[(1, 1), (5, 3), (20, 6), (40, 10), (80, 24), (120, 40), (250, 70)];
/// Shapes the app really hands a pane (a frame's inside can be 0 wide; a split can be 1 row) and other odd ones.
pub(crate) const ODD: &[(u16, u16)] = &[(0, 0), (0, 5), (7, 0), (2, 2), (3, 1), (1, 6), (250, 1), (250, 2), (4, 70), (12, 4), (18, 30), (26, 12), (33, 7), (60, 8)];
/// Sidebar-section sizes: the app gives `side()` what's left of a 26-36 wide sidebar (22-32 columns).
pub(crate) const SIDE: &[(u16, u16)] = &[(0, 0), (1, 1), (5, 3), (22, 1), (22, 2), (22, 6), (24, 10), (32, 24), (32, 60)];
/// A resize sequence (a window being dragged around), rendered on one pane.
pub(crate) const DRAG: &[(u16, u16)] = &[(250, 70), (1, 1), (80, 24), (5, 3), (120, 40), (0, 0), (20, 6), (250, 70), (40, 10)];

const PAD: u16 = 2;
const SENT: &str = "¤";

thread_local! {
    static LAST_PANIC: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Remember where the last panic on this thread happened (file:line), then do what the default hook does.
pub(crate) fn install_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let loc = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
            let msg = info.payload().downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| info.payload().downcast_ref::<String>().cloned()).unwrap_or_default();
            LAST_PANIC.with(|p| *p.borrow_mut() = Some(format!("panicked at {loc}: {msg}")));
            prev(info);
        }));
    });
}

/// Run `f`, turning a panic into Err("panicked at file:line: message").
pub(crate) fn guard<R>(f: impl FnOnce() -> R) -> Result<R, String> {
    install_hook();
    LAST_PANIC.with(|p| *p.borrow_mut() = None);
    catch_unwind(AssertUnwindSafe(f)).map_err(|_| LAST_PANIC.with(|p| p.borrow_mut().take()).unwrap_or_else(|| "panicked".into()))
}

/// One thing that went wrong: which pane (or app scenario), in which phase, at which size, and what.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Problem {
    pub pane: String,
    pub phase: String,
    pub w: u16,
    pub h: u16,
    pub msg: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{} [{}] {}x{}: {}", self.pane, self.phase, self.w, self.h, self.msg)
    }
}

pub(crate) fn problem(pane: &str, phase: &str, w: u16, h: u16, msg: impl Into<String>) -> Problem {
    Problem { pane: pane.into(), phase: phase.into(), w, h, msg: msg.into() }
}

/// A known, reported bug: a label (with the product file:line) and which problems it explains. The sweep
/// still runs everything and fails on anything else; each bug also has its own `#[ignore = "fails: ..."]` test.
pub(crate) type Known = (&'static str, fn(&Problem) -> bool);

/// Cells outside `inside` that don't hold the sentinel any more, and wide glyphs in the last column of `inside`.
pub(crate) fn spill(buf: &Buffer, inside: Rect) -> Vec<String> {
    let mut out = vec![];
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let pos = Position { x, y };
            let sym = buf[(x, y)].symbol();
            if !inside.contains(pos) {
                if sym != SENT {
                    out.push(format!("drew {sym:?} at ({},{}) outside its {}x{} rect", x as i32 - inside.x as i32, y as i32 - inside.y as i32, inside.width, inside.height));
                }
            } else if x + 1 == inside.right() && unicode_width::UnicodeWidthStr::width(sym) > 1 {
                out.push(format!("wide glyph {sym:?} cut in half at the right edge (col {})", x - inside.x));
            }
        }
    }
    out
}

/// Render `p` (or its sidebar section) into a w x h rect inside a padded buffer filled with a sentinel.
/// Ok(text of the rect) or Err(what went wrong: a panic, or drawing outside the rect).
pub(crate) fn check(k: &mut Kit, p: &mut dyn Pane, w: u16, h: u16, side: bool) -> Result<String, String> {
    let (bw, bh) = (w + 2 * PAD, h + 2 * PAD);
    let area = Rect::new(PAD, PAD, w, h);
    let mut term = Terminal::new(TestBackend::new(bw, bh)).unwrap();
    let mut actions = std::mem::take(&mut k.actions);
    let r = guard(|| {
        term.draw(|f| {
            let buf = f.buffer_mut();
            for y in 0..bh {
                for x in 0..bw {
                    if !area.contains(Position { x, y }) {
                        buf[(x, y)].set_symbol(SENT);
                    }
                }
            }
            let mut cx = Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: &mut actions, focused: true, time: k.time };
            if side {
                p.side(f, area, &mut cx);
            } else {
                p.render(f, area, &mut cx);
            }
        })
        .map(|_| ())
    });
    k.actions = actions;
    r?.map_err(|e| format!("draw failed: {e}"))?;
    let buf = term.backend().buffer();
    let bad = spill(buf, area);
    if !bad.is_empty() {
        let n = bad.len();
        return Err(format!("{} (+{} more cells)", bad[0], n - 1));
    }
    Ok((area.y..area.bottom()).map(|y| (area.x..area.right()).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n"))
}

/// Keys that only move a selection or scroll: nothing opens, plays, deletes or types.
const SCROLL_KEYS: &[KeyCode] = &[KeyCode::PageDown, KeyCode::PageDown, KeyCode::PageDown];

/// One pane through everything: fresh at every size, loaded (after its background work wakes it), scrolled
/// to the end, dragged through a resize sequence, and its sidebar section. Returns every problem found.
pub(crate) fn sweep(name: &str, k: &mut Kit, make: &mut dyn FnMut(&Kit) -> Box<dyn Pane>, keys: bool, wait_ms: u64) -> Vec<Problem> {
    let mut bad = vec![];
    let all: Vec<(u16, u16)> = SIZES.iter().chain(ODD).copied().collect();
    let mut p = make(k);
    // a panic can leave a pane half-updated (or a mutex poisoned): start over with a new one
    let mut run = |k: &mut Kit, p: &mut Box<dyn Pane>, phase: &str, w: u16, h: u16, side: bool, bad: &mut Vec<Problem>| {
        if let Err(e) = check(k, p.as_mut(), w, h, side) {
            let restart = e.starts_with("panicked");
            bad.push(problem(name, phase, w, h, e));
            if restart {
                *p = make(k);
            }
        }
    };
    let press = |k: &mut Kit, p: &mut Box<dyn Pane>, c: KeyCode, phase: &str, bad: &mut Vec<Problem>| {
        if let Err(e) = guard(|| k.key(p.as_mut(), c)) {
            bad.push(problem(name, phase, 0, 0, format!("key {c:?}: {e}")));
        }
    };
    for &(w, h) in &all {
        run(k, &mut p, "fresh", w, h, false, &mut bad);
    }
    if wait_ms > 0 {
        if let Err(e) = guard(|| k.wait_wake(p.as_mut(), wait_ms)) {
            bad.push(problem(name, "poll", 0, 0, e));
        }
    }
    for &(w, h) in &all {
        run(k, &mut p, "loaded", w, h, false, &mut bad);
    }
    if keys {
        for _ in 0..30 {
            press(k, &mut p, KeyCode::Down, "keys", &mut bad);
        }
        for &c in SCROLL_KEYS {
            press(k, &mut p, c, "keys", &mut bad);
        }
        for &(w, h) in &all {
            run(k, &mut p, "scrolled", w, h, false, &mut bad);
        }
    }
    for &(w, h) in DRAG {
        run(k, &mut p, "resize", w, h, false, &mut bad);
        if keys {
            // keys right after a shrink use the geometry of the last frame
            press(k, &mut p, KeyCode::Down, &format!("key after {w}x{h}"), &mut bad);
            press(k, &mut p, KeyCode::Up, &format!("key after {w}x{h}"), &mut bad);
        }
    }
    for &(w, h) in SIDE {
        run(k, &mut p, "side", w, h, true, &mut bad);
    }
    bad
}

/// Fail with every problem not explained by a known bug (deduplicated, one per line), or pass. Known ones
/// are counted on stdout.
pub(crate) fn verdict(bad: Vec<Problem>, known: &[Known]) {
    let mut seen = std::collections::BTreeSet::new();
    let bad: Vec<Problem> = bad.into_iter().filter(|b| seen.insert(b.clone())).collect();
    for (label, hit) in known {
        let n = bad.iter().filter(|b| hit(b)).count();
        if n > 0 {
            println!("known ({n} cases): {label}");
        }
    }
    let rest: Vec<String> = bad.iter().filter(|b| !known.iter().any(|(_, hit)| hit(b))).map(|b| b.to_string()).collect();
    assert!(rest.is_empty(), "{} problem(s):
{}", rest.len(), rest.join("
"));
}

/// Problems whose message says something was drawn outside the rect.
pub(crate) fn outside(p: &Problem) -> bool {
    p.msg.contains("outside its")
}

/// target/test-scratch/qa-sizes/<name>, emptied, absolute.
pub(crate) fn scratch(name: &str) -> PathBuf {
    let d = std::path::absolute(format!("target/test-scratch/qa-sizes/{name}")).unwrap();
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A config whose terminals run a quiet shell (no profile, no prompt theme) and whose music folder is `music`.
pub(crate) fn quiet_config(music: Option<&std::path::Path>) -> crate::config::Config {
    let mut c = crate::config::Config::default();
    c.shell = if cfg!(windows) { "cmd.exe /d /q".into() } else { "sh".into() };
    if let Some(m) = music {
        c.music.folders = vec![m.to_string_lossy().to_string()];
    }
    c
}

/// Long, wide and awkward names for fixtures: CJK (2 columns each), emoji, combining marks, a very long word.
pub(crate) const AWKWARD: &[&str] = &[
    "a very long name that goes on and on well past any sensible column width in a narrow pane",
    "日本語のファイル名はとても長くて幅が二倍です",
    "emoji 🎵🎶🔥 party mix — final (v2) FINAL",
    "e\u{301}le\u{301}ve cafe\u{301} na\u{ef}ve",
    "Supercalifragilisticexpialidocious_Supercalifragilisticexpialidocious_Supercalifragilisticexpialidocious",
    "x",
];

// ------------------------------------------------------------------ known bugs (reported; each has a test below)

/// Pane bugs this sweep found. The sweeps skip exactly these and still fail on anything else.
pub(crate) const PANE_KNOWN: &[Known] = &[
    (
        "calendar: the month grid is at least 28 columns (cw = max(grid_w/7, 4)) and never clipped to the pane, and the month name / weekday row are drawn without a height check: spills right under ~30 columns and below at 4 rows or fewer (panes/calendar.rs:251, 253-258, 273)",
        |p| p.pane.starts_with("calendar") && p.phase != "side" && outside(p) && (p.w < 30 || p.h <= 4),
    ),
    ("calendar sidebar: 'nothing in the next two weeks' is drawn on the row below a 1-row section (panes/calendar.rs:453)", |p| p.pane.starts_with("calendar") && p.phase == "side" && outside(p) && p.h <= 1),
    (
        "home: app grid cells are 24 wide and only checked against the bottom (labels spill right under ~16 columns); the tagline has no height check and ui::logo's fallback draws into a 0-row area (panes/home.rs:56-57, 68-69; ui.rs:171)",
        |p| p.pane.starts_with("home") && outside(p) && (p.w < 24 || p.h <= 2),
    ),
    ("themes: the header row is drawn although the body has no rows (panes/themes.rs:148, 159)", |p| p.pane.starts_with("themes") && outside(p) && p.h <= 1),
    ("music: the now-playing strip gets 0 rows but draws 'nothing playing' anyway (panes/music.rs:825, 833 -> 553-557)", |p| p.pane.starts_with("music") && outside(p) && p.h <= 1),
];

/// Render `p` at w x h (or its sidebar section) and fail on a panic or anything drawn outside the rect.
pub(crate) fn assert_clean(k: &mut Kit, p: &mut dyn Pane, w: u16, h: u16, side: bool) -> String {
    check(k, p, w, h, side).unwrap_or_else(|e| panic!("{w}x{h}: {e}"))
}

fn empty_calendar(name: &str) -> crate::panes::calendar::Calendar {
    crate::panes::calendar::Calendar::open_at(scratch(name).join("none.json"))
}

#[test]
#[ignore = "fails: layout::split_rect panics (clamp: min > max) on a split whose area is 1 cell wide or tall (src/layout.rs:143, 148)"]
fn qa_sizes_bug_split_rect_one_cell() {
    use crate::layout::{Dir, Node};
    for dir in [Dir::Right, Dir::Down] {
        // two panes on a 1x1 screen: w.clamp(1.min(1), 1 - 1) = clamp(1, 0)
        let n = Node::Split { dir, ratio: 0.5, a: Box::new(Node::Leaf(1)), b: Box::new(Node::Leaf(2)) };
        let mut out = vec![];
        n.rects(Rect::new(0, 0, 1, 1), &mut out);
        assert_eq!(out.len(), 2);
    }
}

#[test]
#[ignore = "fails: calendar's month grid is at least 28 columns and isn't clipped: it spills past the right edge of a pane a third of an 80-column screen wide (src/panes/calendar.rs:253-258, 273)"]
fn qa_sizes_bug_calendar_narrow_spills_right() {
    let mut k = kit();
    assert_clean(&mut k, &mut empty_calendar("cal-narrow"), 24, 30, false);
}

#[test]
#[ignore = "fails: calendar draws its weekday row below a 3-row pane (src/panes/calendar.rs:251, 254)"]
fn qa_sizes_bug_calendar_short_spills_below() {
    let mut k = kit();
    assert_clean(&mut k, &mut empty_calendar("cal-short"), 80, 3, false);
}

#[test]
#[ignore = "fails: calendar's sidebar section draws 'nothing in the next two weeks' below a 1-row area (src/panes/calendar.rs:453)"]
fn qa_sizes_bug_calendar_side_one_row() {
    let mut k = kit();
    assert_clean(&mut k, &mut empty_calendar("cal-side"), 22, 1, true);
}

#[test]
#[ignore = "fails: home's app grid cells are 24 wide and only checked against the bottom: labels spill past the right edge of a 12-column pane (src/panes/home.rs:68-69)"]
fn qa_sizes_bug_home_narrow_spills_right() {
    let mut k = kit();
    assert_clean(&mut k, &mut crate::panes::home::Home::new(), 12, 30, false);
}

#[test]
#[ignore = "fails: home draws its tagline below a 2-row pane, and ui::logo's fallback draws into a 0-row one (src/panes/home.rs:56-57, src/ui.rs:171)"]
fn qa_sizes_bug_home_short_spills_below() {
    let mut k = kit();
    assert_clean(&mut k, &mut crate::panes::home::Home::new(), 80, 2, false);
    assert_clean(&mut k, &mut crate::panes::home::Home::new(), 80, 0, false);
}

#[test]
#[ignore = "fails: themes draws its header row below a 1-row pane (src/panes/themes.rs:148, 159)"]
fn qa_sizes_bug_themes_one_row() {
    let mut k = kit();
    assert_clean(&mut k, &mut crate::panes::themes::Themes::new(), 80, 1, false);
}

#[test]
#[ignore = "fails: music draws 'nothing playing' below a 1-row pane: the now-playing strip gets 0 rows but draws its first line (src/panes/music.rs:825, 833, 553-557)"]
fn qa_sizes_bug_music_one_row() {
    run_child("qa_sizes::qa_sizes_child_music_one_row", &scratch("music-one-row"));
}

#[test]
#[ignore = "run by qa_sizes_bug_music_one_row in a child process with a scratch data folder"]
fn qa_sizes_child_music_one_row() {
    let Some(dir) = child_dir() else { return };
    let mut k = Kit::new();
    k.config = quiet_config(Some(&dir.join("music")));
    let mut p = crate::panes::open("music", &k.config).unwrap();
    assert_clean(&mut k, p.as_mut(), 80, 1, false);
}

// ------------------------------------------------------------------ in-process panes (safe to render here)

/// Every name panes::open knows, so a new pane can't be added without this file noticing.
const OPEN_NAMES: &[&str] = &["terminal", "shell", "ai", "chat", "music", "system", "files", "notes", "calendar", "storage", "agents", "ais", "home", "help", "themes", "alerts", "updates"];

fn open(name: &'static str) -> impl FnMut(&Kit) -> Box<dyn Pane> {
    move |k: &Kit| crate::panes::open(name, &k.config).unwrap_or_else(|| panic!("panes::open({name}) gave None"))
}

fn kit() -> Kit {
    let mut k = Kit::new();
    k.config = quiet_config(None);
    k
}

#[test]
fn qa_sizes_open_names_match_the_factory() {
    // panes/mod.rs is the source of truth: every name it handles must be in OPEN_NAMES (and so swept somewhere)
    let src = include_str!("panes/mod.rs");
    let body = &src[src.find("pub fn open(").unwrap()..];
    let arms: Vec<&str> = body.lines().filter(|l| l.contains("=> Box::new(")).flat_map(|l| l.split("=>").next().unwrap().split('|')).map(|s| s.trim().trim_matches('"')).collect();
    assert!(arms.len() >= 15, "{arms:?}");
    for a in &arms {
        assert!(OPEN_NAMES.contains(a), "panes::open handles {a:?}, which qa_sizes doesn't sweep");
    }
}

#[test]
fn qa_sizes_help() {
    let mut k = kit();
    verdict(sweep("help", &mut k, &mut open("help"), true, 0), PANE_KNOWN);
}

#[test]
fn qa_sizes_help_every_topic() {
    // the plain sweep draws only the first and the last topic at every size: here every topic, at its top and
    // scrolled past its end, with its sidebar list, then the search box with queries that match a lot, nothing,
    // and wide text
    let mut k = kit();
    let mut bad = vec![];
    let all: Vec<(u16, u16)> = SIZES.iter().chain(ODD).copied().collect();
    let mut seen: Vec<String> = vec![];
    for i in 0..60 {
        let mut p = crate::panes::help::Help::new();
        for _ in 0..i {
            k.key(&mut p, KeyCode::Down);
        }
        let title = p.title();
        if seen.contains(&title) {
            break; // Down stops at the last topic
        }
        seen.push(title.clone());
        for (phase, pages) in [("top", 0), ("end", 40)] {
            for _ in 0..pages {
                k.key(&mut p, KeyCode::PageDown);
            }
            for &(w, h) in &all {
                if let Err(e) = check(&mut k, &mut p, w, h, false) {
                    bad.push(problem(&title, phase, w, h, e));
                }
            }
        }
        for &(w, h) in SIDE {
            if let Err(e) = check(&mut k, &mut p, w, h, true) {
                bad.push(problem(&title, "side", w, h, e));
            }
        }
    }
    assert!(seen.len() >= 15, "only reached {} help topics: {seen:?}", seen.len());
    for q in ["tab", "zzqqxxjj", AWKWARD[1], AWKWARD[0]] {
        let mut p = crate::panes::help::Help::new();
        k.key(&mut p, KeyCode::Char('/'));
        k.typ(&mut p, q);
        for &(w, h) in &all {
            if let Err(e) = check(&mut k, &mut p, w, h, false) {
                bad.push(problem(&format!("help search {q:?}"), "typing", w, h, e));
            }
        }
        for &(w, h) in SIDE {
            if let Err(e) = check(&mut k, &mut p, w, h, true) {
                bad.push(problem(&format!("help search {q:?}"), "side", w, h, e));
            }
        }
    }
    verdict(bad, PANE_KNOWN);
}

#[test]
fn qa_sizes_home() {
    let mut k = kit();
    verdict(sweep("home", &mut k, &mut open("home"), true, 0), PANE_KNOWN);
}

#[test]
fn qa_sizes_themes() {
    let mut k = kit();
    verdict(sweep("themes", &mut k, &mut open("themes"), true, 0), PANE_KNOWN);
}

#[test]
fn qa_sizes_updates() {
    let mut k = kit();
    // the check thread answers "no network in tests" right away
    verdict(sweep("updates", &mut k, &mut open("updates"), true, 400), PANE_KNOWN);
}

#[test]
fn qa_sizes_alerts() {
    let mut k = kit();
    verdict(sweep("alerts", &mut k, &mut open("alerts"), true, 0), PANE_KNOWN);
}

#[test]
fn qa_sizes_calendar() {
    let mut k = kit();
    // panes::open's calendar (tests keep it in target/test-scratch), then one full of long plans
    let mut bad = sweep("calendar", &mut k, &mut open("calendar"), true, 0);
    let dir = scratch("calendar");
    let (today, _) = (crate::alerts::now() / 86400, 0);
    let plans: Vec<serde_json::Value> = (0..40)
        .map(|i| serde_json::json!({"day": today - 3 + (i % 9) as i64, "at": if i % 2 == 0 { serde_json::json!(9 * 60 + i) } else { serde_json::Value::Null }, "text": AWKWARD[i % AWKWARD.len()]}))
        .collect();
    let path = dir.join("calendar.json");
    std::fs::write(&path, serde_json::to_string(&plans).unwrap()).unwrap();
    bad.extend(sweep("calendar(fixture)", &mut k, &mut |_| Box::new(crate::panes::calendar::Calendar::open_at(path.clone())), true, 0));
    verdict(bad, PANE_KNOWN);
}

#[test]
fn qa_sizes_notes() {
    let mut k = kit();
    let mut bad = sweep("notes", &mut k, &mut open("notes"), true, 300);
    let dir = scratch("notes");
    for (i, name) in AWKWARD.iter().enumerate() {
        let body = format!("# {name}\n\n{}\n", AWKWARD.iter().cycle().skip(i).take(30).cloned().collect::<Vec<_>>().join("\n"));
        std::fs::write(dir.join(format!("{}.md", name.chars().filter(|c| !"\\/:*?\"<>|".contains(*c)).take(60).collect::<String>())), body).unwrap();
    }
    bad.extend(sweep("notes(fixture)", &mut k, &mut |_| Box::new(crate::panes::notes::Notes::open_in(dir.clone(), None)), true, 300));
    verdict(bad, PANE_KNOWN);
}

#[test]
fn qa_sizes_files() {
    let mut k = kit();
    // panes::open's files lists the working directory (this repo); the fixture has awkward names and a
    // folder deep enough to scroll
    let mut bad = sweep("files", &mut k, &mut open("files"), true, 600);
    let dir = scratch("files");
    for (i, name) in AWKWARD.iter().enumerate() {
        let safe: String = name.chars().filter(|c| !"\\/:*?\"<>|".contains(*c)).take(80).collect();
        std::fs::write(dir.join(format!("{safe}.txt")), AWKWARD.join("\n").repeat(i + 1)).unwrap();
        std::fs::create_dir_all(dir.join(format!("{safe} folder"))).unwrap();
    }
    for i in 0..80 {
        std::fs::write(dir.join(format!("file-{i:03}.rs")), format!("fn f{i}() {{}}\n")).unwrap();
    }
    bad.extend(sweep("files(fixture)", &mut k, &mut |_| Box::new(crate::panes::files::Files::new(Some(dir.clone()))), true, 600));
    verdict(bad, PANE_KNOWN);
}

#[test]
fn qa_sizes_system() {
    let mut k = kit();
    // live sampler: the first sample lands within a second or so
    verdict(sweep("system", &mut k, &mut open("system"), true, 1500), PANE_KNOWN);
}

#[test]
fn qa_sizes_terminal() {
    // render only: no keys go to the shell
    let mut k = kit();
    let mut bad = sweep("terminal", &mut k, &mut open("terminal"), false, 800);
    bad.extend(sweep("shell", &mut k, &mut open("shell"), false, 0));
    // what panes::open("claude"/"codex") makes, minus the real CLI: a terminal pane under that title and icon
    for (title, icon) in [("claude code", "claude"), ("codex", "robot")] {
        let (prog, args) = crate::config::default_shell(&k.config);
        bad.extend(sweep(title, &mut k, &mut |_| Box::new(crate::panes::term::Term::new(title, icon, &prog, args.clone(), None)), false, 0));
    }
    verdict(bad, PANE_KNOWN);
}

// ------------------------------------------------------------------ child process: panes that read the machine

/// Run one of the `qa_sizes_child_*` tests in a copy of this binary with the data folders pointed at `dir`.
/// Returns its output; fails if it failed.
pub(crate) fn run_child(test: &str, dir: &std::path::Path) -> String {
    let exe = std::env::current_exe().unwrap();
    for d in ["data", "appdata", "codex", "music", "cwd"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    let out = std::process::Command::new(exe)
        .args([test, "--exact", "--ignored", "--nocapture", "--test-threads=1"])
        .env("QA_SIZES_CHILD", dir)
        .env("ORIEL_DATA_DIR", dir.join("data"))
        .env("APPDATA", dir.join("appdata"))
        .env("CODEX_HOME", dir.join("codex"))
        .env("CLAUDE_CONFIG_DIR", dir.join("claude"))
        .env_remove("ORIEL_LOG")
        .current_dir(dir.join("cwd"))
        .output()
        .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("1 passed") || !out.status.success(), "child didn't run {test}:\n{text}");
    assert!(out.status.success(), "{test} failed in the child process:\n{}", child_summary(&text));
    text
}

/// The interesting part of a failed child run: its assertion message.
fn child_summary(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let from = lines.iter().position(|l| l.contains("problem(s)")).or_else(|| lines.iter().position(|l| l.contains("panicked at"))).unwrap_or(0);
    lines[from..].iter().take(80).cloned().collect::<Vec<_>>().join("\n")
}

/// In a child: the scratch folder, or None when run by hand without the parent (then it does nothing, so it
/// never touches the real data folder).
pub(crate) fn child_dir() -> Option<PathBuf> {
    let d = PathBuf::from(std::env::var_os("QA_SIZES_CHILD")?);
    assert_eq!(std::env::var_os("ORIEL_DATA_DIR").map(PathBuf::from), Some(d.join("data")), "child without its scratch data dir");
    Some(d)
}

#[test]
fn qa_sizes_chat() {
    let dir = scratch("chat");
    run_child("qa_sizes::qa_sizes_child_chat", &dir);
}

#[test]
#[ignore = "run by qa_sizes_chat in a child process with a scratch data folder"]
fn qa_sizes_child_chat() {
    let Some(dir) = child_dir() else { return };
    // saved chats with awkward titles and long messages, so the sidebar list and transcript have content
    let chats = dir.join("data").join("chats");
    std::fs::create_dir_all(&chats).unwrap();
    let now = crate::alerts::now() as f64;
    for (i, t) in AWKWARD.iter().enumerate() {
        let msgs: Vec<serde_json::Value> = (0..6)
            .map(|j| serde_json::json!({"role": if j % 2 == 0 { "user" } else { "assistant" }, "content": format!("{t}\n\n```rust\nfn {}() {{ let s = \"{}\"; }}\n```\n| a | b |\n|---|---|\n| {t} | {t} |", "x".repeat(j * 20), AWKWARD[(i + j) % AWKWARD.len()])}))
            .collect();
        let c = serde_json::json!({"id": format!("q{i}"), "title": t, "created": now - i as f64 * 5000.0, "updated": now - i as f64 * 5000.0, "provider": "claude", "messages": msgs});
        std::fs::write(chats.join(format!("q{i}.json")), c.to_string()).unwrap();
    }
    let mut k = kit();
    // the fixture has to actually be on screen, or the sweep proves nothing
    let mut p = open("ai")(&k);
    let side = assert_clean(&mut k, p.as_mut(), 32, 30, true);
    assert!(side.contains("a very long name") || side.contains("日本語"), "saved chats not listed:
{side}");
    let mut bad = sweep("ai", &mut k, &mut open("ai"), true, 0);
    bad.extend(sweep("chat", &mut k, &mut open("chat"), false, 0));
    // an open chat with a long transcript: ctrl+n / the sidebar would do it; the palette-free way is the
    // saved chat list, reached with keys the chat pane owns
    verdict(bad, PANE_KNOWN);
}

#[test]
fn qa_sizes_music() {
    let dir = scratch("music");
    run_child("qa_sizes::qa_sizes_child_music", &dir);
}

#[test]
#[ignore = "run by qa_sizes_music in a child process with a scratch data folder"]
fn qa_sizes_child_music() {
    let Some(dir) = child_dir() else { return };
    // an audio-player library (Windows reads it from %APPDATA%, pointed at scratch) with awkward titles; the
    // tracks must exist, so they're empty files in the scratch music folder. Nothing is played.
    let music = dir.join("music");
    let mut tracks = vec![];
    for i in 0..60 {
        let t = AWKWARD[i % AWKWARD.len()];
        let p = music.join(format!("track-{i:02}.mp3"));
        std::fs::write(&p, b"").unwrap();
        tracks.push(serde_json::json!({"id": format!("t{i}"), "path": p.to_string_lossy(), "title": format!("{t} {i}"), "artist": AWKWARD[(i + 2) % AWKWARD.len()], "album": t, "duration": 30.0 + i as f64 * 17.0, "plays": i % 5}));
    }
    let lib = serde_json::json!({"tracks": tracks, "playlists": [{"id": "p1", "name": AWKWARD[1], "trackIds": ["t1", "t2", "t3"]}, {"id": "p2", "name": AWKWARD[0], "trackIds": ["t4"]}]});
    std::fs::create_dir_all(dir.join("appdata").join("audio-player")).unwrap();
    std::fs::write(dir.join("appdata").join("audio-player").join("library.json"), lib.to_string()).unwrap();
    let mut k = Kit::new();
    k.config = quiet_config(Some(&music));
    let mut p = open("music")(&k);
    let _ = assert_clean(&mut k, p.as_mut(), 120, 40, false);
    k.wait_wake(p.as_mut(), 1500);
    assert_eq!(p.subtitle().as_deref(), Some("60 songs"), "the fixture library didn't load");
    let bad = sweep("music", &mut k, &mut open("music"), true, 800);
    verdict(bad, PANE_KNOWN);
}

#[test]
fn qa_sizes_agents() {
    let dir = scratch("agents");
    run_child("qa_sizes::qa_sizes_child_agents", &dir);
}

#[test]
#[ignore = "run by qa_sizes_agents in a child process with a scratch data folder"]
fn qa_sizes_child_agents() {
    let Some(_dir) = child_dir() else { return };
    // an empty board in a folder that isn't a git repo (the child's cwd)
    let mut k = kit();
    verdict(sweep("agents", &mut k, &mut open("agents"), true, 800), PANE_KNOWN);
}

#[test]
fn qa_sizes_plain_icons() {
    let dir = scratch("plain");
    run_child("qa_sizes::qa_sizes_child_plain_icons", &dir);
}

#[test]
#[ignore = "run by qa_sizes_plain_icons in a child process (flips the global icon switch)"]
fn qa_sizes_child_plain_icons() {
    let Some(dir) = child_dir() else { return };
    // plain-text icons are wider than the Nerd Font glyphs ("shuf", "pkg", "||"): same sweep, no nerd font,
    // and a long awkward alert list for the alerts pane
    crate::ui::NERD.store(false, std::sync::atomic::Ordering::Relaxed);
    for (i, t) in AWKWARD.iter().cycle().take(40).enumerate() {
        let kinds = [crate::alerts::Kind::AgentDone, crate::alerts::Kind::NeedsYou, crate::alerts::Kind::Usage, crate::alerts::Kind::Calendar];
        crate::alerts::push(crate::alerts::Alert { at: crate::alerts::now() - i as i64 * 700, kind: kinds[i % 4], text: t.to_string(), app: Some(["ai", "music", "nowhere", "updates"][i % 4].into()), read: i % 3 == 0, pane: None });
    }
    let mut k = kit();
    // an empty scratch music folder, so the music app never scans the real one
    k.config = quiet_config(Some(&dir.join("music")));
    let mut bad = vec![];
    for name in ["help", "home", "themes", "alerts", "calendar", "notes", "files", "updates", "music", "agents", "ai"] {
        bad.extend(sweep(&format!("{name}(plain)"), &mut k, &mut open(name), true, 300));
    }
    verdict(bad, PANE_KNOWN);
}
