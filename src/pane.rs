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
    Tick,
    /// The clipboard image grab (alt+v / ctrl+v) finished: paste these paths into the pane, or, if the clipboard
    /// had no image, hand it the key it was pressed with.
    Clipboard(PaneId, Option<Vec<String>>, crossterm::event::KeyEvent),
    /// A newer oriel is out (the daily check at start).
    UpdateAvailable(String),
}

/// What a pane can ask the app to do.
pub enum Action {
    /// Open a new pane. `Split` splits the asking pane; `Tab` opens it in a new tab; `Replace` swaps the asking
    /// pane out for it (the launcher does this).
    Open(Box<dyn Pane>, Place),
    Close,
    Notify(String),
    SetTheme(String),
    /// Switch to this theme quietly (the themes app, as you edit): saved, no toast.
    ApplyTheme(String),
    /// Open the palette with this text already typed (e.g. "theme " = the theme picker with live preview).
    Palette(String),
    /// Switch to an app's tab.
    GotoApp(&'static str),
    /// Send a key to an app's pane without switching to it (/play → the music app), opening it if needed.
    AppKey(&'static str, char),
    ToggleSidebar,
    ToggleIcons,
    /// Open a pane in a new tab of its own, named `name` and remembered by `tag` (e.g. an orchestrator task id),
    /// so it can be focused or closed later. Doesn't switch to it unless `focus` is true.
    OpenTagged { pane: Box<dyn Pane>, tag: String, name: String, focus: bool },
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
    /// True if `poll` should keep ticking while the pane isn't on screen (the calendar's reminders).
    fn ticks_hidden(&self) -> bool {
        false
    }
    /// A terminal pane is dead once its process exits; the app then closes it.
    fn alive(&self) -> bool {
        true
    }
    /// True if the pane wants raw keys (terminal): only the global Alt/prefix bindings are intercepted.
    fn is_terminal(&self) -> bool {
        false
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
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Activity {
    Idle,
    Working,
    /// waiting on you: a permission prompt, a question, (y/n)
    Blocked,
}
