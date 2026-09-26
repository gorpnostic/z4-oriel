//! QA pass over the chat (`cargo test qa_chat`): synthetic provider events in odd orders, malformed Claude Code /
//! Codex stream lines, slash commands with bad arguments, and awkward text (CRLF, ANSI escapes, emoji, wide
//! characters, very long lines, 5000-line outputs, empty diffs).
//!
//! Parser and renderer tests run in this process. Tests that build a `Chat` pane (`child_*`) run in a child copy of
//! this test binary with ORIEL_DATA_DIR under target/test-scratch/qa-chat/: a pane saves the chat when a reply ends
//! and may `git init` a work folder, and none of that may land in the real profile. `qa_chat_pane_suite` runs the
//! passing ones; each `qa_chat_bug_*` runs one failing one.
//! A test marked `#[ignore = "fails: …"]` is a real problem, left in on purpose.

use super::agent::{self, Claude, Codex};
use super::approve;
use super::providers::{self, Ev};
use super::store::{self, Msg, Part, Todo, Tool};
use super::{Chat, Stream, activity, md};
use crate::pane::{Action, Cx, Pane};
use crate::testkit::Kit;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{TryRecvError, channel};
use std::time::{Duration, Instant};

const CWD: &str = "C:\\work\\demo";

// ------------------------------------------------------------------ helpers
fn claude() -> Claude {
    Claude::new(Path::new(CWD))
}

fn codex() -> Codex {
    Codex::new(Path::new(CWD))
}

fn feed(p: &mut Claude, v: Value, t: u64) -> (Result<(), String>, Vec<Ev>) {
    let mut evs = vec![];
    let r = p.feed(&v, t, &mut |e| evs.push(e));
    (r, evs)
}

fn feedx(p: &mut Codex, v: Value, t: u64) -> (Result<(), String>, Vec<Ev>) {
    let mut evs = vec![];
    let r = p.feed(&v, t, &mut |e| evs.push(e));
    (r, evs)
}

fn tools(evs: &[Ev]) -> Vec<Tool> {
    evs.iter().filter_map(|e| if let Ev::Tool(t) = e { Some(t.clone()) } else { None }).collect()
}

fn last_tool(evs: &[Ev]) -> Tool {
    tools(evs).pop().expect("no tool event")
}

fn show(e: &Ev) -> String {
    match e {
        Ev::Token(t) => format!("token {t:?}"),
        Ev::Thinking { text, tokens } => format!("thinking {tokens} {text:?}"),
        Ev::Tool(t) => format!("tool {}({}) [{}] {}", t.label, t.target, t.status, t.summary),
        Ev::Todos(v) => format!("todos {}", v.len()),
        Ev::Usage(n) => format!("usage {n}"),
        Ev::Status(s) => format!("status {s}"),
        Ev::State(k, v) => format!("state {k}={v}"),
        Ev::Ask(a) => format!("ask {}", a.tool),
        Ev::Question(q) => format!("question x{}", q.qs.len()),
        Ev::Steered(s) => format!("steered {s}"),
        Ev::Mark(s) => format!("mark {s}"),
        Ev::Done { note } => format!("done {note:?}"),
        Ev::Error(e) => format!("error {e}"),
    }
}

/// Fold events into an assistant message the way the pane's poll does.
fn fold(evs: impl IntoIterator<Item = Ev>) -> Msg {
    let mut m = Msg { role: "assistant".into(), ..Default::default() };
    for e in evs {
        match e {
            Ev::Token(t) => activity::push_text(&mut m, &t),
            Ev::Thinking { text, tokens } => activity::push_thinking(&mut m, &text, tokens),
            Ev::Tool(t) => activity::upsert_tool(&mut m, t),
            Ev::Todos(v) => activity::set_todos(&mut m, v),
            Ev::Steered(s) => activity::push_user(&mut m, &s),
            Ev::Mark(s) => activity::push_mark(&mut m, &s),
            _ => {}
        }
    }
    m
}

/// A reply's parts drawn the way the chat draws them, as text.
fn draw_parts(parts: &[Part], width: u16, expanded: bool) -> String {
    let t = crate::theme::get("oriel");
    let open = HashSet::new();
    let v = activity::View { expanded, open: &open, live: false, time: 0.0 };
    let (mut out, mut hits) = (vec![], vec![]);
    activity::render(parts, width as usize, &t, &v, &mut out, &mut hits);
    text_of(&out, width)
}

fn text_of(lines: &[Line<'static>], width: u16) -> String {
    let mut buf = Buffer::empty(Rect::new(0, 0, width.max(1), lines.len().clamp(1, 60_000) as u16));
    for (y, l) in lines.iter().take(60_000).enumerate() {
        buf.set_line(0, y as u16, l, width);
    }
    let mut s = String::new();
    for y in 0..buf.area.height {
        let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect();
        s.push_str(row.trim_end());
        s.push('\n');
    }
    s
}

/// Wide characters take two cells (the second drawn as a space): compare text without spaces.
fn squash(s: &str) -> String {
    s.chars().filter(|c| *c != ' ').collect()
}

// ================================================================== Claude Code stream parser

/// Lines of the wrong shape, or of a kind the parser has never seen, are skipped without making anything up, and
/// odd values in lines it does know fall back to defaults. Afterwards a normal call still parses.
#[test]
fn qa_chat_claude_malformed_and_unknown_lines() {
    let mut p = claude();
    let silent = [
        Value::Null,
        json!([]),
        json!("a bare string"),
        json!(42),
        json!({}),
        json!({"type": 5}),
        json!({"type": null}),
        json!({"type": "assistant"}),
        json!({"type": "assistant", "message": null}),
        json!({"type": "assistant", "message": {"content": "plain string content"}}),
        json!({"type": "assistant", "message": {"content": [null, 1, "x", {"type": "mystery_block"}]}}),
        json!({"type": "user", "message": {"content": [{"type": "tool_result"}]}}),
        json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "never-started", "content": "x"}]}}),
        json!({"type": "user", "message": {"content": []}}),
        json!({"type": "stream_event"}),
        json!({"type": "stream_event", "event": null}),
        json!({"type": "stream_event", "event": {"type": "content_block_delta", "index": 7, "delta": {"type": "input_json_delta", "partial_json": "{\"file_pa"}}}),
        json!({"type": "stream_event", "event": {"type": "message_delta"}}),
        json!({"type": "stream_event", "event": {"type": "brand_new_stream_event"}}),
        json!({"type": "system"}),
        json!({"type": "system", "subtype": "brand_new_subtype"}),
        json!({"type": "system", "subtype": "task_progress"}),
        json!({"type": "system", "subtype": "task_notification", "tool_use_id": "nobody", "status": "completed"}),
        json!({"type": "rate_limit_event"}),
        json!({"type": "rate_limit_event", "rate_limit_info": {"status": "some_new_status"}}),
        json!({"type": "result"}),
        json!({"type": "totally_new_event", "payload": {"x": 1}}),
        json!({"session_id": 7}),
    ];
    for (i, v) in silent.iter().enumerate() {
        let (r, evs) = feed(&mut p, v.clone(), i as u64);
        assert!(r.is_ok(), "{v} was an error: {r:?}");
        assert!(evs.is_empty(), "{v} made up {:?}", evs.iter().map(show).collect::<Vec<_>>());
    }
    let odd = [
        json!({"type": "system", "subtype": "init", "model": 5}),
        json!({"type": "system", "subtype": "thinking_tokens", "estimated_tokens_delta": -3}),
        json!({"type": "system", "subtype": "compact_boundary"}),
        json!({"type": "system", "subtype": "api_retry", "attempt": "soon"}),
        json!({"type": "system", "subtype": "status", "status": null}),
        json!({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected", "resetsAt": "tomorrow"}}),
        json!({"type": "result", "usage": {"output_tokens": "many"}, "num_turns": -1, "total_cost_usd": "free", "permission_denials": {"not": "a list"}}),
        json!({"type": "assistant", "message": {"content": [{"type": "tool_use"}]}}),
        json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "td", "name": "TodoWrite", "input": {"todos": null}}]}}),
        json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "tu", "name": "TaskUpdate", "input": {"taskId": 99, "status": "completed"}}]}}),
        json!({"type": "user", "message": {"content": "a replayed prompt"}}),
        json!({"type": "session_only", "session_id": "abc"}),
    ];
    let mut seen = vec![];
    for v in odd {
        let shown = v.to_string();
        let (r, evs) = feed(&mut p, v, 50);
        assert!(r.is_ok(), "{shown}: {r:?}");
        seen.extend(evs.iter().map(show));
    }
    println!("{seen:#?}");
    assert!(seen.iter().any(|s| s.starts_with("mark usage limit reached")), "{seen:?}");
    assert!(seen.iter().any(|s| s == "state claude.session=abc"), "{seen:?}");
    assert!(seen.iter().any(|s| s == "steered a replayed prompt"), "{seen:?}");
    // still in step: a normal call parses
    let (_, evs) = feed(&mut p, json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "ok1", "name": "Read", "input": {"file_path": "C:\\work\\demo\\src\\main.rs"}}]}}), 1000);
    assert_eq!(last_tool(&evs).target, "src\\main.rs");
    let (_, evs) = feed(&mut p, json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "ok1", "content": "a\nb\nc"}]}}), 1200);
    let t = last_tool(&evs);
    assert_eq!((t.status.as_str(), t.summary.as_str(), t.ms), ("done", "3 lines", 200));
}

/// A failed turn is an error with Claude's own words when it gives some.
#[test]
fn qa_chat_claude_result_errors() {
    let mut p = claude();
    assert_eq!(feed(&mut p, json!({"type": "result", "is_error": true}), 0).0, Err("Claude Code stopped: error".to_string()));
    let e = feed(&mut p, json!({"type": "result", "is_error": true, "subtype": "error_max_turns"}), 0).0.unwrap_err();
    assert!(e.contains("error_max_turns"), "{e}");
    assert_eq!(feed(&mut p, json!({"type": "result", "is_error": true, "result": "Prompt is too long"}), 0).0, Err("Prompt is too long".to_string()));
    assert!(feed(&mut p, json!({"type": "result", "is_error": "true"}), 0).0.is_ok(), "a string isn't the error flag");
}

/// Tool input streamed in pieces: emoji paths, CRLF and tabs in content, and blocks of pure garbage.
#[test]
fn qa_chat_claude_streamed_input_garbage() {
    let mut p = claude();
    let se = |e: Value| json!({"type": "stream_event", "event": e});
    let delta = |i: u64, s: &str| se(json!({"type": "content_block_delta", "index": i, "delta": {"type": "input_json_delta", "partial_json": s}}));
    let lines = vec![
        se(json!({"type": "message_start", "message": {"usage": {"output_tokens": 1}}})),
        se(json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "w1", "name": "Write"}})),
        delta(0, "{\"file_path\": \"C:\\\\work\\\\demo\\\\sub\\\\😀"),
        delta(0, ".txt\", \"content\": \"héllo\\r\\n世界\\tend"),
        delta(0, "\"}"),
        se(json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "g1", "name": "Bash"}})),
        delta(1, "}}}]]]"),
        delta(1, "\"\\"),
        delta(1, "{\"command\": [1, 2"),
        delta(1, "\u{0}\u{7}"),
        delta(9, "{\"no\": \"start\"}"),
        json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "id": "w1", "name": "Write", "input": {"file_path": "C:\\work\\demo\\sub\\😀.txt", "content": "héllo\r\n世界\tend"}},
            {"type": "tool_use", "id": "g1", "name": "Bash", "input": {"command": "echo hi"}}]}}),
        json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "w1", "content": "ok"}, {"type": "tool_result", "tool_use_id": "g1", "content": "hi"}]}}),
    ];
    let mut all = vec![];
    for v in lines {
        let shown = v.to_string();
        let (r, evs) = feed(&mut p, v, 10);
        assert!(r.is_ok(), "{shown}: {r:?}");
        all.extend(evs);
    }
    let ts = tools(&all);
    let w = ts.iter().rev().find(|t| t.id == "w1").unwrap();
    assert_eq!(w.target, "sub\\😀.txt");
    assert_eq!(w.body, vec!["+1\théllo".to_string(), "+2\t世界    end".to_string()], "CRLF gone, tabs expanded");
    let g = ts.iter().rev().find(|t| t.id == "g1").unwrap();
    assert_eq!((g.target.as_str(), g.status.as_str()), ("echo hi", "done"));
    // while it streamed, the garbage block never showed a made-up target
    assert!(ts.iter().filter(|t| t.id == "g1" && t.status == "running").all(|t| t.target.is_empty() || t.target == "echo hi"), "{ts:?}");
}

/// A rate-limit line with an out-of-range reset time (milliseconds instead of seconds, or garbage) must not bring
/// the provider thread down: a panic there leaves the reply spinning forever with no Done and no Error.
#[test]
#[ignore = "fails: attempt to multiply with overflow in files::clock::local (clock.rs:48) for resetsAt in ms / i64::MAX"]
fn qa_chat_claude_rate_limit_huge_reset_time() {
    for resets in [1_900_000_000_000i64, i64::MAX] {
        let mut p = claude();
        let (r, evs) = feed(&mut p, json!({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected", "resetsAt": resets}}), 0);
        assert!(r.is_ok());
        assert!(evs.iter().any(|e| matches!(e, Ev::Mark(m) if m.starts_with("usage limit reached"))), "{resets}");
    }
}

/// Token counters fed absurd values (a malformed usage block) must not panic the provider thread either.
#[test]
#[ignore = "fails: attempt to add with overflow in Claude::feed (agent.rs:770 tokens_done += …)"]
fn qa_chat_claude_token_counters_dont_overflow() {
    let mut p = claude();
    for i in 0..2 {
        let (r, _) = feed(&mut p, json!({"type": "stream_event", "event": {"type": "message_delta", "usage": {"output_tokens": u64::MAX}}}), i);
        assert!(r.is_ok());
    }
    for i in 0..2 {
        let (r, _) = feed(&mut p, json!({"type": "result", "usage": {"output_tokens": u64::MAX}, "num_turns": u64::MAX}), i);
        assert!(r.is_ok());
    }
    let _ = p.note(Duration::from_secs(1));
}

/// A new 5000-line file: the transcript keeps a capped body, but the line under the call must still say how long the
/// file really is.
#[test]
#[ignore = "fails: Write reports the capped body length (\"Wrote 401 lines\") instead of 5000 (agent.rs:419)"]
fn qa_chat_claude_write_5000_lines_counts_them_all() {
    let mut p = claude();
    let content: String = (1..=5000).map(|i| format!("line {i}\n")).collect();
    let (_, e1) = feed(&mut p, json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "w5k", "name": "Write", "input": {"file_path": "C:\\work\\demo\\big.txt", "content": content}}]}}), 0);
    let (_, e2) = feed(
        &mut p,
        json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "w5k", "content": "File created successfully at: C:\\work\\demo\\big.txt"}]},
               "tool_use_result": {"type": "create", "filePath": "C:\\work\\demo\\big.txt", "content": content}}),
        900,
    );
    let t = last_tool(&e2);
    assert!(t.body.len() <= 401, "the body is capped ({} lines)", t.body.len());
    let s = draw_parts(&fold(e1.into_iter().chain(e2)).parts, 100, false);
    println!("{s}");
    assert_eq!(t.summary, "5000 lines", "a 5000-line file is reported as {}", t.summary);
    assert!(s.contains("Wrote 5000 lines to big.txt"), "{s}");
}

/// 5000 lines of command output: capped, CRLF gone, drawn collapsed and expanded.
#[test]
fn qa_chat_claude_bash_5000_line_output() {
    let mut p = claude();
    let stdout: String = (1..=5000).map(|i| format!("out {i}")).collect::<Vec<_>>().join("\r\n");
    let (_, e1) = feed(&mut p, json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "b5k", "name": "Bash", "input": {"command": "cargo test"}}]}}), 0);
    let (_, e2) = feed(
        &mut p,
        json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "b5k", "content": "…"}]},
               "tool_use_result": {"stdout": stdout, "stderr": "", "interrupted": false}}),
        12_000,
    );
    let t = last_tool(&e2);
    assert!(t.body.len() <= 401, "{}", t.body.len());
    assert_eq!(t.body[0], ">\tout 1");
    assert_eq!(t.body.last().map(String::as_str), Some(">\tout 5000"));
    assert!(t.body.iter().all(|l| !l.contains('\r')));
    let m = fold(e1.into_iter().chain(e2));
    let collapsed = draw_parts(&m.parts, 100, false);
    assert!(collapsed.contains("out 1") && collapsed.contains("ctrl+o to expand") && !collapsed.contains("out 5000"), "{collapsed}");
    assert!(collapsed.contains("· 12s"), "a long call says how long it took: {collapsed}");
    let expanded = draw_parts(&m.parts, 100, true);
    assert!(expanded.contains("out 4999") && expanded.contains("more lines"), "expanded shows the tail and the cut");
}

/// ctrl+o shows a call "in full": for long output that's the capped body, and its last line (for a test run, the
/// summary) must be on screen, not behind a "… +1 line" that nothing can expand.
#[test]
#[ignore = "fails: a capped body is 401 lines (300 + marker + 100, agent.rs:75-83) but the open view shows 400 (activity.rs:433), so the last output line is always hidden"]
fn qa_chat_expanded_view_shows_the_last_line() {
    let mut p = claude();
    let stdout: String = (1..=5000).map(|i| format!("test t{i} ... ok")).chain(["test result: ok. 5000 passed".to_string()]).collect::<Vec<_>>().join("\n");
    let (_, e1) = feed(&mut p, json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t", "name": "Bash", "input": {"command": "cargo test"}}]}}), 0);
    let (_, e2) = feed(&mut p, json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t", "content": "…"}]}, "tool_use_result": {"stdout": stdout, "stderr": ""}}), 10);
    let expanded = draw_parts(&fold(e1.into_iter().chain(e2)).parts, 100, true);
    let tail: Vec<&str> = expanded.lines().filter(|l| !l.trim().is_empty()).collect();
    println!("{:#?}", &tail[tail.len().saturating_sub(3)..]);
    assert!(expanded.contains("test result: ok. 5000 passed"), "the summary line is hidden even with ctrl+o");
}

/// Long command output: the "… N more lines" marker in the capped body must count the lines really cut. Bash
/// output is capped once for stdout and again for stdout+stderr, and the second cap throws the first marker away
/// and counts only what it cut itself.
#[test]
#[ignore = "fails: Bash body is capped twice (agent.rs:433 then :439): 5000 lines of stdout say \"… 1 more lines\", with 3 stderr lines \"… 4 more lines\""]
fn qa_chat_claude_bash_long_output_elision_count() {
    let stdout: String = (1..=5000).map(|i| format!("out {i}")).collect::<Vec<_>>().join("\n");
    let mut got = vec![];
    // (stderr, lines really cut: 5000 minus the 300 head lines and the stdout lines in the 100-line tail)
    for (stderr, cut) in [("", 4600), ("warning: a\nwarning: b\nwarning: c", 4603)] {
        let mut p = claude();
        let _ = feed(&mut p, json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "b2", "name": "Bash", "input": {"command": "make"}}]}}), 0);
        let (_, evs) = feed(
            &mut p,
            json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "b2", "content": "…"}]},
                   "tool_use_result": {"stdout": stdout, "stderr": stderr, "interrupted": false}}),
            100,
        );
        let t = last_tool(&evs);
        let markers: Vec<String> = t.body.iter().filter(|l| l.starts_with('…')).cloned().collect();
        assert!(t.body.last().unwrap().ends_with(if stderr.is_empty() { "out 5000" } else { "warning: c" }));
        got.push((cut, markers));
    }
    println!("{got:?}");
    for (cut, markers) in &got {
        assert_eq!(markers, &vec![format!("…\t{cut} more lines")], "the cut is miscounted");
    }
}

/// Output with colours, carriage returns, tabs and an OSC title sequence: none of that reaches the screen.
#[test]
fn qa_chat_tool_output_ansi_crlf_tabs_stripped() {
    let raw = "\x1b[32mok\x1b[0m done\r\n\ttabbed\r\n\x1b]0;title\x07after osc\r\nemoji 👋🏽 世界\r\n\x1b[2K\rprogress 100%";
    let want = vec![">\tok done", ">\t    tabbed", ">\tafter osc", ">\temoji 👋🏽 世界", ">\tprogress 100%"];
    let mut p = claude();
    let _ = feed(&mut p, json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "a1", "name": "Bash", "input": {"command": "npm test"}}]}}), 0);
    let (_, evs) = feed(&mut p, json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "a1", "content": "x"}]}, "tool_use_result": {"stdout": raw, "stderr": ""}}), 10);
    assert_eq!(last_tool(&evs).body, want);
    let mut x = codex();
    let (_, evs) = feedx(&mut x, json!({"type": "item.completed", "item": {"id": "c1", "type": "command_execution", "command": "npm test", "aggregated_output": raw, "exit_code": 0, "status": "completed"}}), 10);
    assert_eq!(last_tool(&evs).body, want);
    let s = draw_parts(&fold(evs).parts, 80, true);
    assert!(!s.contains("[32m") && !s.contains("[0m") && !s.contains("title") && s.contains("after osc"), "{s}");
}

/// The one-line summary under a call (MCP tools, errors) comes from the raw result text: colours and tabs must be
/// stripped there too, or "⎿  [32mPASS[0m" shows up.
#[test]
#[ignore = "fails: tool summaries are built from raw result text, not clean()ed (agent.rs:397, :500, :1030)"]
fn qa_chat_tool_summaries_strip_ansi() {
    let mut p = claude();
    let _ = feed(&mut p, json!({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": "m1", "name": "mcp__tests__run", "input": {"suite": "all"}},
        {"type": "tool_use", "id": "r1", "name": "Read", "input": {"file_path": "C:\\work\\demo\\gone.rs"}}]}}), 0);
    let (_, mut evs) = feed(&mut p, json!({"type": "user", "message": {"content": [
        {"type": "tool_result", "tool_use_id": "m1", "content": "\x1b[32mPASS\x1b[0m\tall 12 tests\r\nmore"},
        {"type": "tool_result", "tool_use_id": "r1", "is_error": true, "content": "\x1b[31mFile does not exist.\x1b[0m"}]}}), 10);
    let mut x = codex();
    let (_, e2) = feedx(&mut x, json!({"type": "item.completed", "item": {"id": "x1", "type": "mcp_tool_call", "server": "tests", "tool": "run", "arguments": {}, "status": "completed", "result": {"content": [{"type": "text", "text": "\x1b[1mbold\x1b[0m result"}]}}}), 10);
    evs.extend(e2);
    for t in tools(&evs) {
        assert!(!t.summary.contains(['\x1b', '\t', '\r']), "{} summary {:?}", t.name, t.summary);
    }
    let s = draw_parts(&fold(evs).parts, 100, false);
    println!("{s}");
    assert!(!s.contains("[32m") && !s.contains("[31m") && !s.contains("[1m"), "{s}");
}

// ================================================================== Codex stream parser

#[test]
fn qa_chat_codex_malformed_and_unknown_lines() {
    let mut p = codex();
    let lines = [
        Value::Null,
        json!([]),
        json!("x"),
        json!({}),
        json!({"type": 3}),
        json!({"type": "item.completed"}),
        json!({"type": "item.completed", "item": null}),
        json!({"type": "item.started", "item": {"type": "unknown_kind", "id": "u1"}}),
        json!({"type": "item.completed", "item": {"type": "command_execution"}}),
        json!({"type": "item.updated", "item": {"type": "command_execution", "id": "c1", "command": 5, "aggregated_output": null}}),
        json!({"type": "item.completed", "item": {"type": "command_execution", "id": "c1", "exit_code": "one"}}),
        json!({"type": "item.completed", "item": {"type": "todo_list", "id": "t1", "items": null}}),
        json!({"type": "item.completed", "item": {"type": "web_search", "id": "s1"}}),
        json!({"type": "item.completed", "item": {"type": "file_change", "id": "f1", "changes": null}}),
        json!({"type": "item.completed", "item": {"type": "file_change", "id": "f2", "changes": [{"kind": "update"}, 7, null]}}),
        json!({"type": "item.completed", "item": {"type": "mcp_tool_call", "id": "m1"}}),
        json!({"type": "item.completed", "item": {"type": "reasoning", "id": "r1"}}),
        json!({"type": "item.completed", "item": {"type": "agent_message", "id": "a1", "text": null}}),
        json!({"type": "item.completed", "item": {"type": "error", "id": "e1"}}),
        json!({"type": "turn.started"}),
        json!({"type": "turn.completed"}),
        json!({"type": "turn.completed", "usage": {"output_tokens": "lots"}}),
        json!({"type": "thread.started", "thread_id": 5}),
        json!({"type": "brand.new.event"}),
    ];
    let mut seen = vec![];
    for (i, v) in lines.into_iter().enumerate() {
        let shown = v.to_string();
        let (r, evs) = feedx(&mut p, v, i as u64 * 10);
        assert!(r.is_ok(), "{shown}: {r:?}");
        seen.extend(evs.iter().map(show));
    }
    println!("{seen:#?}");
    assert!(p.note(Duration::from_secs(3)).contains("⚠ error"), "a failed item shows in the note");
}

/// Codex errors reach the chat as readable words, not JSON.
#[test]
#[ignore = "fails: {\"type\":\"error\"} becomes the error text \"null\", a string error keeps its JSON quotes (agent.rs:1068)"]
fn qa_chat_codex_error_messages_are_readable() {
    let cases = [
        (json!({"type": "turn.failed", "error": {"message": "quota exceeded"}}), Some("quota exceeded")),
        (json!({"type": "error", "message": "stream disconnected"}), Some("stream disconnected")),
        (json!({"type": "turn.failed", "error": "plain words"}), Some("plain words")),
        (json!({"type": "error"}), None),
        (json!({"type": "error", "message": ""}), None),
    ];
    let mut bad = vec![];
    for (v, want) in cases {
        let e = feedx(&mut codex(), v.clone(), 0).0.unwrap_err();
        let ok = match want {
            Some(w) => e == w,
            None => !e.trim().is_empty() && e != "null",
        };
        if !ok {
            bad.push(format!("{v} -> {e:?}"));
        }
    }
    assert!(bad.is_empty(), "{bad:#?}");
}

/// A web search that "completes before it started" (clock going backwards) must not panic like everything else
/// that uses saturating_sub.
#[test]
#[ignore = "fails: attempt to subtract with overflow, web_search uses t_ms - start (agent.rs:1036)"]
fn qa_chat_codex_web_search_clock_skew() {
    let mut p = codex();
    let _ = feedx(&mut p, json!({"type": "item.started", "item": {"type": "web_search", "id": "w1", "query": "ratatui"}}), 1000);
    let (r, evs) = feedx(&mut p, json!({"type": "item.completed", "item": {"type": "web_search", "id": "w1", "query": "ratatui"}}), 500);
    assert!(r.is_ok());
    assert_eq!(last_tool(&evs).ms, 0);
}

/// A file change that changed nothing (an empty change list) shouldn't claim it wrote "0 new files".
#[test]
#[ignore = "fails: empty changes => label Write, summary \"0 new files\", drawn as \"Wrote 0 new files to\" (agent.rs:990-1016)"]
fn qa_chat_codex_empty_file_change() {
    let mut p = codex();
    let (_, evs) = feedx(&mut p, json!({"type": "item.completed", "item": {"id": "f0", "type": "file_change", "changes": [], "status": "completed"}}), 10);
    let s = draw_parts(&fold(evs).parts, 80, false);
    println!("{s}");
    assert!(!s.contains("0 new files"), "{s}");
}

// ================================================================== diffs, markdown, drawing

/// Empty diffs everywhere: Edit with nothing changed, MultiEdit with no edits, a Codex change with an empty diff.
#[test]
fn qa_chat_empty_diffs() {
    assert!(agent::diff("", "").is_empty());
    assert!(agent::diff("same\n", "same\n").is_empty());
    let mut p = claude();
    let (_, evs) = feed(&mut p, json!({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": "e1", "name": "Edit", "input": {"file_path": "C:\\work\\demo\\a.rs", "old_string": "x", "new_string": "x"}},
        {"type": "tool_use", "id": "e2", "name": "Edit", "input": {"file_path": "C:\\work\\demo\\b.rs", "old_string": "", "new_string": ""}},
        {"type": "tool_use", "id": "e3", "name": "MultiEdit", "input": {"file_path": "C:\\work\\demo\\c.rs", "edits": []}},
        {"type": "tool_use", "id": "e4", "name": "Write", "input": {"file_path": "C:\\work\\demo\\empty.txt", "content": ""}}]}}), 0);
    let (_, done) = feed(&mut p, json!({"type": "user", "message": {"content": [
        {"type": "tool_result", "tool_use_id": "e1", "content": "ok"}, {"type": "tool_result", "tool_use_id": "e2", "content": "ok"},
        {"type": "tool_result", "tool_use_id": "e3", "content": "ok"}, {"type": "tool_result", "tool_use_id": "e4", "content": "ok"}]},
        "tool_use_result": {"structuredPatch": [], "type": "create"}}), 10);
    for t in tools(&done).iter().take(3) {
        assert_eq!((t.summary.as_str(), t.body.len()), ("+0 -0", 0), "{}", t.id);
    }
    let mut x = codex();
    let (_, cx) = feedx(&mut x, json!({"type": "item.completed", "item": {"id": "f1", "type": "file_change", "changes": [{"path": "C:\\work\\demo\\a.txt", "kind": "update", "diff": ""}], "status": "completed"}}), 10);
    assert_eq!(last_tool(&cx).body.len(), 0);
    let s = draw_parts(&fold(evs.into_iter().chain(done).chain(cx)).parts, 80, false);
    println!("{s}");
    assert_eq!(s.matches("No changes").count(), 3, "{s}");
    assert!(s.contains("Update(a.txt)"), "{s}");
}

/// CRLF vs LF isn't a change; wide characters and emoji in changed lines draw at any width.
#[test]
fn qa_chat_diff_crlf_and_wide_chars() {
    let d = agent::diff("a\r\nb\r\nc\r\n", "a\nB\nc\n");
    let kinds: String = d.iter().map(|l| l.chars().next().unwrap()).collect();
    assert_eq!(kinds, " -+ ", "{d:?}");
    let body = agent::diff("let s = \"héllo\";\nlet w = \"世界\";\nend\n", "let s = \"héllo\";\nlet w = \"世界👋🏽 again\";\nend\n");
    assert_eq!(agent::counts(&body), (1, 1));
    let tool = Tool { id: "w".into(), name: "Edit".into(), label: "Update".into(), target: "wide.rs".into(), status: "done".into(), summary: "+1 -1".into(), body, ..Default::default() };
    for w in [1u16, 6, 12, 30, 80] {
        let s = draw_parts(&[Part::Tool(tool.clone())], w, false);
        if w >= 30 {
            assert!(squash(&s).contains("世界👋🏽"), "{w}: {s}");
        }
    }
}

/// A nested list in a narrow pane: the continuation indent is wider than the line.
#[test]
#[ignore = "fails: attempt to subtract with overflow in md::wrap (md.rs:35, width - cont_indent.width())"]
fn qa_chat_md_nested_list_in_a_narrow_pane() {
    let t = crate::theme::get("oriel");
    let text = "- top level item\n    - nested item\n        - third level item with several words\n        12. numbered deep item here";
    for w in [4usize, 8, 10, 11, 12, 13, 20, 40] {
        let lines = md::render(text, w, "  ", &t);
        assert!(lines.len() >= 4, "{w}");
    }
}

/// Markdown with CRLF line ends, emoji and wide characters: no carriage return in any span, and nothing wider than
/// the width asked for.
#[test]
fn qa_chat_md_crlf_emoji_wide() {
    let t = crate::theme::get("oriel");
    let text = "# Tïtle 👋\r\n\r\nSome **bold** 世界 text that wraps around a few times 🦀🦀🦀\r\n- item ✓ with 中文字符 inside\r\n> quoted ✨\r\n```rs\r\nlet x = \"🦀\";\t// tab\r\n```\r\n";
    for w in [8usize, 12, 20, 60] {
        let lines = md::render(text, w, "  ", &t);
        for l in &lines {
            assert!(l.spans.iter().all(|s| !s.content.contains('\r')), "{w}: {l:?}");
            assert!(l.width() <= w.max(8) + 1, "{w}: line {} wide: {l:?}", l.width());
        }
    }
}

/// Wrapping one very long unbroken line (a base64 blob, minified code outside a fence) must scale linearly: a
/// streaming reply is re-rendered every frame.
#[test]
#[ignore = "fails: md::wrap's hard-break loop re-measures and re-collects the rest of the word per line (md.rs:35-56): 10k chars 50 ms, 100k chars 3.8 s per render (debug)"]
fn qa_chat_md_long_unbroken_line_scales() {
    let t = crate::theme::get("oriel");
    let time = |n: usize, runs: usize| {
        let s = "x".repeat(n);
        (0..runs)
            .map(|_| {
                let t0 = Instant::now();
                let l = md::render(&s, 120, "  ", &t);
                assert!(l.len() >= n / 120);
                t0.elapsed()
            })
            .min()
            .unwrap()
    };
    let (small, big) = (time(10_000, 3), time(100_000, 2));
    println!("one render: 10k chars {small:?} · 100k chars {big:?} ({:.0}x for 10x the text)", big.as_secs_f64() / small.as_secs_f64().max(1e-9));
    assert!(big < small * 40, "wrapping a long line is quadratic: 10k chars {small:?}, 100k chars {big:?}");
}

/// Odd parts drawn at any width: deep subagent nesting, a 10k-char target, empty labels, odd body kinds, empty todo
/// lists, CRLF in a queued message.
#[test]
fn qa_chat_activity_odd_parts_any_width() {
    let mut deep = Tool { id: "d40".into(), name: "Read".into(), label: "Read".into(), target: "leaf.rs".into(), status: "running".into(), ..Default::default() };
    for i in (0..40).rev() {
        deep = Tool { id: format!("d{i}"), name: "Agent".into(), label: "Agent".into(), target: format!("level {i}"), status: "running".into(), children: vec![deep], ..Default::default() };
    }
    let weird = Tool {
        id: "w".into(),
        name: "mcp__x__y".into(),
        label: String::new(),
        target: "🦀".repeat(5000),
        status: "error".into(),
        summary: String::new(),
        body: vec!["?\tunknown kind".into(), "@\t".into(), "…\t3 more lines".into(), "+18446744073709551615\thuge number".into(), " ".into(), "!".into()],
        ms: u64::MAX,
        ..Default::default()
    };
    let parts = vec![
        Part::Text { text: "   ".into() },
        Part::Thinking { text: String::new(), tokens: 0 },
        Part::Tool(deep),
        Part::Tool(weird),
        Part::Todos { items: vec![] },
        Part::Todos { items: vec![Todo { text: "写测试 🧪".into(), status: "in_progress".into(), ..Default::default() }] },
        Part::Mark { text: String::new() },
        Part::User { text: "multi\r\nline\r\n".into() },
        Part::Text { text: "end".into() },
    ];
    for w in [0u16, 1, 2, 5, 12, 40, 120] {
        for expanded in [false, true] {
            let s = draw_parts(&parts, w, expanded);
            if w >= 40 {
                assert!(s.contains("end"), "{w} {expanded}: {s}");
            }
        }
    }
}

/// A saved chat whose call has an empty body line (a hand-edited or truncated chat file): opening it must not bring
/// the whole app down while drawing.
#[test]
#[ignore = "fails: agent::split_line(\"\") slices past the end (agent.rs:29 `&s[k.len_utf8()..]`), so an empty body line in a saved chat panics the renderer"]
fn qa_chat_saved_chat_with_an_empty_body_line() {
    let m: Msg = serde_json::from_str(
        r#"{"role":"assistant","content":"ok","parts":[{"kind":"tool","id":"t1","name":"Edit","label":"Update","target":"a.rs","status":"done","summary":"+1 -1","body":["-1\told",""]}]}"#,
    )
    .unwrap();
    let s = draw_parts(&m.parts, 80, false);
    assert!(s.contains("Update(a.rs)"), "{s}");
}

// ================================================================== pane tests, sandboxed in a child process

/// This process is a sandboxed child: ORIEL_DATA_DIR is one of ours.
fn in_sandbox() -> bool {
    std::env::var_os("ORIEL_DATA_DIR").is_some_and(|d| d.to_string_lossy().replace('\\', "/").contains("target/test-scratch/qa-chat/"))
}

macro_rules! sandboxed {
    () => {
        if !in_sandbox() {
            eprintln!("skipped: pane tests run in a child process with their own ORIEL_DATA_DIR (qa_chat_pane_suite)");
            return;
        }
    };
}

fn scratch(name: &str) -> PathBuf {
    let d = std::path::absolute(Path::new("target/test-scratch/qa-chat").join(name)).unwrap();
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Run `tests` (names in this module) in a child copy of the test binary with a fresh sandboxed data dir.
fn run_sandboxed(tag: &str, tests: &[&str]) {
    let data = std::path::absolute(Path::new("target/test-scratch/qa-chat").join(format!("data-{tag}"))).unwrap();
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let names: Vec<String> = tests.iter().map(|t| format!("panes::chat::qa_chat::{t}")).collect();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(&names)
        .args(["--exact", "--ignored", "--test-threads=1", "--color=never"])
        .env("ORIEL_DATA_DIR", &data)
        .env_remove("RUST_TEST_THREADS")
        .output()
        .expect("couldn't start the test binary again");
    let log = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "sandboxed run failed:\n{log}");
    for n in &names {
        assert!(log.contains(&format!("{n} ... ok")), "{n} didn't run:\n{log}");
    }
}

/// The passing pane tests.
const PANE_TESTS: &[&str] = &[
    "child_done_then_late_events",
    "child_error_mid_tool",
    "child_question_then_stop",
    "child_queue_then_error",
    "child_odd_text_renders",
    "child_tiny_sizes",
    "child_slash_bad_args",
    "child_theme_new_weird_names",
    "child_wide_char_editing",
    "child_ask_then_done",
    "child_todos_state_steered_edges",
    "child_5000_line_reply_render",
];

#[test]
fn qa_chat_pane_suite() {
    run_sandboxed("suite", PANE_TESTS);
}

#[test]
#[ignore = "fails: the chat sidebar panics at width < 2 (usize underflow `r.width as usize - 2`, chat.rs:1851)"]
fn qa_chat_bug_side_narrow() {
    run_sandboxed("side", &["child_bug_side_narrow"]);
}

#[test]
#[ignore = "fails: /cwd stores a relative folder as typed, so the chat's agent folder moves with oriel's launch dir (chat.rs:617-619)"]
fn qa_chat_bug_cwd_relative() {
    run_sandboxed("cwd", &["child_bug_cwd_relative"]);
}

#[test]
#[ignore = "fails: /cwd \"C:\\some folder\" (a quoted path, as Explorer's Copy as path gives it) says \"not a folder\" (chat.rs:617)"]
fn qa_chat_bug_cwd_quoted() {
    run_sandboxed("cwdq", &["child_bug_cwd_quoted"]);
}

#[test]
#[ignore = "fails: /provider switch leaves old messages drawn from the render cache in the old AI's style (chat.rs:913-925)"]
fn qa_chat_bug_provider_switch_stale_cache() {
    run_sandboxed("cache", &["child_bug_provider_switch_stale_cache"]);
}

#[test]
#[ignore = "fails: tabs in your message vanish when drawn (pasted tab-indented code loses its indentation, chat.rs:936/947)"]
fn qa_chat_bug_tabs_in_user_message() {
    run_sandboxed("tabs", &["child_bug_tabs_in_user_message"]);
}

#[test]
#[ignore = "fails: a Question with no questions swallows every key, esc included (chat.rs:386-387 + 1605)"]
fn qa_chat_bug_empty_question_traps_keys() {
    run_sandboxed("emptyq", &["child_bug_empty_question_traps_keys"]);
}

fn pane(k: &mut Kit, provider: &str) -> Chat {
    // nothing listens on port 9: the availability probe never reaches a real server
    k.config.ai.ollama_url = "http://127.0.0.1:9".into();
    let mut c = Chat::new(&k.config);
    c.avail = vec!["claude", "codex", "ollama", "openai", "anthropic"];
    c.provider = provider.into();
    c.chat.provider = Some(provider.into());
    let work = scratch("work");
    c.chat.cwd = Some(work.to_string_lossy().to_string());
    c.launch_dir = work;
    c
}

/// A chat mid-reply, as if `send` had started a stream.
fn replying(k: &mut Kit, provider: &str, prompt: &str) -> Chat {
    let mut c = pane(k, provider);
    c.chat.title = store::title_from(prompt);
    c.chat.messages.push(Msg { role: "user".into(), content: prompt.into(), ..Default::default() });
    c.chat.messages.push(Msg { role: "assistant".into(), model: Some(provider.into()), ..Default::default() });
    let steer = providers::steerable(provider).then(|| channel().0);
    c.stream = Some(Stream { stop: Arc::default(), inbox: Arc::default(), status: String::new(), started: Instant::now(), tokens: 0, steer });
    c
}

fn deliver(k: &mut Kit, c: &mut Chat, evs: impl IntoIterator<Item = Ev>) {
    if let Some(s) = &c.stream {
        s.inbox.lock().unwrap().extend(evs);
    }
    k.poll(c);
}

fn cmd(k: &mut Kit, c: &mut Chat, line: &str) {
    let mut cx = Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: &mut k.actions, focused: true, time: k.time };
    c.run_command(line, &mut cx);
}

fn last(c: &Chat) -> &Msg {
    c.chat.messages.last().unwrap()
}

fn bash(id: &str, cmd: &str, status: &str) -> Tool {
    Tool { id: id.into(), name: "Bash".into(), label: "Bash".into(), target: cmd.into(), status: status.into(), ..Default::default() }
}

fn find_tool<'a>(m: &'a Msg, id: &str) -> Option<&'a Tool> {
    m.parts.iter().find_map(|p| if let Part::Tool(t) = p { (t.id == id).then_some(t) } else { None })
}

/// Tokens and events that arrive after Done in the same batch, and polls after the stream is gone.
#[test]
#[ignore = "runs inside a sandboxed child process (qa_chat_pane_suite / qa_chat_bug_*)"]
fn child_done_then_late_events() {
    sandboxed!();
    let mut k = Kit::new();
    let mut c = replying(&mut k, "ollama", "say hello");
    deliver(&mut k, &mut c, [Ev::Token("Hello".into()), Ev::Done { note: Some("1 tokens".into()) }, Ev::Token(" (late)".into()), Ev::Status("late status".into()), Ev::Usage(99)]);
    assert!(c.stream.is_none(), "Done ends the reply");
    println!("content after a token that came after Done in the same batch: {:?}", last(&c).content);
    assert!(last(&c).content.starts_with("Hello"));
    assert_eq!(last(&c).note.as_deref(), Some("1 tokens"));
    let saved = std::fs::read_to_string(store::dir().join(format!("{}.json", c.chat.id))).expect("the chat was saved when the reply ended");
    assert!(saved.contains("Hello"));
    k.poll(&mut c); // no stream: nothing happens
    let s = k.render(&mut c, 80, 20);
    assert!(s.contains("Hello") && !s.contains("esc to interrupt"), "{s}");
}

/// An error in the middle of a running call: the call ends as stopped, the error is the note; an error before
/// anything arrived is the reply itself.
#[test]
#[ignore = "runs inside a sandboxed child process (qa_chat_pane_suite / qa_chat_bug_*)"]
fn child_error_mid_tool() {
    sandboxed!();
    let mut k = Kit::new();
    let mut c = replying(&mut k, "claude", "build it");
    deliver(&mut k, &mut c, [Ev::Token("Building.".into()), Ev::Tool(bash("b1", "cargo build", "running")), Ev::Thinking { text: "hmm".into(), tokens: 3 }]);
    let s = k.render(&mut c, 100, 30);
    assert!(s.contains("Running cargo build"), "{s}");
    deliver(&mut k, &mut c, [Ev::Error("connection reset by peer".into())]);
    assert!(c.stream.is_none());
    assert_eq!(find_tool(last(&c), "b1").map(|t| t.status.as_str()), Some("stopped"));
    assert_eq!(last(&c).note.as_deref(), Some("⚠ connection reset by peer"));
    let s = k.render(&mut c, 100, 30);
    assert!(s.contains("connection reset by peer") && s.contains("Bash(cargo build)") && !s.contains("esc to interrupt"), "{s}");
    let mut c = replying(&mut k, "claude", "hi");
    deliver(&mut k, &mut c, [Ev::Error("claude isn't installed".into())]);
    assert_eq!(last(&c).content, "⚠ claude isn't installed");
}

/// Claude asks, you answer one of two, skip the rest, then stop. A question still open when the reply ends (or is
/// stopped) closes its channel, which the bridge treats as skipped, and the keys go back to the composer.
#[test]
#[ignore = "runs inside a sandboxed child process (qa_chat_pane_suite / qa_chat_bug_*)"]
fn child_question_then_stop() {
    sandboxed!();
    let mut k = Kit::new();
    let q = |s: &str| approve::Q { question: s.into(), header: String::new(), multi: false, options: vec![("Yes".into(), String::new()), ("No".into(), String::new())] };
    let mut c = replying(&mut k, "claude", "quiz me");
    let (tx, rx) = channel();
    deliver(&mut k, &mut c, [Ev::Question(approve::Question { qs: vec![q("First?"), q("Second?")], reply: tx })]);
    assert!(k.render(&mut c, 100, 30).contains("First?"));
    k.key(&mut c, KeyCode::Char('1'));
    assert!(k.render(&mut c, 100, 30).contains("Second?"));
    k.key(&mut c, KeyCode::Esc); // skips the question, the reply goes on
    assert_eq!(rx.try_recv(), Ok(None));
    assert!(c.stream.is_some());
    k.key(&mut c, KeyCode::Esc); // now it stops
    assert!(c.stream.is_none());
    // the reply ends with a question open
    let mut c = replying(&mut k, "claude", "quiz me again");
    let (tx, rx) = channel();
    deliver(&mut k, &mut c, [Ev::Question(approve::Question { qs: vec![q("Third?")], reply: tx }), Ev::Error("usage limit".into())]);
    assert!(c.questions.is_empty());
    assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
    k.typ(&mut c, "ok");
    assert_eq!(c.input, "ok");
    let s = k.render(&mut c, 100, 30);
    assert!(!s.contains("Third?") && !s.contains("asking you"), "{s}");
    // stopped with a question showing
    let mut c = replying(&mut k, "claude", "one more");
    let (tx, rx) = channel();
    deliver(&mut k, &mut c, [Ev::Question(approve::Question { qs: vec![q("Fourth?")], reply: tx })]);
    c.stop();
    assert!(c.questions.is_empty() && c.stream.is_none());
    assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
}

/// Messages queued while a reply runs come back to the box when it fails, with what you were typing.
#[test]
#[ignore = "runs inside a sandboxed child process (qa_chat_pane_suite / qa_chat_bug_*)"]
fn child_queue_then_error() {
    sandboxed!();
    let mut k = Kit::new();
    let mut c = replying(&mut k, "codex", "refactor");
    k.typ(&mut c, "also add tests");
    k.key(&mut c, KeyCode::Enter);
    k.typ(&mut c, "and docs");
    k.key(&mut c, KeyCode::Enter);
    assert_eq!(c.queue.len(), 2);
    k.typ(&mut c, "draft");
    deliver(&mut k, &mut c, [Ev::Token("Working".into()), Ev::Error("rate limited".into())]);
    assert!(c.stream.is_none() && c.queue.is_empty());
    assert_eq!(c.input, "also add tests\n\nand docs\n\ndraft");
    assert_eq!(c.cursor, c.input.chars().count());
    assert!(c.info.iter().any(|l| l.contains("the reply failed")), "{:?}", c.info);
    assert_eq!(last(&c).note.as_deref(), Some("⚠ rate limited"));
    assert_eq!(c.chat.messages.len(), 2, "nothing was sent on its own");
    let s = k.render(&mut c, 100, 24);
    assert!(s.contains("draft") && s.contains("rate limited"), "{s}");
}

/// CRLF, emoji, wide characters, ANSI in output, a 5000-char target: nothing leaks control characters to the screen.
#[test]
#[ignore = "runs inside a sandboxed child process (qa_chat_pane_suite / qa_chat_bug_*)"]
fn child_odd_text_renders() {
    sandboxed!();
    let mut k = Kit::new();
    let mut c = replying(&mut k, "claude", "odd text 👋🏽");
    let mut p = claude();
    let _ = feed(&mut p, json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "o1", "name": "Bash", "input": {"command": "npm test"}}]}}), 0);
    let (_, out) = feed(&mut p, json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "o1", "content": "x"}]}, "tool_use_result": {"stdout": "\x1b[32m✓ passed\x1b[0m 世界\r\n\tindented", "stderr": ""}}), 5);
    let mut evs = vec![Ev::Token("Line one\r\nLine two 👋🏽 世界\r\n\r\n- ✓ done\r\n".into())];
    evs.extend(out);
    evs.push(Ev::Todos(vec![Todo { text: "写测试 🧪".into(), active: "写测试中 🧪".into(), status: "in_progress".into(), id: "1".into() }, Todo { text: "ship 🚀".into(), status: "pending".into(), ..Default::default() }]));
    evs.push(Ev::Tool(bash("long", &"x".repeat(5000), "running")));
    evs.push(Ev::Mark("compacted 🗜".into()));
    evs.push(Ev::Steered("follow-up\r\nwith CRLF".into()));
    deliver(&mut k, &mut c, evs);
    for (w, h) in [(100u16, 40u16), (40, 20), (24, 12)] {
        let s = k.render(&mut c, w, h);
        assert!(!s.contains(['\x1b', '\r', '\t']) && !s.contains("[32m"), "{w}x{h}: {s}");
    }
    let s = k.render(&mut c, 120, 60);
    println!("{s}");
    let sq = squash(&s);
    assert!(sq.contains("Linetwo👋🏽世界") && sq.contains("✓passed世界") && sq.contains("写测试中🧪") && sq.contains("follow-up"), "{s}");
}

/// Tiny terminals: every state the chat can be in, drawn at sizes down to 1x1, and the hero and / menu too.
#[test]
#[ignore = "runs inside a sandboxed child process (qa_chat_pane_suite / qa_chat_bug_*)"]
fn child_tiny_sizes() {
    sandboxed!();
    let mut k = Kit::new();
    let sizes = [(1u16, 1u16), (2, 2), (5, 5), (8, 6), (10, 6), (12, 8), (20, 10), (30, 6), (40, 12), (70, 9)];
    let mut c = replying(&mut k, "claude", "do everything");
    k.typ(&mut c, "a queued message 👋");
    k.key(&mut c, KeyCode::Enter);
    let mut body = Tool { body: (1..=10).map(|i| agent::line('>', None, &format!("line {i}"))).collect(), ..bash("t1", "cargo test --all", "running") };
    body.summary = "10 lines".into();
    let todos = (0..7).map(|i| Todo { text: format!("step {i} with a fairly long description"), status: if i < 3 { "completed" } else if i == 3 { "in_progress" } else { "pending" }.into(), ..Default::default() }).collect();
    let (qtx, _qrx) = channel();
    let (atx, _arx) = channel();
    deliver(
        &mut k,
        &mut c,
        [
            Ev::Token("Working on it.".into()),
            Ev::Tool(body),
            Ev::Todos(todos),
            Ev::Question(approve::Question { qs: vec![approve::Q { question: "Which one? ".repeat(10), header: "Pick".into(), multi: true, options: vec![("a very long option label indeed".into(), "and a long description".into()), ("b".into(), String::new())] }], reply: qtx }),
            Ev::Ask(approve::Ask { tool: "Bash".into(), label: "Bash".into(), target: "rm -rf build ".repeat(8), body: (0..9).map(|i| agent::line('>', None, &format!("cmd {i}"))).collect(), reply: atx }),
        ],
    );
    for (w, h) in sizes {
        k.render(&mut c, w, h);
        k.render_html(&mut c, w, h, "target/test-scratch/qa-chat/tiny.html");
        k.render_side(&mut c, w.max(2), h);
    }
    for provider in ["claude", "none"] {
        let mut c = pane(&mut k, provider);
        if provider == "none" {
            c.provider = "none".into();
            c.chat.provider = None;
        }
        for (w, h) in sizes {
            k.render(&mut c, w, h);
        }
        c.input = "/".into();
        c.cursor = 1;
        for (w, h) in sizes {
            k.render(&mut c, w, h);
        }
        c.input = "/theme ".into();
        c.cursor = 7;
        for (w, h) in sizes {
            k.render(&mut c, w, h);
        }
    }
}

/// Slash commands with arguments they don't know: each says what's possible and changes nothing.
#[test]
#[ignore = "runs inside a sandboxed child process (qa_chat_pane_suite / qa_chat_bug_*)"]
fn child_slash_bad_args() {
    sandboxed!();
    let mut k = Kit::new();
    let mut c = pane(&mut k, "claude");
    c.avail = vec!["claude", "codex"];
    c.perms = "edits".into();
    c.effort = String::new();
    cmd(&mut k, &mut c, "/model    ");
    assert!(c.info.first().is_some_and(|l| l.contains("Claude Code is using")), "{:?}", c.info);
    cmd(&mut k, &mut c, "/provider nope");
    assert_eq!(c.info, vec!["no AI called nope — try /provider".to_string()]);
    assert_eq!(c.provider_of(), "claude");
    cmd(&mut k, &mut c, "/provider ollama");
    assert!(c.info[0].contains("isn't set up"), "{:?}", c.info);
    assert_eq!(c.provider_of(), "claude");
    cmd(&mut k, &mut c, "/provider");
    assert_eq!(c.info.iter().filter(|l| l.contains("(not set up here)")).count(), 3, "{:?}", c.info);
    cmd(&mut k, &mut c, "/perms sideways");
    assert_eq!(c.perms, "edits");
    assert!(c.info[0].starts_with("permissions now: edits"), "{:?}", c.info);
    cmd(&mut k, &mut c, "/perms ask please");
    assert_eq!(c.perms, "edits");
    cmd(&mut k, &mut c, "/effort turbo");
    assert_eq!(c.effort, "");
    assert!(c.info[0].starts_with("effort now: default"), "{:?}", c.info);
    cmd(&mut k, &mut c, "/effort HIGH");
    assert_eq!(c.effort, "high");
    cmd(&mut k, &mut c, "/effort default");
    assert_eq!(c.effort, "");
    // /cwd to a folder that isn't there, to a file, to nothing
    let before = c.chat.cwd.clone();
    c.chat.state.insert("claude".into(), json!({"session": "s1"}));
    let missing = scratch("work").join("no-such-folder");
    cmd(&mut k, &mut c, &format!("/cwd {}", missing.display()));
    assert!(c.info[0].starts_with("not a folder"), "{:?}", c.info);
    let file = scratch("work").join("a-file.txt");
    std::fs::write(&file, "x").unwrap();
    cmd(&mut k, &mut c, &format!("/cwd {}", file.display()));
    assert!(c.info[0].starts_with("not a folder"), "{:?}", c.info);
    cmd(&mut k, &mut c, "/cwd");
    assert!(c.info[0].starts_with("not a folder"), "{:?}", c.info);
    assert_eq!(c.chat.cwd, before, "a bad /cwd changes nothing");
    assert!(c.chat.state.contains_key("claude"), "and keeps the session");
    cmd(&mut k, &mut c, "/key openai");
    assert!(c.info[0].starts_with("/key openai <key>"), "{:?}", c.info);
    cmd(&mut k, &mut c, "/key banana sk-123");
    assert!(c.info[0].starts_with("/key openai <key>"), "{:?}", c.info);
    cmd(&mut k, &mut c, "/frobnicate --now");
    assert_eq!(c.info, vec!["unknown command /frobnicate — type / to see them".to_string()]);
    cmd(&mut k, &mut c, "/model 🤖 turbo max");
    assert_eq!(c.chat.model.as_deref(), Some("🤖 turbo max"));
    assert!(!c.chat.state.contains_key("claude"), "a new model drops the old session");
    cmd(&mut k, &mut c, "/model DEFAULT");
    assert_eq!(c.chat.model, None);
    cmd(&mut k, &mut c, "/retry");
    assert!(c.chat.messages.is_empty() && c.stream.is_none());
    k.actions.clear();
    cmd(&mut k, &mut c, "/theme");
    assert!(matches!(k.actions.last(), Some(Action::Palette(p)) if p == "theme "));
    cmd(&mut k, &mut c, "/theme no-such-theme");
    assert!(matches!(k.actions.last(), Some(Action::SetTheme(t)) if t == "no-such-theme"));
    // the same through the keyboard: the menu has nothing to offer, enter runs it as typed
    k.typ(&mut c, "/perms zzz");
    k.key(&mut c, KeyCode::Enter);
    assert_eq!(c.perms, "edits");
    assert!(c.input.is_empty());
    let s = k.render(&mut c, 100, 30);
    assert!(s.contains("/perms ask"), "{s}");
}

/// /theme new with names that aren't file names: the file always lands in the themes folder, under a clean name.
#[test]
#[ignore = "runs inside a sandboxed child process (qa_chat_pane_suite / qa_chat_bug_*)"]
fn child_theme_new_weird_names() {
    sandboxed!();
    let dir = scratch("themes");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    crate::theme::TEST_DIR.with(|t| *t.borrow_mut() = Some(dir.clone()));
    let mut k = Kit::new();
    let mut c = pane(&mut k, "claude");
    let long = "x".repeat(300);
    let names = ["../../escape", "a/b\\c:d*e?f", "  spaced   name  ", "🎨", "Ünïcode テーマ", "new", "ultra", "\"quoted\"", "nul", long.as_str()];
    let mut made = vec![];
    for n in names {
        k.actions.clear();
        cmd(&mut k, &mut c, &format!("/theme new {n}"));
        let msg = c.info.join(" ");
        if msg.starts_with("made your theme") {
            let applied = k.actions.iter().find_map(|a| if let Action::ApplyTheme(x) = a { Some(x.clone()) } else { None }).expect("the new theme is applied");
            let file = dir.join(format!("{applied}.toml"));
            assert!(file.is_file(), "{n:?}: said {msg:?} but {file:?} isn't there");
            assert!(!applied.contains(['/', '\\', '.', ' ', ':', '*', '?', '"']), "{n:?} -> {applied:?}");
            made.push(applied);
        } else {
            assert!(msg.starts_with("couldn't make the theme"), "{n:?}: {msg}");
        }
    }
    println!("{made:?}");
    assert!(made.contains(&"ultra-2".to_string()), "a built-in name isn't overwritten: {made:?}");
    assert!(!dir.parent().unwrap().join("escape.toml").exists() && !dir.parent().unwrap().parent().unwrap().join("escape.toml").exists(), "nothing escaped the themes folder");
    crate::theme::TEST_DIR.with(|t| *t.borrow_mut() = None);
}

/// Editing in the composer across wide characters and emoji.
#[test]
#[ignore = "runs inside a sandboxed child process (qa_chat_pane_suite / qa_chat_bug_*)"]
fn child_wide_char_editing() {
    sandboxed!();
    let mut k = Kit::new();
    let mut c = pane(&mut k, "claude");
    k.typ(&mut c, "héllo 世界👋");
    k.key(&mut c, KeyCode::Left);
    k.key(&mut c, KeyCode::Left);
    k.key(&mut c, KeyCode::Backspace);
    assert_eq!(c.input, "héllo 界👋");
    k.key(&mut c, KeyCode::Home);
    k.key(&mut c, KeyCode::Delete);
    assert_eq!(c.input, "éllo 界👋");
    k.key(&mut c, KeyCode::End);
    for (w, h) in [(40u16, 10u16), (12, 8), (6, 6)] {
        k.render(&mut c, w, h);
    }
    k.typ(&mut c, &"世".repeat(500));
    k.key_mod(&mut c, KeyCode::Char('a'), KeyModifiers::CONTROL);
    for (w, h) in [(40u16, 10u16), (13, 8), (7, 6)] {
        k.render(&mut c, w, h);
    }
    k.key_mod(&mut c, KeyCode::Char('e'), KeyModifiers::CONTROL);
    let s = k.render(&mut c, 40, 10);
    assert!(s.contains('世'), "{s}");
}

/// An approval still waiting when the reply ends is dropped (the bridge reads that as no); "always" answers later
/// ones itself.
#[test]
#[ignore = "runs inside a sandboxed child process (qa_chat_pane_suite / qa_chat_bug_*)"]
fn child_ask_then_done() {
    sandboxed!();
    let mut k = Kit::new();
    let mut c = replying(&mut k, "claude", "clean up");
    let (tx, rx) = channel();
    let ask = |tool: &str, tx: &std::sync::mpsc::Sender<approve::Decision>| approve::Ask { tool: tool.into(), label: tool.into(), target: "rm -rf build".into(), body: vec![], reply: tx.clone() };
    deliver(&mut k, &mut c, [Ev::Ask(ask("Bash", &tx))]);
    drop(tx);
    deliver(&mut k, &mut c, [Ev::Done { note: None }]);
    assert!(c.asks.is_empty());
    assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
}

/// Todo lists that empty out, all-done lists, odd state keys, a steered message nobody queued.
#[test]
#[ignore = "runs inside a sandboxed child process (qa_chat_pane_suite / qa_chat_bug_*)"]
fn child_todos_state_steered_edges() {
    sandboxed!();
    let mut k = Kit::new();
    let mut c = replying(&mut k, "claude", "plan it");
    let done: Vec<Todo> = (0..7).map(|i| Todo { text: format!("t{i}"), status: "completed".into(), ..Default::default() }).collect();
    deliver(&mut k, &mut c, [Ev::Todos(done)]);
    assert!(k.render(&mut c, 100, 30).contains("7/7 done"));
    deliver(&mut k, &mut c, [Ev::Todos(vec![])]);
    let s = k.render(&mut c, 100, 30);
    assert!(!s.contains("done ·") && !s.contains("7/7"), "{s}");
    deliver(&mut k, &mut c, [Ev::State(String::new(), "x".into()), Ev::State("claude.session".into(), "abc".into()), Ev::State("a.b.c".into(), "v".into())]);
    assert_eq!(c.chat.state[""]["session"], "x");
    assert_eq!(c.chat.state["claude"]["session"], "abc");
    assert_eq!(c.chat.state["a"]["b.c"], "v");
    k.typ(&mut c, "mine");
    k.key(&mut c, KeyCode::Enter);
    deliver(&mut k, &mut c, [Ev::Steered("something nobody queued".into())]);
    assert_eq!(c.queue.len(), 1, "an unknown steered message leaves the queue alone");
    assert!(last(&c).parts.iter().any(|p| matches!(p, Part::User { text } if text == "something nobody queued")));
    deliver(&mut k, &mut c, [Ev::Usage(u64::MAX)]);
    k.render(&mut c, 100, 30);
}

/// 5000 lines of streamed text and a 5000-line command output: draws, and in reasonable time.
#[test]
#[ignore = "runs inside a sandboxed child process (qa_chat_pane_suite / qa_chat_bug_*)"]
fn child_5000_line_reply_render() {
    sandboxed!();
    let mut k = Kit::new();
    let mut c = replying(&mut k, "claude", "dump it all");
    let mut p = claude();
    let _ = feed(&mut p, json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "big", "name": "Bash", "input": {"command": "seq 5000"}}]}}), 0);
    let stdout: String = (1..=5000).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
    let (_, out) = feed(&mut p, json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "big", "content": "x"}]}, "tool_use_result": {"stdout": stdout, "stderr": ""}}), 5);
    let text: String = (1..=5000).map(|i| format!("reply line {i} with some words in it\n")).collect();
    let mut evs = vec![Ev::Token(text)];
    evs.extend(out);
    evs.push(Ev::Token("the end".into()));
    deliver(&mut k, &mut c, evs);
    let t0 = Instant::now();
    let s = k.render(&mut c, 120, 40);
    let took = t0.elapsed();
    println!("one frame with 5000 streamed lines + 5000 lines of output: {took:?}");
    assert!(s.contains("the end"), "{s}");
    k.key_mod(&mut c, KeyCode::Char('o'), KeyModifiers::CONTROL);
    c.scroll = 100_000;
    let s = k.render(&mut c, 120, 40);
    assert!(s.contains("reply line 1 "), "scrolled to the top: {s}");
    assert!(took < Duration::from_secs(3), "{took:?}");
}

// ------------------------------------------------------------------ failing pane tests (see the qa_chat_bug_* wrappers)

#[test]
#[ignore = "runs inside a sandboxed child process (qa_chat_pane_suite / qa_chat_bug_*)"]
fn child_bug_side_narrow() {
    sandboxed!();
    let mut k = Kit::new();
    let mut c = pane(&mut k, "claude");
    let mut other = store::Chat::new("claude");
    other.title = "an older chat".into();
    other.messages.push(Msg { role: "user".into(), content: "hi".into(), ..Default::default() });
    c.chats = vec![other];
    for w in [3u16, 2, 1, 0] {
        k.render_side(&mut c, w, 10);
    }
}

#[test]
#[ignore = "runs inside a sandboxed child process (qa_chat_pane_suite / qa_chat_bug_*)"]
fn child_bug_cwd_relative() {
    sandboxed!();
    let mut k = Kit::new();
    let mut c = pane(&mut k, "claude");
    cmd(&mut k, &mut c, "/cwd src");
    let cwd = c.chat.cwd.clone().unwrap();
    assert!(Path::new(&cwd).is_dir(), "{cwd}");
    assert!(Path::new(&cwd).is_absolute(), "/cwd saved {cwd:?}: a relative folder means something else the next time oriel starts elsewhere");
}

#[test]
#[ignore = "runs inside a sandboxed child process (qa_chat_pane_suite / qa_chat_bug_*)"]
fn child_bug_cwd_quoted() {
    sandboxed!();
    let mut k = Kit::new();
    let mut c = pane(&mut k, "claude");
    let dir = scratch("work").join("with space");
    std::fs::create_dir_all(&dir).unwrap();
    // what Explorer's "Copy as path" puts on the clipboard
    cmd(&mut k, &mut c, &format!("/cwd \"{}\"", dir.display()));
    assert_eq!(c.chat.cwd.as_deref().map(Path::new), Some(dir.as_path()), "{:?}", c.info);
}

#[test]
#[ignore = "runs inside a sandboxed child process (qa_chat_pane_suite / qa_chat_bug_*)"]
fn child_bug_provider_switch_stale_cache() {
    sandboxed!();
    let mut k = Kit::new();
    let mut c = pane(&mut k, "ollama");
    c.avail = vec!["claude", "ollama"];
    c.chat.title = "hello".into();
    c.chat.messages.push(Msg { role: "user".into(), content: "hello there".into(), ..Default::default() });
    c.chat.messages.push(Msg { role: "assistant".into(), model: Some("ollama".into()), content: "hi!".into(), ..Default::default() });
    let before = k.render(&mut c, 100, 24);
    assert!(before.contains("╭") && before.contains("you"), "{before}");
    cmd(&mut k, &mut c, "/provider claude");
    assert_eq!(c.provider_of(), "claude");
    let after = k.render(&mut c, 100, 24);
    c.cache.clear();
    let fresh = k.render(&mut c, 100, 24);
    println!("--- after /provider claude:\n{after}\n--- the same chat drawn fresh:\n{fresh}");
    assert_eq!(after, fresh, "after /provider the old messages come from a stale cache");
}

#[test]
#[ignore = "runs inside a sandboxed child process (qa_chat_pane_suite / qa_chat_bug_*)"]
fn child_bug_tabs_in_user_message() {
    sandboxed!();
    let mut k = Kit::new();
    let mut c = pane(&mut k, "ollama");
    {
        let mut cx = Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: &mut k.actions, focused: true, time: k.time };
        Pane::paste(&mut c, "fn main() {\r\n\tprintln!(\"hi\");\r\n}", &mut cx);
    }
    assert_eq!(c.input, "fn main() {\n\tprintln!(\"hi\");\n}");
    k.key(&mut c, KeyCode::Enter); // sends; the provider refuses to run in tests, which is fine here
    k.poll(&mut c);
    let s = k.render(&mut c, 100, 30);
    println!("{s}");
    let row = s.lines().find(|l| l.contains("println!")).expect("the message is drawn");
    let inside = row.split("│ ").nth(1).unwrap_or(row);
    assert!(inside.starts_with(' '), "the tab indent is gone: {row:?}");
}

#[test]
#[ignore = "runs inside a sandboxed child process (qa_chat_pane_suite / qa_chat_bug_*)"]
fn child_bug_empty_question_traps_keys() {
    sandboxed!();
    let mut k = Kit::new();
    let mut c = replying(&mut k, "claude", "ask me nothing");
    let (tx, _rx) = channel();
    deliver(&mut k, &mut c, [Ev::Question(approve::Question { qs: vec![], reply: tx })]);
    k.key(&mut c, KeyCode::Esc);
    k.key(&mut c, KeyCode::Esc);
    assert!(c.stream.is_none() || c.questions.is_empty(), "esc did nothing: an empty question holds the keyboard");
    k.typ(&mut c, "hi");
    assert_eq!(c.input, "hi");
}
