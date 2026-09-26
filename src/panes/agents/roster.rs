//! The roster (who a lead can hand work to) and the plan limits it routes around: Claude's 5-hour / weekly
//! percentages (what `oriel usage-sink` saved from Claude Code's status line, plus rate-limit events in the
//! workers' own streams) and Codex's primary / secondary windows (its newest rollout file). Reading happens on
//! a background thread; the pane keeps the latest numbers.

use crate::config::RosterEntry;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub const TIERS: &[&str] = &["cheap", "mid", "premium"];

/// A sensible roster from what's installed (used while the config has none).
pub fn defaults(installed: &dyn Fn(&str) -> bool) -> Vec<RosterEntry> {
    let mut v = vec![];
    let e = |name: &str, agent: &str, model: &str, tier: &str, good_at: &str, turns: u32, budget: f64| RosterEntry {
        name: name.into(),
        agent: agent.into(),
        model: model.into(),
        tier: tier.into(),
        good_at: good_at.into(),
        max_turns: turns,
        budget_usd: budget,
        enabled: true,
    };
    if installed("codex") {
        v.push(e("codex", "codex", "", "mid", "refactors, backend, tests", 40, 1.5));
    }
    if installed("kimi") {
        v.push(e("kimi", "kimi", "", "cheap", "bulk edits, docs, simple features", 40, 1.0));
    }
    if installed("claude") {
        v.push(e("claude-haiku", "claude", "haiku", "cheap", "small edits, docs, boilerplate", 25, 0.5));
        v.push(e("claude-worker", "claude", "sonnet", "premium", "hard bugs, architecture, tricky refactors", 40, 1.5));
    }
    v
}

/// One plan window: "5-hour 34%".
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub label: String,
    pub pct: f64,
    pub resets_at: Option<i64>,
}

/// Latest known plan usage per agent.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Limits {
    pub claude: Vec<Window>,
    pub codex: Vec<Window>,
}

impl Limits {
    pub fn of(&self, agent: &str) -> &[Window] {
        match agent {
            "claude" => &self.claude,
            "codex" => &self.codex,
            _ => &[],
        }
    }
    /// Highest window % for an agent (None = unknown).
    pub fn worst(&self, agent: &str) -> Option<f64> {
        self.of(agent).iter().map(|w| w.pct).fold(None, |a, p| Some(a.map_or(p, |a: f64| a.max(p))))
    }
    /// "5h 34% · wk 12%"
    pub fn text(&self, agent: &str) -> String {
        let w = self.of(agent);
        if w.is_empty() {
            return "limits unknown".into();
        }
        w.iter().map(|w| format!("{} {:.0}%", short(&w.label), w.pct)).collect::<Vec<_>>().join(" · ")
    }
}

fn short(label: &str) -> String {
    match label {
        "5-hour" => "5h".into(),
        "weekly" => "wk".into(),
        l => l.replace("-day", "d").replace("-hour", "h"),
    }
}

fn pct_of(v: &Value) -> Option<f64> {
    v.get("used_percentage").or_else(|| v.get("used_percent")).or_else(|| v.get("utilization")).and_then(|x| x.as_f64())
}

fn resets_of(v: &Value) -> Option<i64> {
    let r = v.get("resets_at").or_else(|| v.get("resetsAt"))?;
    r.as_i64().or_else(|| r.as_f64().map(|f| f as i64))
}

fn window_label(mins: i64) -> String {
    match mins {
        280..=320 => "5-hour".into(),
        10000..=10100 => "weekly".into(),
        m if m >= 1440 => format!("{}-day", (m + 720) / 1440),
        m => format!("{}-hour", (m + 30) / 60),
    }
}

/// Drop windows whose reset time has passed (their percentage is stale).
fn fresh(mut w: Vec<Window>, now: i64) -> Vec<Window> {
    w.retain(|w| w.resets_at.is_none_or(|r| r > now));
    w
}

/// What `oriel usage-sink` saved (`<data>/usage/claude.json`).
pub fn claude_sink(v: &Value, now: i64) -> Vec<Window> {
    let rl = &v["rate_limits"];
    let mut out = vec![];
    for (k, label) in [("five_hour", "5-hour"), ("seven_day", "weekly")] {
        if let Some(p) = pct_of(&rl[k]) {
            out.push(Window { label: label.into(), pct: p, resets_at: resets_of(&rl[k]) });
        }
    }
    fresh(out, now)
}

/// A `rate_limit_event`'s `rate_limit_info` from a Claude stream.
pub fn claude_event(info: &Value, now: i64) -> Vec<Window> {
    let mut out = vec![];
    for (k, label) in [("five_hour", "5-hour"), ("seven_day", "weekly")] {
        let w = &info["unifiedWindows"][k];
        if let Some(p) = pct_of(w) {
            // utilization comes as a 0-1 fraction there
            let p = if p <= 1.0 { p * 100.0 } else { p };
            out.push(Window { label: label.into(), pct: p, resets_at: resets_of(w) });
        }
    }
    if out.is_empty() {
        if let Some(p) = pct_of(info) {
            let p = if p <= 1.0 { p * 100.0 } else { p };
            let label = match info["rateLimitType"].as_str() {
                Some(t) if t.contains("seven") || t.contains("week") => "weekly",
                _ => "5-hour",
            };
            out.push(Window { label: label.into(), pct: p, resets_at: resets_of(info) });
        }
    }
    fresh(out, now)
}

/// Codex `rate_limits` from a token_count event.
pub fn codex_rate_limits(v: &Value, now: i64) -> Vec<Window> {
    let mut out = vec![];
    for k in ["primary", "secondary"] {
        let w = &v[k];
        if let Some(p) = pct_of(w) {
            let mins = w["window_minutes"].as_i64().unwrap_or(0);
            out.push(Window { label: if mins > 0 { window_label(mins) } else { k.into() }, pct: p, resets_at: resets_of(w) });
        }
    }
    fresh(out, now)
}

/// Where the readers look (tests point these into a scratch folder).
#[derive(Clone, Debug)]
pub struct LimitPaths {
    pub sink: PathBuf,
    pub codex_sessions: Option<PathBuf>,
}

impl LimitPaths {
    pub fn real() -> LimitPaths {
        let codex = std::env::var_os("CODEX_HOME").map(PathBuf::from).or_else(|| dirs::home_dir().map(|h| h.join(".codex")));
        LimitPaths { sink: crate::config::data_dir().join("usage").join("claude.json"), codex_sessions: codex.map(|c| c.join("sessions")) }
    }
}

/// Newest entries of a folder by name (the YYYY / MM / DD layout sorts by name).
fn newest(dir: &Path, n: usize) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    v.sort();
    v.into_iter().rev().take(n).collect()
}

/// The newest `rate_limits` in the newest Codex rollouts (only the last couple of days are looked at).
pub fn read_codex(sessions: &Path, now: i64) -> Vec<Window> {
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = vec![];
    for y in newest(sessions, 1) {
        for m in newest(&y, 2) {
            for d in newest(&m, 2) {
                for f in newest(&d, 40) {
                    if f.extension().is_some_and(|x| x == "jsonl") {
                        if let Ok(t) = std::fs::metadata(&f).and_then(|m| m.modified()) {
                            files.push((t, f));
                        }
                    }
                }
            }
        }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, f) in files.into_iter().take(4) {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        if let Some(line) = text.lines().rev().find(|l| l.contains("\"rate_limits\"")) {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                let w = codex_rate_limits(&v["payload"]["rate_limits"], now);
                if !w.is_empty() {
                    return w;
                }
            }
        }
    }
    vec![]
}

/// Everything, from disk (background thread).
pub fn read(p: &LimitPaths, now: i64) -> Limits {
    let claude = std::fs::read(&p.sink).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).map(|v| claude_sink(&v, now)).unwrap_or_default();
    let codex = p.codex_sessions.as_deref().map(|s| read_codex(s, now)).unwrap_or_default();
    Limits { claude, codex }
}

/// The roster as the lead's `roster` tool returns it.
pub fn roster_json(roster: &[RosterEntry], limits: &Limits, installed: &dyn Fn(&str) -> bool, busy: &dyn Fn(&str) -> usize) -> Value {
    let workers: Vec<Value> = roster
        .iter()
        .filter(|w| w.enabled)
        .map(|w| {
            let mut o = json!({
                "name": w.name,
                "agent": w.agent,
                "model": if w.model.is_empty() { "default" } else { &w.model },
                "tier": w.tier,
                "good_at": w.good_at,
                "budget_usd_per_task": w.budget_usd,
                "max_turns": w.max_turns,
                "installed": installed(&w.agent),
                "running_now": busy(&w.name),
            });
            let win: Vec<Value> = limits.of(&w.agent).iter().map(|x| json!({"window": x.label, "used_percent": (x.pct * 10.0).round() / 10.0, "resets_at": x.resets_at})).collect();
            o["plan_limits"] = if win.is_empty() { json!("unknown") } else { Value::Array(win) };
            if limits.worst(&w.agent).is_some_and(|p| p >= 90.0) {
                o["warning"] = json!("plan nearly used up: prefer another worker");
            }
            o
        })
        .collect();
    json!({ "workers": workers })
}

/// A short roster listing for the lead's instructions.
pub fn roster_brief(roster: &[RosterEntry]) -> String {
    roster
        .iter()
        .filter(|w| w.enabled)
        .map(|w| format!("- {} ({}{}, {} tier): {}", w.name, w.agent, if w.model.is_empty() { String::new() } else { format!(" · {}", w.model) }, w.tier, if w.good_at.is_empty() { "general coding" } else { &w.good_at }))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_roster_defaults_and_limits() {
        let d = defaults(&|a| a != "kimi");
        assert_eq!(d.iter().map(|w| w.name.as_str()).collect::<Vec<_>>(), vec!["codex", "claude-haiku", "claude-worker"]);
        let now = 1_000;
        let sink = serde_json::json!({"rate_limits":{"five_hour":{"used_percentage":34.2,"resets_at":2000},"seven_day":{"used_percentage":95,"resets_at":500}}});
        let c = claude_sink(&sink, now);
        assert_eq!(c.len(), 1, "a window past its reset is stale");
        assert_eq!(c[0].label, "5-hour");
        let ev = claude_event(&serde_json::json!({"status":"allowed_warning","unifiedWindows":{"five_hour":{"utilization":0.92,"resetsAt":5000}}}), now);
        assert!((ev[0].pct - 92.0).abs() < 1e-9);
        let cx = codex_rate_limits(&serde_json::json!({"primary":{"used_percent":9.0,"window_minutes":299,"resets_at":3000},"secondary":{"used_percent":40.0,"window_minutes":10080}}), now);
        assert_eq!(cx.iter().map(|w| w.label.as_str()).collect::<Vec<_>>(), vec!["5-hour", "weekly"]);
        let l = Limits { claude: ev, codex: cx };
        assert_eq!(l.text("codex"), "5h 9% · wk 40%");
        assert_eq!(l.text("kimi"), "limits unknown");
        let j = roster_json(&d, &l, &|_| true, &|n| if n == "codex" { 1 } else { 0 });
        let w = j["workers"].as_array().unwrap();
        assert_eq!(w[0]["running_now"], 1);
        assert!(w[1]["warning"].is_string(), "claude at 92% gets a warning: {}", w[1]);
        assert!(roster_brief(&d).contains("claude-worker (claude · sonnet, premium tier)"));
    }
}
