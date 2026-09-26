//! The search app (alt r, /recall in the chat, palette → search chats): one box that searches every AI session on
//! this computer — oriel's chats, agent runs, and every Claude Code, Codex and Kimi session. Each result shows when,
//! which AI, the project and the title, then the lines that matched. enter reads it right here in the chat's own
//! renderer, r carries it on in the chat app, a attaches the part that matched to your chat. The index itself
//! (what's kept, the archive, the scan) is recall.rs; this file only asks it things on a worker thread.

use crate::pane::{Action, Cx, Pane, Waker};
use crate::panes::chat::{self, Chat, store};
use crate::recall::{self, Entry, Env, Hit, Manifest, Query, Src, extract::Kind, extract::Turn};
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

thread_local! {
    /// A query to open on (the chat's /recall), picked up the next time the app draws.
    static PENDING: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Open the search app on this query (call it, then switch to the app).
pub fn request_query(q: &str) {
    PENDING.with(|p| *p.borrow_mut() = Some(q.trim().to_string()));
}

/// Sessions are looked at again this often while the app is on screen (only changed files are read).
const REFRESH: Duration = Duration::from_secs(90);
/// Most results a query keeps.
const LIMIT: usize = 200;
/// Rows per result: the digest line, two lines of what matched, a gap.
const PER: usize = 4;

enum Req {
    Update,
    Query(u64, String),
}

enum Msg {
    /// what's in the index already, before a pass has looked for news
    Known(Summary),
    Progress(usize, usize),
    Indexed(Summary, recall::Stats),
    Results { seq: u64, hits: Vec<Hit>, ms: u128 },
}

#[derive(Default, Clone)]
struct Summary {
    total: usize,
    per: Vec<(Src, usize)>,
    agents: usize,
    gone: usize,
    projects: Vec<(String, usize)>,
}

fn summarize(m: &Manifest) -> Summary {
    let (per, agents, gone) = m.counts();
    Summary { total: m.sessions.len(), per, agents, gone, projects: m.projects(8) }
}

#[derive(Clone)]
enum SideHit {
    /// toggles `ai:<id>`
    Ai(&'static str),
    /// toggles `p:<name>`
    Project(String),
}

pub struct Search {
    env: Arc<Env>,
    req: Option<Sender<Req>>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    started: bool,
    summary: Option<Summary>,
    /// (read so far, to read) during an index pass
    indexing: Option<(usize, usize)>,
    last_index: Option<Instant>,
    /// how the last pass went (files looked at, read again, time)
    stats: Option<recall::Stats>,
    query: String,
    /// every edit asks again; results for an older question are dropped
    seq: u64,
    shown: u64,
    hits: Vec<Hit>,
    took: u128,
    sel: usize,
    top: usize,
    /// keys go to the results (r, a, j, k…) instead of the search box
    in_list: bool,
    /// the session being read: its transcript, and the result it is (a copy: a refresh can reorder the list)
    viewer: Option<(Chat, Hit)>,
    row_hits: Vec<(Rect, usize)>,
    side_hits: Vec<(Rect, SideHit)>,
    last_click: Option<(Instant, usize)>,
}

impl Search {
    pub fn new(_cfg: &crate::config::Config) -> Search {
        #[cfg(not(test))]
        let env = Env::real();
        // tests never read the real ~/.claude or oriel's own data
        #[cfg(test)]
        let env = Env::under(&std::path::absolute("target/test-scratch/search/app").unwrap_or_default());
        Search::with_env(env)
    }

    fn with_env(env: Env) -> Search {
        let (tx, rx) = channel();
        Search {
            env: Arc::new(env),
            req: None,
            tx,
            rx,
            started: false,
            summary: None,
            indexing: None,
            last_index: None,
            stats: None,
            query: String::new(),
            seq: 0,
            shown: 0,
            hits: vec![],
            took: 0,
            sel: 0,
            top: 0,
            in_list: false,
            viewer: None,
            row_hits: vec![],
            side_hits: vec![],
            last_click: None,
        }
    }

    /// Start the worker the first time the app is used, and pick up a query sent from the chat.
    fn ensure(&mut self, cx: &Cx) {
        if let Some(q) = PENDING.with(|p| p.borrow_mut().take()) {
            self.query = q;
            self.in_list = false;
            self.viewer = None;
            if self.started {
                self.ask();
            }
        }
        if self.started {
            return;
        }
        self.started = true;
        let (req_tx, req_rx) = channel();
        let (env, tx, waker) = (self.env.clone(), self.tx.clone(), cx.waker());
        std::thread::spawn(move || worker(env, req_rx, tx, waker));
        self.req = Some(req_tx);
        self.refresh();
        self.ask();
    }

    fn refresh(&mut self) {
        if let Some(r) = &self.req {
            if r.send(Req::Update).is_ok() {
                self.indexing = Some((0, 0));
                self.last_index = Some(Instant::now());
            }
        }
    }

    /// Ask the worker for the current query's results.
    fn ask(&mut self) {
        self.seq += 1;
        if let Some(r) = &self.req {
            let _ = r.send(Req::Query(self.seq, self.query.clone()));
        }
    }

    fn drain(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Known(s) => self.summary = Some(s),
                Msg::Progress(d, n) => self.indexing = Some((d, n)),
                Msg::Indexed(s, st) => {
                    self.indexing = None;
                    self.last_index = Some(Instant::now());
                    self.summary = Some(s);
                    self.stats = Some(st);
                }
                Msg::Results { seq, hits, ms } => {
                    if seq < self.seq {
                        continue; // you've typed more since
                    }
                    if seq != self.shown {
                        self.sel = 0;
                        self.top = 0;
                    }
                    self.shown = seq;
                    self.hits = hits;
                    self.took = ms;
                    self.sel = self.sel.min(self.hits.len().saturating_sub(1));
                }
            }
        }
    }

    fn step(&mut self, d: isize) {
        if self.hits.is_empty() {
            return;
        }
        self.sel = (self.sel as isize + d).clamp(0, self.hits.len() as isize - 1) as usize;
    }

    /// Read a result here, in the chat's renderer.
    fn open(&mut self, i: usize, cx: &mut Cx) {
        let Some(h) = self.hits.get(i) else { return };
        let (chat, at) = match saved_chat(&h.entry) {
            Some(c) => {
                let at = find_msg(&c, &h.snippet).unwrap_or(h.msg);
                (c, at)
            }
            None => {
                let turns = recall::load(&self.env, &h.entry);
                if turns.is_empty() {
                    cx.notify("that session's text is missing — it's read again on the next pass");
                    return;
                }
                (to_chat(&h.entry, &turns), h.msg)
            }
        };
        self.viewer = Some((Chat::viewer(cx.config, chat, at), h.clone()));
    }

    /// Carry a result on in the chat app: the CLI's own session when it can be resumed, else a fresh one that has
    /// read the conversation.
    fn resume(&self, h: &Hit, cx: &mut Cx) {
        let e = &h.entry;
        if e.src == Src::Oriel && !e.gone {
            chat::request_open(chat::Open::Chat(e.session.clone()));
            cx.act(Action::GotoApp("ai"));
            return;
        }
        let turns = recall::load(&self.env, e);
        let (c, note) = resumed(e, &turns, std::path::Path::new(&e.cwd).is_dir());
        chat::request_open(chat::Open::Resume(c, note));
        cx.act(Action::GotoApp("ai"));
    }

    /// Paste the part of a result that matched into your chat's box.
    fn attach(&self, h: &Hit, cx: &mut Cx) {
        let turns = recall::load(&self.env, &h.entry);
        if turns.is_empty() {
            cx.notify("that session's text is missing");
            return;
        }
        let text = recall::excerpt(&h.entry, &turns, h.msg, now());
        cx.act(Action::AppPaste("ai", format!("{text}\n\n")));
        cx.notify("attached to your chat's box — add your question and send");
    }

    /// Add a filter to the query, or take it out again (a sidebar click).
    fn toggle(&mut self, prefix: &str, value: &str) {
        // a project folder can have a space in its name: p:"my project"
        let value = value.to_ascii_lowercase();
        let want = if value.contains(char::is_whitespace) { format!("{prefix}\"{value}\"") } else { format!("{prefix}{value}") };
        let old = recall::words(&self.query);
        let had = old.iter().any(|w| w.to_ascii_lowercase() == want);
        let mut words: Vec<String> = old.iter().filter(|w| !w.to_ascii_lowercase().starts_with(prefix)).map(|w| w.to_string()).collect();
        if !had {
            words.push(want);
        }
        self.query = words.join(" ");
        self.in_list = false;
        self.ask();
    }

    // ------------------------------------------------------------------ drawing

    /// The line under the search box: what's happening, or what the filters mean.
    fn status(&self, q: &Query) -> (String, bool) {
        if let Some((d, n)) = self.indexing {
            if n > 0 {
                return (format!("reading sessions… {d}/{n} (the first time takes a while; it's quick after that)"), true);
            }
            if self.summary.is_none() {
                return ("looking for sessions…".into(), true);
            }
        }
        if !q.bad.is_empty() {
            return (format!("didn't understand {} — ai: takes claude, codex, kimi, oriel or agent; since: takes 12h, 30d, 2w, 6m, 1y", q.bad.join(" ")), true);
        }
        let mut f = vec![];
        if let Some(a) = &q.ai {
            f.push(format!("only {}", if a == "agent" { "agent runs" } else { SRC_LABELS.iter().find(|x| x.0 == a).map(|x| x.1).unwrap_or(a) }));
        }
        if let Some(p) = &q.project {
            f.push(format!("in {p}"));
        }
        if let Some(s) = q.since {
            f.push(format!("last {}", span(s)));
        }
        if !f.is_empty() {
            return (f.join(" · "), false);
        }
        match &self.summary {
            Some(s) if s.total > 0 => {
                let per: Vec<String> = s.per.iter().map(|(src, n)| format!("{} {n}", src.label())).collect();
                (format!("{} sessions: {} · p:project ai:codex since:30d \"a phrase\"", s.total, per.join(" · ")), false)
            }
            _ => ("p:project · ai:claude codex kimi oriel agent · since:30d · \"quotes\" for a phrase".into(), false),
        }
    }

    fn draw_results(&mut self, f: &mut Frame, area: Rect, terms: &[String], t: &crate::theme::Theme) {
        self.row_hits.clear();
        if self.hits.is_empty() {
            let msg = match &self.summary {
                None => String::new(), // the status line says it's looking
                Some(s) if s.total == 0 && self.indexing.is_none() => {
                    "no AI sessions here yet. oriel's chats, agent runs and Claude Code, Codex or Kimi sessions show up as soon as there are some.".into()
                }
                Some(_) if self.shown == self.seq && !self.query.trim().is_empty() => format!("nothing matches \"{}\" — try fewer words, or drop a filter", self.query.trim()),
                _ => String::new(),
            };
            f.render_widget(Paragraph::new(Span::styled(msg, ui::muted(t))).wrap(ratatui::widgets::Wrap { trim: true }), Rect { height: 3.min(area.height), ..area });
            return;
        }
        let rows = (area.height as usize / PER).max(1);
        if self.sel < self.top {
            self.top = self.sel;
        }
        if self.sel >= self.top + rows {
            self.top = self.sel + 1 - rows;
        }
        let w = area.width as usize;
        let now = now();
        for (row, i) in (self.top..self.hits.len()).take(rows).enumerate() {
            let h = &self.hits[i];
            let y = area.y + (row * PER) as u16;
            let on = i == self.sel;
            let r = Rect { y, height: (PER as u16 - 1).min(area.bottom().saturating_sub(y)), ..area };
            self.row_hits.push((Rect { height: PER as u16, ..r }, i));
            let e = &h.entry;
            let bar = Span::styled(if on { "▌ " } else { "  " }, ui::accent(t));
            let when = recall::when(e.updated, now);
            let mut head = vec![bar.clone(), Span::styled(format!("{when} · "), ui::muted(t)), Span::styled(e.who().to_string(), Style::default().fg(if on { t.accent } else { t.shine }))];
            if !e.project.is_empty() {
                head.push(Span::styled(format!(" · {}", e.project), ui::muted(t)));
            }
            if e.gone {
                head.push(Span::styled(" · archived", Style::default().fg(t.frame)));
            }
            head.push(Span::styled(" · ", ui::muted(t)));
            let used: usize = head.iter().map(|s| s.width()).sum();
            let title_style = if on { Style::default().fg(t.accent).add_modifier(Modifier::BOLD) } else { Style::default().add_modifier(Modifier::BOLD) };
            head.extend(highlight(&ui::fit(&e.title, w.saturating_sub(used)), terms, title_style, title_style.add_modifier(Modifier::UNDERLINED)));
            f.render_widget(Paragraph::new(Line::from(head)), Rect { height: 1, ..r });
            let who = match h.kind {
                Kind::You => "you ",
                Kind::Ai => "ai  ",
                Kind::Tool => "tool",
            };
            let hi = Style::default().fg(t.accent).add_modifier(Modifier::BOLD);
            for (k, line) in h.snippet.iter().take(2).enumerate() {
                let yy = y + 1 + k as u16;
                if yy >= area.bottom() {
                    break;
                }
                // the selection bar runs down the whole result
                let lead = if k == 0 { format!("  {who} ") } else { "       ".to_string() };
                let mut spans = vec![bar.clone(), Span::styled(lead.clone(), Style::default().fg(t.frame))];
                spans.extend(highlight(&ui::fit(line, w.saturating_sub(lead.len() + 2)), terms, if on { Style::default().fg(t.fg) } else { ui::muted(t) }, hi));
                f.render_widget(Paragraph::new(Line::from(spans)), Rect { y: yy, height: 1, ..r });
            }
        }
    }

    fn draw_viewer_banner(&self, f: &mut Frame, r: Rect, t: &crate::theme::Theme) {
        let Some((_, h)) = &self.viewer else { return };
        let e = &h.entry;
        let mut spans = vec![
            Span::styled("‹ esc ", ui::bold_accent(t)),
            Span::styled(format!("{} · {}", recall::when(e.updated, now()), e.who()), ui::muted(t)),
        ];
        if !e.project.is_empty() {
            spans.push(Span::styled(format!(" · {}", e.project), ui::muted(t)));
        }
        spans.push(Span::styled(" · ", ui::muted(t)));
        let used: usize = spans.iter().map(|s| s.width()).sum();
        spans.push(Span::styled(ui::fit(&e.title, (r.width as usize).saturating_sub(used + 12)), Style::default().add_modifier(Modifier::BOLD)));
        if e.gone {
            spans.push(Span::styled("  archived copy", Style::default().fg(t.frame)));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), r);
    }
}

/// `ai:` values and what the status line calls them.
const SRC_LABELS: &[(&str, &str)] = &[("claude", "claude code"), ("codex", "codex"), ("kimi", "kimi"), ("oriel", "oriel chats")];

fn now() -> i64 {
    crate::panes::files::clock::now_secs()
}

/// "30 days", "12 hours".
fn span(secs: i64) -> String {
    let (n, unit) = if secs % 86400 == 0 { (secs / 86400, "day") } else { (secs / 3600, "hour") };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

/// `text` as spans, with every term (ASCII case-insensitively) in `hi`.
fn highlight(text: &str, terms: &[String], base: Style, hi: Style) -> Vec<Span<'static>> {
    let low = text.to_ascii_lowercase();
    let mut marks = vec![false; text.len()];
    for t in terms.iter().filter(|t| !t.is_empty()) {
        let mut from = 0;
        while let Some(p) = low[from..].find(t.as_str()) {
            let a = from + p;
            marks[a..a + t.len()].iter_mut().for_each(|m| *m = true);
            from = a + t.len();
        }
    }
    let mut out = vec![];
    let (mut cur, mut on) = (String::new(), false);
    for (i, c) in text.char_indices() {
        let m = marks[i];
        if m != on && !cur.is_empty() {
            out.push(Span::styled(std::mem::take(&mut cur), if on { hi } else { base }));
        }
        on = m;
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(Span::styled(cur, if on { hi } else { base }));
    }
    out
}

/// The worker: owns the manifest, runs index passes and queries, newest request wins.
fn worker(env: Arc<Env>, rx: Receiver<Req>, tx: Sender<Msg>, waker: Waker) {
    let send = |m: Msg| {
        if tx.send(m).is_ok() {
            waker.wake();
        }
    };
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8);
    let mut m = Manifest::load(&env);
    if !m.sessions.is_empty() {
        send(Msg::Known(summarize(&m)));
    }
    let mut last: Option<(u64, String)> = None;
    let ask = |m: &Manifest, seq: u64, text: &str| {
        let t0 = Instant::now();
        let hits = recall::search(&env, m, &Query::parse(text), now(), LIMIT, threads);
        send(Msg::Results { seq, hits, ms: t0.elapsed().as_millis() });
    };
    while let Ok(first) = rx.recv() {
        let (mut update, mut q) = (false, None);
        for r in std::iter::once(first).chain(rx.try_iter()) {
            match r {
                Req::Update => update = true,
                Req::Query(seq, text) => q = Some((seq, text)),
            }
        }
        // what's already indexed answers at once; the pass that picks up anything new follows
        if let (true, Some((seq, text))) = (update && !m.sessions.is_empty(), &q) {
            ask(&m, *seq, text);
            last = q.take();
        }
        if update {
            let beat = std::sync::Mutex::new(Instant::now());
            let (fresh, stats) = recall::update(&env, threads, &|d, n| {
                let mut b = beat.lock().unwrap();
                if d == n || b.elapsed() > Duration::from_millis(150) {
                    *b = Instant::now();
                    send(Msg::Progress(d, n));
                }
            });
            m = fresh;
            send(Msg::Indexed(summarize(&m), stats));
            // fresh data: ask the last question again
            if q.is_none() {
                q = last.clone();
            }
        }
        if let Some((seq, text)) = q {
            ask(&m, seq, &text);
            last = Some((seq, text));
        }
    }
}

/// An oriel chat reads best from its own file (every diff and output is there); None for everything else.
fn saved_chat(e: &Entry) -> Option<store::Chat> {
    if e.src != Src::Oriel || e.gone {
        return None;
    }
    serde_json::from_str(&std::fs::read_to_string(&e.path).ok()?).ok()
}

/// Which of a saved chat's messages the matching line is in. The extract leaves out empty messages and splits
/// off messages you queued mid-reply, so its own count can be off by a few in a long chat.
fn find_msg(c: &store::Chat, snippet: &[String]) -> Option<usize> {
    let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let needle = flat(snippet.first()?.trim_matches('…'));
    if needle.is_empty() {
        return None;
    }
    c.messages.iter().position(|m| {
        let mut text = m.content.clone();
        for p in &m.parts {
            match p {
                store::Part::Text { text: s } | store::Part::User { text: s } => text.push_str(&format!("\n{s}")),
                store::Part::Tool(t) => text.push_str(&format!("\n{} {}", t.label, t.target)),
                _ => {}
            }
        }
        flat(&text).contains(&needle)
    })
}

/// The tool a transcript label stands for, so the chat draws it the way it draws a live one.
fn tool_name(label: &str) -> &str {
    match label {
        "Update" => "Edit",
        "List" => "LS",
        "Fetch" => "WebFetch",
        "Agent" => "Task",
        l => l,
    }
}

/// A session's turns as a chat the chat app can draw (and carry on).
fn to_chat(e: &Entry, turns: &[Turn]) -> store::Chat {
    let provider = match e.src {
        Src::Codex => "codex",
        Src::Kimi => "kimi",
        _ => "claude",
    };
    let mut c = store::Chat::new(provider);
    c.title = e.title.clone();
    c.cwd = (!e.cwd.is_empty()).then(|| e.cwd.clone());
    c.created = e.started as f64;
    c.updated = e.updated as f64;
    for m in recall::messages(turns) {
        if m[0].kind == Kind::You {
            c.messages.push(store::Msg { role: "user".into(), content: m[0].text.clone(), ..Default::default() });
            continue;
        }
        let mut msg = store::Msg { role: "assistant".into(), model: Some(provider.into()), ..Default::default() };
        let mut said = vec![];
        for (n, t) in m.iter().enumerate() {
            match t.kind {
                Kind::Ai => {
                    msg.parts.push(store::Part::Text { text: t.text.clone() });
                    said.push(t.text.clone());
                }
                Kind::Tool => {
                    let mut lines = t.text.lines();
                    let head = lines.next().unwrap_or("");
                    let (label, target) = head.split_once('\t').unwrap_or((head, ""));
                    let err = lines.collect::<Vec<_>>().join(" ");
                    // the chat already says "Error:" in front
                    let err = ["error: ", "Error: ", "error:", "Error:"].iter().find_map(|p| err.strip_prefix(p)).map(str::to_string).unwrap_or(err);
                    msg.parts.push(store::Part::Tool(store::Tool {
                        id: format!("{}-{n}", c.messages.len()),
                        name: tool_name(label).to_string(),
                        label: label.to_string(),
                        target: target.to_string(),
                        status: if err.is_empty() { "done".into() } else { "error".into() },
                        summary: err,
                        ..Default::default()
                    }));
                }
                Kind::You => {}
            }
        }
        msg.content = said.join("\n\n");
        c.messages.push(msg);
    }
    c
}

/// A session to carry on in the chat app, and the line that says how. Claude Code and Codex sessions resume their
/// own CLI session (`claude --resume <id>` / `codex exec resume <id>`, in its folder); when that can't work (the
/// CLI deleted it, its folder is gone, or it's an AI oriel can't resume) the next message starts a fresh session
/// that gets the conversation as context.
fn resumed(e: &Entry, turns: &[Turn], folder_ok: bool) -> (store::Chat, String) {
    let mut c = to_chat(e, turns);
    let t = store::now();
    (c.created, c.updated) = (t, t);
    if !folder_ok {
        c.cwd = None;
    }
    let cli = match e.src {
        Src::Claude => Some(("claude", "session")),
        Src::Codex => Some(("codex", "thread")),
        _ => None,
    };
    let note = match cli {
        Some((p, field)) if !e.gone && folder_ok && !e.session.is_empty() => {
            c.state.insert(p.to_string(), serde_json::json!({ field: e.session }));
            format!("carrying on a {} session from {} · your next message continues it, in {}", e.src.label(), recall::when(e.updated, t as i64), e.cwd)
        }
        _ => {
            if cli.is_none() {
                c.provider = None; // your default AI takes it from here
            }
            let why = if cli.is_none() {
                "oriel can't resume that AI's own sessions"
            } else if e.gone {
                "its CLI has deleted the original"
            } else {
                "its folder isn't there any more"
            };
            format!("carrying on a {} session ({why}) · your next message starts a fresh session with this conversation as context", e.src.label())
        }
    };
    (c, note)
}

impl Pane for Search {
    fn title(&self) -> String {
        match &self.viewer {
            Some((_, h)) => format!("search · {}", ui::fit(&h.entry.title, 50)),
            None => "search every AI session".into(),
        }
    }
    fn icon(&self) -> &'static str {
        "history"
    }
    fn subtitle(&self) -> Option<String> {
        let s = self.summary.as_ref()?;
        let mut v = vec![format!("{} sessions", s.total)];
        if s.gone > 0 {
            v.push(format!("{} archived", s.gone));
        }
        if let Some(st) = &self.stats {
            let read = if st.changed > 0 { format!(", {} read again", st.changed) } else { String::new() };
            v.push(format!("{} files checked{read} in {}", st.files, if st.ms < 1000 { format!("{} ms", st.ms) } else { format!("{:.1} s", st.ms as f64 / 1000.0) }));
        }
        Some(v.join(" · "))
    }
    fn badge(&self) -> Option<String> {
        match self.indexing {
            Some((d, n)) if n > 0 => Some(format!("{}%", d * 100 / n)),
            _ => None,
        }
    }
    fn tick_every(&self) -> Option<Duration> {
        Some(Duration::from_secs(30))
    }

    fn poll(&mut self, cx: &mut Cx) {
        self.ensure(cx);
        self.drain();
        if self.indexing.is_none() && self.last_index.is_some_and(|t| t.elapsed() >= REFRESH) {
            self.refresh();
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        self.ensure(cx);
        let t = cx.theme;
        if self.viewer.is_some() {
            self.draw_viewer_banner(f, Rect { height: 1, ..area }, t);
            let rest = Rect { y: area.y + 1, height: area.height.saturating_sub(1), ..area };
            if let Some((v, _)) = &mut self.viewer {
                v.render(f, rest, cx);
            }
            return;
        }
        let hints: Vec<(&str, &str)> = if self.in_list {
            vec![("↑↓", "pick"), ("enter", "read it"), ("r", "resume in chat"), ("a", "attach to chat"), ("type or /", "back to the box"), ("esc", "back")]
        } else {
            vec![("type", "to search"), ("↑↓", "pick"), ("enter", "read it"), ("tab", "results: r resume · a attach"), ("esc", "clear")]
        };
        let area = ui::hint_line(f, area, &hints, t);
        let area = Rect { x: area.x + 1, width: area.width.saturating_sub(2), ..area };
        if area.height < 4 {
            return;
        }
        let q = Query::parse(&self.query);
        // the box
        let right = if self.shown == self.seq && self.shown > 0 {
            if q.terms.is_empty() { format!("{} · newest first", self.hits.len()) } else { format!("{} found · {} ms", self.hits.len(), self.took) }
        } else {
            "…".into()
        };
        let rw = (right.chars().count() as u16).min(area.width / 2); // a narrow split keeps the box
        let typing = !self.in_list;
        let mut line = vec![Span::styled("› ", ui::bold_accent(t))];
        if self.query.is_empty() && typing {
            line.push(Span::styled("▏", ui::accent(t)));
            line.push(Span::styled("what did we decide about… · how did we fix…", ui::muted(t)));
        } else {
            line.push(Span::styled(self.query.clone(), Style::default().add_modifier(Modifier::BOLD)));
            if typing {
                line.push(Span::styled("▏", ui::accent(t)));
            }
        }
        f.render_widget(Paragraph::new(Line::from(line)), Rect { width: area.width.saturating_sub(rw + 1), height: 1, ..area });
        f.render_widget(Paragraph::new(Span::styled(right, ui::muted(t))), Rect { x: area.right().saturating_sub(rw), width: rw, height: 1, ..area });
        let (status, loud) = self.status(&q);
        f.render_widget(Paragraph::new(Span::styled(ui::fit(&status, area.width as usize), if loud { ui::accent(t) } else { ui::muted(t) })), Rect { y: area.y + 1, height: 1, ..area });
        ui::rule(f, Rect { y: area.y + 2, height: 1, ..area }, t);
        let list = Rect { y: area.y + 3, height: area.height - 3, ..area };
        self.draw_results(f, list, &q.terms, t);
    }

    fn key(&mut self, k: KeyEvent, cx: &mut Cx) -> bool {
        self.ensure(cx);
        if k.modifiers.contains(KeyModifiers::ALT) {
            return false;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if let Some((v, h)) = &mut self.viewer {
            match k.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace | KeyCode::Left => self.viewer = None,
                KeyCode::Char('r') if !ctrl => {
                    let h = h.clone();
                    self.resume(&h, cx);
                }
                KeyCode::Char('a') if !ctrl => {
                    let h = h.clone();
                    self.attach(&h, cx);
                }
                _ => return v.key(k, cx),
            }
            return true;
        }
        if self.in_list {
            match k.code {
                KeyCode::Up | KeyCode::Char('k') => self.step(-1),
                KeyCode::Down | KeyCode::Char('j') => self.step(1),
                KeyCode::PageUp => self.step(-5),
                KeyCode::PageDown => self.step(5),
                KeyCode::Home | KeyCode::Char('g') => self.step(-10_000),
                KeyCode::End | KeyCode::Char('G') => self.step(10_000),
                KeyCode::Enter | KeyCode::Right => self.open(self.sel, cx),
                KeyCode::Char('r') if !ctrl => {
                    if let Some(h) = self.hits.get(self.sel) {
                        self.resume(h, cx);
                    }
                }
                KeyCode::Char('a') if !ctrl => {
                    if let Some(h) = self.hits.get(self.sel) {
                        self.attach(h, cx);
                    }
                }
                KeyCode::Esc | KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('/') => self.in_list = false,
                KeyCode::Backspace => {
                    self.in_list = false;
                    self.query.pop();
                    self.ask();
                }
                KeyCode::Char(c) if !ctrl => {
                    self.in_list = false;
                    self.query.push(c);
                    self.ask();
                }
                _ => return false,
            }
            return true;
        }
        match k.code {
            KeyCode::Char('u') if ctrl => {
                self.query.clear();
                self.ask();
            }
            KeyCode::Char(c) if !ctrl => {
                self.query.push(c);
                self.ask();
            }
            KeyCode::Backspace => {
                if self.query.pop().is_some() {
                    self.ask();
                }
            }
            KeyCode::Esc => {
                if self.query.is_empty() {
                    return false;
                }
                self.query.clear();
                self.ask();
            }
            KeyCode::Up => self.step(-1),
            KeyCode::Down => self.step(1),
            KeyCode::PageUp => self.step(-5),
            KeyCode::PageDown => self.step(5),
            KeyCode::Enter => self.open(self.sel, cx),
            KeyCode::Tab if !self.hits.is_empty() => self.in_list = true,
            _ => return false,
        }
        true
    }

    fn paste(&mut self, text: &str, _cx: &mut Cx) {
        if self.viewer.is_none() {
            self.query.push_str(&text.split_whitespace().collect::<Vec<_>>().join(" "));
            self.in_list = false;
            self.ask();
        }
    }

    fn mouse(&mut self, ev: MouseEvent, area: Rect, cx: &mut Cx) {
        if let Some((v, _)) = &mut self.viewer {
            v.mouse(ev, area, cx);
            return;
        }
        match ev.kind {
            MouseEventKind::ScrollDown => self.step(1),
            MouseEventKind::ScrollUp => self.step(-1),
            MouseEventKind::Down(MouseButton::Left) => {
                let pos = Position { x: ev.column, y: ev.row };
                if let Some(&(_, i)) = self.row_hits.iter().find(|(r, _)| r.contains(pos)) {
                    let double = self.last_click.is_some_and(|(t, j)| j == i && t.elapsed() < Duration::from_millis(400));
                    self.last_click = Some((Instant::now(), i));
                    self.sel = i;
                    self.in_list = true;
                    if double {
                        self.open(i, cx);
                    }
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ sidebar: where sessions come from, projects
    fn side(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        self.ensure(cx);
        let t = cx.theme;
        self.side_hits.clear();
        let Some(s) = self.summary.clone() else {
            f.render_widget(Paragraph::new(Span::styled("reading sessions…", ui::muted(t))), Rect { height: 1, ..area });
            return;
        };
        let q = Query::parse(&self.query);
        let mut y = area.y;
        let mut row = |f: &mut Frame, y: &mut u16, label: String, right: String, on: bool, hit: Option<SideHit>| {
            if *y >= area.bottom() {
                return;
            }
            let r = Rect { y: *y, height: 1, ..area };
            ui::side_row(f, r, "", &label, &right, on, t);
            if let Some(h) = hit {
                self.side_hits.push((r, h));
            }
            *y += 1;
        };
        let head = |f: &mut Frame, y: &mut u16, text: &str| {
            if *y + 1 < area.bottom() {
                f.render_widget(Paragraph::new(Span::styled(format!("── {text}"), ui::muted(t))), Rect { y: *y, height: 1, ..area });
                *y += 1;
            }
        };
        head(f, &mut y, "from");
        for (src, n) in &s.per {
            row(f, &mut y, src.label().to_string(), n.to_string(), q.ai.as_deref() == Some(src.id()), Some(SideHit::Ai(src.id())));
        }
        if s.agents > 0 {
            row(f, &mut y, "agent runs".into(), s.agents.to_string(), q.ai.as_deref() == Some("agent"), Some(SideHit::Ai("agent")));
        }
        if s.total == 0 {
            row(f, &mut y, "nothing yet".into(), String::new(), false, None);
        }
        if !s.projects.is_empty() {
            y += 1;
            head(f, &mut y, "projects");
            for (p, n) in &s.projects {
                let on = q.project.as_deref() == Some(p.to_ascii_lowercase().as_str());
                row(f, &mut y, p.clone(), n.to_string(), on, Some(SideHit::Project(p.clone())));
            }
        }
        if s.gone > 0 && y + 3 < area.bottom() {
            y += 1;
            head(f, &mut y, "archive");
            let text = format!("{} session{} kept after their CLI deleted {}", s.gone, if s.gone == 1 { "" } else { "s" }, if s.gone == 1 { "it" } else { "them" });
            f.render_widget(Paragraph::new(Span::styled(text, ui::muted(t))).wrap(ratatui::widgets::Wrap { trim: true }), Rect { y, height: 2.min(area.bottom() - y), ..area });
        }
    }

    fn side_mouse(&mut self, ev: MouseEvent, _area: Rect, _cx: &mut Cx) {
        if let MouseEventKind::Down(MouseButton::Left) = ev.kind {
            let pos = Position { x: ev.column, y: ev.row };
            if let Some((_, h)) = self.side_hits.iter().find(|(r, _)| r.contains(pos)).cloned() {
                self.viewer = None;
                match h {
                    SideHit::Ai(a) => self.toggle("ai:", a),
                    SideHit::Project(p) => self.toggle("p:", &p),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
