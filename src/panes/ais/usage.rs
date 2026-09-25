//! Token usage from the agents' own local logs (docs/ai-research.md §2), parsed incrementally on a background
//! thread and cached in `<data dir>/usage-cache.json`:
//!
//!   * Claude Code  `~/.claude/projects/**/*.jsonl` — assistant lines, deduped on message.id + requestId
//!   * Codex        `~/.codex/sessions/**/rollout-*.jsonl` — `token_count` events (last_token_usage per turn),
//!                  plus the plan limits from the newest event
//!   * Kimi Code    `~/.kimi-code/sessions/**/wire.jsonl` — StatusUpdate usage records (unverified: no sample)
//!   * OpenCode     `…/opencode/storage/message/<sid>/msg_*.json` — per-message tokens + its own cost (unverified)
//!
//! Only numbers, model names, project names, session ids and timestamps are kept. Prompt and response text is
//! never stored (the parsers deserialize into structs that don't have those fields).
//!
//! Incremental: each file remembers how far it was read; a refresh reads only what was appended. Totals are
//! kept per file in hourly buckets, so a file that shrank (rewritten) is simply re-read from the start.

use super::util::{Paths, day_of, parse_iso};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Instant;

const CACHE_VERSION: u32 = 2; // 2: Gemini dropped

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub enum Src {
    Claude,
    Codex,
    Kimi,
    OpenCode,
}

pub const SRCS: [Src; 4] = [Src::Claude, Src::Codex, Src::Kimi, Src::OpenCode];

impl Src {
    pub fn label(self) -> &'static str {
        match self {
            Src::Claude => "Claude Code",
            Src::Codex => "Codex",
            Src::Kimi => "Kimi Code",
            Src::OpenCode => "OpenCode",
        }
    }
    /// catalog id
    pub fn id(self) -> &'static str {
        match self {
            Src::Claude => "claude",
            Src::Codex => "codex",
            Src::Kimi => "kimi",
            Src::OpenCode => "opencode",
        }
    }
    /// Do we have a price table (or a recorded cost) for it?
    pub fn priced(self) -> bool {
        matches!(self, Src::Claude | Src::Codex | Src::OpenCode)
    }
}

/// Token counts + API-equivalent dollars. `inp` is uncached input; `cw` cache writes; `cr` cache reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Tot {
    #[serde(rename = "i")]
    pub inp: u64,
    #[serde(rename = "o")]
    pub out: u64,
    #[serde(rename = "w")]
    pub cw: u64,
    #[serde(rename = "r")]
    pub cr: u64,
    #[serde(rename = "c")]
    pub cost: f64,
    #[serde(rename = "n")]
    pub n: u32,
}

impl Tot {
    pub fn add(&mut self, o: &Tot) {
        self.inp += o.inp;
        self.out += o.out;
        self.cw += o.cw;
        self.cr += o.cr;
        self.cost += o.cost;
        self.n += o.n;
    }
    pub fn tokens(&self) -> u64 {
        self.inp + self.out + self.cw + self.cr
    }
    /// Share of input served from cache, 0..=100 (None with no input yet).
    pub fn cache_hit(&self) -> Option<f64> {
        let all = self.inp + self.cw + self.cr;
        (all > 0).then(|| 100.0 * self.cr as f64 / all as f64)
    }
}

// ------------------------------------------------------------------ pricing ($ per million tokens)

/// (in, 5m cache write, 1h cache write, cache read, out)
pub fn claude_price(model: &str) -> Option<(f64, f64, f64, f64, f64)> {
    let m = model.to_ascii_lowercase();
    Some(if m.contains("opus-5-5") {
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
        return None;
    })
}

/// (in, cached in, out)
pub fn codex_price(model: &str) -> Option<(f64, f64, f64)> {
    let m = model.to_ascii_lowercase();
    Some(if m.contains("sol") {
        (4.0, 0.40, 20.0)
    } else if m.contains("terra") {
        (2.0, 0.20, 12.0)
    } else if m.contains("luna") {
        (0.20, 0.02, 1.20)
    } else if m.contains("5.3-codex") {
        (1.75, 0.175, 14.0)
    } else {
        return None;
    })
}

/// Friendly model name: "claude-opus-5-5" → "opus 5.5", "claude-haiku-4-5-20251001" → "haiku 4.5".
pub fn short_model(m: &str) -> String {
    let Some(m) = m.strip_prefix("claude-") else { return m.to_string() };
    let parts: Vec<&str> = m.split('-').filter(|p| !(p.len() == 8 && p.chars().all(|c| c.is_ascii_digit()))).collect();
    if parts.len() >= 2 && parts[1..].iter().all(|p| p.chars().all(|c| c.is_ascii_digit())) {
        return format!("{} {}", parts[0], parts[1..].join("."));
    }
    parts.join("-")
}

// ------------------------------------------------------------------ the cache

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FileState {
    pub src: Option<Src>,
    /// bytes consumed (always at a line boundary)
    pub off: u64,
    pub project: String,
    pub session: String,
    /// Codex: the model from the latest turn_context, carried across refreshes
    #[serde(default)]
    pub model: String,
    /// Codex: last total_token_usage.total_tokens seen (duplicate token_count events repeat it)
    #[serde(default)]
    pub last_total: u64,
    /// (utc hour, model, totals)
    pub hours: Vec<(i64, String, Tot)>,
    pub last_ts: i64,
    /// Codex: newest rate_limits seen in this file, with its timestamp
    #[serde(default)]
    pub limits: Option<(i64, serde_json::Value)>,
}

impl FileState {
    fn bump(&mut self, t: i64, model: &str, add: &Tot) {
        let h = t.div_euclid(3600);
        self.last_ts = self.last_ts.max(t);
        if let Some(b) = self.hours.iter_mut().rev().find(|b| b.0 == h && b.1 == model) {
            b.2.add(add);
        } else {
            self.hours.push((h, model.to_string(), *add));
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Cache {
    pub v: u32,
    pub files: HashMap<String, FileState>,
    /// Claude dedupe keys (hash of message.id:requestId) already counted
    pub seen: HashSet<u64>,
}

impl Cache {
    pub fn load(path: &Path) -> Cache {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Cache>(&b).ok())
            .filter(|c| c.v == CACHE_VERSION)
            .unwrap_or(Cache { v: CACHE_VERSION, ..Default::default() })
    }
    pub fn save(&self, path: &Path) {
        if let Ok(b) = serde_json::to_vec(self) {
            let _ = super::util::write_atomic(path, &b);
        }
    }
}

fn hash(s: &str) -> u64 {
    // FNV-1a: stable across runs (the std hasher is randomly seeded)
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// ------------------------------------------------------------------ finding files

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>, keep: &dyn Fn(&Path) -> bool) {
    if depth == 0 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        if ft.is_dir() {
            walk(&p, depth - 1, out, keep);
        } else if ft.is_file() && keep(&p) {
            out.push(p);
        }
    }
}

fn ext_is(p: &Path, e: &str) -> bool {
    p.extension().is_some_and(|x| x.eq_ignore_ascii_case(e))
}

/// Every log file we know how to read, with its source.
pub fn discover(paths: &Paths) -> Vec<(Src, PathBuf)> {
    let mut out = vec![];
    let mut add = |src: Src, v: Vec<PathBuf>| out.extend(v.into_iter().map(|p| (src, p)));
    // Claude: the config dir's projects, plus the XDG location newer versions may use
    let mut claude_roots = vec![paths.claude.join("projects")];
    let xdg = paths.home.join(".config").join("claude").join("projects");
    if xdg != claude_roots[0] {
        claude_roots.push(xdg);
    }
    for r in claude_roots {
        let mut v = vec![];
        walk(&r, 6, &mut v, &|p| ext_is(p, "jsonl"));
        add(Src::Claude, v);
    }
    let mut v = vec![];
    walk(&paths.codex.join("sessions"), 5, &mut v, &|p| ext_is(p, "jsonl") && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("rollout-")));
    add(Src::Codex, v);
    let mut v = vec![];
    walk(&paths.home.join(".kimi-code").join("sessions"), 6, &mut v, &|p| p.file_name().is_some_and(|n| n == "wire.jsonl"));
    add(Src::Kimi, v);
    let mut roots = vec![paths.home.join(".local").join("share").join("opencode")];
    if let Some(d) = dirs::data_dir().map(|d| d.join("opencode")).filter(|_| !cfg!(test)) {
        if d != roots[0] {
            roots.push(d);
        }
    }
    for r in roots {
        let mut v = vec![];
        walk(&r.join("storage").join("message"), 3, &mut v, &|p| ext_is(p, "json"));
        add(Src::OpenCode, v);
    }
    out
}

// ------------------------------------------------------------------ Claude

#[derive(Deserialize)]
struct CLine<'a> {
    #[serde(rename = "type", borrow)]
    ty: Option<Cow<'a, str>>,
    #[serde(rename = "requestId", borrow)]
    request_id: Option<Cow<'a, str>>,
    #[serde(borrow)]
    timestamp: Option<Cow<'a, str>>,
    #[serde(borrow)]
    cwd: Option<Cow<'a, str>>,
    #[serde(borrow)]
    message: Option<CMsg<'a>>,
}

#[derive(Deserialize)]
struct CMsg<'a> {
    #[serde(borrow)]
    id: Option<Cow<'a, str>>,
    #[serde(borrow)]
    model: Option<Cow<'a, str>>,
    usage: Option<CUsage<'a>>,
}

#[derive(Deserialize, Default)]
struct CUsage<'a> {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    cache_creation: Option<CCache>,
    #[serde(borrow)]
    inference_geo: Option<Cow<'a, str>>,
}

#[derive(Deserialize, Default)]
struct CCache {
    #[serde(default)]
    ephemeral_5m_input_tokens: u64,
    #[serde(default)]
    ephemeral_1h_input_tokens: u64,
}

/// One priced Claude response.
pub struct CRec {
    pub key: Option<u64>,
    pub t: i64,
    pub model: String,
    pub tot: Tot,
    pub cwd: Option<String>,
}

pub fn claude_line(line: &[u8]) -> Option<CRec> {
    let l: CLine = serde_json::from_slice(line).ok()?;
    if l.ty.as_deref() != Some("assistant") {
        return None;
    }
    let msg = l.message?;
    let u = msg.usage?;
    let model = msg.model.as_deref().unwrap_or("?").to_string();
    if model == "<synthetic>" {
        return None;
    }
    let t = parse_iso(l.timestamp.as_deref()?)?;
    let (w5, w1) = match &u.cache_creation {
        Some(c) if c.ephemeral_5m_input_tokens + c.ephemeral_1h_input_tokens > 0 => (c.ephemeral_5m_input_tokens, c.ephemeral_1h_input_tokens),
        _ => (u.cache_creation_input_tokens, 0),
    };
    let mut cost = 0.0;
    if let Some((pi, p5, p1, pr, po)) = claude_price(&model) {
        cost = (u.input_tokens as f64 * pi + w5 as f64 * p5 + w1 as f64 * p1 + u.cache_read_input_tokens as f64 * pr + u.output_tokens as f64 * po) / 1e6;
        if u.inference_geo.as_deref().is_some_and(|g| g.eq_ignore_ascii_case("us")) {
            cost *= 1.1;
        }
    }
    let key = match (msg.id.as_deref(), l.request_id.as_deref()) {
        (Some(a), Some(b)) => Some(hash(&format!("{a}:{b}"))),
        _ => None,
    };
    Some(CRec {
        key,
        t,
        model,
        tot: Tot { inp: u.input_tokens, out: u.output_tokens, cw: u.cache_creation_input_tokens.max(w5 + w1), cr: u.cache_read_input_tokens, cost, n: 1 },
        cwd: l.cwd.map(|c| c.into_owned()),
    })
}

// ------------------------------------------------------------------ Codex

#[derive(Deserialize)]
struct XLine<'a> {
    #[serde(borrow)]
    timestamp: Option<Cow<'a, str>>,
    #[serde(rename = "type", borrow)]
    ty: Option<Cow<'a, str>>,
    #[serde(borrow)]
    payload: Option<XPayload<'a>>,
}

#[derive(Deserialize)]
struct XPayload<'a> {
    #[serde(rename = "type", borrow)]
    ty: Option<Cow<'a, str>>,
    #[serde(borrow)]
    model: Option<Cow<'a, str>>,
    #[serde(borrow)]
    cwd: Option<Cow<'a, str>>,
    #[serde(borrow)]
    id: Option<Cow<'a, str>>,
    info: Option<XInfo>,
    rate_limits: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct XInfo {
    total_token_usage: Option<XUsage>,
    last_token_usage: Option<XUsage>,
}

#[derive(Deserialize, Default, Clone, Copy)]
struct XUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    cache_write_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
}

fn codex_line(fs: &mut FileState, line: &[u8]) {
    let Ok(l) = serde_json::from_slice::<XLine>(line) else { return };
    let Some(p) = l.payload else { return };
    let t = l.timestamp.as_deref().and_then(parse_iso).unwrap_or(fs.last_ts);
    match l.ty.as_deref() {
        Some("session_meta") => {
            if let Some(c) = p.cwd.as_deref() {
                fs.project = project_name(c);
            }
            if let Some(id) = p.id.as_deref() {
                fs.session = id.to_string();
            }
        }
        Some("turn_context") => {
            if let Some(m) = p.model.as_deref() {
                fs.model = m.to_string();
            }
            if fs.project.is_empty() {
                if let Some(c) = p.cwd.as_deref() {
                    fs.project = project_name(c);
                }
            }
        }
        Some("event_msg") if p.ty.as_deref() == Some("token_count") => {
            if let Some(rl) = p.rate_limits.filter(|v| !v.is_null()) {
                if fs.limits.as_ref().is_none_or(|(lt, _)| t >= *lt) {
                    fs.limits = Some((t, rl));
                }
            }
            let Some(info) = p.info else { return };
            let total = info.total_token_usage.map(|u| u.total_tokens).unwrap_or(0);
            if total != 0 && total == fs.last_total {
                return; // the same event re-emitted
            }
            fs.last_total = total;
            let Some(u) = info.last_token_usage else { return };
            let cached = u.cached_input_tokens.min(u.input_tokens);
            let inp = u.input_tokens - cached;
            let model = if fs.model.is_empty() { "?".to_string() } else { fs.model.clone() };
            let cost = codex_price(&model).map(|(pi, pc, po)| (inp as f64 * pi + (cached + u.cache_write_input_tokens) as f64 * pc + u.output_tokens as f64 * po) / 1e6).unwrap_or(0.0);
            let tot = Tot { inp, out: u.output_tokens, cw: u.cache_write_input_tokens, cr: cached, cost, n: 1 };
            fs.bump(t, &model, &tot);
        }
        _ => {}
    }
}

// ------------------------------------------------------------------ Kimi / OpenCode (by the spec)

fn num(v: &serde_json::Value, k: &str) -> u64 {
    v.get(k).and_then(|x| x.as_u64().or_else(|| x.as_f64().map(|f| f as u64))).unwrap_or(0)
}

/// A timestamp field as epoch seconds: ISO strings, epoch seconds or epoch milliseconds.
fn ts_of(v: &serde_json::Value) -> Option<i64> {
    for k in ["timestamp", "time", "created_at", "createdAt", "ts"] {
        match v.get(k) {
            Some(serde_json::Value::String(s)) => {
                if let Some(t) = parse_iso(s) {
                    return Some(t);
                }
            }
            Some(serde_json::Value::Number(n)) => {
                let n = n.as_f64().unwrap_or(0.0);
                return Some(if n > 1e11 { (n / 1000.0) as i64 } else { n as i64 });
            }
            Some(o @ serde_json::Value::Object(_)) => {
                if let Some(t) = o.get("created").and_then(|c| c.as_f64()) {
                    return Some(if t > 1e11 { (t / 1000.0) as i64 } else { t as i64 });
                }
            }
            _ => {}
        }
    }
    None
}

/// Find the first object anywhere in `v` that has key `k`, and the object holding it.
fn find_with<'a>(v: &'a serde_json::Value, k: &str, depth: usize) -> Option<(&'a serde_json::Value, &'a serde_json::Value)> {
    match v {
        serde_json::Value::Object(m) => {
            if let Some(x) = m.get(k) {
                if x.is_object() {
                    return Some((v, x));
                }
            }
            if depth == 0 {
                return None;
            }
            m.values().find_map(|c| find_with(c, k, depth - 1))
        }
        serde_json::Value::Array(a) if depth > 0 => a.iter().find_map(|c| find_with(c, k, depth - 1)),
        _ => None,
    }
}

fn kimi_line(fs: &mut FileState, line: &[u8]) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else { return };
    let Some((holder, u)) = find_with(&v, "usage", 4) else { return };
    if u.get("inputOther").is_none() && u.get("output").is_none() {
        return;
    }
    // "turn-scoped records only": skip anything that says it's cumulative
    let scope = holder.get("scope").or_else(|| u.get("scope")).and_then(|s| s.as_str()).unwrap_or("turn");
    if !scope.eq_ignore_ascii_case("turn") {
        return;
    }
    let t = ts_of(&v).or_else(|| ts_of(holder)).unwrap_or(fs.last_ts);
    let model = holder.get("model").or_else(|| v.get("model")).and_then(|m| m.as_str()).unwrap_or("kimi").to_string();
    let tot = Tot { inp: num(u, "inputOther"), out: num(u, "output"), cw: num(u, "inputCacheCreation"), cr: num(u, "inputCacheRead"), cost: 0.0, n: 1 };
    if tot.tokens() > 0 {
        fs.bump(t, &model, &tot);
    }
}

fn opencode_file(fs: &mut FileState, v: &serde_json::Value) {
    let Some(tk) = v.get("tokens").filter(|x| x.is_object()) else { return };
    let cache = tk.get("cache").cloned().unwrap_or_default();
    let tot = Tot {
        inp: num(tk, "input"),
        out: num(tk, "output") + num(tk, "reasoning"),
        cw: num(&cache, "write"),
        cr: num(&cache, "read"),
        cost: v.get("cost").and_then(|c| c.as_f64()).unwrap_or(0.0),
        n: 1,
    };
    if tot.tokens() == 0 {
        return;
    }
    let t = ts_of(v).unwrap_or(0);
    let model = v.get("modelID").and_then(|m| m.as_str()).unwrap_or("?").to_string();
    if let Some(c) = v.get("path").and_then(|p| p.get("cwd")).and_then(|c| c.as_str()) {
        fs.project = project_name(c);
    }
    if let Some(s) = v.get("sessionID").and_then(|s| s.as_str()) {
        fs.session = s.to_string();
    }
    fs.bump(t, &model, &tot);
}

// ------------------------------------------------------------------ reading one file

pub fn project_name(cwd: &str) -> String {
    cwd.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or(cwd).to_string()
}

/// Claude session id from the path: `<proj>/<session>.jsonl` or `<proj>/<session>/subagents/…/x.jsonl`.
fn claude_session(p: &Path) -> String {
    let comps: Vec<String> = p.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    if let Some(i) = comps.iter().position(|c| c == "subagents") {
        if i > 0 {
            return comps[i - 1].clone();
        }
    }
    p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The project folder slug under ~/.claude/projects, as a fallback project name ("C--Code-oriel" → "oriel").
fn claude_slug(p: &Path) -> String {
    let comps: Vec<String> = p.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    match comps.iter().position(|c| c == "projects") {
        Some(i) if i + 1 < comps.len() => comps[i + 1].rsplit('-').next().unwrap_or(&comps[i + 1]).to_string(),
        _ => String::new(),
    }
}

/// What reading one file produced. Claude records are returned (not bucketed) so the dedupe can run in a fixed
/// order afterwards.
pub struct Parsed {
    pub key: String,
    pub state: FileState,
    pub claude: Vec<CRec>,
    pub bytes: u64,
    /// the file was re-read from the start (shrank): its old dedupe keys don't count against it
    pub reset: bool,
}

pub fn parse_file(src: Src, path: &Path, prev: Option<&FileState>) -> Option<Parsed> {
    let key = path.to_string_lossy().into_owned();
    let len = std::fs::metadata(path).ok()?.len();
    let mut st = prev.cloned().unwrap_or_else(|| FileState { src: Some(src), ..Default::default() });
    let whole_file = matches!(src, Src::OpenCode);
    let mut reset = false;
    if len < st.off || (whole_file && len != st.off) {
        // rewritten (or a whole-JSON file that changed): start over
        st = FileState { src: Some(src), ..Default::default() };
        reset = prev.is_some();
    }
    if len == st.off {
        return Some(Parsed { key, state: st, claude: vec![], bytes: 0, reset: false });
    }
    let mut f = std::fs::File::open(path).ok()?;
    if whole_file {
        let mut buf = vec![];
        f.read_to_end(&mut buf).ok()?;
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf) {
            opencode_file(&mut st, &v);
        }
        st.off = len;
        if st.session.is_empty() {
            st.session = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        }
        return Some(Parsed { key, state: st, claude: vec![], bytes: buf.len() as u64, reset });
    }
    f.seek(SeekFrom::Start(st.off)).ok()?;
    let mut r = BufReader::with_capacity(1 << 20, f);
    let mut line = Vec::with_capacity(1 << 16);
    let mut claude = vec![];
    let mut read = 0u64;
    loop {
        line.clear();
        let n = r.read_until(b'\n', &mut line).ok()?;
        if n == 0 || line.last() != Some(&b'\n') {
            break; // EOF, or a line still being written: pick it up next time
        }
        read += n as u64;
        match src {
            Src::Claude => {
                // cheap pre-filter before JSON: only assistant lines carry usage
                if contains(&line, b"\"usage\"") && contains(&line, b"\"assistant\"") {
                    if let Some(rec) = claude_line(&line) {
                        claude.push(rec);
                    }
                }
            }
            Src::Codex => {
                if contains(&line, b"\"token_count\"") || contains(&line, b"\"turn_context\"") || contains(&line, b"\"session_meta\"") {
                    codex_line(&mut st, &line);
                }
            }
            Src::Kimi => {
                if contains(&line, b"StatusUpdate") || contains(&line, b"\"usage\"") {
                    kimi_line(&mut st, &line);
                }
            }
            Src::OpenCode => {}
        }
    }
    st.off += read;
    if src == Src::Claude {
        if st.session.is_empty() {
            st.session = claude_session(path);
        }
        if st.project.is_empty() {
            st.project = claude.iter().find_map(|r| r.cwd.as_deref().map(project_name)).unwrap_or_else(|| claude_slug(path));
        }
    }
    if st.session.is_empty() {
        st.session = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    }
    if st.project.is_empty() && src == Src::Kimi {
        // ~/.kimi-code/sessions/wd_<name>_<hash>/session_…/agents/main/wire.jsonl
        st.project = path.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).find_map(|c| c.strip_prefix("wd_").map(|s| s.rsplit_once('_').map(|x| x.0).unwrap_or(s).to_string())).unwrap_or_default();
    }
    Some(Parsed { key, state: st, claude, bytes: read, reset })
}

/// Byte substring search (the logs are huge, so this runs before any JSON parsing).
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    let first = needle[0];
    let n = needle.len();
    if hay.len() < n {
        return false;
    }
    let mut i = 0;
    let last = hay.len() - n;
    while i <= last {
        match hay[i..=last].iter().position(|&b| b == first) {
            None => return false,
            Some(k) => {
                i += k;
                if &hay[i..i + n] == needle {
                    return true;
                }
                i += 1;
            }
        }
    }
    false
}

// ------------------------------------------------------------------ the scan

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub files: usize,
    pub changed: usize,
    #[cfg_attr(not(test), allow(dead_code))]
    pub bytes: u64,
    pub ms: u128,
}

/// Bring the cache up to date with the logs on disk. Parallel over files; the Claude dedupe then runs in a fixed
/// order (oldest file first) so results don't depend on thread timing.
pub fn refresh(cache: &mut Cache, paths: &Paths) -> Stats {
    let t0 = Instant::now();
    let files = discover(paths);
    let live: HashSet<String> = files.iter().map(|(_, p)| p.to_string_lossy().into_owned()).collect();
    cache.files.retain(|k, _| live.contains(k));
    // skip files whose size didn't move (the common case on a refresh)
    let todo: Vec<(Src, PathBuf)> = files
        .iter()
        .filter(|(_, p)| {
            let k = p.to_string_lossy();
            let len = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            cache.files.get(k.as_ref()).is_none_or(|s| s.off != len)
        })
        .cloned()
        .collect();
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(Vec::<Parsed>::new());
    {
        let cache_ref = &*cache;
        std::thread::scope(|s| {
            for _ in 0..threads.min(todo.len().max(1)) {
                s.spawn(|| {
                    let mut mine = vec![];
                    loop {
                        let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some((src, p)) = todo.get(k) else { break };
                        let prev = cache_ref.files.get(p.to_string_lossy().as_ref());
                        if let Some(r) = parse_file(*src, p, prev) {
                            mine.push(r);
                        }
                    }
                    results.lock().unwrap().extend(mine);
                });
            }
        });
    }
    let mut results = results.into_inner().unwrap();
    // oldest first: the first file to record a response keeps it
    results.sort_by_key(|r| r.claude.first().map(|c| c.t).unwrap_or(i64::MAX));
    let mut bytes = 0;
    let changed = results.len();
    for mut r in results {
        bytes += r.bytes;
        let mut local = HashSet::new();
        for rec in r.claude.drain(..) {
            if let Some(k) = rec.key {
                if !local.insert(k) || (!r.reset && cache.seen.contains(&k)) {
                    continue;
                }
                cache.seen.insert(k);
            }
            r.state.bump(rec.t, &rec.model, &rec.tot);
        }
        cache.files.insert(r.key, r.state);
    }
    Stats { files: files.len(), changed, bytes, ms: t0.elapsed().as_millis() }
}

// ------------------------------------------------------------------ summaries for the UI

#[derive(Clone, Debug, Default)]
pub struct Session {
    pub id: String,
    pub project: String,
    pub tot: Tot,
    pub last: i64,
}

#[derive(Clone, Debug, Default)]
pub struct Block {
    pub start: i64,
    pub end: i64,
    pub tot: Tot,
    pub active: bool,
}

#[derive(Clone, Debug)]
pub struct SrcSum {
    pub src: Src,
    /// (local day, totals), oldest first
    pub days: Vec<(i64, Tot)>,
    pub today: Tot,
    pub week: Tot,
    pub all: Tot,
    /// tokens per day, last 14 days (oldest first)
    pub spark: Vec<u64>,
    pub projects: Vec<(String, Tot)>,
    pub sessions: Vec<Session>,
    pub models: Vec<(String, Tot)>,
    /// Claude 5-hour blocks, newest first (an estimate)
    pub blocks: Vec<Block>,
    pub files: usize,
    pub last: i64,
}

pub fn summarize(cache: &Cache, now: i64, off: i64) -> Vec<SrcSum> {
    let today = day_of(now, off);
    let mut out = vec![];
    for src in SRCS {
        let files: Vec<&FileState> = cache.files.values().filter(|f| f.src == Some(src)).collect();
        if files.is_empty() {
            continue;
        }
        let mut days: BTreeMap<i64, Tot> = BTreeMap::new();
        let mut hours: BTreeMap<i64, Tot> = BTreeMap::new();
        let mut projects: HashMap<String, Tot> = HashMap::new();
        let mut models: HashMap<String, Tot> = HashMap::new();
        let mut sessions: HashMap<String, Session> = HashMap::new();
        let mut all = Tot::default();
        let mut last = 0;
        for f in &files {
            let mut ftot = Tot::default();
            for (h, m, t) in &f.hours {
                days.entry(day_of(h * 3600, off)).or_default().add(t);
                hours.entry(*h).or_default().add(t);
                models.entry(short_model(m)).or_default().add(t);
                ftot.add(t);
            }
            if ftot.n == 0 {
                continue;
            }
            all.add(&ftot);
            last = last.max(f.last_ts);
            let proj = if f.project.is_empty() { "?".to_string() } else { f.project.clone() };
            projects.entry(proj.clone()).or_default().add(&ftot);
            let s = sessions.entry(f.session.clone()).or_insert_with(|| Session { id: f.session.clone(), project: proj, ..Default::default() });
            s.tot.add(&ftot);
            s.last = s.last.max(f.last_ts);
        }
        let sum_days = |from: i64| days.range(from..).fold(Tot::default(), |mut a, (_, t)| (a.add(t), a).1);
        let spark = (0..14).map(|k| days.get(&(today - 13 + k)).map(|t| t.tokens()).unwrap_or(0)).collect();
        let by_cost = |v: &mut Vec<(String, Tot)>| v.sort_by(|a, b| b.1.cost.total_cmp(&a.1.cost).then(b.1.tokens().cmp(&a.1.tokens())));
        let mut projects: Vec<(String, Tot)> = projects.into_iter().collect();
        by_cost(&mut projects);
        let mut models: Vec<(String, Tot)> = models.into_iter().filter(|m| m.1.n > 0).collect();
        by_cost(&mut models);
        let mut sessions: Vec<Session> = sessions.into_values().collect();
        sessions.sort_by(|a, b| b.tot.cost.total_cmp(&a.tot.cost).then(b.tot.tokens().cmp(&a.tot.tokens())));
        sessions.truncate(20);
        let blocks = if src == Src::Claude { blocks(&hours, now) } else { vec![] };
        out.push(SrcSum {
            src,
            today: days.get(&today).copied().unwrap_or_default(),
            week: sum_days(today - 6),
            days: days.into_iter().collect(),
            all,
            spark,
            projects,
            sessions,
            models,
            blocks,
            files: files.len(),
            last,
        });
    }
    out
}

/// ccusage-style 5-hour windows: a block starts at the first activity (floored to the hour) and lasts 5 h; the
/// next activity after it ends starts a new one. Newest first, at most 30.
pub fn blocks(hours: &BTreeMap<i64, Tot>, now: i64) -> Vec<Block> {
    let mut v: Vec<Block> = vec![];
    for (&h, t) in hours {
        let ts = h * 3600;
        match v.last_mut() {
            Some(b) if ts < b.end => b.tot.add(t),
            _ => v.push(Block { start: ts, end: ts + 5 * 3600, tot: *t, active: false }),
        }
    }
    if let Some(b) = v.last_mut() {
        b.active = now >= b.start && now < b.end;
    }
    v.reverse();
    v.truncate(30);
    v
}

/// Codex plan limits from the newest token_count event across all rollout files.
pub fn codex_limits(cache: &Cache) -> Option<(i64, serde_json::Value)> {
    cache.files.values().filter(|f| f.src == Some(Src::Codex)).filter_map(|f| f.limits.clone()).max_by_key(|l| l.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ais_claude_line_prices_and_dedupes() {
        let l = br#"{"type":"assistant","requestId":"r1","timestamp":"2026-09-25T05:19:50.077Z","cwd":"C:\\Code\\oriel","message":{"id":"m1","model":"claude-opus-5-5","content":[{"type":"text","text":"secret"}],"usage":{"input_tokens":1000000,"output_tokens":1000000,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
        let r = claude_line(l).unwrap();
        assert!((r.tot.cost - 24.0).abs() < 1e-9, "{}", r.tot.cost);
        assert_eq!(r.cwd.as_deref().map(project_name).as_deref(), Some("oriel"));
        assert!(contains(l, b"\"usage\""));
        assert!(!contains(b"abc", b"abcd"));
        assert_eq!(short_model("claude-haiku-4-5-20251001"), "haiku 4.5");
        assert_eq!(short_model("claude-opus-5-5"), "opus 5.5");
        assert_eq!(short_model("gpt-5.6-terra"), "gpt-5.6-terra");
    }

    #[test]
    fn ais_blocks_split_on_five_hours() {
        let mut h = BTreeMap::new();
        let t = Tot { n: 1, ..Default::default() };
        for hr in [100, 101, 104, 105, 111] {
            h.insert(hr, t);
        }
        let b = blocks(&h, 111 * 3600 + 60);
        assert_eq!(b.len(), 3);
        assert!(b[0].active && b[0].start == 111 * 3600);
        assert_eq!(b[2].tot.n, 3); // 100, 101, 104
    }
}
