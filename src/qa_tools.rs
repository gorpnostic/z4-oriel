//! QA pass over the tool apps through the testkit: calendar, notes, files, help, themes, alerts, updates, plus a
//! "render every tool at every size" sweep. Private-state tests for storage, system, music and updates live next
//! to those panes (`panes/*/qa_tools.rs`).
//!
//! Everything is headless and local: files go under target/test-scratch/qa-tools/, themes into a scratch themes
//! folder, no network, clipboard, audio, windows or real-disk scans. A test marked
//! `#[ignore = "fails: ..."]` documents a bug found here; run them with `cargo test qa_ -- --ignored`.

#![cfg(test)]

use crate::pane::{Action, Cx, Pane};
use crate::testkit::Kit;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Terminal,
    backend::{Backend, TestBackend},
    layout::Rect,
};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// A fresh scratch folder under target/test-scratch/qa-tools/ (never the user's files).
fn scratch(name: &str) -> PathBuf {
    let d = std::path::absolute(PathBuf::from("target/test-scratch/qa-tools").join(name)).unwrap();
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn click(x: u16, y: u16) -> MouseEvent {
    MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: KeyModifiers::NONE }
}

fn wheel(down: bool, x: u16, y: u16) -> MouseEvent {
    MouseEvent { kind: if down { MouseEventKind::ScrollDown } else { MouseEventKind::ScrollUp }, column: x, row: y, modifiers: KeyModifiers::NONE }
}

/// The screen as text. Unlike the testkit's, the blank cell ratatui leaves after a wide character is skipped,
/// so "日本" reads back as "日本" rather than "日 本 ".
fn dump(b: &TestBackend) -> String {
    let buf = b.buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        let mut line = String::new();
        let mut x = 0;
        while x < buf.area.width {
            let s = buf[(x, y)].symbol();
            line.push_str(s);
            x += unicode_width::UnicodeWidthStr::width(s).max(1) as u16;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// The first `n` characters of each line (the left column of a two-column pane), frame row skipped.
fn left(s: &str, n: usize) -> String {
    s.lines().skip(1).map(|l| l.chars().take(n).collect::<String>() + "\n").collect()
}

/// The first line of the files app's preview column (the highlighted entry's name) in a 150-wide framed render.
fn preview_head(s: &str) -> String {
    s.lines().nth(1).unwrap_or("").chars().skip(62).collect::<String>().trim().to_string()
}

/// Tests that touch the global event center take this, so they don't mark each other's alerts read.
static CENTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Render the pane into `area` of a w x h screen (area may be empty). Returns the screen and the cursor position.
fn render_at(k: &mut Kit, p: &mut dyn Pane, w: u16, h: u16, area: Rect) -> (String, (u16, u16)) {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    let mut actions = std::mem::take(&mut k.actions);
    let (theme, config, tx, time) = (&k.theme, &k.config, &k.tx, k.time);
    term.draw(|f| {
        let mut cx = Cx { id: 1, theme, config, tx, actions: &mut actions, focused: true, time };
        p.render(f, area, &mut cx);
    })
    .unwrap();
    k.actions = actions;
    let pos = term.backend_mut().get_cursor_position().map(|p| (p.x, p.y)).unwrap_or((0, 0));
    (dump(term.backend()), pos)
}

/// Render like the app does: the pane's frame (title + subtitle) around its inside.
fn framed(k: &mut Kit, p: &mut dyn Pane, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    let mut actions = std::mem::take(&mut k.actions);
    let (theme, config, tx, time) = (&k.theme, &k.config, &k.tx, k.time);
    term.draw(|f| {
        let inner = crate::ui::frame(f, Rect::new(0, 0, w, h), &format!("{}{}", crate::ui::lead(p.icon()), p.title()), p.subtitle().as_deref(), true, theme);
        let mut cx = Cx { id: 1, theme, config, tx, actions: &mut actions, focused: true, time };
        p.render(f, inner, &mut cx);
    })
    .unwrap();
    k.actions = actions;
    dump(term.backend())
}

/// Render (framed) until `pred` holds, letting background work wake the pane in between.
fn settle(k: &mut Kit, p: &mut dyn Pane, w: u16, h: u16, ms: u64, pred: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_millis(ms);
    loop {
        let s = framed(k, p, w, h);
        if pred(&s) || Instant::now() > deadline {
            return s;
        }
        k.wait_wake(p, 40);
    }
}

fn paste(k: &mut Kit, p: &mut dyn Pane, text: &str) {
    let mut actions = std::mem::take(&mut k.actions);
    {
        let mut cx = Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: &mut actions, focused: true, time: k.time };
        p.paste(text, &mut cx);
    }
    k.actions = actions;
}

fn ctrl(k: &mut Kit, p: &mut dyn Pane, c: char) -> bool {
    k.key_mod(p, KeyCode::Char(c), KeyModifiers::CONTROL)
}

// ====================================================================================================== calendar
mod calendar {
    use super::*;
    use crate::panes::calendar::{Calendar, Plan, civil_from_days, days_from_civil, parse_time, weekday};
    use crate::panes::files::clock;

    const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
    const DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];

    /// (today, minutes since midnight), local time, the way the calendar computes it.
    fn now_day_min() -> (i64, u32) {
        let l = clock::local(clock::now_secs());
        (days_from_civil(l.year as i64, l.month, l.day), l.hour * 60 + l.min)
    }

    fn dim(y: i64, m: u32) -> u32 {
        let next = if m == 12 { days_from_civil(y + 1, 1, 1) } else { days_from_civil(y, m + 1, 1) };
        (next - days_from_civil(y, m, 1)) as u32
    }

    fn cal(name: &str) -> (Calendar, PathBuf) {
        let path = scratch(name).join("calendar.json");
        (Calendar::open_at(path.clone()), path)
    }

    fn load(path: &std::path::Path) -> Vec<Plan> {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap_or_default()).unwrap_or_default()
    }

    /// Walk the calendar to y-m-d with keys only: `t`, then months with [ ], then days with ← →.
    fn goto(k: &mut Kit, c: &mut Calendar, y: i64, m: u32, d: u32) {
        k.key(c, KeyCode::Char('t'));
        let (today, _) = now_day_min();
        let (ty, tm, td) = civil_from_days(today);
        let months = (y * 12 + m as i64) - (ty * 12 + tm as i64);
        let key = if months >= 0 { ']' } else { '[' };
        for _ in 0..months.abs() {
            k.key(c, KeyCode::Char(key));
        }
        let at = days_from_civil(y, m, td.min(dim(y, m)));
        let diff = days_from_civil(y, m, d) - at;
        let key = if diff >= 0 { KeyCode::Right } else { KeyCode::Left };
        for _ in 0..diff.abs() {
            k.key(c, key);
        }
    }

    /// The chosen day's heading as the calendar writes it: "Tuesday 29 February".
    fn heading(y: i64, m: u32, d: u32) -> String {
        format!("{} {d} {}", DAYS[weekday(days_from_civil(y, m, d)) as usize], MONTHS[m as usize - 1])
    }

    fn add(k: &mut Kit, c: &mut Calendar, text: &str) {
        k.key(c, KeyCode::Char('a'));
        k.typ(c, text);
        k.key(c, KeyCode::Enter);
    }

    #[test]
    fn qa_calendar_date_math_roundtrip_and_leap_years() {
        // every day from 1600 to 2400: the civil formulas round-trip, days advance one at a time, weekdays cycle
        let (a, b) = (days_from_civil(1600, 1, 1), days_from_civil(2400, 12, 31));
        let mut prev = civil_from_days(a - 1);
        for z in a..=b {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z, "round trip at {y}-{m}-{d}");
            assert!((1..=12).contains(&m) && d >= 1 && d <= dim(y, m), "{y}-{m}-{d}");
            let next_ok = (y, m, d) == (prev.0, prev.1, prev.2 + 1) || (d == 1 && (m == prev.1 % 12 + 1));
            assert!(next_ok, "{prev:?} -> {y}-{m}-{d}");
            assert_eq!(weekday(z), (weekday(z - 1) + 1) % 7);
            prev = (y, m, d);
        }
        // leap years: every 4, not every 100, yes every 400
        for (y, leap) in [(1900, false), (2000, true), (2024, true), (2026, false), (2028, true), (2100, false), (2200, false), (2400, true)] {
            assert_eq!(dim(y, 2), if leap { 29 } else { 28 }, "{y}");
            assert_eq!(civil_from_days(days_from_civil(y, 2, 29)), if leap { (y, 2, 29) } else { (y, 3, 1) }, "{y}");
        }
        // known weekdays (0 = Monday)
        assert_eq!(weekday(days_from_civil(1970, 1, 1)), 3, "a Thursday");
        assert_eq!(weekday(days_from_civil(1969, 12, 31)), 2, "before the epoch: a Wednesday");
        assert_eq!(weekday(days_from_civil(2000, 1, 1)), 5, "a Saturday");
        assert_eq!(weekday(days_from_civil(2024, 2, 29)), 3, "a Thursday");
        assert_eq!(weekday(days_from_civil(2100, 3, 1)), 0, "a Monday");
    }

    #[test]
    fn qa_calendar_month_keys_clamp_to_the_month_end() {
        let mut k = Kit::new();
        let (mut c, _) = cal("cal-month-end");
        // Jan 31 of a leap year -> ] -> Feb 29
        goto(&mut k, &mut c, 2028, 1, 31);
        let s = k.render(&mut c, 140, 40);
        assert!(s.contains(&heading(2028, 1, 31)), "{s}");
        k.key(&mut c, KeyCode::Char(']'));
        assert_eq!(c.title(), "calendar · February 2028");
        let s = k.render(&mut c, 140, 40);
        assert!(s.contains("Tuesday 29 February"), "leap-year February ends on the 29th:\n{s}");
        // not a leap year
        goto(&mut k, &mut c, 2027, 1, 31);
        k.key(&mut c, KeyCode::Char(']'));
        assert!(k.render(&mut c, 140, 40).contains("Sunday 28 February"));
        // the century rule, backwards with [
        goto(&mut k, &mut c, 2100, 3, 31);
        k.key(&mut c, KeyCode::Char('['));
        assert_eq!(c.title(), "calendar · February 2100");
        assert!(k.render(&mut c, 140, 40).contains("Sunday 28 February"));
        goto(&mut k, &mut c, 2000, 3, 31);
        k.key(&mut c, KeyCode::PageUp);
        assert!(k.render(&mut c, 140, 40).contains("Tuesday 29 February"), "2000 was a leap year");
        // 31 -> 30-day month
        goto(&mut k, &mut c, 2026, 3, 31);
        k.key(&mut c, KeyCode::PageDown);
        assert!(k.render(&mut c, 140, 40).contains(&heading(2026, 4, 30)));
        // the year rolls over both ways
        goto(&mut k, &mut c, 2026, 12, 31);
        k.key(&mut c, KeyCode::Char(']'));
        assert_eq!(c.title(), "calendar · January 2027");
        assert!(k.render(&mut c, 140, 40).contains("Sunday 31 January"));
        k.key(&mut c, KeyCode::Char('['));
        assert_eq!(c.title(), "calendar · December 2026");
        // day keys over the end of February
        goto(&mut k, &mut c, 2100, 2, 28);
        k.key(&mut c, KeyCode::Right);
        assert!(k.render(&mut c, 140, 40).contains("Monday 1 March"));
        goto(&mut k, &mut c, 2028, 2, 28);
        k.key(&mut c, KeyCode::Char('l'));
        assert!(k.render(&mut c, 140, 40).contains("Tuesday 29 February"));
        k.key(&mut c, KeyCode::Down); // a week later
        assert!(k.render(&mut c, 140, 40).contains(&heading(2028, 3, 7)));
        k.key(&mut c, KeyCode::Up);
        k.key(&mut c, KeyCode::Char('h'));
        assert!(k.render(&mut c, 140, 40).contains("Monday 28 February"));
        // t / Home come back to today
        k.key(&mut c, KeyCode::Home);
        let (today, _) = now_day_min();
        let (y, m, d) = civil_from_days(today);
        let s = k.render(&mut c, 140, 40);
        assert!(s.contains(&heading(y, m, d)) && s.contains("today"), "{s}");
    }

    #[test]
    fn qa_calendar_times_at_the_edges_of_the_day() {
        assert_eq!(parse_time("23:59 last train"), (Some(1439), "last train".into()));
        assert_eq!(parse_time("11:59pm night owl"), (Some(1439), "night owl".into()));
        assert_eq!(parse_time("12:00am x"), (Some(0), "x".into()));
        assert_eq!(parse_time("0:00 x"), (Some(0), "x".into()));
        assert_eq!(parse_time("12pm lunch"), (Some(720), "lunch".into()));
        assert_eq!(parse_time("2PM Dentist"), (Some(840), "Dentist".into()), "am/pm in capitals");
        assert_eq!(parse_time("12:60 x"), (None, "12:60 x".into()));
        assert_eq!(parse_time("25:00 x"), (None, "25:00 x".into()));
        assert_eq!(parse_time("9am"), (None, "9am".into()), "a time with nothing after it is just text");
        assert_eq!(parse_time("  7:05   coffee  "), (Some(425), "coffee".into()));

        // adding them through the keys: saved, shown in time order
        let mut k = Kit::new();
        let (mut c, path) = cal("cal-2359");
        add(&mut k, &mut c, "23:59 last train");
        add(&mut k, &mut c, "11:59pm night owl");
        add(&mut k, &mut c, "12am midnight snack");
        add(&mut k, &mut c, "7:05 coffee");
        let plans = load(&path);
        assert_eq!(plans.len(), 4);
        assert_eq!(plans.iter().filter(|p| p.at == Some(1439)).count(), 2);
        let s = k.render(&mut c, 140, 40);
        let (i0, i7, i23) = (s.find("00:00").unwrap(), s.find("07:05").unwrap(), s.find("23:59").unwrap());
        assert!(i0 < i7 && i7 < i23, "time order:\n{s}");
        assert_eq!(c.badge().as_deref(), Some("4 today"));
        let side = k.render_side(&mut c, 40, 10);
        assert!(side.contains("today") && side.contains("23:59") && side.contains("last train"), "{side}");
    }

    /// "24:00 party" typed on a day means midnight at the END of that day; it is stored as 00:00 of the same day
    /// (its start), so it sorts first, its reminder fires ~24 hours early (or never), and editing shows "00:00".
    #[test]
    #[ignore = "fails: parse_time accepts hour 24 and wraps it to 00:00 of the same day (calendar.rs:95-102)"]
    fn qa_calendar_24_00_is_not_the_start_of_the_day() {
        let (at, text) = parse_time("24:00 party");
        assert_ne!(at, Some(0), "24:00 became 00:00 of the same day (text {text:?})");
    }

    /// A '.' separator is accepted for times ("10.30 standup"), but with a one-digit "minute" any decimal at the
    /// start of a plan turns into a time: "1.5 hours of gym" is saved as 01:05 "hours of gym".
    #[test]
    #[ignore = "fails: parse_time takes '1.5' as 01:05 (one-digit minutes after '.') (calendar.rs:88-89)"]
    fn qa_calendar_decimal_number_is_not_a_time() {
        assert_eq!(parse_time("1.5 hours of gym"), (None, "1.5 hours of gym".into()));
        assert_eq!(parse_time("2.5 km run"), (None, "2.5 km run".into()));
    }

    /// All-day plans sort as if they were at 00:00, then by text, so they land between two midnight plans.
    #[test]
    #[ignore = "fails: all-day plans sort as 00:00 and interleave with midnight plans by text (calendar.rs:157)"]
    fn qa_calendar_all_day_plans_are_not_mixed_into_midnight() {
        let mut k = Kit::new();
        let (mut c, _) = cal("cal-allday-order");
        add(&mut k, &mut c, "12am a midnight thing");
        add(&mut k, &mut c, "buy milk");
        add(&mut k, &mut c, "0:00 z midnight thing");
        let s = k.render(&mut c, 140, 40);
        let (a, milk, z) = (s.find("a midnight thing").unwrap(), s.find("buy milk").unwrap(), s.find("z midnight thing").unwrap());
        assert!(!(a < milk && milk < z), "the all-day plan sits between the two 00:00 plans:\n{s}");
    }

    /// Reminders against the real clock: ten minutes before (not eleven), and "now" for ten minutes (not eleven).
    #[test]
    fn qa_calendar_reminders_at_the_boundary() {
        for attempt in 0..3 {
            let (today, mins) = now_day_min();
            let mut want_soon = vec![];
            let mut want_now = vec![];
            let mut quiet = vec![];
            let mut plans = vec![];
            let mut plan = |day: i64, at: i64, text: &str| {
                if (0..1440).contains(&at) {
                    plans.push(Plan { day, at: Some(at as u32), text: text.into() });
                    true
                } else {
                    false
                }
            };
            let m = mins as i64;
            if plan(today, m + 10, "edge-ten-before") {
                want_soon.push("edge-ten-before");
            }
            if plan(today, m + 11, "edge-eleven-before") {
                quiet.push("edge-eleven-before");
            }
            if plan(today, m, "edge-starts-now") {
                want_now.push("edge-starts-now");
            }
            if plan(today, m - 9, "edge-nine-after") {
                want_now.push("edge-nine-after");
            }
            if plan(today, m - 10, "edge-ten-after") {
                quiet.push("edge-ten-after");
            }
            plan(today + 1, m, "edge-tomorrow");
            quiet.push("edge-tomorrow");
            let path = scratch("cal-remind").join("calendar.json");
            std::fs::write(&path, serde_json::to_string(&plans).unwrap()).unwrap();
            let mut k = Kit::new();
            let mut c = Calendar::open_at(path);
            k.poll(&mut c);
            if now_day_min() != (today, mins) && attempt < 2 {
                continue; // the minute turned over mid-test: try again
            }
            let got = k.notices();
            for w in &want_soon {
                let at = mins + 10;
                let want = format!("in 10 min: {:02}:{:02} {w}", at / 60, at % 60);
                assert!(got.contains(&want), "missing {want:?} in {got:?}");
            }
            for w in &want_now {
                assert!(got.iter().any(|g| g.starts_with("now: ") && g.ends_with(w)), "no 'now' reminder for {w}: {got:?}");
            }
            for q in &quiet {
                assert!(!got.iter().any(|g| g.contains(q)), "{q} should be quiet: {got:?}");
            }
            // said once: polling again adds nothing
            let n = got.len();
            k.poll(&mut c);
            assert_eq!(k.notices().len(), n, "reminders repeat: {:?}", k.notices());
            // and they go to the event center as calendar alerts
            assert!(k.actions.iter().all(|a| matches!(a, Action::Alert(crate::alerts::Kind::Calendar, _))));
            return;
        }
    }

    #[test]
    fn qa_calendar_edit_delete_undo() {
        let mut k = Kit::new();
        let (mut c, path) = cal("cal-edit");
        // nothing to pick yet: e / x / u / j / k do nothing and don't panic
        for code in [KeyCode::Char('e'), KeyCode::Char('x'), KeyCode::Delete, KeyCode::Char('u'), KeyCode::Char('j'), KeyCode::Char('k')] {
            k.key(&mut c, code);
        }
        assert!(k.render(&mut c, 140, 40).contains("nothing planned"));
        // esc cancels an add, empty enter adds nothing, ctrl/alt chars are ignored while typing
        k.key(&mut c, KeyCode::Char('a'));
        k.typ(&mut c, "never");
        k.key(&mut c, KeyCode::Esc);
        k.key(&mut c, KeyCode::Enter); // enter also opens the box
        k.typ(&mut c, "   ");
        k.key(&mut c, KeyCode::Enter);
        assert!(load(&path).is_empty());
        k.key(&mut c, KeyCode::Char('a'));
        k.typ(&mut c, "9am standuq");
        k.key(&mut c, KeyCode::Backspace);
        k.typ(&mut c, "p");
        k.key_mod(&mut c, KeyCode::Char('z'), KeyModifiers::CONTROL);
        k.key_mod(&mut c, KeyCode::Char('z'), KeyModifiers::ALT);
        k.key(&mut c, KeyCode::Enter);
        add(&mut k, &mut c, "5pm gym");
        add(&mut k, &mut c, "water plants");
        let plans = load(&path);
        assert_eq!(plans.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), ["water plants", "standup", "gym"], "sorted: all day, 09:00, 17:00");
        // pick the second (standup) and change its text but not its time
        k.key(&mut c, KeyCode::Char('j'));
        let s = k.render(&mut c, 140, 40);
        assert!(s.lines().any(|l| l.contains("▸") && l.contains("standup")), "{s}");
        k.key(&mut c, KeyCode::Char('e'));
        assert!(k.render(&mut c, 140, 40).contains("edit › 09:00 standup"));
        k.typ(&mut c, " meeting");
        k.key(&mut c, KeyCode::Enter);
        assert!(load(&path).iter().any(|p| p.text == "standup meeting" && p.at == Some(540)));
        // editing to an empty text keeps the plan
        k.key(&mut c, KeyCode::Char('e'));
        for _ in 0..40 {
            k.key(&mut c, KeyCode::Backspace);
        }
        k.key(&mut c, KeyCode::Enter);
        assert_eq!(load(&path).len(), 3);
        // delete + undo puts back exactly that plan
        k.key(&mut c, KeyCode::Char('x'));
        assert_eq!(load(&path).len(), 2);
        assert!(k.render(&mut c, 140, 40).contains("deleted · u puts it back"));
        k.key(&mut c, KeyCode::Char('u'));
        assert!(load(&path).iter().any(|p| p.text == "standup meeting" && p.at == Some(540)));
        // undo from another month jumps back to the plan's day
        k.key(&mut c, KeyCode::Char('x'));
        k.key(&mut c, KeyCode::Char(']'));
        k.key(&mut c, KeyCode::Char(']'));
        k.key(&mut c, KeyCode::Char('u'));
        let (today, _) = now_day_min();
        let (y, m, _) = civil_from_days(today);
        assert_eq!(c.title(), format!("calendar · {} {y}", MONTHS[m as usize - 1]));
        // j past the end and k past the start stay in range
        for _ in 0..10 {
            k.key(&mut c, KeyCode::Char('j'));
        }
        let s = k.render(&mut c, 140, 40);
        assert!(s.lines().any(|l| l.contains("▸") && l.contains("gym")), "{s}");
        for _ in 0..10 {
            k.key(&mut c, KeyCode::Char('k'));
        }
        assert!(k.render(&mut c, 140, 40).lines().any(|l| l.contains("▸") && l.contains("water plants")));
        // reopening reads it all back
        let mut c2 = Calendar::open_at(path.clone());
        assert_eq!(c2.badge().as_deref(), Some("3 today"));
        assert!(k.render(&mut c2, 140, 40).contains("standup meeting"));
    }

    /// Edit a plan's time so it re-sorts: the ▸ should stay on the plan you edited (the next `x` deletes the ▸ one).
    #[test]
    #[ignore = "fails: after an edit re-sorts the day, `pick` still points at the old index, so x deletes a different plan (calendar.rs:375-382)"]
    fn qa_calendar_edit_keeps_the_pick_on_the_edited_plan() {
        let mut k = Kit::new();
        let (mut c, path) = cal("cal-edit-pick");
        add(&mut k, &mut c, "9am alpha");
        add(&mut k, &mut c, "5pm beta");
        // pick alpha (first), move it to the evening
        k.key(&mut c, KeyCode::Char('e'));
        for _ in 0..20 {
            k.key(&mut c, KeyCode::Backspace);
        }
        k.typ(&mut c, "8pm alpha");
        k.key(&mut c, KeyCode::Enter);
        let s = k.render(&mut c, 140, 40);
        let picked = s.lines().find(|l| l.contains("▸")).unwrap_or("").to_string();
        // and x now deletes whatever is picked
        k.key(&mut c, KeyCode::Char('x'));
        let left: Vec<String> = load(&path).into_iter().map(|p| p.text).collect();
        assert!(picked.contains("alpha") && left == ["beta"], "picked after the edit: {picked:?}; x left {left:?}");
    }

    /// calendar.json is documented as the place plans live; a hand edit with a typo (or a half-written file) must
    /// not be silently replaced by an empty calendar on the next change.
    #[test]
    #[ignore = "fails: a calendar.json that doesn't parse loads as empty and the next add overwrites it (calendar.rs:140-142, 160-166)"]
    fn qa_calendar_unreadable_file_is_not_overwritten() {
        let dir = scratch("cal-corrupt");
        let path = dir.join("calendar.json");
        let original = "[\n  {\"day\": 20720, \"at\": 600, \"text\": \"keep me\"},\n  {\"day\": 20721, \"text\": \"and me\"}\n  oops\n]";
        std::fs::write(&path, original).unwrap();
        let mut k = Kit::new();
        let mut c = Calendar::open_at(path.clone());
        add(&mut k, &mut c, "new plan");
        let now = std::fs::read_to_string(&path).unwrap();
        let backup = std::fs::read_dir(&dir).unwrap().flatten().any(|e| e.path() != path && std::fs::read_to_string(e.path()).is_ok_and(|s| s.contains("keep me")));
        assert!(now.contains("keep me") || backup, "the old plans are gone; the file is now:\n{now}");
    }

    /// More plans on a day than the day list has rows: j walks the ▸ down, and the list should follow it, or e / x
    /// act on a plan you can't see.
    #[test]
    #[ignore = "fails: the chosen day's list never scrolls, so j moves the ▸ below the bottom and x deletes a plan that isn't on screen (calendar.rs:332-354)"]
    fn qa_calendar_picked_plan_stays_on_screen() {
        let mut k = Kit::new();
        let (mut c, path) = cal("cal-pick-offscreen");
        for i in 0..30 {
            add(&mut k, &mut c, &format!("{}:{:02} plan {i:02}", 6 + i / 4, (i % 4) * 15));
        }
        for _ in 0..29 {
            k.key(&mut c, KeyCode::Char('j'));
        }
        let s = k.render(&mut c, 140, 20);
        let shown = s.lines().any(|l| l.contains("▸") && l.contains("plan 29"));
        k.key(&mut c, KeyCode::Char('x'));
        let gone: Vec<String> = { let left: Vec<String> = load(&path).into_iter().map(|p| p.text).collect(); (0..30).map(|i| format!("plan {i:02}")).filter(|t| !left.contains(t)).collect() };
        assert!(shown, "picked plan 29 (x then deleted {gone:?}), but the ▸ isn't on screen:\n{s}");
    }

    #[test]
    fn qa_calendar_many_plans_small_sizes_and_mouse() {
        let mut k = Kit::new();
        let (mut c, _) = cal("cal-many");
        for i in 0..40 {
            add(&mut k, &mut c, &format!("{}:{:02} plan number {i} with a long description that won't fit anywhere", i % 24, (i * 7) % 60));
        }
        assert_eq!(c.badge().as_deref(), Some("40 today"));
        let s = k.render(&mut c, 140, 40);
        assert!(s.contains("more"), "the day's cell says how many more:\n{s}");
        for (w, h) in [(1, 1), (5, 3), (12, 6), (27, 9), (28, 10), (40, 12), (60, 20), (99, 30), (100, 30), (220, 70)] {
            k.render(&mut c, w, h);
            k.render_side(&mut c, w.min(34), h);
        }
        // click a day in the grid, wheel through months
        k.render(&mut c, 140, 40);
        let before = c.title();
        let area = Rect::new(0, 0, 140, 40);
        k.mouse(&mut c, wheel(true, 10, 10), area);
        assert_ne!(c.title(), before);
        k.mouse(&mut c, wheel(false, 10, 10), area);
        assert_eq!(c.title(), before);
        k.mouse(&mut c, click(3, 6), area); // somewhere in the first week row
        let s = k.render(&mut c, 140, 40);
        assert!(s.contains("today") || s.contains("ago") || s.contains("in "), "{s}");
    }
}

// ========================================================================================================= notes
mod notes {
    use super::*;
    use crate::panes::notes::Notes;

    fn md_files(dir: &std::path::Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "md")).collect()
    }

    fn read_all(dir: &std::path::Path) -> Vec<String> {
        md_files(dir).iter().map(|p| std::fs::read_to_string(p).unwrap_or_default()).collect()
    }

    #[test]
    fn qa_notes_create_rename_delete() {
        let dir = scratch("notes-crud");
        let mut k = Kit::new();
        let mut p = Notes::open_in(dir.clone(), None);
        assert_eq!(p.title(), "welcome to notes", "an empty folder gets a welcome note");
        assert_eq!(md_files(&dir).len(), 1);
        // new note, then rename it by editing its heading
        ctrl(&mut k, &mut p, 'n');
        assert_eq!(p.title(), "new note");
        assert_eq!(md_files(&dir).len(), 2);
        k.key_mod(&mut p, KeyCode::Home, KeyModifiers::CONTROL);
        k.key(&mut p, KeyCode::End);
        for _ in 0.."new note".len() {
            k.key(&mut p, KeyCode::Backspace);
        }
        k.typ(&mut p, "Groceries 🛒 für Montag");
        assert_eq!(p.subtitle().as_deref(), Some("editing…"));
        ctrl(&mut k, &mut p, 's');
        assert!(p.subtitle().unwrap().starts_with("saved "));
        assert_eq!(p.title(), "Groceries 🛒 für Montag");
        assert!(read_all(&dir).iter().any(|t| t.starts_with("# Groceries 🛒 für Montag\n")));
        let side = k.render_side(&mut p, 34, 10);
        assert!(side.contains("Groceries") && side.contains("welcome to notes"), "{side}");
        // ctrl+d then anything but y keeps it
        ctrl(&mut k, &mut p, 'd');
        let (s, _) = render_at(&mut k, &mut p, 100, 20, Rect::new(0, 0, 100, 20));
        assert!(s.contains("delete \"Groceries 🛒 für Montag\"?"), "{s}");
        k.key(&mut p, KeyCode::Char('n'));
        assert_eq!(md_files(&dir).len(), 2);
        // ctrl+d, Y deletes; then the other one; the folder is never left without a note
        ctrl(&mut k, &mut p, 'd');
        k.key(&mut p, KeyCode::Char('Y'));
        assert_eq!(md_files(&dir).len(), 1);
        assert_eq!(p.title(), "welcome to notes");
        assert!(k.notices().iter().any(|n| n.contains("note deleted")));
        ctrl(&mut k, &mut p, 'd');
        k.key(&mut p, KeyCode::Char('y'));
        assert_eq!(md_files(&dir).len(), 1);
        assert_eq!(p.title(), "new note");
        // closing the pane saves what's unsaved
        k.typ(&mut p, "last words");
        drop(p);
        assert!(read_all(&dir).iter().any(|t| t.ends_with("last words")));
    }

    #[test]
    fn qa_notes_unicode_editing_and_cursor() {
        let dir = scratch("notes-unicode");
        std::fs::write(dir.join("a.md"), "").unwrap();
        let mut k = Kit::new();
        let mut p = Notes::open_in(dir.clone(), None);
        k.typ(&mut p, "日本語 🎉");
        // the text area starts at (1, 1): the cursor sits after 2+2+2+1+2 = 9 columns
        let (_, cur) = render_at(&mut k, &mut p, 60, 12, Rect::new(0, 0, 60, 12));
        assert_eq!(cur, (1 + 9, 1), "cursor after wide characters");
        k.key(&mut p, KeyCode::Left);
        k.key(&mut p, KeyCode::Left);
        k.key(&mut p, KeyCode::Backspace); // removes 語
        k.key(&mut p, KeyCode::End);
        k.typ(&mut p, " é ñ ü مرحبا");
        ctrl(&mut k, &mut p, 's');
        assert_eq!(std::fs::read_to_string(dir.join("a.md")).unwrap(), "日本 🎉 é ñ ü مرحبا");
        assert_eq!(p.title(), "日本 🎉 é ñ ü مرحبا");
        // undo / redo over multi-byte text
        ctrl(&mut k, &mut p, 'z');
        ctrl(&mut k, &mut p, 'y');
        ctrl(&mut k, &mut p, 's');
        assert_eq!(std::fs::read_to_string(dir.join("a.md")).unwrap(), "日本 🎉 é ñ ü مرحبا");
        // wide text wraps without spilling past the pane, at every width
        k.typ(&mut p, &"漢字".repeat(60));
        for w in [4u16, 5, 7, 11, 30] {
            let (s, cur) = render_at(&mut k, &mut p, w, 10, Rect::new(0, 0, w, 10));
            assert!(cur.0 < w, "cursor x {} outside a {w}-wide pane", cur.0);
            let _ = s;
        }
        // ctrl+backspace deletes a word, ctrl+left/right jump words with accents in them
        ctrl(&mut k, &mut p, 'a'); // not a notes key: ignored
        k.key_mod(&mut p, KeyCode::Backspace, KeyModifiers::CONTROL);
        k.key_mod(&mut p, KeyCode::Left, KeyModifiers::CONTROL);
        k.key_mod(&mut p, KeyCode::Right, KeyModifiers::CONTROL);
    }

    #[test]
    fn qa_notes_very_long_notes() {
        let dir = scratch("notes-long");
        std::fs::write(dir.join("long.md"), "").unwrap();
        let mut k = Kit::new();
        let mut p = Notes::open_in(dir.clone(), None);
        // 5000 lines
        let text: String = (0..5000).map(|i| format!("line {i} of the long note\n")).collect();
        paste(&mut k, &mut p, &text);
        k.key_mod(&mut p, KeyCode::Home, KeyModifiers::CONTROL);
        let s = k.render(&mut p, 100, 30);
        assert!(s.contains("line 0 of the long note"), "{s}");
        for _ in 0..10 {
            k.key(&mut p, KeyCode::PageDown);
        }
        let s = k.render(&mut p, 100, 30);
        assert!(!s.contains("line 0 of") && s.contains("line 2"), "paged down:\n{s}");
        k.key_mod(&mut p, KeyCode::End, KeyModifiers::CONTROL);
        let s = k.render(&mut p, 100, 30);
        assert!(s.contains("line 4999 of the long note"), "{s}");
        // one 300 000-character line, typed into and scrolled
        let long = "word ".repeat(60_000);
        paste(&mut k, &mut p, &long);
        let t = Instant::now();
        for _ in 0..20 {
            k.key(&mut p, KeyCode::Char('x'));
        }
        for _ in 0..20 {
            k.key(&mut p, KeyCode::Up);
        }
        let per_key = t.elapsed().as_secs_f64() * 1000.0 / 40.0;
        let t = Instant::now();
        k.render(&mut p, 100, 30);
        let render_ms = t.elapsed().as_secs_f64() * 1000.0;
        println!("notes with {} chars: {per_key:.1} ms per key, {render_ms:.1} ms per render (debug build)", p_len(&dir, &mut k, &mut p));
        assert!(per_key < 2000.0, "{per_key} ms per key");
        ctrl(&mut k, &mut p, 's');
        let disk = std::fs::read_to_string(dir.join("long.md")).unwrap();
        assert!(disk.len() > 400_000 && disk.contains("line 4999") && disk.contains("xxxxxxxxxxxxxxxxxxxx"));
        // preview of the long note renders too
        ctrl(&mut k, &mut p, 'e');
        k.render(&mut p, 100, 30);
        k.key(&mut p, KeyCode::End);
        k.render(&mut p, 100, 30);
        ctrl(&mut k, &mut p, 'e');
    }

    fn p_len(dir: &std::path::Path, k: &mut Kit, p: &mut Notes) -> usize {
        ctrl(k, p, 's');
        std::fs::read_to_string(dir.join("long.md")).map(|s| s.chars().count()).unwrap_or(0)
    }

    #[test]
    fn qa_notes_crlf_files_stay_crlf() {
        let dir = scratch("notes-crlf");
        std::fs::write(dir.join("win.md"), "# windows note\r\n\r\nfirst line\r\n").unwrap();
        let mut k = Kit::new();
        let mut p = Notes::open_in(dir.clone(), None);
        assert_eq!(p.title(), "windows note");
        k.typ(&mut p, "added");
        ctrl(&mut k, &mut p, 's');
        let disk = std::fs::read_to_string(dir.join("win.md")).unwrap();
        assert_eq!(disk, "# windows note\r\n\r\nfirst line\r\nadded");
    }

    /// Notes written by Notepad and friends often start with a UTF-8 byte-order mark; the title should still be
    /// the heading text, not "\u{feff}# Title".
    #[test]
    #[ignore = "fails: title_of doesn't strip a UTF-8 BOM, so the sidebar shows \"# Title\" (notes.rs:36-39)"]
    fn qa_notes_bom_heading_title() {
        let dir = scratch("notes-bom");
        std::fs::write(dir.join("bom.md"), "\u{feff}# Shopping\n\n- eggs\n").unwrap();
        let p = Notes::open_in(dir, None);
        assert_eq!(p.title(), "Shopping");
    }

    /// A note that isn't UTF-8 (a legacy-encoded file) can't be opened; the editor still takes typing and shows
    /// "editing…", but nothing is ever saved: ctrl+s, the autosave and closing all drop it.
    #[test]
    #[ignore = "fails: when the open note couldn't be read, typing is accepted but silently never saved (notes.rs:175-192)"]
    fn qa_notes_typing_after_an_unreadable_note_is_not_lost() {
        let dir = scratch("notes-latin1");
        std::fs::write(dir.join("old.md"), b"# caf\xe9 notes\n\nwritten in latin-1\n").unwrap();
        let mut k = Kit::new();
        let mut p = Notes::open_in(dir.clone(), None);
        let s = k.render(&mut p, 100, 20);
        assert!(s.contains("can't open that note"), "{s}");
        k.typ(&mut p, "important thought");
        ctrl(&mut k, &mut p, 's');
        drop(p);
        let saved = read_all(&dir).iter().any(|t| t.contains("important thought"));
        assert!(saved, "typed text vanished; files: {:?}", read_all(&dir));
    }

    /// 60 notes and a sidebar with room for 10: the wheel should scroll the list so an old note can be clicked.
    /// (There is no key for switching notes, so the sidebar is the only way to reach one.)
    #[test]
    #[ignore = "fails: every sidebar render snaps side_scroll back to keep the open note visible, so the wheel can't reach notes below the fold (notes.rs:508-513, 543)"]
    fn qa_notes_sidebar_scrolls_to_old_notes() {
        let dir = scratch("notes-many");
        let now = std::time::SystemTime::now();
        for i in 0..60 {
            let p = dir.join(format!("note-{i:02}.md"));
            std::fs::write(&p, format!("# Note {i}\n\nbody {i}\n")).unwrap();
            let f = std::fs::File::options().write(true).open(&p).unwrap();
            f.set_modified(now - Duration::from_secs(60 * i)).unwrap(); // note 0 is the newest
        }
        let mut k = Kit::new();
        let mut p = Notes::open_in(dir.clone(), None);
        assert_eq!(p.title(), "Note 0", "the newest note opens");
        let area = Rect::new(0, 0, 34, 12);
        let side_mouse = |k: &mut Kit, p: &mut Notes, ev: MouseEvent| {
            let mut actions = vec![];
            let mut cx = Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: &mut actions, focused: true, time: 1.0 };
            p.side_mouse(ev, area, &mut cx);
        };
        let mut side = k.render_side(&mut p, 34, 12);
        for _ in 0..100 {
            side_mouse(&mut k, &mut p, wheel(true, 5, 5));
            side = k.render_side(&mut p, 34, 12); // the app draws the sidebar between wheel events
        }
        let row = side.lines().position(|l| l.contains("Note 59"));
        assert!(row.is_some(), "a hundred wheel steps down and the oldest note is still out of reach:\n{side}");
        side_mouse(&mut k, &mut p, click(5, row.unwrap() as u16));
        assert_eq!(p.title(), "Note 59");
    }

    #[test]
    fn qa_notes_preview_scroll_and_mouse() {
        let dir = scratch("notes-preview");
        let body: String = (0..200).map(|i| format!("- item **{i}** with `code`\n")).collect();
        std::fs::write(dir.join("list.md"), format!("# list\n\n{body}")).unwrap();
        let mut k = Kit::new();
        let mut p = Notes::open_in(dir.clone(), None);
        ctrl(&mut k, &mut p, 'e');
        let s = k.render(&mut p, 80, 20);
        assert!(s.contains("• item 0 with code"), "{s}");
        for code in [KeyCode::Char('j'), KeyCode::PageDown, KeyCode::Char(' '), KeyCode::End, KeyCode::Char('G'), KeyCode::Up, KeyCode::PageUp, KeyCode::Home, KeyCode::Char('g')] {
            k.key(&mut p, code);
            k.render(&mut p, 80, 20);
        }
        let area = Rect::new(0, 0, 80, 20);
        for _ in 0..100 {
            k.mouse(&mut p, wheel(true, 5, 5), area);
        }
        k.render(&mut p, 80, 20);
        k.key(&mut p, KeyCode::Esc);
        // editing mode: wheel and clicks beyond the text stay in range
        for _ in 0..100 {
            k.mouse(&mut p, wheel(true, 5, 5), area);
        }
        k.render(&mut p, 80, 20);
        k.mouse(&mut p, click(79, 19), area);
        k.mouse(&mut p, click(0, 0), area);
        k.typ(&mut p, "ok");
        ctrl(&mut k, &mut p, 's');
        assert!(std::fs::read_to_string(dir.join("list.md")).unwrap().contains("ok"));
    }
}

// ========================================================================================================= files
mod files {
    use super::*;
    use crate::panes::files::Files;

    fn tree(name: &str) -> PathBuf {
        let d = scratch(name);
        std::fs::create_dir_all(d.join("alpha").join("beta").join("gamma")).unwrap();
        std::fs::write(d.join("alpha").join("beta").join("gamma").join("deep.txt"), "at the bottom\n").unwrap();
        std::fs::write(d.join("alpha").join("a1.txt"), "a1\n").unwrap();
        std::fs::write(d.join("zeta.txt"), "z\n").unwrap();
        d
    }

    #[test]
    fn qa_files_nested_navigation_keeps_the_selection() {
        let d = tree("files-nav");
        let mut k = Kit::new();
        let mut p = Files::new(Some(d.clone()));
        let s = settle(&mut k, &mut p, 150, 40, 3000, |s| s.contains("zeta.txt"));
        assert!(s.contains("alpha") && s.contains("1 folder · 1 file"), "{s}");
        // down into alpha/beta/gamma with l / enter / right. Wait on the LIST column: the old preview (of the
        // folder we're entering) already shows the names we're waiting for.
        for (key, want) in [(KeyCode::Char('l'), "a1.txt"), (KeyCode::Enter, "gamma"), (KeyCode::Right, "deep.txt")] {
            k.key(&mut p, KeyCode::Char('j')); // first entry (folders come first)
            k.key(&mut p, key);
            let s = settle(&mut k, &mut p, 150, 40, 3000, |s| left(s, 60).contains(want));
            assert!(left(&s, 60).contains(want), "expected {want}:\n{s}");
        }
        assert!(p.title().ends_with("gamma"), "{}", p.title());
        // highlight deep.txt so the preview shows it, then go up: gamma (where we came from) is selected, so the
        // preview's heading turns to "gamma"
        k.key(&mut p, KeyCode::Char('j'));
        settle(&mut k, &mut p, 150, 40, 3000, |s| preview_head(s).contains("deep.txt"));
        k.key(&mut p, KeyCode::Backspace);
        let s = settle(&mut k, &mut p, 150, 40, 3000, |s| left(s, 60).contains("gamma") && preview_head(s).contains("gamma"));
        assert!(p.title().ends_with("beta") && preview_head(&s).contains("gamma"), "gamma selected after going up:\n{s}");
        k.key(&mut p, KeyCode::Char('h'));
        settle(&mut k, &mut p, 150, 40, 3000, |s| left(s, 60).contains("a1.txt"));
        k.key(&mut p, KeyCode::Left);
        let s = settle(&mut k, &mut p, 150, 40, 3000, |s| left(s, 60).contains("zeta.txt"));
        assert!(p.title().ends_with("files-nav"), "{}", p.title());
        assert!(s.contains("alpha"));
        // g / G / page keys stay in range
        for code in [KeyCode::Char('G'), KeyCode::PageDown, KeyCode::End, KeyCode::Char('g'), KeyCode::PageUp, KeyCode::Home, KeyCode::Up, KeyCode::Char('k')] {
            k.key(&mut p, code);
        }
        // enter on a file focuses the preview; q gives the list back
        k.key(&mut p, KeyCode::Char('G'));
        k.key(&mut p, KeyCode::Enter);
        let s = settle(&mut k, &mut p, 150, 40, 2000, |s| s.contains("esc back to the list"));
        assert!(s.contains("esc back to the list"), "{s}");
        k.key(&mut p, KeyCode::Char('q'));
        assert!(framed(&mut k, &mut p, 150, 40).contains("backspace up"));
    }

    #[test]
    fn qa_files_hidden_files() {
        let d = scratch("files-hidden");
        std::fs::write(d.join("visible.txt"), "v").unwrap();
        std::fs::write(d.join(".dotfile"), "d").unwrap();
        std::fs::create_dir_all(d.join("node_modules").join("x")).unwrap();
        std::fs::create_dir_all(d.join("target")).unwrap();
        std::fs::create_dir_all(d.join("src")).unwrap();
        #[cfg(windows)]
        {
            std::fs::write(d.join("sneaky.txt"), "s").unwrap();
            let _ = std::process::Command::new(std::env::var("SystemRoot").unwrap_or("C:\\Windows".into()) + "\\System32\\attrib.exe").arg("+h").arg(d.join("sneaky.txt")).output();
        }
        let mut k = Kit::new();
        let mut p = Files::new(Some(d.clone()));
        let s = settle(&mut k, &mut p, 150, 40, 3000, |s| s.contains("visible.txt"));
        assert!(s.contains("1 folder · 1 file"), "only src and visible.txt:\n{s}");
        let list = left(&s, 58); // the list column (the path in the frame title has "target" in it)
        for hidden in [".dotfile", "node_modules", "target", "sneaky.txt"] {
            assert!(!list.contains(hidden), "{hidden} shown by default:\n{list}");
        }
        k.key(&mut p, KeyCode::Char('.'));
        let s = framed(&mut k, &mut p, 150, 40);
        let extra = if cfg!(windows) { 1 } else { 0 };
        assert!(s.contains(&format!("3 folders · {} files · hidden shown", 2 + extra)), "{s}");
        let list = left(&s, 58);
        assert!(list.contains(".dotfile") && list.contains("node_modules") && list.contains("target"), "{list}");
        if cfg!(windows) {
            assert!(list.contains("sneaky.txt"), "the OS hidden attribute counts as hidden:\n{list}");
        }
        // select a hidden file, hide them again: the selection falls back to the folder row, nothing panics
        k.key(&mut p, KeyCode::Char('G'));
        k.key(&mut p, KeyCode::Char('.'));
        let s = framed(&mut k, &mut p, 150, 40);
        assert!(!left(&s, 58).contains(".dotfile") && s.contains("1 folder · 1 file"), "{s}");
        k.key(&mut p, KeyCode::Char('j'));
        k.key(&mut p, KeyCode::Char('j'));
        framed(&mut k, &mut p, 150, 40);
    }

    /// With hidden files off, the list hides dotfiles and build folders, but the preview of a folder (the folder
    /// row, or any subfolder you highlight) lists them all anyway.
    #[test]
    #[ignore = "fails: the folder preview ignores the hidden filter and lists dotfiles / node_modules (files/preview.rs:143-148)"]
    fn qa_files_folder_preview_respects_hidden() {
        let d = scratch("files-hidden-preview");
        std::fs::create_dir_all(d.join("proj").join("node_modules")).unwrap();
        std::fs::write(d.join("proj").join(".env"), "SECRET=1").unwrap();
        std::fs::write(d.join("proj").join("main.rs"), "fn main() {}").unwrap();
        let mut k = Kit::new();
        let mut p = Files::new(Some(d.clone()));
        settle(&mut k, &mut p, 150, 40, 3000, |s| s.contains("proj"));
        k.key(&mut p, KeyCode::Char('j')); // highlight proj/: its preview lists what's inside
        let s = settle(&mut k, &mut p, 150, 40, 3000, |s| s.contains("main.rs"));
        let preview: String = s.lines().skip(1).map(|l| l.chars().skip(62).collect::<String>() + "\n").collect();
        assert!(preview.contains("main.rs"), "{s}");
        assert!(!preview.contains(".env") && !preview.contains("node_modules"), "hidden entries in the preview:\n{preview}");
    }

    #[test]
    fn qa_files_unicode_and_long_names_at_every_width() {
        let d = scratch("files-unicode");
        let long = format!("{}.txt", "a-very-long-file-name-".repeat(8));
        for n in ["日本語のファイル.txt", "émoji 🎵 song.mp3", "no_extension", "trailing.dot.", "ÆØÅ.md", long.as_str(), "x.tar.gz"] {
            std::fs::write(d.join(n), "hello").unwrap();
        }
        std::fs::create_dir_all(d.join("папка")).unwrap();
        let mut k = Kit::new();
        let mut p = Files::new(Some(d.clone()));
        let s = settle(&mut k, &mut p, 150, 40, 3000, |s| s.contains("x.tar"));
        assert!(s.contains("папка") && s.contains("ÆØÅ") && s.contains("no_extension"), "{s}");
        for w in [1u16, 3, 8, 20, 26, 40, 49, 50, 51, 60, 80, 200] {
            for h in [1u16, 2, 4, 10, 40] {
                for n in 0..9 {
                    k.key(&mut p, if n == 0 { KeyCode::Char('g') } else { KeyCode::Char('j') });
                    framed(&mut k, &mut p, w, h);
                    k.render(&mut p, w, h);
                }
            }
        }
        // the preview shows the selected unicode file
        k.key(&mut p, KeyCode::Char('g'));
        k.key(&mut p, KeyCode::Char('j')); // папка
        k.key(&mut p, KeyCode::Char('j'));
        let s = settle(&mut k, &mut p, 150, 40, 2000, |s| s.contains("hello"));
        assert!(s.contains("hello"), "{s}");
    }

    #[test]
    fn qa_files_refresh_and_files_that_vanish() {
        let d = scratch("files-vanish");
        for n in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(d.join(n), n).unwrap();
        }
        let mut k = Kit::new();
        let mut p = Files::new(Some(d.clone()));
        settle(&mut k, &mut p, 150, 40, 3000, |s| s.contains("c.txt"));
        // delete c.txt behind the app's back (a.txt selected), refresh: gone
        k.key(&mut p, KeyCode::Char('j'));
        std::fs::remove_file(d.join("c.txt")).unwrap();
        k.key(&mut p, KeyCode::Char('r'));
        let s = settle(&mut k, &mut p, 150, 40, 3000, |s| !s.contains("c.txt"));
        assert!(!s.contains("c.txt") && s.contains("0 folders · 2 files"), "{s}");
        // a file deleted between the listing and its preview: the preview says so
        std::fs::remove_file(d.join("b.txt")).unwrap();
        k.key(&mut p, KeyCode::Char('g'));
        k.key(&mut p, KeyCode::Char('j'));
        k.key(&mut p, KeyCode::Char('j')); // b.txt, still listed
        let s = settle(&mut k, &mut p, 150, 40, 2000, |s| s.to_lowercase().contains("cannot find") || s.contains("No such file"));
        assert!(s.to_lowercase().contains("cannot find") || s.contains("No such file"), "preview of a vanished file:\n{s}");
        // new files show up on F5
        std::fs::write(d.join("new.txt"), "fresh").unwrap();
        k.key(&mut p, KeyCode::F(5));
        let s = settle(&mut k, &mut p, 150, 40, 3000, |s| s.contains("new.txt"));
        assert!(s.contains("new.txt"), "{s}");
        // the folder itself disappearing
        let sub = d.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let mut p2 = Files::new(Some(sub.clone()));
        settle(&mut k, &mut p2, 150, 40, 3000, |s| s.contains("(empty)"));
        std::fs::remove_dir_all(&sub).unwrap();
        k.key(&mut p2, KeyCode::Char('r'));
        let s = settle(&mut k, &mut p2, 150, 40, 3000, |s| !s.contains("loading"));
        assert!(!s.contains("loading…"), "{s}");
    }

    /// Select the last file, delete it outside oriel, press r (or F5): the new, shorter listing arrives and
    /// `refilter` looks up the old selection through the OLD `shown` indices into the NEW `all` list.
    #[test]
    #[ignore = "fails: panics 'index out of bounds: the len is 2 but the index is 2' in sel_entry during refilter after r/F5 (files.rs:217-219, 228)"]
    fn qa_files_refresh_after_the_selected_file_was_deleted() {
        let d = scratch("files-refresh-crash");
        for n in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(d.join(n), n).unwrap();
        }
        let mut k = Kit::new();
        let mut p = Files::new(Some(d.clone()));
        settle(&mut k, &mut p, 150, 40, 3000, |s| s.contains("c.txt"));
        k.key(&mut p, KeyCode::Char('G')); // c.txt
        std::fs::remove_file(d.join("c.txt")).unwrap();
        k.key(&mut p, KeyCode::Char('r'));
        let s = settle(&mut k, &mut p, 150, 40, 3000, |s| !s.contains("c.txt")); // the listing arrives: panic
        assert!(s.contains("0 folders · 2 files"), "{s}");
    }

    /// The same stale-index lookup, through the error branch: select a file, delete the whole folder outside
    /// oriel, press r. The listing comes back as an error, `all` is cleared, and `refilter` still reads the old
    /// selection through the old `shown` indices.
    #[test]
    #[ignore = "fails: panics 'index out of bounds: the len is 0 but the index is 0' when the open folder is deleted and r is pressed with a file selected (files.rs:527-532, 217, 228)"]
    fn qa_files_refresh_after_the_folder_was_deleted() {
        let d = scratch("files-folder-gone");
        let sub = d.join("doomed");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("only.txt"), "x").unwrap();
        let mut k = Kit::new();
        let mut p = Files::new(Some(sub.clone()));
        settle(&mut k, &mut p, 150, 40, 3000, |s| s.contains("only.txt"));
        k.key(&mut p, KeyCode::Char('j')); // only.txt
        std::fs::remove_dir_all(&sub).unwrap();
        k.key(&mut p, KeyCode::Char('r'));
        let s = settle(&mut k, &mut p, 150, 40, 3000, |s| !left(s, 60).contains("only.txt")); // the error arrives: panic
        assert!(!left(&s, 60).contains("only.txt"), "the listing never came back:\n{s}");
    }

    #[test]
    fn qa_files_long_text_preview() {
        let d = scratch("files-longtext");
        let text: String = (0..1000).map(|i| format!("row {i}\t{}\n", "=".repeat(i % 300))).collect();
        std::fs::write(d.join("big.log"), text).unwrap();
        std::fs::write(d.join("one-line.json"), "{\"k\": \"".to_string() + &"v".repeat(200_000) + "\"}").unwrap();
        let mut k = Kit::new();
        let mut p = Files::new(Some(d.clone()));
        settle(&mut k, &mut p, 150, 40, 3000, |s| s.contains("big.log"));
        k.key(&mut p, KeyCode::Char('j')); // big.log
        settle(&mut k, &mut p, 150, 40, 2000, |s| s.contains("row 0"));
        k.key(&mut p, KeyCode::Enter); // focus the preview
        for code in [KeyCode::Char('j'), KeyCode::PageDown, KeyCode::Char(' '), KeyCode::End] {
            k.key(&mut p, code);
        }
        let s = framed(&mut k, &mut p, 150, 40);
        assert!(s.contains("… (first 400 lines)") && s.contains("row 399"), "{s}");
        k.key(&mut p, KeyCode::Char('g'));
        assert!(framed(&mut k, &mut p, 150, 40).contains("row 0"));
        k.key(&mut p, KeyCode::Esc);
        k.key(&mut p, KeyCode::Char('j')); // one-line.json
        let s = settle(&mut k, &mut p, 150, 40, 2000, |s| s.contains("vvvvvvvv"));
        assert!(s.contains("vvvvvvvv"), "{s}");
    }
}

// ========================================================================================================== help
mod help {
    use super::*;
    use crate::panes::help::{Help, set_context};

    /// Every topic's title, walking the list with ↓.
    fn titles(k: &mut Kit, h: &mut Help) -> Vec<String> {
        let mut out = vec![];
        for _ in 0..40 {
            let t = h.title().trim_start_matches("help · ").to_string();
            if out.last() == Some(&t) {
                break;
            }
            out.push(t);
            k.key(h, KeyCode::Down);
        }
        out
    }

    #[test]
    fn qa_help_every_topic_renders_and_is_found_by_search() {
        let mut k = Kit::new();
        let mut h = Help::new();
        let all = titles(&mut k, &mut h);
        assert!(all.len() >= 14, "{all:?}");
        // each topic, at a few sizes, scrolled to the end and back
        for _ in 0..all.len() {
            k.key(&mut h, KeyCode::Up);
        }
        for t in &all {
            assert_eq!(h.title(), format!("help · {t}"));
            let s = k.render(&mut h, 150, 44);
            assert!(s.contains(t.as_str()), "{t}:\n{s}");
            for (w, hh) in [(20, 6), (60, 20), (240, 80)] {
                k.render(&mut h, w, hh);
            }
            for _ in 0..30 {
                k.key(&mut h, KeyCode::PageDown);
            }
            let end = k.render(&mut h, 150, 20);
            assert!(end.lines().filter(|l| !l.trim().is_empty()).count() > 1, "scrolled past the end of {t}:\n{end}");
            k.key(&mut h, KeyCode::PageUp);
            k.key(&mut h, KeyCode::Tab);
        }
        // searching a topic's title finds it (and the side list shows it)
        for t in &all {
            k.key(&mut h, KeyCode::Char('/'));
            k.typ(&mut h, t);
            let side = k.render_side(&mut h, 40, 30);
            assert!(side.contains(t.as_str()), "search {t:?}:\n{side}");
            k.key(&mut h, KeyCode::Esc);
        }
        // case doesn't matter
        k.key(&mut h, KeyCode::Char('/'));
        k.typ(&mut h, "CLIPBOARD");
        let side = k.render_side(&mut h, 40, 30);
        assert!(side.contains("keys & tabs"), "{side}");
        k.key(&mut h, KeyCode::Enter);
        assert!(k.render(&mut h, 150, 44).contains("keys & tabs"));
        k.key(&mut h, KeyCode::Esc); // esc outside the box clears the query
        assert!(k.render_side(&mut h, 40, 30).contains("music"));
    }

    #[test]
    fn qa_help_opens_on_the_topic_of_every_app() {
        let mut k = Kit::new();
        for (app, want) in [("ai", "chat"), ("agents", "agents & lead mode"), ("ais", "your AIs"), ("music", "music"), ("system", "system"), ("files", "files"), ("notes", "notes"), ("calendar", "calendar"), ("storage", "storage"), ("terminal", "terminal"), ("alerts", "alerts"), ("themes", "themes & config"), ("home", "getting started")] {
            let mut h = Help::new();
            k.key(&mut h, KeyCode::Down); // away from the first topic
            set_context(app);
            assert_eq!(h.title(), format!("help · {want}"), "title from {app}");
            let s = k.render(&mut h, 150, 44);
            assert!(s.contains(want), "{app}:\n{s}");
            // once per ask: the next render keeps where you moved to
            k.key(&mut h, KeyCode::Down);
            let moved = h.title();
            k.render(&mut h, 150, 44);
            assert_eq!(h.title(), moved);
        }
        // an app without a topic leaves it where it was
        let mut h = Help::new();
        set_context("help");
        k.render(&mut h, 150, 44);
        assert_eq!(h.title(), "help · getting started");
    }

    /// Search for something no topic has, press enter (the query stays), then ↓ / ↑: index out of bounds.
    #[test]
    #[ignore = "fails: panics (index out of bounds) on ↓/↑/j/k after a search with no results (help.rs:410-419)"]
    fn qa_help_arrow_keys_after_a_search_with_no_results() {
        let mut k = Kit::new();
        let mut h = Help::new();
        k.key(&mut h, KeyCode::Char('/'));
        k.typ(&mut h, "zzzzqqq");
        let side = k.render_side(&mut h, 40, 20);
        assert!(!side.contains("music"), "{side}");
        k.key(&mut h, KeyCode::Enter);
        k.key(&mut h, KeyCode::Down); // panics here
        k.key(&mut h, KeyCode::Char('k'));
        k.render(&mut h, 150, 44);
    }

    #[test]
    fn qa_help_mouse_and_scroll() {
        let mut k = Kit::new();
        let mut h = Help::new();
        let area = Rect::new(0, 0, 150, 20);
        for _ in 0..200 {
            k.mouse(&mut h, wheel(true, 5, 5), area);
        }
        let s = k.render(&mut h, 150, 20);
        assert!(s.contains("oriel --tour") || s.contains("tour"), "clamped to the end:\n{s}");
        for _ in 0..200 {
            k.mouse(&mut h, wheel(false, 5, 5), area);
        }
        assert!(k.render(&mut h, 150, 20).contains("oriel in one minute"));
        // click topics in the sidebar
        k.render_side(&mut h, 30, 20);
        let mut actions = vec![];
        {
            let mut cx = Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: &mut actions, focused: true, time: 1.0 };
            h.side_mouse(click(3, 4), Rect::new(0, 0, 30, 20), &mut cx);
        }
        assert_ne!(h.title(), "help · getting started");
    }
}

// ======================================================================================================== themes
mod themes {
    use super::*;
    use crate::panes::themes::Themes;
    use crate::theme::{self, COLOR_KEYS};
    use ratatui::style::Color;

    /// Theme files go to a scratch folder (per test thread), never the real config folder.
    fn with_dir(name: &str) -> PathBuf {
        let d = scratch(&format!("themes-{name}"));
        theme::TEST_DIR.with(|t| *t.borrow_mut() = Some(d.clone()));
        d
    }

    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            theme::TEST_DIR.with(|t| *t.borrow_mut() = None);
        }
    }

    /// Do what the app does with ApplyTheme; returns the names applied.
    fn apply(k: &mut Kit) -> Vec<String> {
        let mut out = vec![];
        for a in std::mem::take(&mut k.actions) {
            if let Action::ApplyTheme(n) = a {
                k.theme = theme::get(&n);
                out.push(n);
            }
        }
        out
    }

    fn msg_line(k: &mut Kit, p: &mut Themes) -> String {
        k.render(p, 150, 40)
    }

    #[test]
    fn qa_themes_every_key_on_every_colour() {
        let d = with_dir("keys");
        let _r = Reset;
        let mut k = Kit::new();
        k.theme = theme::get("ocean");
        let mut p = Themes::new();
        for (row, (key, _)) in COLOR_KEYS.iter().enumerate() {
            // a hue turn changes every colour (a grey or the terminal's own gets a little colour first)
            let before = theme::field(&k.theme, key);
            assert!(k.key(&mut p, KeyCode::Right), "{key}: → not handled");
            assert_eq!(apply(&mut k), ["my-ocean"], "{key}: →");
            assert_ne!(theme::field(&k.theme, key), before, "{key}: → changed nothing");
            // the fine turn (shift) and the rest: handled and saved (see the fine-hue test for what they change)
            assert!(k.key_mod(&mut p, KeyCode::Left, KeyModifiers::SHIFT));
            assert_eq!(apply(&mut k), ["my-ocean"], "{key}: shift+←");
            for c in ['[', ']', '-', '=', '+'] {
                assert!(k.key(&mut p, KeyCode::Char(c)), "{key}: {c}");
                assert_eq!(apply(&mut k), ["my-ocean"], "{key}: {c}");
            }
            assert!(matches!(theme::field(&k.theme, key), Color::Rgb(..)), "{key} is a real colour after editing");
            let s = msg_line(&mut k, &mut p);
            assert!(s.lines().any(|l| l.contains("▸") && l.contains(key)), "row {row} ({key}) selected:\n{s}");
            k.key(&mut p, KeyCode::Char('j'));
        }
        // the rainbow row: every toggle key flips it, and the file keeps it
        let start = k.theme.animated;
        for code in [KeyCode::Enter, KeyCode::Char(' '), KeyCode::Left, KeyCode::Right] {
            let was = k.theme.animated;
            k.key(&mut p, code);
            apply(&mut k);
            assert_ne!(k.theme.animated, was, "{code:?} on rainbow");
        }
        assert_eq!(k.theme.animated, start);
        k.key(&mut p, KeyCode::Down); // stays on the last row
        k.key(&mut p, KeyCode::Enter);
        apply(&mut k);
        assert_ne!(k.theme.animated, start);
        let file = std::fs::read_to_string(d.join("my-ocean.toml")).unwrap();
        assert!(file.contains(&format!("rainbow = {}", k.theme.animated)) && file.contains("base = \"ocean\""), "{file}");
        assert!(theme::problems("my-ocean").is_empty(), "{:?}", theme::problems("my-ocean"));
        // what's on disk is what's on screen
        let back = theme::get("my-ocean");
        for (key, _) in COLOR_KEYS {
            assert_eq!(theme::field(&back, key), theme::field(&k.theme, key), "{key} round trip");
        }
        // k / up back to the top, and past it
        for _ in 0..20 {
            k.key(&mut p, KeyCode::Char('k'));
        }
        k.key(&mut p, KeyCode::Up);
        let s = msg_line(&mut k, &mut p);
        assert!(s.lines().any(|l| l.contains("▸") && l.contains("accent")), "{s}");
        assert!(s.contains("yours · starts from ocean"));
        // keys it doesn't use fall through
        assert!(!k.key(&mut p, KeyCode::Char('z')));
        assert!(!k.key(&mut p, KeyCode::F(3)));
    }

    /// The fine hue step (shift+← →, 2°) goes through 8-bit RGB on every press, so on a dark or greyish colour
    /// (a background, a frame) the rounding eats it: pressing it any number of times changes nothing.
    #[test]
    #[ignore = "fails: shift+←/→ (2° hue) on a dark colour rounds back to the same RGB every press, so it never moves (themes.rs:59-69)"]
    fn qa_themes_fine_hue_step_moves_dark_colours() {
        let _d = with_dir("fine-hue");
        let _r = Reset;
        let mut k = Kit::new();
        k.theme = theme::get("ultra");
        let mut p = Themes::new();
        // pick "background" and give it a typical dark background colour
        let bg = COLOR_KEYS.iter().position(|(k, _)| *k == "background").unwrap();
        for _ in 0..bg {
            k.key(&mut p, KeyCode::Down);
        }
        k.key(&mut p, KeyCode::Enter);
        for _ in 0..30 {
            k.key(&mut p, KeyCode::Backspace);
        }
        k.typ(&mut p, "#16141c");
        k.key(&mut p, KeyCode::Enter);
        apply(&mut k);
        let start = k.theme.bg;
        for _ in 0..10 {
            k.key_mod(&mut p, KeyCode::Right, KeyModifiers::SHIFT);
            apply(&mut k);
        }
        assert_ne!(k.theme.bg, start, "ten fine hue steps and the background is still {start:?}");
    }

    #[test]
    fn qa_themes_typed_colours() {
        let _d = with_dir("typed");
        let _r = Reset;
        let mut k = Kit::new();
        k.theme = theme::get("ultra");
        let mut p = Themes::new();
        for (typed, want) in [("#abc", Color::Rgb(0xaa, 0xbb, 0xcc)), ("#ABCDEF", Color::Rgb(0xab, 0xcd, 0xef)), ("  #123456  ", Color::Rgb(0x12, 0x34, 0x56)), ("bright-blue", Color::LightBlue), ("Bright Magenta", Color::LightMagenta), ("grey", Color::Gray), ("terminal", Color::Reset)] {
            k.key(&mut p, KeyCode::Enter);
            let s = msg_line(&mut k, &mut p);
            assert!(s.contains("colour › "), "{s}");
            for _ in 0..30 {
                k.key(&mut p, KeyCode::Backspace);
            }
            k.typ(&mut p, typed);
            k.key(&mut p, KeyCode::Enter);
            apply(&mut k);
            assert_eq!(k.theme.accent, want, "typed {typed:?}");
        }
        // the box starts with the current colour's name
        k.key(&mut p, KeyCode::Enter);
        assert!(msg_line(&mut k, &mut p).contains("colour › terminal"));
        k.key(&mut p, KeyCode::Esc);
        assert!(!msg_line(&mut k, &mut p).contains("colour ›"), "esc closes the box");
        assert!(k.actions.is_empty());
    }

    #[test]
    fn qa_themes_bad_colours_change_nothing() {
        let d = with_dir("bad");
        let _r = Reset;
        let mut k = Kit::new();
        k.theme = theme::get("forest");
        let mut p = Themes::new();
        let before = k.theme.accent;
        for bad in ["#12345", "#ggg", "#1234567", "123456", "", "#", "rgb(1,2,3)", "bright", "#12 34 56", "ｂｌｕｅ"] {
            k.key(&mut p, KeyCode::Enter);
            for _ in 0..30 {
                k.key(&mut p, KeyCode::Backspace);
            }
            k.typ(&mut p, bad);
            k.key(&mut p, KeyCode::Enter);
            assert!(apply(&mut k).is_empty(), "{bad:?} was applied");
            assert_eq!(k.theme.accent, before);
            let s = msg_line(&mut k, &mut p);
            assert!(s.contains("isn't a colour: write it like #b48cff"), "{bad:?}:\n{s}");
        }
        assert!(std::fs::read_dir(&d).unwrap().next().is_none(), "no theme file written for bad input");
    }

    #[test]
    fn qa_themes_reset_restores_the_base_colour() {
        let _d = with_dir("reset");
        let _r = Reset;
        let mut k = Kit::new();
        k.theme = theme::get("amber");
        let mut p = Themes::new();
        k.key(&mut p, KeyCode::Char('j')); // shine
        for code in [KeyCode::Right, KeyCode::Right, KeyCode::Right, KeyCode::Char(']')] {
            k.key(&mut p, code);
            apply(&mut k); // the app applies after every key
        }
        assert_eq!(k.theme.name, "my-amber");
        assert_ne!(k.theme.shine, theme::get("amber").shine);
        k.key(&mut p, KeyCode::Char('r'));
        apply(&mut k);
        assert_eq!(k.theme.shine, theme::get("amber").shine);
        assert!(msg_line(&mut k, &mut p).contains("shine is back to amber's"));
        // r on the rainbow row does nothing
        for _ in 0..20 {
            k.key(&mut p, KeyCode::Down);
        }
        k.key(&mut p, KeyCode::Char('r'));
        assert!(apply(&mut k).is_empty());
        // o on your own theme (the editor launch is a no-op in tests), and on a built-in
        k.key(&mut p, KeyCode::Char('o'));
        assert!(msg_line(&mut k, &mut p).contains("opened the file"));
        k.theme = theme::get("amber");
        k.key(&mut p, KeyCode::Char('o'));
        assert!(msg_line(&mut k, &mut p).contains("a built-in has no file"));
    }

    /// Resetting a colour of a built-in you never changed is not a change; it still makes "my-ultra" (a new
    /// theme file, applied, listed as yours).
    #[test]
    #[ignore = "fails: r on an untouched built-in theme saves and applies a copy (my-ultra) (themes.rs:274-279, 41-56)"]
    fn qa_themes_reset_on_an_untouched_builtin_makes_no_copy() {
        let d = with_dir("reset-builtin");
        let _r = Reset;
        let mut k = Kit::new();
        k.theme = theme::get("ultra");
        let mut p = Themes::new();
        k.key(&mut p, KeyCode::Char('r'));
        let applied = apply(&mut k);
        let files: Vec<_> = std::fs::read_dir(&d).unwrap().flatten().map(|e| e.file_name()).collect();
        assert!(applied.is_empty() && files.is_empty(), "applied {applied:?}, wrote {files:?}");
    }

    #[test]
    fn qa_themes_new_theme_names() {
        let d = with_dir("names");
        let _r = Reset;
        let mut k = Kit::new();
        k.theme = theme::get("dracula");
        let mut p = Themes::new();
        let make = |k: &mut Kit, p: &mut Themes, name: Option<&str>| -> Vec<String> {
            k.key(p, KeyCode::Char('n'));
            if let Some(n) = name {
                for _ in 0..40 {
                    k.key(p, KeyCode::Backspace);
                }
                k.typ(p, n);
            }
            k.key(p, KeyCode::Enter);
            apply(k)
        };
        assert_eq!(make(&mut k, &mut p, None), ["my-theme"], "the suggested name");
        assert!(msg_line(&mut k, &mut p).contains("made my-theme"));
        assert_eq!(theme::base_of("my-theme"), "dracula");
        assert_eq!(make(&mut k, &mut p, None), ["my-theme-2"], "a free name next time");
        assert_eq!(theme::base_of("my-theme-2"), "dracula", "a copy of a copy still starts from the built-in");
        assert_eq!(make(&mut k, &mut p, Some("ultra")), ["ultra-2"], "never replaces a built-in");
        assert_eq!(make(&mut k, &mut p, Some("")), ["mine"]);
        assert_eq!(make(&mut k, &mut p, Some("../../Evil Name")), ["evil-name"]);
        assert_eq!(make(&mut k, &mut p, Some("Café ☕")), ["café"]);
        // esc cancels
        k.key(&mut p, KeyCode::Char('n'));
        k.key(&mut p, KeyCode::Esc);
        assert!(apply(&mut k).is_empty());
        // every file landed in the scratch themes folder and nowhere else
        let mut files: Vec<String> = std::fs::read_dir(&d).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        files.sort();
        assert_eq!(files, ["café.toml", "evil-name.toml", "mine.toml", "my-theme-2.toml", "my-theme.toml", "ultra-2.toml"]);
        assert!(!d.parent().unwrap().join("evil-name.toml").exists());
        // they're all listed in the sidebar as yours
        let side = k.render_side(&mut p, 30, 40);
        for n in ["café", "evil-name", "mine", "my-theme", "ultra-2"] {
            assert!(side.contains(n), "{n}:\n{side}");
        }
        assert!(side.contains("yours"));
    }

    /// Windows device names (con, nul, aux, prn, com1, lpt1) pass free_name untouched. Whatever happens to the
    /// write, the theme that gets applied must be one that loads back from its own file. Only "nul" is tried:
    /// the others could reach a console, printer or serial port if Windows maps them to the device.
    #[test]
    fn qa_themes_windows_device_names() {
        let d = with_dir("devices");
        let _r = Reset;
        let mut k = Kit::new();
        k.theme = theme::get("ocean");
        let mut p = Themes::new();
        let mut bad = vec![];
        for want in ["nul", "NUL"] {
            k.key(&mut p, KeyCode::Char('n'));
            for _ in 0..40 {
                k.key(&mut p, KeyCode::Backspace);
            }
            k.typ(&mut p, want);
            k.key(&mut p, KeyCode::Enter);
            for name in apply(&mut k) {
                let back = theme::get(&name);
                let file = d.join(format!("{name}.toml"));
                let on_disk = std::fs::read_to_string(&file).unwrap_or_default();
                if back.name != name || !on_disk.contains("base = \"ocean\"") {
                    bad.push(format!("typed {want:?}: applied {name:?}, loads back as {:?}, file {} ({} bytes)", back.name, file.display(), on_disk.len()));
                }
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    #[test]
    fn qa_themes_broken_file_is_reported() {
        let d = with_dir("broken");
        let _r = Reset;
        std::fs::write(d.join("typo.toml"), "base = \"nope\"\naccent = \"#zzzzzz\"\nrainbow = \"yes\"\n").unwrap();
        let mut k = Kit::new();
        k.theme = theme::get("typo");
        assert_eq!(k.theme.name, "typo");
        let mut p = Themes::new();
        let s = msg_line(&mut k, &mut p);
        assert!(s.contains("⚠ typo.toml:"), "{s}");
        let problems = theme::problems("typo");
        assert_eq!(problems.len(), 3, "{problems:?}");
        for (w, h) in [(1, 1), (20, 5), (60, 20), (111, 30), (112, 30), (200, 60)] {
            k.render(&mut p, w, h);
            k.render_side(&mut p, w.min(30), h);
        }
    }
}

// ======================================================================================================== alerts
mod alerts {
    use super::*;
    use crate::alerts::{self as center, Alert, Kind};
    use crate::panes::alerts::Alerts;

    fn alert(text: &str, kind: Kind, app: Option<&str>, pane: Option<u64>, ago: i64) -> Alert {
        Alert { at: center::now() - ago, kind, text: text.into(), app: app.map(String::from), read: false, pane }
    }

    /// What this module's tests push; anything else in the center came from another test's app meanwhile.
    const OURS: &[&str] = &["qa alert #", "from a pane", "from the calendar", "a new version", "from somewhere odd", "build failed:", "a very long alert", "pick-test "];

    fn foreign() -> bool {
        center::with(|l| l.iter().any(|a| !OURS.iter().any(|p| a.text.starts_with(p))))
    }

    fn texts() -> Vec<String> {
        center::with(|l| l.iter().map(|a| a.text.clone()).collect())
    }

    fn clear() {
        center::clear();
    }

    /// Another test (the app's own tests push real alerts) can add to the global center at any moment, which
    /// shifts every row. A failed check with a foreign alert present is that, not a bug: the scenario is re-run.
    enum Fail {
        Interfered,
        Bug(String),
    }

    macro_rules! check {
        ($cond:expr, $($fmt:tt)+) => {
            if !$cond {
                return Err(if foreign() { Fail::Interfered } else { Fail::Bug(format!($($fmt)+)) });
            }
        };
    }

    /// Stop here if another test's alert is in the list (just before a key that acts on a row position).
    fn clean() -> Result<(), Fail> {
        if foreign() { Err(Fail::Interfered) } else { Ok(()) }
    }

    /// Run a scenario on the global center until it runs without interference; panic on a real failure.
    /// Checks never hold the center's lock, so a failure can't poison it for the app's tests.
    fn run_alone(name: &str, scenario: fn() -> Result<(), Fail>) {
        let _g = CENTER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for _ in 0..40 {
            clear();
            let r = scenario();
            clear();
            match r {
                Ok(()) => return,
                Err(Fail::Bug(msg)) => panic!("{msg}"),
                Err(Fail::Interfered) => std::thread::sleep(Duration::from_millis(250)),
            }
        }
        eprintln!("{name}: skipped, other tests kept pushing alerts into the global center for 10 s");
    }

    /// The event center is one global list (the app's too), so everything that touches it is in one scenario.
    #[test]
    fn qa_alerts_many_dismiss_clear_jump() {
        run_alone("qa_alerts_many_dismiss_clear_jump", many_dismiss_clear_jump);
    }

    fn many_dismiss_clear_jump() -> Result<(), Fail> {
        // many: the list keeps the newest 200
        for i in 0..250 {
            center::push(alert(&format!("qa alert #{i}"), Kind::AgentDone, None, None, 250 - i));
        }
        let t = texts();
        check!(t.len() == 200, "kept {} alerts, want the newest 200", t.len());
        check!(t.first().map(String::as_str) == Some("qa alert #50") && t.last().map(String::as_str) == Some("qa alert #249"), "oldest {:?}, newest {:?}", t.first(), t.last());
        check!(center::unread() == 200, "unread {}", center::unread());
        let mut k = Kit::new();
        let mut p = Alerts::new();
        check!(p.badge().as_deref() == Some("200 new"), "badge {:?}", p.badge());
        clean()?;
        let s = k.render(&mut p, 120, 12);
        check!(s.lines().nth(1).unwrap_or("").contains("qa alert #249"), "newest first (row 0 is air):\n{s}");
        check!(center::unread() == 0, "seen once rendered with focus, still {} unread", center::unread());
        check!(p.badge().is_none(), "badge {:?}", p.badge());
        // walk down past the screen: the picked one stays on screen
        for _ in 0..30 {
            k.key(&mut p, KeyCode::Char('j'));
        }
        clean()?;
        let s = k.render(&mut p, 120, 12);
        let picked = s.lines().find(|l| l.contains("▸")).unwrap_or_default().to_string();
        check!(picked.contains("qa alert #219"), "picked row: {picked:?}\n{s}");
        // x dismisses exactly the picked one
        clean()?;
        k.key(&mut p, KeyCode::Char('x'));
        let t = texts();
        check!(t.len() == 199 && !t.iter().any(|a| a == "qa alert #219") && t.iter().any(|a| a == "qa alert #218"), "x on #219 left {} alerts, #219 still there: {}", t.len(), t.iter().any(|a| a == "qa alert #219"));
        // enter with nowhere to go
        k.key(&mut p, KeyCode::Enter);
        check!(k.notices().iter().any(|n| n == "that one has nowhere to go"), "{:?}", k.notices());
        // down past the end, up past the top
        for _ in 0..500 {
            k.key(&mut p, KeyCode::Down);
        }
        clean()?;
        let s = k.render(&mut p, 120, 12);
        check!(s.lines().any(|l| l.contains("▸") && l.contains("qa alert #50")), "{s}");
        for _ in 0..500 {
            k.key(&mut p, KeyCode::Up);
        }
        // c clears everything
        k.key(&mut p, KeyCode::Char('c'));
        check!(texts().is_empty(), "c left {:?}", texts());
        let s = k.render(&mut p, 120, 12);
        check!(s.contains("nothing yet"), "{s}");
        for code in [KeyCode::Char('x'), KeyCode::Enter, KeyCode::Char('j'), KeyCode::Char('k'), KeyCode::Char('c')] {
            k.key(&mut p, code); // an empty list: nothing to do, no panic
        }

        // jumping back: to the pane it came from, else its app, else say so
        for a in [
            alert("from a pane", Kind::NeedsYou, Some("agents"), Some(42), 30),
            alert("from the calendar", Kind::Calendar, Some("calendar"), None, 20),
            alert("a new version", Kind::Update, Some("updates"), None, 10),
            alert("from somewhere odd", Kind::Memory, Some("not-an-app"), None, 5),
        ] {
            center::push(a);
        }
        k.actions.clear();
        k.render(&mut p, 120, 12);
        clean()?;
        k.key(&mut p, KeyCode::Enter); // newest: not-an-app
        check!(k.notices().iter().any(|n| n == "that one has nowhere to go"), "{:?}", k.notices());
        k.key(&mut p, KeyCode::Char('j'));
        clean()?;
        k.key(&mut p, KeyCode::Enter);
        check!(k.actions.iter().any(|a| matches!(a, Action::GotoApp("updates"))), "enter on the update alert");
        k.key(&mut p, KeyCode::Char('j'));
        clean()?;
        k.key(&mut p, KeyCode::Enter);
        check!(k.actions.iter().any(|a| matches!(a, Action::GotoApp("calendar"))), "enter on the calendar alert");
        k.key(&mut p, KeyCode::Char('j'));
        clean()?;
        k.key(&mut p, KeyCode::Enter);
        check!(k.actions.iter().any(|a| matches!(a, Action::FocusPane(42))), "enter on the pane's alert");
        // mouse: click a row picks it; the wheel past the end is clamped by the next render
        let s = k.render(&mut p, 120, 12);
        let Some(row) = s.lines().position(|l| l.contains("from the calendar")) else { return Err(if foreign() { Fail::Interfered } else { Fail::Bug(format!("no calendar row:\n{s}")) }) };
        k.mouse(&mut p, click(10, row as u16), Rect::new(0, 0, 120, 12));
        k.actions.clear();
        clean()?;
        k.key(&mut p, KeyCode::Enter);
        check!(k.actions.iter().any(|a| matches!(a, Action::GotoApp("calendar"))), "a click picks the row");
        for _ in 0..50 {
            k.mouse(&mut p, wheel(true, 10, 3), Rect::new(0, 0, 120, 12));
        }
        let s = k.render(&mut p, 120, 12);
        check!(s.lines().any(|l| l.contains("▸") && l.contains("from a pane")), "{s}");
        // odd texts: newlines/tabs don't break the rows; long ones are cut; ages read right
        center::push(alert("build failed:\nerror[E0425]: cannot find value\tx", Kind::BuildFailed, None, None, 90_000));
        center::push(alert(&"a very long alert ".repeat(30), Kind::Usage, Some("ais"), None, -50));
        let s = k.render(&mut p, 100, 12);
        let rows: Vec<&str> = s.lines().skip(1).collect();
        check!(rows[0].contains("just now"), "future timestamps read as just now:\n{s}");
        check!(rows[1].contains("1d") && rows[1].contains("cannot find value"), "{s}");
        check!(rows.iter().all(|l| unicode_width::UnicodeWidthStr::width(*l) <= 100), "a row wider than the pane:\n{s}");
        for (w, h) in [(1, 1), (10, 3), (30, 5), (60, 8), (200, 60)] {
            k.render(&mut p, w, h);
        }
        Ok(())
    }

    /// Pick an alert, then a new one arrives (an agent finishes while you're reading): the ▸ is a row number, so
    /// it now sits on a different alert, and x dismisses that one instead of the one you picked.
    #[test]
    #[ignore = "fails: the pick is a row index into a newest-first list, so a new alert shifts it and x / enter act on a different alert (panes/alerts.rs:69-75, 108, 124-126)"]
    fn qa_alerts_pick_survives_a_new_alert() {
        let _g = CENTER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        for i in 0..5 {
            center::push(alert(&format!("pick-test #{i}"), Kind::AgentDone, None, None, 100 - i));
        }
        let mut k = Kit::new();
        let mut p = Alerts::new();
        k.render(&mut p, 120, 12);
        k.key(&mut p, KeyCode::Char('j'));
        k.key(&mut p, KeyCode::Char('j')); // rows: #4 #3 [#2] #1 #0
        let s = k.render(&mut p, 120, 12);
        let before = s.lines().find(|l| l.contains("▸")).unwrap_or_default().to_string();
        assert!(before.contains("pick-test #2"), "{s}");
        center::push(alert("pick-test NEW", Kind::BuildFailed, None, None, 0));
        let s = k.render(&mut p, 120, 12);
        let after = s.lines().find(|l| l.contains("▸")).unwrap_or_default().to_string();
        k.key(&mut p, KeyCode::Char('x'));
        let left = texts();
        clear();
        assert!(after.contains("pick-test #2") && !left.iter().any(|t| t == "pick-test #2"), "picked #2; after the new alert the ▸ is on {after:?}; x left {left:?}");
    }
}

// ======================================================================================================= updates
mod updates {
    use super::*;
    use crate::panes::updates::Updates;

    #[test]
    fn qa_updates_offline_screen_and_keys() {
        let mut k = Kit::new();
        let mut p = Updates::new();
        let s = k.render(&mut p, 110, 30);
        assert!(s.contains(&format!("oriel {}", crate::update::VERSION)), "{s}");
        let s = settle(&mut k, &mut p, 110, 30, 3000, |s| s.contains("couldn't check"));
        assert!(s.contains("couldn't check: no network in tests") && s.contains("c check again"), "{s}");
        assert!(!s.contains("update now"), "nothing to update to:\n{s}");
        // enter with no release does nothing; c checks again
        k.key(&mut p, KeyCode::Enter);
        k.key(&mut p, KeyCode::Char('c'));
        let s = settle(&mut k, &mut p, 110, 30, 3000, |s| s.contains("couldn't check") && !s.contains("checking GitHub"));
        assert!(s.contains("couldn't check"), "{s}");
        // scrolling stays in range
        for code in [KeyCode::Down, KeyCode::Char('j'), KeyCode::PageDown, KeyCode::PageDown, KeyCode::Up, KeyCode::Char('k'), KeyCode::PageUp] {
            k.key(&mut p, code);
            k.render(&mut p, 110, 30);
        }
        assert!(k.render(&mut p, 110, 30).contains("oriel "));
        assert!(!k.key(&mut p, KeyCode::Char('z')));
        for (w, h) in [(1, 1), (8, 3), (30, 6), (60, 12)] {
            k.render(&mut p, w, h);
        }
    }
}

// ============================================================================================= every tool, every size
mod sizes {
    use super::*;

    /// Render each tool pane at many sizes (and in empty areas), feed it keys and clicks, and collect panics.
    #[test]
    fn qa_every_tool_at_every_size() {
        let _g = CENTER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let themes_dir = scratch("sizes-themes");
        crate::theme::TEST_DIR.with(|t| *t.borrow_mut() = Some(themes_dir));
        let notes_dir = scratch("sizes-notes");
        let files_dir = scratch("sizes-files");
        std::fs::write(files_dir.join("readme.md"), "# hi\n").unwrap();
        std::fs::create_dir_all(files_dir.join("sub")).unwrap();
        let cal = scratch("sizes-cal").join("calendar.json");
        let mut k = Kit::new();
        let mut panes: Vec<(&str, Box<dyn Pane>)> = vec![
            ("calendar", Box::new(crate::panes::calendar::Calendar::open_at(cal))),
            ("notes", Box::new(crate::panes::notes::Notes::open_in(notes_dir, None))),
            ("files", Box::new(crate::panes::files::Files::new(Some(files_dir)))),
            ("help", Box::new(crate::panes::help::Help::new())),
            ("themes", Box::new(crate::panes::themes::Themes::new())),
            ("alerts", Box::new(crate::panes::alerts::Alerts::new())),
            ("updates", Box::new(crate::panes::updates::Updates::new())),
        ];
        let sizes: &[(u16, u16)] = &[(1, 1), (2, 1), (1, 2), (3, 3), (5, 2), (8, 4), (10, 5), (19, 6), (20, 8), (27, 9), (33, 10), (49, 12), (50, 12), (79, 24), (99, 30), (100, 30), (111, 33), (112, 33), (150, 44), (300, 90)];
        let mut failures = vec![];
        for (name, p) in panes.iter_mut() {
            for &(w, h) in sizes {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    k.render(p.as_mut(), w, h);
                    k.render_side(p.as_mut(), w.min(34), h);
                    // zero-width / zero-height areas inside a real screen
                    render_at(&mut k, p.as_mut(), w.max(2), h.max(2), Rect::new(1, 1, 0, h.max(2) - 1));
                    render_at(&mut k, p.as_mut(), w.max(2), h.max(2), Rect::new(1, 1, w.max(2) - 1, 0));
                    for code in [KeyCode::Down, KeyCode::Char('j'), KeyCode::End, KeyCode::PageDown, KeyCode::Up, KeyCode::Home] {
                        k.key(p.as_mut(), code);
                    }
                    k.render(p.as_mut(), w, h);
                    let area = Rect::new(0, 0, w, h);
                    for (x, y) in [(0, 0), (w / 2, h / 2), (w.saturating_sub(1), h.saturating_sub(1))] {
                        k.mouse(p.as_mut(), click(x, y), area);
                        k.mouse(p.as_mut(), wheel(true, x, y), area);
                    }
                    k.render(p.as_mut(), w, h);
                }));
                if let Err(e) = r {
                    let msg = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                    failures.push(format!("{name} at {w}x{h}: {msg}"));
                }
            }
        }
        crate::theme::TEST_DIR.with(|t| *t.borrow_mut() = None);
        assert!(failures.is_empty(), "panics:\n{}", failures.join("\n"));
    }
}
