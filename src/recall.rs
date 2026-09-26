//! Search over every AI session on this computer ("what did we decide about X", "how did we fix that last time"):
//! oriel's own chats, agent runs, and every Claude Code, Codex and Kimi session on disk. No model is involved.
//!
//! A background pass copies each session's words into `<data dir>/index/<id>.txt` (extract.rs says what's kept) and
//! keeps `sessions.json`, a manifest of where each one came from, its folder, dates and title. A file is re-read
//! only when its size or modified time changes. When a CLI deletes an old session (Claude Code's 30-day cleanup)
//! the extract stays: the extracts are the archive. A query is a parallel term scan over the extracts, plenty fast
//! for thousands of sessions; if it ever isn't, SQLite FTS5 can slot in behind `search` without the callers
//! noticing.

pub mod extract;

use crate::panes::ais::usage;
use crate::panes::ais::util::{Paths, write_atomic};
use extract::{Kind, Turn};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

/// Bump when extracts change shape: every session is then read again.
const VERSION: u32 = 1;

/// Where sessions are found and where the index lives. Tests point it all at a scratch folder.
#[derive(Clone, Debug)]
pub struct Env {
    /// ~/.claude, ~/.codex, home (for Kimi): the same places the usage view reads
    pub paths: Paths,
    /// oriel's chats
    pub chats: PathBuf,
    /// the agents app's tasks.json (agent runs keep their transcript's path)
    pub tasks: PathBuf,
    pub index: PathBuf,
}

impl Env {
    pub fn real() -> Env {
        let data = crate::config::data_dir();
        Env { paths: Paths::real(&crate::config::Config::default()), chats: data.join("chats"), tasks: data.join("agents").join("tasks.json"), index: data.join("index") }
    }

    /// Everything under one folder (tests): `home/` for the CLIs, `data/` for oriel.
    #[cfg(test)]
    pub fn under(root: &Path) -> Env {
        let paths = Paths::under(root);
        let data = paths.data.clone();
        Env { paths, chats: data.join("chats"), tasks: data.join("agents").join("tasks.json"), index: data.join("index") }
    }

    fn manifest_file(&self) -> PathBuf {
        self.index.join("sessions.json")
    }

    pub fn extract_file(&self, id: &str) -> PathBuf {
        self.index.join(format!("{id}.txt"))
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "lowercase")]
pub enum Src {
    #[default]
    Oriel,
    Claude,
    Codex,
    Kimi,
}

pub const SRCS: [Src; 4] = [Src::Claude, Src::Codex, Src::Kimi, Src::Oriel];

impl Src {
    pub fn label(self) -> &'static str {
        match self {
            Src::Oriel => "oriel chat",
            Src::Claude => "claude code",
            Src::Codex => "codex",
            Src::Kimi => "kimi",
        }
    }
    /// What `ai:` takes.
    pub fn id(self) -> &'static str {
        match self {
            Src::Oriel => "oriel",
            Src::Claude => "claude",
            Src::Codex => "codex",
            Src::Kimi => "kimi",
        }
    }
}

/// One session in the manifest.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Entry {
    /// The extract's name: a hash of the source path.
    pub id: String,
    pub src: Src,
    pub path: String,
    /// The CLI's session id (for resuming), or the oriel chat's id.
    pub session: String,
    pub cwd: String,
    pub project: String,
    pub title: String,
    pub started: i64,
    pub updated: i64,
    /// Your prompts in it.
    pub turns: u32,
    /// The source file's modified time (ms) and size when it was read: it's read again only when they change.
    pub mtime: u64,
    pub size: u64,
    /// The source is gone (a CLI cleaned it up): only the extract is left.
    pub gone: bool,
    /// The agents app ran it: the task's title.
    pub task: String,
}

impl Entry {
    /// "claude code", or "agent" for an agent run.
    pub fn who(&self) -> &'static str {
        if self.task.is_empty() { self.src.label() } else { "agent" }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Manifest {
    pub v: u32,
    /// Newest first.
    pub sessions: Vec<Entry>,
    /// Files with nothing to find in them (a session quit before its first prompt), by path, with the modified
    /// time and size they had: they're only read again once they change.
    pub empty: HashMap<String, (u64, u64)>,
}

impl Manifest {
    pub fn load(env: &Env) -> Manifest {
        std::fs::read(env.manifest_file())
            .ok()
            .and_then(|b| serde_json::from_slice::<Manifest>(&b).ok())
            .map(|mut m| {
                if m.v != VERSION {
                    // an older extract shape: keep the list (gone sessions are the archive) but read the rest again
                    m.sessions.iter_mut().for_each(|e| e.mtime = 0);
                    m.empty.clear();
                }
                m
            })
            .unwrap_or_default()
    }

    fn save(&self, env: &Env) {
        if let Ok(b) = serde_json::to_vec(self) {
            let _ = write_atomic(&env.manifest_file(), &b);
        }
    }

    /// (source, sessions) for the ones that have any, and how many are only in the archive now.
    pub fn counts(&self) -> (Vec<(Src, usize)>, usize, usize) {
        let per: Vec<(Src, usize)> = SRCS.iter().map(|&s| (s, self.sessions.iter().filter(|e| e.src == s).count())).filter(|x| x.1 > 0).collect();
        let agents = self.sessions.iter().filter(|e| !e.task.is_empty()).count();
        (per, agents, self.sessions.iter().filter(|e| e.gone).count())
    }

    /// The projects with the most sessions.
    pub fn projects(&self, n: usize) -> Vec<(String, usize)> {
        let mut m: HashMap<&str, usize> = HashMap::new();
        for e in self.sessions.iter().filter(|e| !e.project.is_empty()) {
            *m.entry(e.project.as_str()).or_default() += 1;
        }
        let mut v: Vec<(String, usize)> = m.into_iter().map(|(k, n)| (k.to_string(), n)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }
}

/// FNV-1a: stable across runs (the std hasher is randomly seeded).
fn hash(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// A path as a key: Windows paths compare without case and with either slash.
fn norm(p: &str) -> String {
    if cfg!(windows) { p.replace('\\', "/").to_lowercase() } else { p.to_string() }
}

// ------------------------------------------------------------------ keeping the index up to date

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub files: usize,
    pub changed: usize,
    pub ms: u128,
}

/// One pass at a time: the background refresher and the search app share the index.
static UPDATING: Mutex<()> = Mutex::new(());

/// Every session file there is: (kind, path, the agent task that ran it).
fn discover(env: &Env) -> Vec<(Src, PathBuf, String)> {
    let mut out: Vec<(Src, PathBuf, String)> = vec![];
    for (s, p) in usage::discover(&env.paths) {
        let src = match s {
            // a subagent's own transcript: its work shows in the session that started it
            usage::Src::Claude if p.components().any(|c| c.as_os_str() == "subagents") => continue,
            usage::Src::Claude => Src::Claude,
            usage::Src::Codex => Src::Codex,
            usage::Src::Kimi => Src::Kimi,
            usage::Src::OpenCode => continue,
        };
        out.push((src, p, String::new()));
    }
    if let Ok(rd) = std::fs::read_dir(&env.chats) {
        out.extend(rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).map(|p| (Src::Oriel, p, String::new())));
    }
    // agent runs: mark the transcripts the agents app knows about (and add any stored somewhere else)
    let tasks: serde_json::Value = std::fs::read_to_string(&env.tasks).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let known: HashMap<String, usize> = out.iter().enumerate().map(|(i, x)| (norm(&x.1.to_string_lossy()), i)).collect();
    for t in tasks["tasks"].as_array().into_iter().flatten() {
        let (path, title) = (t["transcript_path"].as_str().unwrap_or(""), t["title"].as_str().unwrap_or("agent task"));
        if path.is_empty() {
            continue;
        }
        match known.get(&norm(path)) {
            Some(&i) => out[i].2 = title.to_string(),
            None if Path::new(path).is_file() => {
                let src = if path.contains("rollout-") { Src::Codex } else { Src::Claude };
                out.push((src, PathBuf::from(path), title.to_string()));
            }
            None => {}
        }
    }
    out
}

fn stamp(p: &Path) -> Option<(u64, u64)> {
    let m = std::fs::metadata(p).ok()?;
    let ms = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64).unwrap_or(0);
    Some((ms, m.len()))
}

fn read(src: Src, p: &Path) -> Option<extract::Doc> {
    match src {
        Src::Claude => extract::claude(p),
        Src::Codex => extract::codex(p),
        Src::Kimi => extract::generic(p),
        Src::Oriel => extract::oriel(p),
    }
}

/// Bring the index up to date with what's on disk; returns the manifest as it now stands. `threads` read files
/// in parallel (the background pass uses few, the search app more); `progress(done, of)` reports as it goes.
pub fn update(env: &Env, threads: usize, progress: &(dyn Fn(usize, usize) + Sync)) -> (Manifest, Stats) {
    let _one = UPDATING.lock().unwrap_or_else(|e| e.into_inner());
    let t0 = Instant::now();
    let mut m = Manifest::load(env);
    let found = discover(env);
    let at: HashMap<String, usize> = m.sessions.iter().enumerate().map(|(i, e)| (norm(&e.path), i)).collect();
    let was_empty = std::mem::take(&mut m.empty);
    let mut live = HashSet::new();
    let mut todo = vec![];
    for (src, p, task) in found {
        let key = norm(&p.to_string_lossy());
        let Some((mtime, size)) = stamp(&p) else { continue };
        live.insert(key.clone());
        if was_empty.get(&key) == Some(&(mtime, size)) {
            m.empty.insert(key, (mtime, size));
            continue;
        }
        match at.get(&key).map(|&i| &mut m.sessions[i]) {
            Some(e) if e.mtime == mtime && e.size == size && env.extract_file(&e.id).is_file() => {
                e.gone = false;
                if !task.is_empty() && task != e.task {
                    e.title = extract::short(&task, 80); // the agents app has since said which task ran it
                }
                e.task = task;
            }
            _ => todo.push((src, p, task, key, mtime, size)),
        }
    }
    let (next, done) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let results = Mutex::new(Vec::new());
    let _ = std::fs::create_dir_all(&env.index);
    std::thread::scope(|s| {
        for _ in 0..threads.clamp(1, 8).min(todo.len().max(1)) {
            s.spawn(|| {
                loop {
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    let Some((src, p, task, key, mtime, size)) = todo.get(k) else { break };
                    let path = p.to_string_lossy().into_owned();
                    let id = hash(key);
                    // None: couldn't read it just now (tried again next pass) · Some(None): nothing in it
                    let entry = read(*src, p).and_then(|d| {
                        if d.turns.is_empty() {
                            return Some(None);
                        }
                        write_atomic(&env.extract_file(&id), extract::write(&d.turns).as_bytes()).ok()?;
                        Some(Some(Entry {
                            id: id.clone(),
                            src: *src,
                            project: if d.cwd.is_empty() { String::new() } else { usage::project_name(&d.cwd) },
                            turns: d.prompts() as u32,
                            title: if task.is_empty() { d.title } else { extract::short(task, 80) },
                            session: d.session,
                            cwd: d.cwd,
                            started: d.started,
                            updated: if d.updated > 0 { d.updated } else { (*mtime / 1000) as i64 },
                            path,
                            mtime: *mtime,
                            size: *size,
                            gone: false,
                            task: task.clone(),
                        }))
                    });
                    results.lock().unwrap().push((key.clone(), id, entry, (*mtime, *size)));
                    progress(done.fetch_add(1, Ordering::Relaxed) + 1, todo.len());
                }
            });
        }
    });
    let results = results.into_inner().unwrap();
    let changed = results.len();
    // one pass over the list, not one per file read (a first pass reads thousands); a file that couldn't be read
    // just now keeps what it had
    let redone: HashSet<&str> = results.iter().filter(|r| r.2.is_some()).map(|r| r.0.as_str()).collect();
    m.sessions.retain(|e| !redone.contains(norm(&e.path).as_str()));
    for (key, id, entry, seen) in results {
        match entry {
            Some(Some(e)) => m.sessions.push(e),
            Some(None) => {
                let _ = std::fs::remove_file(env.extract_file(&id)); // nothing to find in it
                m.empty.insert(key, seen);
            }
            None => {}
        }
    }
    // gone from disk: a deleted oriel chat was deleted on purpose; a CLI's session lives on in the archive
    m.sessions.retain(|e| {
        let here = live.contains(&norm(&e.path));
        if !here && e.src == Src::Oriel {
            let _ = std::fs::remove_file(env.extract_file(&e.id));
        }
        here || e.src != Src::Oriel
    });
    for e in &mut m.sessions {
        e.gone = !live.contains(&norm(&e.path));
    }
    m.sessions.sort_by(|a, b| b.updated.cmp(&a.updated).then(a.id.cmp(&b.id)));
    m.v = VERSION;
    m.save(env);
    (m, Stats { files: live.len(), changed, ms: t0.elapsed().as_millis() })
}

/// Keep the archive current while oriel runs, whether or not the search app is ever opened: a gentle pass a
/// little after start (two threads), then every half hour.
pub fn start_background() {
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(40));
        let env = Env::real();
        loop {
            update(&env, 2, &|_, _| {});
            std::thread::sleep(std::time::Duration::from_secs(30 * 60));
        }
    });
}

// ------------------------------------------------------------------ queries

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query {
    /// Lowercase words (or "quoted phrases"); every one must match.
    pub terms: Vec<String>,
    /// p:<project> (matches the project name or its folder)
    pub project: Option<String>,
    /// ai:<claude|codex|kimi|oriel|agent>
    pub ai: Option<String>,
    /// since:30d, in seconds
    pub since: Option<i64>,
    /// Filters it couldn't read, to say so.
    pub bad: Vec<String>,
}

impl Query {
    pub fn parse(s: &str) -> Query {
        let mut q = Query::default();
        let mut rest = s;
        while !rest.trim().is_empty() {
            rest = rest.trim_start();
            let word = if let Some(r) = rest.strip_prefix('"') {
                let end = r.find('"').unwrap_or(r.len());
                let w = &r[..end];
                rest = r.get(end + 1..).unwrap_or("");
                if !w.trim().is_empty() {
                    q.terms.push(w.trim().to_ascii_lowercase());
                }
                continue;
            } else {
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                let w = &rest[..end];
                rest = &rest[end..];
                w
            };
            let low = word.to_ascii_lowercase();
            if let Some(v) = low.strip_prefix("p:").or(low.strip_prefix("project:")) {
                q.project = Some(v.to_string()).filter(|v| !v.is_empty());
            } else if let Some(v) = low.strip_prefix("ai:") {
                match ai_alias(v) {
                    Some(a) => q.ai = Some(a.to_string()),
                    None if v.is_empty() => {}
                    None => q.bad.push(word.to_string()),
                }
            } else if let Some(v) = low.strip_prefix("since:") {
                match since(v) {
                    Some(s) => q.since = Some(s),
                    None if v.is_empty() => {}
                    None => q.bad.push(word.to_string()),
                }
            } else {
                q.terms.push(low);
            }
        }
        q
    }

    fn keeps(&self, e: &Entry, now: i64) -> bool {
        if let Some(p) = &self.project {
            if !e.project.to_ascii_lowercase().contains(p.as_str()) && !e.cwd.to_ascii_lowercase().contains(p.as_str()) {
                return false;
            }
        }
        match self.ai.as_deref() {
            Some("agent") if e.task.is_empty() => return false,
            Some(a) if a != "agent" && e.src.id() != a => return false,
            _ => {}
        }
        !matches!(self.since, Some(s) if e.updated < now - s)
    }
}

fn ai_alias(v: &str) -> Option<&'static str> {
    Some(match v {
        "claude" | "cc" | "claude-code" | "claudecode" => "claude",
        "codex" | "cx" => "codex",
        "kimi" => "kimi",
        "oriel" | "chat" | "chats" => "oriel",
        "agent" | "agents" | "task" | "tasks" => "agent",
        _ => return None,
    })
}

/// "30d" → seconds (h, d, w, m = 30 days, y); a bare number is days.
fn since(v: &str) -> Option<i64> {
    let (n, unit) = match v.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => (&v[..i], &v[i..]),
        None => (v, "d"),
    };
    let n: i64 = n.parse().ok()?;
    let per = match unit {
        "h" => 3600,
        "d" => 86400,
        "w" => 7 * 86400,
        "m" | "mo" => 30 * 86400,
        "y" => 365 * 86400,
        _ => return None,
    };
    Some(n * per)
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub entry: Entry,
    /// The best matching line and the one after it (a tool line reads "label target").
    pub snippet: Vec<String>,
    /// Who said the matching line.
    pub kind: Kind,
    /// Which message it's in (you and ai turns count, tool calls belong to the reply around them).
    pub msg: usize,
}

/// Every session matching `q`, best first (all newest first when there are no words).
pub fn search(env: &Env, m: &Manifest, q: &Query, now: i64, limit: usize, threads: usize) -> Vec<Hit> {
    let cands: Vec<&Entry> = m.sessions.iter().filter(|e| q.keeps(e, now)).collect();
    if q.terms.is_empty() {
        return par_map(&cands[..limit.min(cands.len())], threads, |e| Some(first_prompt(env, e)));
    }
    // a cheap pass over everything: does it match, and how well; then lines to show for the best only
    let mut ranked: Vec<(&Entry, u32)> = par_map(&cands, threads, |e| {
        let mut low = std::fs::read_to_string(env.extract_file(&e.id)).ok()?;
        low.make_ascii_lowercase();
        rank(e, &low, &q.terms).map(|n| (*e, n))
    });
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.updated.cmp(&a.0.updated)).then(a.0.id.cmp(&b.0.id)));
    ranked.truncate(limit);
    par_map(&ranked, threads, |(e, _)| {
        let text = std::fs::read_to_string(env.extract_file(&e.id)).unwrap_or_default();
        Some(snippet(e, &text, &q.terms))
    })
}

/// `f` over `items` on up to `threads` threads; the results keep the items' order (None drops one).
fn par_map<T: Sync, R: Send>(items: &[T], threads: usize, f: impl Fn(&T) -> Option<R> + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let out = Mutex::new(Vec::with_capacity(items.len()));
    std::thread::scope(|s| {
        for _ in 0..threads.clamp(1, 8).min(items.len().max(1)) {
            s.spawn(|| {
                let mut mine = vec![];
                loop {
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    let Some(it) = items.get(k) else { break };
                    if let Some(r) = f(it) {
                        mine.push((k, r));
                    }
                }
                out.lock().unwrap().extend(mine);
            });
        }
    });
    let mut out = out.into_inner().unwrap();
    out.sort_by_key(|x| x.0);
    out.into_iter().map(|x| x.1).collect()
}

fn first_prompt(env: &Env, e: &Entry) -> Hit {
    let turns = load(env, e);
    let first = turns.iter().find(|t| t.kind == Kind::You).map(|t| t.text.as_str()).unwrap_or("");
    let snippet: Vec<String> = first.lines().filter(|l| !l.trim().is_empty()).take(2).map(|l| extract::short(l, 220)).collect();
    Hit { entry: e.clone(), snippet, kind: Kind::You, msg: 0 }
}

/// Does this (lowercased) extract have every term, in its text or its title? Then how well it matches.
fn rank(e: &Entry, low: &str, terms: &[String]) -> Option<u32> {
    let (title, project) = (e.title.to_ascii_lowercase(), e.project.to_ascii_lowercase());
    let mut score = 0u32;
    for t in terms {
        let n = low.matches(t.as_str()).take(20).count() as u32;
        let in_title = title.contains(t.as_str());
        if n == 0 && !in_title {
            return None;
        }
        score += n.min(10) + if in_title { 8 } else { 0 } + if project.contains(t.as_str()) { 3 } else { 0 };
    }
    Some(score)
}

/// The line with the most different terms on it (the first such), the one after it, who said it, and which
/// message it's in (grouped as in `messages`: each prompt on its own, a reply's text and tool calls together).
fn snippet(e: &Entry, text: &str, terms: &[String]) -> Hit {
    // ASCII lowercasing keeps byte offsets, so a match in `low` is a match at the same place in `text`
    let low = text.to_ascii_lowercase();
    let (mut best, mut best_n) = (None, 0usize);
    let (mut msg, mut kind, mut first): (usize, Kind, Option<Kind>) = (0, Kind::You, None);
    for (i, line) in low.split('\n').enumerate() {
        if let Some(k) = extract::mark_kind(line) {
            match first {
                None => first = Some(k),
                Some(f) if k == Kind::You || f == Kind::You => {
                    msg += 1;
                    first = Some(k);
                }
                _ => {}
            }
            kind = k;
        } else {
            let n = terms.iter().filter(|t| line.contains(t.as_str())).count();
            if n > best_n {
                best = Some((i, msg, kind));
                best_n = n;
            }
        }
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let (snippet, kind, msg) = match best {
        Some((i, msg, kind)) => {
            let line = lines[i];
            let pos = terms.iter().filter_map(|t| line.to_ascii_lowercase().find(t.as_str())).min().unwrap_or(0);
            let mut out = vec![window(line, pos, 220)];
            if let Some(next) = lines[i + 1..].iter().take_while(|l| !extract::is_mark(l)).find(|l| !l.trim().is_empty()) {
                out.push(extract::short(next, 220));
            }
            (out, kind, msg)
        }
        None => (vec![], Kind::You, 0), // only the title matched
    };
    let snippet = snippet.into_iter().map(|s| s.replace('\t', " ")).collect();
    Hit { entry: e.clone(), snippet, kind, msg }
}

/// About `w` characters of `line` around byte `pos`, with … where it was cut.
fn window(line: &str, pos: usize, w: usize) -> String {
    let line = line.trim_end();
    if line.chars().count() <= w {
        return line.trim().to_string();
    }
    let mut a = pos.saturating_sub(w / 3).min(line.len());
    while !line.is_char_boundary(a) {
        a -= 1;
    }
    // start at a word
    if a > 0 {
        if let Some(sp) = line[a..pos.max(a)].find(' ') {
            a += sp + 1;
        }
    }
    let body: String = line[a..].chars().take(w).collect();
    let cut_end = line[a..].chars().count() > w;
    format!("{}{}{}", if a > 0 { "…" } else { "" }, body.trim(), if cut_end { "…" } else { "" })
}

// ------------------------------------------------------------------ reading one back

/// The turns of a session, from its extract (works for sessions whose source is gone, too).
pub fn load(env: &Env, e: &Entry) -> Vec<Turn> {
    std::fs::read_to_string(env.extract_file(&e.id)).map(|t| extract::parse(&t)).unwrap_or_default()
}

/// Turns grouped the way a chat shows them: each of your prompts, then the reply with its tool calls.
pub fn messages(turns: &[Turn]) -> Vec<Vec<&Turn>> {
    let mut out: Vec<Vec<&Turn>> = vec![];
    for t in turns {
        let new = match out.last().and_then(|m| m.first()) {
            None => true,
            Some(first) => t.kind == Kind::You || first.kind == Kind::You,
        };
        if new {
            out.push(vec![t]);
        } else if let Some(m) = out.last_mut() {
            m.push(t);
        }
    }
    out
}

/// The part of a session around message `msg`, to hand to a chat: where it's from, then who said what.
pub fn excerpt(e: &Entry, turns: &[Turn], msg: usize, now: i64) -> String {
    let msgs = messages(turns);
    let (a, b) = (msg.saturating_sub(1), (msg + 1).min(msgs.len().saturating_sub(1)));
    let mut out = format!("(from a {} session{}, {}: \"{}\")\n", e.who(), if e.project.is_empty() { String::new() } else { format!(" in {}", e.project) }, when(e.updated, now), e.title);
    for m in msgs.get(a..=b).unwrap_or(&[]) {
        let who = if m[0].kind == Kind::You { "you".to_string() } else { e.src.label().to_string() };
        let mut body = String::new();
        for t in m {
            let s = if t.kind == Kind::Tool { format!("[{}]", t.text.lines().next().unwrap_or("").replace('\t', " ")) } else { t.text.clone() };
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&s);
        }
        let body = if body.chars().count() > 700 { format!("{}…", body.chars().take(699).collect::<String>()) } else { body };
        out.push_str(&format!("{who}: {body}\n"));
    }
    out.trim_end().to_string()
}

/// "today 14:02", "yesterday 09:15", "Sep 20", "Sep 20 2025".
pub fn when(t: i64, now: i64) -> String {
    use crate::panes::files::clock::local;
    const MON: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    if t <= 0 {
        return "undated".into();
    }
    let (l, n) = (local(t), local(now));
    let day = |x: &crate::panes::files::clock::Local| crate::panes::ais::util::days_from_civil(x.year as i64, x.month, x.day);
    match day(&n) - day(&l) {
        0 => format!("today {:02}:{:02}", l.hour, l.min),
        1 => format!("yesterday {:02}:{:02}", l.hour, l.min),
        _ if l.year == n.year => format!("{} {}", MON[(l.month.clamp(1, 12) - 1) as usize], l.day),
        _ => format!("{} {} {}", MON[(l.month.clamp(1, 12) - 1) as usize], l.day, l.year),
    }
}

#[cfg(test)]
pub(crate) mod tests;
