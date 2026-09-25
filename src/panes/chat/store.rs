//! Saved chats: one JSON file per chat in <data dir>/oriel/chats. Old files (plain `content`, the legacy `steps`
//! work log) keep loading; agent replies also save `parts`, the ordered transcript of text and activity.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Msg {
    pub role: String,
    /// The reply's text (all text parts joined): what /save, /note and follow-up prompts use.
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The muted line under a reply: duration, tokens, cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Legacy work log from older chats: (label, state, detail). Still rendered, never written by new replies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<(String, String, Option<String>)>,
    /// What a coding agent did, in order: text, thinking, tool calls, the todo list. Empty for plain chat replies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<Part>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Part {
    Text {
        text: String,
    },
    Thinking {
        #[serde(default)]
        text: String,
        #[serde(default)]
        tokens: u64,
    },
    Tool(Tool),
    Todos {
        items: Vec<Todo>,
    },
}

/// One tool call: "Update src/app.rs · +3 -1", its diff or output, and (for subagents) the calls it made.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Tool {
    pub id: String,
    /// The tool's own name: Read, Edit, Bash, command_execution…
    pub name: String,
    /// What the transcript calls it: Read, Update, Write, Bash, Grep, Agent…
    #[serde(default)]
    pub label: String,
    /// Short: a file path, a command, a pattern, a url.
    #[serde(default)]
    pub target: String,
    /// running | done | error | stopped
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ms: u64,
    /// "120 lines", "7 matches", "+3 -1", "exit 1".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    /// Diff or output lines, encoded by agent::line (kind char, line number, tab, text).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub body: Vec<String>,
    /// Set on a subagent's own calls: the Agent/Task call they belong to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Tool>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Todo {
    pub text: String,
    /// The "-ing" form shown while it's in progress ("Writing tests").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub active: String,
    /// pending | in_progress | completed
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Chat {
    pub id: String,
    pub title: String,
    pub created: f64,
    pub updated: f64,
    pub messages: Vec<Msg>,
    /// Which AI: "claude", "codex", "ollama", "openai", "anthropic".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Folder the CLI agents work in for this chat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Per-provider scratch (CLI session ids).
    #[serde(default)]
    pub state: serde_json::Map<String, Value>,
    /// Anything else an older version wrote, kept so files round-trip.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

pub fn dir() -> PathBuf {
    crate::config::data_dir().join("chats")
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
        return; // empty chats vanish
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

/// All chats, newest first.
pub fn load_all() -> Vec<Chat> {
    let mut out: Vec<Chat> = std::fs::read_dir(dir())
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

/// Sidebar groups: today, yesterday, previous 7 days, older.
pub fn bucket(ts: f64, now_ts: f64, utc_offset: i64) -> &'static str {
    let day = |t: f64| ((t as i64 + utc_offset).div_euclid(86400)) as i64;
    match day(now_ts) - day(ts) {
        i64::MIN..=0 => "today",
        1 => "yesterday",
        2..=6 => "previous 7 days",
        _ => "older",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_store_old_files_load() {
        // an older file: legacy steps, an unknown provider, extra keys
        let old = r#"{"id":"a1","title":"t","created":1,"updated":2,"provider":"someai","mystery":7,
            "messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"yo","model":"someai",
            "steps":[["searched","done",null]]}]}"#;
        let c: Chat = serde_json::from_str(old).unwrap();
        assert_eq!(c.messages[1].steps.len(), 1);
        assert!(c.messages[1].parts.is_empty());
        assert_eq!(c.extra.get("mystery").and_then(|v| v.as_u64()), Some(7));
        // and the new shape round-trips
        let mut m = Msg { role: "assistant".into(), content: "done".into(), ..Default::default() };
        m.parts.push(Part::Text { text: "doing it".into() });
        m.parts.push(Part::Tool(Tool { id: "t1".into(), name: "Edit".into(), label: "Update".into(), target: "a.rs".into(), status: "done".into(), ..Default::default() }));
        m.parts.push(Part::Todos { items: vec![Todo { text: "x".into(), status: "pending".into(), ..Default::default() }] });
        m.parts.push(Part::Thinking { text: String::new(), tokens: 40 });
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""kind":"tool""#), "{s}");
        let back: Msg = serde_json::from_str(&s).unwrap();
        assert_eq!(back.parts, m.parts);
    }
}
