//! Every AI the chat can talk to, streaming. Each request runs on its own thread and reports through `send`.
//!
//!   wren       Wren's web server (/api/chat, SSE): research, memory, fast/balanced/smart modes
//!   claude     Claude Code CLI (`claude -p --output-format stream-json`), resumes its session per chat
//!   codex      Codex CLI (`codex exec --json`), resumes its thread per chat
//!   ollama     any local Ollama model
//!   openai     any OpenAI-compatible endpoint (OpenAI, OpenRouter, LM Studio, llama.cpp...)
//!   anthropic  the Anthropic API

use crate::config::{AiConfig, which};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const PROVIDERS: &[(&str, &str, &str)] = &[
    // id, label, what
    ("wren", "Wren", "the from-scratch model on Leif's GPU (research, memory, modes)"),
    ("claude", "Claude Code", "Claude Code CLI, works in the chat's folder"),
    ("codex", "Codex", "OpenAI Codex CLI, works in the chat's folder"),
    ("ollama", "Ollama", "a local model pulled into Ollama"),
    ("openai", "OpenAI-compatible", "OpenAI, OpenRouter, LM Studio, llama.cpp…"),
    ("anthropic", "Anthropic API", "Claude over the API (needs a key)"),
];

pub fn label(id: &str) -> &'static str {
    PROVIDERS.iter().find(|p| p.0 == id).map(|p| p.1).unwrap_or("Wren")
}

pub enum Ev {
    Token(String),
    Status(String),
    Step(String, String, Option<String>),
    /// Remember a value in the chat's per-provider state (CLI session ids).
    State(String, String),
    Done { note: Option<String>, sources: Vec<(String, String)>, memory: Option<Vec<String>> },
    Error(String),
}

pub struct Request {
    pub provider: String,
    pub model: Option<String>,
    pub messages: Vec<(String, String)>,
    pub cwd: std::path::PathBuf,
    pub perms: String,
    pub mode: String,
    pub memory: Vec<String>,
    pub state: serde_json::Map<String, Value>,
    pub cfg: AiConfig,
}

pub fn start(req: Request, stop: Arc<AtomicBool>, send: impl Fn(Ev) + Send + 'static) {
    std::thread::spawn(move || {
        let r = match req.provider.as_str() {
            "claude" => claude(&req, &stop, &send),
            "codex" => codex(&req, &stop, &send),
            "ollama" => ollama(&req, &stop, &send),
            "openai" => openai(&req, &stop, &send),
            "anthropic" => anthropic(&req, &stop, &send),
            _ => wren(&req, &stop, &send),
        };
        if let Err(e) = r {
            send(Ev::Error(e));
        }
    });
}

fn agent(read_timeout: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(4)))
        .timeout_recv_body(Some(Duration::from_secs(read_timeout)))
        .http_status_as_error(false)
        .build()
        .into()
}

/// POST json and hand back a line reader on the streamed body.
fn post_stream(url: &str, headers: &[(&str, String)], body: Value) -> Result<Box<dyn BufRead + Send>, String> {
    let mut rq = agent(600).post(url).header("Content-Type", "application/json");
    for (k, v) in headers {
        rq = rq.header(*k, v.as_str());
    }
    let resp = rq.send(body.to_string()).map_err(|e| format!("couldn't reach {}: {e}", host(url)))?;
    let status = resp.status().as_u16();
    let reader = resp.into_body().into_reader();
    if status >= 400 {
        let mut s = String::new();
        let _ = BufReader::new(reader).take(2000).read_to_string(&mut s);
        let msg = serde_json::from_str::<Value>(&s)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().or(v["error"].as_str()).map(String::from))
            .unwrap_or(s);
        return Err(format!("{} said {status}: {}", host(url), msg.trim()));
    }
    Ok(Box::new(BufReader::new(reader)))
}

fn host(url: &str) -> String {
    url.split("//").nth(1).unwrap_or(url).split('/').next().unwrap_or(url).to_string()
}

/// Server-sent events: calls f(event_name, data) per event; stops early when `stop` is set.
fn sse(mut r: Box<dyn BufRead + Send>, stop: &AtomicBool, mut f: impl FnMut(&str, &str) -> Result<bool, String>) -> Result<(), String> {
    let (mut event, mut data) = (String::new(), String::new());
    let mut line = String::new();
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        line.clear();
        let n = r.read_line(&mut line).map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(());
        }
        let l = line.trim_end_matches(['\r', '\n']);
        if l.is_empty() {
            if !data.is_empty() && !f(&event, &data)? {
                return Ok(());
            }
            event.clear();
            data.clear();
        } else if let Some(v) = l.strip_prefix("event:") {
            event = v.trim().to_string();
        } else if let Some(v) = l.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(v.trim_start());
        }
    }
}

// ------------------------------------------------------------------ wren
fn wren(req: &Request, stop: &AtomicBool, send: &dyn Fn(Ev)) -> Result<(), String> {
    let msgs: Vec<Value> = req.messages.iter().map(|(r, c)| json!({"role": r, "content": c})).collect();
    let body = json!({"messages": msgs, "memory": req.memory, "web": true, "mode": req.mode});
    let mut last_err = String::from("no Wren server configured");
    for base in &req.cfg.wren_urls {
        let url = format!("{}/api/chat", base.trim_end_matches('/'));
        send(Ev::Status(format!("connecting to {}", host(base))));
        let r = match post_stream(&url, &[], body.clone()) {
            Ok(r) => r,
            Err(e) => {
                last_err = e;
                continue;
            }
        };
        return sse(r, stop, |ev, data| {
            let v: Value = serde_json::from_str(data).unwrap_or(Value::Null);
            match ev {
                "token" => send(Ev::Token(v["text"].as_str().unwrap_or("").to_string())),
                "status" => send(Ev::Status(v["text"].as_str().unwrap_or("").to_string())),
                "step" => send(Ev::Step(
                    v["label"].as_str().unwrap_or("").to_string(),
                    v["state"].as_str().unwrap_or("").to_string(),
                    v["detail"].as_str().map(String::from),
                )),
                "done" => {
                    let sources = v["sources"]
                        .as_array()
                        .map(|a| a.iter().map(|s| (s["title"].as_str().unwrap_or("").to_string(), s["url"].as_str().unwrap_or("").to_string())).collect())
                        .unwrap_or_default();
                    let memory = v["memory"].as_array().map(|a| a.iter().filter_map(|m| m.as_str().map(String::from)).collect());
                    send(Ev::Done { note: v["stats"].as_str().map(String::from), sources, memory });
                    return Ok(false);
                }
                _ => {
                    if let Some(e) = v["error"].as_str() {
                        return Err(e.to_string());
                    }
                }
            }
            Ok(true)
        });
    }
    Err(format!("{last_err} — is Wren's server running? (python -m wren.web in C:\\Code\\ai\\wren)"))
}

// ------------------------------------------------------------------ http APIs
fn history(req: &Request) -> Vec<Value> {
    req.messages.iter().map(|(r, c)| json!({"role": r, "content": c})).collect()
}

fn ollama(req: &Request, stop: &AtomicBool, send: &dyn Fn(Ev)) -> Result<(), String> {
    let model = req.model.clone().unwrap_or_else(|| req.cfg.ollama_model.clone());
    let url = format!("{}/api/chat", req.cfg.ollama_url.trim_end_matches('/'));
    send(Ev::Status(format!("ollama · {model}")));
    let mut r = post_stream(&url, &[], json!({"model": model, "messages": history(req), "stream": true}))?;
    let mut line = String::new();
    let mut tokens = 0u64;
    let t0 = std::time::Instant::now();
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        line.clear();
        if r.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            break;
        }
        let v: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
        if let Some(e) = v["error"].as_str() {
            return Err(format!("ollama: {e}"));
        }
        if let Some(t) = v["message"]["content"].as_str() {
            tokens += 1;
            send(Ev::Token(t.to_string()));
        }
        if v["done"].as_bool() == Some(true) {
            break;
        }
    }
    let secs = t0.elapsed().as_secs_f64().max(0.01);
    send(Ev::Done { note: Some(format!("{tokens} tokens · {:.0} tok/s · {model}", tokens as f64 / secs)), sources: vec![], memory: None });
    Ok(())
}

fn key(cfg_key: &str, env: &str) -> Option<String> {
    if !cfg_key.trim().is_empty() {
        return Some(cfg_key.trim().to_string());
    }
    std::env::var(env).ok().filter(|k| !k.is_empty())
}

fn openai(req: &Request, stop: &AtomicBool, send: &dyn Fn(Ev)) -> Result<(), String> {
    let model = req.model.clone().unwrap_or_else(|| req.cfg.openai_model.clone());
    let url = format!("{}/chat/completions", req.cfg.openai_url.trim_end_matches('/'));
    let mut headers = vec![];
    if let Some(k) = key(&req.cfg.openai_key, "OPENAI_API_KEY") {
        headers.push(("Authorization", format!("Bearer {k}")));
    }
    send(Ev::Status(format!("{} · {model}", host(&url))));
    let r = post_stream(&url, &headers, json!({"model": model, "messages": history(req), "stream": true}))?;
    let t0 = std::time::Instant::now();
    let mut n = 0u64;
    sse(r, stop, |_, data| {
        if data == "[DONE]" {
            return Ok(false);
        }
        let v: Value = serde_json::from_str(data).unwrap_or(Value::Null);
        if let Some(t) = v["choices"][0]["delta"]["content"].as_str() {
            n += 1;
            send(Ev::Token(t.to_string()));
        }
        Ok(true)
    })?;
    let secs = t0.elapsed().as_secs_f64().max(0.01);
    send(Ev::Done { note: Some(format!("{n} chunks · {:.1} s · {model}", secs)), sources: vec![], memory: None });
    Ok(())
}

fn anthropic(req: &Request, stop: &AtomicBool, send: &dyn Fn(Ev)) -> Result<(), String> {
    let Some(k) = key(&req.cfg.anthropic_key, "ANTHROPIC_API_KEY") else {
        return Err("no Anthropic key: set ANTHROPIC_API_KEY or ai.anthropic_key in the config (oriel --config)".into());
    };
    let model = req.model.clone().unwrap_or_else(|| req.cfg.anthropic_model.clone());
    send(Ev::Status(format!("anthropic · {model}")));
    let headers = vec![("x-api-key", k), ("anthropic-version", "2023-06-01".to_string())];
    let r = post_stream("https://api.anthropic.com/v1/messages", &headers, json!({"model": model, "max_tokens": 8192, "messages": history(req), "stream": true}))?;
    let mut out_tokens = 0u64;
    sse(r, stop, |ev, data| {
        let v: Value = serde_json::from_str(data).unwrap_or(Value::Null);
        match ev {
            "content_block_delta" => {
                if let Some(t) = v["delta"]["text"].as_str() {
                    send(Ev::Token(t.to_string()));
                }
            }
            "message_delta" => out_tokens = v["usage"]["output_tokens"].as_u64().unwrap_or(out_tokens),
            "error" => return Err(v["error"]["message"].as_str().unwrap_or("anthropic error").to_string()),
            "message_stop" => return Ok(false),
            _ => {}
        }
        Ok(true)
    })?;
    send(Ev::Done { note: Some(format!("{out_tokens} tokens · {model}")), sources: vec![], memory: None });
    Ok(())
}

// ------------------------------------------------------------------ CLI agents
fn transcript(req: &Request) -> String {
    let (last, prior) = req.messages.split_last().map(|(l, p)| (l.1.clone(), p)).unwrap_or_default();
    if prior.is_empty() {
        return last;
    }
    let mut t: String = prior
        .iter()
        .map(|(r, c)| format!("{}: {c}", if r == "user" { "User" } else { "Assistant" }))
        .collect::<Vec<_>>()
        .join("\n\n");
    if t.len() > 12000 {
        t = t[t.len() - 12000..].to_string();
    }
    format!("(Conversation so far, for context:)\n\n{t}\n\n(New message:)\n{last}")
}

/// Spawn a CLI with the prompt on stdin; call parse(json line) per stdout line.
fn run_cli(
    exe: &str,
    args: &[String],
    prompt: &str,
    cwd: &std::path::Path,
    stop: &AtomicBool,
    mut parse: impl FnMut(&Value) -> Result<(), String>,
) -> Result<(), String> {
    let path = which(exe).ok_or_else(|| format!("{exe} isn't installed (or not on PATH)"))?;
    let p = path.to_string_lossy().to_string();
    let mut cmd = if cfg!(windows) && (p.ends_with(".cmd") || p.ends_with(".bat")) {
        let mut c = std::process::Command::new("cmd.exe");
        c.arg("/c").arg(&p);
        c
    } else {
        std::process::Command::new(&p)
    };
    cmd.args(args).current_dir(cwd).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    cmd.env_remove("NO_COLOR");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // no console window flashing up
    }
    let mut child = cmd.spawn().map_err(|e| format!("couldn't start {exe}: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
    }
    let stderr = child.stderr.take().unwrap();
    let err_buf = Arc::new(std::sync::Mutex::new(String::new()));
    let eb = err_buf.clone();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = BufReader::new(stderr).read_to_string(&mut s);
        *eb.lock().unwrap() = s;
    });
    let out = BufReader::new(child.stdout.take().unwrap());
    let mut got = false;
    let mut result = Ok(());
    for line in out.lines() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
        got = true;
        if let Err(e) = parse(&v) {
            result = Err(e);
            break;
        }
    }
    let _ = child.kill();
    let status = child.wait().ok();
    if result.is_ok() && !got && !stop.load(Ordering::SeqCst) {
        let err = err_buf.lock().unwrap().clone();
        let tail = err.trim().lines().last().unwrap_or("").to_string();
        let code = status.and_then(|s| s.code()).unwrap_or(-1);
        return Err(if tail.is_empty() { format!("{exe} exited with code {code}") } else { tail });
    }
    result
}

fn claude(req: &Request, stop: &AtomicBool, send: &dyn Fn(Ev)) -> Result<(), String> {
    let sid = req.state.get("claude").and_then(|s| s.get("session")).and_then(|v| v.as_str()).map(String::from);
    let mode = match req.perms.as_str() {
        "full" => "bypassPermissions",
        "read" => "plan",
        "ask" => "default",
        _ => "acceptEdits",
    };
    let mut args: Vec<String> = ["-p", "--output-format", "stream-json", "--verbose", "--include-partial-messages", "--permission-mode", mode]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Some(s) = &sid {
        args.push("--resume".into());
        args.push(s.clone());
    }
    if let Some(m) = &req.model {
        args.push("--model".into());
        args.push(m.clone());
    }
    let prompt = if sid.is_some() { req.messages.last().map(|m| m.1.clone()).unwrap_or_default() } else { transcript(req) };
    send(Ev::Status("claude code is working".into()));
    let mut note = None;
    run_cli("claude", &args, &prompt, &req.cwd, stop, |ev| {
        if let Some(s) = ev["session_id"].as_str() {
            send(Ev::State("claude.session".into(), s.to_string()));
        }
        match ev["type"].as_str() {
            Some("stream_event") => {
                let d = &ev["event"]["delta"];
                if d["type"] == "text_delta" {
                    send(Ev::Token(d["text"].as_str().unwrap_or("").to_string()));
                }
            }
            Some("assistant") => {
                // tool use shows up as a step, like nest's work log
                if let Some(items) = ev["message"]["content"].as_array() {
                    for it in items {
                        if it["type"] == "tool_use" {
                            let name = it["name"].as_str().unwrap_or("tool");
                            let input = &it["input"];
                            let what = input["file_path"].as_str().or(input["command"].as_str()).or(input["pattern"].as_str()).unwrap_or("");
                            send(Ev::Step(format!("{name} {}", crate::ui::fit(what, 60)), "done".into(), None));
                        }
                    }
                }
            }
            Some("result") => {
                if ev["is_error"].as_bool() == Some(true) {
                    return Err(ev["result"].as_str().unwrap_or("Claude Code returned an error").to_string());
                }
                let tokens = ev["usage"]["output_tokens"].as_u64().unwrap_or(0);
                let cost = ev["total_cost_usd"].as_f64().unwrap_or(0.0);
                note = Some(format!("{tokens} tokens · ${cost:.3} · claude code"));
            }
            _ => {}
        }
        Ok(())
    })
    .map_err(|e| format!("{e} (run `claude` once in a terminal to sign in)"))?;
    send(Ev::Done { note, sources: vec![], memory: None });
    Ok(())
}

fn codex(req: &Request, stop: &AtomicBool, send: &dyn Fn(Ev)) -> Result<(), String> {
    let tid = req.state.get("codex").and_then(|s| s.get("thread")).and_then(|v| v.as_str()).map(String::from);
    let mut args: Vec<String> = vec!["exec".into()];
    if let Some(t) = &tid {
        args.push("resume".into());
        args.push(t.clone());
    }
    args.extend(["--json".into(), "--skip-git-repo-check".into()]);
    if req.perms == "full" {
        args.push("--dangerously-bypass-approvals-and-sandbox".into());
    } else {
        args.push("-c".into());
        args.push(format!("sandbox_mode={}", if req.perms == "read" { "read-only" } else { "workspace-write" }));
    }
    if let Some(m) = &req.model {
        args.push("-m".into());
        args.push(m.clone());
    }
    args.push("-".into());
    let prompt = if tid.is_some() { req.messages.last().map(|m| m.1.clone()).unwrap_or_default() } else { transcript(req) };
    send(Ev::Status("codex is working".into()));
    let mut note = None;
    run_cli("codex", &args, &prompt, &req.cwd, stop, |ev| {
        match ev["type"].as_str() {
            Some("thread.started") => {
                if let Some(t) = ev["thread_id"].as_str() {
                    send(Ev::State("codex.thread".into(), t.to_string()));
                }
            }
            Some("item.completed") => {
                let it = &ev["item"];
                match it["type"].as_str() {
                    Some("agent_message") => send(Ev::Token(format!("{}\n\n", it["text"].as_str().unwrap_or("")))),
                    Some("command_execution") => send(Ev::Step(format!("ran {}", crate::ui::fit(it["command"].as_str().unwrap_or(""), 60)), "done".into(), None)),
                    Some("file_change") => send(Ev::Step("edited files".into(), "done".into(), None)),
                    _ => {}
                }
            }
            Some("turn.completed") => note = Some(format!("{} tokens · codex", ev["usage"]["output_tokens"].as_u64().unwrap_or(0))),
            Some("error") | Some("turn.failed") => {
                return Err(ev["message"].as_str().map(String::from).unwrap_or_else(|| ev["error"].to_string()));
            }
            _ => {}
        }
        Ok(())
    })
    .map_err(|e| format!("{e} (run `codex login` in a terminal)"))?;
    send(Ev::Done { note, sources: vec![], memory: None });
    Ok(())
}
