//! QA key-mash for the music app (see src/qa_keys.rs). A fixed fake library, settings never written, and nothing
//! ever starts playing: the keys and clicks that would queue a song (enter, a double-click on a row) are held back,
//! so the audio device is never opened. A "now playing" track is set straight in the engine's state instead, with
//! its cover and lyrics marked as already looked up (lyrics would otherwise be fetched from lrclib.net).

use super::*;
use crate::qa_keys::{Ev, Opts, mash_pane};

/// A Music with `n` fake tracks (paths that don't exist) and three playlists; `playing` sets a current track.
pub(crate) fn fake(n: usize, playing: bool) -> Music {
    let state = Arc::new(Mutex::new(State::default()));
    let waker = Arc::new(Mutex::new(None));
    let engine = Engine::new(state.clone(), waker.clone(), false); // false: never writes music.json
    let names = ["Song", "中文歌 🙂", "a very long title that goes on and on and on (feat. someone) [Remastered 2011]", "", "é", "\u{200b}"];
    let tracks: Vec<Track> = (0..n)
        .map(|i| Track {
            id: format!("t{i}"),
            path: format!("/nope/{i}.mp3").into(),
            title: format!("{} {i}", names[i % names.len()]),
            artist: if i % 7 == 0 { String::new() } else { format!("Artist {}", i % 13) },
            album: if i % 3 == 0 { "Album ✦".into() } else { String::new() },
            duration: if i % 5 == 0 { 0.0 } else { 30.0 + (i * 37 % 600) as f64 },
            plays: (i % 4) as u64,
        })
        .collect();
    let ids: Vec<String> = (0..n).step_by(3).map(|i| format!("t{i}")).collect();
    let playlists = vec![
        library::Playlist { id: "p1".into(), name: "Vibe Coding".into(), ids },
        library::Playlist { id: "p2".into(), name: "empty one".into(), ids: vec![] },
        library::Playlist { id: "p3".into(), name: "ghosts 👻".into(), ids: vec!["gone1".into(), "gone2".into()] },
    ];
    let load = Arc::new(Mutex::new(Load { lib: Some(Lib { tracks: tracks.clone(), playlists, source: "qa".into() }), status: String::new(), done: true, version: 1, run: 0 }));
    let mut m = Music::build(state, waker, engine, load);
    if playing && !tracks.is_empty() {
        let t = tracks[n / 2].clone();
        {
            // current track, but an empty queue: toggle / next / prev then have nothing to start
            let mut s = m.engine.shared.lock().unwrap();
            s.track = Some(t.clone());
            s.playing = true;
            s.pos = 42.0;
            s.dur = t.duration;
            s.changed += 1;
        }
        {
            let mut sc = m.engine.scope.lock().unwrap();
            sc.buf = (0..4096).map(|i| ((i as f32) * 0.07).sin() * 0.5).collect();
        }
        m.cover_for = t.id.clone();
        m.cover_done = true;
        m.lyrics_for = t.id.clone();
        m.lyrics = LyricState::Have((0..40).map(|i| (i as f64 * 5.0, format!("lyric line {i} 🎵 {}", "la ".repeat(i % 12)))).collect());
    }
    m
}

/// Hold back whatever would queue a song (and so open the audio device, fetch lyrics, count plays).
fn guard(p: &mut Music, ev: &mut Ev) -> bool {
    match ev {
        Ev::Key(k) => {
            if k.modifiers.intersects(KeyModifiers::ALT) {
                return true;
            }
            let searching = p.searching;
            let no_track = p.engine.shared.lock().unwrap().track.is_none();
            match k.code {
                KeyCode::Enter => searching,
                // space with nothing loaded plays the selection
                KeyCode::Char(' ') => searching || !no_track,
                _ => true,
            }
        }
        Ev::Mouse(m) => {
            if let MouseEventKind::Down(MouseButton::Left) = m.kind {
                let pos = Position { x: m.column, y: m.row };
                let no_track = p.engine.shared.lock().unwrap().track.is_none();
                match p.hits.iter().find(|(r, _)| r.contains(pos)).map(|x| x.1) {
                    // a second click on the selected row plays it
                    Some(Hit::Row(n)) if n == p.sel || p.last_click.is_some_and(|c| c.0 == n) => return false,
                    Some(Hit::Toggle) if no_track => return false,
                    _ => {}
                }
            }
            true
        }
        _ => true,
    }
}

#[test]
fn qa_keys_music_now_playing() {
    mash_pane("music-playing", Opts { snap: Some("music-playing".into()), ..Default::default() }, |_| fake(60, true), guard);
}

#[test]
fn qa_keys_music_idle() {
    mash_pane("music-idle", Opts::default(), |_| fake(7, false), guard);
}

#[test]
fn qa_keys_music_empty_library() {
    mash_pane("music-empty", Opts { keys: crate::qa_keys::keys_for(1500), ..Default::default() }, |_| fake(0, false), guard);
}

#[test]
fn qa_keys_music_big_library() {
    mash_pane("music-big", Opts { keys: crate::qa_keys::keys_for(1500), ..Default::default() }, |_| fake(3000, true), guard);
}
