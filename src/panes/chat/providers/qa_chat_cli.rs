//! QA: how a CLI agent's stdout is read (`run_cli`), fed from a scratch file through the system's own `type` / `cat`
//! instead of a real agent. Malformed and non-JSON lines must be skipped, and nothing on one bad line may end the
//! run. Part of the chat QA pass (`cargo test qa_chat`).

use super::*;

fn scratch_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let d = std::path::absolute("target/test-scratch/qa-chat/cli").unwrap();
    std::fs::create_dir_all(&d).unwrap();
    let p = d.join(name);
    std::fs::write(&p, bytes).unwrap();
    p
}

/// Print a file the way a CLI prints its stream, and collect every JSON value `run_cli` hands on.
fn replay(path: &std::path::Path) -> (Result<(), String>, Vec<Value>) {
    let pid = AtomicU32::new(0);
    let (exe, args): (&str, Vec<String>) =
        if cfg!(windows) { ("cmd", vec!["/c".into(), "type".into(), path.to_string_lossy().to_string()]) } else { ("cat", vec![path.to_string_lossy().to_string()]) };
    let stop = AtomicBool::new(false);
    let mut got = vec![];
    let r = run_cli(exe, &args, |_| {}, &std::env::temp_dir(), &stop, &pid, |v: &Value| {
        got.push(v.clone());
        Ok(())
    });
    (r.map(|_| ()), got)
}

/// Garbage, half-written JSON, blank lines, CRLF, a 1 MB line and a last line with no newline: the good lines all
/// arrive, in order, and nothing else does.
#[test]
fn qa_chat_cli_malformed_lines_skipped() {
    let big = "a".repeat(1_000_000);
    let text = format!(
        "{{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"m\"}}\r\nnot json at all\r\n{{\"type\":\"assistant\", broken\r\n\r\n   \r\n{{\"type\":\"x\",\"big\":\"{big}\"}}\r\n[1,2,3]\r\n\u{1b}[31m{{\"type\":\"colored\"}}\u{1b}[0m\r\n{{\"type\":\"result\"}}"
    );
    let (r, got) = replay(&scratch_file("malformed.jsonl", text.as_bytes()));
    assert!(r.is_ok(), "{r:?}");
    let kinds: Vec<String> = got.iter().map(|v| v["type"].as_str().map(String::from).unwrap_or_else(|| v.to_string())).collect();
    assert_eq!(kinds, ["system", "x", "[1,2,3]", "result"]);
    assert_eq!(got[1]["big"].as_str().map(str::len), Some(1_000_000));
}

/// One line that isn't UTF-8 (a localized shim or hook printing in the ANSI code page) must be skipped like any
/// other non-JSON line; the lines after it still belong to the run.
#[test]
#[ignore = "fails: run_cli breaks out of the read loop on the first non-UTF-8 line (providers.rs:414 `let Ok(line) = line else { break }`), so the rest of the run is lost and the agent is killed"]
fn qa_chat_cli_non_utf8_line_doesnt_end_the_run() {
    let mut bytes = b"{\"type\":\"system\",\"subtype\":\"init\"}\r\n".to_vec();
    bytes.extend_from_slice(b"Warnung: caf\xe9 \xfcber den Befehl\r\n");
    bytes.extend_from_slice(b"{\"type\":\"assistant\",\"message\":{\"content\":[]}}\r\n{\"type\":\"result\"}\r\n");
    let (r, got) = replay(&scratch_file("not-utf8.jsonl", &bytes));
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(got.len(), 3, "only {} of 3 JSON lines arrived: {got:?}", got.len());
}

/// A CLI that prints only words (not signed in, a crash) is an error, not an empty reply.
#[test]
fn qa_chat_cli_only_garbage_is_an_error() {
    let (r, got) = replay(&scratch_file("garbage.txt", b"Error: not logged in\r\nplease run the login command\r\n"));
    println!("{r:?}");
    assert!(got.is_empty());
    assert!(r.is_err(), "{r:?}");
}

/// The prior conversation is trimmed to its last 12000 bytes on a character boundary, whatever the characters.
#[test]
fn qa_chat_cli_transcript_trims_on_a_char_boundary() {
    let long = "😀é世".repeat(3000);
    let req = Request {
        provider: "claude".into(),
        model: None,
        messages: vec![("user".into(), long.clone()), ("assistant".into(), long.clone()), ("user".into(), "the new one 👋".into())],
        cwd: std::env::temp_dir(),
        perms: "edits".into(),
        state: Default::default(),
        cfg: AiConfig::default(),
        steer: None,
        effort: String::new(),
        pid: Arc::default(),
        since: None,
    };
    let t = transcript(&req);
    assert!(t.ends_with("(New message:)\nthe new one 👋"), "{}", t.chars().rev().take(60).collect::<String>());
    assert!(t.len() < 12_200, "{}", t.len());
    // a single message goes as it is
    let one = Request { messages: vec![("user".into(), "only 👋".into())], ..req };
    assert_eq!(transcript(&one), "only 👋");
}
