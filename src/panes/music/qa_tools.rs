//! QA: the music app with a fake library and a silent engine (persist off, nothing ever queued), driven with
//! every key that must NOT start playback: navigation, views, search, shuffle / repeat / volume, and the
//! transport keys with an empty queue. The audio device is only opened by a play, which never happens here.

use super::*;
use crate::testkit::Kit;

fn track(i: usize, title: &str, artist: &str) -> Track {
    Track { id: format!("t{i}"), path: format!("/nope/{i}.mp3").into(), title: title.into(), artist: artist.into(), album: "Album".into(), duration: 60.0 + i as f64, plays: (i % 3) as u64 }
}

fn fake(tracks: Vec<Track>, playlists: Vec<library::Playlist>) -> Music {
    let state = Arc::new(Mutex::new(State::default()));
    let waker = Arc::new(Mutex::new(None));
    let engine = Engine::new(state.clone(), waker.clone(), false);
    let load = Arc::new(Mutex::new(Load { lib: Some(Lib { tracks, playlists, source: "test".into() }), status: String::new(), done: true, version: 1, run: 0 }));
    Music::build(state, waker, engine, load)
}

fn library(n: usize) -> Vec<Track> {
    let mut v: Vec<Track> = (0..n).map(|i| track(i, &format!("Song {i}"), &format!("Artist {}", i % 7))).collect();
    v.push(track(n, "Jóga", "Björk"));
    v.push(track(n + 1, "夜に駆ける", "YOASOBI"));
    v.push(track(n + 2, &"A Very Long Title That Goes On ".repeat(8), &"Someone ".repeat(10)));
    v
}

/// Nothing is playing and nothing was queued.
fn silent(p: &Music) {
    let s = p.engine.shared.lock().unwrap();
    assert!(s.track.is_none() && !s.playing && s.queue.is_empty(), "playback started: {:?}", s.track.as_ref().map(|t| &t.title));
}

/// Let the engine thread handle what was sent.
fn settle(p: &Music) {
    std::thread::sleep(Duration::from_millis(150));
    silent(p);
}

#[test]
fn qa_music_keys_that_never_play() {
    let mut k = Kit::new();
    let mut p = fake(library(50), vec![library::Playlist { id: "p1".into(), name: "Mix ✨".into(), ids: vec!["t1".into(), "t2".into(), "gone".into()] }]);
    k.render(&mut p, 150, 44);
    // navigation
    for code in [KeyCode::Char('j'), KeyCode::Down, KeyCode::PageDown, KeyCode::Char('G'), KeyCode::End, KeyCode::Char('k'), KeyCode::Up, KeyCode::PageUp, KeyCode::Char('g'), KeyCode::Home] {
        assert!(k.key(&mut p, code), "{code:?} unused");
        k.render(&mut p, 150, 44);
    }
    // transport keys with nothing queued: seek, next, previous
    for code in [KeyCode::Left, KeyCode::Right, KeyCode::Char('n'), KeyCode::Char('p')] {
        k.key(&mut p, code);
    }
    // shuffle / repeat round trips, volume clamps at both ends
    let (vol0, shuf0, rep0) = {
        let s = p.engine.shared.lock().unwrap();
        (s.volume, s.shuffle, s.repeat)
    };
    k.key(&mut p, KeyCode::Char('s'));
    assert_ne!(p.engine.shared.lock().unwrap().shuffle, shuf0);
    k.key(&mut p, KeyCode::Char('s'));
    for _ in 0..3 {
        k.key(&mut p, KeyCode::Char('r'));
    }
    assert!(p.engine.shared.lock().unwrap().repeat == rep0, "three presses cycle off → all → one → off");
    for _ in 0..40 {
        k.key(&mut p, KeyCode::Char('+'));
    }
    assert_eq!(p.engine.shared.lock().unwrap().volume, 1.0);
    for _ in 0..40 {
        k.key(&mut p, KeyCode::Char('_'));
    }
    assert_eq!(p.engine.shared.lock().unwrap().volume, 0.0);
    k.key(&mut p, KeyCode::Char('='));
    assert!((p.engine.shared.lock().unwrap().volume - 0.05).abs() < 1e-6);
    let _ = vol0;
    // views: library → most played → the playlist → back
    k.key(&mut p, KeyCode::Tab);
    assert!(p.title().contains("most played"));
    assert!(p.rows.iter().all(|&i| p.lib.tracks[i].plays > 0));
    k.key(&mut p, KeyCode::Tab);
    assert_eq!(p.title(), "music · Mix ✨");
    assert_eq!(p.rows.len(), 2, "a missing id in a playlist is skipped");
    k.key(&mut p, KeyCode::Tab);
    assert!(p.title().ends_with("library"));
    k.key(&mut p, KeyCode::BackTab);
    assert_eq!(p.title(), "music · Mix ✨");
    // modifiers: ctrl / alt keys are left for the app
    assert!(!k.key_mod(&mut p, KeyCode::Char('j'), KeyModifiers::CONTROL));
    assert!(!k.key_mod(&mut p, KeyCode::Char('j'), KeyModifiers::ALT));
    assert!(!k.key(&mut p, KeyCode::F(5)));
    assert!(p.badge().is_none());
    settle(&p);
}

#[test]
fn qa_music_search_edge_cases() {
    let mut k = Kit::new();
    let mut p = fake(library(30), vec![]);
    k.render(&mut p, 150, 44); // the first render picks up the library
    k.key(&mut p, KeyCode::Char('/'));
    k.typ(&mut p, "björk");
    assert_eq!(p.rows.len(), 1);
    for _ in 0.."björk".chars().count() {
        k.key(&mut p, KeyCode::Backspace);
    }
    k.typ(&mut p, "BJÖRK");
    assert_eq!(p.rows.len(), 1, "unicode search ignores case");
    k.key_mod(&mut p, KeyCode::Char('u'), KeyModifiers::CONTROL);
    assert!(p.query.is_empty() && p.rows.len() == 33);
    k.typ(&mut p, "夜に");
    assert_eq!(p.rows.len(), 1);
    k.key_mod(&mut p, KeyCode::Char('u'), KeyModifiers::CONTROL);
    // nothing matches: the table says so, ↑↓ and enter are safe
    k.typ(&mut p, "zzzz nothing");
    assert!(p.rows.is_empty());
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains("nothing matches") && s.contains("esc clears the search"), "{s}");
    assert_eq!(p.subtitle().as_deref(), Some("0 songs matching “zzzz nothing”"));
    k.key(&mut p, KeyCode::Down);
    k.key(&mut p, KeyCode::Up);
    k.key(&mut p, KeyCode::Enter); // closes the box
    assert!(!p.searching);
    for code in [KeyCode::Char('j'), KeyCode::Char('G'), KeyCode::PageDown, KeyCode::Char('g')] {
        k.key(&mut p, code);
    }
    // a query longer than the box shows its end
    k.key(&mut p, KeyCode::Char('/'));
    k.typ(&mut p, &"x".repeat(300));
    k.typ(&mut p, "END");
    let s = k.render(&mut p, 120, 30);
    assert!(s.contains("xxxEND"), "{s}");
    // tab / F-keys while searching go to the app
    assert!(!k.key(&mut p, KeyCode::Tab));
    assert!(!k.key(&mut p, KeyCode::F(2)));
    k.key(&mut p, KeyCode::Esc);
    assert!(p.query.is_empty() && !p.searching && p.rows.len() == 33);
    settle(&p);
}

#[test]
fn qa_music_empty_library_and_empty_playlist() {
    let mut k = Kit::new();
    let mut p = fake(vec![], vec![library::Playlist { id: "e".into(), name: "Empty".into(), ids: vec!["nope".into()] }]);
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains("no songs found in test") && s.contains("add folders under [music] in config.toml"), "{s}");
    assert_eq!(p.subtitle().as_deref(), Some("0 songs"));
    for code in [KeyCode::Char('j'), KeyCode::Char('k'), KeyCode::Char('G'), KeyCode::PageDown, KeyCode::PageUp, KeyCode::Left, KeyCode::Char('n'), KeyCode::Char('p')] {
        k.key(&mut p, code);
    }
    k.key(&mut p, KeyCode::Tab); // most played
    assert!(k.render(&mut p, 150, 44).contains("no songs found"));
    k.key(&mut p, KeyCode::Tab); // the playlist, whose only song is missing
    assert!(p.rows.is_empty());
    let side = k.render_side(&mut p, 34, 12);
    assert!(side.contains("Empty") && side.contains("0"), "{side}");
    let area = Rect::new(0, 0, 150, 44);
    for _ in 0..10 {
        k.mouse(&mut p, MouseEvent { kind: MouseEventKind::ScrollDown, column: 80, row: 20, modifiers: KeyModifiers::NONE }, area);
        k.mouse(&mut p, MouseEvent { kind: MouseEventKind::ScrollUp, column: 80, row: 20, modifiers: KeyModifiers::NONE }, area);
    }
    k.render(&mut p, 150, 44);
    settle(&p);
}

#[test]
fn qa_music_every_size_and_the_wheel() {
    let mut k = Kit::new();
    let mut p = fake(library(1000), vec![]);
    let area = Rect::new(0, 0, 150, 44);
    for &(w, h) in &[(1u16, 1u16), (2, 2), (8, 3), (20, 5), (37, 10), (60, 15), (99, 16), (100, 15), (100, 16), (150, 44), (300, 90)] {
        k.render(&mut p, w, h);
        k.render_side(&mut p, w.min(34), h);
        k.key(&mut p, KeyCode::Char('G'));
        k.render(&mut p, w, h);
        k.key(&mut p, KeyCode::Char('g'));
    }
    // wheel over the table scrolls and drags the cursor along; far past the end is clamped
    k.render(&mut p, 150, 44);
    for _ in 0..500 {
        k.mouse(&mut p, MouseEvent { kind: MouseEventKind::ScrollDown, column: 100, row: 25, modifiers: KeyModifiers::NONE }, area);
    }
    let s = k.render(&mut p, 150, 44);
    assert!(p.sel < p.rows.len() && s.contains("A Very Long Title"), "the end of the list:\n{s}");
    for _ in 0..500 {
        k.mouse(&mut p, MouseEvent { kind: MouseEventKind::ScrollUp, column: 100, row: 25, modifiers: KeyModifiers::NONE }, area);
    }
    assert!(k.render(&mut p, 150, 44).contains("Song 0"));
    // one click on a row only selects it; a click on the search box opens the search
    let (r, _) = p.hits.iter().copied().find(|(_, h)| *h == Hit::Row(3)).unwrap();
    k.mouse(&mut p, MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: r.x + 3, row: r.y, modifiers: KeyModifiers::NONE }, area);
    assert_eq!(p.sel, 3);
    let (r, _) = p.hits.iter().copied().find(|(_, h)| *h == Hit::Search).unwrap();
    k.mouse(&mut p, MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: r.x + 2, row: r.y + 1, modifiers: KeyModifiers::NONE }, area);
    assert!(p.searching);
    settle(&p);
}
