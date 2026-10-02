//! QA: the updates screen in every state, with fake releases and fake kept versions (no network, no install).
//! Kept versions are dummy files in a scratch state folder (update::TEST_STATE points update.rs at it).

use super::*;
use crate::testkit::Kit;
use crate::update::{self, Release};

fn rel(v: &str, notes: &str) -> Release {
    Release { version: v.into(), notes: notes.into(), asset: "https://example.invalid/oriel.zip".into(), ..Default::default() }
}

/// A version relative to this one, e.g. ver(0, 1, 0) = the next minor.
fn ver(dmaj: u64, dmin: u64, dpatch: u64) -> String {
    let (a, b, c) = update::parse(update::VERSION);
    format!("{}.{}.{}", a + dmaj, b + dmin, c + dpatch)
}

fn older() -> String {
    let (a, b, c) = update::parse(update::VERSION);
    if c > 0 { format!("{a}.{b}.{}", c - 1) } else if b > 0 { format!("{a}.{}.9", b - 1) } else { "0.0.1".into() }
}

fn with(rs: Vec<Release>) -> Updates {
    let mut p = Updates::new();
    p.asked = true; // don't start a check
    p.st.lock().unwrap().releases = Some(Ok(rs));
    p
}

#[test]
fn qa_updates_newer_releases_newest_first() {
    let mut k = Kit::new();
    let (two, one) = (ver(0, 2, 0), ver(0, 1, 0));
    let mut p = with(vec![
        rel(&two, "- the newest thing\n\n**Full Changelog**: https://github.com/x/y/compare/a...b"),
        rel(&one, ""),
        rel(update::VERSION, "- what you already have"),
        rel(&older(), "- ancient history"),
    ]);
    let s = k.render(&mut p, 110, 40);
    assert!(s.contains(&format!("{two} is out")) && s.contains("what's new since your version"), "{s}");
    assert!(s.contains("enter update now"), "{s}");
    assert!(s.contains("the newest thing") && s.contains("(no notes)"), "{s}");
    assert!(!s.contains("what you already have") && !s.contains("ancient history") && !s.contains("Full Changelog"), "{s}");
    assert!(s.find(&two).unwrap() < s.find(&one).unwrap(), "newest first:\n{s}");
}

#[test]
fn qa_updates_up_to_date_and_unknown_version() {
    let mut k = Kit::new();
    let mut p = with(vec![rel(update::VERSION, "- this version's notes"), rel(&older(), "- old")]);
    let s = k.render(&mut p, 110, 30);
    assert!(s.contains("you have the newest version") && s.contains("what's in this version") && s.contains("this version's notes"), "{s}");
    assert!(!s.contains("update now") && !s.contains("- old"), "{s}");
    // this version isn't in the list (a dev build, or it fell off the first page)
    let mut p = with(vec![rel(&older(), "- old")]);
    let s = k.render(&mut p, 110, 30);
    assert!(s.contains("you have the newest version") && s.contains("no notes for this one"), "{s}");
    // no releases at all
    let mut p = with(vec![]);
    assert!(k.render(&mut p, 110, 30).contains("no notes for this one"));
    // still checking
    let mut p = Updates::new();
    p.asked = true;
    p.st.lock().unwrap().busy = Some("checking GitHub for releases".into());
    let s = k.render(&mut p, 110, 30);
    assert!(s.contains("checking…") && s.contains("checking GitHub for releases…"), "{s}");
}

#[test]
fn qa_updates_enter_runs_the_update_and_reports() {
    let mut k = Kit::new();
    let next = ver(1, 0, 0);
    let mut p = with(vec![rel(&next, "- big one")]);
    k.render(&mut p, 110, 30);
    k.key(&mut p, KeyCode::Enter);
    let s = k.render(&mut p, 110, 30);
    assert!(s.contains(&format!("updating to {next}")) || s.contains("no updating in tests"), "{s}");
    k.wait_wake(&mut p, 1000);
    let s = k.render(&mut p, 110, 30);
    assert!(s.contains("✗ no updating in tests"), "the failure is shown:\n{s}");
    assert!(s.contains("enter update now"), "a failed update can be retried:\n{s}");
    // a success hides the offer and says what to do
    p.st.lock().unwrap().result = Some(Ok(next.clone()));
    let s = k.render(&mut p, 110, 30);
    assert!(s.contains(&format!("✓ {next} is installed")) && !s.contains("update now"), "{s}");
}

#[test]
fn qa_updates_rollback_asks_twice() {
    // a state folder of its own (update.rs looks there from this thread): the shared one is emptied and filled
    // by the update tests running beside this one
    let state = std::path::absolute("target/test-scratch/qa-updates-rollback").unwrap();
    let _ = std::fs::remove_dir_all(&state);
    update::TEST_STATE.with(|s| *s.borrow_mut() = Some(state.clone()));
    let dir = state.join("previous");
    std::fs::create_dir_all(&dir).unwrap();
    let ext = if cfg!(windows) { ".exe" } else { "" };
    let fakes = [format!("oriel-0.6.9{ext}"), format!("oriel-0.6.10{ext}"), format!("oriel-{}{ext}", update::VERSION)];
    for f in &fakes {
        std::fs::write(dir.join(f), b"not really oriel").unwrap();
    }
    let mut k = Kit::new();
    let mut p = with(vec![rel(update::VERSION, "- notes")]);
    let s = k.render(&mut p, 110, 30);
    assert!(s.contains("kept for rollback: 0.6.10 · 0.6.9"), "newest kept first, this version left out:\n{s}");
    assert!(s.contains("r roll back"), "{s}");
    k.key(&mut p, KeyCode::Char('r'));
    assert!(k.render(&mut p, 110, 30).contains("press r again to go back to 0.6.10"));
    // anything else calls it off
    k.key(&mut p, KeyCode::Char('j'));
    assert!(!k.render(&mut p, 110, 30).contains("press r again"));
    k.key(&mut p, KeyCode::Char('r'));
    k.key(&mut p, KeyCode::Char('r'));
    // the rollback runs on a worker thread: wait for its answer
    let t0 = std::time::Instant::now();
    while p.st.lock().unwrap().result.is_none() && t0.elapsed() < std::time::Duration::from_secs(10) {
        k.wait_wake(&mut p, 50);
    }
    let s = k.render(&mut p, 110, 30);
    assert!(s.contains("✗ no rollback in tests"), "{s}");
    update::TEST_STATE.with(|s| *s.borrow_mut() = None);
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn qa_updates_long_notes_scroll_and_sizes() {
    let mut k = Kit::new();
    let notes: String = (0..300).map(|i| format!("- change number {i} with **bold** and `code`\n")).collect();
    let mut p = with(vec![rel(&ver(0, 0, 1), &notes)]);
    for _ in 0..1000 {
        k.key(&mut p, KeyCode::Down);
    }
    let s = k.render(&mut p, 110, 30);
    assert!(s.contains("change number 299"), "scrolled to the end:\n{s}");
    for _ in 0..100 {
        k.key(&mut p, KeyCode::PageUp);
    }
    assert!(k.render(&mut p, 110, 30).contains("is out"));
    for (w, h) in [(1, 1), (4, 2), (12, 4), (30, 8), (60, 20), (200, 60)] {
        k.render(&mut p, w, h);
    }
}
