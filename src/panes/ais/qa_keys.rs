//! QA key-mash for "your AIs" (see src/qa_keys.rs). Every path points into a scratch folder (util::Paths::under),
//! CLI detection is off (live = false), and under cfg(test) installs, sign-ins and web pages are only recorded —
//! so "yes" to connecting or to a token-saver preset edits scratch copies of the settings files, nothing else.

use super::util::Paths;
use crossterm::event::KeyCode;
use super::*;
use crate::qa_keys::{Ev, Opts, mash_pane, scratch};
use std::path::Path;

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

/// Some usage logs and settings to read (fake numbers, no prompts), plus a broken file or two.
fn fixture(root: &Path) {
    let now = util::now();
    let line = |t: i64, id: &str, model: &str, i: u64, o: u64| {
        let (y, m, d) = util::civil_from_days(t.div_euclid(86400));
        let s = t.rem_euclid(86400);
        format!(
            r#"{{"type":"assistant","requestId":"r{id}","timestamp":"{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z","cwd":"C:/work/中文","message":{{"id":"{id}","model":"{model}","role":"assistant","content":[],"usage":{{"input_tokens":{i},"output_tokens":{o},"cache_creation_input_tokens":10,"cache_read_input_tokens":2000}}}}}}"#,
            s / 3600,
            (s / 60) % 60,
            s % 60
        )
    };
    let lines: Vec<String> = (0..300).map(|n| line(now - n * 3571, &format!("m{n}"), ["claude-opus-5-5", "claude-sonnet-5", "claude-haiku-4-5", "mystery-model"][n as usize % 4], (n * 97) as u64, (n * 13) as u64)).collect();
    write(&root.join("home/.claude/projects/proj/session.jsonl"), &(lines.join("\n") + "\n{not json\n"));
    write(&root.join("home/.claude/settings.json"), r#"{"model":"opus","env":{"X":"1"}}"#);
    write(&root.join("home/.claude.json"), "{ broken");
    write(&root.join("home/.codex/config.toml"), "model = \"gpt-5\"\n[mcp_servers.x]\ncommand = \"x\"\n");
}

pub(crate) fn safe(root: &Path) -> Ais {
    Ais::with(Paths::under(root), false)
}

/// Nothing to step around: the size crashes below (a popup under 64 columns, the install view under ~14, the
/// usage view when wide but short) are fixed, so the mash draws every size in every state.
fn guard(_p: &mut Ais, _ev: &mut Ev) -> bool {
    true
}

#[test]
fn qa_keys_ais() {
    let root = scratch("ais");
    fixture(&root);
    mash_pane("ais", Opts { snap: Some("ais".into()), ..Default::default() }, move |_| safe(&root), guard);
}

#[test]
fn qa_keys_ais_nothing_installed() {
    let root = scratch("ais-empty");
    mash_pane("ais-empty", Opts { keys: crate::qa_keys::keys_for(1500), ..Default::default() }, move |_| safe(&root), guard);
}

/// Found by qa_keys_ais_nothing_installed. In the app on an 80-column terminal (the pane is ~52 wide): F3, c —
/// the "connect" answer pops up and oriel exits. Any of its popups (asks, notes) does it below 64 columns.
#[test]
fn qa_keys_ais_bug_popup_in_a_narrow_pane() {
    let root = scratch("ais-bug-popup");
    let mut k = crate::testkit::Kit::new();
    let mut p = safe(&root);
    k.render(&mut p, 120, 40);
    k.key(&mut p, KeyCode::Char('c')); // overview: connect oriel to your AIs (the plan comes back from a thread)
    let t0 = std::time::Instant::now();
    while p.ask.is_none() && p.note.is_none() && t0.elapsed() < std::time::Duration::from_secs(10) {
        k.wait_wake(&mut p, 100);
    }
    assert!(p.ask.is_some() || p.note.is_some(), "no popup came up");
    k.render(&mut p, 52, 20);
}

/// Found by qa_keys_ais. The install view's detail card does `width - 10` on a card narrower than 10 columns.
#[test]
fn qa_keys_ais_bug_install_view_very_narrow() {
    let root = scratch("ais-bug-narrow");
    fixture(&root);
    let mut k = crate::testkit::Kit::new();
    let mut p = safe(&root);
    k.key(&mut p, KeyCode::Char('2')); // the install view
    k.render(&mut p, 12, 20);
}

/// Found by qa_keys_ais. The usage view's right column (shown once the pane is 120+ wide) sizes "top projects"
/// as max(rest / 2, 4) and "top sessions" as rest - that, so with fewer than 4 rows to spare it underflows. In
/// the app: a 150-column terminal, split the screen top/bottom (ctrl+space -), F3, 3 — oriel exits.
#[test]
fn qa_keys_ais_bug_usage_view_wide_and_short() {
    let root = scratch("ais-bug-usage");
    fixture(&root);
    let mut k = crate::testkit::Kit::new();
    let mut p = safe(&root);
    k.key(&mut p, KeyCode::Char('3')); // the usage view
    let t0 = std::time::Instant::now();
    while p.usage_srcs().is_empty() && t0.elapsed() < std::time::Duration::from_secs(10) {
        k.wait_wake(&mut p, 100);
    }
    assert!(!p.usage_srcs().is_empty(), "the fixture's usage log wasn't read");
    k.render(&mut p, 150, 40); // fine
    k.render(&mut p, 150, 14); // about half of a 30-row terminal
}
