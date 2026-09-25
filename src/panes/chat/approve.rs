//! `/perms ask` for Claude Code: approve each tool call from the chat.
//!
//! Claude Code runs with `--permission-prompt-tool mcp__oriel__approve` and an MCP server that is oriel itself
//! (`oriel --mcp-approve <port> <token>`, JSON-RPC over stdio). That server forwards every approval request over
//! loopback TCP to the running oriel (a `Broker` on the provider thread, one per reply, random token), which
//! shows it in the chat and answers with the user's y / n / a.

use super::providers::Ev;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

pub const TOOL: &str = "mcp__oriel__approve";

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Decision {
    Allow,
    Deny,
}

/// One pending approval, shown in the chat until answered.
pub struct Ask {
    pub tool: String,
    pub label: String,
    pub target: String,
    /// A diff or the command, when there is one to look at.
    pub body: Vec<String>,
    pub reply: mpsc::Sender<Decision>,
}

/// The loopback end in oriel. Dropping it stops accepting.
pub struct Broker {
    pub port: u16,
    pub token: String,
    stop: Arc<AtomicBool>,
}

impl Drop for Broker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut s = String::new();
    for i in 0..2u64 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(i ^ std::process::id() as u64);
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        s.push_str(&format!("{:016x}", h.finish()));
    }
    s
}

impl Broker {
    pub fn start(cwd: std::path::PathBuf, send: Arc<dyn Fn(Ev) + Send + Sync>) -> std::io::Result<Broker> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let tok = token();
        let stop = Arc::new(AtomicBool::new(false));
        let (st, tk) = (stop.clone(), tok.clone());
        std::thread::spawn(move || {
            while !st.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let (send, tk, cwd) = (send.clone(), tk.clone(), cwd.clone());
                        std::thread::spawn(move || {
                            let _ = handle(stream, &tk, &cwd, &*send);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(40)),
                    Err(_) => break,
                }
            }
        });
        Ok(Broker { port, token: tok, stop })
    }
}

/// One request from the MCP server: `{"token", "tool_name", "input"}` -> `{"behavior": "allow"|"deny"}`.
fn handle(stream: TcpStream, tok: &str, cwd: &std::path::Path, send: &dyn Fn(Ev)) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let v: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
    let mut out = &stream;
    if v["token"].as_str() != Some(tok) {
        return writeln!(out, "{}", json!({"behavior": "deny", "message": "bad token"}));
    }
    let tool = v["tool_name"].as_str().unwrap_or("tool").to_string();
    let t = super::agent::describe("", &tool, &v["input"], cwd);
    let mut body = t.body;
    if body.is_empty() {
        // a long or multi-line command is shown whole (a one-liner is already the target)
        if let Some(c) = v["input"]["command"].as_str().filter(|c| c.contains('\n') || c.chars().count() > 60) {
            body = c.lines().map(|l| super::agent::line('>', None, l)).collect();
        }
    }
    let (tx, rx) = mpsc::channel();
    send(Ev::Ask(Ask { tool, label: t.label, target: t.target, body, reply: tx }));
    // no answer (the reply was stopped, oriel closed) counts as a no
    let d = rx.recv().unwrap_or(Decision::Deny);
    let ans = match d {
        Decision::Allow => json!({"behavior": "allow"}),
        Decision::Deny => json!({"behavior": "deny", "message": "The user said no in oriel."}),
    };
    writeln!(out, "{ans}")
}

/// Ask the running oriel. Anything going wrong is a deny.
fn forward(port: u16, tok: &str, tool: &str, input: &Value) -> Result<Decision, String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("oriel isn't answering: {e}"))?;
    writeln!(s, "{}", json!({"token": tok, "tool_name": tool, "input": input})).map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line).map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(line.trim()).map_err(|e| e.to_string())?;
    Ok(if v["behavior"] == "allow" { Decision::Allow } else { Decision::Deny })
}

/// The MCP server side: `oriel --mcp-approve <port> <token>`, newline-delimited JSON-RPC on stdin/stdout.
pub fn serve_stdio(port: &str, tok: &str) {
    let port: u16 = port.parse().unwrap_or(0);
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(stdin.lock(), stdout.lock(), |tool, input| forward(port, tok, tool, input));
}

pub fn serve(input: impl BufRead, mut out: impl Write, ask: impl Fn(&str, &Value) -> Result<Decision, String>) {
    for line in input.lines() {
        let Ok(line) = line else { break };
        let Ok(req) = serde_json::from_str::<Value>(line.trim()) else { continue };
        let id = req.get("id").cloned();
        let method = req["method"].as_str().unwrap_or("");
        let result = match method {
            "initialize" => json!({
                "protocolVersion": req["params"]["protocolVersion"].as_str().unwrap_or("2024-11-05"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "oriel", "version": env!("CARGO_PKG_VERSION")}
            }),
            "ping" => json!({}),
            "tools/list" => json!({"tools": [{
                "name": "approve",
                "description": "Ask the person using oriel to allow or deny a tool call.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "tool_name": {"type": "string"},
                        "input": {"type": "object"},
                        "tool_use_id": {"type": "string"}
                    },
                    "required": ["tool_name", "input"]
                }
            }]}),
            "tools/call" => {
                let a = &req["params"]["arguments"];
                let input = if a["input"].is_object() { a["input"].clone() } else { json!({}) };
                let answer = match ask(a["tool_name"].as_str().unwrap_or("tool"), &input) {
                    Ok(Decision::Allow) => json!({"behavior": "allow", "updatedInput": input}),
                    Ok(Decision::Deny) => json!({"behavior": "deny", "message": "The user said no in oriel."}),
                    Err(e) => json!({"behavior": "deny", "message": format!("Couldn't ask the user: {e}")}),
                };
                json!({"content": [{"type": "text", "text": answer.to_string()}]})
            }
            _ => {
                if let Some(id) = id {
                    let _ = writeln!(out, "{}", json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("no method {method}")}}));
                    let _ = out.flush();
                }
                continue;
            }
        };
        if let Some(id) = id {
            let _ = writeln!(out, "{}", json!({"jsonrpc": "2.0", "id": id, "result": result}));
            let _ = out.flush();
        }
    }
}

/// Claude Code's arguments for asking through oriel, and the config file they point at (delete it afterwards).
pub fn claude_args(b: &Broker) -> Option<(Vec<String>, std::path::PathBuf)> {
    let exe = std::env::current_exe().ok()?;
    let cfg = json!({"mcpServers": {"oriel": {"command": exe.to_string_lossy(), "args": ["--mcp-approve", b.port.to_string(), b.token.clone()]}}});
    let path = std::env::temp_dir().join(format!("oriel-approve-{}-{}.json", std::process::id(), b.port));
    std::fs::write(&path, cfg.to_string()).ok()?;
    Some((vec!["--mcp-config".into(), path.to_string_lossy().to_string(), "--permission-prompt-tool".into(), TOOL.into()], path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The whole loop without Claude: JSON-RPC in -> the TCP broker -> an Ask in the "chat" -> answer -> reply.
    #[test]
    fn chat_approve_roundtrip() {
        let asks: Arc<Mutex<Vec<String>>> = Arc::default();
        let a2 = asks.clone();
        let send: Arc<dyn Fn(Ev) + Send + Sync> = Arc::new(move |ev| {
            if let Ev::Ask(a) = ev {
                a2.lock().unwrap().push(format!("{} {}", a.label, a.target));
                let _ = a.reply.send(if a.tool == "Bash" { Decision::Allow } else { Decision::Deny });
            }
        });
        let b = Broker::start(std::path::PathBuf::from("/w"), send).unwrap();
        let rpc = [
            json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "approve", "arguments": {"tool_name": "Bash", "input": {"command": "cargo test"}}}}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "approve", "arguments": {"tool_name": "Write", "input": {"file_path": "/w/a.txt", "content": "hi"}}}}),
        ]
        .map(|v| v.to_string())
        .join("\n");
        let mut out = vec![];
        let (port, tok) = (b.port, b.token.clone());
        serve(rpc.as_bytes(), &mut out, |tool, input| forward(port, &tok, tool, input));
        let replies: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(replies.len(), 4, "{replies:?}");
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(replies[1]["result"]["tools"][0]["name"], "approve");
        let ans = |i: usize| serde_json::from_str::<Value>(replies[i]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(ans(2)["behavior"], "allow");
        assert_eq!(ans(2)["updatedInput"]["command"], "cargo test");
        assert_eq!(ans(3)["behavior"], "deny");
        assert_eq!(*asks.lock().unwrap(), vec!["Bash cargo test".to_string(), "Write a.txt".to_string()]);
        // a wrong token is refused without asking
        assert_eq!(forward(b.port, "nope", "Bash", &json!({})).unwrap(), Decision::Deny);
        assert_eq!(asks.lock().unwrap().len(), 2);
    }
}
