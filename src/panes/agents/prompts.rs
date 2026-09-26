//! Saved prompts: standing instructions you'd otherwise type into every goal ("keep changes small, run cargo test,
//! update the README"). One file, `prompts.toml` in oriel's data folder, shared by agents (ctrl+t in a goal or
//! prompt field) and chat (`/p <name>`):
//!
//!     [[prompt]]
//!     name = "careful"
//!     text = "keep changes small, run cargo test, update the README"

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Prompt {
    pub name: String,
    pub text: String,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct File {
    prompt: Vec<Prompt>,
}

#[cfg(test)]
thread_local! {
    /// Tests: where prompts live on this thread (never the real data folder).
    pub static TEST_PATH: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Where they're kept.
pub fn path() -> PathBuf {
    #[cfg(test)]
    return TEST_PATH.with(|p| p.borrow().clone()).unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-scratch").join("no-prompts.toml"));
    #[cfg(not(test))]
    return crate::config::data_dir().join("prompts.toml");
}

pub fn load() -> Vec<Prompt> {
    load_from(&path())
}

/// Every prompt with a name and some text, in file order. A missing or broken file is no prompts.
pub fn load_from(p: &Path) -> Vec<Prompt> {
    let f: File = std::fs::read_to_string(p).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default();
    f.prompt.into_iter().filter(|x| !x.name.trim().is_empty() && !x.text.trim().is_empty()).collect()
}

pub fn save_to(p: &Path, prompts: &[Prompt]) -> Result<(), String> {
    let s = toml::to_string_pretty(&File { prompt: prompts.to_vec() }).map_err(|e| e.to_string())?;
    super::store::write_atomic(p, s.as_bytes()).map_err(|e| format!("couldn't save {}: {e}", p.display()))
}

/// The one called `name` (any case).
pub fn find<'a>(prompts: &'a [Prompt], name: &str) -> Option<&'a Prompt> {
    prompts.iter().find(|p| p.name.eq_ignore_ascii_case(name.trim()))
}

/// A name for text you're saving: its first few words.
pub fn name_for(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().take(4).collect();
    let n: String = words.join(" ").chars().filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-').collect();
    if n.trim().is_empty() { "prompt".into() } else { n.trim().to_lowercase() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_prompts_round_trip() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-scratch").join(format!("agents-prompts-{}", std::process::id()));
        let p = dir.join("prompts.toml");
        assert!(load_from(&p).is_empty(), "no file, no prompts");
        let v = vec![Prompt { name: "careful".into(), text: "keep changes small\nrun cargo test".into() }, Prompt { name: "".into(), text: "nameless".into() }];
        save_to(&p, &v).unwrap();
        let back = load_from(&p);
        assert_eq!(back, vec![v[0].clone()], "nameless ones are skipped");
        assert_eq!(find(&back, " CAREFUL ").map(|x| x.text.as_str()), Some("keep changes small\nrun cargo test"));
        std::fs::write(&p, "not [valid toml").unwrap();
        assert!(load_from(&p).is_empty());
        assert_eq!(name_for("Keep changes small, run cargo test"), "keep changes small run");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
