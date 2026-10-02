//! QA: corrupt and edge-case saved data. Every loader here must survive empty files, truncated JSON/TOML, wrong
//! types, huge numbers, unicode (emoji, RTL, wide CJK, combining marks), very long text, missing folders and
//! read-only files, and fall back without panicking or throwing the user's data away.
//!
//! Everything writes under target/test-scratch/qa-data/. Modules whose files hang off data_dir() or a cfg(test)
//! path relative to the working folder are tested in a child process (this same test binary, one test, with its
//! working folder and ORIEL_DATA_DIR pointed into scratch) — see `run_child`. Tests that document a real bug
//! are `#[ignore = "fails: ..."]` so the suite stays green; run them with `cargo test qa_ -- --ignored`.

#![cfg(test)]

use crate::alerts::{self, Alert, Kind};
use crate::pane::Pane;
use crate::panes::calendar::{self, Calendar};
use crate::panes::files::clock;
use crate::testkit::Kit;
use crate::theme;
use crate::update;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::path::{Path, PathBuf};

// ------------------------------------------------------------------ helpers (shared with the pane qa modules)

/// A fresh folder under target/test-scratch/qa-data.
pub(crate) fn scratch(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-scratch").join("qa-data").join(format!("{name}-{nanos}"));
    std::fs::create_dir_all(&d).unwrap();
    d
}

const CHILD: &str = "ORIEL_QA_CHILD";

/// True only inside the child process `run_child` started for exactly this test. Child tests are no-ops in the
/// normal run, so they can never touch the real data folder.
pub(crate) fn is_child(test: &str) -> bool {
    std::env::var(CHILD).ok().as_deref() == Some(test) && std::env::var_os("ORIEL_DATA_DIR").is_some()
}

/// Run one test of this binary in a child process: working folder = `dir` (so cfg(test) paths such as
/// "target/test-scratch/alerts.json" resolve inside it) and ORIEL_DATA_DIR = `dir/data`. Err(output) if it
/// failed or didn't run.
pub(crate) fn run_child(test: &str, dir: &Path) -> Result<String, String> {
    std::fs::create_dir_all(dir.join("target").join("test-scratch")).unwrap();
    std::fs::create_dir_all(dir.join("data")).unwrap();
    let mut c = std::process::Command::new(std::env::current_exe().unwrap());
    c.args([test, "--exact", "--nocapture", "--test-threads=1"]).current_dir(dir).env("ORIEL_DATA_DIR", dir.join("data")).env(CHILD, test);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x08000000);
    }
    let out = c.output().map_err(|e| e.to_string())?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        return Err(text);
    }
    if !text.contains("1 passed") {
        return Err(format!("the child test didn't run:\n{text}"));
    }
    Ok(text)
}

/// Run a child and fail with its output (the panic message is in there).
pub(crate) fn child_ok(test: &str, name: &str) {
    let d = scratch(name);
    if let Err(out) = run_child(test, &d) {
        let msg: Vec<&str> = out.lines().filter(|l| l.contains("panicked") || l.contains("assert") || l.starts_with("  ") || l.contains("fails") || l.contains("left") || l.contains("right")).collect();
        panic!("child {test} failed:\n{}\n---- full output ----\n{out}", msg.join("\n"));
    }
}

/// Makes a file read-only; puts it back when dropped (even if the test panics) so scratch can be cleaned.
pub(crate) struct ReadOnly(PathBuf);

impl ReadOnly {
    pub(crate) fn set(p: &Path) -> ReadOnly {
        let mut perm = std::fs::metadata(p).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(p, perm).unwrap();
        ReadOnly(p.to_path_buf())
    }
}

impl Drop for ReadOnly {
    fn drop(&mut self) {
        if let Ok(m) = std::fs::metadata(&self.0) {
            let mut perm = m.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(false);
            let _ = std::fs::set_permissions(&self.0, perm);
        }
    }
}

/// Control characters (other than the line breaks `dump` adds) that made it into a rendered screen: a terminal
/// would act on them (ESC starts an escape sequence).
pub(crate) fn controls(screen: &str) -> Vec<char> {
    screen.chars().filter(|c| c.is_control() && *c != '\n').collect()
}

/// Nasty strings for titles and text.
pub(crate) fn nasty() -> Vec<String> {
    vec![
        "🎂 birthday 👨‍👩‍👧‍👦 party 🏳️‍🌈".into(),
        "موعد الطبيب في المستشفى".into(),              // RTL Arabic
        "שלום עולם mixed with english".into(),        // RTL Hebrew + LTR
        "歯医者の予約 · 東京駅で待ち合わせ".into(),    // wide CJK
        "e\u{0301}\u{0301}\u{0301} zalgo z\u{0336}\u{0337}a\u{0338}l\u{0335}g\u{0334}o".into(), // combining marks
        "zero\u{200b}width\u{200d}joiner\u{feff}bom".into(),
        "x".repeat(5000),
        "🎉".repeat(2000),
        String::new(),
        "   ".into(),
    ]
}

/// Strings that carry control characters (a hand-edited or pasted file can hold these).
fn with_controls() -> Vec<String> {
    vec!["tab\there".into(), "line\nbreak".into(), "carriage\rreturn".into(), "esc \u{1b}[31mred\u{1b}[0m".into(), "bell\u{7}".into(), "nul\u{0}byte".into()]
}

fn today() -> i64 {
    let l = clock::local(clock::now_secs());
    calendar::days_from_civil(l.year as i64, l.month, l.day)
}

/// Contents that are not a valid file of any of our JSON shapes.
pub(crate) fn corrupt_json() -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = ["", " ", "\n\n", "{", "[", "[{\"day\":1,", "{\"id\":", "null", "true", "42", "\"just a string\"", "{}", "[]", "[1,2,3]", "[null]", "[[[]]]", "{\"a\":}", "\u{feff}[]"]
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
    v.push(vec![0xff, 0xfe, 0x00, 0x5b, 0x00, 0x5d]); // UTF-16 "[]" with a BOM (Notepad's old "Unicode")
    v.push(vec![0x80, 0x81, 0x82, b'{', b'}']); // not UTF-8
    v.push("[".repeat(10_000).into_bytes()); // deep nesting: must not blow the stack
    v.push(format!("[{}]", "{\"x\":1},".repeat(20_000) + "{}").into_bytes()); // big and wrong
    v
}

// ------------------------------------------------------------------ calendar.json

#[test]
fn qa_calendar_corrupt_files_load_empty_and_render() {
    let d = scratch("cal-corrupt");
    for (i, bytes) in corrupt_json().into_iter().enumerate() {
        let path = d.join(format!("cal-{i}.json"));
        std::fs::write(&path, &bytes).unwrap();
        let mut k = Kit::new();
        let mut c = Calendar::open_at(path.clone());
        for (w, h) in [(140, 40), (80, 24), (20, 6), (1, 1)] {
            let s = k.render(&mut c, w, h);
            if w >= 100 {
                assert!(s.contains("nothing planned"), "case {i}: corrupt file shows no plans\n{s}");
            }
        }
        k.render_side(&mut c, 34, 12);
        k.poll(&mut c);
        for code in [KeyCode::Char('j'), KeyCode::Char('e'), KeyCode::Char('x'), KeyCode::Char('u'), KeyCode::Char(']'), KeyCode::Char('[')] {
            k.key(&mut c, code);
        }
        // nothing was changed, so the file must be exactly as it was
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "case {i}: just looking at a corrupt calendar must not rewrite it");
    }
}

#[test]
#[ignore = "fails: one bad plan makes the whole calendar.json load as empty, and the next add overwrites every plan"]
fn qa_calendar_one_bad_plan_then_add_wipes_file() {
    let d = scratch("cal-onebad");
    let path = d.join("calendar.json");
    let t = today();
    // three good plans, and one a newer version or a hand edit got slightly wrong (a negative time)
    let json = format!(
        r#"[{{"day":{t},"text":"keep one"}},{{"day":{t},"at":600,"text":"keep two"}},{{"day":{},"text":"keep three"}},{{"day":{t},"at":-5,"text":"bad"}}]"#,
        t + 1
    );
    std::fs::write(&path, &json).unwrap();
    let mut k = Kit::new();
    let mut c = Calendar::open_at(path.clone());
    let s = k.render(&mut c, 140, 40);
    let shown = s.contains("keep one") && s.contains("keep two");
    k.key(&mut c, KeyCode::Char('a'));
    k.typ(&mut c, "new plan");
    k.key(&mut c, KeyCode::Enter);
    let disk = std::fs::read_to_string(&path).unwrap();
    let kept = disk.contains("keep one") && disk.contains("keep two") && disk.contains("keep three");
    assert!(shown && kept, "good plans shown after load: {shown}; still on disk after adding one plan: {kept}\nfile now:\n{disk}");
}

#[test]
#[ignore = "fails: a plan time of u32::MAX in calendar.json panics with 'attempt to add with overflow' (debug builds)"]
fn qa_calendar_huge_time_value() {
    let d = scratch("cal-hugeat");
    let path = d.join("calendar.json");
    let t = today();
    std::fs::write(&path, format!(r#"[{{"day":{t},"at":4294967295,"text":"overflow"}}]"#)).unwrap();
    let mut k = Kit::new();
    let mut c = Calendar::open_at(path);
    k.render(&mut c, 140, 40); // plan_line: a + 60
    k.render_side(&mut c, 34, 12);
    k.poll(&mut c); // reminders: at + 10_000
}

#[test]
fn qa_calendar_extreme_days_and_out_of_range_times() {
    let d = scratch("cal-days");
    let path = d.join("calendar.json");
    let t = today();
    let json = format!(
        r#"[{{"day":9223372036854775807,"text":"max"}},{{"day":-9223372036854775808,"text":"min"}},{{"day":-1000000,"text":"ancient"}},
            {{"day":10000000,"text":"far future"}},{{"day":{t},"at":1500,"text":"25 o'clock"}},{{"day":{t},"at":0,"text":"midnight"}}]"#
    );
    std::fs::write(&path, json).unwrap();
    let mut k = Kit::new();
    let mut c = Calendar::open_at(path);
    let s = k.render(&mut c, 140, 40);
    assert!(s.contains("midnight"), "{s}");
    k.render_side(&mut c, 34, 12);
    k.poll(&mut c);
    for code in [KeyCode::Char(']'), KeyCode::Char('['), KeyCode::PageDown, KeyCode::Up, KeyCode::Down, KeyCode::Left, KeyCode::Right, KeyCode::Char('t'), KeyCode::Char('j'), KeyCode::Char('j')] {
        k.key(&mut c, code);
        k.render(&mut c, 100, 30);
    }
    // a lot of months back and forth
    for _ in 0..600 {
        k.key(&mut c, KeyCode::Char('['));
    }
    k.render(&mut c, 140, 40);
    for _ in 0..1200 {
        k.key(&mut c, KeyCode::Char(']'));
    }
    k.render(&mut c, 140, 40);
}

#[test]
fn qa_calendar_unicode_and_long_text() {
    let d = scratch("cal-unicode");
    let path = d.join("calendar.json");
    let t = today();
    let plans: Vec<serde_json::Value> = nasty().into_iter().enumerate().map(|(i, s)| serde_json::json!({"day": t + (i as i64 % 3), "at": 60 * i as u32, "text": s})).collect();
    std::fs::write(&path, serde_json::to_string(&plans).unwrap()).unwrap();
    let mut k = Kit::new();
    let mut c = Calendar::open_at(path.clone());
    // the buffer clips, so there's no width to check here (and a wide char's second cell dumps as a space)
    for (w, h) in [(160, 50), (140, 40), (100, 30), (60, 20), (30, 10)] {
        let s = k.render(&mut c, w, h);
        assert_eq!(s.lines().count(), h as usize);
    }
    let side = k.render_side(&mut c, 34, 14);
    assert!(side.contains('…'), "the 5000-char plan is cut in the sidebar:\n{side}");
    // add, edit and delete a plan typed in emoji and RTL, then reload
    k.key(&mut c, KeyCode::Char('a'));
    k.typ(&mut c, "2pm 🦷 طبيب الأسنان");
    k.key(&mut c, KeyCode::Enter);
    let back: Vec<calendar::Plan> = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(back.iter().any(|p| p.text == "🦷 طبيب الأسنان" && p.at == Some(14 * 60)), "emoji/RTL plan saved intact");
    assert_eq!(back.len(), plans.len() + 1);
}

#[test]
fn qa_calendar_control_chars_in_saved_text_stay_off_screen() {
    let d = scratch("cal-controls");
    let path = d.join("calendar.json");
    let t = today();
    let plans: Vec<serde_json::Value> = with_controls().into_iter().map(|s| serde_json::json!({"day": t, "text": s})).collect();
    std::fs::write(&path, serde_json::to_string(&plans).unwrap()).unwrap();
    let mut k = Kit::new();
    let mut c = Calendar::open_at(path);
    let s = k.render(&mut c, 140, 40);
    let side = k.render_side(&mut c, 34, 12);
    let bad: Vec<char> = controls(&s).into_iter().chain(controls(&side)).collect();
    assert!(bad.is_empty(), "control characters from calendar.json reached the screen: {bad:?}\n{s}");
}

#[test]
#[ignore = "fails: a read-only calendar.json drops new plans on restart with no message (save errors are ignored)"]
fn qa_calendar_readonly_file_says_it_couldnt_save() {
    let d = scratch("cal-ro");
    let path = d.join("calendar.json");
    std::fs::write(&path, "[]").unwrap();
    let _ro = ReadOnly::set(&path);
    let mut k = Kit::new();
    let mut c = Calendar::open_at(path.clone());
    k.key(&mut c, KeyCode::Char('a'));
    k.typ(&mut c, "2pm dentist");
    k.key(&mut c, KeyCode::Enter);
    let s = k.render(&mut c, 140, 40);
    let persisted = Calendar::open_at(path.clone());
    let mut k2 = Kit::new();
    let mut persisted = persisted;
    let after = k2.render(&mut persisted, 140, 40);
    let saved = after.contains("dentist");
    let told = !k.notices().is_empty() || s.contains("couldn't") || s.contains("can't");
    assert!(saved || told, "the plan is not on disk ({saved}) and nothing said so ({told}):\n{s}");
}

#[test]
fn qa_calendar_missing_folder_and_folder_in_the_way() {
    // a folder that doesn't exist yet is made on the first save
    let d = scratch("cal-missing");
    let path = d.join("a").join("b").join("c").join("calendar.json");
    let mut k = Kit::new();
    let mut c = Calendar::open_at(path.clone());
    k.render(&mut c, 140, 40);
    k.key(&mut c, KeyCode::Char('a'));
    k.typ(&mut c, "lunch");
    k.key(&mut c, KeyCode::Enter);
    assert!(std::fs::read_to_string(&path).unwrap().contains("lunch"));
    // a folder where the file should be: loads empty, saving fails quietly, nothing panics
    let dir_path = d.join("calendar-dir.json");
    std::fs::create_dir_all(&dir_path).unwrap();
    let mut c = Calendar::open_at(dir_path.clone());
    k.key(&mut c, KeyCode::Char('a'));
    k.typ(&mut c, "lunch");
    k.key(&mut c, KeyCode::Enter);
    k.render(&mut c, 140, 40);
    assert!(dir_path.is_dir());
}

// ------------------------------------------------------------------ alerts.json (child process: cfg(test) path)

fn alerts_file() -> PathBuf {
    std::path::absolute("target/test-scratch/alerts.json").unwrap()
}

#[test]
fn qa_alerts_ago_normal_and_future() {
    let now = alerts::now();
    assert_eq!(alerts::ago(now), "just now");
    assert_eq!(alerts::ago(now + 10_000), "just now", "a clock that went backwards");
    assert_eq!(alerts::ago(i64::MAX), "just now");
    assert_eq!(alerts::ago(now - 7200), "2h");
    assert!(alerts::ago(0).ends_with('d'));
    assert!(alerts::ago(-1_000_000_000).ends_with('d'));
}

#[test]
#[ignore = "fails: an alert with at = i64::MIN panics in alerts::ago ('attempt to subtract with overflow', debug builds)"]
fn qa_alerts_ago_min_timestamp() {
    let _ = alerts::ago(i64::MIN);
}

const CHILD_ALERTS_CORRUPT: &str = "qa_data::child_alerts_corrupt";
#[test]
fn child_alerts_corrupt() {
    if !is_child(CHILD_ALERTS_CORRUPT) {
        return;
    }
    for (i, bytes) in corrupt_json().into_iter().enumerate() {
        std::fs::write(alerts_file(), &bytes).unwrap();
        alerts::open_at(alerts_file());
        let got = alerts::with(|a| a.to_vec());
        assert!(got.is_empty(), "case {i}: corrupt alerts.json loads as nothing, got {}", got.len());
    }
    // wrong types and missing fields
    for s in [r#"[{"at":"yesterday","kind":"update","text":"x"}]"#, r#"[{"kind":"update","text":"x"}]"#, r#"[{"at":1,"kind":"update"}]"#, r#"[{"at":1,"kind":7,"text":"x"}]"#] {
        std::fs::write(alerts_file(), s).unwrap();
        alerts::open_at(alerts_file());
    }
    // missing folder: save fails quietly
    let _ = std::fs::remove_dir_all(std::path::absolute("target/test-scratch").unwrap());
    alerts::clear();
    alerts::open_at(alerts_file());
    assert!(alerts::with(|a| a.is_empty()));
}

#[test]
fn qa_alerts_corrupt_files() {
    child_ok(CHILD_ALERTS_CORRUPT, "alerts-corrupt");
}

const CHILD_ALERTS_KIND: &str = "qa_data::child_alerts_unknown_kind";
#[test]
fn child_alerts_unknown_kind() {
    if !is_child(CHILD_ALERTS_KIND) {
        return;
    }
    // two alerts from this version and one kind a newer version added (`oriel rollback` brings you back here)
    let now = alerts::now();
    let json = format!(
        r#"[{{"at":{now},"kind":"agent_done","text":"first alert","app":"agents","read":true}},
            {{"at":{now},"kind":"update","text":"second alert"}},
            {{"at":{now},"kind":"deploy_finished","text":"from a newer oriel"}}]"#
    );
    std::fs::write(alerts_file(), json).unwrap();
    alerts::open_at(alerts_file());
    let n = alerts::with(|a| a.len());
    alerts::push(Alert { at: now, kind: Kind::Calendar, text: "a new one".into(), app: None, read: false, pane: None });
    let disk = std::fs::read_to_string(alerts_file()).unwrap();
    assert!(n >= 2 && disk.contains("first alert"), "loaded {n} of 3 alerts; after one new alert the file holds:\n{disk}");
}

#[test]
#[ignore = "fails: one alert of an unknown kind makes alerts.json load empty, and the next alert overwrites the whole history"]
fn qa_alerts_unknown_kind_wipes_history() {
    child_ok(CHILD_ALERTS_KIND, "alerts-kind");
}

const CHILD_ALERTS_PANE: &str = "qa_data::child_alerts_pane_weird_rows";
#[test]
fn child_alerts_pane_weird_rows() {
    if !is_child(CHILD_ALERTS_PANE) {
        return;
    }
    let now = alerts::now();
    let mut rows = vec![];
    for (i, s) in nasty().into_iter().chain(with_controls()).enumerate() {
        let app = ["agents", "", "../../etc", "updates", "🎉", "nonexistent-app"][i % 6];
        let at = [now, 0, i64::MAX, now - 90_000, -5][i % 5];
        let kind = ["agent_done", "needs_you", "approval", "build_failed", "calendar", "download", "memory", "usage", "update"][i % 9];
        rows.push(serde_json::json!({"at": at, "kind": kind, "text": s, "app": app}));
    }
    std::fs::write(alerts_file(), serde_json::to_string(&rows).unwrap()).unwrap();
    alerts::open_at(alerts_file());
    assert_eq!(alerts::with(|a| a.len()), rows.len());
    let mut k = Kit::new();
    let mut p = crate::panes::alerts::Alerts::new();
    for (w, h) in [(160, 50), (120, 30), (40, 10), (10, 3)] {
        let s = k.render(&mut p, w, h);
        assert_eq!(s.lines().count(), h as usize);
    }
    let s = k.render(&mut p, 160, 50);
    let bad = controls(&s);
    assert!(bad.is_empty(), "control characters from alerts.json reached the screen: {bad:?}");
    for _ in 0..rows.len() + 2 {
        k.key(&mut p, KeyCode::Enter);
        k.key(&mut p, KeyCode::Char('j'));
    }
    k.key(&mut p, KeyCode::Char('x'));
    k.key(&mut p, KeyCode::Char('c'));
    k.render(&mut p, 120, 30);
    assert_eq!(alerts::unread(), 0);
}

#[test]
fn qa_alerts_pane_weird_rows() {
    child_ok(CHILD_ALERTS_PANE, "alerts-pane");
}

// ------------------------------------------------------------------ custom themes

/// Points theme files at a scratch folder for this test thread, and back when dropped.
struct ThemeDir(PathBuf);

impl ThemeDir {
    fn new(name: &str) -> ThemeDir {
        let d = scratch(name);
        theme::TEST_DIR.with(|t| *t.borrow_mut() = Some(d.clone()));
        ThemeDir(d)
    }
}

impl Drop for ThemeDir {
    fn drop(&mut self) {
        theme::TEST_DIR.with(|t| *t.borrow_mut() = None);
    }
}

#[test]
fn qa_theme_corrupt_files_never_panic() {
    let td = ThemeDir::new("themes-corrupt");
    let d = &td.0;
    let long = "l".repeat(200);
    let files: Vec<(String, Vec<u8>)> = vec![
        ("qa-empty".into(), b"".to_vec()),
        ("qa-trunc".into(), b"accent = \"#ff".to_vec()),
        ("qa-types".into(), b"accent = 5\nbase = 7\nrainbow = \"yes\"\ngood = [1,2]\n[muted]\nx = 1\n".to_vec()),
        ("qa-binary".into(), vec![0xff, 0xfe, b'a', 0, b'=', 0]),
        ("qa-huge".into(), format!("accent = \"#{}\"\nshine = \"{}\"\n", "f".repeat(100_000), "🎨".repeat(10_000)).into_bytes()),
        ("qa-badcolors".into(), "accent = \"#12345\"\nshine = \"#ggg\"\ngood = \"#ÿÿÿ\"\ndanger = \"\"\nmuted = \"bright-\"\nframe = \"#\"\nuser = \"0x\"\n".as_bytes().to_vec()),
        ("qa-cycle-a".into(), b"base = \"qa-cycle-b\"\naccent = \"#ff0000\"\n".to_vec()),
        ("qa-cycle-b".into(), b"base = \"qa-cycle-a\"\n".to_vec()),
        ("qa-self".into(), b"base = \"qa-self\"\n".to_vec()),
        ("qa-missing-base".into(), b"base = \"no-such-theme\"\n".to_vec()),
        ("🎨emoji".into(), b"accent = \"red\"\n".to_vec()),
        ("ثيم".into(), b"base = \"ocean\"\n".to_vec()),
        (format!("qa-{long}"), b"accent = \"#00ff00\"\n".to_vec()),
        ("qa-bom".into(), "\u{feff}accent = \"#00ff00\"\n".as_bytes().to_vec()),
    ];
    for (n, bytes) in &files {
        std::fs::write(d.join(format!("{n}.toml")), bytes).unwrap();
    }
    std::fs::create_dir_all(d.join("qa-a-folder.toml")).unwrap();
    let names = theme::custom_names();
    assert!(names.iter().any(|n| n == "qa-a-folder"), "a folder called *.toml is listed: {names:?}");
    let mut k = Kit::new();
    for n in names.iter().chain(["qa-not-there".to_string()].iter()) {
        let t = theme::get(n);
        let _ = theme::problems(n);
        let _ = theme::base_of(n);
        let _ = theme::is_custom(n);
        // the themes app with it applied
        k.theme = t;
        let mut p = crate::panes::themes::Themes::new();
        k.render(&mut p, 140, 40);
        k.render_side(&mut p, 34, 20);
    }
    assert_eq!(theme::get("qa-empty").accent, theme::get("ultra").accent, "an empty file is plain ultra");
    assert_eq!(theme::get("qa-cycle-a").accent, ratatui::style::Color::Rgb(255, 0, 0), "a base cycle stops and keeps its own colours");
    assert!(theme::problems("qa-types").len() >= 3, "{:?}", theme::problems("qa-types"));
    assert!(!theme::problems("qa-trunc").is_empty());
    assert!(theme::problems("qa-missing-base").iter().any(|p| p.contains("no-such-theme")));
}

#[test]
#[ignore = "fails: a theme file that isn't UTF-8 (e.g. saved as UTF-16) silently turns into the built-in 'oriel' theme and loses its name"]
fn qa_theme_unreadable_file_keeps_its_name() {
    let td = ThemeDir::new("themes-unreadable");
    // UTF-16LE with a BOM: what older Notepad writes for "Unicode"
    let text = "accent = \"#ff0000\"\n";
    let mut bytes = vec![0xff, 0xfe];
    for u in text.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    std::fs::write(td.0.join("qa-utf16.toml"), bytes).unwrap();
    let t = theme::get("qa-utf16");
    let probs = theme::problems("qa-utf16");
    assert!(t.name == "qa-utf16" && !probs.is_empty(), "got theme '{}' (expected the file's own name, like a TOML typo keeps), problems reported: {probs:?}", t.name);
}

#[test]
fn qa_theme_save_errors_are_reported() {
    let td = ThemeDir::new("themes-save");
    let t = theme::get("ultra");
    // read-only file: an error, not a panic or a silent success
    let p = theme::save_custom("qa-ro", "ultra", &t).unwrap();
    let _ro = ReadOnly::set(&p);
    assert!(theme::save_custom("qa-ro", "ultra", &t).is_err(), "writing over a read-only theme file reports an error");
    // a name too long for the file system
    let long = theme::free_name(&"n".repeat(400));
    assert!(theme::save_custom(&long, "ultra", &t).is_err(), "a 400-char name can't be a file");
    // weird names come out as file-safe names
    for want in ["🎨🎨", "../../etc/passwd", "a/b\\c", "  ", "", "---", "My Theme!!", "ثيم جديد", "con", "NUL"] {
        let n = theme::free_name(want);
        assert!(!n.is_empty() && !n.contains(['/', '\\', '.', ' ', ':']), "free_name({want:?}) = {n:?}");
    }
    drop(td);
}

#[test]
#[cfg(windows)]
fn qa_theme_reserved_windows_name() {
    let td = ThemeDir::new("themes-nul");
    let n = theme::free_name("nul");
    let saved = theme::save_custom(&n, "ultra", &theme::get("ultra"));
    let listed = theme::custom_names().contains(&n);
    let on_disk = std::fs::read_dir(&td.0).unwrap().flatten().any(|e| e.file_name().to_string_lossy().to_lowercase().starts_with("nul"));
    assert!(saved.is_err() || (listed && on_disk), "free_name gave {n:?}, save said {saved:?}, listed: {listed}, file on disk: {on_disk}");
}

// ------------------------------------------------------------------ update state (child process: cfg(test) path)

#[test]
fn qa_update_version_parsing_edge_cases() {
    assert_eq!(update::parse(""), (0, 0, 0));
    assert_eq!(update::parse("v"), (0, 0, 0));
    assert_eq!(update::parse("1.2.3.4"), (1, 2, 3));
    assert_eq!(update::parse("99999999999999999999.1.1"), (0, 1, 1), "too big for u64 falls back to 0");
    assert_eq!(update::parse("١.٢.٣"), (0, 0, 0), "non-ASCII digits");
    assert_eq!(update::parse("🎉.🎉.🎉"), (0, 0, 0));
    assert_eq!(update::parse(" v1..2 "), (1, 0, 2));
    assert!(!update::newer("garbage", "0.0.0"));
    assert!(update::available(&[]).is_none());
    let rs = vec![
        update::Release { version: "🎉".into(), notes: "x".repeat(100_000), ..Default::default() },
        update::Release { version: "999.0.0".into(), notes: "موعد\u{1b}[31m 🎉".into(), ..Default::default() },
        update::Release { version: "".into(), ..Default::default() },
    ];
    let n = update::notes_since(&rs, "0.0.0");
    assert!(n.contains("## 999.0.0"));
    assert_eq!(update::available(&rs).map(|r| r.version), Some("999.0.0".into()));
}

const CHILD_UPDATE_STATE: &str = "qa_data::child_update_state_files";
#[test]
fn child_update_state_files() {
    if !is_child(CHILD_UPDATE_STATE) {
        return;
    }
    let dir = std::path::absolute("target/test-scratch/update").unwrap();
    // missing folder
    assert!(update::just_updated().is_none());
    assert!(update::backups().is_empty());
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("updated.json");
    for (i, bytes) in corrupt_json().into_iter().enumerate() {
        std::fs::write(&file, &bytes).unwrap();
        assert!(update::just_updated().is_none(), "case {i}");
    }
    let v = update::VERSION;
    for s in [r#"{"from":1,"to":2}"#.to_string(), format!(r#"{{"from":null,"to":"{v}"}}"#), format!(r#"{{"to":"{v}"}}"#), r#"{"from":"0.1.0","to":"9.9.9"}"#.into(), format!(r#"{{"from":"0.1.0","to":"{v}","seen":true}}"#)] {
        std::fs::write(&file, &s).unwrap();
        assert!(update::just_updated().is_none(), "{s}");
    }
    // the real thing: shown once
    std::fs::write(&file, format!(r#"{{"from":"0.1.0 🎉","to":"{v}","at":"not a number","extra":[1,2]}}"#)).unwrap();
    assert_eq!(update::just_updated(), Some(("0.1.0 🎉".into(), v.into(), false)));
    assert!(update::just_updated().is_none(), "only once");
    // odd files in the rollback folder
    let prev = dir.join("previous");
    std::fs::create_dir_all(prev.join("oriel-9.9.9")).unwrap(); // a folder
    for n in ["oriel-0.1.0.exe", "oriel-.exe", "oriel-garbage", "oriel-🎉.exe", "notoriel.txt", "oriel-99999999999999999999.0.0"] {
        std::fs::write(prev.join(n), b"not a program").unwrap();
    }
    let b = update::backups();
    assert!(b.iter().any(|(v, _)| v == "0.1.0"));
    assert!(!b.iter().any(|(_, p)| p.ends_with("notoriel.txt")));
}

#[test]
fn qa_update_state_files() {
    child_ok(CHILD_UPDATE_STATE, "update-state");
}

const CHILD_UPDATE_RO: &str = "qa_data::child_update_readonly_marker";
#[test]
fn child_update_readonly_marker() {
    if !is_child(CHILD_UPDATE_RO) {
        return;
    }
    let dir = std::path::absolute("target/test-scratch/update").unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("updated.json");
    std::fs::write(&file, format!(r#"{{"from":"0.1.0","to":"{}"}}"#, update::VERSION)).unwrap();
    let _ro = ReadOnly::set(&file);
    let first = update::just_updated();
    let second = update::just_updated();
    assert!(first.is_some() && second.is_none(), "a read-only updated.json: first start {first:?}, second start {second:?} (the 'what's new' shows on every start)");
}

#[test]
#[ignore = "fails: when updated.json can't be written, 'just updated' is reported on every start (the seen mark is never saved)"]
fn qa_update_readonly_marker_shows_whats_new_forever() {
    child_ok(CHILD_UPDATE_RO, "update-ro");
}

// ------------------------------------------------------------------ notes folder

use crate::panes::notes::Notes;

fn ctrl(k: &mut Kit, p: &mut dyn Pane, c: char) {
    k.key_mod(p, KeyCode::Char(c), KeyModifiers::CONTROL);
}

fn set_mtime(p: &Path, secs: u64) {
    let f = std::fs::File::options().write(true).open(p).unwrap();
    f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)).unwrap();
}

/// Click every row of the notes sidebar (opens each note).
fn click_every_note(k: &mut Kit, p: &mut Notes, rows: u16) {
    for y in 2..rows {
        let ev = MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 3, row: y, modifiers: KeyModifiers::NONE };
        let mut cx_actions = vec![];
        let mut cx = crate::pane::Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: &mut cx_actions, focused: true, time: 1.0 };
        p.side_mouse(ev, ratatui::layout::Rect::new(0, 0, 34, rows), &mut cx);
        k.render(p, 150, 44);
        k.render(p, 40, 12);
    }
}

#[test]
fn qa_notes_odd_files_open_and_render() {
    let d = scratch("notes-odd");
    let long = format!("# {}", "long title ".repeat(300));
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("empty.md", b"".to_vec()),
        ("newline.md", b"\n".to_vec()),
        ("hashes.md", b"#######\n".to_vec()),
        ("emoji.md", "# 🎉 party 👨‍👩‍👧‍👦 🏳️‍🌈\nbody".as_bytes().to_vec()),
        ("rtl.md", "# ملاحظة مهمة\nנקודה".as_bytes().to_vec()),
        ("cjk.md", "# 東京駅で待ち合わせ\n".as_bytes().to_vec()),
        ("long.md", long.clone().into_bytes()),
        ("crlf.md", b"# crlf\r\nline two\r\n".to_vec()),
        ("controls.md", "# esc \u{1b}[31mred\u{7}\nnul\u{0}".as_bytes().to_vec()),
        ("🎉.md", b"# emoji file name\n".to_vec()),
        ("UPPER.MD", b"# upper case extension\n".to_vec()),
        ("leftover.md.tmp", b"# temp file, not a note\n".to_vec()),
        ("noext", b"# not a note\n".to_vec()),
        ("bad.md", vec![b'#', b' ', 0xff, 0xfe, b'\n', 0x80]),
    ];
    for (i, (n, bytes)) in files.iter().enumerate() {
        std::fs::write(d.join(n), bytes).unwrap();
        set_mtime(&d.join(n), 1_600_000_000 + i as u64 * 60);
    }
    std::fs::create_dir_all(d.join("folder.md")).unwrap();
    let mut k = Kit::new();
    let mut p = Notes::open_in(d.clone(), None);
    let side = k.render_side(&mut p, 34, 30);
    assert!(!side.contains("temp file") && !side.contains("not a note"), "only .md files are notes:\n{side}");
    assert!(side.contains("upper case extension"), ".MD counts:\n{side}");
    click_every_note(&mut k, &mut p, 30);
    assert!(p.title().chars().count() <= 61, "window title is cut: {}", p.title());
    // preview every note too
    ctrl(&mut k, &mut p, 'e');
    click_every_note(&mut k, &mut p, 30);
    ctrl(&mut k, &mut p, 'e');
    // nothing was edited, so nothing was rewritten
    for (n, bytes) in &files {
        assert_eq!(&std::fs::read(d.join(n)).unwrap(), bytes, "{n} was rewritten just by opening it");
    }
}

#[test]
fn qa_notes_control_chars_stay_off_screen() {
    let d = scratch("notes-controls");
    std::fs::write(d.join("c.md"), "# esc \u{1b}[31mred\u{1b}[0m bell\u{7}\nnul\u{0} and \u{1b}]0;title\u{7}").unwrap();
    let mut k = Kit::new();
    let mut p = Notes::open_in(d, None);
    let s = k.render(&mut p, 120, 30);
    let side = k.render_side(&mut p, 34, 10);
    ctrl(&mut k, &mut p, 'e');
    let pv = k.render(&mut p, 120, 30);
    let bad: Vec<char> = controls(&s).into_iter().chain(controls(&side)).chain(controls(&pv)).collect();
    assert!(bad.is_empty(), "control characters from a note reached the screen: {bad:?}\n{s}");
}

#[test]
#[ignore = "fails: a note saved with a UTF-8 BOM (Notepad) gets the title \"\\u{feff}# Title\" instead of \"Title\""]
fn qa_notes_bom_title() {
    let d = scratch("notes-bom");
    std::fs::write(d.join("bom.md"), "\u{feff}# Bom title\nbody\n").unwrap();
    let p = Notes::open_in(d, None);
    assert_eq!(p.title(), "Bom title");
}

#[test]
#[ignore = "fails: editing one line of a note rewrites every tab in the file as 4 spaces and every lone \\r as a line break"]
fn qa_notes_edit_keeps_the_rest_of_the_file() {
    let d = scratch("notes-tabs");
    let path = d.join("table.md");
    std::fs::write(&path, "# table\nname\tqty\napples\t3\nold mac line\rend").unwrap();
    let mut k = Kit::new();
    let mut p = Notes::open_in(d, None);
    k.typ(&mut p, "!"); // the cursor starts at the end
    ctrl(&mut k, &mut p, 's');
    let disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(disk, "# table\nname\tqty\napples\t3\nold mac line\rend!", "typing one character changed other lines");
}

#[test]
#[ignore = "fails: when the newest note can't be read (not UTF-8), the editor still takes typing and silently throws it away"]
fn qa_notes_unreadable_newest_note_loses_typing() {
    let d = scratch("notes-unreadable");
    std::fs::write(d.join("good.md"), "# good\n").unwrap();
    set_mtime(&d.join("good.md"), 1_600_000_000);
    std::fs::write(d.join("bad.md"), [b'#', b' ', 0xff, 0xfe, b'\n']).unwrap();
    let mut k = Kit::new();
    let mut p = Notes::open_in(d.clone(), None);
    let s = k.render(&mut p, 120, 30);
    assert!(s.contains("can't open that note"), "the error is shown:\n{s}");
    k.typ(&mut p, "hello typed text");
    ctrl(&mut k, &mut p, 's');
    drop(p);
    let saved = std::fs::read_dir(&d).unwrap().flatten().any(|e| std::fs::read(e.path()).map(|b| String::from_utf8_lossy(&b).contains("hello typed text")).unwrap_or(false));
    assert!(saved, "what was typed is nowhere on disk");
}

#[test]
fn qa_notes_folder_is_a_file_or_missing() {
    // a file where the folder should be: errors on screen, no panic
    let d = scratch("notes-file");
    let f = d.join("notes");
    std::fs::write(&f, "i am a file").unwrap();
    let mut k = Kit::new();
    let mut p = Notes::open_in(f.clone(), None);
    let s = k.render(&mut p, 120, 30);
    assert!(s.contains("can't"), "the problem is shown:\n{s}");
    k.typ(&mut p, "typing anyway");
    ctrl(&mut k, &mut p, 's');
    ctrl(&mut k, &mut p, 'n');
    ctrl(&mut k, &mut p, 'd');
    k.key(&mut p, KeyCode::Char('y'));
    k.render(&mut p, 120, 30);
    k.render_side(&mut p, 34, 10);
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "i am a file");
    // a folder that doesn't exist yet is made, with the welcome note in it
    let deep = d.join("x").join("y").join("z");
    let p = Notes::open_in(deep.clone(), None);
    assert_eq!(p.title(), "welcome to notes");
    assert!(std::fs::read_dir(&deep).unwrap().count() == 1);
}

#[test]
fn qa_notes_readonly_note_shows_save_error() {
    let d = scratch("notes-ro");
    let path = d.join("ro.md");
    std::fs::write(&path, "# locked\n").unwrap();
    let _ro = ReadOnly::set(&path);
    let mut k = Kit::new();
    let mut p = Notes::open_in(d.clone(), None);
    k.typ(&mut p, " more");
    ctrl(&mut k, &mut p, 's');
    let s = k.render(&mut p, 120, 30);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "# locked\n");
    assert!(s.contains("couldn't save"), "a read-only note says it couldn't save:\n{s}");
    assert!(!d.join("ro.md.tmp").exists(), "no temp file left behind");
}

#[test]
fn qa_notes_huge_note_stays_usable() {
    let d = scratch("notes-huge");
    let one_line = "word ".repeat(400_000); // 2 MB, one line
    let many = (0..20_000).map(|i| format!("line {i} 🎉 موعد")).collect::<Vec<_>>().join("\n");
    std::fs::write(d.join("a-one-line.md"), &one_line).unwrap();
    set_mtime(&d.join("a-one-line.md"), 1_600_000_100);
    std::fs::write(d.join("b-many.md"), &many).unwrap();
    set_mtime(&d.join("b-many.md"), 1_600_000_000);
    let t0 = std::time::Instant::now();
    let mut k = Kit::new();
    let mut p = Notes::open_in(d.clone(), None);
    k.render(&mut p, 150, 44);
    let open_ms = t0.elapsed().as_millis();
    let t1 = std::time::Instant::now();
    for _ in 0..10 {
        k.key(&mut p, KeyCode::Char('x'));
        k.render(&mut p, 150, 44);
    }
    let per_key = t1.elapsed().as_millis() / 10;
    println!("qa_notes_huge: open+render {open_ms} ms, then {per_key} ms per keystroke (debug build, 2 MB one-line note)");
    k.key(&mut p, KeyCode::PageUp);
    k.key_mod(&mut p, KeyCode::Home, KeyModifiers::CONTROL);
    ctrl(&mut k, &mut p, 'e');
    k.render(&mut p, 150, 44);
    // generous: debug build on a busy machine (other builds run alongside); the number above is the finding
    assert!(per_key < 15_000, "a keystroke in a 2 MB note took {per_key} ms");
}
