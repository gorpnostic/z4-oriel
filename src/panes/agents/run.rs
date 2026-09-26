//! Running an agent headless: the command lines (workers and leads, per agent — minimal launch profiles, see
//! docs/ai-research.md "lead mode"), and a runner that streams the agent's JSON output through stream.rs,
//! enforces a spend cap and can be stopped at any moment. Always on a background thread; child processes never
//! get a console window on Windows.

use super::stream::{Ev, Kind, Parser};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One headless run of an agent.
#[derive(Clone, Debug, Default)]
pub struct Spec {
    /// claude | codex | kimi (picks the stream parser)
    pub agent: String,
    pub prog: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    /// Sent on stdin, then stdin is closed.
    pub stdin: Option<String>,
    /// For pricing streams that don't name the model (codex).
    pub model: String,
    /// Stop the run once its cost passes this (0 = no cap). Claude enforces its own via --max-budget-usd.
    pub budget_usd: f64,
    /// Temp files to delete when the run ends (MCP config, system prompt).
    pub cleanup: Vec<PathBuf>,
    /// What it was asked (fakes read it) and the session it resumes ("" = a new one).
    #[cfg_attr(not(test), allow(dead_code))]
    pub prompt: String,
    pub resume: String,
    /// A session id we chose up front (claude --session-id), so a crash can still resume it.
    pub session: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outcome {
    pub session: String,
    pub cost: f64,
    pub tokens: u64,
    /// The agent's final answer.
    pub text: String,
    /// Its structured closing report {status, summary, questions}, when it gave one.
    pub report: Option<Value>,
    pub error: Option<String>,
    /// Stopped by us (the user, a budget, a take-over, the watchdog), not finished.
    pub stopped: bool,
    pub touched: Vec<String>,
}

/// Tests replace the process with this.
pub type Fake = Arc<dyn Fn(&Spec, &AtomicBool, &mut dyn FnMut(Ev)) -> Outcome + Send + Sync>;

/// Feed events into an outcome as they pass.
pub fn track(o: &mut Outcome, e: &Ev) {
    match e {
        Ev::Session(s) => o.session = s.clone(),
        Ev::Cost(c) => o.cost = *c,
        Ev::Tokens(t) => o.tokens = *t,
        Ev::Final(t) => o.text = t.clone(),
        Ev::Report(r) => o.report = Some(r.clone()),
        Ev::Error(m) => o.error = Some(m.clone()),
        Ev::Touched(f) => {
            if !o.touched.contains(f) {
                o.touched.push(f.clone())
            }
        }
        _ => {}
    }
}

/// A `{status, summary, questions}` report from a final message (JSON, or a ```json block at its end).
pub fn parse_report(text: &str) -> Option<Value> {
    let t = text.trim();
    let body = match t.rfind("```") {
        Some(end) if end > 0 => {
            let start = t[..end].rfind("```")?;
            let b = &t[start + 3..end];
            b.split_once('\n').map(|(_, rest)| rest).unwrap_or(b)
        }
        _ => t,
    };
    let (a, b) = (body.find('{')?, body.rfind('}')?);
    let v: Value = serde_json::from_str(body.get(a..=b)?).ok()?;
    (v["summary"].is_string() || v["status"].is_string()).then_some(v)
}

/// Run it: stream events to `on` until it exits or `stop` is set (then it's killed).
pub fn run(spec: &Spec, stop: &Arc<AtomicBool>, fake: Option<&Fake>, on: &mut dyn FnMut(Ev)) -> Outcome {
    let mut o = Outcome { session: if spec.resume.is_empty() { spec.session.clone() } else { spec.resume.clone() }, ..Default::default() };
    if let Some(f) = fake {
        let mut r = f(spec, stop, &mut |e| {
            track(&mut o, &e);
            on(e);
        });
        cleanup(spec);
        if r.session.is_empty() {
            r.session = o.session.clone();
        }
        for t in o.touched {
            if !r.touched.contains(&t) {
                r.touched.push(t);
            }
        }
        if r.report.is_none() {
            r.report = o.report.or_else(|| parse_report(&r.text));
        }
        return r;
    }
    let r = run_process(spec, stop, &mut o, on);
    cleanup(spec);
    if let Err(e) = r {
        o.error = Some(e);
    }
    if o.report.is_none() {
        o.report = parse_report(&o.text);
    }
    o.stopped = o.stopped || stop.load(Ordering::SeqCst);
    o
}

fn cleanup(spec: &Spec) {
    for f in &spec.cleanup {
        let _ = std::fs::remove_file(f);
    }
}

fn is_shim(bin: &Path) -> bool {
    let low = bin.to_string_lossy().to_lowercase();
    cfg!(windows) && (low.ends_with(".cmd") || low.ends_with(".bat"))
}

/// Program + args for `bin`: `.cmd`/`.bat` shims (npm installs on Windows) go through cmd.exe.
pub fn command_for(bin: &Path, args: &[String]) -> Command {
    let p = bin.to_string_lossy().to_string();
    #[allow(unused_mut)]
    let mut c = if is_shim(bin) {
        let mut c = Command::new("cmd.exe");
        c.arg("/c").arg(&p);
        // cmd.exe can't carry newlines inside an argument
        c.args(args.iter().map(|a| a.replace(['\r', '\n'], " ")));
        c
    } else {
        let mut c = Command::new(&p);
        c.args(args);
        c
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

fn run_process(spec: &Spec, stop: &Arc<AtomicBool>, o: &mut Outcome, on: &mut dyn FnMut(Ev)) -> Result<(), String> {
    let mut cmd = command_for(&spec.prog, &spec.args);
    cmd.current_dir(&spec.cwd).stdin(if spec.stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.env_remove("NO_COLOR").env("CI", "1");
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    let name = spec.prog.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| spec.agent.clone());
    let mut child = cmd.spawn().map_err(|e| format!("couldn't start {name}: {e}"))?;
    if let (Some(mut si), Some(text)) = (child.stdin.take(), spec.stdin.clone()) {
        // a thread, so a huge prompt can't deadlock against a full stdout pipe
        std::thread::spawn(move || {
            let _ = si.write_all(text.as_bytes());
        });
    }
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let stderr = child.stderr.take().ok_or("no stderr")?;
    let err_buf = Arc::new(Mutex::new(String::new()));
    let eb = err_buf.clone();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = BufReader::new(stderr).take(1 << 20).read_to_string(&mut s);
        *eb.lock().unwrap() = s;
    });
    // the killer: stops the child the moment `stop` is set (reading stdout blocks, so it can't check itself)
    let child = Arc::new(Mutex::new(child));
    let finished = Arc::new(AtomicBool::new(false));
    {
        let (child, finished, stop) = (child.clone(), finished.clone(), stop.clone());
        std::thread::spawn(move || {
            while !finished.load(Ordering::SeqCst) {
                if stop.load(Ordering::SeqCst) {
                    kill_tree(&mut child.lock().unwrap());
                    break;
                }
                std::thread::sleep(Duration::from_millis(80));
            }
        });
    }
    // codex reports spend only when a turn ends: watch its rollout file so a budget holds mid-turn
    let thread_id = Arc::new(Mutex::new(String::new()));
    let over: Arc<Mutex<Option<String>>> = Arc::default();
    if spec.agent == "codex" && spec.budget_usd > 0.0 {
        let (tid, over, finished, stop, budget, model) = (thread_id.clone(), over.clone(), finished.clone(), stop.clone(), spec.budget_usd, spec.model.clone());
        std::thread::spawn(move || codex_budget_watch(&tid, &over, &finished, &stop, budget, &model));
    }
    let mut parser = Parser::new(Kind::of(&spec.agent), &spec.cwd, &spec.model);
    let mut got = false;
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
        got = true;
        parser.feed(&v, &mut |e| {
            if let Ev::Session(s) = &e {
                *thread_id.lock().unwrap() = s.clone();
            }
            track(o, &e);
            on(e);
        });
        if spec.budget_usd > 0.0 && parser.cost() > spec.budget_usd && !stop.load(Ordering::SeqCst) {
            let msg = format!("stopped at its budget (${:.2} of ${:.2})", parser.cost(), spec.budget_usd);
            on(Ev::Error(msg.clone()));
            o.error = Some(msg);
            o.stopped = true;
            stop.store(true, Ordering::SeqCst);
        }
    }
    parser.end(&mut |e| {
        track(o, &e);
        on(e);
    });
    finished.store(true, Ordering::SeqCst);
    if let Some(m) = over.lock().unwrap().take() {
        on(Ev::Error(m.clone()));
        o.error = Some(m);
        o.stopped = true;
    }
    let status = {
        let mut c = child.lock().unwrap();
        let _ = c.kill();
        c.wait().ok()
    };
    if !got && !stop.load(Ordering::SeqCst) {
        let err = err_buf.lock().unwrap().clone();
        let tail = err.trim().lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
        let code = status.and_then(|s| s.code()).unwrap_or(-1);
        return Err(if tail.is_empty() { format!("{name} exited with code {code} and no output") } else { tail });
    }
    Ok(())
}

/// Kill the agent and whatever it started (shells, builds). Windows: `taskkill /T`; elsewhere the child.
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &child.id().to_string()]).creation_flags(0x0800_0000).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    let _ = child.kill();
}

/// Poll the codex rollout for this thread (`~/.codex/sessions/**/rollout-*-<thread>.jsonl`) and stop the run
/// when its token count prices past the budget.
fn codex_budget_watch(tid: &Mutex<String>, over: &Mutex<Option<String>>, finished: &AtomicBool, stop: &AtomicBool, budget: f64, model: &str) {
    let Some(root) = std::env::var_os("CODEX_HOME").map(PathBuf::from).or_else(|| dirs::home_dir().map(|h| h.join(".codex"))) else { return };
    let mut file: Option<PathBuf> = None;
    while !finished.load(Ordering::SeqCst) && !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_secs(3));
        let id = tid.lock().unwrap().clone();
        if id.is_empty() {
            continue;
        }
        if file.is_none() {
            file = find_rollout(&root.join("sessions"), &id);
        }
        let Some(f) = &file else { continue };
        let Ok(text) = std::fs::read_to_string(f) else { continue };
        let Some(line) = text.lines().rev().find(|l| l.contains("\"total_token_usage\"")) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let u = &v["payload"]["info"]["total_token_usage"];
        let n = |k: &str| u[k].as_u64().unwrap_or(0) as f64;
        let (pi, pc, po) = super::cost::codex_price(model);
        let cached = n("cached_input_tokens");
        let cost = ((n("input_tokens") - cached).max(0.0) * pi + cached * pc + n("output_tokens") * po) / 1e6;
        if cost > budget {
            *over.lock().unwrap() = Some(format!("stopped at its budget (${cost:.2} of ${budget:.2})"));
            stop.store(true, Ordering::SeqCst);
            return;
        }
    }
}

fn find_rollout(sessions: &Path, id: &str) -> Option<PathBuf> {
    let newest = |d: &Path, n: usize| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(d).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        v.sort();
        v.into_iter().rev().take(n).collect()
    };
    for y in newest(sessions, 1) {
        for m in newest(&y, 2) {
            for d in newest(&m, 2) {
                if let Some(f) = newest(&d, 400).into_iter().find(|f| f.to_string_lossy().contains(id)) {
                    return Some(f);
                }
            }
        }
    }
    None
}

// ------------------------------------------------------------------ command lines

/// The closing report every worker gives (claude --json-schema, codex --output-schema, kimi: a ```json block).
pub const REPORT_SCHEMA: &str = r#"{"type":"object","properties":{"status":{"type":"string","enum":["done","blocked"]},"summary":{"type":"string","description":"what you changed, at most 120 words"},"questions":{"type":"array","items":{"type":"string"}}},"required":["status","summary","questions"],"additionalProperties":false}"#;

/// The part of every worker brief that never changes, so it's cached across workers and tasks.
pub const WORKER_PREAMBLE: &str = "You are a worker agent run by oriel, alone in your own git worktree (a branch made just for this task). Rules:\n\
- Do exactly the task below. Edit ONLY files matching OWNS. If the task can't be done without touching anything else, stop and report status \"blocked\" with your question.\n\
- Edit files directly. Do NOT run git commit, merge, rebase, reset or push: oriel commits your work and merges it.\n\
- Don't start sub-agents. Keep it small and focused.\n\
- If ACCEPTANCE names a command, run it and make it pass. It runs again on the merged result before your work lands.\n\
- Finish with your report: status (done or blocked), a summary of what you changed (at most 120 words) and any questions.\n\n";

/// The task block that follows the preamble.
#[allow(clippy::too_many_arguments)]
pub fn task_block(id: &str, title: &str, goal: &str, owns: &[String], reads: &[String], acceptance: &str, size: &str, history: &[String], shell: &str) -> String {
    let mut s = format!("TASK {id}: {title}\nGOAL:\n{}\nOWNS: {}\n", goal.trim(), if owns.is_empty() { "(only what the goal needs)".to_string() } else { owns.join(", ") });
    if !reads.is_empty() {
        s.push_str(&format!("READ FIRST: {}\n", reads.join(", ")));
    }
    if !acceptance.is_empty() {
        s.push_str(&format!("ACCEPTANCE (runs in {shell} from the repo root): {acceptance}\n"));
    }
    match size {
        "S" => s.push_str("SIZE: S (about 100 lines, up to 3 files)\n"),
        "M" => s.push_str("SIZE: M (about 400 lines, up to 8 files)\n"),
        _ => {}
    }
    if !history.is_empty() {
        s.push_str("EARLIER ATTEMPTS (don't repeat them):\n");
        for h in history {
            s.push_str(&format!("- {h}\n"));
        }
    }
    s
}

/// A random v4 UUID (for claude --session-id).
pub fn uuid() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut b = [0u8; 16];
    for (i, chunk) in b.chunks_mut(8).enumerate() {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_usize(i);
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        chunk.copy_from_slice(&h.finish().to_le_bytes());
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// A worker's command line: `prompt` is the brief (first run) or the follow-up (`resume` = its session).
/// `tmp` holds throwaway files (the report schema for codex).
#[allow(clippy::too_many_arguments)]
pub fn worker_spec(agent: &str, bin: &Path, model: &str, prompt: &str, resume: &str, max_turns: u32, budget: f64, wt: &Path, tmp: &Path) -> Spec {
    let model = model.trim();
    let mut a: Vec<String> = vec![];
    let mut stdin = None;
    let mut env = vec![];
    let mut cleanup = vec![];
    let mut session = String::new();
    match agent {
        "codex" => {
            a.push("exec".into());
            if !resume.is_empty() {
                a.extend(["resume".into(), resume.into(), "-c".into(), "sandbox_mode=workspace-write".into()]);
            } else {
                a.extend(["-C".into(), wt.to_string_lossy().to_string(), "--sandbox".into(), "workspace-write".into()]);
                let _ = std::fs::create_dir_all(tmp);
                let schema = tmp.join(format!("oriel-report-{}.json", uuid()));
                if std::fs::write(&schema, REPORT_SCHEMA).is_ok() {
                    a.extend(["--output-schema".into(), schema.to_string_lossy().to_string()]);
                    cleanup.push(schema);
                }
            }
            // minimal profile: no user config (MCP servers, profiles), no nested agents
            a.extend(["--json".into(), "--skip-git-repo-check".into(), "--ignore-user-config".into(), "-c".into(), "agents.enabled=false".into()]);
            if !model.is_empty() {
                a.extend(["-m".into(), model.into()]);
            }
            a.push("-".into());
            stdin = Some(prompt.to_string());
        }
        "kimi" => {
            // prompt mode takes the prompt as an argument and runs with auto permissions
            if !resume.is_empty() {
                a.extend(["-S".into(), resume.into()]);
            }
            if !model.is_empty() {
                a.extend(["-m".into(), model.into()]);
            }
            let p = if resume.is_empty() { format!("{prompt}\n\nEnd with your report as a ```json block: {REPORT_SCHEMA}") } else { prompt.to_string() };
            a.extend(["-p".into(), p, "--output-format".into(), "stream-json".into()]);
            env.push(("KIMI_CODE_AGENT_SWARM_MAX_CONCURRENCY".to_string(), "2".to_string()));
        }
        _ => {
            a.extend(["-p", "--output-format", "stream-json", "--verbose", "--permission-mode", "acceptEdits", "--strict-mcp-config", "--exclude-dynamic-system-prompt-sections"].map(String::from));
            // git stays oriel's job, pushes are never allowed, and no nested fan-out
            a.extend(["--disallowedTools".into(), "Agent,Task,Bash(git push:*),Bash(git commit:*),Bash(git merge:*),Bash(git rebase:*),Bash(git reset:*)".into()]);
            if !is_shim(bin) {
                // cmd.exe can't carry the quotes in a JSON argument
                a.extend(["--json-schema".into(), REPORT_SCHEMA.into()]);
            }
            if !model.is_empty() {
                a.extend(["--model".into(), model.into()]);
            }
            if max_turns > 0 {
                a.extend(["--max-turns".into(), max_turns.to_string()]);
            }
            if budget > 0.0 {
                a.extend(["--max-budget-usd".into(), format!("{budget:.2}")]);
            }
            if resume.is_empty() {
                session = uuid();
                a.extend(["--session-id".into(), session.clone()]);
            } else {
                a.extend(["--resume".into(), resume.into()]);
            }
            stdin = Some(prompt.to_string());
        }
    }
    Spec {
        agent: agent.into(),
        prog: bin.to_path_buf(),
        args: a,
        cwd: wt.to_path_buf(),
        env,
        stdin,
        model: model.into(),
        // claude stops itself at --max-budget-usd; the others are stopped by the runner
        budget_usd: if agent == "claude" { 0.0 } else { budget },
        cleanup,
        prompt: prompt.into(),
        resume: resume.into(),
        session,
    }
}

/// How a lead talks to oriel.
#[derive(Clone, Debug, PartialEq)]
pub enum Proto {
    /// oriel's MCP server: (oriel binary, broker port, token)
    Mcp(PathBuf, u16, String),
    /// JSON actions in its replies
    Text,
}

/// Can this agent's CLI load oriel's MCP server for one run without touching the user's own config?
/// claude: --mcp-config + --strict-mcp-config · codex: `-c mcp_servers.<name>.*` overrides · kimi: a project
/// `.kimi-code/mcp.json` in the lead's own checkout. Anything else leads through the text protocol, and a lead
/// whose tools never reach oriel falls back to it too.
pub fn supports_mcp(agent: &str) -> bool {
    matches!(agent, "claude" | "codex" | "kimi")
}

/// oriel's lead tools as Claude Code names them.
pub fn claude_tool_names() -> String {
    super::mcp::TOOLS.iter().map(|t| format!("mcp__oriel__{}", t.0)).collect::<Vec<_>>().join(",")
}

/// A lead turn's command line. `system` = oriel's lead instructions; `prompt` = the goal (first turn) or the
/// results of its last actions (text protocol, `resume` = its session). Kimi's MCP file is written by the caller.
#[allow(clippy::too_many_arguments)]
pub fn lead_spec(agent: &str, bin: &Path, model: &str, prompt: &str, system: &str, resume: &str, proto: &Proto, budget: f64, cwd: &Path, tmp: &Path) -> Spec {
    let model = model.trim();
    let mut a: Vec<String> = vec![];
    let mut cleanup = vec![];
    let mut env = vec![];
    let mut stdin = None;
    let _ = std::fs::create_dir_all(tmp);
    let tag = uuid();
    // agents without a system-prompt flag get the instructions at the top of their first prompt
    let with_system = |p: &str| if resume.is_empty() { format!("{system}\n\n---\n\n{p}") } else { p.to_string() };
    match agent {
        "codex" => {
            a.push("exec".into());
            if !resume.is_empty() {
                a.extend(["resume".into(), resume.into(), "-c".into(), "sandbox_mode=read-only".into()]);
            } else {
                a.extend(["-C".into(), cwd.to_string_lossy().to_string(), "--sandbox".into(), "read-only".into()]);
            }
            a.extend(["--json".into(), "--skip-git-repo-check".into(), "--ignore-user-config".into(), "-c".into(), "agents.enabled=false".into()]);
            if let Proto::Mcp(exe, port, token) = proto {
                // TOML literal strings (single quotes) survive cmd.exe and need no escaping
                let exe = exe.to_string_lossy().replace('\\', "/");
                for kv in [
                    format!("mcp_servers.oriel.command='{exe}'"),
                    format!("mcp_servers.oriel.args=['mcp-lead','{port}','{token}']"),
                    "mcp_servers.oriel.required=true".into(),
                    "mcp_servers.oriel.startup_timeout_sec=30".into(),
                    "mcp_servers.oriel.tool_timeout_sec=1800".into(),
                    "mcp_servers.oriel.default_tools_approval_mode='approve'".into(),
                ] {
                    a.extend(["-c".into(), kv]);
                }
            }
            if !model.is_empty() {
                a.extend(["-m".into(), model.into()]);
            }
            a.push("-".into());
            stdin = Some(with_system(prompt));
        }
        "kimi" => {
            if !resume.is_empty() {
                a.extend(["-S".into(), resume.into()]);
            }
            if !model.is_empty() {
                a.extend(["-m".into(), model.into()]);
            }
            a.extend(["-p".into(), with_system(prompt), "--output-format".into(), "stream-json".into()]);
            env.push(("KIMI_CODE_AGENT_SWARM_MAX_CONCURRENCY".to_string(), "2".to_string()));
        }
        _ => {
            a.extend(["-p", "--output-format", "stream-json", "--verbose", "--permission-mode", "default", "--exclude-dynamic-system-prompt-sections"].map(String::from));
            // the lead reads code and calls oriel; it never edits, runs commands or starts sub-agents
            let mut allowed = "Read,Grep,Glob,LS".to_string();
            if let Proto::Mcp(exe, port, token) = proto {
                let cfg = serde_json::json!({"mcpServers": {"oriel": {"command": exe.to_string_lossy(), "args": ["mcp-lead", port.to_string(), token]}}});
                let path = tmp.join(format!("oriel-lead-mcp-{tag}.json"));
                let _ = std::fs::write(&path, cfg.to_string());
                a.extend(["--mcp-config".into(), path.to_string_lossy().to_string()]);
                cleanup.push(path);
                allowed = format!("{allowed},{}", claude_tool_names());
                // wait long-polls for up to 10 minutes and merges run the gate
                env.push(("MCP_TOOL_TIMEOUT".to_string(), "1800000".to_string()));
                // load oriel's dozen tools up front instead of a ToolSearch round trip per tool
                env.push(("ENABLE_TOOL_SEARCH".to_string(), "false".to_string()));
            }
            a.push("--strict-mcp-config".into());
            a.extend(["--allowedTools".into(), allowed]);
            a.extend(["--disallowedTools".into(), "Edit,Write,MultiEdit,NotebookEdit,Bash,PowerShell,Agent,Task".into()]);
            let sp = tmp.join(format!("oriel-lead-prompt-{tag}.md"));
            if std::fs::write(&sp, system).is_ok() {
                a.extend(["--append-system-prompt-file".into(), sp.to_string_lossy().to_string()]);
                cleanup.push(sp);
            }
            if !model.is_empty() {
                a.extend(["--model".into(), model.into()]);
            }
            if budget > 0.0 {
                a.extend(["--max-budget-usd".into(), format!("{budget:.2}")]);
            }
            if !resume.is_empty() {
                a.extend(["--resume".into(), resume.into()]);
            }
            stdin = Some(prompt.to_string());
        }
    }
    Spec {
        agent: agent.into(),
        prog: bin.to_path_buf(),
        args: a,
        cwd: cwd.to_path_buf(),
        env,
        stdin,
        model: model.into(),
        budget_usd: if agent == "claude" { 0.0 } else { budget },
        cleanup,
        prompt: prompt.into(),
        resume: resume.into(),
        session: String::new(),
    }
}

/// Take over a headless task in a terminal tab: resume its session interactively.
pub fn takeover_args(agent: &str, model: &str, session: &str) -> Vec<String> {
    let model = model.trim();
    let mut a: Vec<String> = vec![];
    match agent {
        "codex" => {
            a.push("resume".into());
            if session.is_empty() {
                a.push("--last".into());
            } else {
                a.push(session.into());
            }
            if !model.is_empty() {
                a.extend(["-m".into(), model.into()]);
            }
        }
        "kimi" => {
            if session.is_empty() {
                a.push("-c".into());
            } else {
                a.extend(["-S".into(), session.into()]);
            }
            if !model.is_empty() {
                a.extend(["-m".into(), model.into()]);
            }
        }
        _ => {
            if !model.is_empty() {
                a.extend(["--model".into(), model.into()]);
            }
            if session.is_empty() {
                a.push("--continue".into());
            } else {
                a.extend(["--resume".into(), session.into()]);
            }
        }
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_run_command_lines() {
        let wt = Path::new("/w/t");
        let tmp = std::env::temp_dir().join(format!("oriel-lead-args-{}", std::process::id()));
        let s = worker_spec("claude", Path::new("claude"), "haiku", "do it", "", 20, 0.5, wt, &tmp);
        for pair in [["--permission-mode", "acceptEdits"], ["--max-turns", "20"], ["--max-budget-usd", "0.50"]] {
            assert!(s.args.windows(2).any(|w| w == pair), "{pair:?} in {:?}", s.args);
        }
        assert!(s.args.iter().any(|a| a == "--exclude-dynamic-system-prompt-sections") && s.args.iter().any(|a| a == "--strict-mcp-config"));
        assert!(s.args.iter().any(|a| a.starts_with("Agent,")), "no nested fan-out");
        assert_eq!(s.session.len(), 36, "a session id chosen up front");
        assert!(s.args.windows(2).any(|w| w[0] == "--session-id" && w[1] == s.session));
        assert_eq!(s.stdin.as_deref(), Some("do it"));
        let s = worker_spec("claude", Path::new("claude"), "", "more", "S1", 0, 0.0, wt, &tmp);
        assert!(s.args.windows(2).any(|w| w == ["--resume", "S1"]) && !s.args.iter().any(|a| a == "--max-turns" || a == "--session-id"));
        let c = worker_spec("codex", Path::new("codex"), "", "go", "", 0, 1.0, wt, &tmp);
        assert_eq!(&c.args[..5], &["exec", "-C", "/w/t", "--sandbox", "workspace-write"].map(String::from));
        assert!(c.args.iter().any(|a| a == "--ignore-user-config") && c.args.iter().any(|a| a == "--output-schema"));
        assert_eq!(c.budget_usd, 1.0, "the runner enforces codex's budget");
        cleanup(&c);
        let c = worker_spec("codex", Path::new("codex"), "", "go", "T1", 0, 0.0, wt, &tmp);
        assert_eq!(&c.args[..3], &["exec", "resume", "T1"].map(String::from));
        let k = worker_spec("kimi", Path::new("kimi"), "", "go", "K1", 0, 0.0, wt, &tmp);
        assert_eq!(k.args, ["-S", "K1", "-p", "go", "--output-format", "stream-json"].map(String::from));
        assert!(k.env.iter().any(|(e, v)| e == "KIMI_CODE_AGENT_SWARM_MAX_CONCURRENCY" && v == "2"));

        let mcp = Proto::Mcp(PathBuf::from("/bin/oriel"), 4242, "tok".into());
        let l = lead_spec("claude", Path::new("claude"), "opus", "goal", "SYSTEM", "", &mcp, 2.0, wt, &tmp);
        assert!(l.args.iter().any(|a| a == "--strict-mcp-config") && l.args.iter().any(|a| a.contains("mcp__oriel__plan")));
        assert!(l.args.iter().any(|a| a.contains("Edit,Write")), "the lead can't edit");
        let cfg = std::fs::read_to_string(&l.cleanup[0]).unwrap();
        assert!(cfg.contains("mcp-lead") && cfg.contains("4242"), "{cfg}");
        assert!(l.env.iter().any(|(k, _)| k == "MCP_TOOL_TIMEOUT"));
        cleanup(&l);
        let cx = lead_spec("codex", Path::new("codex"), "", "goal", "SYSTEM", "", &mcp, 2.0, wt, &tmp);
        assert!(cx.args.iter().any(|a| a == "mcp_servers.oriel.args=['mcp-lead','4242','tok']"), "{:?}", cx.args);
        assert!(cx.args.iter().any(|a| a == "mcp_servers.oriel.tool_timeout_sec=1800"));
        assert!(cx.stdin.as_deref().unwrap().starts_with("SYSTEM"));
        let kt = lead_spec("kimi", Path::new("kimi"), "", "results", "SYSTEM", "K1", &Proto::Text, 0.0, wt, &tmp);
        assert_eq!(kt.args[..3], ["-S", "K1", "-p"].map(String::from));
        assert_eq!(kt.args[3], "results", "a resumed turn doesn't repeat the instructions");
        assert!(supports_mcp("kimi") && supports_mcp("codex") && !supports_mcp("gemini"));
        assert_eq!(takeover_args("claude", "", "S1"), vec!["--resume", "S1"]);
        assert_eq!(takeover_args("codex", "", "T1"), vec!["resume", "T1"]);
        let _ = std::fs::remove_dir_all(&tmp);
        // reports: plain JSON or a fenced block at the end
        assert_eq!(parse_report(r#"{"status":"done","summary":"x","questions":[]}"#).unwrap()["summary"], "x");
        assert_eq!(parse_report("did it\n```json\n{\"status\":\"blocked\",\"summary\":\"y\",\"questions\":[\"why?\"]}\n```").unwrap()["status"], "blocked");
        assert!(parse_report("no report").is_none());
        assert_ne!(uuid(), uuid());
    }
}
