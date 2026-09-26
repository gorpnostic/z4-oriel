//! The contract every pane (terminal, chat, music, ...) implements. The app owns layout, frames and focus;
//! a pane only draws its inside and handles the input it is given.

use crate::config::Config;
use crate::theme::Theme;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{Frame, layout::Rect};
use std::sync::mpsc::Sender;
use std::time::Duration;

pub use crate::layout::PaneId;

/// Things the event loop wakes up for.
pub enum Event {
    Input(crossterm::event::Event),
    /// A pane's background work produced something: redraw (and call `Pane::poll`).
    Wake(PaneId),
    ThemeFilesChanged,
    /// config.toml changed on disk (a hand edit, another oriel window, or our own save).
    ConfigFileChanged,
    Tick,
    /// The clipboard image grab (alt+v / ctrl+v) finished: paste these paths into the pane, or, if the clipboard
    /// had no image, hand it the key it was pressed with.
    Clipboard(PaneId, Option<Vec<String>>, crossterm::event::KeyEvent),
    /// A newer oriel is out (the daily check at start).
    UpdateAvailable(String),
    /// From a background watcher (memory, AI usage).
    Alert(crate::alerts::Kind, String),
}

/// What a pane can ask the app to do.
pub enum Action {
    /// Open a new pane. `Split` splits the asking pane; `Tab` opens it in a new tab; `Replace` swaps the asking
    /// pane out for it (the launcher does this).
    Open(Box<dyn Pane>, Place),
    Close,
    Notify(String),
    /// Something for the event center (and a toast, and a desktop notification if you're elsewhere).
    Alert(crate::alerts::Kind, String),
    SetTheme(String),
    /// Switch to this theme quietly (the themes app, as you edit): saved, no toast.
    ApplyTheme(String),
    /// Show this theme without saving it (settings' live preview; the previous one is sent back on esc).
    PreviewTheme(String),
    /// Change the config (use `cx.edit_config`). The app is its only writer: it re-reads config.toml, applies
    /// this, saves, and calls `config_changed` on every pane.
    Config(Box<dyn FnOnce(&mut Config) + Send>),
    /// Open the palette with this text already typed (e.g. "theme " = the theme picker with live preview).
    Palette(String),
    /// Switch to an app's tab.
    GotoApp(&'static str),
    /// Send a key to an app's pane without switching to it (/play → the music app), opening it if needed.
    AppKey(&'static str, char),
    /// Paste text into an app's pane (opening it if needed), and switch to it.
    AppPaste(&'static str, String),
    ToggleSidebar,
    ToggleIcons,
    /// Replay the tour (settings › tools).
    Tour,
    /// Open a pane in a new tab of its own, named `name` and remembered by `tag` (e.g. an orchestrator task id),
    /// so it can be focused or closed later. Doesn't switch to it unless `focus` is true.
    OpenTagged { pane: Box<dyn Pane>, tag: String, name: String, focus: bool },
    /// Switch to the tab holding this pane (the alerts app jumps back to where something happened).
    FocusPane(u64),
    /// FocusPane, or this app if that pane has closed since.
    FocusPaneOr(u64, &'static str),
    /// Answer one of a pane's open items (Pane::open_now) by its key: the alerts app's 1-9, y / n; Go switches
    /// to the pane first, then lets it show the item.
    Respond(u64, String, crate::alerts::Reply),
    /// Switch to the tab holding the pane opened with this tag (no-op if it's gone).
    FocusTag(String),
    /// Close the pane opened with this tag.
    CloseTag(String),
    Quit,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Place {
    SplitRight,
    SplitDown,
    Split, // along the longer side
    Tab,
    Replace,
}

/// Everything a pane gets while handling input or rendering.
pub struct Cx<'a> {
    pub id: PaneId,
    pub theme: &'a Theme,
    pub config: &'a Config,
    pub tx: &'a Sender<Event>,
    pub actions: &'a mut Vec<Action>,
    pub focused: bool,
    /// Seconds since start, for animations.
    pub time: f64,
}

impl Cx<'_> {
    pub fn act(&mut self, a: Action) {
        self.actions.push(a);
    }
    pub fn notify(&mut self, s: impl Into<String>) {
        self.actions.push(Action::Notify(s.into()));
    }
    /// A toast that's also kept in the event center.
    pub fn alert(&mut self, kind: crate::alerts::Kind, s: impl Into<String>) {
        self.actions.push(Action::Alert(kind, s.into()));
    }
    /// Remember a setting: `f` changes only what it's about, the app saves it and every pane hears.
    /// Never write config.toml yourself (a stale copy would undo what others saved).
    pub fn edit_config(&mut self, f: impl FnOnce(&mut Config) + Send + 'static) {
        self.actions.push(Action::Config(Box::new(f)));
    }
    /// A Sender + id a background thread can use to wake the UI: `waker.wake()`.
    pub fn waker(&self) -> Waker {
        Waker { id: self.id, tx: self.tx.clone() }
    }
}

#[derive(Clone)]
pub struct Waker {
    pub id: PaneId,
    pub tx: Sender<Event>,
}

impl Waker {
    pub fn wake(&self) {
        let _ = self.tx.send(Event::Wake(self.id));
    }
}

pub trait Pane {
    /// Shown in the pane's top border.
    fn title(&self) -> String;
    /// Shown in the bottom-right of the border (status, counts). Optional.
    fn subtitle(&self) -> Option<String> {
        None
    }
    /// Nerd Font icon for tabs/titles (see ui::icon).
    fn icon(&self) -> &'static str {
        "term"
    }
    /// Draw inside `area` (already inside the frame).
    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx);
    /// Return true if the key was used. Unused keys fall through to the app's own bindings.
    fn key(&mut self, key: KeyEvent, cx: &mut Cx) -> bool;
    /// Mouse event with coordinates relative to the whole screen; `area` is the pane's inner rect.
    fn mouse(&mut self, _ev: MouseEvent, _area: Rect, _cx: &mut Cx) {}
    fn paste(&mut self, _text: &str, _cx: &mut Cx) {}
    /// Called after an Event::Wake for this pane, and on each tick if `tick_every` says so.
    fn poll(&mut self, _cx: &mut Cx) {}
    /// If Some, the app calls `poll` at least this often while the pane is visible.
    fn tick_every(&self) -> Option<Duration> {
        None
    }
    /// The config changed (a setting saved from any pane, or config.toml edited by hand): pick up what applies
    /// to an open pane.
    fn config_changed(&mut self, _cfg: &Config) {}
    /// When the pane's program has ended: what to tell the event center, if anything ("Firefox installed").
    fn exit_note(&mut self) -> Option<(crate::alerts::Kind, String)> {
        None
    }
    /// True if `poll` should keep ticking while the pane isn't on screen (the calendar's reminders).
    fn ticks_hidden(&self) -> bool {
        false
    }
    /// A terminal pane is dead once its process exits; the app then closes it.
    fn alive(&self) -> bool {
        true
    }
    /// True if the pane wants raw keys (terminal): only the global Alt/prefix bindings are intercepted, and the
    /// F-keys unless `wants_fkeys`.
    fn is_terminal(&self) -> bool {
        false
    }
    /// A full-screen program (htop, mc, vim) is running: bare F-keys go to it instead of switching apps.
    fn wants_fkeys(&self) -> bool {
        false
    }
    /// How many agents closing this pane would stop mid-work (a live reply, running workers). Closing or quitting
    /// asks first while it's above 0. Default: a coding agent that's working or waiting on you.
    fn busy(&self) -> usize {
        matches!(self.activity(), Some(Activity::Working | Activity::Blocked)) as usize
    }
    /// ctrl+v here should try the clipboard for an image first (pasted as a file path): chat, agents, and
    /// terminals running a coding agent. Everywhere else ctrl+v goes straight to the program.
    fn wants_images(&self) -> bool {
        self.activity().is_some()
    }
    /// Whatever the mouse hovering changes in this pane's drawing (e.g. the highlighted row). Mouse moves that
    /// change neither this nor the app's own hover targets skip the redraw.
    fn hover(&self) -> usize {
        0
    }
    /// This app's own section of the left sidebar, under the app list (nest style): the chat list, playlists,
    /// places, sort options... `area` is the space left in the sidebar. Only called for app tabs.
    fn side(&mut self, _f: &mut Frame, _area: Rect, _cx: &mut Cx) {}
    /// Mouse event inside the side section (screen coordinates; `area` is the side section's rect).
    fn side_mouse(&mut self, _ev: MouseEvent, _area: Rect, _cx: &mut Cx) {}
    /// Tagged panes the app still has open, activity included — lets the orchestrator follow its agents' tabs.
    /// Called with the live list after every event batch; default: ignore.
    fn tagged_panes(&mut self, _live: &[(String, Option<Activity>)]) {}
    /// Short live status shown next to the app's name in the sidebar, e.g. "▶ song" or "12%".
    fn badge(&self) -> Option<String> {
        None
    }
    /// For panes running a coding agent (Claude Code, Codex...): what it's doing. The app turns Working→Idle
    /// into "done" for tabs you aren't looking at, and shows it all as status dots in the sidebar (herdr style).
    fn activity(&self) -> Option<Activity> {
        None
    }
    /// True when the program inside wants mouse events itself (so right-click goes to it, not our menu).
    fn wants_mouse(&self) -> bool {
        false
    }
    /// The folder this pane works in (files' folder, a chat's /cwd, a task's worktree, a shell's folder): a
    /// terminal opened from here (alt n) starts there, and it's saved with the session.
    fn cwd(&self) -> Option<std::path::PathBuf> {
        None
    }
    /// The panes::open name that makes this pane again when oriel restarts ("terminal", "claude", "files"...).
    /// None = it isn't brought back.
    fn reopen(&self) -> Option<&'static str> {
        None
    }
    /// What to reopen inside it next time (the open chat's or note's id)...
    fn resume_id(&self) -> Option<String> {
        None
    }
    /// ...and reopening it, at start.
    fn resume(&mut self, _id: &str) {}
    /// Something the palette can send to chat from here: (what it is, the text), e.g. ("this note", its text).
    fn for_chat(&self) -> Option<(String, String)> {
        None
    }
    /// What's waiting on you here right now (a question, an approval, a stuck task, work to review), for the
    /// alerts app's "open now" list. Asked before every draw, so it must be cheap. A pane whose activity() is
    /// Blocked and lists nothing gets a plain "needs you" row from the app.
    fn open_now(&self) -> Vec<crate::alerts::Open> {
        vec![]
    }
    /// Answer one of them (the alerts app: 1-9, y / n), or show it (enter; the app has switched here already).
    /// False = it's gone or has changed since it was listed.
    fn respond(&mut self, _key: &str, _r: crate::alerts::Reply, _cx: &mut Cx) -> bool {
        false
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Activity {
    Idle,
    Working,
    /// waiting on you: a permission prompt, a question, (y/n)
    Blocked,
}
