//! Token saver presets (docs/ai-research.md §4) and the careful config edits behind them.
//!
//! Claude: only the listed keys of ~/.claude/settings.json change. The file is edited as text, member by member,
//! so every other key (hooks, plugins …) keeps its exact bytes and order; the result is re-parsed and must equal
//! the intended JSON value, or nothing is written. Codex: a `[profiles.oriel-*]` block is added to (or replaced
//! in) ~/.codex/config.toml, again text-level and validated. Every write keeps a timestamped backup, goes through
//! a temp file + rename, and refuses if the file changed since the diff was shown.

use super::util::{Paths, write_atomic};
use serde_json::{Map, Value, json};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug)]
pub enum V {
    S(&'static str),
    B(bool),
}

impl V {
    fn json(self) -> Value {
        match self {
            V::S(s) => Value::String(s.into()),
            V::B(b) => Value::Bool(b),
        }
    }
}

pub struct Preset {
    pub name: &'static str,
    pub blurb: &'static str,
    /// top-level settings.json keys
    pub claude: &'static [(&'static str, V)],
    /// settings.json `env` keys (None = remove it)
    pub env: &'static [(&'static str, Option<&'static str>)],
    /// the Codex profile name and its keys (value = a TOML literal)
    pub profile: &'static str,
    pub codex: &'static [(&'static str, &'static str)],
}

/// env keys any preset manages (so switching presets cleans up after the previous one)
pub const ENV_KEYS: &[&str] = &["CLAUDE_CODE_SUBAGENT_MODEL", "CLAUDE_CODE_MAX_OUTPUT_TOKENS", "MAX_MCP_OUTPUT_TOKENS", "BASH_MAX_OUTPUT_LENGTH", "CLAUDE_CODE_DISABLE_1M_CONTEXT"];

#[rustfmt::skip]
pub static PRESETS: &[Preset] = &[
    Preset {
        name: "Frugal", blurb: "cheapest: Sonnet, low effort, Haiku subagents, capped outputs, no 1M context",
        claude: &[("model", V::S("sonnet")), ("effortLevel", V::S("low")), ("autoCompactEnabled", V::B(true))],
        env: &[("CLAUDE_CODE_SUBAGENT_MODEL", Some("haiku")), ("CLAUDE_CODE_MAX_OUTPUT_TOKENS", Some("16000")), ("MAX_MCP_OUTPUT_TOKENS", Some("10000")),
               ("BASH_MAX_OUTPUT_LENGTH", Some("15000")), ("CLAUDE_CODE_DISABLE_1M_CONTEXT", Some("1"))],
        profile: "oriel-frugal",
        codex: &[("model", "\"gpt-5.6-luna\""), ("model_reasoning_effort", "\"low\""), ("model_reasoning_summary", "\"none\""), ("model_verbosity", "\"low\""), ("tool_output_token_limit", "8000")],
    },
    Preset {
        name: "Balanced", blurb: "everyday: Sonnet, medium effort, Haiku subagents, trimmed tool output",
        claude: &[("model", V::S("sonnet")), ("effortLevel", V::S("medium")), ("autoCompactEnabled", V::B(true))],
        env: &[("CLAUDE_CODE_SUBAGENT_MODEL", Some("haiku")), ("CLAUDE_CODE_MAX_OUTPUT_TOKENS", None), ("MAX_MCP_OUTPUT_TOKENS", Some("25000")),
               ("BASH_MAX_OUTPUT_LENGTH", Some("30000")), ("CLAUDE_CODE_DISABLE_1M_CONTEXT", None)],
        profile: "oriel-balanced",
        codex: &[("model", "\"gpt-5.6-terra\""), ("model_reasoning_effort", "\"medium\""), ("model_verbosity", "\"medium\"")],
    },
    Preset {
        name: "Max", blurb: "best results: Opus, high effort, no caps (clears the frugal env overrides)",
        claude: &[("model", V::S("opus")), ("effortLevel", V::S("high")), ("autoCompactEnabled", V::B(true))],
        env: &[("CLAUDE_CODE_SUBAGENT_MODEL", None), ("CLAUDE_CODE_MAX_OUTPUT_TOKENS", None), ("MAX_MCP_OUTPUT_TOKENS", None),
               ("BASH_MAX_OUTPUT_LENGTH", None), ("CLAUDE_CODE_DISABLE_1M_CONTEXT", None)],
        profile: "oriel-max",
        codex: &[("model", "\"gpt-5.6-sol\""), ("model_reasoning_effort", "\"high\"")],
    },
];

pub const TIPS: &[&str] = &[
    "keep CLAUDE.md under ~200 lines: it's re-sent with every request",
    "/clear between unrelated tasks; /compact when a long session drifts",
    "/context shows what fills the window; /mcp disables servers you aren't using",
    "don't set DISABLE_PROMPT_CACHING: cache reads cost ~10% of fresh input",
    "use subagents (Haiku) for searches so the main context stays small",
    "headless runs: claude -p --max-turns N --max-budget-usd B",
    "codex -p oriel-frugal for quick questions, the default profile for real work",
];

// ------------------------------------------------------------------ a planned edit

#[derive(Clone, Debug)]
pub struct Plan {
    pub title: String,
    pub target: PathBuf,
    /// the file as it was when the diff was made (None = it didn't exist)
    pub original: Option<String>,
    pub new_text: String,
    /// ('-' | '+' | ' ', text)
    pub diff: Vec<(char, String)>,
    pub notes: Vec<String>,
    pub done: String,
}

/// Write a confirmed plan: refuse if the file changed since, back it up, then temp file + rename.
/// Returns the backup's path (None when the file is new).
pub fn apply(p: &Plan, stamp: &str) -> Result<Option<PathBuf>, String> {
    let now = std::fs::read_to_string(&p.target).ok();
    if now != p.original {
        return Err(format!("{} changed since the diff was made — nothing written, try again", p.target.display()));
    }
    let mut backup = None;
    if p.original.is_some() {
        let mut b = p.target.as_os_str().to_owned();
        b.push(format!(".oriel-backup-{stamp}"));
        let b = PathBuf::from(b);
        std::fs::copy(&p.target, &b).map_err(|e| format!("couldn't back up {}: {e}", p.target.display()))?;
        backup = Some(b);
    }
    write_atomic(&p.target, p.new_text.as_bytes()).map_err(|e| format!("couldn't write {}: {e}", p.target.display()))?;
    Ok(backup)
}

// ------------------------------------------------------------------ JSON member surgery

struct Member {
    key: String,
    kstart: usize,
    vstart: usize,
    vend: usize,
}

fn ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && (b[i] as char).is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// End (exclusive) of the JSON string starting at `i` (which must be a quote).
fn str_end(b: &[u8], mut i: usize) -> Option<usize> {
    i += 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

fn value_end(b: &[u8], i: usize) -> Option<usize> {
    match *b.get(i)? {
        b'"' => str_end(b, i),
        b'{' | b'[' => {
            let mut depth = 0i32;
            let mut j = i;
            while j < b.len() {
                match b[j] {
                    b'"' => {
                        j = str_end(b, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            None
        }
        _ => {
            let mut j = i;
            while j < b.len() && !matches!(b[j], b',' | b'}' | b']') && !(b[j] as char).is_ascii_whitespace() {
                j += 1;
            }
            (j > i).then_some(j)
        }
    }
}

/// The top-level object's members with their byte spans, and the index of its closing brace.
fn members(s: &str) -> Option<(Vec<Member>, usize)> {
    let b = s.as_bytes();
    let mut i = ws(b, 0);
    if b.get(i) != Some(&b'{') {
        return None;
    }
    i = ws(b, i + 1);
    let mut out = vec![];
    if b.get(i) == Some(&b'}') {
        return Some((out, i));
    }
    loop {
        if b.get(i) != Some(&b'"') {
            return None;
        }
        let kend = str_end(b, i)?;
        let key: String = serde_json::from_str(&s[i..kend]).ok()?;
        let mut j = ws(b, kend);
        if b.get(j) != Some(&b':') {
            return None;
        }
        j = ws(b, j + 1);
        let vend = value_end(b, j)?;
        out.push(Member { key, kstart: i, vstart: j, vend });
        let k = ws(b, vend);
        match b.get(k) {
            Some(b',') => i = ws(b, k + 1),
            Some(b'}') => return Some((out, k)),
            _ => return None,
        }
    }
}

fn indent_of(s: &str, m: &[Member]) -> String {
    let Some(first) = m.first() else { return "  ".into() };
    let line_start = s[..first.kstart].rfind('\n').map(|p| p + 1).unwrap_or(0);
    let ind = &s[line_start..first.kstart];
    if ind.chars().all(|c| c == ' ' || c == '\t') && !ind.is_empty() { ind.to_string() } else { "  ".into() }
}

fn pretty(v: &Value, indent: &str) -> String {
    use serde::Serialize;
    let mut buf = vec![];
    let fmt = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    let _ = v.serialize(&mut ser);
    String::from_utf8(buf).unwrap_or_default().replace('\n', &format!("\n{indent}"))
}

/// Set (Some) or remove (None) one top-level member, leaving every other byte alone.
pub fn set_member(s: &str, key: &str, val: Option<&Value>) -> Option<String> {
    let (m, close) = members(s)?;
    let ind = indent_of(s, &m);
    let pos = m.iter().position(|x| x.key == key);
    Some(match (pos, val) {
        (Some(k), Some(v)) => format!("{}{}{}", &s[..m[k].vstart], pretty(v, &ind), &s[m[k].vend..]),
        (Some(k), None) => {
            if k + 1 < m.len() {
                format!("{}{}", &s[..m[k].kstart], &s[m[k + 1].kstart..])
            } else if k > 0 {
                format!("{}{}", &s[..m[k - 1].vend], &s[m[k].vend..])
            } else {
                format!("{}{}", &s[..m[k].kstart], &s[m[k].vend..])
            }
        }
        (None, Some(v)) => {
            let item = format!("{}: {}", serde_json::to_string(key).ok()?, pretty(v, &ind));
            match m.last() {
                Some(last) => format!("{},\n{ind}{item}{}", &s[..last.vend], &s[last.vend..]),
                None => {
                    let open = s.find('{')?;
                    format!("{}{{\n{ind}{item}\n{}", &s[..open], &s[close..])
                }
            }
        }
        (None, None) => s.to_string(),
    })
}

fn show(v: Option<&Value>) -> String {
    match v {
        None => "(not set)".into(),
        Some(v) => serde_json::to_string(v).unwrap_or_default(),
    }
}

/// Apply a set of top-level changes to settings.json text; returns (new text, diff lines). Errors if the file
/// isn't a JSON object or the edit wouldn't reproduce exactly the intended value.
pub fn edit_settings(text: Option<&str>, changes: &[(&str, Option<Value>)], env: &[(&str, Option<&str>)]) -> Result<(String, Vec<(char, String)>), String> {
    let src = text.unwrap_or("{}\n");
    let before: Value = serde_json::from_str(src).map_err(|e| format!("settings.json isn't valid JSON ({e}); fix it first"))?;
    let Value::Object(obj) = &before else { return Err("settings.json isn't a JSON object".into()) };
    let mut want = obj.clone();
    let mut out = src.to_string();
    let mut diff = vec![];
    for (k, v) in changes {
        let old = obj.get(*k);
        if old == v.as_ref() {
            diff.push((' ', format!("\"{k}\": {}", show(old))));
            continue;
        }
        diff.push(('-', format!("\"{k}\": {}", show(old))));
        diff.push(('+', format!("\"{k}\": {}", show(v.as_ref()))));
        out = set_member(&out, k, v.as_ref()).ok_or("couldn't edit settings.json safely")?;
        match v {
            Some(v) => want.insert(k.to_string(), v.clone()),
            None => want.remove(*k),
        };
    }
    if !env.is_empty() {
        let old_env = obj.get("env").and_then(|e| e.as_object()).cloned();
        if obj.get("env").is_some_and(|e| !e.is_object()) {
            return Err("settings.json has an \"env\" that isn't an object".into());
        }
        let mut new_env = old_env.clone().unwrap_or_default();
        for (k, v) in env {
            let old = new_env.get(*k).cloned();
            let nv = v.map(|s| Value::String(s.into()));
            if old == nv {
                if old.is_some() {
                    diff.push((' ', format!("env.{k} = {}", show(old.as_ref()))));
                }
                continue;
            }
            diff.push(('-', format!("env.{k} = {}", show(old.as_ref()))));
            diff.push(('+', format!("env.{k} = {}", show(nv.as_ref()))));
            match nv {
                Some(x) => new_env.insert(k.to_string(), x),
                None => new_env.remove(*k),
            };
        }
        if Some(&new_env) != old_env.as_ref() {
            let val = if new_env.is_empty() && old_env.is_none() { None } else { Some(Value::Object(new_env.clone())) };
            if let Some(v) = &val {
                out = set_member(&out, "env", Some(v)).ok_or("couldn't edit settings.json safely")?;
                want.insert("env".into(), v.clone());
            }
        }
    }
    let after: Value = serde_json::from_str(&out).map_err(|_| "the edit produced invalid JSON — nothing written".to_string())?;
    if after != Value::Object(want) {
        return Err("the edit didn't come out exactly as planned — nothing written".into());
    }
    Ok((out, diff))
}

pub fn plan_claude(paths: &Paths, p: &Preset) -> Result<Plan, String> {
    let target = paths.claude_settings();
    let original = std::fs::read_to_string(&target).ok();
    let changes: Vec<(&str, Option<Value>)> = p.claude.iter().map(|(k, v)| (*k, Some(v.json()))).collect();
    let (new_text, diff) = edit_settings(original.as_deref(), &changes, p.env)?;
    let mut notes = vec![format!("only these keys change; everything else in {} stays byte-for-byte", target.display())];
    notes.push("a backup is kept next to it (settings.json.oriel-backup-<time>); new Claude sessions pick it up".into());
    Ok(Plan { title: format!("apply {} to Claude Code?", p.name), target, original, new_text, diff, notes, done: format!("Claude Code set to {}", p.name) })
}

/// The command Claude Code should run as its statusLine.
pub fn sink_command() -> String {
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_else(|_| "oriel".into());
    format!("\"{exe}\" usage-sink")
}

pub enum Connect {
    Plan(Plan),
    /// the user already has a statusLine: leave it and explain how to chain
    Chain { current: String, suggestion: String },
    Already,
}

pub fn plan_connect(paths: &Paths) -> Result<Connect, String> {
    let target = paths.claude_settings();
    let original = std::fs::read_to_string(&target).ok();
    let cur: Value = match &original {
        Some(t) => serde_json::from_str(t).map_err(|e| format!("settings.json isn't valid JSON ({e})"))?,
        None => json!({}),
    };
    let cmd = sink_command();
    if let Some(sl) = cur.get("statusLine") {
        let current = sl.get("command").and_then(|c| c.as_str()).unwrap_or("").to_string();
        if current.contains("usage-sink") {
            return Ok(Connect::Already);
        }
        let suggestion = if current.is_empty() { cmd } else { format!("{cmd} --then {current}") };
        return Ok(Connect::Chain { current: if current.is_empty() { serde_json::to_string(sl).unwrap_or_default() } else { current }, suggestion });
    }
    let v = json!({ "type": "command", "command": cmd });
    let (new_text, diff) = edit_settings(original.as_deref(), &[("statusLine", Some(v))], &[])?;
    Ok(Connect::Plan(Plan {
        title: "connect Claude's plan limits?".into(),
        target: target.clone(),
        original,
        new_text,
        diff,
        notes: vec![
            "Claude Code runs this after each response and hands it your 5-hour / weekly usage".into(),
            "oriel saves only the numbers and prints a short status line (5h 34% · wk 12% · $0.42)".into(),
            format!("adds one key to {}; a backup is kept", target.display()),
        ],
        done: "limits connected — they show up after Claude's next response".into(),
    }))
}

// ------------------------------------------------------------------ Codex profiles

fn is_profile_header(line: &str, name: &str) -> bool {
    let t = line.trim();
    if !t.starts_with('[') || t.starts_with("[[") {
        return false;
    }
    let inner = t.trim_start_matches('[').split(']').next().unwrap_or("").trim().replace(['"', '\'', ' '], "");
    inner == format!("profiles.{name}") || inner.starts_with(&format!("profiles.{name}."))
}

/// Remove `[profiles.<name>]` (and its subtables) from the text; returns (text, removed lines).
fn strip_profile(text: &str, name: &str) -> (String, Vec<String>) {
    let mut out = String::new();
    let mut removed = vec![];
    let mut inside = false;
    for line in text.split_inclusive('\n') {
        let t = line.trim();
        if t.starts_with('[') {
            inside = is_profile_header(line, name);
        }
        if inside {
            removed.push(line.trim_end().to_string());
        } else {
            out.push_str(line);
        }
    }
    while removed.last().is_some_and(|l| l.is_empty()) {
        removed.pop();
    }
    (out, removed)
}

pub fn edit_codex(text: Option<&str>, p: &Preset) -> Result<(String, Vec<(char, String)>), String> {
    let src = text.unwrap_or("");
    let before: toml::Table = toml::from_str(src).map_err(|e| format!("config.toml doesn't parse ({}); fix it first", e.message()))?;
    let (mut out, removed) = strip_profile(src, p.profile);
    let block: Vec<String> = std::iter::once(format!("[profiles.{}]", p.profile)).chain(p.codex.iter().map(|(k, v)| format!("{k} = {v}"))).collect();
    let trimmed = out.trim_end().len();
    out.truncate(trimmed);
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(&block.join("\n"));
    out.push('\n');
    let after: toml::Table = toml::from_str(&out).map_err(|e| format!("adding the profile would break config.toml ({}) — nothing written", e.message()))?;
    // everything except our profile must be unchanged, and our profile must be exactly the block
    let strip = |mut t: toml::Table| {
        if let Some(toml::Value::Table(pr)) = t.get_mut("profiles") {
            pr.remove(p.profile);
            if pr.is_empty() {
                t.remove("profiles");
            }
        }
        t
    };
    let ours = after.get("profiles").and_then(|x| x.get(p.profile)).cloned();
    let want: toml::Table = toml::from_str(&block[1..].join("\n")).map_err(|_| "bad preset".to_string())?;
    if strip(before) != strip(after) || ours != Some(toml::Value::Table(want)) {
        return Err("the config.toml edit didn't come out exactly as planned — nothing written".into());
    }
    let mut diff: Vec<(char, String)> = removed.into_iter().map(|l| ('-', l)).collect();
    diff.extend(block.into_iter().map(|l| ('+', l)));
    Ok((out, diff))
}

pub fn plan_codex(paths: &Paths, p: &Preset) -> Result<Plan, String> {
    let target = paths.codex_config();
    let original = std::fs::read_to_string(&target).ok();
    let (new_text, diff) = edit_codex(original.as_deref(), p)?;
    Ok(Plan {
        title: format!("add the {} profile to Codex?", p.profile),
        target: target.clone(),
        original,
        new_text,
        diff,
        notes: vec![
            format!("a separate profile: your default Codex settings don't change. Use it with  codex -p {}", p.profile),
            format!("adds (or replaces) one [profiles.{}] block in {}; a backup is kept", p.profile, target.display()),
        ],
        done: format!("added — run  codex -p {}", p.profile),
    })
}

// ------------------------------------------------------------------ live readouts

#[derive(Clone, Debug, Default)]
pub struct Readouts {
    /// global CLAUDE.md (lines, bytes)
    pub claude_md: Option<(usize, usize)>,
    /// MCP server names from ~/.claude.json + settings.json (user scope)
    pub mcp: Vec<String>,
    /// projects in ~/.claude.json with their own MCP servers
    pub project_mcp: usize,
    /// current values of the keys the presets touch ("model" → "\"opus\"")
    pub current: Vec<(String, String)>,
    /// which preset the current settings match, if any
    pub matches: Option<&'static str>,
    pub codex_profiles: Vec<String>,
    pub status_line: Option<String>,
    pub settings_error: Option<String>,
}

pub fn readouts(paths: &Paths) -> Readouts {
    let mut r = Readouts::default();
    if let Ok(t) = std::fs::read_to_string(paths.claude.join("CLAUDE.md")) {
        r.claude_md = Some((t.lines().count(), t.len()));
    }
    let settings: Option<Value> = match std::fs::read_to_string(paths.claude_settings()) {
        Ok(t) => match serde_json::from_str(&t) {
            Ok(v) => Some(v),
            Err(e) => {
                r.settings_error = Some(format!("settings.json doesn't parse: {e}"));
                None
            }
        },
        Err(_) => None,
    };
    let mut names: Vec<String> = vec![];
    if let Ok(b) = std::fs::read(&paths.claude_json) {
        if let Ok(v) = serde_json::from_slice::<Value>(&b) {
            if let Some(m) = v.get("mcpServers").and_then(|m| m.as_object()) {
                names.extend(m.keys().cloned());
            }
            if let Some(p) = v.get("projects").and_then(|p| p.as_object()) {
                r.project_mcp = p.values().filter(|x| x.get("mcpServers").and_then(|m| m.as_object()).is_some_and(|m| !m.is_empty())).count();
            }
        }
    }
    if let Some(s) = &settings {
        if let Some(m) = s.get("mcpServers").and_then(|m| m.as_object()) {
            names.extend(m.keys().cloned());
        }
        let obj = s.as_object().cloned().unwrap_or_default();
        for k in ["model", "effortLevel", "autoCompactEnabled"] {
            r.current.push((k.into(), show(obj.get(k))));
        }
        let env = obj.get("env").and_then(|e| e.as_object()).cloned().unwrap_or_default();
        for k in ENV_KEYS {
            if let Some(v) = env.get(*k) {
                r.current.push((format!("env.{k}"), show(Some(v))));
            }
        }
        r.status_line = s.get("statusLine").map(|sl| sl.get("command").and_then(|c| c.as_str()).map(String::from).unwrap_or_else(|| sl.to_string()));
        r.matches = PRESETS.iter().find(|p| preset_matches(p, &obj, &env)).map(|p| p.name);
    }
    names.sort();
    names.dedup();
    r.mcp = names;
    if let Ok(t) = std::fs::read_to_string(paths.codex_config()) {
        if let Ok(tb) = toml::from_str::<toml::Table>(&t) {
            if let Some(toml::Value::Table(p)) = tb.get("profiles") {
                r.codex_profiles = p.keys().cloned().collect();
            }
        }
    }
    r
}

fn preset_matches(p: &Preset, obj: &Map<String, Value>, env: &Map<String, Value>) -> bool {
    p.claude.iter().all(|(k, v)| obj.get(*k) == Some(&v.json())) && p.env.iter().all(|(k, v)| env.get(*k).and_then(|x| x.as_str()) == *v)
}

pub fn backup_stamp() -> String {
    crate::panes::files::clock::compact(super::util::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str = "{\n    \"model\": \"opus\",\n    \"hooks\": {\n        \"Stop\": [ { \"hooks\": [ { \"type\": \"command\", \"command\": \"echo {\\\"x\\\"}\" } ] } ]\n    },\n    \"effortLevel\": \"high\",\n    \"theme\": \"dark\"\n}\n";

    #[test]
    fn ais_settings_edit_keeps_everything_else() {
        let p = &PRESETS[0];
        let changes: Vec<(&str, Option<Value>)> = p.claude.iter().map(|(k, v)| (*k, Some(v.json()))).collect();
        let (out, diff) = edit_settings(Some(USER), &changes, p.env).unwrap();
        // hooks block untouched byte-for-byte, order kept
        assert!(out.contains("\"hooks\": {\n        \"Stop\": [ { \"hooks\": [ { \"type\": \"command\", \"command\": \"echo {\\\"x\\\"}\" } ] } ]\n    },"), "{out}");
        assert!(out.find("\"model\"").unwrap() < out.find("\"hooks\"").unwrap());
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["model"], "sonnet");
        assert_eq!(v["effortLevel"], "low");
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["env"]["CLAUDE_CODE_SUBAGENT_MODEL"], "haiku");
        assert!(diff.contains(&('-', "\"model\": \"opus\"".into())) && diff.contains(&('+', "\"model\": \"sonnet\"".into())));
        // Max afterwards removes the env keys again (env stays, empty)
        let p = &PRESETS[2];
        let changes: Vec<(&str, Option<Value>)> = p.claude.iter().map(|(k, v)| (*k, Some(v.json()))).collect();
        let (out2, _) = edit_settings(Some(&out), &changes, p.env).unwrap();
        let v2: Value = serde_json::from_str(&out2).unwrap();
        assert_eq!(v2["model"], "opus");
        assert_eq!(v2["env"], json!({}));
        assert_eq!(v2["hooks"], v["hooks"]);
        // an empty / missing file works too
        let (o3, _) = edit_settings(None, &[("statusLine", Some(json!({"type":"command","command":"x"})))], &[]).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&o3).unwrap()["statusLine"]["command"], "x");
        assert!(edit_settings(Some("not json"), &[], &[]).is_err());
        // removal of first / last / only members
        assert_eq!(set_member("{\"a\":1,\"b\":2}", "a", None).unwrap(), "{\"b\":2}");
        assert_eq!(set_member("{\"a\":1,\"b\":2}", "b", None).unwrap(), "{\"a\":1}");
        assert_eq!(set_member("{\"a\":1}", "a", None).unwrap(), "{}");
    }

    #[test]
    fn ais_codex_profile_block_replaced_not_duplicated() {
        let user = "model = \"gpt-5.6-terra\"\n# my comment\n[features]\nx = true\n\n[profiles.oriel-frugal]\nmodel = \"old\"\n\n[windows]\nsandbox = \"y\"\n";
        let (out, diff) = edit_codex(Some(user), &PRESETS[0]).unwrap();
        assert!(out.contains("# my comment") && out.contains("[windows]\nsandbox = \"y\""));
        assert_eq!(out.matches("[profiles.oriel-frugal]").count(), 1);
        assert!(!out.contains("\"old\""));
        assert!(diff.contains(&('-', "model = \"old\"".into())));
        let t: toml::Table = toml::from_str(&out).unwrap();
        assert_eq!(t["profiles"]["oriel-frugal"]["model"].as_str(), Some("gpt-5.6-luna"));
        assert_eq!(t["model"].as_str(), Some("gpt-5.6-terra"));
        // dotted keys that would collide: refuse rather than break the file
        assert!(edit_codex(Some("profiles.oriel-frugal.model = \"x\"\n"), &PRESETS[0]).is_err());
        assert!(edit_codex(None, &PRESETS[1]).is_ok());
    }
}
