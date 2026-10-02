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
    /// On a reply: the chat's CLI sessions as they were before it started, so regenerating it can go back to them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_before: Option<serde_json::Map<String, Value>>,
    /// What the reply cost in dollars, when the AI says (Claude Code does): the chat adds them up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// On a coding agent's reply: the checkpoint of its folder taken just before it started (ckpt.rs), which
    /// /diff, /undo and /rewind go back to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ckpt: Option<String>,
    /// How full the context was on the reply's last step: (input-side tokens, the model's window).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx: Option<(u64, u64)>,
    /// What running the check after it said ("✓ checked 14:02 · cargo test"), from /verify or /check: it replaces
    /// the badge worked out from its tool calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<String>,
}

impl Msg {
    /// The reply's cost: its own number, else the "$0.136" an older version wrote into the note.
    pub fn cost(&self) -> f64 {
        self.cost_usd.unwrap_or_else(|| {
            let note = self.note.as_deref().unwrap_or("");
            note.split(" · ").find_map(|x| x.strip_prefix('$').and_then(|n| n.trim().parse::<f64>().ok())).unwrap_or(0.0)
        })
    }
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
    /// A message you queued while the agent worked, at the point it read it.
    User {
        text: String,
    },
    /// Something that happened to the run itself: the conversation was compacted, a usage limit was hit.
    Mark {
        text: String,
    },
    /// A part this version can't read (a newer oriel's, after a rollback): kept as it was, so the chat still loads
    /// and saving it doesn't lose the part. Not drawn.
    #[serde(untagged)]
    Other(Value),
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
    /// When it started running (live only: drives the "· 12s" on a long command).
    #[serde(skip)]
    pub since: Since,
}

/// A running call's start time. Live only, so it never makes a saved transcript differ from the live one.
#[derive(Clone, Copy, Debug, Default)]
pub struct Since(pub Option<std::time::Instant>);

impl PartialEq for Since {
    fn eq(&self, _: &Self) -> bool {
        true
    }
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
    // tests keep their chats to themselves (unless they picked a data folder, as the README screenshots do); files
    // an earlier run left behind are cleared once
    #[cfg(test)]
    if std::env::var_os("ORIEL_DATA_DIR").is_none() {
        let d = std::path::absolute("target/test-scratch/chat/chats").unwrap_or_default();
        static SWEEP: std::sync::Once = std::sync::Once::new();
        SWEEP.call_once(|| {
            let old = |e: &std::fs::DirEntry| e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().unwrap_or_default() > std::time::Duration::from_secs(600));
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                if old(&e) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        });
        return d;
    }
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

/// A chat's file name: its id, made safe (an id read from a damaged or planted file can hold anything, `../x`
/// included, and must never name a file outside the chats folder).
fn file_name(id: &str) -> String {
    let safe: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    format!("{}.json", if safe.is_empty() { "chat" } else { &safe })
}

/// Save a chat (written to a temp file, then swapped in). Err says why it couldn't be, and leaves nothing behind.
pub fn save(c: &mut Chat) -> Result<(), String> {
    if c.messages.is_empty() {
        return Ok(()); // empty chats vanish
    }
    let d = dir();
    let _ = std::fs::create_dir_all(&d);
    c.updated = now();
    let name = file_name(&c.id);
    let path = d.join(&name);
    let tmp = d.join(format!("{name}.tmp"));
    let s = serde_json::to_string_pretty(c).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, s).map_err(|e| format!("{}: {e}", d.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

/// Delete chat `id`: every file in the chats folder that holds it, whatever the file is called (a copied or synced
/// "abc (1).json" too), and nothing outside the folder.
pub fn delete(id: &str) {
    #[derive(Deserialize)]
    struct Id {
        id: String,
    }
    let _ = std::fs::remove_file(dir().join(file_name(id)));
    for e in std::fs::read_dir(dir()).into_iter().flatten().flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "json") && std::fs::read_to_string(&p).ok().and_then(|s| serde_json::from_str::<Id>(&s).ok()).is_some_and(|x| x.id == id) {
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// All chats, newest first (a chat in two files, e.g. a synced copy, once: its newest).
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
    let mut seen = std::collections::HashSet::new();
    out.retain(|c| seen.insert(c.id.clone()));
    out
}

pub fn title_from(text: &str) -> String {
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= 34 { t } else { format!("{}…", t.chars().take(33).collect::<String>().trim_end()) }
}

/// Sidebar groups: today, yesterday, previous 7 days, older.
pub fn bucket(ts: f64, now_ts: f64, utc_offset: i64) -> &'static str {
    // (a hand-edited file can say 1e300: saturate instead of overflowing)
    let day = |t: f64| (t as i64).saturating_add(utc_offset).div_euclid(86400);
    match day(now_ts).saturating_sub(day(ts)) {
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
