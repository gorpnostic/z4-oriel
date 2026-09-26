//! First run: a welcome screen, a few quick setup steps (theme, icons, folders, defaults, AIs) and an interactive
//! tour that waits for you to actually do each thing. The tour replays any time (palette → "take the tour", or
//! `oriel --tour`); the setup pages start from your current settings and only save what you change.

use crate::theme::{self, Theme};
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What the app looks like right now, for checking tour steps.
#[derive(Clone, Default, PartialEq)]
pub struct Probe {
    pub app: Option<&'static str>,
    pub user_tabs: usize,
    pub panes_in_tab: usize,
    pub focus: u64,
    pub palette_open: bool,
    pub tab_named: bool,
    pub ctx_seen: usize,
    pub palette_seen: usize,
    /// a right-click menu or the tab rename box is open (esc is theirs, not the tour's)
    pub menu_open: bool,
    /// a terminal has the keyboard: esc belongs to the program in it (Claude Code, vim), not the tour
    pub in_terminal: bool,
}

impl Probe {
    /// Does esc end the tour right now?
    fn esc_ends_tour(&self) -> bool {
        !self.palette_open && !self.menu_open && !self.in_terminal
    }
}

struct Step {
    title: &'static str,
    body: &'static str,
    keys: &'static str,
    /// done when this is true (compared with the state when the step began); None = read and press next
    done: Option<fn(&Probe, &Probe) -> bool>,
}

const STEPS: &[Step] = &[
    Step {
        title: "switch apps",
        body: "Everything lives in the sidebar: the ai apps on top, the tools below. Open music: press F4, or click it.",
        keys: "F4",
        done: Some(|now, _| now.app == Some("music")),
    },
    Step {
        title: "chat with any AI",
        body: "F1 is chat. /provider picks the AI (Claude Code, Codex, Ollama\u{2026}), /model its model, /perms what coding agents may do; alt , has every setting. Go back to chat.",
        keys: "F1",
        done: Some(|now, _| now.app == Some("ai")),
    },
    Step {
        title: "your AIs",
        body: "F3 installs and signs in to AI tools with one key each, and shows your plan limits and usage. Have a look.",
        keys: "F3",
        done: Some(|now, _| now.app == Some("ais")),
    },
    Step {
        title: "a team of agents",
        body: "F2 is agents. Press L there, give one goal and pick a lead AI: it splits the work between worker AIs, each in its own copy of the repo, and merges it safely. Open it.",
        keys: "F2",
        done: Some(|now, _| now.app == Some("agents")),
    },
    Step {
        title: "the palette",
        body: "alt p opens a search over every app, action and theme. Open it, have a look, then esc.",
        keys: "alt p  \u{b7}  esc",
        done: Some(|now, start| now.palette_seen > start.palette_seen && !now.palette_open),
    },
    Step {
        title: "a tab of your own",
        body: "Tabs you make show under the apps. Make one with alt t (or click \u{201c}new tab\u{201d}).",
        keys: "alt t",
        done: Some(|now, start| now.user_tabs > start.user_tabs),
    },
    Step {
        title: "split it",
        body: "alt n opens a real terminal beside whatever you're looking at. Try it.",
        keys: "alt n",
        done: Some(|now, start| now.panes_in_tab > start.panes_in_tab || (now.panes_in_tab >= 2 && start.panes_in_tab >= 2 && now.focus != start.focus)),
    },
    Step {
        title: "move between panes",
        body: "alt + arrow keys move between panes; alt shift + arrows resize them. Or just click, and drag the divider.",
        keys: "alt \u{2190}  alt \u{2192}",
        done: Some(|now, start| now.focus != start.focus),
    },
    Step {
        title: "name the tab",
        body: "Double-click your tab in the sidebar to rename it (or ctrl+space then ,). Type a name, enter.",
        keys: "double-click",
        done: Some(|now, _| now.tab_named),
    },
    Step {
        title: "right-click",
        body: "Right-click a pane or a tab for a menu: split, zoom, rename, close.",
        keys: "right-click",
        done: Some(|now, start| now.ctx_seen > start.ctx_seen),
    },
    Step {
        title: "copy, paste, close",
        body: "Drag over any text to copy it (shift+drag in programs that use the mouse). alt v pastes a screenshot into Claude Code, Codex or chat. The \u{d7} on a pane or tab closes it, and so does alt w.",
        keys: "",
        done: None,
    },
    Step {
        title: "agents look after themselves",
        body: "Run Claude Code or Codex in a terminal (or start tasks in agents, F2) and their tabs get status dots: \u{25d0} working, red \u{25cf} needs you, green \u{25cf} done. You get a toast when one finishes while you're elsewhere.",
        keys: "",
        done: None,
    },
    Step {
        title: "the help screen",
        body: "F10 (or ? in most apps) opens the guide: every key and command, by topic, starting on the app you're in. Open it.",
        keys: "F10",
        done: Some(|now, _| now.app == Some("help")),
    },
    Step {
        title: "you're set",
        body: "alt p finds everything and F10 explains it. Replay this tour from the palette (\u{201c}take the tour\u{201d}) or with oriel --tour.",
        keys: "",
        done: None,
    },
];

/// The step whose key is the app's own F10, so the tour lets it through instead of skipping.
const HELP_STEP: &str = "the help screen";

#[derive(Clone, Copy, PartialEq)]
enum Btn {
    Tour,
    Skip,
    Next,
    End,
}

pub enum Stage {
    Welcome,
    Theme { sel: usize },
    Icons,
    /// a folder path being typed (music, then notes)
    Music { input: String, found: Option<usize> },
    Notes { input: String },
    /// what oriel opens on + the default AI: which row is active, and each choice
    Defaults { row: usize, start: usize, ai: usize },
    Ais,
    Tour { step: usize, start: Probe },
}

const STARTS: &[(&str, &str)] = &[("last", "where you left off"), ("ai", "chat"), ("agents", "agents"), ("home", "home screen"), ("terminal", "terminal"), ("music", "music")];
const STEPS_SETUP: usize = 6; // theme, icons, music, notes, defaults, AIs

/// What the app should do after a key/click. Setup pages only send a value you actually changed.
pub enum Out {
    None,
    /// preview (save=false) or pick (save=true) a theme
    Theme(String, bool),
    Icons(bool),
    /// the first music folder (the others are kept)
    MusicFolder(String),
    NotesFolder(String),
    /// empty = unchanged
    Defaults { startup: String, provider: String },
    /// the setup finished or was skipped: write the marker
    Finished,
}

pub struct Onboard {
    pub stage: Stage,
    themes: Vec<String>,
    theme_before: String,
    ais: Vec<(&'static str, bool)>,
    /// provider ids for the default-AI choice: the ones this machine has, then the saved one if it isn't among them
    ai_ids: Vec<&'static str>,
    /// how many of ai_ids were found on this machine
    ai_found: usize,
    /// what oriel can open on: STARTS, plus the saved startup app if it's another one
    starts: Vec<(String, String)>,
    /// the saved choices, where the Defaults page starts (Enter without changing them saves nothing)
    start_was: usize,
    ai_was: usize,
    /// icons as they are now: Enter on the icons page keeps them
    nerd_was: bool,
    music_default: String,
    notes_default: String,
    /// the audio-player app's library exists (Windows): music uses it automatically
    audio_player: Option<usize>,
    /// the song count under the music field: worked out on a thread a moment after typing stops (a network
    /// drive can take seconds to walk), so typing never waits on the disk
    count_due: Option<(String, Instant)>,
    counted: Arc<Mutex<Vec<(String, Option<usize>)>>>,
    counting: bool,
    /// esc ends the tour right now (not while a terminal has the keyboard): the card's end button says which
    esc_ends: bool,
    hits: Vec<(Rect, Btn)>,
    /// the tour card, drawn over the app: clicks on it are its own, not the pane's underneath
    card: Rect,
    /// the prefix key as configured, for the tour's text
    prefix: String,
}

/// Quick count of audio files under a folder (bounded: at most 20k entries, 4 levels down). None = no such folder.
pub(crate) fn count_audio(dir: &str) -> Option<usize> {
    let root = std::path::Path::new(dir.trim());
    if !root.is_dir() {
        return None;
    }
    let mut n = 0;
    let mut seen = 0;
    let mut stack = vec![(root.to_path_buf(), 0)];
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen > 20_000 {
                return Some(n);
            }
            let p = e.path();
            // the entry's own type: no extra stat per file
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                if depth < 4 {
                    stack.push((p, depth + 1));
                }
            } else if let Some(x) = p.extension().and_then(|x| x.to_str()) {
                if ["mp3", "flac", "ogg", "wav", "m4a", "opus", "aac"].contains(&x.to_ascii_lowercase().as_str()) {
                    n += 1;
                }
            }
        }
    }
    Some(n)
}

/// Tab-complete a folder path: extend to the longest common prefix of matching sub-folders.
pub(crate) fn complete_dir(input: &str) -> String {
    let path = std::path::Path::new(input);
    let (parent, stem) = if input.ends_with(['/', '\\']) {
        (path.to_path_buf(), String::new())
    } else {
        (path.parent().map(|p| p.to_path_buf()).unwrap_or_default(), path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default())
    };
    let Ok(rd) = std::fs::read_dir(if parent.as_os_str().is_empty() { std::path::Path::new(".") } else { &parent }) else { return input.to_string() };
    let low = stem.to_lowercase();
    let names: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.to_lowercase().starts_with(&low))
        .collect();
    if names.is_empty() {
        return input.to_string();
    }
    let mut common = names[0].clone();
    for n in &names[1..] {
        while !n.to_lowercase().starts_with(&common.to_lowercase()) {
            common.pop();
        }
    }
    let sep = if cfg!(windows) { "\\" } else { "/" };
    let mut out = parent.join(&common).to_string_lossy().to_string();
    if names.len() == 1 {
        out.push_str(sep);
    }
    out
}

/// Edit a one-line text field with a key; true if it changed.
fn edit_line(input: &mut String, k: &KeyEvent) -> bool {
    match k.code {
        KeyCode::Char(c) if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
            input.push(c);
            true
        }
        KeyCode::Backspace => input.pop().is_some(),
        KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            input.clear();
            true
        }
        KeyCode::Tab => {
            let c = complete_dir(input);
            let ch = c != *input;
            *input = c;
            ch
        }
        _ => false,
    }
}

pub fn marker() -> std::path::PathBuf {
    crate::config::data_dir().join("onboarded")
}

pub fn done_before() -> bool {
    marker().exists()
}

pub fn mark_done() {
    if cfg!(test) {
        return;
    }
    let _ = std::fs::write(marker(), "1");
}

impl Onboard {
    /// The first run: welcome, setup, then the tour. Every setup page starts from `cfg`.
    pub fn new(theme_now: &str, cfg: &crate::config::Config) -> Onboard {
        use crate::panes::chat::providers;
        let themes = theme::names();
        // tests never probe this machine for AIs (TCP) or read its audio-player library
        let avail = if cfg!(test) { vec![] } else { providers::available(&cfg.ai) };
        let ais = providers::PROVIDERS.iter().map(|p| (p.1, avail.contains(&p.0))).collect();
        let music_default = cfg.music.folders.first().cloned().unwrap_or_else(|| dirs::audio_dir().map(|d| d.to_string_lossy().to_string()).unwrap_or_default());
        let notes_default = if cfg.notes_folder.is_empty() { crate::config::data_dir().join("notes").to_string_lossy().to_string() } else { cfg.notes_folder.clone() };
        // the audio-player app (Windows): its library is picked up automatically
        let audio_player = std::env::var_os("APPDATA")
            .filter(|_| !cfg!(test))
            .map(|a| std::path::PathBuf::from(a).join("audio-player").join("library.json"))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .map(|v| v.get("tracks").and_then(|t| t.as_array()).map(|a| a.len()).or_else(|| v.as_array().map(|a| a.len())).unwrap_or(0));
        // the saved startup app and default AI are always on offer, so pressing enter keeps them
        let mut starts: Vec<(String, String)> = STARTS.iter().map(|s| (s.0.to_string(), s.1.to_string())).collect();
        if !cfg.startup.is_empty() && !starts.iter().any(|s| s.0 == cfg.startup) {
            starts.push((cfg.startup.clone(), cfg.startup.clone()));
        }
        let start_was = starts.iter().position(|s| s.0 == cfg.startup).unwrap_or(0);
        let ai_found = avail.len();
        let mut ai_ids = avail;
        if let Some(p) = providers::PROVIDERS.iter().find(|p| p.0 == cfg.ai.provider).filter(|p| !ai_ids.contains(&p.0)) {
            ai_ids.push(p.0);
        }
        let ai_was = ai_ids.iter().position(|p| *p == cfg.ai.provider).unwrap_or(0);
        Onboard {
            stage: Stage::Welcome,
            themes,
            theme_before: theme_now.to_string(),
            ais,
            ai_ids,
            ai_found,
            starts,
            start_was,
            ai_was,
            nerd_was: ui::NERD.load(std::sync::atomic::Ordering::Relaxed),
            music_default,
            notes_default,
            audio_player,
            count_due: None,
            counted: Arc::default(),
            counting: false,
            esc_ends: true,
            hits: vec![],
            card: Rect::default(),
            prefix: crate::config::prefix(cfg),
        }
    }

    /// Straight into the tour (a replay: palette "take the tour", `oriel --tour`). The setup pages can't be
    /// reached from here, so none of their data is gathered (no looking for AIs on the network).
    pub fn tour(theme_now: &str, probe: &Probe, prefix: &str) -> Onboard {
        Onboard {
            stage: Stage::Tour { step: 0, start: probe.clone() },
            themes: vec![],
            theme_before: theme_now.to_string(),
            ais: vec![],
            ai_ids: vec![],
            ai_found: 0,
            starts: vec![],
            start_was: 0,
            ai_was: 0,
            nerd_was: ui::NERD.load(std::sync::atomic::Ordering::Relaxed),
            music_default: String::new(),
            notes_default: String::new(),
            audio_player: None,
            count_due: None,
            counted: Arc::default(),
            counting: false,
            esc_ends: true,
            hits: vec![],
            card: Rect::default(),
            prefix: prefix.to_string(),
        }
    }

    fn to_music(&mut self) {
        let input = self.music_default.clone();
        self.recount(&input, Duration::ZERO);
        self.stage = Stage::Music { input, found: None };
    }

    /// Count the songs under `path` once `after` has passed without another change.
    fn recount(&mut self, path: &str, after: Duration) {
        self.count_due = Some((path.to_string(), Instant::now() + after));
        self.counting = true;
    }

    /// Start a count that's due, and take in the ones that finished (called as the music page draws).
    fn pump_count(&mut self) {
        let Stage::Music { input, found } = &mut self.stage else { return };
        for (p, n) in std::mem::take(&mut *self.counted.lock().unwrap()) {
            if p == *input {
                *found = n;
                self.counting = self.count_due.is_some();
            }
        }
        if let Some((p, _)) = self.count_due.take_if(|(_, at)| Instant::now() >= *at) {
            let out = self.counted.clone();
            std::thread::spawn(move || {
                let n = count_audio(&p);
                out.lock().unwrap().push((p, n));
            });
        }
    }

    /// The welcome animates (ultra's rainbow) even when the app theme doesn't; the music page redraws until its
    /// song count is in.
    pub fn animating(&self) -> bool {
        matches!(self.stage, Stage::Welcome) || (matches!(self.stage, Stage::Music { .. }) && self.counting)
    }

    pub fn is_modal(&self) -> bool {
        !matches!(self.stage, Stage::Tour { .. })
    }

    fn start_tour(&mut self, probe: &Probe) {
        self.stage = Stage::Tour { step: 0, start: probe.clone() };
    }

    /// Keys while a setup screen is up (modal) or during the tour (only F10, esc and enter on a read-only step
    /// are ours; F11 still ends it too, though Windows Terminal keeps F11 for fullscreen).
    pub fn key(&mut self, k: KeyEvent, probe: &Probe) -> (bool, Out) {
        let plain = !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match &mut self.stage {
            Stage::Welcome => match k.code {
                KeyCode::Enter => {
                    let sel = self.themes.iter().position(|t| *t == self.theme_before).unwrap_or(0);
                    self.stage = Stage::Theme { sel };
                }
                KeyCode::Char('s') | KeyCode::Esc if plain => return (true, self.finish()),
                _ => {}
            },
            Stage::Theme { sel } => match k.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    *sel = sel.saturating_sub(1);
                    return (true, Out::Theme(self.themes[*sel].clone(), false));
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                    *sel = (*sel + 1).min(self.themes.len() - 1);
                    return (true, Out::Theme(self.themes[*sel].clone(), false));
                }
                KeyCode::Enter => {
                    let t = self.themes[*sel].clone();
                    self.stage = Stage::Icons;
                    return (true, Out::Theme(t, true));
                }
                KeyCode::Esc => {
                    let t = self.theme_before.clone();
                    self.stage = Stage::Icons;
                    return (true, Out::Theme(t, false));
                }
                _ => {}
            },
            Stage::Icons => {
                // enter keeps what you have; y / n say which, and only a change is saved
                let pick = match k.code {
                    KeyCode::Char('y') => Some(true),
                    KeyCode::Char('n') => Some(false),
                    KeyCode::Enter | KeyCode::Esc => Some(self.nerd_was),
                    _ => None,
                };
                if let Some(nerd) = pick {
                    self.to_music();
                    if nerd != self.nerd_was {
                        return (true, Out::Icons(nerd));
                    }
                }
            }
            Stage::Music { input, .. } => match k.code {
                KeyCode::Enter => {
                    let path = input.trim().to_string();
                    self.stage = Stage::Notes { input: self.notes_default.clone() };
                    self.count_due = None;
                    self.counting = false;
                    if !path.is_empty() && path != self.music_default.trim() && std::path::Path::new(&path).is_dir() {
                        return (true, Out::MusicFolder(path));
                    }
                }
                KeyCode::Esc => {
                    self.stage = Stage::Notes { input: self.notes_default.clone() };
                    self.count_due = None;
                    self.counting = false;
                }
                _ => {
                    if edit_line(input, &k) {
                        let path = input.clone();
                        self.recount(&path, Duration::from_millis(300));
                    }
                }
            },
            Stage::Notes { input } => match k.code {
                KeyCode::Enter => {
                    let path = input.trim().to_string();
                    self.stage = Stage::Defaults { row: 0, start: self.start_was, ai: self.ai_was };
                    if !path.is_empty() && path != self.notes_default {
                        return (true, Out::NotesFolder(path));
                    }
                }
                KeyCode::Esc => self.stage = Stage::Defaults { row: 0, start: self.start_was, ai: self.ai_was },
                _ => {
                    edit_line(input, &k);
                }
            },
            Stage::Defaults { row, start, ai } => match k.code {
                KeyCode::Up | KeyCode::Down | KeyCode::Tab => *row = 1 - (*row).min(1),
                KeyCode::Left | KeyCode::Char('h') => {
                    if *row == 0 {
                        *start = (*start + self.starts.len() - 1) % self.starts.len();
                    } else if !self.ai_ids.is_empty() {
                        *ai = (*ai + self.ai_ids.len() - 1) % self.ai_ids.len();
                    }
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    if *row == 0 {
                        *start = (*start + 1) % self.starts.len();
                    } else if !self.ai_ids.is_empty() {
                        *ai = (*ai + 1) % self.ai_ids.len();
                    }
                }
                KeyCode::Enter => {
                    // only what you changed: the rest stays as it's saved
                    let startup = if *start != self.start_was { self.starts[*start].0.clone() } else { String::new() };
                    let provider = if *ai != self.ai_was { self.ai_ids.get(*ai).map(|s| s.to_string()).unwrap_or_default() } else { String::new() };
                    self.stage = Stage::Ais;
                    if startup.is_empty() && provider.is_empty() {
                        return (true, Out::None);
                    }
                    return (true, Out::Defaults { startup, provider });
                }
                KeyCode::Esc => self.stage = Stage::Ais,
                _ => {}
            },
            Stage::Ais => match k.code {
                KeyCode::Enter => self.start_tour(probe),
                KeyCode::Char('s') | KeyCode::Esc if plain => return (true, self.finish()),
                _ => {}
            },
            Stage::Tour { step, start } => match k.code {
                KeyCode::F(10) if STEPS[*step].title == HELP_STEP => return (false, Out::None),
                KeyCode::F(10) => {
                    return (true, self.advance(probe));
                }
                // esc ends it, unless the palette, a menu or the rename box is open (esc closes those first) or
                // a terminal has the keyboard (its program gets esc; the card's × still ends the tour)
                KeyCode::Esc if probe.esc_ends_tour() => return (true, self.finish()),
                KeyCode::F(11) => return (true, self.finish()),
                KeyCode::Enter if STEPS[*step].done.is_none() => {
                    let _ = start;
                    return (true, self.advance(probe));
                }
                _ => return (false, Out::None),
            },
        }
        (true, Out::None)
    }

    /// Pasted text (ctrl+v of a folder path) into the folder field that's up; swallowed by any other setup
    /// screen, so it never lands in the pane behind.
    pub fn paste(&mut self, text: &str) {
        let text = text.trim();
        // Explorer's "copy as path" wraps it in quotes
        let text = text.strip_prefix('"').and_then(|t| t.strip_suffix('"')).unwrap_or(text);
        let clean = |input: &mut String| input.extend(text.chars().filter(|c| !c.is_control()));
        match &mut self.stage {
            Stage::Music { input, found } => {
                clean(input);
                *found = count_audio(input);
            }
            Stage::Notes { input } => clean(input),
            _ => {}
        }
    }

    fn advance(&mut self, probe: &Probe) -> Out {
        if let Stage::Tour { step, start } = &mut self.stage {
            if *step + 1 >= STEPS.len() {
                return self.finish();
            }
            *step += 1;
            *start = probe.clone();
        }
        Out::None
    }

    fn finish(&mut self) -> Out {
        Out::Finished
    }

    /// Called after every event batch during the tour: moves on when the step's action happened.
    pub fn check(&mut self, probe: &Probe) -> Out {
        self.esc_ends = probe.esc_ends_tour();
        if let Stage::Tour { step, start } = &self.stage {
            if let Some(done) = STEPS[*step].done {
                if done(probe, start) {
                    return self.advance(probe);
                }
            }
        }
        Out::None
    }

    pub fn mouse(&mut self, m: MouseEvent, probe: &Probe) -> (bool, Out) {
        let pos = Position { x: m.column, y: m.row };
        if let MouseEventKind::Down(MouseButton::Left) = m.kind {
            if let Some(&(_, b)) = self.hits.iter().find(|(r, _)| r.contains(pos)) {
                return (true, match b {
                    Btn::Tour => {
                        let sel = self.themes.iter().position(|t| *t == self.theme_before).unwrap_or(0);
                        self.stage = Stage::Theme { sel };
                        Out::None
                    }
                    Btn::Skip | Btn::End => self.finish(),
                    Btn::Next => self.advance(probe),
                });
            }
        }
        (self.is_modal() || self.card.contains(pos), Out::None)
    }

    // ------------------------------------------------------------------ drawing
    pub fn draw(&mut self, f: &mut Frame, area: Rect, t: &Theme, time: f64) {
        self.hits.clear();
        self.pump_count();
        self.card = Rect::default();
        let (n, title) = match &self.stage {
            // the welcome is always ultra, whatever theme is set
            Stage::Welcome => return self.draw_welcome(f, area, &theme::get("ultra"), time),
            Stage::Tour { step, .. } => {
                let step = *step;
                return self.draw_tour(f, area, t, step);
            }
            Stage::Theme { .. } => (1, "pick a look"),
            Stage::Icons => (2, "icons"),
            Stage::Music { .. } => (3, "your music"),
            Stage::Notes { .. } => (4, "your notes"),
            Stage::Defaults { .. } => (5, "defaults"),
            Stage::Ais => (6, "your AIs"),
        };
        let (body, hints_y) = page(f, area, t, time, n, title);
        let w = body.width.min(84);
        let body = Rect { x: body.x + (body.width - w) / 2, width: w, ..body };
        let mut lines: Vec<Line> = vec![];
        let hints: Vec<(&str, &str)>;
        match &self.stage {
            Stage::Theme { sel } => {
                lines.push(Line::styled("It changes as you move. Switch any time with alt p → theme.", ui::muted(t)));
                lines.push(Line::raw(""));
                // two columns of themes
                let half = self.themes.len().div_ceil(2);
                for r in 0..half {
                    let mut spans = vec![];
                    for c in 0..2 {
                        let i = r + c * half;
                        let Some(name) = self.themes.get(i) else { continue };
                        let on = i == *sel;
                        let th = theme::get(name);
                        let note = match name.as_str() {
                            "omarchy" => " · live",
                            "terminal" => " · yours",
                            "ultra" => " · animated",
                            _ => "",
                        };
                        spans.push(Span::styled(if on { " ▌ " } else { "   " }, Style::default().fg(t.accent)));
                        spans.push(Span::styled("■■ ", Style::default().fg(th.accent)));
                        spans.push(Span::styled(format!("{:<24}", format!("{name}{note}")), if on { Style::default().fg(t.accent).add_modifier(Modifier::BOLD) } else { Style::default() }));
                    }
                    lines.push(Line::from(spans));
                }
                hints = vec![("↑↓", "choose"), ("enter", "use it"), ("esc", "keep this one")];
            }
            Stage::Icons => {
                let icons = ["ai", "music", "system", "files", "notes", "storage", "term", "robot", "gauge"];
                lines.push(Line::styled("Do these look like little pictures?", Style::default().add_modifier(Modifier::BOLD)));
                lines.push(Line::raw(""));
                lines.push(Line::from(icons.iter().map(|i| Span::styled(format!(" {}   ", ui::icon_nerd(i)), Style::default().fg(t.accent))).collect::<Vec<_>>()));
                lines.push(Line::raw(""));
                lines.push(Line::styled("Boxes or question marks mean your terminal font has no icons. Pick plain text,", ui::muted(t)));
                lines.push(Line::styled("or install a Nerd Font (nerdfonts.com) and switch back from the palette later.", ui::muted(t)));
                hints = vec![("y", "yes, pictures"), ("n", "boxes: plain text"), ("enter", if self.nerd_was { "keep pictures" } else { "keep plain text" })];
            }
            Stage::Music { input, found } => {
                if let Some(n) = self.audio_player {
                    lines.push(Line::from(vec![Span::styled("✓ ", Style::default().fg(t.good)), Span::styled(format!("found your audio-player library ({n} songs) — music uses it automatically"), Style::default().add_modifier(Modifier::BOLD))]));
                    lines.push(Line::styled("The folder below is only used when that library isn't there.", ui::muted(t)));
                } else {
                    lines.push(Line::styled("Where's your music? oriel plays mp3, flac, ogg, wav and m4a from this folder.", Style::default().add_modifier(Modifier::BOLD)));
                }
                lines.push(Line::raw(""));
                lines.extend(field(input, t, body.width));
                lines.push(Line::raw(""));
                lines.push(match found {
                    _ if self.counting => Line::styled("  looking for songs…", ui::muted(t)),
                    Some(0) => Line::styled("  no songs found there yet (that's ok)", ui::muted(t)),
                    Some(n) => Line::styled(format!("  ✓ {n}{} songs found", if *n >= 20_000 { "+" } else { "" }), Style::default().fg(t.good)),
                    None => Line::styled("  that folder doesn't exist", Style::default().fg(t.danger)),
                });
                hints = vec![("type", "a path"), ("tab", "complete"), ("enter", "use it"), ("esc", "skip")];
            }
            Stage::Notes { input } => {
                lines.push(Line::styled("Where should notes live? They're plain .md files, so any folder works:", Style::default().add_modifier(Modifier::BOLD)));
                lines.push(Line::styled("a synced folder (OneDrive, Syncthing, an Obsidian vault) keeps them everywhere.", ui::muted(t)));
                lines.push(Line::raw(""));
                lines.extend(field(input, t, body.width));
                hints = vec![("type", "a path"), ("tab", "complete"), ("enter", "use it"), ("esc", "keep the default")];
            }
            Stage::Defaults { row, start, ai } => {
                let choice = |on_row: bool, items: Vec<String>, sel: usize| -> Line<'static> {
                    let mut spans = vec![];
                    for (i, it) in items.iter().enumerate() {
                        let on = i == sel;
                        let st = if on && on_row {
                            Style::default().fg(t.accent).add_modifier(Modifier::BOLD | Modifier::REVERSED)
                        } else if on {
                            Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
                        } else {
                            ui::muted(t)
                        };
                        spans.push(Span::styled(format!(" {it} "), st));
                        spans.push(Span::raw(" "));
                    }
                    Line::from(spans)
                };
                lines.push(Line::styled("open oriel on", Style::default().add_modifier(Modifier::BOLD)));
                lines.push(choice(*row == 0, self.starts.iter().map(|s| s.1.clone()).collect(), *start));
                lines.push(Line::raw(""));
                lines.push(Line::styled("default AI for chat", Style::default().add_modifier(Modifier::BOLD)));
                if self.ai_ids.is_empty() {
                    lines.push(Line::styled("  none found yet — the next page shows how to add one", ui::muted(t)));
                } else {
                    let label = |i: usize, id: &str| {
                        let l = crate::panes::chat::providers::label(id);
                        if i < self.ai_found { l.to_string() } else { format!("{l} (not found here)") }
                    };
                    lines.push(choice(*row == 1, self.ai_ids.iter().enumerate().map(|(i, id)| label(i, id)).collect(), *ai));
                }
                hints = vec![("↑↓", "row"), ("←→", "choose"), ("enter", "next")];
            }
            Stage::Ais => {
                let found = self.ais.iter().filter(|a| a.1).count();
                lines.push(Line::styled(
                    if found > 0 { format!("Found {found} AI{} ready to use:", if found == 1 { "" } else { "s" }) } else { "No AI set up yet — that's fine, everything else works without one.".into() },
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                lines.push(Line::raw(""));
                for (label, ok) in &self.ais {
                    lines.push(Line::from(vec![
                        Span::styled(if *ok { "  ✓ " } else { "  · " }, Style::default().fg(if *ok { t.good } else { t.muted })),
                        Span::styled(label.to_string(), if *ok { Style::default() } else { ui::muted(t) }),
                    ]));
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled("your AIs (F3) installs Claude Code, Codex, Kimi, OpenCode and more with one key,", ui::muted(t)));
                lines.push(Line::styled("and shows your plan limits and usage.", ui::muted(t)));
                hints = vec![("enter", "take the tour (about a minute)"), ("s", "skip it")];
            }
            _ => hints = vec![],
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
        let spans: Vec<Span> = hints.iter().flat_map(|(k, w)| ui::key_hint(k, w, t)).collect();
        f.render_widget(Paragraph::new(Line::from(spans)).centered(), Rect { x: area.x, y: hints_y, width: area.width, height: 1 });
    }

    fn draw_welcome(&mut self, f: &mut Frame, area: Rect, t: &Theme, time: f64) {
        f.render_widget(Clear, area);
        if !matches!(t.bg, ratatui::style::Color::Reset) {
            f.render_widget(ratatui::widgets::Block::default().style(Style::default().bg(t.bg)), area);
        }
        let logo = ui::logo_lines();
        let lw = logo[0].chars().count() as u16;
        let h = 8 + 3 + 5 + 3;
        let y0 = area.y + area.height.saturating_sub(h) / 2;
        if area.width > lw + 2 {
            ui::big_logo(f, &logo, area.x + (area.width - lw) / 2, y0, t, time);
        }
        let mut y = y0 + 10;
        let center = |f: &mut Frame, y: u16, line: Line| {
            f.render_widget(Paragraph::new(line).centered(), Rect { x: area.x, y, width: area.width, height: 1 });
        };
        center(f, y, Line::styled("a window onto everything", Style::default().fg(t.shine)));
        y += 2;
        for l in [
            "AI chat and coding agents · an orchestrator for many at once · your AI limits",
            "music · system · files · notes · storage · real terminals, split any way you like",
        ] {
            center(f, y, Line::styled(l, ui::muted(t)));
            y += 1;
        }
        y += 2;
        // two buttons
        let a = " enter  take the tour ";
        let b = " s  skip ";
        let total = (a.chars().count() + 3 + b.chars().count()) as u16;
        let x = area.x + area.width.saturating_sub(total) / 2;
        let ra = Rect { x, y, width: a.chars().count() as u16, height: 1 };
        let rb = Rect { x: x + ra.width + 3, y, width: b.chars().count() as u16, height: 1 };
        f.render_widget(Paragraph::new(Span::styled(a, Style::default().fg(t.accent).add_modifier(Modifier::REVERSED | Modifier::BOLD))), ra);
        f.render_widget(Paragraph::new(Span::styled(b, Style::default().fg(t.muted).add_modifier(Modifier::REVERSED))), rb);
        self.hits.push((ra, Btn::Tour));
        self.hits.push((rb, Btn::Skip));
        center(f, y + 2, Line::styled("takes about a minute · replay it later from the palette", ui::muted(t)));
    }

    fn draw_tour(&mut self, f: &mut Frame, area: Rect, t: &Theme, step: usize) {
        let s = &STEPS[step];
        let body = s.body.replace("ctrl+space", &self.prefix);
        let w = 58.min(area.width.saturating_sub(4));
        let body_lines = (body.chars().count() as u16).div_ceil(w.saturating_sub(4).max(1)) + 1;
        let h = 4 + body_lines + if s.keys.is_empty() { 0 } else { 2 };
        let r = Rect { x: area.right().saturating_sub(w + 2), y: area.bottom().saturating_sub(h + 1), width: w, height: h.min(area.height) };
        self.card = r;
        f.render_widget(Clear, r);
        let title = format!("tour · {}/{} · {}", step + 1, STEPS.len(), s.title);
        let inner = ui::frame(f, r, &title, None, true, t);
        // a × on the frame, like a pane's
        if r.width > 12 {
            let x = Rect { x: r.right() - 5, y: r.y, width: 3, height: 1 };
            f.render_widget(Paragraph::new(Span::styled(" × ", Style::default().fg(t.accent))), x);
            self.hits.push((x, Btn::End));
        }
        let mut lines = vec![Line::raw(body)];
        if !s.keys.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![Span::styled("try  ", ui::muted(t)), Span::styled(s.keys, Style::default().fg(t.accent).add_modifier(Modifier::BOLD))]));
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), Rect { height: inner.height.saturating_sub(1), ..inner });
        // buttons on the bottom row
        let by = inner.bottom().saturating_sub(1);
        let next = if s.done.is_none() { if step + 1 == STEPS.len() { " enter  finish " } else { " enter  next › " } } else if s.title == HELP_STEP { " skip step › " } else { " F10  skip step › " };
        // in a terminal esc is the program's, so the button is the way out (as is the ×)
        let end = if self.esc_ends { " esc  end tour " } else { " end tour " };
        let rn = Rect { x: inner.x, y: by, width: next.chars().count() as u16, height: 1 };
        let re = Rect { x: inner.x + rn.width + 2, y: by, width: end.chars().count() as u16, height: 1 };
        f.render_widget(Paragraph::new(Span::styled(next, Style::default().fg(t.accent).add_modifier(Modifier::REVERSED | Modifier::BOLD))), rn);
        f.render_widget(Paragraph::new(Span::styled(end, Style::default().fg(t.muted).add_modifier(Modifier::REVERSED))), re);
        self.hits.push((rn, Btn::Next));
        self.hits.push((re, Btn::End));
    }
}

/// A full-screen setup page like the home screen: the logo, step dots, the step's title. Returns the body rect
/// and the row for the key hints.
fn page(f: &mut Frame, area: Rect, t: &Theme, time: f64, n: usize, title: &str) -> (Rect, u16) {
    f.render_widget(Clear, area);
    if !matches!(t.bg, ratatui::style::Color::Reset) {
        f.render_widget(ratatui::widgets::Block::default().style(Style::default().bg(t.bg)), area);
    }
    let logo = ui::logo_lines();
    let lw = logo[0].chars().count() as u16;
    let big = area.height >= 34 && area.width > lw + 2;
    let mut y = area.y + if big { area.height.saturating_sub(34) / 3 + 1 } else { 1 };
    if big {
        ui::big_logo(f, &logo, area.x + (area.width - lw) / 2, y, t, time);
        y += 9;
    } else {
        f.render_widget(Paragraph::new(Span::styled("oriel", ui::bold_accent(t))).centered(), Rect { x: area.x, y, width: area.width, height: 1 });
        y += 2;
    }
    let dots: Vec<Span> = (1..=STEPS_SETUP)
        .map(|i| Span::styled(if i == n { " ● " } else if i < n { " ● " } else { " ○ " }, Style::default().fg(if i == n { t.accent } else if i < n { t.muted } else { t.frame })))
        .collect();
    f.render_widget(Paragraph::new(Line::from(dots)).centered(), Rect { x: area.x, y, width: area.width, height: 1 });
    y += 2;
    f.render_widget(
        Paragraph::new(Span::styled(format!("setup · {title}"), Style::default().fg(t.accent).add_modifier(Modifier::BOLD))).centered(),
        Rect { x: area.x, y, width: area.width, height: 1 },
    );
    y += 2;
    let hints_y = area.bottom().saturating_sub(2);
    (Rect { x: area.x + 2, y, width: area.width.saturating_sub(4), height: hints_y.saturating_sub(y + 1) }, hints_y)
}

/// A one-line input box (rounded look drawn with text so it wraps with the page).
fn field(input: &str, t: &Theme, width: u16) -> Vec<Line<'static>> {
    let w = (width as usize).saturating_sub(4).max(20);
    let shown = crate::ui::fit(input, w - 2);
    let pad = w.saturating_sub(unicode_width::UnicodeWidthStr::width(shown.as_str()) + 2);
    vec![
        Line::styled(format!("╭{}╮", "─".repeat(w)), Style::default().fg(t.accent)),
        Line::from(vec![
            Span::styled("│ ", Style::default().fg(t.accent)),
            Span::styled(shown, Style::default().add_modifier(Modifier::BOLD)),
            Span::styled("▏", Style::default().fg(t.accent)),
            Span::raw(" ".repeat(pad.saturating_sub(1))),
            Span::styled("│", Style::default().fg(t.accent)),
        ]),
        Line::styled(format!("╰{}╯", "─".repeat(w)), Style::default().fg(t.accent)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn key(o: &mut Onboard, c: KeyCode) -> Out {
        o.key(KeyEvent::new(c, KeyModifiers::NONE), &Probe::default()).1
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::path::absolute(format!("target/test-scratch/config/{name}")).unwrap();
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Setup starts from what's saved, and enter on a page you didn't change saves nothing.
    #[test]
    fn onboard_setup_starts_from_your_settings() {
        let (a, b) = (scratch("onboard-a"), scratch("onboard-b"));
        let mut cfg = crate::config::Config::default();
        cfg.startup = "files".into(); // a fine startup app, just not one of setup's five
        cfg.ai.provider = "anthropic".into(); // kept on offer even if this machine doesn't have it
        cfg.music.folders = vec![a.to_string_lossy().into(), b.to_string_lossy().into()];
        cfg.notes_folder = b.to_string_lossy().into();
        let mut o = Onboard::new("oriel", &cfg);
        o.nerd_was = false; // plain icons, picked before
        o.stage = Stage::Icons;
        assert!(matches!(key(&mut o, KeyCode::Enter), Out::None), "enter keeps plain icons");
        assert!(matches!(&o.stage, Stage::Music { input, .. } if *input == cfg.music.folders[0]));
        assert!(matches!(key(&mut o, KeyCode::Enter), Out::None), "the same music folder: nothing to save");
        assert!(matches!(key(&mut o, KeyCode::Enter), Out::None), "the same notes folder");
        let Stage::Defaults { start, ai, .. } = o.stage else { panic!("on the defaults page") };
        assert_eq!((o.starts[start].0.as_str(), o.ai_ids[ai]), ("files", "anthropic"));
        assert!(matches!(key(&mut o, KeyCode::Enter), Out::None), "defaults untouched: nothing to save");
        // change one thing and only that is sent
        o.stage = Stage::Defaults { row: 0, start: o.start_was, ai: o.ai_was };
        key(&mut o, KeyCode::Right);
        match key(&mut o, KeyCode::Enter) {
            Out::Defaults { startup, provider } => assert_eq!((startup.as_str(), provider.as_str()), (STARTS[0].0, "")),
            _ => panic!("expected the new startup app"),
        }
        o.stage = Stage::Icons;
        assert!(matches!(key(&mut o, KeyCode::Char('y')), Out::Icons(true)));
        o.stage = Stage::Music { input: b.to_string_lossy().into(), found: None };
        match key(&mut o, KeyCode::Enter) {
            Out::MusicFolder(p) => assert_eq!(p, b.to_string_lossy()),
            _ => panic!("a different folder is sent"),
        }
    }

    /// The song count under the music field is worked out on a thread a moment after typing stops, so a slow
    /// drive never holds up typing.
    #[test]
    fn onboard_counts_songs_in_the_background() {
        let d = scratch("onboard-songs");
        for f in ["a.mp3", "b.flac", "notes.txt"] {
            std::fs::write(d.join(f), "").unwrap();
        }
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub").join("c.ogg"), "").unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.music.folders = vec![d.to_string_lossy().into()];
        let mut o = Onboard::new("oriel", &cfg);
        o.stage = Stage::Icons;
        key(&mut o, KeyCode::Enter);
        assert!(o.counting && o.animating(), "the page redraws until the count is in");
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let t = theme::get("oriel");
        let mut wait = |o: &mut Onboard| {
            let t0 = Instant::now();
            while o.counting && t0.elapsed() < Duration::from_secs(5) {
                term.draw(|f| {
                    let a = f.area();
                    o.draw(f, a, &t, 0.0)
                })
                .unwrap();
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        wait(&mut o);
        assert!(matches!(o.stage, Stage::Music { found: Some(3), .. }), "a.mp3, b.flac and sub/c.ogg");
        assert!(!o.animating());
        // type on: counted again once you pause, and a folder that isn't there says so
        key(&mut o, KeyCode::Char('x'));
        assert!(o.counting, "a new count is due");
        wait(&mut o);
        assert!(matches!(o.stage, Stage::Music { found: None, .. }));
    }

    /// During the tour esc ends it, except while a terminal has the keyboard (Claude Code and vim need esc):
    /// then esc goes to the program and the card's button just says "end tour".
    #[test]
    fn onboard_tour_esc_leaves_terminals_alone() {
        let mut o = Onboard::tour("oriel", &Probe::default(), "ctrl+space");
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let t = theme::get("oriel");
        let mut shot = |o: &mut Onboard| -> String {
            term.draw(|f| {
                let a = f.area();
                o.draw(f, a, &t, 0.0)
            })
            .unwrap();
            let b = term.backend().buffer();
            (0..b.area.height).map(|y| (0..b.area.width).map(|x| b[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
        };
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let in_term = Probe { in_terminal: true, ..Probe::default() };
        o.check(&in_term);
        let s = shot(&mut o);
        assert!(s.contains(" end tour ") && !s.contains("esc  end tour"), "{s}");
        let (used, out) = o.key(esc, &in_term);
        assert!(!used && matches!(out, Out::None), "the terminal gets esc");
        // back in one of oriel's own panes: esc ends it
        o.check(&Probe::default());
        assert!(shot(&mut o).contains("esc  end tour"));
        assert!(matches!(o.key(esc, &Probe::default()), (true, Out::Finished)));
    }
}
