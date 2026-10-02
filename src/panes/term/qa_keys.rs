//! QA for the terminal pane with private access (see src/qa_keys.rs; the key-mash itself is qa_keys_term there).

use super::*;
use crate::testkit::Kit;

/// Found by qa_keys_app_tour (QA_SEED=1): the app exited with vt100's grid.rs:683 panic on a pty reader thread,
/// then `PoisonError` at term.rs:150. vt100 panics when a line wraps on a screen one row tall
/// (qa_keys_term_bug_vt100_wrap_on_one_row); the reader thread died holding the parser lock, and the UI thread's
/// next `parser.lock().unwrap()` panicked too. Term now keeps the screen MIN_ROWS tall whatever the pane's height
/// and shows the bottom of it, so this does what start_reader's thread does (lock the parser, feed it a line of
/// output) on a pane one row tall and expects the reader to live, the pane to show the cursor's row, and the
/// next frames to draw. The pty's own reader is never started, so nothing else writes to the screen meanwhile.
#[test]
fn qa_keys_term_bug_one_row_pane_crashes_the_ui() {
    let prog = r"C:\Windows\System32\PING.EXE";
    if !cfg!(windows) || !std::path::Path::new(prog).exists() {
        return;
    }
    let mut k = Kit::new();
    let mut p = Term::new("inert", "term", prog, vec!["-n".into(), "2".into(), "127.0.0.1".into()], None);
    p.started = true; // no pty reader: the thread below stands in for it, so the screen holds only its output
    k.render(&mut p, 30, 1); // a pane one row tall
    assert_eq!(p.parser.lock().unwrap().screen().size(), (MIN_ROWS, 30), "the screen stays MIN_ROWS tall");
    let parser = p.parser.clone();
    let reader = std::thread::spawn(move || {
        // start_reader, one chunk of output: a line that wraps at 30 columns, then a prompt
        parser.lock().unwrap().process(b"Reply from 127.0.0.1: bytes=32 time<1ms TTL=128\r\nC:\\work> ");
    });
    assert!(reader.join().is_ok(), "the reader thread panicked on a line that wraps in a 1-row pane");
    let s = k.render(&mut p, 30, 1);
    assert_eq!(s.trim_end(), r"C:\work>", "a 1-row pane shows the cursor's row (the bottom of the screen)");
    let s = k.render(&mut p, 40, 2); // the next frame: resize -> parser.lock()
    assert!(s.contains("TTL=128") && s.contains(r"C:\work>"), "{s}");
    k.render(&mut p, 30, 1);
    k.render(&mut p, 80, 24);
}
