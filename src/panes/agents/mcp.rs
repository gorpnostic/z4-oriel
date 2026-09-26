//! The lead's tools, two ways:
//!
//!  * MCP: `oriel mcp-lead <port> <token>` is a tiny MCP stdio server the lead's CLI starts. Each tool call is
//!    forwarded over loopback TCP (random port, random token, like chat/approve.rs) to a `Broker` in the running
//!    oriel, which hands it to the agents app and writes back its answer.
//!  * Text protocol, for leads that can't load MCP servers per run: the lead ends each turn with a ```json block
//!    of actions, oriel runs them through the very same handler and sends the results as its next prompt.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

/// (name, description for the model, JSON schema of the arguments). Shared by MCP and the text protocol.
pub const TOOLS: &[(&str, &str, &str)] = &[
    ("roster", "The workers you can hand tasks to: agent, model, tier (cheap < mid < premium), what each is good at, per-task budget, how many it's running, its track record, and its plan's usage (% of the 5-hour / weekly windows). Paused or nearly used-up workers are marked.", r#"{"type":"object","properties":{}}"#),
    ("plan", "Submit tasks for workers. oriel checks the plan mechanically and rejects it with reasons if concurrent tasks own overlapping files, several tasks own hotspot files (manifests, lockfiles, mod.rs/registries, migrations: give them to ONE scaffold task, the rest will wait for it), a task is size L (split it) or deps are unknown/cyclic. Tasks start as soon as their depends_on are merged and a worker slot is free. Returns the task ids.", r#"{"type":"object","properties":{"tasks":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string","description":"your short id, used by depends_on"},"title":{"type":"string"},"goal":{"type":"string","description":"complete instructions a worker with no other context can follow"},"worker":{"type":"string","description":"a roster name"},"owns":{"type":"array","items":{"type":"string"},"description":"globs it may edit, e.g. src/cli/**"},"reads":{"type":"array","items":{"type":"string"}},"depends_on":{"type":"array","items":{"type":"string"}},"acceptance":{"type":"string","description":"a shell command that proves it works (run in the merge gate)"},"size":{"type":"string","enum":["S","M"],"description":"S about 100 lines / up to 3 files, M about 400 / up to 8"},"kind":{"type":"string","description":"scaffold, feature, mechanical, refactor, bugfix, test, docs"}},"required":["id","title","goal","worker","owns"]}}},"required":["tasks"]}"#),
    ("wait", "Block until something happens instead of polling: a task finished (with a compact result card), merged, bounced back to its worker (conflict or failed gate, oriel retries it by itself), got blocked or failed. Returns the new events since your last wait, or after timeout_s (default 300, max 600) with none.", r#"{"type":"object","properties":{"timeout_s":{"type":"integer"},"ids":{"type":"array","items":{"type":"string"},"description":"only wake for these tasks"}}}"#),
    ("task_status", "Compact cards for one task, or every task in this run when id is omitted: state, summary, files with lines changed, gate result, conflicts, questions, cost.", r#"{"type":"object","properties":{"id":{"type":"string"}}}"#),
    ("task_diff", "A task's diff against where it branched off, one page at a time (200 lines), optionally just one path. Only when a card isn't enough.", r#"{"type":"object","properties":{"id":{"type":"string"},"path":{"type":"string"},"page":{"type":"integer"}},"required":["id"]}"#),
    ("merge", "Queue a finished task for merging into the integration branch. Merges run one at a time: conflict check, then the gate (build/tests + the task's acceptance command) on the merged result, then the branch moves. A conflict or failed gate goes back to the same worker automatically (2 tries, then a fresh worker) and merges when fixed. If the acceptance command itself was wrong (event acceptance_broken), merge again with a corrected one (or \"\" for none). Watch wait for the outcome.", r#"{"type":"object","properties":{"id":{"type":"string"},"acceptance":{"type":"string","description":"replace the task's acceptance command first"}},"required":["id"]}"#),
    ("send_followup", "Send a finished task's worker more instructions in the same session (review feedback); it goes back to running.", r#"{"type":"object","properties":{"id":{"type":"string"},"text":{"type":"string"}},"required":["id","text"]}"#),
    ("resolve_conflicts", "Merge the current integration branch into a task's worktree now and have its worker resolve the conflict markers (merge does this by itself on a conflict).", r#"{"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}"#),
    ("spawn_task", "Add one task outside a plan (same checks as plan).", r#"{"type":"object","properties":{"worker":{"type":"string"},"title":{"type":"string"},"goal":{"type":"string"},"owns":{"type":"array","items":{"type":"string"}},"depends_on":{"type":"array","items":{"type":"string"}},"acceptance":{"type":"string"},"size":{"type":"string","enum":["S","M"]}},"required":["worker","title","goal","owns"]}"#),
    ("discard", "Throw a task away: stops its worker and deletes its worktree and branch.", r#"{"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}"#),
    ("note", "Post a short progress note on oriel's board. Set needs_user to notify the user (e.g. a task is blocked and needs a decision).", r#"{"type":"object","properties":{"text":{"type":"string"},"needs_user":{"type":"boolean"}},"required":["text"]}"#),
    ("done", "Finish the run once everything useful is merged into the integration branch. The user then reviews it and merges it into their own branch.", r#"{"type":"object","properties":{"summary":{"type":"string"}},"required":["summary"]}"#),
];

pub fn tool_list() -> Value {
    Value::Array(TOOLS.iter().map(|(n, d, s)| json!({"name": n, "description": d, "inputSchema": serde_json::from_str::<Value>(s).unwrap_or(json!({"type":"object"}))})).collect())
}

/// One tool call waiting for the agents app.
pub struct Call {
    pub run: String,
    pub tool: String,
    pub args: Value,
    pub reply: mpsc::Sender<Result<String, String>>,
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

/// The loopback end, one per lead run. Dropping it stops accepting.
pub struct Broker {
    pub port: u16,
    pub token: String,
    /// tool calls answered so far (a lead that never calls one didn't get its tools)
    pub calls: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl Drop for Broker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Broker {
    /// `deliver` hands a call to the agents app (and wakes it).
    pub fn start(run: String, deliver: Arc<dyn Fn(Call) + Send + Sync>) -> std::io::Result<Broker> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let tok = token();
        let stop = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let (st, tk, n) = (stop.clone(), tok.clone(), calls.clone());
        std::thread::spawn(move || {
            while !st.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let (deliver, tk, run, n, st) = (deliver.clone(), tk.clone(), run.clone(), n.clone(), st.clone());
                        std::thread::spawn(move || {
                            let _ = handle(stream, &tk, &run, &*deliver, &n, &st);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(30)),
                    Err(_) => break,
                }
            }
        });
        Ok(Broker { port, token: tok, calls, stop })
    }
}

/// `{"token","tool","args"}` → `{"ok":true,"text":…}` / `{"ok":false,"error":…}`.
fn handle(stream: TcpStream, tok: &str, run: &str, deliver: &dyn Fn(Call), calls: &AtomicUsize, stop: &AtomicBool) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let v: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
    let mut out = &stream;
    if v["token"].as_str() != Some(tok) {
        return writeln!(out, "{}", json!({"ok": false, "error": "bad token"}));
    }
    let (tx, rx) = mpsc::channel();
    deliver(Call { run: run.to_string(), tool: v["tool"].as_str().unwrap_or("").to_string(), args: v["args"].clone(), reply: tx });
    // wait long-polls for up to 10 minutes; give it room, but notice the run being stopped
    let deadline = std::time::Instant::now() + Duration::from_secs(12 * 60);
    let r = loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(r) => break r,
            Err(mpsc::RecvTimeoutError::Disconnected) => break Err("oriel dropped the call".into()),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if stop.load(Ordering::SeqCst) {
                    break Err("the run was stopped".into());
                }
                if std::time::Instant::now() > deadline {
                    break Err("oriel took too long to answer".into());
                }
            }
        }
    };
    calls.fetch_add(1, Ordering::SeqCst);
    let ans = match r {
        Ok(t) => json!({"ok": true, "text": t}),
        Err(e) => json!({"ok": false, "error": e}),
    };
    writeln!(out, "{ans}")
}

/// Ask the running oriel (the MCP server side). Anything going wrong is an error result.
pub fn forward(port: u16, tok: &str, tool: &str, args: &Value) -> Result<String, String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("oriel isn't answering: {e}"))?;
    writeln!(s, "{}", json!({"token": tok, "tool": tool, "args": args})).map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line).map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(line.trim()).map_err(|e| format!("bad answer from oriel: {e}"))?;
    if v["ok"].as_bool() == Some(true) { Ok(v["text"].as_str().unwrap_or("").to_string()) } else { Err(v["error"].as_str().unwrap_or("failed").to_string()) }
}

/// `oriel mcp-lead <port> <token>`: newline-delimited JSON-RPC on stdin/stdout.
pub fn serve_stdio(port: &str, tok: &str) {
    let port: u16 = port.parse().unwrap_or(0);
    let stdin = std::io::stdin();
    serve(stdin.lock(), std::io::stdout(), |tool, args| forward(port, tok, tool, args));
}

/// How often a long call (wait, merge) reports progress, so clients that time out idle calls keep waiting.
const PROGRESS_EVERY: Duration = Duration::from_secs(25);

pub fn serve<W: Write + Send>(input: impl BufRead, out: W, call: impl Fn(&str, &Value) -> Result<String, String> + Sync) {
    serve_with(input, out, call, PROGRESS_EVERY)
}

pub fn serve_with<W: Write + Send>(input: impl BufRead, out: W, call: impl Fn(&str, &Value) -> Result<String, String> + Sync, every: Duration) {
    let out = std::sync::Mutex::new(out);
    let write = |v: &Value| {
        let mut o = out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(o, "{v}");
        let _ = o.flush();
    };
    for line in input.lines() {
        let Ok(line) = line else { break };
        let Ok(req) = serde_json::from_str::<Value>(line.trim()) else { continue };
        let id = req.get("id").cloned();
        let method = req["method"].as_str().unwrap_or("");
        let result = match method {
            "initialize" => json!({
                "protocolVersion": req["params"]["protocolVersion"].as_str().unwrap_or("2024-11-05"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "oriel", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "oriel's lead tools: hand tasks to worker agents, wait for them, review and merge their work into the integration branch."
            }),
            "ping" => json!({}),
            "tools/list" => json!({"tools": tool_list()}),
            "tools/call" => {
                let name = req["params"]["name"].as_str().unwrap_or("");
                let args = if req["params"]["arguments"].is_object() { req["params"]["arguments"].clone() } else { json!({}) };
                if !TOOLS.iter().any(|t| t.0 == name) {
                    json!({"content": [{"type": "text", "text": format!("no tool called {name}")}], "isError": true})
                } else {
                    let token = req["params"]["_meta"]["progressToken"].clone();
                    let finished = AtomicBool::new(false);
                    let r = std::thread::scope(|s| {
                        if !token.is_null() {
                            s.spawn(|| {
                                let (mut n, mut last) = (0u64, std::time::Instant::now());
                                while !finished.load(Ordering::SeqCst) {
                                    std::thread::sleep(Duration::from_millis(20).min(every));
                                    if last.elapsed() >= every && !finished.load(Ordering::SeqCst) {
                                        n += 1;
                                        last = std::time::Instant::now();
                                        write(&json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": token, "progress": n, "message": "oriel is still on it"}}));
                                    }
                                }
                            });
                        }
                        let r = call(name, &args);
                        finished.store(true, Ordering::SeqCst);
                        r
                    });
                    match r {
                        Ok(t) => json!({"content": [{"type": "text", "text": t}]}),
                        Err(e) => json!({"content": [{"type": "text", "text": e}], "isError": true}),
                    }
                }
            }
            _ => {
                if let Some(id) = id {
                    write(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("no method {method}")}}));
                }
                continue;
            }
        };
        if let Some(id) = id {
            write(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
        }
    }
}

/// Kimi Code loads MCP servers only from a project file: `<folder>/.kimi-code/mcp.json` (kept out of git).
pub fn write_kimi_mcp(dir: &std::path::Path, exe: &std::path::Path, port: u16, tok: &str) -> std::io::Result<std::path::PathBuf> {
    let d = dir.join(".kimi-code");
    std::fs::create_dir_all(&d)?;
    let path = d.join("mcp.json");
    let cfg = json!({"mcpServers": {"oriel": {"command": exe.to_string_lossy(), "args": ["mcp-lead", port.to_string(), tok], "toolTimeoutMs": 1_800_000}}});
    std::fs::write(&path, serde_json::to_string_pretty(&cfg).unwrap_or_default())?;
    Ok(path)
}

// ------------------------------------------------------------------ the lead's instructions

pub struct Brief<'a> {
    pub integration: &'a str,
    pub base: &'a str,
    pub max_parallel: u32,
    pub budget: f64,
    pub roster: &'a str,
    /// The shell acceptance commands and the gate run in ("bash", "PowerShell", "cmd", "sh").
    pub shell: &'a str,
}

/// oriel's instructions for the lead (Claude gets them as an appended system prompt, the others at the top of
/// the first prompt). Byte-stable apart from the run's own facts, so repeated turns hit the prompt cache.
pub fn lead_system(b: &Brief, mcp: bool) -> String {
    let mut s = String::from(
        "You are the LEAD of a small team of coding agents, run by oriel (a terminal workspace). You never edit code yourself: you plan, hand tasks to workers, read their result cards and merge their work. Your folder is a read-only checkout of the integration branch; read code only to plan.\n\n\
         HOW A RUN WORKS\n\
         - Every task runs in its own git worktree branched from the integration branch. Finished tasks are squash-merged into it one at a time, after a gate (build/tests + the task's acceptance command) passes on the merged result. The user reviews the integration branch at the end and merges it themselves: never touch their branch, never push.\n\
         - Conflicts and gate failures go back to the same worker automatically (2 tries, then a fresh worker on a stronger tier); you see the outcome in wait.\n\n\
         HOW TO WORK\n\
         1. roster first. Then ONE plan with small tasks (S about 100 lines / 3 files, M about 400 / 8), each owning disjoint globs, each with an acceptance command when there's a way to check it. Hotspot files (Cargo.toml, package.json, lockfiles, mod.rs/lib.rs/index.ts registries, migrations) go to one scaffold task the others depend on. Two tasks or fewer run one after the other: for a small goal use a single task.\n\
         2. Write each goal so a worker with no other context can do it: what, where, how to verify. Never paste transcripts.\n\
         3. wait for events. For each finished card: merge it if the summary and files look right, send_followup with precise fixes, or discard it. Use task_diff only when the card isn't enough.\n\
         4. If a task comes back blocked, answer its questions with send_followup, re-plan it, or post a note with needs_user.\n\
         5. When everything useful is merged, call done with a short summary.\n\n\
         ROUTING\n\
         - Tiers: premium = opus / gpt-5.6-sol; mid = sonnet / gpt-5.6-terra / kimi-for-coding; cheap = haiku / gpt-5.6-luna.\n\
         - mechanical work (renames, docs, tests by pattern, lint fixes): cheap. A feature inside its owned files with an acceptance test: mid. Cross-cutting refactors, concurrency, bugs with an unclear cause, performance: premium.\n\
         - Frontend/UI: kimi if it's on the roster. Shell, CI and build scripts: codex.\n\
         - Spread concurrent tasks across vendors (separate usage limits). Skip a worker at 5h >= 85% or weekly >= 90%; below 25% weekly left, give it only S tasks.\n\n",
    );
    s.push_str(&format!(
        "THIS RUN\n- Integration branch: {} (made from the user's {}).\n- At most {} workers at once; the rest queue.\n- Budget for the whole run (you + workers): ${:.2}. New work is refused once it's spent.\n- Acceptance commands and the gate run in {} from the repo root: write them for {}{}.\n- Workers:\n{}\n",
        b.integration,
        b.base,
        b.max_parallel.clamp(1, 5),
        b.budget,
        b.shell,
        b.shell,
        match b.shell {
            "bash" | "sh" => " (e.g. `test -f notes.md && grep -q Usage notes.md`, `cargo test cli`)",
            "PowerShell" => " (e.g. `if (-not (Select-String -Quiet Usage notes.md)) { exit 1 }`)",
            _ => " (e.g. `findstr /c:Usage notes.md`)",
        },
        b.roster
    ));
    if mcp {
        s.push_str("\nYour tools are oriel's MCP tools (mcp__oriel__*): roster, plan, wait, task_status, task_diff, merge, send_followup, resolve_conflicts, spawn_task, discard, note, done.\n");
    } else {
        s.push_str(&text_protocol());
    }
    s
}

/// The action format for leads without MCP.
pub fn text_protocol() -> String {
    let mut s = String::from(
        "\nHOW TO ACT: you have no tool access to oriel. Instead, END EVERY REPLY with exactly one fenced JSON block listing the actions to run, in order:\n\
         ```json\n{\"actions\": [{\"tool\": \"plan\", \"args\": {\"tasks\": [{\"id\": \"a\", \"title\": \"Add a --json flag\", \"goal\": \"...\", \"worker\": \"codex\", \"owns\": [\"src/cli.rs\"], \"acceptance\": \"cargo test cli\", \"size\": \"S\"}]}}, {\"tool\": \"wait\", \"args\": {}}]}\n```\n\
         oriel runs them and replies with each result; then you decide the next actions. Use {\"tool\": \"done\", \"args\": {\"summary\": \"...\"}} to finish. Available tools and their args:\n",
    );
    for (n, d, schema) in TOOLS {
        s.push_str(&format!("- {n} {schema}: {d}\n"));
    }
    s
}

/// Pull the actions out of a text-protocol reply: the last ```json block (or bare JSON) with an "actions" list.
pub fn parse_actions(text: &str) -> Option<Vec<(String, Value)>> {
    let mut candidates: Vec<&str> = vec![];
    let mut rest = text;
    while let Some(i) = rest.find("```") {
        let after = &rest[i + 3..];
        let body_start = after.find('\n').map(|n| n + 1).unwrap_or(0);
        let Some(end) = after[body_start..].find("```") else { break };
        candidates.push(&after[body_start..body_start + end]);
        rest = &after[body_start + end + 3..];
    }
    if let (Some(a), Some(b)) = (text.find('{'), text.rfind('}')) {
        if a < b {
            candidates.push(&text[a..=b]);
        }
    }
    for c in candidates.into_iter().rev() {
        let Ok(v) = serde_json::from_str::<Value>(c.trim()) else { continue };
        let list = if v.is_array() { v.as_array().cloned() } else { v["actions"].as_array().cloned() };
        let Some(list) = list else { continue };
        let acts: Vec<(String, Value)> = list
            .iter()
            .filter_map(|a| {
                let tool = a["tool"].as_str().or(a["name"].as_str())?.to_string();
                let args = if a["args"].is_object() { a["args"].clone() } else if a["arguments"].is_object() { a["arguments"].clone() } else { json!({}) };
                Some((tool, args))
            })
            .collect();
        return Some(acts);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// JSON-RPC in → the TCP broker (a fake agents app answers) → results out, like the real lead's CLI sees it.
    #[test]
    fn agents_mcp_server_speaks_jsonrpc_over_the_bridge() {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let s2 = seen.clone();
        let deliver: Arc<dyn Fn(Call) + Send + Sync> = Arc::new(move |c: Call| {
            s2.lock().unwrap().push(format!("{} {} {}", c.run, c.tool, c.args));
            let _ = c.reply.send(match c.tool.as_str() {
                "roster" => Ok("codex: mid".into()),
                "merge" => Err("conflicts in a.txt".into()),
                _ => Ok("ok".into()),
            });
        });
        let b = Broker::start("r1".into(), deliver).unwrap();
        let rpc = [
            json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "roster", "arguments": {}}}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "merge", "arguments": {"id": "t1"}}}),
            json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "nope", "arguments": {}}}),
            json!({"jsonrpc": "2.0", "id": 5, "method": "bogus"}),
        ]
        .map(|v| v.to_string())
        .join("\n");
        let mut out = vec![];
        let (port, tok) = (b.port, b.token.clone());
        serve(rpc.as_bytes(), &mut out, |tool, args| forward(port, &tok, tool, args));
        let replies: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(replies.len(), 6, "{replies:?}");
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(replies[0]["result"]["capabilities"]["tools"], json!({}));
        let names: Vec<&str> = replies[1]["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names.len(), TOOLS.len());
        assert!(names.contains(&"plan") && names.contains(&"resolve_conflicts") && names.contains(&"wait") && names.contains(&"task_diff"));
        assert_eq!(replies[1]["result"]["tools"][1]["inputSchema"]["required"], json!(["tasks"]));
        assert_eq!(replies[1]["result"]["tools"][1]["inputSchema"]["properties"]["tasks"]["items"]["required"], json!(["id", "title", "goal", "worker", "owns"]));
        assert_eq!(replies[2]["result"]["content"][0]["text"], "codex: mid");
        assert!(replies[2]["result"]["isError"].is_null());
        assert_eq!(replies[3]["result"]["isError"], true);
        assert_eq!(replies[3]["result"]["content"][0]["text"], "conflicts in a.txt");
        assert_eq!(replies[4]["result"]["isError"], true);
        assert_eq!(replies[5]["error"]["code"], -32601);
        assert_eq!(*seen.lock().unwrap(), vec!["r1 roster {}".to_string(), "r1 merge {\"id\":\"t1\"}".to_string()]);
        assert_eq!(b.calls.load(Ordering::SeqCst), 2);
        // a wrong token never reaches the app
        assert!(forward(b.port, "nope", "roster", &json!({})).is_err());
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn agents_text_protocol_actions() {
        let reply = "Plan: two tasks.\n```json\n{\"actions\": [{\"tool\": \"spawn_task\", \"args\": {\"worker\": \"kimi\", \"title\": \"Docs\", \"prompt\": \"x\"}}, {\"tool\": \"wait\"}]}\n```\n";
        let a = parse_actions(reply).unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].0, "spawn_task");
        assert_eq!(a[0].1["worker"], "kimi");
        assert_eq!(a[1], ("wait".to_string(), json!({})));
        assert_eq!(parse_actions("{\"actions\":[{\"tool\":\"done\",\"args\":{\"summary\":\"ok\"}}]}").unwrap()[0].0, "done");
        assert!(parse_actions("no json here").is_none());
        let sys = lead_system(&Brief { integration: "oriel/lead-x", base: "master", max_parallel: 3, budget: 5.0, roster: "- codex", shell: "bash" }, false);
        assert!(sys.contains("END EVERY REPLY") && sys.contains("resolve_conflicts") && sys.contains("oriel/lead-x"));
        let a = lead_system(&Brief { integration: "i", base: "b", max_parallel: 3, budget: 5.0, roster: "", shell: "bash" }, true);
        assert!(a.contains("mcp__oriel__") && a.contains("ROUTING"));
        // byte-stable: the shared part comes first and doesn't depend on the run
        let b = lead_system(&Brief { integration: "j", base: "c", max_parallel: 2, budget: 1.0, roster: "- kimi", shell: "bash" }, true);
        let cut = a.find("THIS RUN").unwrap();
        assert_eq!(a[..cut], b[..cut]);
    }

    /// A slow call (wait) sends progress notifications when the client asked for them.
    #[test]
    fn agents_mcp_progress_while_waiting() {
        let rpc = json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "wait", "arguments": {}, "_meta": {"progressToken": "p1"}}}).to_string();
        let mut out = vec![];
        serve_with(rpc.as_bytes(), &mut out, |_, _| {
            std::thread::sleep(Duration::from_millis(250));
            Ok("{\"events\":[]}".into())
        }, Duration::from_millis(60));
        let lines: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let progress = lines.iter().filter(|l| l["method"] == "notifications/progress").count();
        assert!(progress >= 2, "{lines:?}");
        assert_eq!(lines.last().unwrap()["id"], 7, "the answer comes last");
        assert!(lines.iter().all(|l| l["method"] != "notifications/progress" || l["params"]["progressToken"] == "p1"));
    }
}
