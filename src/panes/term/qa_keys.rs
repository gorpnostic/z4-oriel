//! QA for the terminal pane with private access (see src/qa_keys.rs; the key-mash itself is qa_keys_term there).

use super::*;
use crate::testkit::Kit;

/// Found by qa_keys_app_tour (QA_SEED=1): the app exited with vt100's grid.rs:683 panic on a pty reader thread,
/// then `PoisonError` at term.rs:150. This does what start_reader's thread does (lock the parser, feed it a line
/// of output) on a pane one row tall, so the crash is the same every time instead of depending on pty timing:
/// the reader thread panics inside vt100 while it holds the lock (qa_keys_term_bug_vt100_wrap_on_one_row), the
/// lock is poisoned, and the UI thread's next `parser.lock().unwrap()` panics too. oriel exits.
#[test]
#[ignore = "fails: a line that wraps in a 1-row terminal pane panics its pty reader in vt100 and poisons the parser lock; the next render panics (PoisonError, term.rs:150)"]
fn qa_keys_term_bug_one_row_pane_crashes_the_ui() {
    let prog = r"C:\Windows\System32\PING.EXE";
    if !cfg!(windows) || !std::path::Path::new(prog).exists() {
        return;
    }
    let mut k = Kit::new();
    let mut p = Term::new("inert", "term", prog, vec!["-n".into(), "2".into(), "127.0.0.1".into()], None);
    k.render(&mut p, 30, 1); // a pane one row tall: the parser is 1 x 30 now
    let parser = p.parser.clone();
    let reader = std::thread::spawn(move || {
        // start_reader, one chunk of output
        parser.lock().unwrap().process(b"Reply from 127.0.0.1: bytes=32 time<1ms TTL=128\r\n");
    });
    assert!(reader.join().is_err(), "the reader thread survived the line (vt100 fixed?)");
    k.render(&mut p, 40, 2); // the next frame: resize -> parser.lock().unwrap()
}
