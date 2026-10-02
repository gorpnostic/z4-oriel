//! QA pass: sizes, the whole app. `App` drawn into a TestBackend at every size from 1x1 to 250x70 (and the
//! sidebar's edge at 70 columns, very flat and very narrow screens): every app tab, the sidebar shown and
//! hidden, your own tabs (30 of them too), many splits, zoom, a text selection across a resize, and every
//! popup (palette, rename, right-click menu, toasts, the prefix hint, every onboarding page and tour step).
//! Nothing may panic, and nothing may be drawn over a frame's border or next to a pane's × button (which is
//! how text spilling out of a pane, a title or a popup shows up on a full screen).
//!
//! A child module of `app` (registered there with #[path]) because it drives App's private state. The app
//! tabs and onboarding read the data folder and %APPDATA%, so they run in a child copy of the test binary
//! with those pointed at target/test-scratch/qa-sizes (see crate::qa_sizes::run_child); the rest runs here.
//!
//!   cargo test qa_sizes_app -- --nocapture

use super::*;
use crate::qa_sizes::{self as qa, AWKWARD, Known, Problem, guard, problem, verdict};
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

/// The brief's sizes, then the sidebar's edge (it appears at 70 columns), flat and narrow screens.
const APP_SIZES: &[(u16, u16)] = &[
    (1, 1), (5, 3), (20, 6), (40, 10), (80, 24), (120, 40), (250, 70),
    (69, 24), (70, 24), (70, 3), (70, 1), (80, 2), (250, 3), (26, 70), (100, 12),
];

fn cfg() -> Config {
    let mut c = qa::quiet_config(None);
    c.theme = "oriel".into();
    c.startup = "help".into(); // App::new opens this; "ai" would read the saved chats
    c
}

fn new_app() -> (App, std::sync::mpsc::Receiver<Event>) {
    let (tx, rx) = std::sync::mpsc::channel();
    (App::new(cfg(), tx), rx)
}

fn draw(app: &mut App, w: u16, h: u16) -> Result<Buffer, String> {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    guard(|| term.draw(|f| app.draw(f)).map(|_| ()))?.map_err(|e| format!("draw failed: {e}"))?;
    Ok(term.backend().buffer().clone())
}

fn sym(b: &Buffer, x: u16, y: u16) -> &str {
    b[(x, y)].symbol()
}

/// A rounded frame drawn at `r` still has its corners and side borders; with `plain_bottom` its bottom edge
/// is all line too (popups have no subtitle). Frames that don't fit on screen aren't checked.
fn frame_intact(b: &Buffer, r: Rect, plain_bottom: bool) -> Result<(), String> {
    if r.intersection(b.area) != r || r.width < 2 || r.height < 2 {
        return Ok(());
    }
    let (l, rt, t, bm) = (r.x, r.right() - 1, r.y, r.bottom() - 1);
    for (x, y, want) in [(l, t, "╭"), (rt, t, "╮"), (l, bm, "╰"), (rt, bm, "╯")] {
        if sym(b, x, y) != want {
            return Err(format!("frame {}x{} at ({},{}): corner ({x},{y}) is {:?}, not {want}", r.width, r.height, r.x, r.y, sym(b, x, y)));
        }
    }
    for y in t + 1..bm {
        for x in [l, rt] {
            if sym(b, x, y) != "│" {
                return Err(format!("frame {}x{} at ({},{}): side border at ({x},{y}) overwritten with {:?}", r.width, r.height, r.x, r.y, sym(b, x, y)));
            }
        }
    }
    if plain_bottom {
        let row: String = (l + 1..rt).map(|x| sym(b, x, bm)).collect();
        if row.chars().any(|c| c != '─') {
            return Err(format!("frame {}x{} at ({},{}): bottom border overwritten: {:?}", r.width, r.height, r.x, r.y, row));
        }
    }
    Ok(())
}

/// A pane's frame: sides and corners intact, and the bottom edge only line plus (part of) the subtitle.
fn pane_frame_intact(b: &Buffer, r: Rect, sub: Option<String>) -> Result<(), String> {
    frame_intact(b, r, false)?;
    if r.intersection(b.area) != r || r.width < 3 || r.height < 2 {
        return Ok(());
    }
    let bm = r.bottom() - 1;
    let rest: String = (r.x + 1..r.right() - 1).map(|x| sym(b, x, bm)).collect::<String>().replace('─', "");
    let rest = rest.trim();
    if !rest.is_empty() && !sub.unwrap_or_default().contains(rest) {
        return Err(format!("pane {}x{} at ({},{}): bottom border overwritten: {:?}", r.width, r.height, r.x, r.y, rest));
    }
    Ok(())
}

/// Draw at w x h; check the sidebar's frame and every pane's frame. The buffer, if it drew.
fn shot(app: &mut App, scen: &str, w: u16, h: u16, bad: &mut Vec<Problem>) -> Option<Buffer> {
    shot_with(app, scen, w, h, bad, true)
}

/// Like `shot`; `frames` = false when a popup is up (it covers frames on purpose: only its own is checked).
fn shot_with(app: &mut App, scen: &str, w: u16, h: u16, bad: &mut Vec<Problem>, frames: bool) -> Option<Buffer> {
    let b = match draw(app, w, h) {
        Ok(b) => b,
        Err(e) => {
            bad.push(problem(scen, "draw", w, h, e));
            return None;
        }
    };
    if !frames {
        return Some(b);
    }
    if app.sidebar && w >= 70 {
        let side = Rect { x: 0, y: 0, width: (w / 5).clamp(26, 36), height: h };
        if let Err(e) = frame_intact(&b, side, false) {
            bad.push(problem(scen, "sidebar", w, h, e));
        }
    }
    for (id, r) in app.outer.clone() {
        let sub = app.panes.get(&id).and_then(|p| p.subtitle());
        let title = app.panes.get(&id).map(|p| p.title()).unwrap_or_default();
        if let Err(e) = pane_frame_intact(&b, r, sub) {
            bad.push(problem(scen, &format!("pane '{title}'"), w, h, e));
        }
    }
    // the × close button on a pane's top border: only border between it and the corner, no title text
    for &(bx, id) in &app.pane_close {
        let x = bx.right();
        if x < b.area.width && sym(&b, x, bx.y) != "─" {
            let title = app.panes.get(&id).map(|p| p.title()).unwrap_or_default();
            bad.push(problem(scen, &format!("pane '{title}' close button"), w, h, format!("the title runs under the × button: {:?} is left between the × and the corner", sym(&b, x, bx.y))));
        }
    }
    Some(b)
}

fn text(b: &Buffer) -> String {
    (0..b.area.height).map(|y| (0..b.area.width).map(|x| sym(b, x, y)).collect::<String>()).collect::<Vec<_>>().join("\n")
}

fn press(app: &mut App, c: KeyCode, m: KeyModifiers) {
    app.key(KeyEvent::new(c, m));
}

/// Put an app tab in sidebar order with a pane of our choosing (the fixture ones for storage, ais, agents).
fn put_app(app: &mut App, name: &'static str, p: Box<dyn Pane>) {
    if app.tabs.iter().any(|t| t.app == Some(name)) {
        return;
    }
    let id = app.add(p);
    let order = |a: Option<&str>| a.and_then(|n| SIDEBAR.iter().position(|s| s.0 == n)).unwrap_or(usize::MAX);
    let me = order(Some(name));
    let at = app.tabs.iter().position(|t| order(t.app) > me).unwrap_or(app.tabs.len());
    let tid = app.tab_id();
    app.tabs.insert(at, Tab { id: tid, app: Some(name), name: None, root: Node::Leaf(id), focus: id, zoom: false });
    if app.cur >= at {
        app.cur += 1;
    }
}

/// Handle whatever background work sent (pane wakes, ticks) for `ms`.
fn settle(app: &mut App, rx: &std::sync::mpsc::Receiver<Event>, ms: u64, bad: &mut Vec<Problem>) {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(ms) {
        while let Ok(e) = rx.try_recv() {
            if let Err(err) = guard(|| app.handle(e)) {
                bad.push(problem("settle", "handle", 0, 0, err));
            }
        }
        let _ = guard(|| app.handle(Event::Tick)).map_err(|err| bad.push(problem("settle", "tick", 0, 0, err)));
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// A pane's border overwritten at app level, as the pane-level problem it is: (pane name, inner size).
fn as_pane_problem(p: &Problem) -> Option<Problem> {
    let name = p.phase.strip_prefix("pane '")?.split(['\'', ' ']).next()?;
    let (w, h) = p.msg.split_whitespace().nth(1)?.split_once('x')?;
    Some(problem(name, "app", w.parse::<u16>().ok()?.saturating_sub(2), h.parse::<u16>().ok()?.saturating_sub(2), "drew outside its rect (onto its frame)"))
}

/// Known, reported bugs the app sweeps run into (each also has its own ignored test). Everything else fails.
const KNOWN: &[Known] = &[
    (
        "layout::split_rect panics (clamp: min > max) on a split 1 cell wide or tall: ~7+ alt n splits at 80x24, or any split tab on a 1-column/1-row screen, crash the app (src/layout.rs:143, 148)",
        |p| p.msg.contains("layout.rs:143") || p.msg.contains("layout.rs:148"),
    ),
    (
        "a pane-level overflow bug (crate::qa_sizes::PANE_KNOWN: home, calendar, themes, music) showing up as text on that pane's own frame",
        |p| as_pane_problem(p).is_some_and(|pp| qa::PANE_KNOWN.iter().any(|(_, hit)| hit(&pp))),
    ),
    ("right-click menu: every item is drawn even when the menu was cut down to a short screen: they go over its bottom border and past it (src/app.rs:1409-1420)", |p| p.pane.starts_with("right-click menu") && p.phase == "frame"),
    ("palette: 'nothing matches' is drawn on the palette's own bottom border when it has 2 rows inside (src/app.rs:1612)", |p| p.pane.starts_with("palette") && p.phase == "frame" && p.msg.contains("nothing matc")),
    ("tour card: its two buttons need 35 columns and aren't fitted to the card: on screens under ~41 columns the second spills over its right border (src/onboard.rs:655, 672-675)", |p| p.phase == "tour card" && p.msg.contains("side border")),
    ("welcome: laid out for 19 rows, and the logo's rows are reserved even when it's too narrow to draw: buttons off screen below ~17 rows, a blank screen at 40x10 (src/onboard.rs:620-625)", |p| p.phase == "buttons"),
    ("rename: invisible below 70 columns (start_rename turns the sidebar on but draw() hides it under 70), and at 70+ the field doesn't scroll, so a long name pushes the cursor off (src/app.rs:593, 1319, 1533-1540)", |p| p.pane == "rename" && p.phase == "field"),
    ("pane frames: a title longer than the pane runs under the × close button, leaving a stray letter between the × and the corner (the title isn't shortened to make room: src/ui.rs:90-93, src/app.rs:1348-1356)", |p| p.phase.ends_with("close button")),
    ("sidebar: your own tabs are listed with no scrolling, after the fixed app list: at 80x24 only the first 2 fit, so the tab you're on (and its rename field) can be missing from the sidebar (src/app.rs:1514-1517)", |p| p.pane == "own tabs" && p.phase == "sidebar list"),
];

// ------------------------------------------------------------------ many splits, zoom (here: only light panes)

fn light_pane(i: usize) -> Box<dyn Pane> {
    match i % 4 {
        0 => Box::new(crate::panes::home::Home::new()),
        1 => Box::new(crate::panes::help::Help::new()),
        2 => Box::new(crate::panes::themes::Themes::new()),
        _ => Box::new(crate::panes::updates::Updates::new()),
    }
}

/// A tab of your own split `n` times, the way alt n does it (along the longer side of the focused pane).
fn split_tab(app: &mut App, n: usize, w: u16, h: u16, bad: &mut Vec<Problem>) {
    app.new_tab(Box::new(crate::panes::home::Home::new()));
    for i in 0..n {
        let _ = shot(app, "splits (building)", w, h, bad); // Place::Split reads the last frame's rects
        let from = app.focused();
        app.open(light_pane(i), Place::Split, from);
    }
}

#[test]
fn qa_sizes_app_splits_and_zoom() {
    let mut bad = vec![];
    for &(w, h) in APP_SIZES {
        for n in [3, 7, 12] {
            let (mut app, _rx) = new_app();
            split_tab(&mut app, n, w, h, &mut bad);
            let scen = format!("{n} splits");
            let _ = shot(&mut app, &scen, w, h, &mut bad);
            app.sidebar = false;
            let _ = shot(&mut app, &format!("{scen}, no sidebar"), w, h, &mut bad);
            app.sidebar = true;
            // zoom, unzoom, and dividers pushed to their limits both ways
            app.run_cmd(Cmd::Zoom);
            let _ = shot(&mut app, &format!("{scen}, zoomed"), w, h, &mut bad);
            app.run_cmd(Cmd::Zoom);
            for _ in 0..20 {
                app.resize(Dir::Right, 0.05);
                app.resize(Dir::Down, 0.05);
            }
            let _ = shot(&mut app, &format!("{scen}, dividers at 0.9"), w, h, &mut bad);
            for _ in 0..40 {
                app.resize(Dir::Right, -0.05);
                app.resize(Dir::Down, -0.05);
            }
            let _ = shot(&mut app, &format!("{scen}, dividers at 0.1"), w, h, &mut bad);
            // walk focus everywhere (neighbor() on zero-sized rects)
            for (dx, dy) in [(1, 0), (0, 1), (-1, 0), (0, -1)] {
                for _ in 0..6 {
                    if let Err(e) = guard(|| app.move_focus(dx, dy)) {
                        bad.push(problem(&scen, "move focus", w, h, e));
                    }
                }
            }
            let _ = shot(&mut app, &format!("{scen}, after moving focus"), w, h, &mut bad);
        }
    }
    verdict(bad, KNOWN);
}

#[test]
fn qa_sizes_app_resize_with_splits() {
    // one workspace, the window dragged through every size (the tree stays, the screen changes)
    let mut bad = vec![];
    let (mut app, _rx) = new_app();
    split_tab(&mut app, 5, 120, 40, &mut bad);
    for _ in 0..2 {
        for &(w, h) in APP_SIZES {
            let _ = shot(&mut app, "5 splits, resizing", w, h, &mut bad);
        }
    }
    verdict(bad, KNOWN);
}

// ------------------------------------------------------------------ many of your own tabs, selections (here)

/// Was tab `i` listed in the sidebar on the last frame?
fn listed(app: &App, i: usize) -> bool {
    app.side_hits.iter().any(|(_, s)| matches!(s, SideHit::Tab(t) if *t == app.tabs[i].id))
}

fn click(app: &mut App, kind: MouseEventKind, x: u16, y: u16) {
    app.mouse(MouseEvent { kind, column: x, row: y, modifiers: KeyModifiers::NONE });
}

#[test]
fn qa_sizes_app_many_own_tabs() {
    let mut bad = vec![];
    for &(w, h) in APP_SIZES {
        let (mut app, _rx) = new_app();
        for i in 0..30 {
            app.new_tab(light_pane(i));
            let c = app.cur;
            app.tabs[c].name = Some(format!("{} {i}", AWKWARD[i % AWKWARD.len()]));
        }
        let user = app.user_tabs();
        for n in [0, 2, 14, 29] {
            let i = user[n];
            app.cur = i;
            let scen = format!("30 own tabs, #{} current", n + 1);
            // on a screen at least the usual 80x24 (sidebar shown), the tab you're on should be in the list
            if shot(&mut app, &scen, w, h, &mut bad).is_some() && w >= 70 && h >= 24 && !listed(&app, i) {
                bad.push(problem("own tabs", "sidebar list", w, h, format!("your tab #{} of 30 is the current one but isn't listed in the sidebar (the list doesn't scroll)", n + 1)));
            }
        }
        // closed one by one with the sidebar's × at this size (the help tab App::new opened stays)
        for _ in 0..40 {
            let Some(&(r, _)) = app.side_hits.iter().find(|(_, s)| matches!(s, SideHit::CloseTab(_))) else { break };
            if let Err(e) = guard(|| click(&mut app, MouseEventKind::Down(MouseButton::Left), r.x, r.y)) {
                bad.push(problem("own tabs", "close from the sidebar", w, h, e));
                break;
            }
            if shot(&mut app, "own tabs, closing", w, h, &mut bad).is_none() {
                break;
            }
        }
    }
    verdict(bad, KNOWN);
}

#[test]
fn qa_sizes_app_selection_then_resize() {
    // a text selection dragged out on a big screen, then the window shrinks (or grows) before and after the
    // mouse is let go: the selection keeps the last frame's geometry. The copy is a no-op under cfg(test).
    let mut bad = vec![];
    for &(w, h) in APP_SIZES {
        let (mut app, _rx) = new_app();
        app.new_tab(Box::new(crate::panes::help::Help::new()));
        let _ = shot(&mut app, "selection (before)", 250, 70, &mut bad);
        let Some(&(_, inner)) = app.inner.last() else { continue };
        let r = guard(|| {
            click(&mut app, MouseEventKind::Down(MouseButton::Left), inner.x + 2, inner.y + 1);
            click(&mut app, MouseEventKind::Drag(MouseButton::Left), inner.right() - 3, inner.bottom() - 2);
        });
        if let Err(e) = r {
            bad.push(problem("selection", "drag", 250, 70, e));
            continue;
        }
        let _ = shot(&mut app, "selecting, then resized", w, h, &mut bad);
        let _ = guard(|| click(&mut app, MouseEventKind::Up(MouseButton::Left), inner.right() - 3, inner.bottom() - 2)).map_err(|e| bad.push(problem("selection", "release", w, h, e)));
        // the "copied" toast covers frames on purpose
        let _ = shot_with(&mut app, "selection copied after resizing", w, h, &mut bad, false);
        app.notices.clear();
        let _ = shot(&mut app, "selection, back to full size", 250, 70, &mut bad);
    }
    verdict(bad, KNOWN);
}

// ------------------------------------------------------------------ popups (here)

/// Where ui::popup puts a w x h box on the screen.
fn popup_rect(screen: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(screen.width.saturating_sub(4));
    let h = h.min(screen.height.saturating_sub(2));
    Rect { x: screen.x + (screen.width - w) / 2, y: screen.y + screen.height.saturating_sub(h) / 3, width: w, height: h }
}

#[test]
fn qa_sizes_app_palette() {
    let mut bad = vec![];
    for &(w, h) in APP_SIZES {
        for (label, query, downs) in [("empty", "", 0), ("themes", "theme", 3), ("nothing matches", "zzqqxxjj", 0), ("last item", "", 200)] {
            let (mut app, _rx) = new_app();
            app.open_palette();
            for c in query.chars() {
                press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
            }
            for _ in 0..downs {
                press(&mut app, KeyCode::Down, KeyModifiers::NONE);
            }
            let scen = format!("palette ({label})");
            if let Some(b) = shot_with(&mut app, &scen, w, h, &mut bad, false) {
                if let Err(e) = frame_intact(&b, popup_rect(b.area, 64, 18), true) {
                    bad.push(problem(&scen, "frame", w, h, e));
                }
            }
        }
    }
    verdict(bad, KNOWN);
}

#[test]
fn qa_sizes_app_context_menus() {
    let mut bad = vec![];
    for &(w, h) in APP_SIZES {
        for (label, at) in [("pane, top-left", (0.0, 0.0)), ("pane, middle", (0.5, 0.5)), ("pane, bottom-right", (1.0, 1.0)), ("tab in sidebar", (-1.0, -1.0))] {
            let (mut app, _rx) = new_app();
            app.new_tab(Box::new(crate::panes::home::Home::new()));
            let _ = shot(&mut app, "menu (before)", w, h, &mut bad);
            let (x, y) = if at.0 < 0.0 {
                match app.side_hits.iter().find(|(_, s)| matches!(s, SideHit::Tab(_))) {
                    Some((r, _)) => (r.x + 3, r.y),
                    None => continue, // no sidebar at this size
                }
            } else {
                let Some(&(_, r)) = app.outer.first() else { continue };
                if r.width == 0 || r.height == 0 {
                    continue;
                }
                (r.x + ((r.width - 1) as f32 * at.0) as u16, r.y + ((r.height - 1) as f32 * at.1) as u16)
            };
            let scen = format!("right-click menu ({label})");
            if let Err(e) = guard(|| app.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Right), column: x, row: y, modifiers: KeyModifiers::NONE })) {
                bad.push(problem(&scen, "click", w, h, e));
                continue;
            }
            if app.ctx.is_none() {
                continue;
            }
            if let Some(b) = shot_with(&mut app, &scen, w, h, &mut bad, false) {
                let m = app.ctx.as_ref().unwrap();
                if let Err(e) = frame_intact(&b, m.rect, true) {
                    bad.push(problem(&scen, "frame", w, h, e));
                }
                // hovering and arrowing through it at this size
                let r = m.rect;
                let _ = guard(|| app.mouse(MouseEvent { kind: MouseEventKind::Moved, column: r.x + 1, row: r.bottom().saturating_sub(1), modifiers: KeyModifiers::NONE })).map_err(|e| bad.push(problem(&scen, "hover", w, h, e)));
                for _ in 0..8 {
                    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
                }
                if let Some(b) = shot_with(&mut app, &format!("{scen}, last item"), w, h, &mut bad, false) {
                    if let Some(m) = app.ctx.as_ref() {
                        if let Err(e) = frame_intact(&b, m.rect, true) {
                            bad.push(problem(&format!("{scen}, last item"), "frame", w, h, e));
                        }
                    }
                }
            }
        }
    }
    verdict(bad, KNOWN);
}

#[test]
fn qa_sizes_app_rename_toast_prefix() {
    let mut bad = vec![];
    for &(w, h) in APP_SIZES {
        // rename a tab of your own: a long awkward name, typed
        let (mut app, _rx) = new_app();
        app.new_tab(Box::new(crate::panes::home::Home::new()));
        let _ = shot(&mut app, "rename (before)", w, h, &mut bad);
        app.start_rename(app.cur);
        for _ in 0..20 {
            press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        }
        for c in AWKWARD[1].chars().chain(AWKWARD[2].chars()) {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        if let Some(b) = shot(&mut app, "rename", w, h, &mut bad) {
            // what you're typing has to be somewhere on screen: the field's cursor at least
            if w >= 20 && h >= 3 && !text(&b).contains('▏') {
                bad.push(problem("rename", "field", w, h, format!("renaming, but the name being typed isn't on screen (sidebar {})", if w >= 70 { "shown: the field scrolls off" } else { "hidden below 70 columns" })));
            }
        }
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let _ = shot(&mut app, "renamed tab", w, h, &mut bad);
        // a toast with a long message, then the prefix hint
        app.notify(AWKWARD.join(" · "));
        if let Some(b) = shot_with(&mut app, "toast", w, h, &mut bad, false) {
            let tw = (unicode_width::UnicodeWidthStr::width(format!(" {} ", AWKWARD.join(" · ")).as_str()) as u16 + 10).min(w);
            let r = Rect { x: w.saturating_sub(tw + 1), y: h.saturating_sub(4), width: tw, height: 3 };
            if let Err(e) = frame_intact(&b, r, true) {
                bad.push(problem("toast", "frame", w, h, e));
            }
        }
        app.notices.clear();
        app.prefix_armed = true;
        let _ = shot_with(&mut app, "prefix hint", w, h, &mut bad, false);
        app.prefix_armed = false;
        // an update waiting (one more row at the bottom of the sidebar), an app that isn't there
        app.update_ready = Some(format!("9.99.999-{}", AWKWARD[4]));
        app.goto_app("help");
        let _ = shot(&mut app, "update ready", w, h, &mut bad);
    }
    verdict(bad, KNOWN);
}

// ------------------------------------------------------------------ every app tab + onboarding (child process)

#[test]
fn qa_sizes_app_tabs() {
    let dir = qa::scratch("app-tabs");
    qa::run_child("app::qa_sizes_app::qa_sizes_child_app_tabs", &dir);
}

/// The data the app tabs show in the child: saved chats, a music library, alerts, files, all awkward.
fn seed(dir: &std::path::Path) {
    let chats = dir.join("data").join("chats");
    std::fs::create_dir_all(&chats).unwrap();
    let now = crate::alerts::now() as f64;
    for (i, t) in AWKWARD.iter().enumerate() {
        let c = serde_json::json!({"id": format!("q{i}"), "title": t, "created": now - i as f64 * 5000.0, "updated": now - i as f64 * 5000.0, "provider": "claude",
            "messages": [{"role": "user", "content": t}, {"role": "assistant", "content": AWKWARD.join("\n\n")}]});
        std::fs::write(chats.join(format!("q{i}.json")), c.to_string()).unwrap();
    }
    let music = dir.join("music");
    let mut tracks = vec![];
    for i in 0..30 {
        let p = music.join(format!("t{i:02}.mp3"));
        std::fs::write(&p, b"").unwrap();
        tracks.push(serde_json::json!({"id": format!("t{i}"), "path": p.to_string_lossy(), "title": AWKWARD[i % AWKWARD.len()], "artist": AWKWARD[(i + 1) % AWKWARD.len()], "duration": 200.0}));
    }
    std::fs::create_dir_all(dir.join("appdata").join("audio-player")).unwrap();
    std::fs::write(dir.join("appdata").join("audio-player").join("library.json"), serde_json::json!({"tracks": tracks, "playlists": [{"id": "p", "name": AWKWARD[0], "trackIds": ["t1"]}]}).to_string()).unwrap();
    for (i, t) in AWKWARD.iter().enumerate() {
        let safe: String = t.chars().filter(|c| !"\\/:*?\"<>|".contains(*c)).take(80).collect();
        std::fs::write(dir.join("cwd").join(format!("{safe}.txt")), t.repeat(i + 1)).unwrap();
    }
    for i in 0..40 {
        let kinds = [crate::alerts::Kind::AgentDone, crate::alerts::Kind::NeedsYou, crate::alerts::Kind::Usage, crate::alerts::Kind::Update];
        crate::alerts::push(crate::alerts::Alert { at: crate::alerts::now() - i * 900, kind: kinds[i as usize % 4], text: AWKWARD[i as usize % AWKWARD.len()].into(), app: Some("music".into()), read: false, pane: None });
    }
}

/// Every app tab (and the updates app, and your own tabs) at every size, sidebar shown and hidden.
fn tabs_sweep(dir: &std::path::Path, theme: &str, bad: &mut Vec<Problem>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut c = qa::quiet_config(Some(&dir.join("music")));
    c.theme = theme.into();
    c.startup = "help".into();
    let mut app = App::new(c, tx);
    // storage and "your AIs" read the real home folder whatever the environment says: fixture panes
    let tx = app.tx.clone();
    put_app(&mut app, "storage", crate::panes::storage::qa_sizes::app_pane(&tx));
    put_app(&mut app, "ais", crate::panes::ais::qa_sizes::app_pane());
    put_app(&mut app, "agents", crate::panes::agents::qa_sizes::app_pane());
    for &(name, ..) in SIDEBAR {
        app.goto_app(name);
    }
    app.goto_app("updates");
    // your own tabs: a named one, one with splits, one plain
    app.new_tab(Box::new(crate::panes::home::Home::new()));
    let t = app.cur;
    app.tabs[t].name = Some(AWKWARD[1].repeat(3));
    app.new_tab(Box::new(crate::panes::home::Home::new()));
    for i in 0..3 {
        let from = app.focused();
        app.open(light_pane(i), Place::SplitRight, from);
    }
    app.new_tab(Box::new(crate::panes::home::Home::new()));
    app.update_ready = Some("9.9.9".into());
    // let the panes load (chat list, music library, files, system samples)
    for &(w, h) in &[(120, 40), (250, 70)] {
        for i in 0..app.tabs.len() {
            app.cur = i;
            let _ = shot(&mut app, "loading", w, h, bad);
        }
    }
    settle(&mut app, &rx, 1500, bad);
    for i in 0..app.tabs.len() {
        app.cur = i;
        let label = app.tab_label(i);
        for &(w, h) in APP_SIZES {
            for side in [true, false] {
                app.sidebar = side;
                app.notices.clear();
                let scen = format!("tab '{label}' ({theme}{})", if side { "" } else { ", no sidebar" });
                let _ = shot(&mut app, &scen, w, h, bad);
            }
        }
        app.sidebar = true;
    }
}

#[test]
#[ignore = "run by qa_sizes_app_tabs in a child process with a scratch data folder"]
fn qa_sizes_child_app_tabs() {
    let Some(dir) = qa::child_dir() else { return };
    seed(&dir);
    let mut bad = vec![];
    tabs_sweep(&dir, "oriel", &mut bad);
    tabs_sweep(&dir, "ultra", &mut bad); // the animated theme: rainbow sidebar title, clock
    // and without a Nerd Font: plain-text icons are wider
    crate::ui::NERD.store(false, std::sync::atomic::Ordering::Relaxed);
    tabs_sweep(&dir, "oriel", &mut bad);
    verdict(bad, KNOWN);
}

#[test]
fn qa_sizes_app_onboarding() {
    let dir = qa::scratch("app-onboarding");
    qa::run_child("app::qa_sizes_app::qa_sizes_child_app_onboarding", &dir);
}

/// The tour card: found on screen by its title, then its frame checked (buttons spilling out break it).
fn tour_frame(b: &Buffer) -> Option<Rect> {
    for y in 0..b.area.height {
        for x in 1..b.area.width.saturating_sub(5) {
            if (0..6).map(|i| sym(b, x + i, y)).collect::<String>() == " tour " && sym(b, x - 1, y) == "╭" {
                let l = x - 1;
                let rt = (x..b.area.width).find(|&xx| sym(b, xx, y) == "╮")?;
                let bm = (y..b.area.height).find(|&yy| sym(b, l, yy) == "╰").unwrap_or(b.area.height - 1);
                return Some(Rect { x: l, y, width: rt - l + 1, height: bm - y + 1 });
            }
        }
    }
    None
}

fn onboard_step(app: &mut App) {
    let p = app.probe();
    let out = app.onboard.as_mut().map(|o| o.check(&p));
    if let Some(out) = out {
        app.onboard_out(out);
    }
}

/// Every onboarding page and tour step, each drawn at every size.
fn onboarding_sweep(dir: &std::path::Path, bad: &mut Vec<Problem>) {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut c = qa::quiet_config(Some(&dir.join("music")));
    c.theme = "ultra".into();
    c.startup = "help".into();
    let mut app = App::new(c, tx);
    app.start_tour();
    let sweep = |app: &mut App, page: &str, bad: &mut Vec<Problem>| {
        for &(w, h) in APP_SIZES {
            let Some(b) = shot_with(app, &format!("onboarding: {page}"), w, h, bad, false) else { continue };
            if page == "welcome" && h >= 8 && w >= 30 && !text(&b).contains("take the tour") {
                bad.push(problem("onboarding: welcome", "buttons", w, h, "the enter/skip buttons are off screen (the welcome is blank apart from its text)"));
            }
            if page.starts_with("tour") {
                match tour_frame(&b) {
                    Some(r) => {
                        if let Err(e) = frame_intact(&b, r, true) {
                            bad.push(problem(&format!("onboarding: {page}"), "tour card", w, h, e));
                        }
                    }
                    None if w >= 20 && h >= 6 => bad.push(problem(&format!("onboarding: {page}"), "tour card", w, h, "no tour card on screen")),
                    None => {}
                }
            }
        }
    };
    let k = |app: &mut App, c: KeyCode| {
        press(app, c, KeyModifiers::NONE);
        onboard_step(app);
    };
    sweep(&mut app, "welcome", bad);
    k(&mut app, KeyCode::Enter);
    sweep(&mut app, "theme", bad);
    for _ in 0..40 {
        k(&mut app, KeyCode::Down); // to the last theme (the list is two columns)
    }
    sweep(&mut app, "theme (last)", bad);
    k(&mut app, KeyCode::Enter);
    sweep(&mut app, "icons", bad);
    k(&mut app, KeyCode::Char('y'));
    sweep(&mut app, "music", bad);
    for c in AWKWARD[0].chars() {
        k(&mut app, KeyCode::Char(c));
    }
    sweep(&mut app, "music (long path)", bad);
    k(&mut app, KeyCode::Esc);
    sweep(&mut app, "notes", bad);
    for c in AWKWARD[1].chars() {
        k(&mut app, KeyCode::Char(c));
    }
    sweep(&mut app, "notes (long path)", bad);
    k(&mut app, KeyCode::Esc);
    sweep(&mut app, "defaults", bad);
    k(&mut app, KeyCode::Down);
    k(&mut app, KeyCode::Right);
    sweep(&mut app, "defaults (row 2)", bad);
    k(&mut app, KeyCode::Esc);
    sweep(&mut app, "your AIs", bad);
    k(&mut app, KeyCode::Enter);
    // the tour: 14 steps; F10 skips a step (on the help step F10 is the app's own key, which finishes it)
    for step in 1..=20 {
        let Some(crate::onboard::Stage::Tour { .. }) = app.onboard.as_ref().map(|o| &o.stage) else { break };
        sweep(&mut app, &format!("tour step {step}"), bad);
        k(&mut app, KeyCode::F(10));
    }
    k(&mut app, KeyCode::F(11));
    if app.onboard.is_some() {
        bad.push(problem("onboarding", "end", 0, 0, "F11 didn't end the tour"));
    }
}

#[test]
#[ignore = "run by qa_sizes_app_onboarding in a child process with a scratch %APPDATA%"]
fn qa_sizes_child_app_onboarding() {
    let Some(dir) = qa::child_dir() else { return };
    let mut bad = vec![];
    onboarding_sweep(&dir, &mut bad);
    verdict(bad, KNOWN);
}

// ------------------------------------------------------------------ known bugs, one test each (ignored: they fail)

#[test]
#[ignore = "fails: splitting the way alt n does at 80x24 crashes the app within ~12 splits: layout::split_rect panics on a 1-cell area (src/layout.rs:143, 148)"]
fn qa_sizes_bug_app_many_splits_crash() {
    let (mut app, _rx) = new_app();
    app.new_tab(Box::new(crate::panes::home::Home::new()));
    for i in 0..12 {
        draw(&mut app, 80, 24).unwrap_or_else(|e| panic!("after {i} splits: {e}"));
        let from = app.focused();
        app.open(Box::new(crate::panes::home::Home::new()), Place::Split, from);
    }
    draw(&mut app, 80, 24).unwrap_or_else(|e| panic!("after 12 splits: {e}"));
}

#[test]
#[ignore = "fails: a tab with two side-by-side panes crashes the app when the terminal is 1 column wide (src/layout.rs:143)"]
fn qa_sizes_bug_app_split_tab_one_column() {
    let (mut app, _rx) = new_app();
    app.new_tab(Box::new(crate::panes::home::Home::new()));
    let from = app.focused();
    app.open(Box::new(crate::panes::home::Home::new()), Place::SplitRight, from);
    draw(&mut app, 80, 24).unwrap();
    draw(&mut app, 1, 24).unwrap();
}

#[test]
#[ignore = "fails: the right-click menu draws all 6 items on an 80x5 screen: they go over its bottom border (src/app.rs:1409-1420)"]
fn qa_sizes_bug_context_menu_short_screen() {
    let (mut app, _rx) = new_app();
    app.new_tab(Box::new(crate::panes::home::Home::new()));
    draw(&mut app, 80, 5).unwrap();
    let r = app.outer[0].1;
    app.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Right), column: r.x + r.width / 2, row: r.y + 2, modifiers: KeyModifiers::NONE });
    let b = draw(&mut app, 80, 5).unwrap();
    let m = app.ctx.as_ref().expect("menu open");
    frame_intact(&b, m.rect, true).unwrap_or_else(|e| panic!("{e}\n{}", text(&b)));
}

#[test]
#[ignore = "fails: on a 20x6 screen the palette draws 'nothing matches' on its own bottom border (src/app.rs:1612)"]
fn qa_sizes_bug_palette_short_screen() {
    let (mut app, _rx) = new_app();
    app.open_palette();
    for c in "zzqq".chars() {
        press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
    }
    let b = draw(&mut app, 20, 6).unwrap();
    frame_intact(&b, popup_rect(b.area, 64, 18), true).unwrap_or_else(|e| panic!("{e}\n{}", text(&b)));
}

#[test]
#[ignore = "fails: renaming a tab on a screen under 70 columns is invisible: start_rename turns the sidebar on, but draw() only shows it from 70 columns (src/app.rs:593, 1319)"]
fn qa_sizes_bug_rename_invisible_when_narrow() {
    let (mut app, _rx) = new_app();
    app.new_tab(Box::new(crate::panes::home::Home::new()));
    draw(&mut app, 60, 20).unwrap();
    app.run_cmd(Cmd::Rename);
    for c in "qaname".chars() {
        press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
    }
    let b = draw(&mut app, 60, 20).unwrap();
    assert!(text(&b).contains("qaname"), "typing a tab name, but it's nowhere on screen:\n{}", text(&b));
}

#[test]
#[ignore = "fails: the tab rename field doesn't scroll: a 30-character name pushes the cursor (and the end of the name) out of the sidebar (src/app.rs:1533-1540)"]
fn qa_sizes_bug_rename_long_name_cut_off() {
    let (mut app, _rx) = new_app();
    app.new_tab(Box::new(crate::panes::home::Home::new()));
    draw(&mut app, 120, 40).unwrap();
    app.run_cmd(Cmd::Rename);
    for _ in 0..10 {
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    }
    for c in "a-thirty-character-tab-name-xy".chars() {
        press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
    }
    let b = draw(&mut app, 120, 40).unwrap();
    assert!(text(&b).contains("name-xy▏"), "the end of the name and its cursor are cut off:\n{}", text(&b).lines().take(30).collect::<Vec<_>>().join("\n"));
}

#[test]
#[ignore = "fails: on an 80x24 screen the sidebar lists only 2 of your own tabs and doesn't scroll, so with 3 the one you're on isn't in it (src/app.rs:1514-1517)"]
fn qa_sizes_bug_sidebar_hides_current_tab() {
    let (mut app, _rx) = new_app();
    for _ in 0..3 {
        app.new_tab(Box::new(crate::panes::home::Home::new()));
    }
    let b = draw(&mut app, 80, 24).unwrap();
    assert!(listed(&app, app.cur), "your third tab is the current one, but the sidebar doesn't list it:\n{}", text(&b));
}

#[test]
#[ignore = "fails: closing a tab while renaming another one moves the rename to the next tab over: it's kept as an index, so Enter renames the wrong tab (src/app.rs:164, 592, 837-838; close() at 410-429 never adjusts it)"]
fn qa_sizes_bug_rename_moves_when_a_tab_closes() {
    // found in passing: the mouse still works while renaming, and a middle-click on a tab in the sidebar closes it
    let (mut app, _rx) = new_app();
    for name in ["first", "second", "third"] {
        app.new_tab(Box::new(crate::panes::home::Home::new()));
        let c = app.cur;
        app.tabs[c].name = Some(name.into());
    }
    draw(&mut app, 120, 40).unwrap();
    let at = |app: &App, n: &str| app.tabs.iter().position(|t| t.name.as_deref() == Some(n)).unwrap();
    app.start_rename(at(&app, "second"));
    for _ in 0..10 {
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    }
    for c in "renamed".chars() {
        press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
    }
    let first = at(&app, "first");
    let (r, _) = *app.side_hits.iter().find(|(_, s)| matches!(s, SideHit::Tab(t) if *t == app.tabs[first].id)).expect("'first' listed");
    click(&mut app, MouseEventKind::Down(MouseButton::Middle), r.x + 3, r.y);
    let b = draw(&mut app, 120, 40).unwrap();
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let names: Vec<String> = app.tabs.iter().filter_map(|t| t.name.clone()).collect();
    assert_eq!(names, ["renamed", "third"], "renamed 'second' while 'first' was closed; the sidebar showed:\n{}", text(&b).lines().take(24).collect::<Vec<_>>().join("\n"));
}

#[test]
#[ignore = "fails: FocusGained/FocusLost never reach their arms: Event::Input(_) above them matches first, so term_focused never changes (desktop notifications never fire, alerts are always marked read) (src/app.rs:727 vs 755-756)"]
fn qa_sizes_bug_focus_events_ignored() {
    // found in passing: rustc also warns "unreachable pattern" on these two arms
    let (mut app, _rx) = new_app();
    app.handle(Event::Input(CEvent::FocusLost));
    assert!(!app.term_focused, "FocusLost was ignored");
}

#[test]
#[ignore = "fails: the tour card's buttons spill over its right border on a 40x10 screen (src/onboard.rs:655, 672-675)"]
fn qa_sizes_bug_tour_buttons_narrow() {
    qa::run_child("app::qa_sizes_app::qa_sizes_child_tour_buttons", &qa::scratch("tour-buttons"));
}

#[test]
#[ignore = "run by qa_sizes_bug_tour_buttons_narrow in a child process with a scratch %APPDATA%"]
fn qa_sizes_child_tour_buttons() {
    let Some(dir) = qa::child_dir() else { return };
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(qa::quiet_config(Some(&dir.join("music"))), tx);
    app.start_tour();
    for c in [KeyCode::Enter, KeyCode::Enter, KeyCode::Char('y'), KeyCode::Esc, KeyCode::Esc, KeyCode::Esc, KeyCode::Enter] {
        press(&mut app, c, KeyModifiers::NONE); // welcome, theme, icons, music, notes, defaults, your AIs -> tour
    }
    let b = draw(&mut app, 40, 10).unwrap();
    let r = tour_frame(&b).unwrap_or_else(|| panic!("no tour card:\n{}", text(&b)));
    frame_intact(&b, r, true).unwrap_or_else(|e| panic!("{e}\n{}", text(&b)));
}

#[test]
#[ignore = "fails: the welcome screen's buttons are off screen at 100x12, and at 40x10 it's blank: it lays out for 19 rows and reserves the logo's 10 even when the logo isn't drawn (src/onboard.rs:620-625)"]
fn qa_sizes_bug_welcome_buttons_short_screen() {
    qa::run_child("app::qa_sizes_app::qa_sizes_child_welcome_buttons", &qa::scratch("welcome-buttons"));
}

#[test]
#[ignore = "run by qa_sizes_bug_welcome_buttons_short_screen in a child process with a scratch %APPDATA%"]
fn qa_sizes_child_welcome_buttons() {
    let Some(dir) = qa::child_dir() else { return };
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(qa::quiet_config(Some(&dir.join("music"))), tx);
    app.start_tour();
    for (w, h) in [(100, 12), (40, 10)] {
        let b = draw(&mut app, w, h).unwrap();
        assert!(text(&b).contains("take the tour"), "{w}x{h}: the welcome's buttons aren't on screen:\n{}", text(&b));
    }
}

#[test]
#[ignore = "fails: a pane title longer than its frame runs under the × close button: at 80x24 with 4 panes the help pane's top reads '╭ 󰍉 help  × e╮' (the title isn't shortened to make room: src/ui.rs:90-93, src/app.rs:1348-1356)"]
fn qa_sizes_bug_title_under_close_button() {
    let (mut app, _rx) = new_app();
    split_tab(&mut app, 3, 80, 24, &mut vec![]);
    let b = draw(&mut app, 80, 24).unwrap();
    for &(bx, _) in &app.pane_close {
        let top: String = (bx.x.saturating_sub(12)..(bx.right() + 2).min(b.area.width)).map(|x| sym(&b, x, bx.y)).collect();
        assert_eq!(sym(&b, bx.right(), bx.y), "─", "title text between the × and the frame's corner: {top:?}\n{}", text(&b));
    }
}
