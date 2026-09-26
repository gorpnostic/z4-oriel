//! Where you left off: your own tabs (names, splits, what ran in each pane and in which folder), the app or tab
//! you were in, the sidebar, and the chat and note you had open. Kept in session.json in oriel's data folder,
//! written as it changes and on quit, and read back at start when the config says startup = "last" (the default).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Session {
    /// The app tab you were in ("ai", "music"...), or None for one of your own tabs (then `tab`).
    pub app: Option<String>,
    /// Which of your tabs you were in (0 = the first), when `app` is None.
    pub tab: Option<usize>,
    pub sidebar: bool,
    pub tabs: Vec<Tab>,
    /// The chat open in the chat app, and the note open in notes.
    pub chat: Option<String>,
    pub note: Option<String>,
}

impl Default for Session {
    fn default() -> Self {
        Session { app: None, tab: None, sidebar: true, tabs: vec![], chat: None, note: None }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Tab {
    /// A name you gave it (None = named after its focused pane).
    #[serde(default)]
    pub name: Option<String>,
    pub root: Node,
    /// The focused pane: its place among the tab's panes in reading order.
    #[serde(default)]
    pub focus: usize,
}

/// A tab's split tree, with what to reopen in each pane.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Node {
    /// `kind` is the panes::open name ("terminal", "claude", "files", "home"...), `cwd` the folder it worked in,
    /// `id` what was open inside it (a chat's id).
    Leaf {
        kind: String,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        id: Option<String>,
    },
    /// `right` = side by side, anything else stacked.
    Split { right: bool, ratio: f32, a: Box<Node>, b: Box<Node> },
}

fn path() -> PathBuf {
    crate::config::data_dir().join("session.json")
}

/// The saved session, if there is one (tests never read the real one).
pub fn load() -> Option<Session> {
    if cfg!(test) {
        return None;
    }
    load_from(&path())
}

pub fn load_from(file: &Path) -> Option<Session> {
    serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()
}

/// Write it out (tests never touch the real one). Written whole through a temp file, so a crash mid-write
/// leaves the last good one.
pub fn save(json: &str) {
    if cfg!(test) {
        return;
    }
    save_to(&path(), json);
}

pub fn save_to(file: &Path, json: &str) {
    if let Some(d) = file.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let tmp = file.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, file).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_file_round_trip() {
        let d = std::path::absolute("target/test-scratch/shell").unwrap();
        let f = d.join("session-round-trip.json");
        let _ = std::fs::remove_file(&f);
        assert!(load_from(&f).is_none(), "no file, no session");
        let s = Session {
            app: None,
            tab: Some(1),
            sidebar: false,
            tabs: vec![Tab {
                name: Some("work".into()),
                root: Node::Split {
                    right: true,
                    ratio: 0.4,
                    a: Box::new(Node::Leaf { kind: "claude".into(), cwd: Some("proj".into()), id: None }),
                    b: Box::new(Node::Leaf { kind: "terminal".into(), cwd: None, id: None }),
                },
                focus: 1,
            }],
            chat: Some("c1".into()),
            note: None,
        };
        save_to(&f, &serde_json::to_string(&s).unwrap());
        assert_eq!(load_from(&f), Some(s));
        // an older or hand-edited file with fields missing still loads, with the defaults
        std::fs::write(&f, r#"{"tabs": [{"root": {"leaf": {"kind": "home"}}}]}"#).unwrap();
        let s = load_from(&f).unwrap();
        assert!(s.sidebar && s.tabs.len() == 1 && s.tabs[0].focus == 0);
        // a torn one is just no session
        std::fs::write(&f, r#"{"tabs": [{"ro"#).unwrap();
        assert!(load_from(&f).is_none());
        assert!(std::fs::read_dir(&d).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with("session-round-trip.tmp")));
    }
}
