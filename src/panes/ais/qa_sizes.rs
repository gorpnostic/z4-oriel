//! QA sizes for "your AIs" (see crate::qa_sizes). The real pane reads ~/.claude, ~/.claude.json and ~/.codex
//! whatever the environment says, so this sweeps one pointed at a scratch folder (util::Paths::under) with
//! awkward project names, versions and models: nothing is detected, installed or signed in.

use super::catalog::{CLIS, Found};
use super::util::{Paths, civil_from_days, now};
use super::*;
use crate::qa_sizes::{AWKWARD, Known, scratch, sweep, verdict};
use crate::testkit::Kit;
use std::path::{Path, PathBuf};

fn iso(t: i64) -> String {
    let (y, m, d) = civil_from_days(t.div_euclid(86400));
    let s = t.rem_euclid(86400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z", s / 3600, (s / 60) % 60, s % 60)
}

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

/// Scratch home with a Claude log per awkward project, a Codex rollout and settings files.
fn paths(root: &Path) -> Paths {
    let p = Paths::under(root);
    let n = now();
    for (j, proj) in AWKWARD.iter().enumerate() {
        let cwd = format!("C:\\Code\\{proj}");
        let mut a = String::new();
        for d in 0..10i64 {
            let t = n - d * 86400 - 600 * j as i64;
            let k = (10 - d) as u64 * (j as u64 + 1);
            a.push_str(&format!(
                r#"{{"type":"assistant","requestId":"r{j}-{d}","timestamp":"{}","cwd":{},"message":{{"id":"m{j}-{d}","model":"claude-{}","role":"assistant","content":[],"usage":{{"input_tokens":{},"output_tokens":{},"cache_creation_input_tokens":{},"cache_read_input_tokens":{}}}}}}}"#,
                iso(t),
                serde_json::to_string(&cwd).unwrap(),
                ["opus-5-5", "sonnet-5", "haiku-4-5-20251001"][j % 3],
                40 * k,
                2_000 * k,
                30_000 * k,
                400_000 * k
            ));
            a.push('\n');
        }
        write(&p.claude.join("projects").join(format!("proj{j}")).join(format!("s{j}.jsonl")), &a);
    }
    write(&p.claude_settings(), "{\n  \"model\": \"opus\"\n}\n");
    write(&p.claude_json, r#"{"mcpServers":{"a-very-long-mcp-server-name-that-goes-on-and-on":{},"日本語":{}}}"#);
    write(&p.claude.join("CLAUDE.md"), &format!("{}\n", AWKWARD.join("\n")).repeat(40));
    write(&p.codex_config(), "model = \"gpt-5.6-terra\"\n");
    write(&p.sink_file(), &format!(r#"{{"received_at":{},"rate_limits":{{"five_hour":{{"used_percentage":99,"resets_at":{}}},"seven_day":{{"used_percentage":100,"resets_at":{}}}}},"cost":{{"total_cost_usd":123456.78}}}}"#, n - 5, n + 60, n + 86400));
    p
}

/// A pane on `root` in `view`, with every CLI "found" under awkward paths and a long Ollama model list.
fn fixture(root: &Path, view: View) -> Ais {
    let mut a = Ais::with(paths(root), false);
    for (i, _) in CLIS.iter().enumerate() {
        a.found[i] = Some(match i % 3 {
            0 => Found { path: Some(PathBuf::from(format!("C:/Users/you/{}/bin/cli.exe", AWKWARD[0]))), version: Some(AWKWARD[4].into()), signed: Some(AWKWARD[1].into()) },
            1 => Found { path: Some(PathBuf::from(AWKWARD[2])), version: Some("0.0.1".into()), signed: Some(String::new()) },
            _ => Found::default(),
        });
    }
    a.ollama = Some(ollama::Info {
        installed: true,
        running: true,
        version: Some(AWKWARD[3].into()),
        models: AWKWARD.iter().map(|m| ollama::Model { name: m.to_string(), size: 5_200_000_000, params: "8.2B".into(), quant: AWKWARD[4].into() }).collect(),
        loaded: vec![(AWKWARD[0].into(), 6_100_000_000)],
    });
    a.view = view;
    a
}

/// For the whole-app sweep: the "your AIs" tab reading only a scratch folder.
pub(crate) fn app_pane() -> Box<dyn crate::pane::Pane> {
    Box::new(fixture(&scratch("ais-app"), View::Overview))
}

/// Bugs found by this sweep that are still open (each with its own test below). The four it found (the install
/// view's `dw - 10`, the Ollama card's `width - 9`, the usage view's `rest - ph` and its rule in a 0-row pane)
/// are fixed, so the sweep fails on any problem at all.
const KNOWN: &[Known] = &[];

fn loaded(k: &mut Kit, view: View, name: &str) -> Ais {
    let mut a = fixture(&scratch(name), view);
    k.render(&mut a, 150, 44); // starts the (scratch-only) usage worker
    let t0 = std::time::Instant::now();
    while (a.usage.is_none() || a.readouts.is_none()) && t0.elapsed().as_secs() < 10 {
        k.wait_wake(&mut a, 50);
    }
    a
}

#[test]
fn qa_sizes_bug_ais_install_narrow_panics() {
    let mut k = Kit::new();
    let mut a = loaded(&mut k, View::Install, "ais-bug-install");
    crate::qa_sizes::assert_clean(&mut k, &mut a, 12, 20, false);
}

#[test]
fn qa_sizes_bug_ais_ollama_card_narrow_panics() {
    let mut k = Kit::new();
    let mut a = loaded(&mut k, View::Overview, "ais-bug-ollama");
    crate::qa_sizes::assert_clean(&mut k, &mut a, 10, 60, false);
}

#[test]
fn qa_sizes_bug_ais_usage_wide_short_panics() {
    let mut k = Kit::new();
    let mut a = loaded(&mut k, View::Usage, "ais-bug-usage");
    assert!(a.usage.is_some(), "the fixture's usage never loaded");
    crate::qa_sizes::assert_clean(&mut k, &mut a, 140, 16, false);
}

#[test]
fn qa_sizes_bug_ais_usage_zero_rows() {
    let mut k = Kit::new();
    let mut a = loaded(&mut k, View::Usage, "ais-bug-usage0");
    crate::qa_sizes::assert_clean(&mut k, &mut a, 7, 0, false);
}

#[test]
fn qa_sizes_ais() {
    let mut k = Kit::new();
    let mut bad = vec![];
    for view in VIEWS {
        let root = scratch(&format!("ais-{view:?}"));
        bad.extend(sweep(&format!("ais({view:?})"), &mut k, &mut |_| Box::new(fixture(&root, view)), true, 1500));
    }
    verdict(bad, KNOWN);
}
