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

fn is_shell(t: &Tool) -> bool {
    matches!(t.name.as_str(), "Bash" | "PowerShell" | "command_execution")
}

/// Calls that only look around; Claude Code folds a run of them into one line ("Searched for 1 pattern, read 2 files").
fn is_lookup(t: &Tool) -> bool {
    matches!(t.name.as_str(), "Read" | "Grep" | "Glob" | "LS") && t.status != "error" && t.parent.is_none()
}

/// A new file: its lines are shown as plain code, like Claude Code's "Wrote 28 lines to x".
fn is_new_file(t: &Tool) -> bool {
    (t.name == "Write" || (t.name == "file_change" && t.label == "Write")) && !t.summary.starts_with('+')
}

/// How many body lines a call shows when collapsed.
fn compact_rows(t: &Tool) -> usize {
    if is_new_file(t) {
        10
    } else if is_diff_tool(t) {
        60 // Claude Code shows an edit's whole diff
    } else if is_shell(t) || t.status == "error" {
        4
    } else {
        0
    }
}

fn plural_n(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The line under a call, the way Claude Code words it: "Added 7 lines, removed 1 line", "Read 12 lines"…
fn sentence(tool: &Tool) -> Option<String> {
    if tool.status == "error" {
        return Some(format!("Error: {}", if tool.summary.is_empty() { "failed" } else { &tool.summary }));
    }
    if let Some((a, d)) = tool.summary.strip_prefix('+').and_then(|s| s.split_once(" -")) {
        let (a, d) = (a.parse::<u64>().unwrap_or(0), d.parse::<u64>().unwrap_or(0));
        return Some(match (a, d) {
            (0, 0) => "No changes".into(),
            (a, 0) => format!("Added {}", plural_n(a, "line", "lines")),
            (0, d) => format!("Removed {}", plural_n(d, "line", "lines")),
            (a, d) => format!("Added {}, removed {}", plural_n(a, "line", "lines"), plural_n(d, "line", "lines")),
        });
    }
    if tool.summary.is_empty() {
        return if tool.status == "running" && is_shell(tool) { Some("Running…".into()) } else { None };
    }
    Some(match tool.name.as_str() {
        _ if is_new_file(tool) => format!("Wrote {} to {}", tool.summary, tool.target),
        "Read" => format!("Read {}", tool.summary),
        "Grep" | "Glob" => format!("Found {}", tool.summary),
        "LS" => format!("Listed {}", tool.summary),
        // a command's output speaks for itself
        _ if is_shell(tool) && !tool.body.is_empty() && !tool.summary.starts_with("exit") => return None,
        _ if is_shell(tool) && tool.summary == "no output" => "(No output)".into(),
        _ => capitalize(&tool.summary),
    })
}

fn icon(status: &str, t: &Theme, time: f64) -> Span<'static> {
    match status {
        "running" => Span::styled(SPIN[(time * 10.0) as usize % SPIN.len()].to_string(), Style::default().fg(t.accent)),
        "error" => Span::styled("●", Style::default().fg(t.danger)),
        "stopped" => Span::styled("○", Style::default().fg(t.muted)),
        _ => Span::styled("●", Style::default().fg(t.good)),
    }
}

fn tok_style(tk: crate::panes::files::preview::Tok, t: &Theme) -> Style {
    use crate::panes::files::preview::Tok;
    match tk {
        Tok::Plain => Style::default().fg(t.fg),
        Tok::Kw => Style::default().fg(t.accent),
        Tok::Str => Style::default().fg(t.inline),
        Tok::Com => Style::default().fg(t.muted).add_modifier(Modifier::ITALIC),
        Tok::Num => Style::default().fg(t.good),
        Tok::Func => Style::default().fg(t.shine),
        Tok::Head => Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        Tok::Bold => Style::default().fg(t.fg).add_modifier(Modifier::BOLD),
    }
}

/// A diff line's background band (Claude Code tints whole added / removed lines).
fn band(t: &Theme, add: bool) -> Option<ratatui::style::Color> {
    use ratatui::style::Color;
    let base = match t.bg {
        Color::Rgb(..) => t.bg,
        _ => Color::Rgb(14, 14, 16),
    };
    let tint = if add { t.good } else { t.danger };
    match (base, tint) {
        (Color::Rgb(..), Color::Rgb(..)) => Some(crate::theme::mix(base, tint, if add { 0.16 } else { 0.22 })),
        _ => None,
    }
}

fn tool_lines(tool: &Tool, depth: usize, width: usize, t: &Theme, v: &View, out: &mut Vec<Line<'static>>, hits: &mut Vec<(usize, String)>) {
    let ind = "     ".repeat(depth);
    let muted = Style::default().fg(t.muted);
    // ---- header: ● Update(src/app.rs)
    let mut meta: Vec<Span<'static>> = vec![];
    // only a long call says how long it took (Claude Code shows none)
    if tool.ms >= 10_000 {
        meta.push(Span::styled(format!(" · {}", human_ms(tool.ms)), muted));
    } else if let (true, Some(since)) = (tool.status == "running" && v.live, tool.since.0) {
        // a long command: count up while it runs
        let s = since.elapsed().as_secs();
        if s >= 2 {
            meta.push(Span::styled(format!(" · {}", human_ms(s * 1000)), muted));
        }
    }
    let meta_w: usize = meta.iter().map(|s| s.content.width()).sum();
    let room = width.saturating_sub(ind.width() + 2 + tool.label.width() + 2 + meta_w).max(12);
    let mut spans = vec![Span::raw(ind.clone()), icon(&tool.status, t, v.time), Span::raw(" "), Span::styled(tool.label.clone(), Style::default().fg(t.fg).add_modifier(Modifier::BOLD))];
    if !tool.target.is_empty() {
        spans.push(Span::styled(format!("({})", ui::fit(&tool.target, room)), Style::default().fg(t.fg)));
    }
    spans.extend(meta);
    hits.push((out.len(), tool.id.clone()));
    out.push(Line::from(spans));

    let open = v.expanded ^ v.open.contains(&tool.id);
    let lead = format!("{ind}  ⎿  ");
    let cont = format!("{ind}     ");
    let mut first = true;
    // ---- ⎿  Added 7 lines, removed 1 line
    if let Some(sn) = sentence(tool) {
        let style = if tool.status == "error" { Style::default().fg(t.danger) } else { Style::default().fg(t.fg) };
        out.push(Line::from(vec![Span::styled(lead.clone(), muted), Span::styled(ui::fit(&sn, width.saturating_sub(lead.width())), style)]));
        first = false;
    }
    // ---- the body: a highlighted diff, new code, or output
    let rows = if open { 400 } else { compact_rows(tool) };
    let shown = tool.body.len().min(rows);
    let hidden = tool.body.len() - shown;
    let body = &tool.body[..shown];
    let nw = body.iter().map(|l| split_line(l).1.len()).max().unwrap_or(0);
    let code = is_diff_tool(tool);
    let new_file = is_new_file(tool);
    let ext = std::path::Path::new(tool.target.split(", ").next().unwrap_or("")).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let colored = if code {
        let texts: Vec<String> = body.iter().map(|l| split_line(l).2.to_string()).collect();
        crate::panes::files::preview::highlight(&texts, &ext)
    } else {
        vec![]
    };
    for (n, l) in body.iter().enumerate() {
        let (k, num, text) = split_line(l);
        let prefix = if first { lead.clone() } else { cont.clone() };
        first = false;
        let mut sp = vec![Span::styled(prefix.clone(), muted)];
        let avail = width.saturating_sub(prefix.width() + if nw > 0 { nw + 3 } else { 0 }).max(8);
        match k {
            '+' | '-' | ' ' if code => {
                let bg = if new_file || k == ' ' { None } else { band(t, k == '+') };
                let with_bg = |st: Style| if let Some(b) = bg { st.bg(b) } else { st };
                let (numc, mark) = match k {
                    '+' if !new_file => (t.good, "+"),
                    '-' => (t.danger, "-"),
                    _ => (t.muted, " "),
                };
                if nw > 0 {
                    sp.push(Span::styled(format!("{num:>nw$} "), with_bg(Style::default().fg(numc))));
                }
                if !new_file {
                    sp.push(Span::styled(mark.to_string(), with_bg(Style::default().fg(numc).add_modifier(Modifier::BOLD))));
                }
                // the code, syntax coloured, cut to fit, the band carried to the edge
                let mut used = 0;
                for (tk, piece) in colored.get(n).cloned().unwrap_or_default() {
                    let piece = piece.replace('\t', "    ");
                    let left = avail.saturating_sub(used);
                    if left == 0 {
                        break;
                    }
                    let piece = if piece.width() > left { ui::fit(&piece, left) } else { piece };
                    used += piece.width();
                    sp.push(Span::styled(piece, with_bg(tok_style(tk, t))));
                }
                if bg.is_some() {
                    sp.push(Span::styled(" ".repeat(avail.saturating_sub(used)), with_bg(Style::default())));
                }
            }
            '+' | '-' | ' ' => {
                let c = match k {
                    '+' => t.good,
                    '-' => t.danger,
                    _ => t.muted,
                };
                if nw > 0 {
                    sp.push(Span::styled(format!("{num:>nw$} "), muted));
                }
                sp.push(Span::styled(format!("{k} {}", ui::fit(text, avail)), Style::default().fg(c)));
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
            Span::styled(format!("… +{hidden} line{}", if hidden == 1 { "" } else { "s" }), muted),
            Span::styled(if open { String::new() } else { " (ctrl+o to expand)".into() }, Style::default().fg(t.frame)),
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

/// "Searched for 2 patterns, read 3 files" for a run of look-around calls (present tense while one runs).
fn lookup_line(tools: &[&Tool], t: &Theme, v: &View) -> Line<'static> {
    let count = |names: &[&str]| tools.iter().filter(|x| names.contains(&x.name.as_str())).count() as u64;
    let running = tools.iter().any(|x| x.status == "running");
    let (search, read, list) = (count(&["Grep", "Glob"]), count(&["Read"]), count(&["LS"]));
    let mut bits = vec![];
    if search > 0 {
        bits.push(format!("{} for {}", if running { "searching" } else { "searched" }, plural_n(search, "pattern", "patterns")));
    }
    if read > 0 {
        bits.push(format!("{} {}", if running { "reading" } else { "read" }, plural_n(read, "file", "files")));
    }
    if list > 0 {
        bits.push(format!("{} {}", if running { "listing" } else { "listed" }, plural_n(list, "directory", "directories")));
    }
    let mut text = capitalize(&bits.join(", "));
    if running {
        text.push('…');
    }
    let muted = Style::default().fg(t.muted);
    let mut spans = vec![Span::raw("  "), Span::styled(text, muted)];
    if running {
        spans.insert(0, icon("running", t, v.time));
        spans[1] = Span::raw(" ");
    }
    Line::from(spans)
}

/// Claude Code marks each thing it says with a bullet: "● Fixing both bugs now."
fn bullet(lines: &mut [Line<'static>], style: Style) {
    let Some(l) = lines.first_mut() else { return };
    let Some(first) = l.spans.first() else { return };
    let Some(rest) = first.content.strip_prefix("  ") else { return };
    let (rest, st) = (rest.to_string(), first.style);
    let mut spans = vec![Span::styled("● ", style)];
    if !rest.is_empty() {
        spans.push(Span::styled(rest, st));
    }
    spans.extend(l.spans.iter().skip(1).cloned());
    *l = Line::from(spans);
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
    // a blank line between blocks, like Claude Code
    let mut prev: Option<&'static str> = None;
    let gap = |out: &mut Vec<Line<'static>>, prev: Option<&str>, _kind: &str| {
        if prev.is_some() {
            out.push(Line::raw(""));
        }
    };
    let hidden_thinking = |i: usize| matches!(parts[i], Part::Thinking { .. }) && !v.expanded && !(v.live && i + 1 == parts.len());
    let mut skip_to = 0;
    for (i, p) in parts.iter().enumerate() {
        if i < skip_to {
            continue;
        }
        match p {
            Part::Text { text } => {
                if text.trim().is_empty() {
                    continue;
                }
                gap(out, prev, "text");
                let mut lines = md::render(text.trim(), width.saturating_sub(1), "  ", t);
                bullet(&mut lines, Style::default().fg(t.fg));
                out.extend(lines);
                prev = Some("text");
            }
            // a run of reads and searches: one line, until ctrl+o or a click opens it
            Part::Tool(tool) if is_lookup(tool) && !v.expanded && !v.open.contains(&tool.id) => {
                let mut run: Vec<&Tool> = vec![];
                let mut j = i;
                while j < parts.len() {
                    match &parts[j] {
                        Part::Tool(x) if is_lookup(x) => run.push(x),
                        _ if hidden_thinking(j) => {}
                        _ => break,
                    }
                    j += 1;
                }
                skip_to = j;
                gap(out, prev, "tool");
                hits.push((out.len(), tool.id.clone()));
                out.push(lookup_line(&run, t, v));
                prev = Some("tool");
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
