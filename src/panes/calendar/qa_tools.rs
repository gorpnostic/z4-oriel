//! QA: the calendar's midnight rollover, driven through `poll` with the pane's idea of "today" wound back a day
//! (the pane reads the wall clock itself, so the only way to cross midnight in a test is to make it think it's
//! still yesterday). Plans go to target/test-scratch/qa-tools/.

use super::*;
use crate::testkit::Kit;

fn cal(name: &str) -> (Calendar, PathBuf) {
    let d = std::path::absolute(PathBuf::from("target/test-scratch/qa-tools").join(name)).unwrap();
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let path = d.join("calendar.json");
    (Calendar::open_at(path.clone()), path)
}

/// It's "yesterday" as far as the pane knows; the next poll is the first after midnight.
fn wind_back(c: &mut Calendar) -> i64 {
    let (real, _) = now();
    c.today = real - 1;
    real
}

#[test]
fn qa_calendar_midnight_moves_the_view_only_from_today() {
    let mut k = Kit::new();
    // looking at today when midnight passes: the view follows the date, the badge counts the new day
    let (mut c, _) = cal("cal-midnight-follow");
    let real = wind_back(&mut c);
    c.sel = real - 1;
    c.plans.push(Plan { day: real - 1, at: None, text: "old day".into() });
    c.plans.push(Plan { day: real, at: None, text: "new day".into() });
    assert_eq!(c.badge().as_deref(), Some("1 today"));
    k.poll(&mut c);
    assert_eq!((c.today, c.sel), (real, real));
    let s = k.render(&mut c, 140, 40);
    assert!(s.contains("new day") && s.contains("today"), "{s}");
    // browsing another day when midnight passes: that day stays chosen
    let (mut c, _) = cal("cal-midnight-browse");
    let real = wind_back(&mut c);
    c.sel = real + 20;
    k.poll(&mut c);
    assert_eq!((c.today, c.sel), (real, real + 20));
    assert!(k.render(&mut c, 140, 40).contains("in 20 days"));
}

/// Start adding a plan to today at 23:59, finish typing after midnight: the rollover moves the chosen day under
/// the open box, and enter saves the plan on the NEW day, not the one whose heading was showing when you pressed a.
#[test]
#[ignore = "fails: poll moves `sel` to the new day while the add box is open, so the plan is saved a day later than the day you were adding to (calendar.rs:215-218, 373)"]
fn qa_calendar_midnight_while_typing_keeps_the_day() {
    let mut k = Kit::new();
    let (mut c, path) = cal("cal-midnight-typing");
    let real = wind_back(&mut c);
    c.sel = real - 1;
    k.key(&mut c, KeyCode::Char('a'));
    k.typ(&mut c, "call grandma");
    k.poll(&mut c); // midnight
    k.key(&mut c, KeyCode::Enter);
    let saved: Vec<Plan> = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved.iter().map(|p| p.day - real).collect::<Vec<_>>(), [-1], "day relative to the new today");
}

/// Same for an edit that's open over midnight: the plan being edited is found by index, so it keeps its day.
#[test]
fn qa_calendar_midnight_while_editing_changes_the_right_plan() {
    let mut k = Kit::new();
    let (mut c, path) = cal("cal-midnight-edit");
    let real = wind_back(&mut c);
    c.sel = real - 1;
    c.plans = vec![Plan { day: real - 1, at: Some(600), text: "yesterday's".into() }, Plan { day: real, at: Some(600), text: "today's".into() }];
    k.key(&mut c, KeyCode::Char('e'));
    k.typ(&mut c, " edited");
    k.poll(&mut c);
    k.key(&mut c, KeyCode::Enter);
    let saved: Vec<Plan> = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(saved.iter().any(|p| p.day == real - 1 && p.text == "yesterday's edited") && saved.iter().any(|p| p.text == "today's"), "{saved:?}");
}
