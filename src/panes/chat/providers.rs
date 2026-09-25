//! Every AI the chat can talk to, streaming. Each request runs on its own thread and reports through `send`.
//!
//!   claude     Claude Code CLI (`claude -p --output-format stream-json`), resumes its session per chat
//!   codex      Codex CLI (`codex exec --json`), resumes its thread per chat
//!   ollama     any local Ollama model
//!   openai     any OpenAI-compatible endpoint (OpenAI, OpenRouter, LM Studio, llama.cpp...)
//!   anthropic  the Anthropic API
//!
//! The two CLI agents report everything they do (tool calls, diffs, output, todos) through agent.rs.

use super::agent;
use super::approve;
use super::store::{Todo, Tool};
use crate::config::{AiConfig, which};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const PROVIDERS: &[(&str, &str, &str)] = &[
    // id, label, what
    ("claude", "Claude Code", "Claude Code CLI, works in the chat's folder"),
    ("codex", "Codex", "OpenAI Codex CLI, works in the chat's folder"),
    ("ollama", "Ollama", "a local model pulled into Ollama"),
    ("openai", "OpenAI-compatible", "OpenAI, OpenRouter, LM Studio, llama.cpp…"),
    ("anthropic", "Anthropic API", "Claude over the API (needs a key)"),
];

/// Which AIs this machine can actually use, in the order the default is picked from.
pub fn available(cfg: &AiConfig) -> Vec<&'static str> {
    let reach = |url: &str| -> bool {
        let hostport = url.split("//").nth(1).unwrap_or(url).split('/').next().unwrap_or("");
        use std::net::ToSocketAddrs;
        hostport
            .to_socket_addrs()
            .ok()
            .and_then(|mut a| a.next())
            .map(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_millis(150)).is_ok())
            .unwrap_or(false)
    };
    let mut out = vec![];
    if which("claude").is_some() {
        out.push("claude");
    }
    if which("codex").is_some() {
        out.push("codex");
    }
    if which("ollama").is_some() || reach(&cfg.ollama_url) {
        out.push("ollama");
    }
    if key(&cfg.openai_key, "OPENAI_API_KEY").is_some() || (!cfg.openai_url.contains("api.openai.com") && reach(&cfg.openai_url)) {
        out.push("openai");
    }
    if key(&cfg.anthropic_key, "ANTHROPIC_API_KEY").is_some() {
        out.push("anthropic");
    }
    out
}

/// A provider's name for people. Anything unknown (e.g. an AI an old chat used) is just "ai".
pub fn label(id: &str) -> &'static str {
    match id {
        "none" => "no AI",
        _ => PROVIDERS.iter().find(|p| p.0 == id).map(|p| p.1).unwrap_or("ai"),
    }
}

pub fn is_known(id: &str) -> bool {
    PROVIDERS.iter().any(|p| p.0 == id)
}

pub enum Ev {
    /// Reply text.
    Token(String),
    /// Thinking: text (often empty: redacted) and an estimated token count, both added to the current block.
    Thinking { text: String, tokens: u64 },
    /// A tool call, new or updated (matched by id; `parent` nests it under a subagent).
    Tool(Tool),
    /// The agent's todo list, whole.
    Todos(Vec<Todo>),
    /// Output tokens so far.
    Usage(u64),
    Status(String),
    /// Remember a value in the chat's per-provider state (CLI session ids).
    State(String, String),
    /// Claude Code wants permission for a tool call (/perms ask).
    Ask(approve::Ask),
    Done { note: Option<String> },
    Error(String),
}

pub struct Request {
    pub provider: String,
    pub model: Option<String>,
    pub messages: Vec<(String, String)>,
    pub cwd: std::path::PathBuf,
    pub perms: String,
    pub state: serde_json::Map<String, Value>,
    pub cfg: AiConfig,
}

pub fn start(req: Request, stop: Arc<AtomicBool>, send: impl Fn(Ev) + Send + Sync + 'static) {
    let send: Arc<dyn Fn(Ev) + Send + Sync> = Arc::new(send);
    std::thread::spawn(move || {
        let r = match req.provider.as_str() {
            "claude" => claude(&req, &stop, send.clone()),
            "codex" => codex(&req, &stop, &*send),
            "ollama" => ollama(&req, &stop, &*send),
            "openai" => openai(&req, &stop, &*send),
            "anthropic" => anthropic(&req, &stop, &*send),
            _ => Err("no AI is set up: install Claude Code or Codex, run Ollama, or add a key with /key openai <key>".into()),
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
    let t0 = Instant::now();
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
            send(Ev::Usage(tokens));
        }
        if v["done"].as_bool() == Some(true) {
            break;
        }
    }
    let secs = t0.elapsed().as_secs_f64().max(0.01);
    send(Ev::Done { note: Some(format!("{tokens} tokens · {:.0} tok/s · {model}", tokens as f64 / secs)) });
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
    let t0 = Instant::now();
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
    send(Ev::Done { note: Some(format!("{n} chunks · {:.1} s · {model}", secs)) });
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
    send(Ev::Done { note: Some(format!("{out_tokens} tokens · {model}")) });
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
        let mut cut = t.len() - 12000;
        while !t.is_char_boundary(cut) {
            cut += 1;
        }
        t = t[cut..].to_string();
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

fn claude(req: &Request, stop: &AtomicBool, send: Arc<dyn Fn(Ev) + Send + Sync>) -> Result<(), String> {
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
    // /perms ask: every call that needs permission comes back to the chat through oriel's own MCP server
    let mut broker = None;
    let mut cfg_file = None;
    if req.perms == "ask" {
        if let Ok(b) = approve::Broker::start(req.cwd.clone(), send.clone()) {
            if let Some((a, path)) = approve::claude_args(&b) {
                args.extend(a);
                cfg_file = Some(path);
                broker = Some(b);
            }
        }
        if broker.is_none() {
            send(Ev::Status("couldn't set up approvals: anything that needs permission will be refused".into()));
        }
    }
    if let Some(s) = &sid {
        args.push("--resume".into());
        args.push(s.clone());
    }
    if let Some(m) = &req.model {
        args.push("--model".into());
        args.push(m.clone());
    }
    let prompt = if sid.is_some() { req.messages.last().map(|m| m.1.clone()).unwrap_or_default() } else { transcript(req) };
    send(Ev::Status("claude code is starting".into()));
    let t0 = Instant::now();
    let mut p = agent::Claude::new(&req.cwd);
    let r = run_cli("claude", &args, &prompt, &req.cwd, stop, |ev| p.feed(ev, t0.elapsed().as_millis() as u64, &mut |e| send(e)));
    drop(broker);
    if let Some(f) = cfg_file {
        let _ = std::fs::remove_file(f);
    }
    r.map_err(|e| format!("{e} (run `claude` once in a terminal to sign in)"))?;
    send(Ev::Done { note: Some(p.note(t0.elapsed())) });
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
        // codex exec can't stop and ask, so "ask" is read-only
        args.push("-c".into());
        args.push(format!("sandbox_mode={}", if req.perms == "read" || req.perms == "ask" { "read-only" } else { "workspace-write" }));
    }
    if let Some(m) = &req.model {
        args.push("-m".into());
        args.push(m.clone());
    }
    args.push("-".into());
    let prompt = if tid.is_some() { req.messages.last().map(|m| m.1.clone()).unwrap_or_default() } else { transcript(req) };
    send(Ev::Status("codex is starting".into()));
    let t0 = Instant::now();
    let mut p = agent::Codex::new(&req.cwd);
    run_cli("codex", &args, &prompt, &req.cwd, stop, |ev| p.feed(ev, t0.elapsed().as_millis() as u64, &mut |e| send(e)))
        .map_err(|e| format!("{e} (run `codex login` in a terminal)"))?;
    send(Ev::Done { note: Some(p.note(t0.elapsed())) });
    Ok(())
}
