//! Coding-agent transcripts. Turns Claude Code's `stream-json` and Codex's `exec --json` into the chat's
//! activity (tool calls with targets, diffs, command output, the todo list, subagents, thinking). Runs on the
//! provider thread; the pane only receives finished `Ev`s.

use super::providers::Ev;
use super::store::{Todo, Tool};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Most body lines a tool keeps (the head and the tail survive, with "… N more lines" between).
const BODY_CAP: usize = 400;

// ------------------------------------------------------------------ body lines
/// One diff/output line: a kind char (`+` added, `-` removed, ` ` context, `@` hunk gap, `>` output,
/// `!` error output, `#` plain text, `…` elided), an optional line number, a tab, the text.
pub fn line(kind: char, num: Option<u64>, text: &str) -> String {
    let t = clean(text);
    match num {
        Some(n) => format!("{kind}{n}\t{t}"),
        None => format!("{kind}\t{t}"),
    }
}

/// (kind, line number, text) of an encoded line.
pub fn split_line(s: &str) -> (char, &str, &str) {
    let k = s.chars().next().unwrap_or(' ');
    let rest = &s[k.len_utf8()..];
    match rest.split_once('\t') {
        Some((n, t)) => (k, n, t),
        None => (k, "", rest),
    }
}

/// No ANSI escapes, carriage returns or tabs in anything we draw.
fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\x1b' => match it.peek() {
                Some('[') => {
                    it.next();
                    while let Some(x) = it.next() {
                        if ('@'..='~').contains(&x) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(x) = it.next() {
                        if x == '\x07' {
                            break;
                        }
                        if x == '\x1b' {
                            it.next();
                            break;
                        }
                    }
                }
                _ => {
                    it.next();
                }
            },
            '\t' => out.push_str("    "),
            '\r' => {}
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

fn cap(mut v: Vec<String>) -> Vec<String> {
    if v.len() > BODY_CAP {
        let tail = v.split_off(v.len() - 100);
        let dropped = v.len() - 300;
        v.truncate(300);
        v.push(line('…', None, &format!("{dropped} more lines")));
        v.extend(tail);
    }
    v
}

fn output(s: &str, kind: char) -> Vec<String> {
    let s = s.trim_end_matches(['\n', '\r', ' ']);
    if s.trim().is_empty() {
        return vec![];
    }
    cap(s.lines().map(|l| line(kind, None, l)).collect())
}

pub fn counts(body: &[String]) -> (usize, usize) {
    let add = body.iter().filter(|l| l.starts_with('+')).count();
    let del = body.iter().filter(|l| l.starts_with('-')).count();
    (add, del)
}

/// Line diff of two texts (LCS), trimmed to 3 lines of context around the changes.
pub fn diff(old: &str, new: &str) -> Vec<String> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let mut raw: Vec<(char, &str)> = vec![];
    if a.len() * b.len() > 250_000 {
        raw.extend(a.iter().map(|l| ('-', *l)));
        raw.extend(b.iter().map(|l| ('+', *l)));
    } else {
        let (n, m) = (a.len(), b.len());
        let mut dp = vec![0u32; (n + 1) * (m + 1)];
        let at = |i: usize, j: usize| i * (m + 1) + j;
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                dp[at(i, j)] = if a[i] == b[j] { dp[at(i + 1, j + 1)] + 1 } else { dp[at(i + 1, j)].max(dp[at(i, j + 1)]) };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n && j < m {
            if a[i] == b[j] {
                raw.push((' ', a[i]));
                i += 1;
                j += 1;
            } else if dp[at(i + 1, j)] >= dp[at(i, j + 1)] {
                raw.push(('-', a[i]));
                i += 1;
            } else {
                raw.push(('+', b[j]));
                j += 1;
            }
        }
        raw.extend(a[i..].iter().map(|l| ('-', *l)));
        raw.extend(b[j..].iter().map(|l| ('+', *l)));
    }
    // keep 3 lines of context around changes
    let near: Vec<bool> = (0..raw.len())
        .map(|k| raw[k].0 != ' ' || raw[k.saturating_sub(3)..(k + 4).min(raw.len())].iter().any(|x| x.0 != ' '))
        .collect();
    let mut out = vec![];
    let mut gap = false;
    for (k, (kind, text)) in raw.iter().enumerate() {
        if near[k] {
            if gap && !out.is_empty() {
                out.push(line('@', None, ""));
            }
            gap = false;
            out.push(line(*kind, None, text));
        } else {
            gap = true;
        }
    }
    cap(out)
}

/// Claude Code's `structuredPatch` (hunks of " "/"-"/"+" lines) with real line numbers.
fn patch(p: &Value) -> Vec<String> {
    let mut out = vec![];
    for (hi, h) in p.as_array().into_iter().flatten().enumerate() {
        if hi > 0 {
            out.push(line('@', None, ""));
        }
        let (mut o, mut n) = (h["oldStart"].as_u64().unwrap_or(1), h["newStart"].as_u64().unwrap_or(1));
        for l in h["lines"].as_array().into_iter().flatten().filter_map(|l| l.as_str()) {
            let k = l.chars().next().unwrap_or(' ');
            let text = l.get(k.len_utf8()..).unwrap_or("");
            match k {
                '-' => {
                    out.push(line('-', Some(o), text));
                    o += 1;
                }
                '+' => {
                    out.push(line('+', Some(n), text));
                    n += 1;
                }
                '\\' => {}
                _ => {
                    out.push(line(' ', Some(n), text));
                    o += 1;
                    n += 1;
                }
            }
        }
    }
    cap(out)
}

/// A unified diff as text (`@@ -1,3 +1,3 @@` headers), e.g. from Codex.
fn unified(s: &str) -> Vec<String> {
    let mut out = vec![];
    let (mut o, mut n) = (0u64, 0u64);
    for l in s.lines() {
        if l.starts_with("+++") || l.starts_with("---") || l.starts_with("diff ") || l.starts_with("index ") {
            continue;
        }
        if let Some(h) = l.strip_prefix("@@") {
            // @@ -a,b +c,d @@
            let num = |p: &str| p.trim_start_matches(['-', '+']).split(',').next().and_then(|x| x.parse::<u64>().ok()).unwrap_or(1);
            let mut it = h.split_whitespace();
            o = it.next().map(num).unwrap_or(1);
            n = it.next().map(num).unwrap_or(1);
            if !out.is_empty() {
                out.push(line('@', None, ""));
            }
            continue;
        }
        let k = l.chars().next().unwrap_or(' ');
        let text = l.get(k.len_utf8()..).unwrap_or("");
        match k {
            '-' => {
                out.push(line('-', Some(o), text));
                o += 1;
            }
            '+' => {
                out.push(line('+', Some(n), text));
                n += 1;
            }
            '\\' => {}
            _ => {
                out.push(line(' ', Some(n), text));
                o += 1;
                n += 1;
            }
        }
    }
    cap(out)
}

// ------------------------------------------------------------------ describing a call
/// A path relative to the agent's folder when it's inside it.
pub fn rel(cwd: &Path, p: &str) -> String {
    let c = cwd.to_string_lossy();
    let c = c.trim_end_matches(['/', '\\']);
    let norm = |s: &str| s.replace('\\', "/");
    let (pn, cn) = (norm(p), norm(c));
    if !cn.is_empty() && pn.len() > cn.len() + 1 && pn.as_bytes()[..cn.len()].eq_ignore_ascii_case(cn.as_bytes()) && pn.as_bytes()[cn.len()] == b'/' {
        return p[cn.len() + 1..].to_string();
    }
    p.to_string()
}

/// A command with the agent's own folder taken out of it ("cat C:\work\demo\a.txt" -> "cat a.txt").
fn strip_cwd(cwd: &Path, s: &str) -> String {
    let c = cwd.to_string_lossy().trim_end_matches(['/', '\\']).to_string();
    if c.len() < 3 {
        return s.to_string();
    }
    let fwd = c.replace('\\', "/");
    let needles = [format!("{c}\\"), format!("{fwd}/"), format!("{}\\\\", c.replace('\\', "\\\\"))];
    let mut out = s.to_string();
    for n in needles {
        let n = n.to_ascii_lowercase();
        // ASCII lowercasing keeps byte offsets, so positions found in the copy are valid in `out`
        while let Some(i) = out.to_ascii_lowercase().find(&n) {
            out.replace_range(i..i + n.len(), "");
        }
    }
    out
}

fn first_line(s: &str) -> String {
    let l = s.trim().lines().next().unwrap_or("").trim().to_string();
    if s.trim().lines().count() > 1 { format!("{l} …") } else { l }
}

pub fn label(name: &str) -> String {
    match name {
        "Edit" | "MultiEdit" => "Update".into(),
        "Task" | "Agent" => "Agent".into(),
        "WebFetch" => "Fetch".into(),
        "WebSearch" | "web_search" => "Web search".into(),
        "NotebookEdit" => "Notebook".into(),
        "LS" => "List".into(),
        "command_execution" => "Run".into(),
        "file_change" => "Update".into(),
        n if n.starts_with("mcp__") => {
            let mut it = n.trim_start_matches("mcp__").splitn(2, "__");
            let (s, t) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
            format!("{s} · {t}")
        }
        n => n.to_string(),
    }
}

/// The short "what": a path, a command, a pattern, a url…
pub fn target(name: &str, input: &Value, cwd: &Path) -> String {
    let s = |k: &str| input[k].as_str().unwrap_or("").to_string();
    match name {
        "Read" | "Write" | "Edit" | "MultiEdit" => rel(cwd, &s("file_path")),
        "NotebookEdit" => rel(cwd, &s("notebook_path")),
        "Bash" | "PowerShell" | "BashOutput" => first_line(&strip_cwd(cwd, &s("command"))),
        "Grep" => {
            let mut t = format!("'{}'", s("pattern"));
            let p = if !s("path").is_empty() { rel(cwd, &s("path")) } else { s("glob") };
            if !p.is_empty() {
                t.push_str(&format!(" in {p}"));
            }
            t
        }
        "Glob" => {
            let mut t = s("pattern");
            if !s("path").is_empty() {
                t.push_str(&format!(" in {}", rel(cwd, &s("path"))));
            }
            t
        }
        "LS" => rel(cwd, &s("path")),
        "WebFetch" => s("url"),
        "WebSearch" => s("query"),
        "Task" | "Agent" => s("description"),
        _ => {
            // first short string argument
            for k in ["file_path", "path", "command", "url", "query", "pattern", "description", "name"] {
                if let Some(v) = input[k].as_str() {
                    return first_line(v);
                }
            }
            input.as_object().and_then(|o| o.values().find_map(|v| v.as_str())).map(first_line).unwrap_or_default()
        }
    }
}

/// A new call, before it runs: label, target, and for edits the diff.
pub fn describe(id: &str, name: &str, input: &Value, cwd: &Path) -> Tool {
    let mut t = Tool { id: id.into(), name: name.into(), label: label(name), target: target(name, input, cwd), status: "running".into(), ..Default::default() };
    match name {
        "Edit" => t.body = diff(input["old_string"].as_str().unwrap_or(""), input["new_string"].as_str().unwrap_or("")),
        "MultiEdit" => {
            let mut b = vec![];
            for e in input["edits"].as_array().into_iter().flatten() {
                if !b.is_empty() {
                    b.push(line('@', None, ""));
                }
                b.extend(diff(e["old_string"].as_str().unwrap_or(""), e["new_string"].as_str().unwrap_or("")));
            }
            t.body = cap(b);
        }
        "Write" => t.body = cap(input["content"].as_str().unwrap_or("").lines().enumerate().map(|(i, l)| line('+', Some(i as u64 + 1), l)).collect()),
        "NotebookEdit" => t.body = output(input["new_source"].as_str().unwrap_or(""), '+'),
        "Task" | "Agent" => {
            if let Some(a) = input["subagent_type"].as_str() {
                t.summary = a.to_string();
            }
        }
        _ => {}
    }
    if matches!(name, "Edit" | "MultiEdit") {
        let (a, d) = counts(&t.body);
        t.summary = format!("+{a} -{d}");
    }
    t
}

/// The text of a tool_result's `content` (a string or a list of text blocks).
fn result_text(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Fill in a finished call from its tool_result (`text`) and Claude Code's structured `tool_use_result`.
fn finish(t: &mut Tool, text: &str, r: &Value, is_error: bool) {
    t.status = if is_error { "error".into() } else { "done".into() };
    if is_error {
        let msg = text.replace("<tool_use_error>", "").replace("</tool_use_error>", "");
        let mut lines = msg.trim().lines();
        let head = lines.next().unwrap_or("error").trim().to_string();
        // "Exit code 1" leads a failed command's output
        if let Some(code) = head.strip_prefix("Exit code ") {
            t.summary = format!("exit {code}");
            t.body = output(&lines.collect::<Vec<_>>().join("\n"), '!');
        } else {
            t.summary = crate::ui::fit(&head, 80);
            let rest = lines.collect::<Vec<_>>().join("\n");
            if !rest.trim().is_empty() {
                t.body = output(&rest, '!');
            }
        }
        return;
    }
    match t.name.as_str() {
        "Read" => {
            let n = r["file"]["numLines"].as_u64().unwrap_or_else(|| text.lines().count() as u64);
            t.summary = plural(n, "line", "lines");
        }
        "Write" => {
            if r["type"] == "update" {
                let p = patch(&r["structuredPatch"]);
                if !p.is_empty() {
                    t.body = p;
                }
                let (a, d) = counts(&t.body);
                t.summary = format!("+{a} -{d}");
            } else {
                t.summary = plural(t.body.len() as u64, "line", "lines");
            }
        }
        "Edit" | "MultiEdit" => {
            let p = patch(&r["structuredPatch"]);
            if !p.is_empty() {
                t.body = p;
            }
            let (a, d) = counts(&t.body);
            t.summary = format!("+{a} -{d}");
        }
        "Bash" | "PowerShell" => {
            let (so, se) = (r["stdout"].as_str(), r["stderr"].as_str());
            let mut b = match so {
                Some(o) => output(o, '>'),
                None => output(text, '>'),
            };
            if let Some(e) = se {
                b.extend(output(e, '!'));
            }
            t.body = cap(b);
            t.summary = if r["interrupted"].as_bool() == Some(true) {
                "interrupted".into()
            } else if t.body.is_empty() {
                "no output".into()
            } else {
                plural(t.body.len() as u64, "line", "lines")
            };
        }
        "Grep" => {
            let n = match r["mode"].as_str() {
                Some("files_with_matches") => plural(r["numFiles"].as_u64().unwrap_or(0), "file", "files"),
                Some("count") => plural(r["numMatches"].as_u64().or(r["numLines"].as_u64()).unwrap_or(0), "match", "matches"),
                _ => plural(r["numLines"].as_u64().unwrap_or_else(|| text.lines().count() as u64), "match", "matches"),
            };
            t.summary = n;
            let c = r["content"].as_str().map(String::from).unwrap_or_else(|| {
                r["filenames"].as_array().map(|a| a.iter().filter_map(|f| f.as_str()).collect::<Vec<_>>().join("\n")).unwrap_or_else(|| text.to_string())
            });
            t.body = output(&c, '>');
        }
        "Glob" => {
            let files: Vec<&str> = r["filenames"].as_array().map(|a| a.iter().filter_map(|f| f.as_str()).collect()).unwrap_or_else(|| text.lines().collect());
            t.summary = plural(r["numFiles"].as_u64().unwrap_or(files.len() as u64), "file", "files");
            t.body = output(&files.join("\n"), '>');
        }
        "LS" => {
            t.summary = plural(text.lines().filter(|l| l.trim_start().starts_with("- ")).count() as u64, "entry", "entries");
            t.body = output(text, '>');
        }
        "WebFetch" => {
            let bytes = r["bytes"].as_u64().unwrap_or(text.len() as u64);
            t.summary = match r["code"].as_u64() {
                Some(c) => format!("{c} · {}", crate::ui::human_bytes(bytes)),
                None => crate::ui::human_bytes(bytes),
            };
            t.body = output(r["result"].as_str().unwrap_or(text), '#');
        }
        "Task" | "Agent" => {
            if r["isAsync"].as_bool() == Some(true) || r["status"] == "async_launched" {
                t.status = "running".into();
                t.summary = "in the background".into();
                return;
            }
            let answer = r["content"].as_array().map(|a| a.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n")).unwrap_or_else(|| text.to_string());
            t.summary = crate::ui::fit(&first_line(&answer), 70);
            if answer.trim().lines().count() > 1 {
                t.body = output(&answer, '#');
            }
            if let Some(ms) = r["totalDurationMs"].as_u64() {
                t.ms = ms;
            }
        }
        _ => {
            let f = first_line(text);
            if !f.is_empty() {
                t.summary = crate::ui::fit(&f, 60);
            }
            t.body = output(text, '#');
        }
    }
}

pub fn human_tokens(n: u64) -> String {
    if n < 1000 { n.to_string() } else if n < 100_000 { format!("{:.1}k", n as f64 / 1000.0) } else { format!("{}k", n / 1000) }
}

pub fn human_ms(ms: u64) -> String {
    let s = ms / 1000;
    if ms < 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m {:02}s", s / 60, s % 60)
    }
}

// ------------------------------------------------------------------ Claude Code
/// Tools that only manage the todo list (shown as the list, not as calls) or are plumbing.
fn hidden(name: &str) -> bool {
    matches!(name, "TodoWrite" | "TaskCreate" | "TaskUpdate" | "TaskList" | "TaskGet" | "ToolSearch")
}

#[derive(Default)]
pub struct Claude {
    cwd: PathBuf,
    tools: HashMap<String, Tool>,
    started: HashMap<String, u64>,
    /// a streaming tool_use block: (parent, index) -> (id, name, partial json)
    blocks: HashMap<(String, u64), (String, String, String)>,
    todos: Vec<Todo>,
    /// TaskCreate call id -> index in todos (its real id arrives with the result)
    created: HashMap<String, usize>,
    skip: HashSet<String>,
    streamed: bool,
    tokens_done: u64,
    msg_tokens: u64,
    msg_chars: u64,
    think_tokens: u64,
    sent_tokens: u64,
    result_tokens: u64,
    turns: u64,
    cost: f64,
    denied: u64,
    session: String,
    pub model: String,
}

impl Claude {
    pub fn new(cwd: &Path) -> Claude {
        Claude { cwd: cwd.to_path_buf(), ..Default::default() }
    }

    fn emit(&mut self, id: &str, send: &mut dyn FnMut(Ev)) {
        if let Some(t) = self.tools.get(id) {
            send(Ev::Tool(t.clone()));
        }
    }

    fn todos_changed(&mut self, send: &mut dyn FnMut(Ev)) {
        send(Ev::Todos(self.todos.clone()));
    }

    fn usage(&mut self, send: &mut dyn FnMut(Ev)) {
        let live = self.tokens_done + self.msg_tokens.max(self.msg_chars / 4 + self.think_tokens);
        let total = live.max(self.result_tokens);
        if total >= self.sent_tokens + 8 || (total != self.sent_tokens && self.result_tokens > 0) {
            self.sent_tokens = total;
            send(Ev::Usage(total));
        }
    }

    /// A tool_use with its full input: TodoWrite/TaskCreate/TaskUpdate change the todo list, everything else
    /// becomes (or updates) a call in the transcript.
    fn tool_use(&mut self, id: &str, name: &str, input: &Value, parent: Option<&str>, t_ms: u64, send: &mut dyn FnMut(Ev)) {
        match name {
            "TodoWrite" => {
                self.todos = input["todos"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|x| Todo {
                        text: x["content"].as_str().unwrap_or("").into(),
                        active: x["activeForm"].as_str().unwrap_or("").into(),
                        status: x["status"].as_str().unwrap_or("pending").into(),
                        id: x["id"].as_str().unwrap_or("").into(),
                    })
                    .collect();
                self.todos_changed(send);
            }
            "TaskCreate" if !self.created.contains_key(id) => {
                self.created.insert(id.into(), self.todos.len());
                self.todos.push(Todo {
                    text: input["subject"].as_str().unwrap_or("").into(),
                    active: input["activeForm"].as_str().unwrap_or("").into(),
                    status: "pending".into(),
                    id: (self.todos.len() + 1).to_string(),
                });
                self.todos_changed(send);
            }
            "TaskUpdate" => {
                let tid = input["taskId"].as_str().map(String::from).or_else(|| input["taskId"].as_u64().map(|n| n.to_string())).unwrap_or_default();
                if input["status"] == "deleted" {
                    self.todos.retain(|t| t.id != tid);
                } else if let Some(t) = self.todos.iter_mut().find(|t| t.id == tid) {
                    if let Some(s) = input["status"].as_str() {
                        t.status = s.into();
                    }
                    if let Some(s) = input["subject"].as_str() {
                        t.text = s.into();
                    }
                    if let Some(s) = input["activeForm"].as_str() {
                        t.active = s.into();
                    }
                }
                self.todos_changed(send);
            }
            _ => {}
        }
        if hidden(name) {
            self.skip.insert(id.into());
            return;
        }
        let mut t = describe(id, name, input, &self.cwd);
        t.parent = parent.map(String::from);
        if let Some(old) = self.tools.get(id) {
            if old.status != "running" {
                return; // already finished (results can race ahead of repeats)
            }
        }
        self.started.entry(id.into()).or_insert(t_ms);
        self.tools.insert(id.into(), t);
        self.emit(id, send);
    }

    pub fn feed(&mut self, v: &Value, t_ms: u64, send: &mut dyn FnMut(Ev)) -> Result<(), String> {
        if let Some(s) = v["session_id"].as_str() {
            if s != self.session {
                self.session = s.to_string();
                send(Ev::State("claude.session".into(), s.to_string()));
            }
        }
        let parent = v["parent_tool_use_id"].as_str();
        match v["type"].as_str() {
            Some("system") => match v["subtype"].as_str() {
                Some("init") => {
                    self.model = v["model"].as_str().unwrap_or("").to_string();
                    send(Ev::Status(format!("claude code · {}", self.model)));
                }
                // redacted thinking still reports how long it is
                Some("thinking_tokens") => {
                    let n = v["estimated_tokens_delta"].as_u64().unwrap_or(0);
                    self.think_tokens += n;
                    send(Ev::Thinking { text: String::new(), tokens: n });
                    self.usage(send);
                }
                Some("status") => {
                    let s = v["status"].as_str().unwrap_or("");
                    send(Ev::Status(if s == "compacting" { "compacting the conversation".into() } else if s == "requesting" { String::new() } else { s.to_string() }));
                }
                Some("compact_boundary") => {
                    let before = v["compact_metadata"]["pre_tokens"].as_u64().unwrap_or(0);
                    let auto = v["compact_metadata"]["trigger"] == "auto";
                    let mut s = String::from(if auto { "context was full: conversation compacted" } else { "conversation compacted" });
                    if before > 0 {
                        s.push_str(&format!(" (from {} tokens)", human_tokens(before)));
                    }
                    send(Ev::Mark(s));
                }
                Some("api_retry") => {
                    let n = v["attempt"].as_u64().unwrap_or(1);
                    send(Ev::Status(format!("API hiccup, retrying (attempt {n})")));
                }
                Some("task_started" | "task_progress" | "task_notification") => {
                    let Some(id) = v["tool_use_id"].as_str() else { return Ok(()) };
                    let uses = v["usage"]["tool_uses"].as_u64().unwrap_or(0);
                    if let Some(t) = self.tools.get_mut(id) {
                        match v["subtype"].as_str() {
                            Some("task_notification") => {
                                t.status = if v["status"] == "completed" { "done".into() } else { "error".into() };
                                let s = v["summary"].as_str().unwrap_or("");
                                t.summary = crate::ui::fit(&first_line(s), 70);
                                if s.trim().lines().count() > 1 {
                                    t.body = output(s, '#');
                                }
                                t.ms = v["usage"]["duration_ms"].as_u64().unwrap_or(t.ms);
                            }
                            Some("task_progress") => {
                                let d = v["description"].as_str().unwrap_or("");
                                t.summary = format!("{d} · {}", plural(uses, "tool use", "tool uses"));
                            }
                            _ => t.summary = v["subagent_type"].as_str().unwrap_or(&t.summary).to_string(),
                        }
                    }
                    self.emit(id, send);
                }
                _ => {}
            },
            Some("stream_event") => {
                self.streamed = true;
                let e = &v["event"];
                let key = (parent.unwrap_or("").to_string(), e["index"].as_u64().unwrap_or(0));
                match e["type"].as_str() {
                    Some("message_start") if parent.is_none() => {
                        self.msg_tokens = e["message"]["usage"]["output_tokens"].as_u64().unwrap_or(0);
                        self.msg_chars = 0;
                        self.think_tokens = 0;
                    }
                    Some("content_block_start") => {
                        let b = &e["content_block"];
                        match b["type"].as_str() {
                            Some("tool_use") => {
                                let (id, name) = (b["id"].as_str().unwrap_or("").to_string(), b["name"].as_str().unwrap_or("tool").to_string());
                                self.blocks.insert(key, (id.clone(), name.clone(), String::new()));
                                if !hidden(&name) && !self.tools.contains_key(&id) {
                                    let mut t = describe(&id, &name, &Value::Null, &self.cwd);
                                    t.parent = parent.map(String::from);
                                    self.started.insert(id.clone(), t_ms);
                                    self.tools.insert(id.clone(), t);
                                    self.emit(&id, send);
                                }
                            }
                            Some("thinking") if parent.is_none() => send(Ev::Thinking { text: String::new(), tokens: 0 }),
                            _ => {}
                        }
                    }
                    Some("content_block_delta") => {
                        let d = &e["delta"];
                        match d["type"].as_str() {
                            Some("text_delta") if parent.is_none() => {
                                let s = d["text"].as_str().unwrap_or("");
                                self.msg_chars += s.len() as u64;
                                send(Ev::Token(s.to_string()));
                                self.usage(send);
                            }
                            Some("thinking_delta") if parent.is_none() => {
                                // the count comes from the thinking_tokens system events
                                let s = d["thinking"].as_str().unwrap_or("");
                                if !s.is_empty() {
                                    send(Ev::Thinking { text: s.to_string(), tokens: 0 });
                                }
                            }
                            Some("input_json_delta") => {
                                if let Some((id, name, buf)) = self.blocks.get_mut(&key) {
                                    buf.push_str(d["partial_json"].as_str().unwrap_or(""));
                                    self.msg_chars += d["partial_json"].as_str().unwrap_or("").len() as u64;
                                    if hidden(name) {
                                        return Ok(());
                                    }
                                    // show the target as soon as it has streamed in (the path of a file being written...)
                                    let partial = partial_input(buf);
                                    let tgt = target(name, &partial, &self.cwd);
                                    let id = id.clone();
                                    if let Some(t) = self.tools.get_mut(&id) {
                                        if t.status == "running" && t.target != tgt && !tgt.is_empty() {
                                            t.target = tgt;
                                            self.emit(&id, send);
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Some("message_delta") if parent.is_none() => {
                        self.tokens_done += e["usage"]["output_tokens"].as_u64().unwrap_or(self.msg_tokens);
                        self.msg_tokens = 0;
                        self.msg_chars = 0;
                        self.think_tokens = 0;
                        self.usage(send);
                    }
                    _ => {}
                }
            }
            Some("assistant") => {
                for it in v["message"]["content"].as_array().into_iter().flatten() {
                    match it["type"].as_str() {
                        Some("tool_use") => {
                            let (id, name) = (it["id"].as_str().unwrap_or(""), it["name"].as_str().unwrap_or("tool"));
                            self.tool_use(id, name, &it["input"], parent, t_ms, send);
                        }
                        // without partial messages the text only arrives here
                        Some("text") if parent.is_none() && !self.streamed => send(Ev::Token(it["text"].as_str().unwrap_or("").to_string())),
                        Some("thinking") if parent.is_none() && !self.streamed => {
                            let s = it["thinking"].as_str().unwrap_or("");
                            send(Ev::Thinking { text: s.to_string(), tokens: s.len() as u64 / 4 });
                        }
                        _ => {}
                    }
                }
            }
            // --replay-user-messages: a message we sent, at the moment Claude read it (subagents' prompts aside)
            Some("user") if parent.is_none() && v["message"]["content"].is_string() => {
                send(Ev::Steered(v["message"]["content"].as_str().unwrap_or("").to_string()));
            }
            Some("user")
                if parent.is_none()
                    && v["message"]["content"].as_array().is_some_and(|a| !a.is_empty() && a.iter().all(|c| c["type"] == "text")) =>
            {
                let text: Vec<&str> = v["message"]["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect();
                send(Ev::Steered(text.join("\n")));
            }
            Some("rate_limit_event") => {
                let info = &v["rate_limit_info"];
                let resets = info["resetsAt"].as_i64().map(|s| {
                    let l = crate::panes::files::clock::local(s);
                    format!(" · resets {:02}:{:02}", l.hour, l.min)
                });
                match info["status"].as_str() {
                    Some("rejected") => send(Ev::Mark(format!("usage limit reached{}", resets.unwrap_or_default()))),
                    Some("allowed_warning") => send(Ev::Status(format!("close to your usage limit{}", resets.unwrap_or_default()))),
                    _ => {}
                }
            }
            Some("user") => {
                let r = &v["tool_use_result"];
                for it in v["message"]["content"].as_array().into_iter().flatten() {
                    if it["type"] != "tool_result" {
                        continue;
                    }
                    let id = it["tool_use_id"].as_str().unwrap_or("");
                    if self.skip.contains(id) {
                        // TaskCreate's result carries the task's real id
                        if let (Some(&i), Some(tid)) = (self.created.get(id), r["task"]["id"].as_str()) {
                            if let Some(t) = self.todos.get_mut(i) {
                                t.id = tid.to_string();
                            }
                        }
                        continue;
                    }
                    let text = result_text(&it["content"]);
                    let start = self.started.get(id).copied().unwrap_or(t_ms);
                    if let Some(t) = self.tools.get_mut(id) {
                        t.ms = t_ms.saturating_sub(start);
                        finish(t, &text, r, it["is_error"].as_bool() == Some(true));
                    }
                    self.emit(id, send);
                }
            }
            Some("result") => {
                if v["is_error"].as_bool() == Some(true) {
                    return Err(v["result"].as_str().map(String::from).unwrap_or_else(|| format!("Claude Code stopped: {}", v["subtype"].as_str().unwrap_or("error"))));
                }
                self.result_tokens += v["usage"]["output_tokens"].as_u64().unwrap_or(0);
                self.turns += v["num_turns"].as_u64().unwrap_or(0);
                self.cost = self.cost.max(v["total_cost_usd"].as_f64().unwrap_or(0.0));
                self.denied += v["permission_denials"].as_array().map(|a| a.len() as u64).unwrap_or(0);
                self.usage(send);
            }
            _ => {}
        }
        Ok(())
    }

    /// The line under the finished reply: duration · tokens · cost · turns.
    pub fn note(&self, elapsed: Duration) -> String {
        let mut p = vec![human_ms(elapsed.as_millis() as u64), format!("{} tokens", human_tokens(self.result_tokens.max(self.sent_tokens)))];
        if self.cost > 0.0 {
            p.push(format!("${:.3}", self.cost));
        }
        if self.turns > 0 {
            p.push(plural(self.turns, "turn", "turns"));
        }
        if self.denied > 0 {
            p.push(format!("{} denied", self.denied));
        }
        p.push("claude code".into());
        p.join(" · ")
    }
}

/// Best-effort parse of a tool input that's still streaming in: close the open string and braces.
fn partial_input(buf: &str) -> Value {
    if let Ok(v) = serde_json::from_str(buf) {
        return v;
    }
    let (mut in_str, mut esc) = (false, false);
    let mut stack = vec![];
    for c in buf.chars() {
        if in_str {
            match (esc, c) {
                (true, _) => esc = false,
                (false, '\\') => esc = true,
                (false, '"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    let mut s = buf.to_string();
    if esc {
        s.pop();
    }
    if in_str {
        s.push('"');
    }
    let trimmed = s.trim_end();
    if trimmed.ends_with(':') {
        s.push_str("null");
    } else if trimmed.ends_with(',') {
        s = trimmed.trim_end_matches(',').to_string();
    }
    while let Some(c) = stack.pop() {
        s.push(c);
    }
    serde_json::from_str(&s).unwrap_or(Value::Null)
}

// ------------------------------------------------------------------ Codex
#[derive(Default)]
pub struct Codex {
    cwd: PathBuf,
    started: HashMap<String, u64>,
    tokens: u64,
    errors: Vec<String>,
}

/// `"...pwsh.exe" -Command 'Get-Content x'` / `bash -lc 'ls'` -> the command inside.
fn unwrap_shell(c: &str) -> String {
    for flag in [" -Command ", " -lc ", " -c "] {
        if let Some((_, rest)) = c.split_once(flag) {
            let r = rest.trim();
            let r = r.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')).or_else(|| r.strip_prefix('"').and_then(|x| x.strip_suffix('"'))).unwrap_or(r);
            return r.replace("\\\\", "\\");
        }
    }
    c.to_string()
}

impl Codex {
    pub fn new(cwd: &Path) -> Codex {
        Codex { cwd: cwd.to_path_buf(), ..Default::default() }
    }

    pub fn feed(&mut self, v: &Value, t_ms: u64, send: &mut dyn FnMut(Ev)) -> Result<(), String> {
        let ty = v["type"].as_str().unwrap_or("");
        match ty {
            "thread.started" => {
                if let Some(t) = v["thread_id"].as_str() {
                    send(Ev::State("codex.thread".into(), t.to_string()));
                }
            }
            "item.started" | "item.updated" | "item.completed" => {
                let it = &v["item"];
                let id = it["id"].as_str().unwrap_or("").to_string();
                let done = ty == "item.completed";
                let start = *self.started.entry(id.clone()).or_insert(t_ms);
                let status = |ok: bool| if !done { "running".to_string() } else if ok { "done".into() } else { "error".into() };
                match it["type"].as_str() {
                    Some("agent_message") if done => send(Ev::Token(format!("{}\n\n", it["text"].as_str().unwrap_or("").trim_end()))),
                    Some("reasoning") if done => {
                        let s = it["text"].as_str().unwrap_or("");
                        send(Ev::Thinking { text: s.to_string(), tokens: s.len() as u64 / 4 });
                    }
                    Some("command_execution") => {
                        let cmd = strip_cwd(&self.cwd, &unwrap_shell(it["command"].as_str().unwrap_or("")));
                        let code = it["exit_code"].as_i64();
                        let ok = it["status"] != "failed" && code.unwrap_or(0) == 0;
                        let mut t = Tool { id, name: "command_execution".into(), label: label("command_execution"), target: first_line(&cmd), status: status(ok), ..Default::default() };
                        if !done {
                            t.body = output(it["aggregated_output"].as_str().unwrap_or(""), '>');
                        }
                        if done {
                            t.ms = t_ms.saturating_sub(start);
                            t.body = output(it["aggregated_output"].as_str().unwrap_or(""), if ok { '>' } else { '!' });
                            t.summary = match code {
                                Some(c) if c != 0 => format!("exit {c}"),
                                _ if t.body.is_empty() => "no output".into(),
                                _ => plural(t.body.len() as u64, "line", "lines"),
                            };
                        }
                        send(Ev::Tool(t));
                    }
                    Some("file_change") => {
                        let changes = it["changes"].as_array().cloned().unwrap_or_default();
                        let kinds: Vec<&str> = changes.iter().filter_map(|c| c["kind"].as_str()).collect();
                        let lbl = if kinds.iter().all(|k| *k == "add") {
                            "Write"
                        } else if kinds.iter().all(|k| *k == "delete") {
                            "Delete"
                        } else {
                            "Update"
                        };
                        let files: Vec<String> = changes.iter().filter_map(|c| c["path"].as_str()).map(|p| rel(&self.cwd, p)).collect();
                        let ok = it["status"] != "failed";
                        let mut t = Tool { id, name: "file_change".into(), label: lbl.into(), target: files.join(", "), status: status(ok), ..Default::default() };
                        let mut body = vec![];
                        for c in &changes {
                            if let Some(d) = c["diff"].as_str().or(c["unified_diff"].as_str()) {
                                if !body.is_empty() {
                                    body.push(line('@', None, ""));
                                }
                                body.extend(unified(d));
                            }
                        }
                        t.body = cap(body);
                        if done {
                            t.ms = t_ms.saturating_sub(start);
                            let (a, d) = counts(&t.body);
                            t.summary = if a + d > 0 {
                                format!("+{a} -{d}")
                            } else if lbl == "Write" {
                                if files.len() == 1 { "new file".into() } else { format!("{} new files", files.len()) }
                            } else {
                                String::new()
                            };
                        }
                        send(Ev::Tool(t));
                    }
                    Some("mcp_tool_call") => {
                        let name = format!("mcp__{}__{}", it["server"].as_str().unwrap_or("mcp"), it["tool"].as_str().unwrap_or("tool"));
                        let ok = it["status"] != "failed" && it["error"].is_null();
                        let mut t = Tool { id, label: label(&name), target: target(&name, &it["arguments"], &self.cwd), name, status: status(ok), ..Default::default() };
                        if done {
                            t.ms = t_ms.saturating_sub(start);
                            let txt = it["result"]["content"].as_array().map(|a| a.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n")).unwrap_or_default();
                            t.summary = crate::ui::fit(&first_line(it["error"]["message"].as_str().unwrap_or(&txt)), 60);
                            t.body = output(&txt, '#');
                        }
                        send(Ev::Tool(t));
                    }
                    Some("web_search") => {
                        send(Ev::Tool(Tool { id, name: "web_search".into(), label: label("web_search"), target: it["query"].as_str().unwrap_or("").into(), status: status(true), ms: if done { t_ms - start } else { 0 }, ..Default::default() }));
                    }
                    Some("todo_list") => {
                        let items: Vec<&Value> = it["items"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
                        let first_open = items.iter().position(|x| x["completed"].as_bool() != Some(true));
                        send(Ev::Todos(
                            items
                                .iter()
                                .enumerate()
                                .map(|(i, x)| Todo {
                                    text: x["text"].as_str().unwrap_or("").into(),
                                    status: if x["completed"].as_bool() == Some(true) {
                                        "completed".into()
                                    } else if Some(i) == first_open && !done {
                                        "in_progress".into()
                                    } else {
                                        "pending".into()
                                    },
                                    ..Default::default()
                                })
                                .collect(),
                        ));
                    }
                    Some("error") if done => self.errors.push(it["message"].as_str().unwrap_or("error").to_string()),
                    _ => {}
                }
            }
            "turn.completed" => {
                self.tokens += v["usage"]["output_tokens"].as_u64().unwrap_or(0);
                send(Ev::Usage(self.tokens));
            }
            "error" | "turn.failed" => {
                return Err(v["message"].as_str().or(v["error"]["message"].as_str()).map(String::from).unwrap_or_else(|| v["error"].to_string()));
            }
            _ => {}
        }
        Ok(())
    }

    pub fn note(&self, elapsed: Duration) -> String {
        let mut p = vec![human_ms(elapsed.as_millis() as u64), format!("{} tokens", human_tokens(self.tokens))];
        if let Some(e) = self.errors.last() {
            p.push(format!("⚠ {}", crate::ui::fit(e, 60)));
        }
        p.push("codex".into());
        p.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_agent_diff() {
        let d = diff("a\nb\nc\n", "a\nB\nc\n");
        let kinds: String = d.iter().map(|l| l.chars().next().unwrap()).collect();
        assert_eq!(kinds, " -+ ");
        // far-apart changes get a gap marker, context trimmed to 3 lines
        let old: String = (0..30).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l2\n", "L2\n").replace("l25\n", "L25\n");
        let d = diff(&old, &new);
        assert!(d.iter().any(|l| l.starts_with('@')));
        assert!(d.len() < 20, "{d:?}");
        assert_eq!(counts(&d), (2, 2));
        let l = line('+', Some(12), "x\ty\x1b[31m!");
        let (k, n, t) = split_line(&l);
        assert_eq!((k, n, t), ('+', "12", "x    y!"));
    }

    #[test]
    fn chat_agent_partial_input() {
        let v = partial_input(r#"{"file_path": "C:\\work\\demo\\hel"#);
        assert_eq!(v["file_path"], "C:\\work\\demo\\hel");
        assert_eq!(partial_input(r#"{"a": "#)["a"], Value::Null);
        assert_eq!(rel(Path::new("C:\\work\\demo"), "c:\\work\\demo\\src\\a.rs"), "src\\a.rs");
        assert_eq!(rel(Path::new("/home/x/p"), "/home/x/p/a.rs"), "a.rs");
        assert_eq!(rel(Path::new("/home/x/p"), "/home/x/pq/a.rs"), "/home/x/pq/a.rs");
        assert_eq!(strip_cwd(Path::new("C:\\work\\demo"), "Get-Content 'C:\\Work\\demo\\notes.txt'"), "Get-Content 'notes.txt'");
        assert_eq!(strip_cwd(Path::new("/home/x/p"), "cat /home/x/p/a /home/x/pq"), "cat a /home/x/pq");
        assert_eq!(unwrap_shell(r#""C:\\pwsh.exe" -Command 'Get-Content notes.txt'"#), "Get-Content notes.txt");
    }
}
