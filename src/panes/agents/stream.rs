//! Headless agent output → live transcript entries. Understands Claude Code's `-p --output-format stream-json`,
//! `codex exec --json` and Kimi Code's `-p --output-format stream-json` (OpenAI-style role/content/tool_calls
//! lines). A lean cousin of chat/agent.rs: the board only needs what each agent did, the files it touched, its
//! session id (to resume it) and what it cost.

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// One line of a live transcript: a tool call, something the agent said, an error.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entry {
    /// 't' tool call · 's' said · 'e' error · 'n' note from oriel
    pub kind: char,
    /// Tool calls are updated in place by id.
    pub id: String,
    pub label: String,
    pub target: String,
    /// "+3 -1", "exit 1", "12 lines"…
    pub summary: String,
    /// running | done | error
    pub status: String,
    /// Diff/output lines, each starting with '+', '-', '>' or '!'.
    pub body: Vec<String>,
}

impl Entry {
    pub fn say(text: &str) -> Entry {
        Entry { kind: 's', label: text.trim().to_string(), status: "done".into(), ..Default::default() }
    }
    pub fn note(text: &str) -> Entry {
        Entry { kind: 'n', label: text.trim().to_string(), status: "done".into(), ..Default::default() }
    }
    pub fn error(text: &str) -> Entry {
        Entry { kind: 'e', label: text.trim().to_string(), status: "error".into(), ..Default::default() }
    }
    /// One line for cards, task_status and the lead's log: "Edit src/app.rs".
    pub fn line(&self) -> String {
        match self.kind {
            't' => {
                let mut s = if self.target.is_empty() { self.label.clone() } else { format!("{} {}", self.label, self.target) };
                if !self.summary.is_empty() {
                    s.push_str(&format!(" · {}", self.summary));
                }
                s
            }
            _ => self.label.lines().next().unwrap_or("").to_string(),
        }
    }
}

/// What the parser reports.
#[derive(Clone, Debug, PartialEq)]
pub enum Ev {
    Session(String),
    /// New or updated (same id) entry.
    Entry(Entry),
    /// A file the agent edited (relative to its folder when inside it).
    Touched(String),
    /// Spend so far in this process, USD (replaces the last value).
    Cost(f64),
    Tokens(u64),
    /// Claude's rate_limit_event payload.
    Limit(Value),
    /// Its structured closing report {status, summary, questions} (claude --json-schema).
    Report(Value),
    /// The agent's final answer.
    Final(String),
    Error(String),
}

/// Which stream format.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Claude,
    Codex,
    Kimi,
}

impl Kind {
    pub fn of(agent: &str) -> Kind {
        match agent {
            "codex" => Kind::Codex,
            "kimi" => Kind::Kimi,
            _ => Kind::Claude,
        }
    }
}

const BODY_CAP: usize = 14;

pub struct Parser {
    kind: Kind,
    cwd: String,
    /// codex pricing needs the model (the stream doesn't name it)
    model: String,
    session: String,
    tools: HashMap<String, Entry>,
    seen_msgs: HashSet<String>,
    cost: f64,
    tokens: u64,
    text: String,
}

impl Parser {
    pub fn new(kind: Kind, cwd: &Path, model: &str) -> Parser {
        Parser { kind, cwd: cwd.to_string_lossy().to_string(), model: model.to_string(), session: String::new(), tools: HashMap::new(), seen_msgs: HashSet::new(), cost: 0.0, tokens: 0, text: String::new() }
    }

    pub fn cost(&self) -> f64 {
        self.cost
    }

    pub fn feed(&mut self, v: &Value, out: &mut dyn FnMut(Ev)) {
        match self.kind {
            Kind::Claude => self.claude(v, out),
            Kind::Codex => self.codex(v, out),
            Kind::Kimi => self.kimi(v, out),
        }
    }

    fn session(&mut self, s: Option<&str>, out: &mut dyn FnMut(Ev)) {
        if let Some(s) = s.filter(|s| !s.is_empty()) {
            if s != self.session {
                self.session = s.to_string();
                out(Ev::Session(s.to_string()));
            }
        }
    }

    fn tool(&mut self, id: &str, name: &str, input: &Value, out: &mut dyn FnMut(Ev)) {
        let mut e = Entry { kind: 't', id: id.to_string(), label: label(name), target: target(name, input, &self.cwd), status: "running".into(), ..Default::default() };
        let lower = name.to_lowercase();
        let path = ["file_path", "path", "notebook_path"].iter().find_map(|k| input[*k].as_str()).unwrap_or("");
        let edits = matches!(name, "Edit" | "MultiEdit" | "Write" | "NotebookEdit") || ["write", "edit", "replace", "patch", "create"].iter().any(|w| lower.contains(w));
        if edits && !path.is_empty() && !lower.starts_with("mcp__") {
            out(Ev::Touched(rel(&self.cwd, path)));
        }
        match name {
            "Edit" => e.body = mini_diff(input["old_string"].as_str().unwrap_or(""), input["new_string"].as_str().unwrap_or("")),
            "MultiEdit" => {
                for ed in input["edits"].as_array().into_iter().flatten() {
                    e.body.extend(mini_diff(ed["old_string"].as_str().unwrap_or(""), ed["new_string"].as_str().unwrap_or("")));
                }
                e.body.truncate(BODY_CAP);
            }
            "Write" => e.body = input["content"].as_str().unwrap_or("").lines().take(BODY_CAP).map(|l| format!("+{l}")).collect(),
            _ => {}
        }
        if !e.body.is_empty() {
            let (a, d) = (e.body.iter().filter(|l| l.starts_with('+')).count(), e.body.iter().filter(|l| l.starts_with('-')).count());
            e.summary = format!("+{a} -{d}");
        }
        self.tools.insert(id.to_string(), e.clone());
        out(Ev::Entry(e));
    }

    fn finish_tool(&mut self, id: &str, text: &str, error: bool, out: &mut dyn FnMut(Ev)) {
        let Some(e) = self.tools.get_mut(id) else { return };
        e.status = if error { "error".into() } else { "done".into() };
        let first = text.trim().lines().next().unwrap_or("").trim().to_string();
        if error {
            e.summary = crate::ui::fit(&first.replace("<tool_use_error>", "").replace("</tool_use_error>", ""), 70);
        } else if e.summary.is_empty() {
            let n = text.trim().lines().count();
            e.summary = if matches!(e.label.as_str(), "Bash" | "PowerShell" | "Run") {
                if n == 0 { "no output".into() } else { format!("{n} line{}", if n == 1 { "" } else { "s" }) }
            } else {
                crate::ui::fit(&first, 60)
            };
        }
        if matches!(e.label.as_str(), "Bash" | "PowerShell" | "Run") && e.body.is_empty() {
            let lines: Vec<&str> = text.trim_end().lines().collect();
            let k = if error { '!' } else { '>' };
            e.body = lines.iter().rev().take(6).rev().map(|l| format!("{k}{}", l.replace('\t', "    "))).collect();
        }
        out(Ev::Entry(e.clone()));
    }

    fn said(&mut self, text: &str, out: &mut dyn FnMut(Ev)) {
        let t = text.trim();
        if t.is_empty() {
            return;
        }
        self.text = t.to_string();
        out(Ev::Entry(Entry::say(t)));
    }

    // ------------------------------------------------------------------ Claude Code
    fn claude(&mut self, v: &Value, out: &mut dyn FnMut(Ev)) {
        self.session(v["session_id"].as_str(), out);
        match v["type"].as_str() {
            Some("system") if v["subtype"] == "init" => {
                if let Some(m) = v["model"].as_str() {
                    self.model = m.to_string();
                }
            }
            Some("assistant") => {
                let msg = &v["message"];
                // live cost until the result gives the real figure (one response can span several lines)
                let key = format!("{}:{}", msg["id"].as_str().unwrap_or(""), v["requestId"].as_str().unwrap_or(""));
                if key != ":" && self.seen_msgs.insert(key) {
                    let u = &msg["usage"];
                    let n = |k: &str| u[k].as_u64().unwrap_or(0);
                    let (pi, p5, _p1, pr, po) = super::cost::claude_price(msg["model"].as_str().unwrap_or(&self.model));
                    self.cost += (n("input_tokens") as f64 * pi + n("cache_creation_input_tokens") as f64 * p5 + n("cache_read_input_tokens") as f64 * pr + n("output_tokens") as f64 * po) / 1e6;
                    self.tokens += n("input_tokens") + n("output_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens");
                    out(Ev::Cost(self.cost));
                    out(Ev::Tokens(self.tokens));
                }
                if v["parent_tool_use_id"].is_string() {
                    return; // a subagent's inner steps: the Task call already shows
                }
                for it in msg["content"].as_array().into_iter().flatten() {
                    match it["type"].as_str() {
                        Some("tool_use") => {
                            let (id, name) = (it["id"].as_str().unwrap_or(""), it["name"].as_str().unwrap_or("tool"));
                            if name == "StructuredOutput" {
                                // --json-schema: the closing report arrives as this tool's input
                                let r = it["input"].clone();
                                let s = r["summary"].as_str().unwrap_or("").to_string();
                                if !s.is_empty() {
                                    self.text = s.clone();
                                    out(Ev::Entry(Entry::say(&s)));
                                }
                                out(Ev::Report(r));
                            } else if !matches!(name, "TodoWrite" | "ToolSearch" | "TaskCreate" | "TaskUpdate" | "TaskList" | "TaskGet") {
                                self.tool(id, name, &it["input"], out);
                            }
                        }
                        Some("text") => {
                            let t = it["text"].as_str().unwrap_or("").to_string();
                            self.said(&t, out);
                        }
                        _ => {}
                    }
                }
            }
            Some("user") => {
                for it in v["message"]["content"].as_array().into_iter().flatten() {
                    if it["type"] == "tool_result" {
                        let text = match &it["content"] {
                            Value::String(s) => s.clone(),
                            Value::Array(a) => a.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
                            _ => String::new(),
                        };
                        self.finish_tool(it["tool_use_id"].as_str().unwrap_or(""), &text, it["is_error"].as_bool() == Some(true), out);
                    }
                }
            }
            Some("rate_limit_event") => out(Ev::Limit(v["rate_limit_info"].clone())),
            Some("result") => {
                if v["structured_output"].is_object() {
                    out(Ev::Report(v["structured_output"].clone()));
                }
                if let Some(c) = v["total_cost_usd"].as_f64() {
                    self.cost = c;
                    out(Ev::Cost(c));
                }
                let u = &v["usage"];
                let n = |k: &str| u[k].as_u64().unwrap_or(0);
                let t = n("input_tokens") + n("output_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens");
                if t > 0 {
                    self.tokens = t;
                    out(Ev::Tokens(t));
                }
                let text = v["result"].as_str().unwrap_or("").to_string();
                if v["is_error"].as_bool() == Some(true) || v["subtype"].as_str().is_some_and(|s| s.starts_with("error")) {
                    let why = match v["subtype"].as_str() {
                        Some("error_max_turns") => "hit its turn limit".to_string(),
                        Some("error_max_budget_usd") => "hit its budget".to_string(),
                        _ if !text.is_empty() => text.clone(),
                        Some(s) => s.replace('_', " "),
                        None => "Claude Code stopped with an error".into(),
                    };
                    out(Ev::Error(why));
                }
                if !text.is_empty() {
                    self.text = text.clone();
                }
                out(Ev::Final(self.text.clone()));
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------ Codex
    fn codex(&mut self, v: &Value, out: &mut dyn FnMut(Ev)) {
        let ty = v["type"].as_str().unwrap_or("");
        match ty {
            "thread.started" => self.session(v["thread_id"].as_str(), out),
            "item.started" | "item.updated" | "item.completed" => {
                let it = &v["item"];
                let id = it["id"].as_str().unwrap_or("").to_string();
                let done = ty == "item.completed";
                match it["type"].as_str() {
                    Some("command_execution") => {
                        if !self.tools.contains_key(&id) {
                            let cmd = unwrap_shell(it["command"].as_str().unwrap_or(""));
                            self.tool(&id, "command_execution", &serde_json::json!({ "command": cmd }), out);
                        }
                        if done {
                            let code = it["exit_code"].as_i64().unwrap_or(0);
                            let failed = it["status"] == "failed" || code != 0;
                            self.finish_tool(&id, it["aggregated_output"].as_str().unwrap_or(""), failed, out);
                            if failed {
                                if let Some(e) = self.tools.get_mut(&id) {
                                    e.summary = format!("exit {code}");
                                    out(Ev::Entry(e.clone()));
                                }
                            }
                        }
                    }
                    Some("file_change") => {
                        let changes = it["changes"].as_array().cloned().unwrap_or_default();
                        let files: Vec<String> = changes.iter().filter_map(|c| c["path"].as_str()).map(|p| rel(&self.cwd, p)).collect();
                        for f in &files {
                            out(Ev::Touched(f.clone()));
                        }
                        let mut body = vec![];
                        for c in &changes {
                            if let Some(d) = c["diff"].as_str().or(c["unified_diff"].as_str()) {
                                body.extend(d.lines().filter(|l| (l.starts_with('+') && !l.starts_with("+++")) || (l.starts_with('-') && !l.starts_with("---"))).map(String::from));
                            }
                        }
                        body.truncate(BODY_CAP);
                        let all_new = !changes.is_empty() && changes.iter().all(|c| c["kind"] == "add");
                        let mut e = Entry { kind: 't', id: id.clone(), label: if all_new { "Write".into() } else { "Edit".into() }, target: files.join(", "), status: if done { "done".into() } else { "running".into() }, body, ..Default::default() };
                        if !e.body.is_empty() {
                            let (a, d) = (e.body.iter().filter(|l| l.starts_with('+')).count(), e.body.iter().filter(|l| l.starts_with('-')).count());
                            e.summary = format!("+{a} -{d}");
                        }
                        if it["status"] == "failed" {
                            e.status = "error".into();
                        }
                        self.tools.insert(id, e.clone());
                        out(Ev::Entry(e));
                    }
                    Some("mcp_tool_call") => {
                        if !self.tools.contains_key(&id) {
                            let name = format!("mcp__{}__{}", it["server"].as_str().unwrap_or("mcp"), it["tool"].as_str().unwrap_or("tool"));
                            self.tool(&id, &name, &it["arguments"], out);
                        }
                        if done {
                            let txt = it["result"]["content"].as_array().map(|a| a.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n")).unwrap_or_default();
                            let err = it["status"] == "failed" || !it["error"].is_null();
                            let msg = it["error"]["message"].as_str().map(String::from).unwrap_or(txt);
                            self.finish_tool(&id, &msg, err, out);
                        }
                    }
                    Some("agent_message") if done => {
                        let t = it["text"].as_str().unwrap_or("").to_string();
                        self.said(&t, out);
                    }
                    Some("error") if done => out(Ev::Entry(Entry::error(it["message"].as_str().unwrap_or("error")))),
                    _ => {}
                }
            }
            "turn.completed" => {
                let u = &v["usage"];
                let n = |k: &str| u[k].as_u64().unwrap_or(0) as f64;
                let (pi, pc, po) = super::cost::codex_price(&self.model);
                let cached = n("cached_input_tokens");
                self.cost += ((n("input_tokens") - cached).max(0.0) * pi + cached * pc + n("output_tokens") * po) / 1e6;
                self.tokens += (n("input_tokens") + n("output_tokens")) as u64;
                out(Ev::Cost(self.cost));
                out(Ev::Tokens(self.tokens));
                out(Ev::Final(self.text.clone()));
            }
            "turn.failed" | "error" => {
                let m = v["message"].as_str().or(v["error"]["message"].as_str()).map(String::from).unwrap_or_else(|| v["error"].to_string());
                out(Ev::Error(m));
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------ Kimi Code
    fn kimi(&mut self, v: &Value, out: &mut dyn FnMut(Ev)) {
        self.session(v["session_id"].as_str().or(v["sessionId"].as_str()), out);
        let content = |c: &Value| -> String {
            match c {
                Value::String(s) => s.clone(),
                Value::Array(a) => a.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
                _ => String::new(),
            }
        };
        match v["role"].as_str() {
            Some("assistant") => {
                let t = content(&v["content"]);
                self.said(&t, out);
                for c in v["tool_calls"].as_array().into_iter().flatten() {
                    let f = &c["function"];
                    let args: Value = match &f["arguments"] {
                        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
                        x => x.clone(),
                    };
                    self.tool(c["id"].as_str().unwrap_or(""), f["name"].as_str().unwrap_or("tool"), &args, out);
                }
            }
            Some("tool") => {
                let t = content(&v["content"]);
                let err = v["is_error"].as_bool() == Some(true);
                self.finish_tool(v["tool_call_id"].as_str().unwrap_or(""), &t, err, out);
            }
            _ => {}
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            let n = |k: &str| u[k].as_u64().unwrap_or(0);
            let t = n("input_tokens") + n("output_tokens") + n("prompt_tokens") + n("completion_tokens");
            if t > 0 {
                self.tokens += t;
                out(Ev::Tokens(self.tokens));
            }
        }
    }

    /// Call once the process ended: Kimi has no result line, so its last reply is the answer.
    pub fn end(&mut self, out: &mut dyn FnMut(Ev)) {
        if self.kind == Kind::Kimi && !self.text.is_empty() {
            out(Ev::Final(self.text.clone()));
        }
    }
}

// ------------------------------------------------------------------ describing a call

pub fn label(name: &str) -> String {
    match name {
        "command_execution" => "Run".into(),
        "MultiEdit" => "Edit".into(),
        "Task" | "Agent" => "Agent".into(),
        n if n.starts_with("mcp__oriel__") => n.trim_start_matches("mcp__oriel__").to_string(),
        n if n.starts_with("mcp__") => {
            let mut it = n.trim_start_matches("mcp__").splitn(2, "__");
            let (s, t) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
            format!("{s}·{t}")
        }
        n => n.to_string(),
    }
}

/// The short "what": a path, a command, a pattern, or for oriel's own tools the worker / task.
pub fn target(name: &str, input: &Value, cwd: &str) -> String {
    let s = |k: &str| input[k].as_str().unwrap_or("").to_string();
    if name.starts_with("mcp__oriel__") {
        let mut parts = vec![];
        for k in ["worker", "id", "title", "text", "summary"] {
            let v = s(k);
            if !v.is_empty() {
                parts.push(if k == "title" || k == "text" || k == "summary" { format!("\"{}\"", crate::ui::fit(&v.replace('\n', " "), 60)) } else { v });
            }
        }
        if let Some(ids) = input["ids"].as_array() {
            parts.push(ids.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(","));
        }
        return parts.join(" ");
    }
    for k in ["file_path", "path", "notebook_path"] {
        if let Some(p) = input[k].as_str() {
            return rel(cwd, p);
        }
    }
    if let Some(c) = input["command"].as_str().or(input["cmd"].as_str()) {
        let c = strip_cwd(cwd, c);
        let first = c.trim().lines().next().unwrap_or("").trim().to_string();
        return if c.trim().lines().count() > 1 { format!("{first} …") } else { first };
    }
    let p = s("pattern");
    if !p.is_empty() {
        return format!("'{p}'");
    }
    for k in ["url", "query", "description", "prompt", "name"] {
        if let Some(v) = input[k].as_str() {
            return crate::ui::fit(v.lines().next().unwrap_or(""), 80);
        }
    }
    String::new()
}

/// A path relative to the agent's folder when it's inside it.
pub fn rel(cwd: &str, p: &str) -> String {
    let norm = |s: &str| s.replace('\\', "/");
    let (pn, cn) = (norm(p), norm(cwd.trim_end_matches(['/', '\\'])));
    if !cn.is_empty() && pn.len() > cn.len() + 1 && pn.as_bytes()[..cn.len()].eq_ignore_ascii_case(cn.as_bytes()) && pn.as_bytes()[cn.len()] == b'/' {
        return pn[cn.len() + 1..].to_string();
    }
    pn
}

fn strip_cwd(cwd: &str, s: &str) -> String {
    let c = cwd.trim_end_matches(['/', '\\']);
    if c.len() < 3 {
        return s.to_string();
    }
    let mut out = s.to_string();
    for n in [format!("{c}\\"), format!("{}/", c.replace('\\', "/"))] {
        let n = n.to_ascii_lowercase();
        while let Some(i) = out.to_ascii_lowercase().find(&n) {
            out.replace_range(i..i + n.len(), "");
        }
    }
    out
}

/// `"pwsh.exe" -Command 'x'` / `bash -lc 'x'` → x
fn unwrap_shell(c: &str) -> String {
    for flag in [" -Command ", " -lc ", " -c "] {
        if let Some((_, rest)) = c.split_once(flag) {
            let r = rest.trim();
            let r = r.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')).or_else(|| r.strip_prefix('"').and_then(|x| x.strip_suffix('"'))).unwrap_or(r);
            return r.to_string();
        }
    }
    c.to_string()
}

/// Old lines as '-', new as '+', only what differs at the ends trimmed away — enough to see an edit at a glance.
fn mini_diff(old: &str, new: &str) -> Vec<String> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let mut s = 0;
    while s < a.len() && s < b.len() && a[s] == b[s] {
        s += 1;
    }
    let mut e = 0;
    while e < a.len() - s && e < b.len() - s && a[a.len() - 1 - e] == b[b.len() - 1 - e] {
        e += 1;
    }
    let mut out: Vec<String> = a[s..a.len() - e].iter().map(|l| format!("-{l}")).collect();
    out.extend(b[s..b.len() - e].iter().map(|l| format!("+{l}")));
    out.truncate(BODY_CAP);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(kind: Kind, lines: &[Value]) -> Vec<Ev> {
        let mut p = Parser::new(kind, Path::new("/w/demo"), "");
        let mut evs = vec![];
        for l in lines {
            p.feed(l, &mut |e| evs.push(e));
        }
        p.end(&mut |e| evs.push(e));
        evs
    }

    #[test]
    fn agents_stream_claude() {
        let evs = run(
            Kind::Claude,
            &[
                json!({"type":"system","subtype":"init","session_id":"S1","model":"claude-haiku-4-5"}),
                json!({"type":"assistant","session_id":"S1","message":{"id":"m1","model":"claude-haiku-4-5","usage":{"input_tokens":1000,"output_tokens":100},"content":[{"type":"text","text":"On it."},{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/w/demo/src/a.rs","old_string":"x\ny","new_string":"x\nz"}}]}}),
                json!({"type":"user","session_id":"S1","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}),
                json!({"type":"assistant","session_id":"S1","message":{"id":"m2","content":[{"type":"tool_use","id":"t2","name":"mcp__oriel__spawn_task","input":{"worker":"codex","title":"Fix it","prompt":"..."}}]}}),
                json!({"type":"result","subtype":"success","session_id":"S1","total_cost_usd":0.0123,"result":"All done.","usage":{"input_tokens":10,"output_tokens":5}}),
            ],
        );
        assert!(evs.contains(&Ev::Session("S1".into())));
        assert!(evs.contains(&Ev::Touched("src/a.rs".into())));
        assert!(evs.contains(&Ev::Cost(0.0123)));
        assert!(evs.contains(&Ev::Final("All done.".into())));
        let edit = evs.iter().rev().find_map(|e| if let Ev::Entry(x) = e { (x.id == "t1").then(|| x.clone()) } else { None }).unwrap();
        assert_eq!((edit.label.as_str(), edit.target.as_str(), edit.status.as_str(), edit.summary.as_str()), ("Edit", "src/a.rs", "done", "+1 -1"));
        assert_eq!(edit.body, vec!["-y", "+z"]);
        let spawn = evs.iter().find_map(|e| if let Ev::Entry(x) = e { (x.id == "t2").then(|| x.clone()) } else { None }).unwrap();
        assert_eq!(spawn.line(), "spawn_task codex \"Fix it\"");
        let err = run(Kind::Claude, &[json!({"type":"result","subtype":"error_max_turns","is_error":true,"total_cost_usd":0.5})]);
        assert!(err.contains(&Ev::Error("hit its turn limit".into())));
    }

    #[test]
    fn agents_stream_codex_and_kimi() {
        let evs = run(
            Kind::Codex,
            &[
                json!({"type":"thread.started","thread_id":"T9"}),
                json!({"type":"item.started","item":{"id":"i1","type":"command_execution","command":"bash -lc 'cargo test'"}}),
                json!({"type":"item.completed","item":{"id":"i1","type":"command_execution","command":"bash -lc 'cargo test'","exit_code":1,"aggregated_output":"fail\n"}}),
                json!({"type":"item.completed","item":{"id":"i2","type":"file_change","changes":[{"path":"/w/demo/b.txt","kind":"add"}]}}),
                json!({"type":"item.completed","item":{"id":"i3","type":"agent_message","text":"Fixed."}}),
                json!({"type":"turn.completed","usage":{"input_tokens":1000000,"cached_input_tokens":0,"output_tokens":0}}),
            ],
        );
        assert!(evs.contains(&Ev::Session("T9".into())));
        assert!(evs.contains(&Ev::Touched("b.txt".into())));
        assert!(evs.contains(&Ev::Final("Fixed.".into())));
        assert!(evs.iter().any(|e| matches!(e, Ev::Cost(c) if (*c - 2.0).abs() < 1e-9)));
        assert!(evs.iter().any(|e| matches!(e, Ev::Entry(x) if x.label == "Run" && x.target == "cargo test" && x.summary == "exit 1" && x.status == "error")));
        let k = run(
            Kind::Kimi,
            &[
                json!({"role":"assistant","content":"","tool_calls":[{"id":"c1","type":"function","function":{"name":"WriteFile","arguments":"{\"path\":\"/w/demo/n.md\",\"content\":\"hi\"}"}}],"session_id":"K1"}),
                json!({"role":"tool","tool_call_id":"c1","content":"written"}),
                json!({"role":"assistant","content":"Wrote n.md"}),
            ],
        );
        assert!(k.contains(&Ev::Session("K1".into())));
        assert!(k.contains(&Ev::Touched("n.md".into())));
        assert!(k.contains(&Ev::Final("Wrote n.md".into())));
    }
}
