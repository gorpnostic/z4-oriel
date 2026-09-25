//! Where the songs come from, and what oriel remembers about them.
//!
//! * Windows with the audio-player app: `%APPDATA%\audio-player\library.json` (+ its playlists, covers\ and
//!   lyrics\). That app owns the file and rewrites it, so oriel only ever *reads* it — exactly like nest did.
//! * Anywhere else: scan `[music] folders` from the config, or the OS music folder, for audio files.
//!
//! oriel's own play counts, volume, shuffle and repeat live in `data_dir()/music.json`; lyrics it fetches from
//! lrclib.net are cached in `data_dir()/lyrics/`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const AUDIO_EXT: &[&str] = &["mp3", "flac", "ogg", "wav", "m4a", "opus", "aac", "oga"];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Track {
    pub id: String,
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Seconds; 0 = unknown until it plays.
    pub duration: f64,
    /// Plays the library itself recorded (audio-player's count); oriel's own are added on top.
    pub plays: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub ids: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Lib {
    pub tracks: Vec<Track>,
    pub playlists: Vec<Playlist>,
    /// "audio-player" or the folders that were scanned, for the empty-state message.
    pub source: String,
}

// ---------------------------------------------------------------- the audio-player app's library
/// `%APPDATA%\audio-player`, if the app is installed (Windows only).
pub fn app_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let d = PathBuf::from(std::env::var_os("APPDATA")?).join("audio-player");
        if d.join("library.json").is_file() {
            return Some(d);
        }
    }
    None
}

#[derive(Deserialize)]
struct AppLib {
    #[serde(default)]
    tracks: Vec<AppTrack>,
    #[serde(default)]
    playlists: Vec<AppPlaylist>,
}
#[derive(Deserialize)]
struct AppTrack {
    #[serde(default)]
    id: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    album: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    plays: Option<u64>,
}
#[derive(Deserialize)]
struct AppPlaylist {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default, rename = "trackIds")]
    track_ids: Vec<String>,
}

/// Read library.json. Read-only: this never writes anything into the app's folder.
pub fn load_app(dir: &Path) -> Result<Lib, String> {
    let file = dir.join("library.json");
    let text = std::fs::read_to_string(&file).map_err(|e| format!("couldn't read {} ({e})", file.display()))?;
    let data: AppLib = serde_json::from_str(&text).map_err(|e| format!("couldn't parse {} ({e})", file.display()))?;
    let tracks: Vec<Track> = data
        .tracks
        .into_iter()
        .filter(|t| !t.id.is_empty() && Path::new(&t.path).exists())
        .map(|t| Track {
            title: t.title.filter(|s| !s.is_empty()).unwrap_or_else(|| stem(Path::new(&t.path))),
            id: t.id,
            path: PathBuf::from(t.path),
            artist: t.artist.unwrap_or_default(),
            album: t.album.unwrap_or_default(),
            duration: t.duration.unwrap_or(0.0),
            plays: t.plays.unwrap_or(0),
        })
        .collect();
    let playlists = data
        .playlists
        .into_iter()
        .filter(|p| !p.track_ids.is_empty())
        .map(|p| Playlist { id: p.id, name: p.name, ids: p.track_ids })
        .collect();
    Ok(Lib { tracks, playlists, source: "audio-player".into() })
}

// ---------------------------------------------------------------- folder scan
/// The folders to scan: the config's, else the OS music folder.
pub fn scan_roots(cfg_folders: &[String]) -> Vec<PathBuf> {
    let v: Vec<PathBuf> = cfg_folders.iter().filter(|s| !s.trim().is_empty()).map(|s| expand(s.trim())).collect();
    if !v.is_empty() {
        return v;
    }
    dirs::audio_dir().into_iter().collect()
}

fn expand(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~") {
        if let Some(h) = dirs::home_dir() {
            return h.join(rest.trim_start_matches(['/', '\\']));
        }
    }
    PathBuf::from(s)
}

/// Every audio file under `roots` (hidden folders skipped), sorted by path.
pub fn find_audio(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut stack: Vec<(PathBuf, u32)> = roots.iter().map(|r| (r.clone(), 0)).collect();
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if depth < 12 {
                    stack.push((p, depth + 1));
                }
            } else if is_audio(&p) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

pub fn is_audio(p: &Path) -> bool {
    p.extension().map(|e| AUDIO_EXT.contains(&e.to_string_lossy().to_lowercase().as_str())).unwrap_or(false)
}

pub fn stem(p: &Path) -> String {
    p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
}

/// A track for a file, with just its name (tags come later from `read_tags`).
pub fn bare_track(p: &Path) -> Track {
    Track { id: p.to_string_lossy().to_string(), path: p.to_path_buf(), title: stem(p), ..Default::default() }
}

/// Title / artist / album / duration from the file's tags (ID3, Vorbis comments, MP4 atoms...) via symphonia.
pub fn read_tags(t: &mut Track) {
    use symphonia::core::meta::StandardTagKey as K;
    let Some(mut probed) = probe(&t.path) else { return };
    let mut apply = |tags: &[symphonia::core::meta::Tag]| {
        for tag in tags {
            let v = tag.value.to_string();
            if v.trim().is_empty() {
                continue;
            }
            match tag.std_key {
                Some(K::TrackTitle) => t.title = v.trim().to_string(),
                Some(K::Artist) => t.artist = v.trim().to_string(),
                Some(K::AlbumArtist) if t.artist.is_empty() => t.artist = v.trim().to_string(),
                Some(K::Album) => t.album = v.trim().to_string(),
                _ => {}
            }
        }
    };
    if let Some(m) = probed.metadata.get() {
        if let Some(rev) = m.current() {
            apply(rev.tags());
        }
    }
    if let Some(rev) = probed.format.metadata().current() {
        apply(rev.tags());
    }
    if t.duration <= 0.0 {
        if let Some(tr) = probed.format.default_track() {
            let p = &tr.codec_params;
            if let (Some(n), Some(tb)) = (p.n_frames, p.time_base) {
                let time = tb.calc_time(n);
                t.duration = time.seconds as f64 + time.frac;
            } else if let (Some(n), Some(sr)) = (p.n_frames, p.sample_rate) {
                t.duration = n as f64 / sr as f64;
            }
        }
    }
}

pub fn probe(path: &Path) -> Option<symphonia::core::probe::ProbeResult> {
    use symphonia::core::{formats::FormatOptions, io::MediaSourceStream, meta::MetadataOptions, probe::Hint};
    let file = std::fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(e) = path.extension() {
        hint.with_extension(&e.to_string_lossy());
    }
    let probed = symphonia::default::get_probe().format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default()).ok()?;
    Some(probed)
}

// ---------------------------------------------------------------- oriel's own state
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct State {
    pub plays: HashMap<String, u64>,
    pub volume: f32,
    pub shuffle: bool,
    /// "off" | "all" | "one"
    pub repeat: String,
    pub last: Option<String>,
}

impl Default for State {
    fn default() -> Self {
        State { plays: HashMap::new(), volume: 0.7, shuffle: false, repeat: "off".into(), last: None }
    }
}

pub fn state_path() -> PathBuf {
    crate::config::data_dir().join("music.json")
}

pub fn load_state() -> State {
    std::fs::read_to_string(state_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Atomic write (tmp + rename) so a crash never leaves half a file.
pub fn save_state(s: &State) {
    let p = state_path();
    let tmp = p.with_extension("json.tmp");
    if let Ok(text) = serde_json::to_string_pretty(s) {
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

// ---------------------------------------------------------------- lyrics
pub type Lyrics = Vec<(f64, String)>;

/// A file-name-safe key for a track id (library ids are UUIDs; scanned tracks use their path).
pub fn file_key(id: &str) -> String {
    if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return id.to_string();
    }
    let mut h: u64 = 0xcbf29ce484222325; // FNV-1a
    for b in id.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn read_lrc_file(p: &Path) -> Option<Lyrics> {
    let text = std::fs::read_to_string(p).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(parse_lrc(v.get("synced").and_then(|s| s.as_str()).unwrap_or("")))
}

/// Synced lyrics: the audio-player app's cache, oriel's cache, then lrclib.net (cached afterwards, even a miss).
/// Some(empty) = looked, there are none. None = couldn't reach lrclib (try again next time). Blocking: call it
/// from a background thread.
pub fn lyrics(t: &Track, fetch: bool) -> Option<Lyrics> {
    let key = file_key(&t.id);
    let own = crate::config::data_dir().join("lyrics");
    let mut dirs_: Vec<PathBuf> = app_dir().map(|d| d.join("lyrics")).into_iter().collect();
    dirs_.push(own.clone());
    for d in &dirs_ {
        let p = d.join(format!("{key}.json"));
        if p.is_file() {
            if let Some(l) = read_lrc_file(&p) {
                return Some(l);
            }
        }
    }
    // a .lrc next to the file (common for local collections)
    let side = t.path.with_extension("lrc");
    if side.is_file() {
        if let Ok(s) = std::fs::read_to_string(&side) {
            return Some(parse_lrc(&s));
        }
    }
    if !fetch {
        return None;
    }
    let (title, artist) = clean_title(t);
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(8))).build().into();
    let body = agent
        .get("https://lrclib.net/api/search")
        .query("track_name", &title)
        .query("artist_name", &artist)
        .header("User-Agent", "oriel-music (https://github.com/gorpnostic)")
        .call()
        .ok()?
        .body_mut()
        .read_to_string()
        .ok()?;
    let hits: Vec<serde_json::Value> = serde_json::from_str(&body).ok()?;
    let dur = t.duration;
    let mut hits: Vec<&serde_json::Value> =
        hits.iter().filter(|h| h.get("syncedLyrics").and_then(|s| s.as_str()).map(|s| !s.is_empty()).unwrap_or(false)).collect();
    hits.sort_by(|a, b| {
        let da = (a.get("duration").and_then(|d| d.as_f64()).unwrap_or(0.0) - dur).abs();
        let db = (b.get("duration").and_then(|d| d.as_f64()).unwrap_or(0.0) - dur).abs();
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    });
    let synced = hits.first().and_then(|h| h.get("syncedLyrics")).and_then(|s| s.as_str()).unwrap_or("").to_string();
    let _ = std::fs::create_dir_all(&own);
    let _ = std::fs::write(own.join(format!("{key}.json")), serde_json::json!({ "synced": synced }).to_string());
    Some(parse_lrc(&synced))
}

/// "[01:02.50][01:40.00] words" -> [(62.5, words), (100.0, words)], sorted.
pub fn parse_lrc(text: &str) -> Lyrics {
    let mut out = vec![];
    for line in text.lines() {
        let mut rest = line.trim_start();
        let mut stamps = vec![];
        while let Some(r) = rest.strip_prefix('[') {
            let Some(end) = r.find(']') else { break };
            let tag = &r[..end];
            let Some((m, s)) = tag.split_once(':') else { break };
            match (m.trim().parse::<f64>(), s.trim().parse::<f64>()) {
                (Ok(m), Ok(s)) => stamps.push(m * 60.0 + s),
                _ => break, // [ar:Artist] style header, not a timestamp
            }
            rest = &r[end + 1..];
        }
        let words = rest.trim().to_string();
        for ts in stamps {
            out.push((ts, words.clone()));
        }
    }
    out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// YouTube-style titles ("Artist - Song (Official Video)") -> (song, artist), for display and lyrics search.
/// Same rules as nest's clean_title.
pub fn clean_title(t: &Track) -> (String, String) {
    let mut title = t.title.clone();
    let mut artist = t.artist.clone();
    if let Some((a, rest)) = title.clone().split_once(" - ") {
        title = rest.to_string();
        if artist.is_empty() {
            artist = a.to_string();
        }
        if artist.to_lowercase() == "the vibe guide" || a.chars().count() < 40 {
            artist = a.to_string();
        }
    }
    (strip_junk(&title).trim().to_string(), artist.trim().to_string())
}

/// Remove "(Official Video)", "[4K]", "(Lyrics)"... groups.
fn strip_junk(s: &str) -> String {
    const WORDS: &[&str] = &["official", "video", "lyric", "audio", "4k", "hd", "visualizer", "remaster"];
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '(' || chars[i] == '[' {
            if let Some(off) = chars[i + 1..].iter().position(|&c| c == ')' || c == ']') {
                let inner: String = chars[i + 1..i + 1 + off].iter().collect::<String>().to_lowercase();
                if WORDS.iter().any(|w| inner.contains(w)) {
                    while out.ends_with(char::is_whitespace) {
                        out.pop();
                    }
                    i += off + 2;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn music_lrc_and_titles() {
        let l = parse_lrc("[ar:Someone]\n[00:03.50] Why you so mad?\n[01:00.00][00:10.00]Chorus\n[02:11.40] ");
        assert_eq!(l.len(), 4);
        assert_eq!(l[0], (3.5, "Why you so mad?".into()));
        assert_eq!(l[1].1, "Chorus");
        assert!((l[2].0 - 60.0).abs() < 1e-9);
        let t = Track { title: "House of Pain - Jump Around (Official 4K Music Video)".into(), artist: "The Vibe Guide".into(), ..Default::default() };
        assert_eq!(clean_title(&t), ("Jump Around".into(), "House of Pain".into()));
        let t = Track { title: "Loser".into(), artist: "Tame Impala".into(), ..Default::default() };
        assert_eq!(clean_title(&t), ("Loser".into(), "Tame Impala".into()));
        assert_eq!(file_key("0708663e-da06"), "0708663e-da06");
        assert_eq!(file_key("C:\\x\\y.mp3").len(), 16);
    }
}
