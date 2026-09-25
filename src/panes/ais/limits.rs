//! Plan limits: Claude's real 5-hour / weekly percentages (written by `oriel usage-sink`, Claude Code's
//! statusLine command) and Codex's primary / secondary windows (from its rollout files, see usage.rs).

use super::util::{Paths, now, write_atomic};
use serde_json::{Value, json};
use std::io::Read;
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub label: String,
    pub pct: f64,
    pub resets_at: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Limits {
    pub windows: Vec<Window>,
    /// when the numbers were recorded (epoch s)
    pub updated: i64,
    /// plan name if known ("free", "plus" …)
    pub plan: Option<String>,
    /// Claude: this session's cost as Claude reports it
    pub session_cost: Option<f64>,
}

impl Limits {
    pub fn first(&self) -> Option<&Window> {
        self.windows.first()
    }
}

fn pct_of(v: &Value) -> Option<f64> {
    v.get("used_percentage").or_else(|| v.get("used_percent")).or_else(|| v.get("utilization")).and_then(|x| x.as_f64())
}

fn resets_of(v: &Value) -> Option<i64> {
    let r = v.get("resets_at").or_else(|| v.get("resetsAt"))?;
    r.as_i64().or_else(|| r.as_f64().map(|f| f as i64)).or_else(|| r.as_str().and_then(super::util::parse_iso))
}

/// A window length in minutes as a short label.
pub fn window_label(mins: i64) -> String {
    match mins {
        280..=320 => "5-hour".into(),
        10000..=10100 => "weekly".into(),
        43000..=43400 => "30-day".into(),
        m if m % 1440 == 0 || m > 2880 => format!("{}-day", (m + 720) / 1440),
        m => format!("{}-hour", (m + 30) / 60),
    }
}

/// Parse what `usage-sink` saved.
pub fn claude_from(v: &Value) -> Option<Limits> {
    let rl = v.get("rate_limits")?;
    let mut windows = vec![];
    for (k, label) in [("five_hour", "5-hour"), ("seven_day", "weekly"), ("spend_limit", "spend")] {
        if let Some(w) = rl.get(k) {
            if let Some(p) = pct_of(w) {
                windows.push(Window { label: label.into(), pct: p, resets_at: resets_of(w) });
            }
        }
    }
    Some(Limits {
        windows,
        updated: v.get("received_at").and_then(|x| x.as_i64()).unwrap_or(0),
        plan: None,
        session_cost: v.get("cost").and_then(|c| c.get("total_cost_usd")).and_then(|x| x.as_f64()),
    })
}

pub fn read_claude(paths: &Paths) -> Option<Limits> {
    let b = std::fs::read(paths.sink_file()).ok()?;
    let v: Value = serde_json::from_slice(&b).ok()?;
    claude_from(&v)
}

/// Codex's `rate_limits` object from a token_count event.
pub fn codex_from(t: i64, v: &Value) -> Limits {
    let mut windows = vec![];
    for k in ["primary", "secondary"] {
        if let Some(w) = v.get(k).filter(|w| w.is_object()) {
            if let Some(p) = pct_of(w) {
                let mins = w.get("window_minutes").and_then(|x| x.as_i64()).unwrap_or(0);
                windows.push(Window { label: if mins > 0 { window_label(mins) } else { k.into() }, pct: p, resets_at: resets_of(w) });
            }
        }
    }
    Limits { windows, updated: t, plan: v.get("plan_type").and_then(|p| p.as_str()).map(String::from), session_cost: None }
}

/// A window whose reset time has passed: its percentage is stale (it has reset since).
pub fn is_reset(w: &Window, now: i64) -> bool {
    w.resets_at.is_some_and(|r| r <= now)
}

// ------------------------------------------------------------------ oriel usage-sink

/// Keep only the numbers from Claude's statusLine JSON (no paths or prompt text), stamped with when we got it.
pub fn filter_status(v: &Value, received_at: i64) -> Value {
    let mut o = json!({ "received_at": received_at });
    for k in ["rate_limits", "context_window", "prompt_cache", "effort"] {
        if let Some(x) = v.get(k) {
            o[k] = x.clone();
        }
    }
    if let Some(c) = v.get("cost").and_then(|c| c.get("total_cost_usd")) {
        o["cost"] = json!({ "total_cost_usd": c });
    }
    if let Some(m) = v.get("model") {
        let mut mm = json!({});
        for k in ["id", "display_name"] {
            if let Some(x) = m.get(k) {
                mm[k] = x.clone();
            }
        }
        o["model"] = mm;
    }
    o
}

/// "5h 34% · wk 12% · $0.42"
pub fn status_line(v: &Value) -> String {
    let mut parts = vec![];
    if let Some(rl) = v.get("rate_limits") {
        for (k, l) in [("five_hour", "5h"), ("seven_day", "wk")] {
            if let Some(p) = rl.get(k).and_then(pct_of) {
                parts.push(format!("{l} {p:.0}%"));
            }
        }
    }
    if let Some(c) = v.get("cost").and_then(|c| c.get("total_cost_usd")).and_then(|x| x.as_f64()) {
        parts.push(format!("${c:.2}"));
    }
    if parts.is_empty() {
        let m = v.get("model").and_then(|m| m.get("display_name")).and_then(|x| x.as_str()).unwrap_or("claude");
        parts.push(m.to_string());
    }
    parts.join(" · ")
}

/// The sink's work, minus stdin/stdout: save the filtered JSON, return the line to print.
pub fn sink(input: &str, file: &Path, at: i64) -> String {
    let Ok(v) = serde_json::from_str::<Value>(input) else { return "oriel: no status".into() };
    let keep = filter_status(&v, at);
    if let Ok(b) = serde_json::to_vec_pretty(&keep) {
        let _ = write_atomic(file, &b);
    }
    status_line(&v)
}

/// `oriel usage-sink [--then <command…>]`. Always exits 0: a status line must never break Claude Code.
pub fn cli(args: &[String]) -> i32 {
    let mut input = String::new();
    let _ = std::io::stdin().take(4 << 20).read_to_string(&mut input);
    let file = crate::config::data_dir().join("usage").join("claude.json");
    let line = sink(&input, &file, now());
    // chaining: hand the same JSON to the user's own statusLine command and print its output instead
    if let Some(i) = args.iter().position(|a| a == "--then") {
        let cmd = args[i + 1..].join(" ");
        if !cmd.trim().is_empty() {
            if let Some(out) = run_chained(&cmd, &input) {
                print!("{out}");
                return 0;
            }
        }
    }
    println!("{line}");
    0
}

fn run_chained(cmd: &str, input: &str) -> Option<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    // Claude Code runs statusLine commands through Git Bash on Windows, so use the same shell when it's there
    // (not System32\bash.exe: that's the WSL launcher)
    // (and not WindowsApps\bash.exe, the same launcher's alias) — find Git for Windows' own bash
    let bash = if cfg!(windows) {
        let from_git = crate::config::which("git").and_then(|g| Some(g.parent()?.parent()?.join("bin").join("bash.exe")));
        [Some(std::path::PathBuf::from(r"C:\Program Files\Git\bin\bash.exe")), from_git]
            .into_iter()
            .flatten()
            .find(|p| p.is_file())
            .or_else(|| {
                crate::config::which("bash").filter(|p| {
                    let l = p.to_string_lossy().to_lowercase();
                    !l.contains("system32") && !l.contains("windowsapps")
                })
            })
    } else {
        None
    };
    let mut c = if let Some(b) = bash {
        let mut c = Command::new(b);
        c.args(["-c", cmd]);
        c
    } else if cfg!(windows) {
        let mut c = Command::new("cmd.exe");
        c.args(["/C", cmd]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    let mut child = c.spawn().ok()?;
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(input.as_bytes());
    }
    let out = child.wait_with_output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ais_sink_writes_numbers_only() {
        let dir = std::env::temp_dir().join(format!("oriel-ais-sink-{}", std::process::id()));
        let file = dir.join("usage").join("claude.json");
        let input = r#"{"session_id":"s","transcript_path":"C:/secret/path.jsonl","cwd":"C:/x","model":{"id":"claude-opus-5-5","display_name":"Opus 5.5"},
            "cost":{"total_cost_usd":0.4213,"total_lines_added":3},
            "rate_limits":{"five_hour":{"used_percentage":34.2,"resets_at":2000000000},"seven_day":{"used_percentage":12,"resets_at":2000300000}}}"#;
        let line = sink(input, &file, 1_900_000_000);
        assert_eq!(line, "5h 34% · wk 12% · $0.42");
        let saved = std::fs::read_to_string(&file).unwrap();
        assert!(!saved.contains("secret") && !saved.contains("transcript_path"), "{saved}");
        let v: Value = serde_json::from_str(&saved).unwrap();
        let l = claude_from(&v).unwrap();
        assert_eq!(l.windows.len(), 2);
        assert_eq!(l.windows[0], Window { label: "5-hour".into(), pct: 34.2, resets_at: Some(2_000_000_000) });
        assert_eq!(l.updated, 1_900_000_000);
        assert_eq!(sink("not json", &file, 0), "oriel: no status");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ais_codex_limits_parse() {
        let v: Value = serde_json::from_str(r#"{"primary":{"used_percent":9.0,"window_minutes":43200,"resets_at":1792798322},"secondary":null,"plan_type":"free"}"#).unwrap();
        let l = codex_from(5, &v);
        assert_eq!(l.windows.len(), 1);
        assert_eq!(l.windows[0].label, "30-day");
        assert_eq!(l.plan.as_deref(), Some("free"));
        assert_eq!(window_label(299), "5-hour");
        assert_eq!(window_label(10079), "weekly");
    }
}
