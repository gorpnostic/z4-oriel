//! What a task cost, read from the agent's own transcript (docs/ai-research.md §2). API-equivalent dollars:
//! on a subscription nothing is billed per token, but it's the honest way to compare tasks.

use serde_json::Value;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Cost {
    pub usd: f64,
    pub tokens: u64,
}

/// $/MTok: input, 5-minute cache write, 1-hour cache write, cache read, output.
fn claude_price(model: &str) -> (f64, f64, f64, f64, f64) {
    let m = model.to_lowercase();
    if m.contains("opus-5-5") || m.contains("opus-5.5") {
        (4.0, 5.0, 8.0, 0.20, 20.0)
    } else if m.contains("opus") {
        (5.0, 6.25, 10.0, 0.50, 25.0)
    } else if m.contains("sonnet-5") {
        (2.0, 2.5, 4.0, 0.20, 10.0)
    } else if m.contains("sonnet") {
        (3.0, 3.75, 6.0, 0.30, 15.0)
    } else if m.contains("haiku") {
        (1.0, 1.25, 2.0, 0.10, 5.0)
    } else if m.contains("fable") {
        (10.0, 12.5, 20.0, 0.25, 50.0)
    } else {
        (3.0, 3.75, 6.0, 0.30, 15.0) // unknown: price like Sonnet 4.6
    }
}

/// Add up one Claude Code transcript (JSONL), deduping the several lines one response is written as.
pub fn claude_lines(r: impl BufRead, seen: &mut HashSet<String>) -> Cost {
    let mut c = Cost::default();
    for line in r.lines().map_while(Result::ok) {
        if !line.contains("\"usage\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if v["type"].as_str() != Some("assistant") {
            continue;
        }
        let msg = &v["message"];
        let key = format!("{}:{}", msg["id"].as_str().unwrap_or(""), v["requestId"].as_str().unwrap_or(""));
        if key != ":" && !seen.insert(key) {
            continue;
        }
        let u = &msg["usage"];
        let n = |k: &str| u[k].as_u64().unwrap_or(0) as f64;
        let (p_in, p_5m, p_1h, p_read, p_out) = claude_price(msg["model"].as_str().unwrap_or(""));
        let (w5, w1) = match (u["cache_creation"]["ephemeral_5m_input_tokens"].as_u64(), u["cache_creation"]["ephemeral_1h_input_tokens"].as_u64()) {
            (None, None) => (n("cache_creation_input_tokens"), 0.0),
            (a, b) => (a.unwrap_or(0) as f64, b.unwrap_or(0) as f64),
        };
        let mut usd = (n("input_tokens") * p_in + w5 * p_5m + w1 * p_1h + n("cache_read_input_tokens") * p_read + n("output_tokens") * p_out) / 1e6;
        if u["inference_geo"].as_str().map(|g| g.to_lowercase().starts_with("us")).unwrap_or(false) {
            usd *= 1.1;
        }
        c.usd += usd;
        c.tokens += (n("input_tokens") + n("output_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens")) as u64;
    }
    c
}

/// A session's transcript plus its subagents' (`<session>/subagents/**/*.jsonl`).
pub fn claude_session(transcript: &Path) -> Cost {
    let mut seen = HashSet::new();
    let mut files = vec![transcript.to_path_buf()];
    let sub = transcript.with_extension("").join("subagents");
    collect_jsonl(&sub, &mut files, 3);
    let mut total = Cost::default();
    for f in files {
        if let Ok(file) = std::fs::File::open(&f) {
            let c = claude_lines(BufReader::new(file), &mut seen);
            total.usd += c.usd;
            total.tokens += c.tokens;
        }
    }
    total
}

/// Every Claude session that ran in `cwd` (follow-ups with --continue start new files): the transcripts in
/// `~/.claude/projects/<cwd-slug>/`, modified since `since`.
pub fn claude_for_dir(cwd: &Path, known: &str, since: i64) -> Cost {
    let mut total = Cost::default();
    let mut files: Vec<PathBuf> = vec![];
    if let Some(dir) = claude_project_dir(cwd) {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                let fresh = e.metadata().ok().and_then(|m| m.modified().ok()).map(|t| crate::panes::files::clock::secs_of(t) >= since - 60).unwrap_or(false);
                if p.extension().map(|x| x == "jsonl").unwrap_or(false) && fresh {
                    files.push(p);
                }
            }
        }
    }
    if !known.is_empty() && !files.iter().any(|f| f == Path::new(known)) {
        files.push(PathBuf::from(known));
    }
    for f in files {
        let c = claude_session(&f);
        total.usd += c.usd;
        total.tokens += c.tokens;
    }
    total
}

/// Claude Code's project folder for a working directory: every non-alphanumeric character becomes '-'.
fn claude_project_dir(cwd: &Path) -> Option<PathBuf> {
    let slug: String = cwd.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let mut roots = vec![];
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        roots.push(PathBuf::from(d).join("projects"));
    }
    if let Some(h) = dirs::home_dir() {
        roots.push(h.join(".claude").join("projects"));
        roots.push(h.join(".config").join("claude").join("projects"));
    }
    roots.into_iter().map(|r| r.join(&slug)).find(|p| p.is_dir())
}

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>, depth: u32) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() && depth > 0 {
            collect_jsonl(&p, out, depth - 1);
        } else if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
            out.push(p);
        }
    }
}

// ------------------------------------------------------------------ codex

/// $/MTok: input, cached input, output.
fn codex_price(model: &str) -> (f64, f64, f64) {
    let m = model.to_lowercase();
    if m.contains("sol") {
        (4.0, 0.40, 20.0)
    } else if m.contains("luna") {
        (0.20, 0.02, 1.20)
    } else if m.contains("5.3-codex") {
        (1.75, 0.175, 14.0)
    } else {
        (2.0, 0.20, 12.0) // terra and anything unknown
    }
}

/// One Codex rollout file: (its cwd, its cost).
pub fn codex_lines(r: impl BufRead) -> (String, Cost) {
    let mut cwd = String::new();
    let mut model = String::new();
    let mut c = Cost::default();
    for line in r.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let p = &v["payload"];
        match v["type"].as_str() {
            Some("session_meta") => cwd = p["cwd"].as_str().unwrap_or("").to_string(),
            Some("turn_context") => model = p["model"].as_str().unwrap_or(&model).to_string(),
            Some("event_msg") if p["type"].as_str() == Some("token_count") => {
                let u = &p["info"]["last_token_usage"];
                let n = |k: &str| u[k].as_u64().unwrap_or(0) as f64;
                let (pi, pc, po) = codex_price(&model);
                let cached = n("cached_input_tokens");
                c.usd += ((n("input_tokens") - cached).max(0.0) * pi + cached * pc + n("output_tokens") * po) / 1e6;
                c.tokens += (n("input_tokens") + n("output_tokens")) as u64;
            }
            _ => {}
        }
    }
    (cwd, c)
}

/// Codex sessions that ran in `cwd` since `since`: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`.
pub fn codex_for_dir(cwd: &Path, since: i64) -> Cost {
    let root = std::env::var_os("CODEX_HOME").map(PathBuf::from).or_else(|| dirs::home_dir().map(|h| h.join(".codex")));
    let Some(root) = root else { return Cost::default() };
    let mut files = vec![];
    collect_jsonl(&root.join("sessions"), &mut files, 3);
    let want = norm(&cwd.to_string_lossy());
    let mut total = Cost::default();
    for f in files {
        let fresh = std::fs::metadata(&f).ok().and_then(|m| m.modified().ok()).map(|t| crate::panes::files::clock::secs_of(t) >= since - 60).unwrap_or(false);
        if !fresh {
            continue;
        }
        let Ok(file) = std::fs::File::open(&f) else { continue };
        // the first line is session_meta with the cwd: skip other folders' sessions without reading them
        let mut r = BufReader::new(file);
        let mut first = String::new();
        if r.read_line(&mut first).is_err() || !norm(&first.replace("\\\\", "\\")).contains(&want) {
            continue;
        }
        let (dir, c) = codex_lines(std::io::Cursor::new(first).chain(r));
        if norm(&dir) == want {
            total.usd += c.usd;
            total.tokens += c.tokens;
        }
    }
    total
}

fn norm(p: &str) -> String {
    p.replace('\\', "/").trim_end_matches('/').to_lowercase()
}
