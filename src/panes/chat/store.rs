//! Saved chats: one JSON file per chat, same format as nest's (~/.wren/chats), so old chats carry over.
//! oriel keeps its own copy in <data dir>/oriel/chats; nest's chats are copied in once, never moved.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Msg {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The muted line under a reply: tokens, speed, sources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Smart-mode work log: (label, state, detail).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<(String, String, Option<String>)>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Chat {
    pub id: String,
    pub title: String,
    pub created: f64,
    pub updated: f64,
    pub messages: Vec<Msg>,
    /// Which AI: "wren", "claude", "codex", "ollama", "openai", "anthropic".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Folder the CLI agents work in for this chat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Per-provider scratch (CLI session ids), like nest's chat["state"].
    #[serde(default)]
    pub state: serde_json::Map<String, Value>,
    /// Anything else nest wrote, kept so files round-trip.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

pub fn dir() -> PathBuf {
    crate::config::data_dir().join("chats")
}

fn nest_dir() -> Option<PathBuf> {
    if std::env::var_os("ORIEL_DATA_DIR").is_some() {
        return None; // a separate profile starts clean
    }
    Some(dirs::home_dir()?.join(".wren").join("chats"))
}

impl Chat {
    pub fn new(provider: &str) -> Chat {
        let t = now();
        let id = format!("{:012x}", (t * 1e6) as u64 ^ (std::process::id() as u64) << 20);
        Chat {
            id: id[id.len() - 12..].to_string(),
            title: "New chat".into(),
            created: t,
            updated: t,
            messages: vec![],
            provider: Some(provider.to_string()),
            model: None,
            cwd: None,
            state: Default::default(),
            extra: Default::default(),
        }
    }
}

pub fn save(c: &mut Chat) {
    if c.messages.is_empty() {
        return; // empty chats vanish, like nest
    }
    let d = dir();
    let _ = std::fs::create_dir_all(&d);
    c.updated = now();
    let path = d.join(format!("{}.json", c.id));
    let tmp = d.join(format!("{}.json.tmp", c.id));
    if let Ok(s) = serde_json::to_string_pretty(c) {
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

pub fn delete(id: &str) {
    let _ = std::fs::remove_file(dir().join(format!("{id}.json")));
}

/// All chats, newest first. On the first run nest's chats are copied in.
pub fn load_all() -> Vec<Chat> {
    let d = dir();
    let empty = std::fs::read_dir(&d).map(|mut r| r.next().is_none()).unwrap_or(true);
    if empty {
        if let Some(n) = nest_dir() {
            if let Ok(rd) = std::fs::read_dir(&n) {
                let _ = std::fs::create_dir_all(&d);
                for e in rd.flatten() {
                    let p = e.path();
                    if p.extension().map(|x| x == "json").unwrap_or(false) {
                        let _ = std::fs::copy(&p, d.join(e.file_name()));
                    }
                }
            }
        }
    }
    let mut out: Vec<Chat> = std::fs::read_dir(&d)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
                .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                .filter_map(|s| serde_json::from_str::<Chat>(&s).ok())
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| b.updated.partial_cmp(&a.updated).unwrap_or(std::cmp::Ordering::Equal));
    out
}

pub fn title_from(text: &str) -> String {
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= 34 { t } else { format!("{}…", t.chars().take(33).collect::<String>().trim_end()) }
}

/// nest's sidebar groups: today, yesterday, previous 7 days, older.
pub fn bucket(ts: f64, now_ts: f64, utc_offset: i64) -> &'static str {
    let day = |t: f64| ((t as i64 + utc_offset).div_euclid(86400)) as i64;
    match day(now_ts) - day(ts) {
        i64::MIN..=0 => "today",
        1 => "yesterday",
        2..=6 => "previous 7 days",
        _ => "older",
    }
}
