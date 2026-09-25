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
    /// nest's order: off -> this song -> whole list -> off.
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
    /// Bumps whenever the current track changes (or restarts), so the UI knows to refresh cover/lyrics/marks.
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
        }
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
                self.shared.lock().unwrap().queue = queue;
                self.start(start, true);
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
                    None => self.start(index, false),
                    Some(p) if p.empty() => self.start(index, false),
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
                    self.start((i + 1) % n, true);
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
                    self.start((i + n - 1) % n, true);
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

    fn start(&mut self, i: usize, count: bool) {
        let (track, vol) = {
            let mut s = self.shared.lock().unwrap();
            if s.queue.is_empty() {
                return;
            }
            let i = i % s.queue.len();
            s.index = i;
            (s.queue[i].clone(), s.volume)
        };
        match self.open(&track) {
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
                s.error = None;
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
        if count {
            let mut st = self.state.lock().unwrap();
            *st.plays.entry(track.id.clone()).or_insert(0) += 1;
            st.last = Some(track.id.clone());
            if self.persist {
                library::save_state(&st);
            }
        }
        self.wake();
    }

    fn open(&mut self, t: &Track) -> Result<(Decoder<std::io::BufReader<std::fs::File>>, f64), String> {
        if self.sink.is_none() {
            let mut sink = DeviceSinkBuilder::open_default_sink().map_err(|e| format!("audio device: {e}"))?;
            sink.log_on_drop(false);
            self.sink = Some(sink);
        }
        let file = std::fs::File::open(&t.path).map_err(|e| format!("can't open {}: {e}", t.path.display()))?;
        let dec = Decoder::try_from(file).map_err(|e| format!("can't decode {}: {e}", t.title))?;
        let dur = dec.total_duration().map(|d| d.as_secs_f64()).unwrap_or(0.0);
        Ok((dec, dur))
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
            self.start(i, false);
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
            Repeat::One => self.start(i, true),
            _ if i + 1 < n || repeat == Repeat::All => self.start(i + 1, true),
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

/// Perceptual-ish volume curve (nest used volume ** 1.6).
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
/// 0..1 levels per band (log-spaced 40 Hz – 16 kHz) of the last 1024 samples, nest's scaling.
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
