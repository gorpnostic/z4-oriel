//! QA: first run through the whole App (a child module of app.rs, for its private state). Builds App the way
//! main.rs does, replays the setup + tour with real key and mouse events, and checks what each page does to the
//! app's config, theme, tabs and notices.
//!
//! Headless and silent: TestBackend only; the config keeps music/notes/themes in target/test-scratch/qa-first-run
//! (see qa_first_run::cfg_in); startup is the home screen, and the tour's app-switching steps are skipped with F10
//! rather than opening chat/music/agents (which would read the real data folder or open an audio device). The
//! "split it" step splits with a home pane instead of a real shell. config::save and onboard::mark_done are
//! no-ops under cfg(test), so nothing is written outside the scratch folder.

use super::*;
use crate::onboard::Stage;
use crate::qa_first_run::{cfg_in, scratch};
use ratatui::{Terminal, backend::TestBackend};

struct T {
    app: App,
    _rx: std::sync::mpsc::Receiver<Event>,
    cfg: Config,
}

fn new_app(name: &str, tweak: impl FnOnce(&mut Config)) -> T {
    let d = scratch(name);
    let mut cfg = cfg_in(&d);
    tweak(&mut cfg);
    let (tx, rx) = std::sync::mpsc::channel();
    T { app: App::new(cfg.clone(), tx), _rx: rx, cfg }
}

fn key(app: &mut App, code: KeyCode) {
    app.key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn key_mod(app: &mut App, code: KeyCode, m: KeyModifiers) {
    app.key(KeyEvent::new(code, m));
}

/// What App::run does after every event batch: let the tour look at the app and move on.
fn settle(app: &mut App) {
    if app.onboard.is_some() {
        let p = app.probe();
        let out = app.onboard.as_mut().unwrap().check(&p);
        app.onboard_out(out);
    }
}

fn shot(app: &mut App, w: u16, h: u16) -> (String, ratatui::buffer::Buffer) {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| app.draw(f)).unwrap();
    let b = term.backend().buffer().clone();
    let s = (0..b.area.height).map(|y| (0..b.area.width).map(|x| b[(x, y)].symbol()).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n");
    (s, b)
}

fn find(buf: &ratatui::buffer::Buffer, pat: &str) -> Option<(u16, u16)> {
    let pc: Vec<String> = pat.chars().map(|c| c.to_string()).collect();
    for y in 0..buf.area.height {
        let row: Vec<&str> = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        for x0 in 0..row.len().saturating_sub(pc.len() - 1) {
            if pc.iter().enumerate().all(|(i, c)| row[x0 + i] == c) {
                return Some((x0 as u16, y));
            }
        }
    }
    None
}

fn click(app: &mut App, btn: MouseButton, x: u16, y: u16) {
    app.mouse(MouseEvent { kind: MouseEventKind::Down(btn), column: x, row: y, modifiers: KeyModifiers::NONE });
}

fn stage(app: &App) -> String {
    match app.onboard.as_ref().map(|o| &o.stage) {
        None => "off".into(),
        Some(Stage::Welcome) => "welcome".into(),
        Some(Stage::Theme { .. }) => "theme".into(),
        Some(Stage::Icons) => "icons".into(),
        Some(Stage::Music { .. }) => "music".into(),
        Some(Stage::Notes { .. }) => "notes".into(),
        Some(Stage::Defaults { .. }) => "defaults".into(),
        Some(Stage::Ais) => "ais".into(),
        Some(Stage::Tour { step, .. }) => format!("tour {step}"),
    }
}

/// Enter through the setup pages (y on icons) until the tour starts.
fn to_tour(app: &mut App) {
    for _ in 0..10 {
        if stage(app).starts_with("tour") {
            return;
        }
        let code = if stage(app) == "icons" { KeyCode::Char('y') } else { KeyCode::Enter };
        key(app, code);
        settle(app);
    }
    panic!("never reached the tour: {}", stage(app));
}

// ------------------------------------------------------------------ construction

/// App::new opens whatever `startup` names: an app from the sidebar, the home screen, or home for anything it
/// doesn't know. Tests never get the first-run screen by themselves (it's gated off under cfg(test)).
#[test]
fn app_new_opens_the_startup_choice() {
    for (startup, app_tab, title) in [("home", None, "home"), ("", None, "home"), ("no-such-app", None, "home"), ("help", Some("help"), "")] {
        let mut t = new_app("new-startup", |c| c.startup = startup.into());
        let a = &mut t.app;
        assert_eq!(a.tabs.len(), 1, "startup {startup:?}: one tab");
        assert_eq!(a.tabs[a.cur].app, app_tab, "startup {startup:?}");
        if !title.is_empty() {
            let id = a.focused();
            assert_eq!(a.panes[&id].title(), title, "startup {startup:?} opens the home screen");
        }
        assert!(a.onboard.is_none(), "no first-run screen under test");
        assert!(a.notices.is_empty() && a.palette.is_none() && !a.quit);
        assert_eq!(a.theme.name, "ultra");
        let (s, _) = shot(a, 120, 36);
        assert!(s.contains("oriel"), "startup {startup:?} draws:\n{s}");
    }
    // every theme the setup offers can be the one App starts with
    for name in crate::theme::names() {
        let t = new_app("new-theme", |c| c.theme = name.clone());
        assert_eq!(t.app.theme.name, name, "App::new with theme {name}");
    }
}

// ------------------------------------------------------------------ the setup through the app

/// Each setup page writes its choice into the app's config (what config::save would write), the theme page
/// previews live, and finishing leaves the welcome notice. The app itself stays where it was.
#[test]
fn app_setup_saves_each_choice() {
    let mut t = new_app("app-setup", |_| {});
    let music = t.cfg.music.folders[0].clone();
    let a = &mut t.app;
    let tabs_before = a.tabs.len();
    a.start_tour();
    assert_eq!(stage(a), "welcome");
    let (s, _) = shot(a, 150, 42);
    assert!(s.contains("take the tour") && s.contains("a window onto everything"), "{s}");

    key(a, KeyCode::Enter);
    assert_eq!(stage(a), "theme");
    key(a, KeyCode::Down);
    let previewed = a.theme.name.clone();
    assert_ne!(previewed, "ultra", "moving previews the next theme");
    assert_eq!(a.config.theme, "ultra", "previewing doesn't save");
    key(a, KeyCode::Enter);
    assert_eq!((a.config.theme.as_str(), a.theme.name.as_str()), (previewed.as_str(), previewed.as_str()), "enter keeps the previewed theme");

    assert_eq!(stage(a), "icons");
    key(a, KeyCode::Char('y'));
    assert!(!a.config.plain_icons);

    assert_eq!(stage(a), "music");
    key(a, KeyCode::Enter);
    assert_eq!(a.config.music.folders, vec![music.clone()]);

    assert_eq!(stage(a), "notes");
    key_mod(a, KeyCode::Char('u'), KeyModifiers::CONTROL);
    let notes = scratch("app-setup-notes").join("vault").to_string_lossy().to_string();
    for c in notes.chars() {
        key(a, KeyCode::Char(c));
    }
    key(a, KeyCode::Enter);
    assert_eq!(a.config.notes_folder, notes);

    assert_eq!(stage(a), "defaults");
    key(a, KeyCode::Right); // chat -> agents
    key(a, KeyCode::Enter);
    assert_eq!(a.config.startup, "agents");
    let avail = crate::panes::chat::providers::available(&t.cfg.ai);
    assert_eq!(a.config.ai.provider, avail.first().map(|s| s.to_string()).unwrap_or_default());

    assert_eq!(stage(a), "ais");
    key(a, KeyCode::Enter);
    assert_eq!(stage(a), "tour 0");
    assert_eq!(a.tabs.len(), tabs_before, "the setup doesn't open anything: startup applies next launch");
    key(a, KeyCode::F(11));
    assert_eq!(stage(a), "off");
    assert!(a.notices.iter().any(|n| n.text.contains("welcome to oriel")), "{:?}", a.notices.iter().map(|n| &n.text).collect::<Vec<_>>());
}

/// Esc on the theme page puts back the look you had, and saves nothing.
#[test]
fn app_esc_on_theme_page_restores_the_look() {
    let mut t = new_app("app-theme-esc", |c| c.theme = "ocean".into());
    let a = &mut t.app;
    a.start_tour();
    key(a, KeyCode::Enter);
    key(a, KeyCode::Down);
    key(a, KeyCode::Down);
    assert_ne!(a.theme.name, "ocean");
    key(a, KeyCode::Esc);
    assert_eq!((a.theme.name.as_str(), a.config.theme.as_str()), ("ocean", "ocean"));
    assert_eq!(stage(a), "icons");
}

/// While a setup page is up, nothing reaches the app: no palette, no app keys, no prefix, no quitting, and clicks
/// on the sidebar do nothing.
#[test]
fn app_setup_pages_are_modal() {
    let mut t = new_app("app-modal", |_| {});
    let a = &mut t.app;
    a.start_tour();
    let (tabs, cur) = (a.tabs.len(), a.cur);
    for page in ["welcome", "theme", "icons", "music", "notes", "defaults", "ais"] {
        // walk to the page (from the welcome: enter, then y/enter)
        while stage(a) != page {
            let code = if stage(a) == "icons" { KeyCode::Char('y') } else { KeyCode::Enter };
            key(a, code);
        }
        let _ = shot(a, 150, 42);
        key_mod(a, KeyCode::Char('p'), KeyModifiers::ALT);
        key_mod(a, KeyCode::Char('t'), KeyModifiers::ALT);
        key(a, KeyCode::F(4));
        key(a, KeyCode::F(10));
        key_mod(a, KeyCode::Char(' '), KeyModifiers::CONTROL);
        key(a, KeyCode::Char('q'));
        click(a, MouseButton::Left, 3, 5); // the sidebar's app list
        click(a, MouseButton::Right, 60, 20);
        a.mouse(MouseEvent { kind: MouseEventKind::ScrollDown, column: 60, row: 20, modifiers: KeyModifiers::NONE });
        assert!(a.palette.is_none(), "{page}: alt p opened the palette under the setup");
        assert!(!a.quit && !a.prefix_armed && a.ctx.is_none(), "{page}: a key or click got through");
        assert_eq!((a.tabs.len(), a.cur), (tabs, cur), "{page}: tabs changed under the setup");
        assert_eq!(stage(a), page, "{page}: stray keys moved the setup on");
        // the text fields took the q (that's typing), undo it so enter still walks on
        if page == "music" || page == "notes" {
            key(a, KeyCode::Backspace);
        }
    }
}

/// Replaying the setup (palette → take the tour, oriel --tour) and pressing enter on the music page must not
/// throw away the other music folders in config (the page only shows the first).
#[test]
#[ignore = "fails: onboard.rs:283 shows only folders[0] and app.rs:340-342 replaces the whole list with it on enter"]
fn app_replaying_setup_keeps_every_music_folder() {
    let d = scratch("app-music-two");
    let second = d.join("more-music");
    std::fs::create_dir_all(&second).unwrap();
    let second = second.to_string_lossy().to_string();
    let mut t = new_app("app-music-two-cfg", |c| c.music.folders.push(second.clone()));
    let folders = t.cfg.music.folders.clone();
    assert_eq!(folders.len(), 2);
    let a = &mut t.app;
    a.start_tour();
    while stage(a) != "music" {
        let code = if stage(a) == "icons" { KeyCode::Char('y') } else { KeyCode::Enter };
        key(a, code);
    }
    key(a, KeyCode::Enter); // keep what it shows
    assert_eq!(a.config.music.folders, folders, "enter on an untouched music page dropped a library folder");
}

// ------------------------------------------------------------------ the tour through the app

/// The whole tour with the app's own keys and clicks: each step waits for its action and moves on when the
/// app's state (the probe) shows it happened.
#[test]
fn app_tour_end_to_end() {
    let mut t = new_app("app-tour", |_| {});
    let a = &mut t.app;
    a.start_tour();
    to_tour(a);
    let expect = |a: &mut App, n: usize, title: &str| {
        settle(a);
        assert_eq!(stage(a), format!("tour {n}"), "expected step {} '{title}'", n + 1);
        let (s, _) = shot(a, 150, 42);
        assert!(s.contains(&format!("tour · {}/14 · {title}", n + 1)), "step {}:\n{s}", n + 1);
    };
    expect(a, 0, "switch apps");
    // the app-switching steps: skipped with F10 (their own app keys are covered in the onboard tests)
    for (n, title) in [(1, "chat with any AI"), (2, "your AIs"), (3, "a team of agents")] {
        key(a, KeyCode::F(10));
        expect(a, n, title);
    }
    assert!(a.tabs.iter().all(|t| t.app != Some("help")), "F10 as 'skip step' must not also open help");
    key(a, KeyCode::F(10));
    expect(a, 4, "the palette");
    key_mod(a, KeyCode::Char('p'), KeyModifiers::ALT);
    assert!(a.palette.is_some(), "alt p goes through to the app during the tour");
    expect(a, 4, "the palette"); // still open: waits for esc
    key(a, KeyCode::Esc);
    expect(a, 5, "a tab of your own");
    let tabs = a.tabs.len();
    key_mod(a, KeyCode::Char('t'), KeyModifiers::ALT);
    assert_eq!(a.tabs.len(), tabs + 1);
    expect(a, 6, "split it");
    let id = a.focused();
    a.apply(id, vec![Action::Open(Box::new(crate::panes::home::Home::new()), Place::Split)]);
    expect(a, 7, "move between panes");
    let _ = shot(a, 150, 42); // pane geometry for alt+arrows
    let f0 = a.focused();
    key_mod(a, KeyCode::Left, KeyModifiers::ALT);
    if a.focused() == f0 {
        key_mod(a, KeyCode::Up, KeyModifiers::ALT);
    }
    assert_ne!(a.focused(), f0, "alt+arrow moved focus");
    expect(a, 8, "name the tab");
    key_mod(a, KeyCode::Char(' '), KeyModifiers::CONTROL);
    key(a, KeyCode::Char(','));
    assert!(a.renaming.is_some(), "prefix , renames during the tour");
    for _ in 0..45 {
        key(a, KeyCode::Backspace);
    }
    for c in "qa tab".chars() {
        key(a, KeyCode::Char(c));
    }
    key(a, KeyCode::Enter);
    assert_eq!(a.tabs[a.cur].name.as_deref(), Some("qa tab"));
    expect(a, 9, "right-click");
    let _ = shot(a, 150, 42);
    let (_, r) = a.outer[0];
    click(a, MouseButton::Right, r.x + 3, r.y + 2);
    assert!(a.ctx.is_some(), "right-click menu opened");
    expect(a, 10, "copy, paste, close");
    key(a, KeyCode::Esc); // closes the menu (the tour lets esc through)
    assert!(a.ctx.is_none());
    key(a, KeyCode::Enter);
    expect(a, 11, "agents look after themselves");
    key(a, KeyCode::Enter);
    expect(a, 12, "the help screen");
    key(a, KeyCode::F(10));
    assert_eq!(a.tabs[a.cur].app, Some("help"), "F10 on the help step is the app's own key");
    expect(a, 13, "you're set");
    key(a, KeyCode::Enter);
    assert_eq!(stage(a), "off");
    assert!(a.notices.iter().any(|n| n.text.contains("welcome to oriel")));
}

/// The welcome's buttons work through App::mouse (their rects come from the last draw), and during the tour a
/// click outside the box belongs to the app.
#[test]
fn app_welcome_and_tour_clicks() {
    let mut t = new_app("app-clicks", |_| {});
    let a = &mut t.app;
    a.start_tour();
    let (_, buf) = shot(a, 150, 42);
    let (x, y) = find(&buf, "enter  take the tour").expect("tour button");
    click(a, MouseButton::Left, x + 3, y);
    assert_eq!(stage(a), "theme", "the welcome's tour button starts the setup");
    a.start_tour();
    let (_, buf) = shot(a, 150, 42);
    let (x, y) = find(&buf, "s  skip").expect("skip button");
    click(a, MouseButton::Left, x + 1, y);
    assert_eq!(stage(a), "off", "skip ends it");
    assert!(a.notices.iter().any(|n| n.text.contains("welcome to oriel")));

    a.start_tour();
    to_tour(a);
    let (_, buf) = shot(a, 150, 42);
    let (x, y) = find(&buf, "F10  skip step").expect("skip-step button");
    click(a, MouseButton::Left, x + 1, y);
    settle(a);
    assert_eq!(stage(a), "tour 1");
    // a right-click on the pane, well away from the box, is the app's
    let _ = shot(a, 150, 42);
    let (_, r) = a.outer[0];
    click(a, MouseButton::Right, r.x + 2, r.y + 1);
    assert!(a.ctx.is_some(), "clicks outside the tour box reach the app");
}

/// The tour box floats over the app, so a click on its text (not a button) shouldn't land in the pane under it.
#[test]
#[ignore = "fails: onboard.rs:474 returns is_modal() (false in the tour) for clicks inside the box, so they reach the pane below"]
fn app_click_on_tour_box_text_stays_in_the_box() {
    let mut t = new_app("app-box-click", |_| {});
    let a = &mut t.app;
    a.start_tour();
    to_tour(a);
    let (_, buf) = shot(a, 150, 42);
    let (x, y) = find(&buf, "Everything lives").expect("step 1 text");
    let ctx_before = a.ctx_seen;
    click(a, MouseButton::Left, x + 2, y);
    let started_selection = a.sel.is_some();
    click(a, MouseButton::Right, x + 2, y);
    assert!(!started_selection && a.ctx.is_none() && a.ctx_seen == ctx_before, "a click on the tour box's text went to the pane underneath (selection: {started_selection}, menu: {})", a.ctx.is_some());
}

/// "Replay this tour from the palette (take the tour)": the palette item restarts the welcome, even mid-tour.
#[test]
fn app_palette_take_the_tour() {
    let mut t = new_app("app-palette-tour", |_| {});
    let a = &mut t.app;
    key_mod(a, KeyCode::Char('p'), KeyModifiers::ALT);
    for c in "take the tour".chars() {
        key(a, KeyCode::Char(c));
    }
    let (s, _) = shot(a, 150, 42);
    assert!(s.contains("take the tour"), "{s}");
    key(a, KeyCode::Enter);
    assert_eq!(stage(a), "welcome");
    assert!(a.palette.is_none());
    // again from inside the tour (the palette is allowed there)
    to_tour(a);
    key_mod(a, KeyCode::Char('p'), KeyModifiers::ALT);
    for c in "take the tour".chars() {
        key(a, KeyCode::Char(c));
    }
    key(a, KeyCode::Enter);
    assert_eq!(stage(a), "welcome", "take the tour mid-tour starts over");
}

/// The whole app with every onboarding screen over it draws at any size without panicking.
#[test]
fn app_draws_onboarding_at_any_size() {
    let mut t = new_app("app-sizes", |_| {});
    let a = &mut t.app;
    let mut fails = vec![];
    a.start_tour();
    let mut screens = 0;
    loop {
        for (w, h) in [(150u16, 42u16), (80, 24), (60, 16), (40, 12), (20, 6), (8, 3), (1, 1)] {
            let st = stage(a);
            if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                shot(a, w, h);
            })) {
                let msg = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                fails.push(format!("{st} at {w}x{h}: {msg}"));
            }
        }
        screens += 1;
        if stage(a) == "off" || screens > 30 {
            break;
        }
        // on to the next screen; in the tour, F10 skips (and on the help step opens help, which finishes it)
        let code = match stage(a).as_str() {
            "icons" => KeyCode::Char('y'),
            s if s.starts_with("tour") => {
                let n: usize = s[5..].parse().unwrap();
                if n == 13 || n == 10 || n == 11 { KeyCode::Enter } else { KeyCode::F(10) }
            }
            _ => KeyCode::Enter,
        };
        key(a, code);
        settle(a);
    }
    assert!(fails.is_empty(), "draw panicked:\n{}", fails.join("\n"));
    assert_eq!(screens, 1 + 6 + 14 + 1, "welcome, six setup pages, fourteen tour steps, then the app on its own");
}

/// The icons page says: pick plain text now, "install a Nerd Font and switch back from the palette later". The
/// palette's switch has to stick like the setup's choice does, or the next launch is back to plain text.
#[test]
#[ignore = "fails: app.rs:1034-1038 Cmd::Icons flips ui::NERD but never sets config.plain_icons or saves"]
fn palette_icon_switch_is_remembered() {
    let mut t = new_app("app-icons", |c| c.plain_icons = false);
    let a = &mut t.app;
    // flip once and straight back: ui::NERD is global and other tests render with it
    let before = crate::ui::NERD.load(std::sync::atomic::Ordering::Relaxed);
    a.config.plain_icons = !before;
    a.run_cmd(Cmd::Icons);
    let after = crate::ui::NERD.load(std::sync::atomic::Ordering::Relaxed);
    let saved = a.config.plain_icons;
    crate::ui::NERD.store(before, std::sync::atomic::Ordering::Relaxed);
    assert_ne!(after, before, "the palette item flips the icons");
    assert_eq!(saved, !after, "palette 'toggle nerd font icons' changed the icons but not config.plain_icons, so the next launch undoes it");
}
