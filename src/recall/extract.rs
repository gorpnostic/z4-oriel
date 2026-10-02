//! The words out of every kind of session transcript, for the search index (recall.rs). Kept: your prompts, the
//! replies, one line per tool call ("Bash\tcargo test") and the first lines of a call that failed. Left out: tool
//! output, file contents, thinking, the CLIs' own boilerplate, and anything inside `<private>…</private>`.
//!
//! An extract is plain text: each turn starts with a line `\x1e<kind> <unix secs>` (you, ai or tool), then its
//! text. The record separator never appears in the text itself, so a pasted diff's `@@` can't be mistaken for one.

use crate::panes::ais::util::parse_iso;
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// The longest a single turn gets in an extract (a pasted log shouldn't dwarf the conversation).
const TURN_MAX: usize = 8000;
/// The first lines of a failed tool call are kept: "how did we fix that error" needs the error.
const ERROR_MAX: usize = 300;
const MARK: char = '\u{1e}';

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    You,
    Ai,
    /// "label\ttarget", then the start of its error output if it failed
    Tool,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Kind::You => "you",
            Kind::Ai => "ai",
            Kind::Tool => "tool",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Turn {
    pub kind: Kind,
    /// unix seconds, 0 = unknown
    pub t: i64,
    pub text: String,
}

/// One session, read.
#[derive(Default, Debug)]
pub struct Doc {
    pub title: String,
    /// The CLI's own session id (claude --resume / codex exec resume), or the oriel chat's id.
    pub session: String,
    pub cwd: String,
    pub started: i64,
    pub updated: i64,
    pub turns: Vec<Turn>,
}

impl Doc {
    /// Add a turn: cleaned, empty ones dropped, back-to-back replies joined into one.
    fn push(&mut self, kind: Kind, t: i64, text: &str) {
        let text = clean(text);
        if text.is_empty() {
            return;
        }
        self.seen(t);
        if kind == Kind::Ai {
            if let Some(last) = self.turns.last_mut().filter(|l| l.kind == Kind::Ai) {
                if last.text.len() < TURN_MAX {
                    last.text.push_str("\n\n");
                    last.text.push_str(&text);
                    cap(&mut last.text, TURN_MAX);
                }
                return;
            }
        }
        self.turns.push(Turn { kind, t, text });
    }

    fn seen(&mut self, t: i64) {
        if t > 0 {
            self.started = if self.started == 0 { t } else { self.started.min(t) };
            self.updated = self.updated.max(t);
        }
    }

    /// Your prompts in it.
    pub fn prompts(&self) -> usize {
        self.turns.iter().filter(|t| t.kind == Kind::You).count()
    }

    /// The title a session gets when its CLI didn't name it: your first prompt, cut short.
    fn default_title(&mut self) {
        if self.title.trim().is_empty() {
            let first = self.turns.iter().find(|t| t.kind == Kind::You).map(|t| t.text.as_str()).unwrap_or("");
            self.title = short(first, 60);
        }
        self.title = short(&self.title, 80);
    }
}

/// One line, at most `n` characters: "fix the flaky login test…".
pub fn short(s: &str, n: usize) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= n { one } else { format!("{}…", one.chars().take(n - 1).collect::<String>().trim_end()) }
}

fn cap(s: &mut String, n: usize) {
    if s.len() > n {
        let mut cut = n;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push('…');
    }
}

// ------------------------------------------------------------------ cleaning

/// Drop `<private>` spans (an unclosed one hides the rest), the CLIs' injected blocks, and stray separators.
pub fn clean(s: &str) -> String {
    let mut s = strip_private(s);
    for tag in ["system-reminder", "local-command-stdout", "local-command-stderr", "local-command-caveat", "command-message"] {
        s = strip_block(&s, tag);
    }
    // a slash command in Claude's transcript: keep the command and its arguments, not the markup
    for tag in ["command-name", "command-args"] {
        s = s.replace(&format!("<{tag}>"), "").replace(&format!("</{tag}>"), " ");
    }
    let mut s = s.replace(MARK, " ").replace("\r\n", "\n").trim().to_string();
    cap(&mut s, TURN_MAX);
    s
}

fn strip_private(s: &str) -> String {
    let low = s.to_ascii_lowercase();
    if !low.contains("<private>") {
        return s.to_string();
    }
    let mut out = String::new();
    let mut i = 0;
    while let Some(a) = low[i..].find("<private>").map(|a| a + i) {
        out.push_str(&s[i..a]);
        match low[a..].find("</private>") {
            Some(b) => i = a + b + "</private>".len(),
            None => return out, // never closed: nothing after it is kept
        }
        // "flaky <private>…</private> please" reads "flaky please"
        if out.ends_with(' ') && s[i..].starts_with(' ') {
            out.pop();
        }
    }
    out.push_str(&s[i..]);
    out
}

fn strip_block(s: &str, tag: &str) -> String {
    let (open, close) = (format!("<{tag}"), format!("</{tag}>"));
    if !s.contains(&open) {
        return s.to_string();
    }
    let mut out = String::new();
    let mut i = 0;
    while let Some(a) = s[i..].find(&open).map(|a| a + i) {
        out.push_str(&s[i..a]);
        match s[a..].find(&close) {
            Some(b) => i = a + b + close.len(),
            None => return out,
        }
    }
    out.push_str(&s[i..]);
    out
}

// ------------------------------------------------------------------ the extract file

pub fn write(turns: &[Turn]) -> String {
    let mut out = String::new();
    for t in turns {
        out.push(MARK);
        out.push_str(&format!("{} {}\n", t.kind.word(), t.t));
        out.push_str(&t.text);
        out.push('\n');
    }
    out
}

pub fn parse(s: &str) -> Vec<Turn> {
    let mut out: Vec<Turn> = vec![];
    for line in s.lines() {
        if let Some(head) = line.strip_prefix(MARK) {
            let mut it = head.split(' ');
            let kind = match it.next() {
                Some("you") => Kind::You,
                Some("tool") => Kind::Tool,
                _ => Kind::Ai,
            };
            let t = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
            out.push(Turn { kind, t, text: String::new() });
        } else if let Some(cur) = out.last_mut() {
            if !cur.text.is_empty() {
                cur.text.push('\n');
            }
            cur.text.push_str(line);
        }
    }
    out
}

/// Is this line of an extract a turn header?
pub fn is_mark(line: &str) -> bool {
    line.starts_with(MARK)
}

/// The kind a header line starts.
pub fn mark_kind(line: &str) -> Option<Kind> {
    let head = line.strip_prefix(MARK)?;
    Some(match head.split(' ').next() {
        Some("you") => Kind::You,
        Some("tool") => Kind::Tool,
        _ => Kind::Ai,
    })
}

// ------------------------------------------------------------------ the formats

/// Each line of a JSONL file, parsed, skipping the ones `keep` rules out before parsing (they're huge).
fn jsonl(path: &Path, keep: impl Fn(&[u8]) -> bool, mut f: impl FnMut(&Value)) -> Option<()> {
    let file = std::fs::File::open(path).ok()?;
    let mut r = BufReader::with_capacity(1 << 20, file);
    let mut line = Vec::with_capacity(1 << 16);
    loop {
        line.clear();
        if r.read_until(b'\n', &mut line).ok()? == 0 {
            break;
        }
        if !keep(&line) {
            continue;
        }
        if let Ok(v) = serde_json::from_slice::<Value>(&line) {
            f(&v);
        }
    }
    Some(())
}

fn has(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn ts(v: &Value) -> i64 {
    match v {
        Value::String(s) => parse_iso(s).unwrap_or(0),
        Value::Number(n) => {
            let n = n.as_f64().unwrap_or(0.0);
            if n > 1e11 { (n / 1000.0) as i64 } else { n as i64 }
        }
        _ => 0,
    }
}

/// The text of a message's content: a string, or the text blocks of a list.
fn text_of(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter(|b| matches!(b["type"].as_str(), None | Some("text" | "input_text" | "output_text")))
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Claude Code: `~/.claude/projects/<slug>/<session>.jsonl`.
pub fn claude(path: &Path) -> Option<Doc> {
    let mut d = Doc::default();
    let (mut summary, mut named) = (String::new(), String::new());
    // tool_use id -> its turn, so a failed result can add its error to the right call
    let mut calls: std::collections::HashMap<String, usize> = Default::default();
    let keep = |l: &[u8]| has(l, b"\"user\"") || has(l, b"\"assistant\"") || has(l, b"\"summary\"") || has(l, b"itle\"");
    jsonl(path, keep, |v| {
        match v["type"].as_str().unwrap_or("") {
            "summary" => {
                if let Some(s) = v["summary"].as_str() {
                    summary = s.to_string();
                }
            }
            "custom-title" => {
                if let Some(s) = v["customTitle"].as_str() {
                    named = s.to_string();
                }
            }
            ty @ ("user" | "assistant") => {
                if v["isMeta"].as_bool() == Some(true) || v["isSidechain"].as_bool() == Some(true) {
                    return;
                }
                let t = ts(&v["timestamp"]);
                if d.cwd.is_empty() {
                    d.cwd = v["cwd"].as_str().unwrap_or("").to_string();
                }
                if d.session.is_empty() {
                    d.session = v["sessionId"].as_str().unwrap_or("").to_string();
                }
                let who = if ty == "user" { Kind::You } else { Kind::Ai };
                let content = &v["message"]["content"];
                if let Value::String(s) = content {
                    if !s.starts_with("[Request interrupted") {
                        d.push(who, t, s);
                    }
                    return;
                }
                for b in content.as_array().into_iter().flatten() {
                    match b["type"].as_str().unwrap_or("") {
                        "text" => {
                            let s = b["text"].as_str().unwrap_or("");
                            if !s.starts_with("[Request interrupted") {
                                d.push(who, t, s);
                            }
                        }
                        "tool_use" => {
                            let name = b["name"].as_str().unwrap_or("");
                            if let Some((label, target)) = crate::panes::chat::tool_brief(name, &b["input"], Path::new(&d.cwd)) {
                                let n = d.turns.len();
                                d.push(Kind::Tool, t, &format!("{label}\t{target}"));
                                // (a call that cleans to nothing isn't kept, so there's no turn to point at)
                                if let Some(id) = b["id"].as_str().filter(|_| d.turns.len() > n) {
                                    calls.insert(id.to_string(), n);
                                }
                            }
                        }
                        "tool_result" if b["is_error"].as_bool() == Some(true) => {
                            let err = short(&text_of(&b["content"]), ERROR_MAX);
                            if let Some(turn) = b["tool_use_id"].as_str().and_then(|id| calls.get(id)).and_then(|&i| d.turns.get_mut(i)) {
                                if !err.is_empty() && !turn.text.contains('\n') {
                                    turn.text.push('\n');
                                    turn.text.push_str(&clean(&err));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    })?;
    d.title = if !named.is_empty() { named } else { summary };
    d.default_title();
    Some(d)
}

/// What Codex puts in the conversation that you didn't type.
fn codex_boilerplate(s: &str) -> bool {
    let s = s.trim_start();
    ["<environment_context>", "<user_instructions>", "<permissions", "<INSTRUCTIONS>", "# AGENTS.md instructions", "<developer"].iter().any(|p| s.starts_with(p))
}

/// Codex: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (older files have the items without the wrapper).
pub fn codex(path: &Path) -> Option<Doc> {
    let mut d = Doc::default();
    jsonl(path, |_| true, |v| {
        let t = ts(&v["timestamp"]);
        let (ty, p) = match v["type"].as_str() {
            Some("response_item") => (v["payload"]["type"].as_str().unwrap_or(""), &v["payload"]),
            Some(x) if v.get("payload").is_some() => (x, &v["payload"]),
            Some(x) => (x, v),
            None => ("", v),
        };
        match ty {
            "session_meta" => {
                d.session = p["id"].as_str().unwrap_or("").to_string();
                d.cwd = p["cwd"].as_str().unwrap_or("").to_string();
                d.seen(ts(&p["timestamp"]));
            }
            "turn_context" if d.cwd.is_empty() => d.cwd = p["cwd"].as_str().unwrap_or("").to_string(),
            "message" => {
                let who = match p["role"].as_str() {
                    Some("user") => Kind::You,
                    Some("assistant") => Kind::Ai,
                    _ => return, // developer / system instructions
                };
                let s = text_of(&p["content"]);
                if who == Kind::You && codex_boilerplate(&s) {
                    return;
                }
                d.push(who, t, &s);
            }
            "function_call" | "custom_tool_call" | "local_shell_call" => {
                let name = p["name"].as_str().unwrap_or("shell");
                let args: Value = match &p["arguments"] {
                    Value::String(s) => serde_json::from_str(s).unwrap_or(Value::String(s.clone())),
                    other => other.clone(),
                };
                let args = if args.is_null() { p["input"].clone() } else { args };
                let cmd = match (&args["command"], &p["action"]["command"]) {
                    (Value::Array(a), _) | (_, Value::Array(a)) => a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" "),
                    (Value::String(s), _) => s.clone(),
                    _ => args.as_str().map(|s| s.lines().next().unwrap_or("").to_string()).unwrap_or_default(),
                };
                let cmd = unwrap_shell(&cmd);
                let label = if matches!(name, "shell" | "exec_command" | "local_shell" | "shell_command") || ty == "local_shell_call" { "Bash" } else { name };
                d.push(Kind::Tool, t, &format!("{label}\t{}", short(&cmd, 160)));
            }
            _ => {}
        }
    })?;
    d.default_title();
    Some(d)
}

/// `bash -lc "cargo test"` → `cargo test`.
fn unwrap_shell(c: &str) -> String {
    for pre in ["bash -lc ", "bash -c ", "sh -c ", "powershell.exe -Command ", "pwsh -Command ", "cmd /c "] {
        if let Some(rest) = c.strip_prefix(pre) {
            return rest.trim_matches(['"', '\'']).to_string();
        }
    }
    c.to_string()
}

/// Kimi Code (`~/.kimi-code/sessions/**/wire.jsonl`) and anything shaped like it: any object with a user or
/// assistant `role` and a `content`. The format isn't documented, so this reads it generously.
pub fn generic(path: &Path) -> Option<Doc> {
    fn find<'a>(v: &'a Value, depth: usize) -> Option<&'a Value> {
        match v {
            Value::Object(m) => {
                if matches!(m.get("role").and_then(|r| r.as_str()), Some("user" | "assistant")) && m.contains_key("content") {
                    return Some(v);
                }
                if depth == 0 {
                    return None;
                }
                m.values().find_map(|c| find(c, depth - 1))
            }
            Value::Array(a) if depth > 0 => a.iter().find_map(|c| find(c, depth - 1)),
            _ => None,
        }
    }
    let mut d = Doc::default();
    jsonl(path, |l| has(l, b"\"role\""), |v| {
        let Some(m) = find(v, 4) else { return };
        let t = ["timestamp", "time", "created_at", "ts"].iter().map(|k| ts(&v[*k])).find(|t| *t > 0).unwrap_or(0);
        if d.cwd.is_empty() {
            d.cwd = v["cwd"].as_str().or(v["work_dir"].as_str()).unwrap_or("").to_string();
        }
        let who = if m["role"] == "user" { Kind::You } else { Kind::Ai };
        d.push(who, t, &text_of(&m["content"]));
    })?;
    if d.session.is_empty() {
        // .../session_<id>/agents/main/wire.jsonl
        d.session = path.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).find(|c| c.starts_with("session") && c != "sessions").unwrap_or_default();
    }
    d.default_title();
    Some(d)
}

/// oriel's own chats: `<data dir>/chats/<id>.json`.
pub fn oriel(path: &Path) -> Option<Doc> {
    use crate::panes::chat::store::{Chat, Part};
    let c: Chat = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let mut d = Doc { title: c.title.clone(), session: c.id.clone(), cwd: c.cwd.clone().unwrap_or_default(), ..Default::default() };
    let (created, updated) = (c.created as i64, c.updated as i64);
    for (i, m) in c.messages.iter().enumerate() {
        let t = if i == 0 { created } else { 0 };
        if m.role == "user" {
            d.push(Kind::You, t, &m.content);
            continue;
        }
        if m.parts.is_empty() {
            d.push(Kind::Ai, t, &m.content);
        }
        for p in &m.parts {
            match p {
                Part::Text { text } => d.push(Kind::Ai, t, text),
                Part::User { text } => d.push(Kind::You, t, text),
                Part::Tool(tool) => {
                    let mut s = format!("{}\t{}", tool.label, tool.target);
                    if tool.status == "error" && !tool.summary.is_empty() {
                        s.push('\n');
                        s.push_str(&tool.summary);
                    }
                    d.push(Kind::Tool, t, &s);
                }
                _ => {}
            }
        }
    }
    d.seen(created);
    d.seen(updated);
    d.default_title();
    Some(d)
}
