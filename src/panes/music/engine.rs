//! Playback: one thread owns the audio device (rodio) and the play queue, so the UI never blocks on opening a
//! device, decoding headers or seeking. The UI sends `Cmd`s and reads `Shared`; the thread wakes the UI when the
//! track changes and advances by itself at the end of a song, even when the music tab isn't on screen.

use super::library::{self, State, Track};
use crate::pane::Waker;
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Repeat {
    Off,
    One,
    All,
}

impl Repeat {
    pub fn parse(s: &str) -> Repeat {
        match s {
            "one" => Repeat::One,
            "all" => Repeat::All,
            _ => Repeat::Off,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Repeat::Off => "off",
            Repeat::One => "one",
            Repeat::All => "all",
        }
    }
    /// The order: off -> this song -> whole list -> off.
    pub fn next(self) -> Repeat {
        match self {
            Repeat::Off => Repeat::One,
            Repeat::One => Repeat::All,
            Repeat::All => Repeat::Off,
        }
    }
}

/// What the UI reads. Only the engine thread changes playback fields; the UI sets volume/shuffle/repeat
/// directly (so the next frame shows them) and sends `Cmd::Persist`.
pub struct Shared {
    pub queue: Vec<Track>,
    pub index: usize,
    pub track: Option<Track>,
    pub playing: bool,
    pub pos: f64,
    pub dur: f64,
    pub volume: f32,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub error: Option<String>,
    /// Bumps whenever the current track changes (or restarts, or its play is counted), so the UI knows to
    /// refresh cover/lyrics/marks and "most played".
    pub changed: u64,
}

/// The last ~2k mono samples that went to the speakers, for the spectrum. Kept apart from `Shared` so the audio
/// callback never contends with the UI for the big lock.
pub struct Scope {
    pub buf: Vec<f32>,
    pub rate: u32,
}

pub enum Cmd {
    /// Play `tracks` starting at index (shuffled if shuffle is on).
    PlayList(Vec<Track>, usize),
    Toggle,
    Next,
    Prev,
    /// Absolute seconds.
    Seek(f64),
    /// Relative seconds.
    Nudge(f64),
    /// Apply volume and shuffle from `Shared` and save settings.
    Persist,
    #[cfg_attr(not(test), allow(dead_code))]
    Stop,
}

pub struct Engine {
    tx: Sender<Cmd>,
    pub shared: Arc<Mutex<Shared>>,
    pub scope: Arc<Mutex<Scope>>,
}

impl Engine {
    /// `persist` = write play counts and settings to music.json (off in tests).
    pub fn new(state: Arc<Mutex<State>>, waker: Arc<Mutex<Option<Waker>>>, persist: bool) -> Engine {
        let (volume, shuffle, repeat) = {
            let s = state.lock().unwrap();
            (s.volume.clamp(0.0, 1.0), s.shuffle, Repeat::parse(&s.repeat))
        };
        let shared = Arc::new(Mutex::new(Shared {
            queue: vec![],
            index: 0,
            track: None,
            playing: false,
            pos: 0.0,
            dur: 0.0,
            volume,
            shuffle,
            repeat,
            error: None,
            changed: 0,
        }));
        let scope = Arc::new(Mutex::new(Scope { buf: vec![], rate: 44100 }));
        let (tx, rx) = channel();
        let mut inner = Inner {
            rx,
            shared: shared.clone(),
            scope: scope.clone(),
            state,
            waker,
            persist,
            sink: None,
            player: None,
            orig: vec![],
            rng: seed(),
            pending: None,
        };
        std::thread::Builder::new().name("oriel-music".into()).spawn(move || inner.run()).ok();
        Engine { tx, shared, scope }
    }

    pub fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
    }
}

struct Inner {
    rx: Receiver<Cmd>,
    shared: Arc<Mutex<Shared>>,
    scope: Arc<Mutex<Scope>>,
    state: Arc<Mutex<State>>,
    waker: Arc<Mutex<Option<Waker>>>,
    persist: bool,
    sink: Option<MixerDeviceSink>,
    player: Option<Player>,
    /// The queue in its original order, to restore when shuffle is turned off.
    orig: Vec<Track>,
    rng: u64,
    /// The song that started but hasn't played long enough to count yet (see `count_play`).
    pending: Option<String>,
}

impl Inner {
    fn run(&mut self) {
        loop {
            match self.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(c) => self.handle(c),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break, // the pane closed: dropping the sink stops audio
            }
            // position + end of track
            let ended = match &self.player {
                Some(p) => {
                    let mut s = self.shared.lock().unwrap();
                    s.pos = p.get_pos().as_secs_f64();
                    s.playing && p.empty()
                }
                None => false,
            };
            if ended {
                self.ended();
            }
            self.count_play();
        }
    }

    /// A song counts as played once it has played 30 seconds or half its length, whichever is less (not when it
    /// starts: skipping through a list mustn't make "most played" the songs you skipped).
    fn count_play(&mut self) {
        let Some(id) = &self.pending else { return };
        let (pos, dur, current) = {
            let s = self.shared.lock().unwrap();
            (s.pos, s.dur, s.track.as_ref().is_some_and(|t| &t.id == id))
        };
        if !current {
            self.pending = None;
            return;
        }
        let need = if dur > 0.0 { (dur / 2.0).min(30.0) } else { 30.0 };
        if pos < need {
            return;
        }
        let id = self.pending.take().unwrap_or_default();
        let mut st = self.state.lock().unwrap();
        *st.plays.entry(id.clone()).or_insert(0) += 1;
        st.last = Some(id);
        if self.persist {
            library::save_state(&st);
        }
        drop(st);
        self.shared.lock().unwrap().changed += 1;
        self.wake();
    }

    fn wake(&self) {
        if let Some(w) = self.waker.lock().unwrap().as_ref() {
            w.wake();
        }
    }

    fn handle(&mut self, c: Cmd) {
        match c {
            Cmd::PlayList(tracks, i) => {
                if tracks.is_empty() {
                    return;
                }
                let i = i.min(tracks.len() - 1);
                self.orig = tracks.clone();
                let shuffle = self.shared.lock().unwrap().shuffle;
                let (queue, start) = if shuffle { self.shuffled(&tracks, i) } else { (tracks, i) };
                let n = queue.len();
                self.shared.lock().unwrap().queue = queue;
                self.start(start, true, 1, n);
            }
            Cmd::Toggle => {
                let (playing, index, has_track) = {
                    let s = self.shared.lock().unwrap();
                    (s.playing, s.index, s.track.is_some())
                };
                if !has_track {
                    return;
                }
                match &self.player {
                    // finished or failed: play it again from the top
                    None => self.start(index, false, 1, 1),
                    Some(p) if p.empty() => self.start(index, false, 1, 1),
                    Some(p) => {
                        if playing { p.pause() } else { p.play() }
                        self.shared.lock().unwrap().playing = !playing;
                    }
                }
                self.wake();
            }
            Cmd::Next => {
                let (n, i) = {
                    let s = self.shared.lock().unwrap();
                    (s.queue.len(), s.index)
                };
                if n > 0 {
                    self.start((i + 1) % n, true, 1, n);
                }
            }
            Cmd::Prev => {
                let (n, i, pos) = {
                    let s = self.shared.lock().unwrap();
                    (s.queue.len(), s.index, s.pos)
                };
                if n == 0 {
                    return;
                }
                if pos > 3.0 {
                    self.seek(0.0);
                } else {
                    self.start((i + n - 1) % n, true, -1, n);
                }
            }
            Cmd::Seek(t) => self.seek(t),
            Cmd::Nudge(d) => {
                let pos = self.shared.lock().unwrap().pos;
                self.seek(pos + d);
            }
            Cmd::Persist => {
                let (vol, shuffle, repeat) = {
                    let s = self.shared.lock().unwrap();
                    (s.volume, s.shuffle, s.repeat)
                };
                if let Some(p) = &self.player {
                    p.set_volume(curve(vol));
                }
                self.apply_shuffle(shuffle);
                let mut st = self.state.lock().unwrap();
                st.volume = (vol * 1000.0).round() / 1000.0;
                st.shuffle = shuffle;
                st.repeat = repeat.name().into();
                if self.persist {
                    library::save_state(&st);
                }
            }
            Cmd::Stop => {
                self.player = None;
                let mut s = self.shared.lock().unwrap();
                s.playing = false;
                s.track = None;
                s.pos = 0.0;
                s.changed += 1;
                drop(s);
                self.wake();
            }
        }
    }

    /// Shuffle turned on mid-queue: shuffle what's left around the current song. Off: back to the original order.
    fn apply_shuffle(&mut self, on: bool) {
        let mut s = self.shared.lock().unwrap();
        let Some(cur) = s.track.clone() else { return };
        let is_shuffled = s.queue.len() == self.orig.len() && s.queue != self.orig;
        if on && !is_shuffled && s.queue.len() > 1 {
            let i = s.index;
            drop(s);
            let (q, idx) = self.shuffled(&self.orig.clone(), i);
            let mut s = self.shared.lock().unwrap();
            s.queue = q;
            s.index = idx;
        } else if !on && is_shuffled {
            s.queue = self.orig.clone();
            s.index = s.queue.iter().position(|t| t.id == cur.id).unwrap_or(0);
        }
    }

    /// `tracks` with `first` moved to the front and the rest in random order.
    fn shuffled(&mut self, tracks: &[Track], first: usize) -> (Vec<Track>, usize) {
        let mut rest: Vec<Track> = tracks.to_vec();
        let head = rest.remove(first.min(rest.len().saturating_sub(1)));
        for i in (1..rest.len()).rev() {
            let j = (self.rand() % (i as u64 + 1)) as usize;
            rest.swap(i, j);
        }
        rest.insert(0, head);
        (rest, 0)
    }

    fn rand(&mut self) -> u64 {
        // xorshift64*
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545F4914F6CDD1D)
    }

    /// Play queue[i]. A song that can't be opened or decoded is skipped: up to `tries` songs are tried, stepping
    /// `step` (+1 onward, -1 back) each time, so one bad file doesn't stop a playlist and a queue where nothing
    /// plays can't loop forever. The last error stays on screen.
    fn start(&mut self, i: usize, count: bool, step: isize, tries: usize) {
        let Some(found) = self.find(i, step, tries) else { return };
        let (track, vol) = {
            let mut s = self.shared.lock().unwrap();
            s.index = found.index;
            (found.track.clone(), s.volume)
        };
        let opened = found.opened.and_then(|src| self.ensure_sink().map(|()| src));
        match opened {
            Ok((src, dur)) => {
                self.scope.lock().unwrap().buf.clear();
                let player = Player::connect_new(self.sink.as_ref().unwrap().mixer());
                player.set_volume(curve(vol));
                player.append(Tap { inner: src, scope: self.scope.clone(), ch: 2, acc: 0.0, k: 0, local: Vec::with_capacity(512) });
                self.player = Some(player); // the old one drops here and goes quiet
                let mut s = self.shared.lock().unwrap();
                s.dur = if track.duration > 0.0 { track.duration } else { dur };
                s.track = Some(track.clone());
                s.playing = true;
                s.pos = 0.0;
                s.error = found.skipped.map(|e| format!("skipped: {e}"));
                s.changed += 1;
            }
            Err(e) => {
                self.player = None;
                let mut s = self.shared.lock().unwrap();
                s.track = Some(track.clone());
                s.playing = false;
                s.pos = 0.0;
                s.dur = track.duration;
                s.error = Some(e);
                s.changed += 1;
                drop(s);
                self.wake();
                return;
            }
        }
        // counted later, once it has really played (count_play, from the run loop)
        self.pending = if count { Some(track.id.clone()) } else { None };
        self.wake();
    }

    /// The first song from queue[i] on (stepping `step`, at most `tries` songs) that opens and decodes, else the
    /// last one tried, with its error. None = empty queue. Never touches the audio device, so a queue of bad files
    /// fails without opening it.
    fn find(&self, i: usize, step: isize, tries: usize) -> Option<Found> {
        let n = self.shared.lock().unwrap().queue.len();
        if n == 0 {
            return None;
        }
        let tries = tries.clamp(1, n);
        let mut i = i % n;
        let mut skipped = None;
        for k in 0..tries {
            if k > 0 {
                i = (i as isize + step).rem_euclid(n as isize) as usize;
            }
            let track = self.shared.lock().unwrap().queue.get(i).cloned()?;
            match open(&track) {
                Ok(src) => return Some(Found { index: i, track, opened: Ok(src), skipped }),
                Err(e) if k + 1 < tries => skipped = Some(e),
                Err(e) => return Some(Found { index: i, track, opened: Err(e), skipped: None }),
            }
        }
        None
    }

    fn ensure_sink(&mut self) -> Result<(), String> {
        if self.sink.is_none() {
            let mut sink = DeviceSinkBuilder::open_default_sink().map_err(|e| format!("audio device: {e}"))?;
            sink.log_on_drop(false);
            self.sink = Some(sink);
        }
        Ok(())
    }

    fn seek(&mut self, t: f64) {
        let (dur, has) = {
            let s = self.shared.lock().unwrap();
            (s.dur, s.track.is_some())
        };
        if !has {
            return;
        }
        if self.player.as_ref().map(|p| p.empty()).unwrap_or(true) {
            // ended or failed: restart the song, then seek into it
            let i = self.shared.lock().unwrap().index;
            self.start(i, false, 1, 1);
        }
        let Some(p) = &self.player else { return };
        let t = t.clamp(0.0, (dur - 0.5).max(0.0));
        match p.try_seek(Duration::from_secs_f64(t)) {
            Ok(()) => {
                let mut s = self.shared.lock().unwrap();
                s.pos = t;
                s.error = None;
            }
            Err(e) => self.shared.lock().unwrap().error = Some(format!("seek: {e}")),
        }
        self.scope.lock().unwrap().buf.clear();
        self.wake();
    }

    fn ended(&mut self) {
        let (repeat, i, n) = {
            let s = self.shared.lock().unwrap();
            (s.repeat, s.index, s.queue.len())
        };
        match repeat {
            Repeat::One => self.start(i, true, 1, 1),
            // a song that won't play is skipped; without repeat, only as far as the end of the list
            Repeat::All => self.start(i + 1, true, 1, n),
            _ if i + 1 < n => self.start(i + 1, true, 1, n - i - 1),
            _ => {
                let mut s = self.shared.lock().unwrap();
                s.playing = false;
                s.pos = s.dur;
                drop(s);
                self.wake();
            }
        }
    }
}

type Dec = Decoder<std::io::BufReader<std::fs::File>>;

/// What `find` came up with: the song it stopped at, and either its decoder (+ duration) or why it won't play.
struct Found {
    index: usize,
    track: Track,
    opened: Result<(Dec, f64), String>,
    /// why the last song passed over on the way was skipped
    skipped: Option<String>,
}

/// Open and decode a song's file (headers only; nothing plays).
fn open(t: &Track) -> Result<(Dec, f64), String> {
    if let Some(why) = library::unplayable(&t.path) {
        return Err(format!("can't play {}: {why}", t.title));
    }
    let file = std::fs::File::open(&t.path).map_err(|e| format!("can't open {}: {e}", t.path.display()))?;
    let dec = Decoder::try_from(file).map_err(|e| format!("can't decode {}: {e}", t.title))?;
    let dur = dec.total_duration().map(|d| d.as_secs_f64()).unwrap_or(0.0);
    Ok((dec, dur))
}

/// Perceptual-ish volume curve (volume ** 1.6).
fn curve(v: f32) -> f32 {
    v.clamp(0.0, 1.0).powf(1.6)
}

fn seed() -> u64 {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
    n | 1
}

/// Passes audio through untouched and copies a mono mix into `Scope` for the spectrum.
struct Tap<S: Source> {
    inner: S,
    scope: Arc<Mutex<Scope>>,
    ch: u16,
    acc: f32,
    k: u16,
    local: Vec<f32>,
}

impl<S: Source> Iterator for Tap<S> {
    type Item = rodio::Sample;
    #[inline]
    fn next(&mut self) -> Option<rodio::Sample> {
        let s = self.inner.next()?;
        self.acc += s;
        self.k += 1;
        if self.k >= self.ch {
            self.local.push(self.acc / self.ch as f32);
            self.acc = 0.0;
            self.k = 0;
            if self.local.len() >= 512 {
                if let Ok(mut sc) = self.scope.try_lock() {
                    sc.buf.extend_from_slice(&self.local);
                    let n = sc.buf.len();
                    if n > 2048 {
                        sc.buf.drain(..n - 2048);
                    }
                    sc.rate = self.inner.sample_rate().get();
                }
                self.local.clear();
                self.ch = self.inner.channels().get().max(1);
            }
        }
        Some(s)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<S: Source> Source for Tap<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }
    fn channels(&self) -> rodio::ChannelCount {
        self.inner.channels()
    }
    fn sample_rate(&self) -> rodio::SampleRate {
        self.inner.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
    fn try_seek(&mut self, pos: Duration) -> Result<(), rodio::source::SeekError> {
        self.k = 0;
        self.acc = 0.0;
        self.inner.try_seek(pos)
    }
}

// ---------------------------------------------------------------- spectrum
/// 0..1 levels per band (log-spaced 40 Hz – 16 kHz) of the last 1024 samples.
pub fn spectrum(samples: &[f32], rate: u32, bands: usize) -> Vec<f32> {
    const N: usize = 1024;
    if samples.len() < N {
        return vec![0.0; bands];
    }
    let x = &samples[samples.len() - N..];
    let mut re: Vec<f32> = x.iter().enumerate().map(|(i, v)| v * (0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / (N - 1) as f32).cos())).collect();
    let mut im = vec![0.0f32; N];
    fft(&mut re, &mut im);
    let bin_hz = rate as f32 / N as f32;
    let (lo, hi) = (40.0f32, 16000.0f32);
    (0..bands)
        .map(|b| {
            let f0 = lo * (hi / lo).powf(b as f32 / bands as f32);
            let f1 = lo * (hi / lo).powf((b + 1) as f32 / bands as f32);
            let (k0, k1) = ((f0 / bin_hz).ceil() as usize, ((f1 / bin_hz).ceil() as usize).min(N / 2));
            let mut m = 0.0f32;
            // a band narrower than one bin borrows the nearest bin, so the low end isn't blank
            for k in k0.min(N / 2 - 1)..k1.max(k0.min(N / 2 - 1) + 1) {
                m = m.max((re[k] * re[k] + im[k] * im[k]).sqrt());
            }
            let db = 20.0 * (m + 1e-6).log10();
            ((db + 10.0) / 50.0).clamp(0.0, 1.0)
        })
        .collect()
}

/// In-place iterative radix-2 FFT; len must be a power of two.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -std::f32::consts::TAU / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let (tr, ti) = (re[b] * cr - im[b] * ci, re[b] * ci + im[b] * cr);
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
        }
        len <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine's inside with no thread and no audio device: `find`/`start` are called directly.
    fn inner(queue: Vec<Track>) -> Inner {
        let (_tx, rx) = channel();
        let shared = Shared { queue, index: 0, track: None, playing: false, pos: 0.0, dur: 0.0, volume: 0.0, shuffle: false, repeat: Repeat::Off, error: None, changed: 0 };
        Inner {
            rx,
            shared: Arc::new(Mutex::new(shared)),
            scope: Arc::new(Mutex::new(Scope { buf: vec![], rate: 44100 })),
            state: Arc::new(Mutex::new(State::default())),
            waker: Arc::new(Mutex::new(None)),
            persist: false,
            sink: None,
            player: None,
            orig: vec![],
            rng: 1,
            pending: None,
        }
    }

    fn track(p: &std::path::Path) -> Track {
        library::bare_track(p)
    }

    /// A tiny silent WAV (never played: `find` only reads its header).
    fn wav(p: &std::path::Path) {
        let data = vec![0u8; 1600];
        let mut b = b"RIFF".to_vec();
        b.extend((36 + data.len() as u32).to_le_bytes());
        b.extend(b"WAVEfmt ");
        b.extend(16u32.to_le_bytes());
        b.extend(1u16.to_le_bytes()); // PCM
        b.extend(1u16.to_le_bytes()); // mono
        b.extend(8000u32.to_le_bytes());
        b.extend(16000u32.to_le_bytes());
        b.extend(2u16.to_le_bytes());
        b.extend(16u16.to_le_bytes());
        b.extend(b"data");
        b.extend((data.len() as u32).to_le_bytes());
        b.extend(data);
        std::fs::write(p, b).unwrap();
    }

    #[test]
    fn music_bad_songs_are_skipped_not_fatal() {
        let dir = crate::panes::files::tests::scratch("music-skip");
        std::fs::write(dir.join("broken.mp3"), b"this is not audio").unwrap();
        std::fs::write(dir.join("song.opus"), b"OggS...").unwrap();
        wav(&dir.join("good.wav"));
        let (broken, opus, missing, good) = (track(&dir.join("broken.mp3")), track(&dir.join("song.opus")), track(&dir.join("gone.mp3")), track(&dir.join("good.wav")));

        // onward from a bad song to the next one that decodes, saying what was skipped
        let e = inner(vec![broken.clone(), opus.clone(), missing.clone(), good.clone()]);
        let f = e.find(0, 1, 4).unwrap();
        assert_eq!(f.index, 3);
        assert!(f.opened.is_ok());
        assert!(f.skipped.as_deref().is_some_and(|s| s.starts_with("can't open")), "{:?}", f.skipped);
        // backwards too (prev), wrapping round
        let f = e.find(2, -1, 4).unwrap();
        assert_eq!(f.index, 3);
        // opus is refused up front: no decoder for it
        let f = e.find(1, 1, 1).unwrap();
        assert!(f.opened.as_ref().err().is_some_and(|m| m.contains("no opus decoder")));
        // a limit on tries (no repeat: only as far as the end of the list) stops at the last one tried
        let f = e.find(0, 1, 2).unwrap();
        assert_eq!(f.index, 1);
        assert!(f.opened.is_err());

        // nothing in the queue plays: every song is tried once, the error stays, and the audio device is never opened
        let mut e = inner(vec![broken.clone(), opus, missing, broken]);
        e.start(0, true, 1, 4);
        let s = e.shared.lock().unwrap();
        assert!(!s.playing && s.error.as_deref().is_some_and(|m| m.starts_with("can't decode")), "{:?}", s.error);
        assert_eq!(s.index, 3);
        drop(s);
        assert!(e.sink.is_none() && e.player.is_none());
        assert!(e.state.lock().unwrap().plays.is_empty(), "a song that never played isn't counted");
    }

    #[test]
    fn music_counts_a_play_only_after_listening() {
        let t = Track { id: "song".into(), title: "song".into(), ..Default::default() };
        let mut e = inner(vec![t.clone()]);
        let plays = |e: &Inner| e.state.lock().unwrap().plays.get("song").copied().unwrap_or(0);
        let at = |e: &mut Inner, pos: f64, dur: f64| {
            let mut s = e.shared.lock().unwrap();
            s.track = Some(t.clone());
            (s.pos, s.dur) = (pos, dur);
        };
        // just started (a skip): not a play
        e.pending = Some("song".into());
        at(&mut e, 4.0, 200.0);
        e.count_play();
        assert_eq!(plays(&e), 0);
        // 30 seconds in: counted, once
        at(&mut e, 30.5, 200.0);
        e.count_play();
        e.count_play();
        assert_eq!(plays(&e), 1);
        assert_eq!(e.state.lock().unwrap().last.as_deref(), Some("song"));
        // a short song counts at half its length
        e.pending = Some("song".into());
        at(&mut e, 12.0, 20.0);
        e.count_play();
        assert_eq!(plays(&e), 2);
        // skipped to another song before it counted: dropped
        e.pending = Some("other".into());
        at(&mut e, 100.0, 200.0);
        e.count_play();
        assert!(e.pending.is_none());
        assert_eq!(plays(&e), 2);
    }

    #[test]
    fn music_spectrum_finds_a_tone() {
        let rate = 44100;
        let s: Vec<f32> = (0..2048).map(|i| (std::f32::consts::TAU * 1000.0 * i as f32 / rate as f32).sin() * 0.5).collect();
        let lv = super::spectrum(&s, rate, 18);
        let peak = lv.iter().enumerate().max_by(|a, b| a.1.partial_cmp(b.1).unwrap()).unwrap().0;
        // 1 kHz sits in band ~ log(1000/40)/log(400) * 18 = 9.7
        assert!((8..=11).contains(&peak), "peak band {peak}: {lv:?}");
        assert!(lv[peak] > 0.7);
    }
}
