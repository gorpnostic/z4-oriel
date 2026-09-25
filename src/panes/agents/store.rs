//! The orchestrator's data: tasks (tasks.json), the per-task status files hooks write, and `oriel report`.
//! Everything is written atomically (temp file + rename) so a crash or two hooks racing never leave half a file.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Todo,
    Running,
    Blocked,
    Review,
    Done,
}

impl Status {
    /// Board column: TODO · RUNNING (blocked too) · REVIEW · DONE.
    pub fn column(self) -> usize {
        match self {
            Status::Todo => 0,
            Status::Running | Status::Blocked => 1,
            Status::Review => 2,
            Status::Done => 3,
        }
    }
    pub fn active(self) -> bool {
        matches!(self, Status::Running | Status::Blocked)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Task {
    pub id: String,
    /// The main repo's top-level folder.
    pub repo: String,
    pub title: String,
    pub prompt: String,
    /// claude | codex | kimi
    pub agent: String,
    /// Empty = the agent's default.
    pub model: String,
    pub status: Status,
    /// For Done: "merged" / "discarded".
    pub outcome: String,
    pub branch: String,
    /// The branch the task started from (what it merges back into) and its commit then.
    pub base_branch: String,
    pub base_sha: String,
    pub worktree: String,
    pub session_id: String,
    pub transcript_path: String,
    pub cost_usd: f64,
    pub tokens: u64,
    pub added: u64,
    pub removed: u64,
    pub files: u64,
    /// Last thing the agent did ("Edit src/app.rs"), or a progress line of ours ("merging…").
    pub last: String,
    /// What a blocked agent is asking, when known.
    pub question: String,
    pub error: String,
    pub created: i64,
    pub started: i64,
    pub finished: i64,
    /// Follow-up sessions started with `c` (0 = only the first run).
    pub followups: u32,
}

impl Task {
    pub fn tag(&self) -> String {
        format!("agent-task:{}", self.id)
    }
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
#[serde(default)]
pub struct Store {
    /// Repos the board has worked on, most recent first.
    pub repos: Vec<String>,
    pub tasks: Vec<Task>,
}

/// Where the orchestrator keeps things. `agents` = tasks.json + status/, `wt` = the worktrees.
#[derive(Clone, Debug)]
pub struct Paths {
    pub agents: PathBuf,
    pub wt: PathBuf,
}

impl Paths {
    pub fn default() -> Paths {
        let d = crate::config::data_dir();
        Paths { agents: d.join("agents"), wt: d.join("wt") }
    }
    pub fn tasks(&self) -> PathBuf {
        self.agents.join("tasks.json")
    }
    pub fn status_dir(&self) -> PathBuf {
        self.agents.join("status")
    }
    pub fn status(&self, id: &str) -> PathBuf {
        self.status_dir().join(format!("{id}.json"))
    }
}

/// Write `data` to `path` via a temp file in the same folder and a rename (rename replaces on Windows too).
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let tmp = path.with_extension(format!("tmp{}-{nanos}", std::process::id()));
    std::fs::write(&tmp, data)?;
    let r = std::fs::rename(&tmp, path);
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

pub fn load(p: &Paths) -> Store {
    std::fs::read_to_string(p.tasks()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

// ------------------------------------------------------------------ status files

/// What the hooks report about a task, merged across calls.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct StatusFile {
    /// running | blocked | idle | session
    pub state: String,
    /// Unix seconds of the last report.
    pub ts: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub session_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub transcript_path: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub last_tool: String,
    /// A Notification's message (the question, when blocked).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message: String,
}

pub fn read_status(path: &Path) -> Option<StatusFile> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// One-line summary of a tool call from a PreToolUse payload: "Edit app.rs", "Bash cargo test".
pub fn tool_summary(v: &Value) -> String {
    let name = v["tool_name"].as_str().unwrap_or("");
    if name.is_empty() {
        return String::new();
    }
    let inp = &v["tool_input"];
    let arg = ["file_path", "path", "command", "pattern", "url", "description", "prompt"]
        .iter()
        .find_map(|k| inp[*k].as_str())
        .unwrap_or("");
    let arg = if inp["file_path"].is_string() || inp["path"].is_string() {
        Path::new(arg).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or(arg.to_string())
    } else {
        arg.lines().next().unwrap_or("").to_string()
    };
    let s = if arg.is_empty() { name.to_string() } else { format!("{name} {arg}") };
    s.chars().take(120).collect()
}

/// Merge one hook call into the status: `state` from the command line, the rest from the hook's JSON.
pub fn merge_report(mut st: StatusFile, state: &str, hook: &Value, now: i64) -> StatusFile {
    st.ts = now;
    let s = |k: &str| hook[k].as_str().unwrap_or("").to_string();
    if !s("session_id").is_empty() {
        st.session_id = s("session_id");
    }
    if !s("transcript_path").is_empty() {
        st.transcript_path = s("transcript_path");
    }
    match state {
        // a session starting (first run or a follow-up) always has a prompt to work on
        "session" => {
            st.state = "running".into();
            st.message.clear();
        }
        "blocked" => {
            let msg = s("message");
            // "Claude is waiting for your input" = it's done and idle, not stuck on a question
            let idle = hook["notification_type"].as_str() == Some("idle_prompt") || msg.to_lowercase().contains("waiting for your input");
            if idle {
                st.state = "idle".into();
            } else {
                st.state = "blocked".into();
                st.message = msg;
            }
        }
        "running" => {
            st.state = "running".into();
            st.message.clear();
            let t = tool_summary(hook);
            if !t.is_empty() {
                st.last_tool = t;
            } else if hook["hook_event_name"].as_str() == Some("UserPromptSubmit") {
                st.last_tool = "thinking…".into();
            }
        }
        other => {
            st.state = other.to_string();
            st.message.clear();
        }
    }
    st
}

pub fn now() -> i64 {
    crate::panes::files::clock::now_secs()
}

/// `oriel report --task <id> --state <s>` with the hook JSON on stdin. Must be quick and silent: it runs inside
/// the agent's hooks, and anything printed could end up in the agent's context.
pub fn report(paths: &Paths, args: &[String], stdin: Option<&str>) -> i32 {
    let mut task = std::env::var("ORIEL_TASK_ID").unwrap_or_default();
    let mut state = String::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--task" => {
                task = args.get(i + 1).cloned().unwrap_or_default();
                i += 1;
            }
            "--state" => {
                state = args.get(i + 1).cloned().unwrap_or_default();
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    // ids are ours (alphanumeric); refuse anything that could climb out of the status folder
    if task.is_empty() || !task.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return 0;
    }
    if !matches!(state.as_str(), "running" | "blocked" | "idle" | "session") {
        return 0;
    }
    let hook: Value = stdin.and_then(|s| serde_json::from_str(s.trim()).ok()).unwrap_or(Value::Null);
    let path = paths.status(&task);
    let st = merge_report(read_status(&path).unwrap_or_default(), &state, &hook, now());
    if let Ok(j) = serde_json::to_vec(&st) {
        let _ = write_atomic(&path, &j);
    }
    0
}

/// Read stdin without ever hanging: a hook always pipes JSON, but a person running `oriel report` by hand
/// has a terminal there. Give up after a moment.
pub fn read_stdin_quick() -> Option<String> {
    use std::io::{IsTerminal, Read};
    if std::io::stdin().is_terminal() {
        return None;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::stdin().take(4 << 20).read_to_string(&mut s);
        let _ = tx.send(s);
    });
    rx.recv_timeout(std::time::Duration::from_millis(1500)).ok()
}

// ------------------------------------------------------------------ ids + names

/// A short unique id: base36 milliseconds, bumped past any id already taken.
pub fn new_id(taken: &[Task]) -> String {
    let mut ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    loop {
        let id = base36(ms);
        if !taken.iter().any(|t| t.id == id) {
            return id;
        }
        ms += 1;
    }
}

fn base36(mut n: u64) -> String {
    const D: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut v = vec![];
    while n > 0 {
        v.push(D[(n % 36) as usize]);
        n /= 36;
    }
    if v.is_empty() {
        v.push(b'0');
    }
    v.reverse();
    String::from_utf8(v).unwrap()
}

/// Branch/folder-safe name: "Fix the PTY resize!" + id -> "fix-the-pty-resize-k3f9".
pub fn slug(title: &str, id: &str) -> String {
    let mut s = String::new();
    for c in title.chars().flat_map(|c| c.to_lowercase()) {
        if c.is_ascii_alphanumeric() {
            s.push(c);
        } else if !s.ends_with('-') && !s.is_empty() {
            s.push('-');
        }
        if s.len() >= 28 {
            break;
        }
    }
    let s = s.trim_matches('-');
    let tail: String = id.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    if s.is_empty() { format!("task-{tail}") } else { format!("{s}-{tail}") }
}
