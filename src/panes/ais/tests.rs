//! Headless tests for "your AIs". Everything runs against fixture files in a temp dir (util::Paths::under):
//! the real ~/.claude and ~/.codex are never read or written, nothing is installed, nobody signs in.
//!
//!   cargo test ais                                   # all of these
//!   cargo test ais_snapshots -- --nocapture          # + target/snap/ais-*.html (tools/snap.ps1 → PNG)
//!   cargo test ais_real_usage -- --ignored --nocapture   # read THIS machine's logs read-only, print totals

use super::catalog::{CLIS, Found};
use super::util::{Paths, civil_from_days, now};
use super::*;
use crate::pane::Pane;
use crate::testkit::Kit;
use crossterm::event::KeyCode;
use std::path::{Path, PathBuf};

fn iso(t: i64) -> String {
    let (y, m, d) = civil_from_days(t.div_euclid(86400));
    let s = t.rem_euclid(86400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z", s / 3600, (s / 60) % 60, s % 60)
}

fn root(name: &str) -> PathBuf {
    let r = std::env::temp_dir().join(format!("oriel-ais-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&r);
    std::fs::create_dir_all(&r).unwrap();
    r
}

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

fn claude_line(t: i64, id: &str, req: &str, model: &str, cwd: &str, i: u64, o: u64, cw: u64, cr: u64) -> String {
    format!(
        r#"{{"parentUuid":"x","type":"assistant","requestId":"{req}","timestamp":"{}","cwd":{},"message":{{"id":"{id}","model":"{model}","role":"assistant","content":[{{"type":"text","text":"PRIVATE PROMPT TEXT"}}],"usage":{{"input_tokens":{i},"output_tokens":{o},"cache_creation_input_tokens":{cw},"cache_read_input_tokens":{cr},"cache_creation":{{"ephemeral_5m_input_tokens":{cw},"ephemeral_1h_input_tokens":0}},"service_tier":"standard","inference_geo":"not_available"}}}}}}"#,
        iso(t),
        serde_json::to_string(cwd).unwrap()
    )
}

/// A believable home dir: two Claude projects over two weeks, a subagent file, duplicated streaming lines, a
/// Codex rollout with limits, settings.json with hooks, MCP servers, a CLAUDE.md and a usage-sink file.
fn fixture(r: &Path) -> Paths {
    let p = Paths::under(r);
    let n = now();
    let mut a = String::new();
    let mut b = String::new();
    for d in 0..14i64 {
        let t = n - d * 86400 - 1800;
        let k = (14 - d) as u64;
        a.push_str(&claude_line(t, &format!("m{d}"), &format!("r{d}"), "claude-opus-5-5", "C:\\Code\\devtools\\oriel", 40 * k, 2_000 * k, 30_000 * k, 400_000 * k));
        a.push('\n');
        // the same response written twice (streaming): must count once
        a.push_str(&claude_line(t, &format!("m{d}"), &format!("r{d}"), "claude-opus-5-5", "C:\\Code\\devtools\\oriel", 40 * k, 2_000 * k, 30_000 * k, 400_000 * k));
        a.push('\n');
        a.push_str("{\"type\":\"user\",\"message\":{\"content\":\"PRIVATE PROMPT TEXT\"}}\n");
        if d % 3 == 0 {
            b.push_str(&claude_line(t - 7200, &format!("s{d}"), &format!("q{d}"), "claude-sonnet-5", "C:\\Code\\games\\rainy-hollow", 10, 900 * k, 8_000, 90_000 * k));
            b.push('\n');
        }
    }
    write(&p.claude.join("projects").join("C--Code-devtools-oriel").join("aaaa1111-2222.jsonl"), &a);
    write(&p.claude.join("projects").join("C--Code-games-rainy-hollow").join("bbbb3333-4444.jsonl"), &b);
    let sub = claude_line(n - 600, "h1", "hr1", "claude-haiku-4-5-20251001", "C:\\Code\\devtools\\oriel", 5, 300, 2_000, 20_000);
    write(&p.claude.join("projects").join("C--Code-devtools-oriel").join("aaaa1111-2222").join("subagents").join("agent-1.jsonl"), &format!("{sub}\n"));
    // Codex
    let mut x = String::new();
    x.push_str(&format!(r#"{{"timestamp":"{}","type":"session_meta","payload":{{"id":"cx-session-1","cwd":"C:\\Code\\web\\yarna","cli_version":"0.147.0"}}}}"#, iso(n - 5000)));
    x.push('\n');
    x.push_str(&format!(r#"{{"timestamp":"{}","type":"turn_context","payload":{{"cwd":"C:\\Code\\web\\yarna","model":"gpt-5.6-terra"}}}}"#, iso(n - 4900)));
    x.push('\n');
    for (k, tot) in [(1u64, 16_345u64), (2, 40_000), (2, 40_000)] {
        x.push_str(&format!(
            r#"{{"timestamp":"{}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":1,"cached_input_tokens":0,"output_tokens":0,"total_tokens":{tot}}},"last_token_usage":{{"input_tokens":{},"cached_input_tokens":{},"cache_write_input_tokens":0,"output_tokens":{},"reasoning_output_tokens":0,"total_tokens":1}},"model_context_window":258400}},"rate_limits":{{"limit_id":"codex","primary":{{"used_percent":{},"window_minutes":299,"resets_at":{}}},"secondary":{{"used_percent":21.0,"window_minutes":10079,"resets_at":{}}},"plan_type":"plus"}}}}}}"#,
            iso(n - 4800 + k as i64 * 60),
            16_314 * k,
            15_104 * k,
            31 * k,
            9 * k,
            n + 2 * 3600 + 14 * 60,
            n + 4 * 86400 + 3 * 3600
        ));
        x.push('\n');
    }
    write(&p.codex.join("sessions").join("2026").join("09").join("24").join("rollout-2026-09-24T10-00-00-cx.jsonl"), &x);
    // settings with hooks, MCP servers, CLAUDE.md, codex config
    write(&p.claude_settings(), "{\n  \"model\": \"opus\",\n  \"hooks\": {\n    \"Stop\": [{ \"hooks\": [{ \"type\": \"command\", \"command\": \"notify.ps1\" }] }]\n  },\n  \"effortLevel\": \"high\"\n}\n");
    write(&p.claude_json, r#"{"numStartups":3,"mcpServers":{"blender":{},"chrome-devtools":{},"websim":{}},"projects":{"C:/Code/x":{"mcpServers":{"edot":{}}}}}"#);
    write(&p.claude.join("CLAUDE.md"), &"line\n".repeat(161));
    write(&p.codex_config(), "model = \"gpt-5.6-terra\"\n\n[features]\nhooks = true\n");
    write(
        &p.sink_file(),
        &format!(r#"{{"received_at":{},"rate_limits":{{"five_hour":{{"used_percentage":34,"resets_at":{}}},"seven_day":{{"used_percentage":12,"resets_at":{}}}}},"cost":{{"total_cost_usd":0.42}}}}"#, n - 180, n + 2 * 3600 + 14 * 60, n + 4 * 86400 + 3 * 3600),
    );
    p
}

/// Detection results as they'd come back on a machine like the dev box (no commands run).
fn fake_found(a: &mut Ais) {
    for (i, c) in CLIS.iter().enumerate() {
        a.found[i] = Some(match c.id {
            "claude" => Found { path: Some("C:/Users/you/.local/bin/claude.exe".into()), version: Some("2.1.282".into()), signed: Some("OAuth".into()) },
            "codex" => Found { path: Some("C:/Users/you/AppData/Roaming/npm/codex.cmd".into()), version: Some("0.147.0".into()), signed: Some("ChatGPT".into()) },
            "kimi" => Found { path: Some("C:/Users/you/.kimi-code/bin/kimi.exe".into()), version: Some("0.29.1".into()), signed: None },
            "aider" => Found { path: Some("C:/Users/you/.local/bin/aider.exe".into()), version: Some("0.86.2".into()), signed: Some(String::new()) },
            _ => Found::default(),
        });
    }
    a.ollama = Some(ollama::Info {
        installed: true,
        running: true,
        version: Some("0.34.0".into()),
        models: vec![
            ollama::Model { name: "qwen3:8b".into(), size: 5_200_000_000, params: "8.2B".into(), quant: "Q4_K_M".into() },
            ollama::Model { name: "gemma3:12b".into(), size: 8_100_000_000, params: "12.2B".into(), quant: "Q4_K_M".into() },
            ollama::Model { name: "nomic-embed-text:latest".into(), size: 274_000_000, params: "137M".into(), quant: "F16".into() },
        ],
        loaded: vec![("qwen3:8b".into(), 6_100_000_000)],
    });
}

/// Render once (starts the workers), then wait until the usage summary and readouts have arrived.
fn ready(k: &mut Kit, a: &mut Ais) {
    k.render(a, 150, 44);
    let t0 = std::time::Instant::now();
    while (a.usage.is_none() || a.readouts.is_none()) && t0.elapsed().as_secs() < 20 {
        k.wait_wake(a, 50);
    }
    assert!(a.usage.is_some(), "usage never arrived");
}

#[test]
fn ais_usage_totals_dedupe_and_price() {
    let r = root("totals");
    let p = fixture(&r);
    let mut cache = usage::Cache::load(&p.cache_file());
    let st = usage::refresh(&mut cache, &p);
    assert_eq!(st.files, 4);
    let sums = usage::summarize(&cache, now(), util::local_offset());
    let c = sums.iter().find(|s| s.src == usage::Src::Claude).unwrap();
    // 14 opus responses (each written twice) + 5 sonnet + 1 haiku subagent
    assert_eq!(c.all.n, 14 + 5 + 1);
    // opus day k=14 (today): 40*14 in, 2000*14 out, 30000*14 5m-write, 400000*14 read
    let k = 14.0;
    let opus_today = (40.0 * k * 4.0 + 2000.0 * k * 20.0 + 30000.0 * k * 5.0 + 400000.0 * k * 0.2) / 1e6;
    assert!(c.today.cost >= opus_today - 1e-9, "{} < {}", c.today.cost, opus_today);
    assert_eq!(c.projects.len(), 2);
    assert_eq!(c.projects[0].0, "oriel");
    // the subagent file belongs to its parent session
    assert!(c.sessions.iter().any(|s| s.id == "aaaa1111-2222" && s.tot.n == 15));
    assert!(!c.blocks.is_empty() && c.blocks[0].active);
    let x = sums.iter().find(|s| s.src == usage::Src::Codex).unwrap();
    assert_eq!(x.all.n, 2, "duplicate token_count events count once");
    assert_eq!(x.all.cr, 15_104 * 3);
    assert_eq!(x.projects[0].0, "yarna");
    let lim = usage::codex_limits(&cache).map(|(t, v)| limits::codex_from(t, &v)).unwrap();
    assert_eq!(lim.windows[0].label, "5-hour");
    assert_eq!(lim.windows[0].pct, 18.0);
    assert_eq!(lim.windows[1].label, "weekly");
    // the cache round-trips and a second refresh reads nothing new
    cache.save(&p.cache_file());
    let saved = std::fs::read_to_string(p.cache_file()).unwrap();
    assert!(!saved.contains("PRIVATE"), "prompt text must never reach the cache");
    let mut again = usage::Cache::load(&p.cache_file());
    let st2 = usage::refresh(&mut again, &p);
    assert_eq!((st2.changed, st2.bytes), (0, 0));
    // appending one line reads just that line
    let f = p.claude.join("projects").join("C--Code-devtools-oriel").join("aaaa1111-2222.jsonl");
    let line = claude_line(now(), "new", "newreq", "claude-opus-5-5", "C:\\Code\\devtools\\oriel", 1, 1, 0, 0) + "\n";
    std::fs::OpenOptions::new().append(true).open(&f).and_then(|mut h| std::io::Write::write_all(&mut h, line.as_bytes())).unwrap();
    let st3 = usage::refresh(&mut again, &p);
    assert_eq!((st3.changed, st3.bytes), (1, line.len() as u64));
    let c3 = usage::summarize(&again, now(), util::local_offset()).into_iter().find(|s| s.src == usage::Src::Claude).unwrap();
    assert_eq!(c3.all.n, 21);
    let _ = std::fs::remove_dir_all(&r);
}

#[test]
fn ais_install_asks_then_opens_a_terminal() {
    let r = root("install");
    let mut k = Kit::new();
    let mut a = Ais::with(fixture(&r), false);
    fake_found(&mut a);
    k.key(&mut a, KeyCode::Char('2'));
    // OpenCode (not installed): enter asks first, esc cancels
    let gi = CLIS.iter().position(|c| c.id == "opencode").unwrap();
    for _ in 0..gi {
        k.key(&mut a, KeyCode::Down);
    }
    k.key(&mut a, KeyCode::Enter);
    assert!(matches!(a.ask, Some(Ask::Install { .. })));
    let txt = k.render(&mut a, 150, 44);
    assert!(txt.contains("install OpenCode?"), "{txt}");
    k.key(&mut a, KeyCode::Esc);
    assert!(a.ask.is_none() && a.launched.is_empty());
    k.key(&mut a, KeyCode::Enter);
    k.key(&mut a, KeyCode::Char('y'));
    assert_eq!(a.launched.len(), 1);
    assert!(a.launched[0].contains("opencode"), "{:?}", a.launched);
    // sign in on something not installed says so; on Kimi it runs `kimi login` by full path (not on PATH here)
    k.key(&mut a, KeyCode::Char('l'));
    assert_eq!(a.launched.len(), 1);
    k.key(&mut a, KeyCode::Char('g'));
    k.key(&mut a, KeyCode::Down);
    k.key(&mut a, KeyCode::Down);
    k.key(&mut a, KeyCode::Char('l'));
    assert!(a.launched[1].contains("kimi") && a.launched[1].ends_with(" login"), "{:?}", a.launched);
    k.key(&mut a, KeyCode::Char('o'));
    assert_eq!(a.launched[2], "open: https://code.kimi.com");
    let _ = std::fs::remove_dir_all(&r);
}

#[test]
fn ais_token_saver_edits_only_its_keys_with_backup() {
    let r = root("saver");
    let p = fixture(&r);
    let before = std::fs::read_to_string(p.claude_settings()).unwrap();
    let mut k = Kit::new();
    let mut a = Ais::with(p.clone(), false);
    ready(&mut k, &mut a);
    k.key(&mut a, KeyCode::Char('4'));
    k.key(&mut a, KeyCode::Up); // Frugal
    k.key(&mut a, KeyCode::Enter);
    let t0 = std::time::Instant::now();
    while a.ask.is_none() && t0.elapsed().as_secs() < 5 {
        k.wait_wake(&mut a, 30);
    }
    let Some(Ask::Edit(plan)) = &a.ask else { panic!("no diff shown: {:?}", k.notices()) };
    assert!(plan.diff.contains(&('+', "\"model\": \"sonnet\"".into())));
    assert_eq!(std::fs::read_to_string(p.claude_settings()).unwrap(), before, "nothing written before y");
    std::fs::create_dir_all("target/snap").unwrap();
    k.render_html(&mut a, 150, 44, "target/snap/ais-confirm.html");
    k.key(&mut a, KeyCode::Char('y'));
    let t0 = std::time::Instant::now();
    while a.busy && t0.elapsed().as_secs() < 5 {
        k.wait_wake(&mut a, 30);
    }
    let after = std::fs::read_to_string(p.claude_settings()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&after).unwrap();
    assert_eq!(v["model"], "sonnet");
    assert_eq!(v["env"]["CLAUDE_CODE_SUBAGENT_MODEL"], "haiku");
    assert!(after.contains("\"hooks\": {\n    \"Stop\": [{ \"hooks\": [{ \"type\": \"command\", \"command\": \"notify.ps1\" }] }]\n  },"), "{after}");
    let backups: Vec<_> = std::fs::read_dir(&p.claude).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("settings.json.oriel-backup-")).collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(std::fs::read_to_string(backups[0].path()).unwrap(), before);
    // Codex profile
    k.key(&mut a, KeyCode::Char('x'));
    let t0 = std::time::Instant::now();
    while a.ask.is_none() && t0.elapsed().as_secs() < 5 {
        k.wait_wake(&mut a, 30);
    }
    k.key(&mut a, KeyCode::Char('y'));
    let t0 = std::time::Instant::now();
    while a.busy && t0.elapsed().as_secs() < 5 {
        k.wait_wake(&mut a, 30);
    }
    let cfg = std::fs::read_to_string(p.codex_config()).unwrap();
    assert!(cfg.starts_with("model = \"gpt-5.6-terra\"\n\n[features]\nhooks = true\n") && cfg.contains("[profiles.oriel-frugal]"), "{cfg}");
    let _ = std::fs::remove_dir_all(&r);
}

#[test]
fn ais_connect_limits_never_replaces_a_status_line() {
    let r = root("connect");
    let p = fixture(&r);
    let mut k = Kit::new();
    let mut a = Ais::with(p.clone(), false);
    ready(&mut k, &mut a);
    k.key(&mut a, KeyCode::Char('c'));
    let t0 = std::time::Instant::now();
    while a.ask.is_none() && t0.elapsed().as_secs() < 5 {
        k.wait_wake(&mut a, 30);
    }
    let Some(Ask::Edit(plan)) = &a.ask else { panic!("expected a diff") };
    assert!(plan.diff.iter().any(|d| d.0 == '+' && d.1.contains("usage-sink")));
    k.key(&mut a, KeyCode::Esc);
    // with a statusLine already there: a note explaining how to chain, and no edit offered
    let mine = "{\n  \"statusLine\": { \"type\": \"command\", \"command\": \"bash ~/my-status.sh\" }\n}\n";
    write(&p.claude_settings(), mine);
    k.key(&mut a, KeyCode::Char('c'));
    let t0 = std::time::Instant::now();
    while a.note.is_none() && t0.elapsed().as_secs() < 5 {
        k.wait_wake(&mut a, 30);
    }
    assert!(a.ask.is_none());
    let n = a.note.as_ref().expect("a note about chaining");
    assert!(n.lines.iter().any(|l| l.contains("usage-sink --then 'bash ~/my-status.sh'")), "{:?}", n.lines);
    std::fs::create_dir_all("target/snap").unwrap();
    k.render_html(&mut a, 150, 44, "target/snap/ais-chain.html");
    assert_eq!(std::fs::read_to_string(p.claude_settings()).unwrap(), mine);
    let _ = std::fs::remove_dir_all(&r);
}

#[test]
fn ais_snapshots() {
    let r = root("snap");
    let mut k = Kit::new();
    let mut a = Ais::with(fixture(&r), false);
    fake_found(&mut a);
    ready(&mut k, &mut a);
    std::fs::create_dir_all("target/snap").unwrap();
    let over = k.render_html(&mut a, 150, 44, "target/snap/ais-overview.html");
    println!("{over}");
    assert!(over.contains("Claude Code") && over.contains("5-hour") && over.contains("34%") && over.contains("resets in 2h"), "{over}");
    assert!(over.contains("Ollama") && over.contains("qwen3:8b"));
    assert!(over.contains("Aider"), "installed CLIs get a card");
    assert_eq!(a.badge().as_deref(), Some("5h 34%"));
    k.key(&mut a, KeyCode::Char('2'));
    let inst = k.render_html(&mut a, 150, 44, "target/snap/ais-install.html");
    println!("{inst}");
    assert!(inst.contains("Factory Droid") && inst.contains("Goose"));
    k.key(&mut a, KeyCode::Char('3'));
    let us = k.render_html(&mut a, 150, 44, "target/snap/ais-usage.html");
    println!("{us}");
    assert!(us.contains("today") && us.contains("top projects") && us.contains("5-hour blocks") && us.contains("oriel"), "{us}");
    k.key(&mut a, KeyCode::Right);
    k.render_html(&mut a, 150, 44, "target/snap/ais-usage-codex.html");
    k.key(&mut a, KeyCode::Char('4'));
    let t0 = std::time::Instant::now();
    while a.readouts.is_none() && t0.elapsed().as_secs() < 5 {
        k.wait_wake(&mut a, 30);
    }
    let sv = k.render_html(&mut a, 150, 44, "target/snap/ais-saver.html");
    println!("{sv}");
    assert!(sv.contains("Frugal") && sv.contains("161 lines") && sv.contains("blender"), "{sv}");
    println!("{}", k.render_side(&mut a, 30, 6));
    // narrow terminals don't panic
    for (w, h) in [(60, 20), (90, 30), (20, 6)] {
        for v in ['1', '2', '3', '4'] {
            k.key(&mut a, KeyCode::Char(v));
            k.render(&mut a, w, h);
        }
    }
    let _ = std::fs::remove_dir_all(&r);
}

/// Reads this machine's real logs READ-ONLY (the cache goes to a temp dir) and prints only aggregates.
#[test]
#[ignore]
fn ais_real_usage() {
    let tmp = root("real");
    let mut p = Paths::real(&crate::config::Config::default());
    p.data = tmp.clone();
    let mut cache = usage::Cache::load(&p.cache_file());
    let st = usage::refresh(&mut cache, &p);
    println!("cold: {} files, {:.1} MB read, {} ms", st.files, st.bytes as f64 / 1e6, st.ms);
    let t0 = std::time::Instant::now();
    cache.save(&p.cache_file());
    println!("cache save: {} ms, {:.1} KB", t0.elapsed().as_millis(), std::fs::metadata(p.cache_file()).map(|m| m.len()).unwrap_or(0) as f64 / 1e3);
    let t0 = std::time::Instant::now();
    let mut warm = usage::Cache::load(&p.cache_file());
    let load_ms = t0.elapsed().as_millis();
    let st2 = usage::refresh(&mut warm, &p);
    println!("warm: load {load_ms} ms + refresh {} ms ({} changed files, {:.1} KB new)", st2.ms, st2.changed, st2.bytes as f64 / 1e3);
    let t0 = std::time::Instant::now();
    let sums = usage::summarize(&warm, now(), util::local_offset());
    println!("summarize: {} ms", t0.elapsed().as_millis());
    for s in &sums {
        println!(
            "{:<12} today {:>8} tok ${:>8.2} (cache hit {:.0}%) · 7d {:>8} tok ${:>9.2} · all {:>8} tok ${:>10.2} · {} req · {} files · {} projects",
            s.src.label(),
            util::tok(s.today.tokens()),
            s.today.cost,
            s.today.cache_hit().unwrap_or(0.0),
            util::tok(s.week.tokens()),
            s.week.cost,
            util::tok(s.all.tokens()),
            s.all.cost,
            s.all.n,
            s.files,
            s.projects.len()
        );
        let models: Vec<String> = s.models.iter().map(|(m, t)| format!("{m} {} ${:.2}", util::tok(t.tokens()), t.cost)).collect();
        println!("             models: {}", models.join(" · "));
    }
    if let Some((t, v)) = usage::codex_limits(&warm) {
        let l = limits::codex_from(t, &v);
        for w in l.windows {
            println!("codex {} {:.0}% (resets in {})", w.label, w.pct, w.resets_at.map(|r| util::dur(r - now())).unwrap_or_default());
        }
    }
    // detection (runs `--version` and `codex login status` — read-only)
    let t0 = std::time::Instant::now();
    let found: Vec<(&str, Found)> = std::thread::scope(|s| {
        let hs: Vec<_> = CLIS.iter().map(|c| (c.id, s.spawn(|| catalog::detect(c, &p)))).collect();
        hs.into_iter().map(|(id, h)| (id, h.join().unwrap())).collect()
    });
    println!("detection: {} ms", t0.elapsed().as_millis());
    for (id, f) in found.iter().filter(|f| f.1.installed()) {
        println!("  {id:<9} {:<10} signed in: {:?}", f.version.clone().unwrap_or_default(), f.signed);
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
