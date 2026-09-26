//! An agent reply's transcript: text and activity in the order they happened, drawn like Claude Code's own
//! (● Update src/app.rs · +3 -1, a diff under it, command output, a todo list, subagents nested), plus the helpers
//! that fold streamed events into a message's parts.

use super::agent::{self, human_ms, split_line};
use super::md;
use super::store::{Msg, Part, Todo, Tool};
use crate::theme::Theme;
use crate::ui;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::HashSet;
use unicode_width::UnicodeWidthStr;

pub const SPIN: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// ------------------------------------------------------------------ folding events into parts
/// Parts start when the first bit of activity arrives; text streamed before that moves into a text part.
fn ensure_parts(m: &mut Msg) {
    if m.parts.is_empty() && !m.content.trim().is_empty() {
        m.parts.push(Part::Text { text: m.content.clone() });
    }
}

pub fn push_text(m: &mut Msg, s: &str) {
    if m.parts.is_empty() {
        m.content.push_str(s);
        return;
    }
    match m.parts.last_mut() {
        Some(Part::Text { text }) => text.push_str(s),
        _ => {
            if s.trim().is_empty() {
                return;
            }
            if !m.content.is_empty() && !m.content.ends_with("\n\n") {
                m.content.push_str(if m.content.ends_with('\n') { "\n" } else { "\n\n" });
            }
            m.parts.push(Part::Text { text: s.trim_start_matches('\n').to_string() });
        }
    }
    m.content.push_str(s);
}

pub fn push_thinking(m: &mut Msg, s: &str, n: u64) {
    ensure_parts(m);
    if let Some(Part::Thinking { text, tokens }) = m.parts.last_mut() {
        text.push_str(s);
        *tokens += n;
    } else {
        m.parts.push(Part::Thinking { text: s.to_string(), tokens: n });
    }
}

fn find<'a>(tools: &'a mut [Tool], id: &str) -> Option<&'a mut Tool> {
    for t in tools.iter_mut() {
        if t.id == id {
            return Some(t);
        }
        if let Some(x) = find(&mut t.children, id) {
            return Some(x);
        }
    }
    None
}

fn find_part<'a>(parts: &'a mut [Part], id: &str) -> Option<&'a mut Tool> {
    for p in parts.iter_mut() {
        if let Part::Tool(t) = p {
            if t.id == id {
                return Some(t);
            }
            if let Some(x) = find(&mut t.children, id) {
                return Some(x);
            }
        }
    }
    None
}

/// A call, new or updated in place (its children stay; a subagent's calls go under it).
pub fn upsert_tool(m: &mut Msg, mut t: Tool) {
    ensure_parts(m);
    if let Some(old) = find_part(&mut m.parts, &t.id) {
        t.children = std::mem::take(&mut old.children);
        t.since = old.since;
        *old = t;
        return;
    }
    if t.status == "running" {
        t.since = super::store::Since(Some(std::time::Instant::now()));
    }
    if let Some(p) = t.parent.clone() {
        if let Some(parent) = find_part(&mut m.parts, &p) {
            parent.children.push(t);
            return;
        }
    }
    m.parts.push(Part::Tool(t));
}

/// A queued message the agent just read: it goes in the transcript where it landed.
pub fn push_user(m: &mut Msg, text: &str) {
    ensure_parts(m);
    m.parts.push(Part::User { text: text.to_string() });
}

pub fn push_mark(m: &mut Msg, text: &str) {
    ensure_parts(m);
    m.parts.push(Part::Mark { text: text.to_string() });
}

/// The todo list lives where it first appeared and updates there.
pub fn set_todos(m: &mut Msg, items: Vec<Todo>) {
    ensure_parts(m);
    for p in m.parts.iter_mut() {
        if let Part::Todos { items: old } = p {
            *old = items;
            return;
        }
    }
    if !items.is_empty() {
        m.parts.push(Part::Todos { items });
    }
}

/// Calls still "running" when the reply ends become `status` (done at the end, stopped on esc).
pub fn settle(parts: &mut [Part], status: &str) {
    fn walk(t: &mut Tool, status: &str) {
        if t.status == "running" {
            t.status = status.to_string();
        }
        for c in &mut t.children {
            walk(c, status);
        }
    }
    for p in parts {
        if let Part::Tool(t) = p {
            walk(t, status);
        }
    }
}

pub fn todos(parts: &[Part]) -> Option<&Vec<Todo>> {
    parts.iter().find_map(|p| if let Part::Todos { items } = p { Some(items) } else { None })
}

/// What the agent is doing right now, for the status line: "Editing src/app.rs".
pub fn current_action(parts: &[Part]) -> Option<String> {
    fn running(t: &Tool) -> Option<&Tool> {
        t.children.iter().rev().find_map(running).or(if t.status == "running" { Some(t) } else { None })
    }
    let tool = parts.iter().rev().find_map(|p| if let Part::Tool(t) = p { running(t) } else { None });
    if let Some(t) = tool {
        let tgt = ui::fit(&t.target, 48);
        let s = match t.label.as_str() {
            "Read" => format!("reading {tgt}"),
            "Update" | "Notebook" => format!("editing {tgt}"),
            "Write" => format!("writing {tgt}"),
            "Delete" => format!("deleting {tgt}"),
            "Bash" | "PowerShell" | "Run" => format!("running {tgt}"),
            "Grep" => format!("searching for {tgt}"),
            "Glob" => format!("finding {tgt}"),
            "List" => format!("listing {tgt}"),
            "Fetch" => format!("fetching {tgt}"),
            "Web search" => format!("searching the web for {tgt}"),
            "Agent" => format!("agent: {tgt}"),
            l => format!("{l} {tgt}"),
        };
        return Some(s.trim_end().to_string());
    }
    match parts.last() {
        Some(Part::Thinking { .. }) => Some("thinking".into()),
        Some(Part::Text { .. }) => Some("writing".into()),
        _ => None,
    }
}

pub fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

// ------------------------------------------------------------------ drawing
pub struct View<'a> {
    /// ctrl+o: full diffs and outputs everywhere.
    pub expanded: bool,
    /// Calls clicked open (or closed, when expanded).
    pub open: &'a HashSet<String>,
    /// This reply is still streaming.
    pub live: bool,
    pub time: f64,
}

fn is_diff_tool(t: &Tool) -> bool {
    matches!(t.name.as_str(), "Edit" | "MultiEdit" | "Write" | "NotebookEdit" | "file_change")
}

/// How many body lines a call shows when collapsed.
fn compact_rows(t: &Tool) -> usize {
    if is_diff_tool(t) {
        10
    } else if matches!(t.name.as_str(), "Bash" | "PowerShell" | "command_execution") || t.status == "error" {
        4
    } else {
        0
    }
}

fn icon(status: &str, t: &Theme, time: f64) -> Span<'static> {
    match status {
        "running" => Span::styled(SPIN[(time * 10.0) as usize % SPIN.len()].to_string(), Style::default().fg(t.accent)),
        "error" => Span::styled("●", Style::default().fg(t.danger)),
        "stopped" => Span::styled("○", Style::default().fg(t.muted)),
        _ => Span::styled("●", Style::default().fg(t.good)),
    }
}

fn tool_lines(tool: &Tool, depth: usize, width: usize, t: &Theme, v: &View, out: &mut Vec<Line<'static>>, hits: &mut Vec<(usize, String)>) {
    let ind = "  ".to_string() + &"   ".repeat(depth);
    let muted = Style::default().fg(t.muted);
    // ---- header: ● Update src/app.rs · +3 -1 · 1.2s
    let mut meta: Vec<Span<'static>> = vec![];
    if !tool.summary.is_empty() {
        let style = if tool.status == "error" { Style::default().fg(t.danger) } else { muted };
        meta.push(Span::styled(" · ", muted));
        // colour a "+3 -1" summary like the diff
        if let Some((a, d)) = tool.summary.strip_prefix('+').and_then(|s| s.split_once(" -")) {
            meta.push(Span::styled(format!("+{a}"), Style::default().fg(t.good)));
            meta.push(Span::styled(format!(" -{d}"), Style::default().fg(t.danger)));
        } else {
            meta.push(Span::styled(ui::fit(&tool.summary, 70), style));
        }
    }
    if tool.ms >= 1000 || (tool.ms >= 100 && tool.status != "running") {
        meta.push(Span::styled(format!(" · {}", human_ms(tool.ms)), muted));
    } else if let (true, Some(since)) = (tool.status == "running" && v.live, tool.since.0) {
        // a long command: count up while it runs
        let s = since.elapsed().as_secs();
        if s >= 2 {
            meta.push(Span::styled(format!(" · {}", human_ms(s * 1000)), muted));
        }
    }
    let meta_w: usize = meta.iter().map(|s| s.content.width()).sum();
    let label_w = tool.label.width();
    let room = width.saturating_sub(ind.width() + 2 + label_w + 1 + meta_w).max(12);
    let mut spans = vec![
        Span::raw(ind.clone()),
        icon(&tool.status, t, v.time),
        Span::raw(" "),
        Span::styled(tool.label.clone(), Style::default().fg(t.accent).add_modifier(Modifier::BOLD)),
    ];
    if !tool.target.is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(ui::fit(&tool.target, room), Style::default().fg(t.fg)));
    }
    spans.extend(meta);
    hits.push((out.len(), tool.id.clone()));
    out.push(Line::from(spans));

    let open = v.expanded ^ v.open.contains(&tool.id);
    // ---- body: a diff, output, or an answer
    let rows = if open { 300 } else { compact_rows(tool) };
    let shown = tool.body.len().min(rows);
    let hidden = tool.body.len() - shown;
    let nw = tool.body[..shown].iter().map(|l| split_line(l).1.len()).max().unwrap_or(0);
    let lead = format!("{ind}  └  ");
    let cont = format!("{ind}     ");
    let mut first = true;
    for l in &tool.body[..shown] {
        let (k, num, text) = split_line(l);
        let prefix = if first { lead.clone() } else { cont.clone() };
        first = false;
        let mut sp = vec![Span::styled(prefix.clone(), muted)];
        let avail = width.saturating_sub(prefix.width() + if nw > 0 { nw + 3 } else { 0 }).max(8);
        match k {
            '+' | '-' | ' ' => {
                let (c, mark) = match k {
                    '+' => (t.good, "+"),
                    '-' => (t.danger, "-"),
                    _ => (t.muted, " "),
                };
                if nw > 0 {
                    sp.push(Span::styled(format!("{num:>nw$} "), muted));
                }
                sp.push(Span::styled(format!("{mark} "), Style::default().fg(c).add_modifier(Modifier::BOLD)));
                let body_style = if k == ' ' { Style::default().fg(t.muted) } else { Style::default().fg(c) };
                sp.push(Span::styled(ui::fit(text, avail), body_style));
            }
            '@' => sp.push(Span::styled(format!("{}┈┈", " ".repeat(nw + if nw > 0 { 1 } else { 0 })), muted)),
            '!' => sp.push(Span::styled(ui::fit(text, avail), Style::default().fg(t.danger))),
            '>' => sp.push(Span::styled(ui::fit(text, avail), Style::default().fg(t.fg))),
            _ => sp.push(Span::styled(ui::fit(text, avail), muted)),
        }
        out.push(Line::from(sp));
    }
    if hidden > 0 && shown > 0 {
        out.push(Line::from(vec![
            Span::styled(cont.clone(), muted),
            Span::styled(format!("… {hidden} more line{}", if hidden == 1 { "" } else { "s" }), muted),
            Span::styled(if open { String::new() } else { "  (ctrl+o or click to expand)".into() }, Style::default().fg(t.frame)),
        ]));
    }
    // ---- a subagent's own calls
    if !tool.children.is_empty() {
        let keep = if open { tool.children.len() } else { 3 };
        let skip = tool.children.len().saturating_sub(keep);
        if skip > 0 {
            out.push(Line::from(vec![Span::styled(cont.clone(), muted), Span::styled(format!("+{skip} earlier tool use{}", if skip == 1 { "" } else { "s" }), muted)]));
        }
        for c in &tool.children[skip..] {
            tool_lines(c, depth + 1, width, t, v, out, hits);
        }
    }
}

fn todo_lines(items: &[Todo], width: usize, t: &Theme, time: f64, out: &mut Vec<Line<'static>>) {
    let done = items.iter().filter(|x| x.status == "completed").count();
    let all = done == items.len();
    let muted = Style::default().fg(t.muted);
    out.push(Line::from(vec![
        Span::raw("  "),
        if all { icon("done", t, time) } else { Span::styled("◐", Style::default().fg(t.accent)) },
        Span::raw(" "),
        Span::styled("Todos", Style::default().fg(t.accent).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" · {done}/{} done", items.len()), muted),
    ]));
    for (i, it) in items.iter().enumerate() {
        out.push(todo_row(it, if i == 0 { "    └  " } else { "       " }, width, t));
    }
}

pub fn todo_row(it: &Todo, prefix: &str, width: usize, t: &Theme) -> Line<'static> {
    let (mark, ms, ts) = match it.status.as_str() {
        "completed" => ("✓", Style::default().fg(t.good), Style::default().fg(t.muted).add_modifier(Modifier::CROSSED_OUT)),
        "in_progress" => ("◐", Style::default().fg(t.accent), Style::default().fg(t.accent).add_modifier(Modifier::BOLD)),
        _ => ("○", Style::default().fg(t.muted), Style::default().fg(t.fg)),
    };
    Line::from(vec![
        Span::styled(prefix.to_string(), Style::default().fg(t.muted)),
        Span::styled(format!("{mark} "), ms),
        Span::styled(ui::fit(&it.text, width.saturating_sub(prefix.width() + 2)), ts),
    ])
}

/// Draw a reply's parts. `hits` gets (line index, call id) for every call's header line (click to expand).
pub fn render(parts: &[Part], width: usize, t: &Theme, v: &View, out: &mut Vec<Line<'static>>, hits: &mut Vec<(usize, String)>) {
    let muted = Style::default().fg(t.muted);
    // blank lines between text and activity; consecutive calls sit together
    let mut prev: Option<&'static str> = None;
    let gap = |out: &mut Vec<Line<'static>>, prev: Option<&str>, kind: &str| {
        if prev.is_some() && (prev != Some(kind) || kind != "tool") {
            out.push(Line::raw(""));
        }
    };
    for (i, p) in parts.iter().enumerate() {
        match p {
            Part::Text { text } => {
                if text.trim().is_empty() {
                    continue;
                }
                gap(out, prev, "text");
                out.extend(md::render(text.trim(), width.saturating_sub(1), "  ", t));
                prev = Some("text");
            }
            Part::Thinking { text, tokens } => {
                let live_last = v.live && i + 1 == parts.len();
                if !v.expanded && !live_last {
                    continue;
                }
                gap(out, prev, "thinking");
                let toks = if *tokens > 0 { format!(" · ~{} tokens", agent::human_tokens(*tokens)) } else { String::new() };
                let (word, style) = if live_last { ("Thinking…", Style::default().fg(t.shine).add_modifier(Modifier::ITALIC)) } else { ("Thought", muted.add_modifier(Modifier::ITALIC)) };
                out.push(Line::from(vec![Span::styled("  ◇ ", Style::default().fg(t.shine)), Span::styled(word, style), Span::styled(toks, muted)]));
                if v.expanded && !text.trim().is_empty() {
                    out.extend(md::wrap(vec![Span::styled(text.trim().to_string(), muted.add_modifier(Modifier::ITALIC))], width.saturating_sub(1), "    ", "    "));
                } else if live_last && !text.trim().is_empty() {
                    // what it's thinking right now: the last few lines, like watching it think
                    let lines = md::wrap(vec![Span::styled(text.trim().to_string(), muted.add_modifier(Modifier::ITALIC))], width.saturating_sub(1), "    ", "    ");
                    let skip = lines.len().saturating_sub(3);
                    out.extend(lines.into_iter().skip(skip));
                }
                prev = Some("thinking");
            }
            Part::Tool(tool) => {
                gap(out, prev, "tool");
                tool_lines(tool, 0, width, t, v, out, hits);
                prev = Some("tool");
            }
            Part::Todos { items } => {
                if items.is_empty() {
                    continue;
                }
                gap(out, prev, "todos");
                todo_lines(items, width, t, v.time, out);
                prev = Some("todos");
            }
            Part::User { text } => {
                // "❯ you  also fix the tests": what you sent while it worked, where it read it
                gap(out, prev, "user");
                let head = vec![Span::styled("  ❯ ", Style::default().fg(t.user).add_modifier(Modifier::BOLD)), Span::styled("you  ", muted)];
                let mut lines = md::wrap(vec![Span::styled(text.trim().to_string(), Style::default().fg(t.fg))], width.saturating_sub(10), "", "");
                if let Some(first) = lines.first_mut() {
                    let mut spans = head;
                    spans.append(&mut first.spans);
                    *first = Line::from(spans);
                }
                for (n, l) in lines.into_iter().enumerate() {
                    if n == 0 {
                        out.push(l);
                    } else {
                        let mut spans = vec![Span::raw("       ")];
                        spans.extend(l.spans);
                        out.push(Line::from(spans));
                    }
                }
                prev = Some("user");
            }
            Part::Mark { text } => {
                gap(out, prev, "mark");
                let side = "─".repeat(3);
                out.push(Line::from(Span::styled(format!("  {side} {text} {side}"), muted)));
                prev = Some("mark");
            }
        }
    }
}
