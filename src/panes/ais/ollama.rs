//! Ollama: version, installed models and what's loaded right now (its local HTTP API; no limits to show).

use serde_json::Value;
use std::time::Duration;

#[derive(Clone, Debug, Default)]
pub struct Model {
    pub name: String,
    pub size: u64,
    pub params: String,
    pub quant: String,
}

#[derive(Clone, Debug, Default)]
pub struct Info {
    pub installed: bool,
    pub running: bool,
    pub version: Option<String>,
    pub models: Vec<Model>,
    /// (name, bytes in VRAM)
    pub loaded: Vec<(String, u64)>,
}

fn get(agent: &ureq::Agent, url: &str) -> Option<Value> {
    let body = agent.get(url).call().ok()?.body_mut().read_to_string().ok()?;
    serde_json::from_str(&body).ok()
}

/// Blocking (HTTP with a short timeout): background threads only.
pub fn probe(base: &str) -> Info {
    let base = base.trim_end_matches('/');
    let installed = crate::config::which("ollama").is_some();
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_millis(1500))).build().into();
    let Some(ver) = get(&agent, &format!("{base}/api/version")) else { return Info { installed, ..Default::default() } };
    let mut info = Info { installed: true, running: true, version: ver.get("version").and_then(|v| v.as_str()).map(String::from), ..Default::default() };
    if let Some(tags) = get(&agent, &format!("{base}/api/tags")) {
        for m in tags.get("models").and_then(|m| m.as_array()).into_iter().flatten() {
            let d = m.get("details").cloned().unwrap_or_default();
            info.models.push(Model {
                name: m.get("name").and_then(|x| x.as_str()).unwrap_or("?").to_string(),
                size: m.get("size").and_then(|x| x.as_u64()).unwrap_or(0),
                params: d.get("parameter_size").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                quant: d.get("quantization_level").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            });
        }
    }
    if let Some(ps) = get(&agent, &format!("{base}/api/ps")) {
        for m in ps.get("models").and_then(|m| m.as_array()).into_iter().flatten() {
            info.loaded.push((m.get("name").and_then(|x| x.as_str()).unwrap_or("?").to_string(), m.get("size_vram").and_then(|x| x.as_u64()).unwrap_or(0)));
        }
    }
    info
}
