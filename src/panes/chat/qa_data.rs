//! QA: saved chats (store.rs) that are corrupt, odd or hostile. load_all must skip what it can't read without
//! panicking, the pane must render whatever did load, and deleting / saving must stay inside the chats folder.
//!
//! store::dir() hangs off data_dir(), so every test that touches disk runs as a child process of this test binary
//! with ORIEL_DATA_DIR in target/test-scratch/qa-data (see crate::qa_data::run_child). Tests that document a real
//! bug are `#[ignore = "fails: ..."]`; run them with `cargo test qa_chat -- --ignored`.

use super::*;
use crate::qa_data::{ReadOnly, child_ok, controls, corrupt_json, is_child, nasty};
use crate::testkit::Kit;
use serde_json::{Value, json};

/// A kit whose AI config never probes the network (ollama on a closed loopback port).
fn kit() -> Kit {
    let mut k = Kit::new();
    k.config.ai.ollama_url = "http://127.0.0.1:9".into();
    k.config.ai.openai_url = "https://api.openai.com/v1".into();
    k
}

fn chat_json(id: &str, title: &str, updated: f64, messages: Vec<Value>) -> Value {
    json!({"id": id, "title": title, "created": 1_700_000_000.0, "updated": updated, "messages": messages, "provider": "claude"})
}

fn user(text: &str) -> Value {
    json!({"role": "user", "content": text})
}

fn reply(text: &str, parts: Vec<Value>) -> Value {
    json!({"role": "assistant", "content": text, "model": "claude", "parts": parts})
}

fn tool(id: &str, name: &str, label: &str, target: &str, status: &str, body: Vec<String>) -> Value {
    json!({"kind": "tool", "id": id, "name": name, "label": label, "target": target, "status": status, "body": body})
}

fn write_chat(file: &str, v: &Value) -> PathBuf {
    let d = store::dir();
    std::fs::create_dir_all(&d).unwrap();
    let p = d.join(file);
    std::fs::write(&p, serde_json::to_string_pretty(v).unwrap()).unwrap();
    p
}

/// Render the pane the ways the app does: main area at a few sizes, sidebar, then everything expanded (ctrl+o).
fn render_all(k: &mut Kit, c: &mut Chat) -> String {
    let mut all = String::new();
    for (w, h) in [(150, 44), (100, 30), (60, 16), (24, 8)] {
        all.push_str(&k.render(c, w, h));
    }
    all.push_str(&k.render_side(c, 34, 30));
    c.expanded = true;
    for (w, h) in [(150, 44), (60, 16)] {
        all.push_str(&k.render(c, w, h));
    }
    c.expanded = false;
    all
}

// ------------------------------------------------------------------ corrupt files in the chats folder

const CHILD_CORRUPT: &str = "panes::chat::qa_data::child_chat_corrupt_files";
#[test]
fn child_chat_corrupt_files() {
    if !is_child(CHILD_CORRUPT) {
        return;
    }
    let d = store::dir();
    std::fs::create_dir_all(&d).unwrap();
    let good = write_chat("good1.json", &chat_json("good1", "the good one", store::now(), vec![user("hi"), reply("hello", vec![])]));
    let mut files: Vec<(PathBuf, Vec<u8>)> = vec![];
    for (i, bytes) in corrupt_json().into_iter().enumerate() {
        let p = d.join(format!("c{i}.json"));
        std::fs::write(&p, &bytes).unwrap();
        files.push((p, bytes));
    }
    // right shape, wrong types / missing fields
    for (i, s) in [
        r#"{"id":"w1","title":"t","created":"yesterday","updated":1,"messages":[]}"#,
        r#"{"id":"w2","title":"t","created":1,"updated":1,"messages":{"role":"user"}}"#,
        r#"{"id":"w3","title":"t","created":1,"updated":1,"messages":[{"content":"no role"}]}"#,
        r#"{"id":"w4","title":"t","created":1,"updated":1,"messages":[{"role":"user","content":null}]}"#,
        r#"{"id":7,"title":"t","created":1,"updated":1,"messages":[]}"#,
        r#"{"id":"w6","title":["t"],"created":1,"updated":1,"messages":[]}"#,
        r#"{"id":"w7","title":"t","created":1,"updated":1e999,"messages":[]}"#,
        r#"{"id":"w8","title":"t","created":1,"updated":1,"messages":[],"state":[1,2]}"#,
    ]
    .iter()
    .enumerate()
    {
        let p = d.join(format!("w{i}.json"));
        std::fs::write(&p, s).unwrap();
        files.push((p, s.as_bytes().to_vec()));
    }
    // not chats at all: a leftover temp file, a folder called *.json
    std::fs::write(d.join("x.json.tmp"), serde_json::to_string(&chat_json("tmp1", "temp", 1.0, vec![user("x")])).unwrap()).unwrap();
    std::fs::create_dir_all(d.join("folder.json")).unwrap();

    let loaded = store::load_all();
    let ids: Vec<&str> = loaded.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, vec!["good1"], "only the good chat loads");

    let mut k = kit();
    let mut c = Chat::new(&k.config);
    assert_eq!(c.chats.len(), 1);
    render_all(&mut k, &mut c);
    c.open_chat(0);
    let s = render_all(&mut k, &mut c);
    assert!(s.contains("the good one") || s.contains("hello"), "{s}");
    c.new_chat();
    // nothing was changed, so nothing on disk changed
    for (p, bytes) in &files {
        assert_eq!(&std::fs::read(p).unwrap(), bytes, "{} was rewritten", p.display());
    }
    assert!(good.exists());
}

#[test]
fn qa_chat_corrupt_files_are_skipped() {
    child_ok(CHILD_CORRUPT, "chat-corrupt");
}

const CHILD_FOLDER: &str = "panes::chat::qa_data::child_chat_folder_missing_or_a_file";
#[test]
fn child_chat_folder_missing_or_a_file() {
    if !is_child(CHILD_FOLDER) {
        return;
    }
    // no chats folder at all
    let d = store::dir();
    let _ = std::fs::remove_dir_all(&d);
    assert!(store::load_all().is_empty());
    let mut k = kit();
    let mut c = Chat::new(&k.config);
    render_all(&mut k, &mut c);
    // a file where the folder should be: loads nothing, saving fails without a panic
    std::fs::write(&d, "i am a file").unwrap();
    assert!(store::load_all().is_empty());
    c.chat.messages.push(store::Msg { role: "user".into(), content: "hi".into(), ..Default::default() });
    c.persist();
    render_all(&mut k, &mut c);
    assert_eq!(std::fs::read_to_string(&d).unwrap(), "i am a file");
    // an empty chat is never written
    std::fs::remove_file(&d).unwrap();
    let mut empty = store::Chat::new("claude");
    store::save(&mut empty);
    assert!(!d.join(format!("{}.json", empty.id)).exists());
}

#[test]
fn qa_chat_folder_missing_or_a_file() {
    child_ok(CHILD_FOLDER, "chat-folder");
}

// ------------------------------------------------------------------ timestamps

#[test]
fn qa_chat_bucket_edges() {
    let now = 1_790_000_000.0;
    assert_eq!(store::bucket(now, now, 0), "today");
    assert_eq!(store::bucket(now + 1e9, now, 0), "today", "a chat from the future");
    assert_eq!(store::bucket(now - 86_400.0, now, 0), "yesterday");
    assert_eq!(store::bucket(0.0, now, -25_200), "older");
    assert_eq!(store::bucket(-1.0, now, 3600), "older");
    assert_eq!(store::bucket(f64::NAN, now, 0), "older");
}

#[test]
#[ignore = "fails: a chat file with a huge 'updated' (1e300 / -1e300) panics in store::bucket with 'attempt to add with overflow' (debug builds)"]
fn qa_chat_bucket_huge_timestamps() {
    let now = store::now();
    let _ = store::bucket(1e300, now, 3600); // east of UTC
    let _ = store::bucket(-1e300, now, -25_200); // west of UTC
}

const CHILD_TS: &str = "panes::chat::qa_data::child_chat_sidebar_huge_timestamps";
#[test]
fn child_chat_sidebar_huge_timestamps() {
    if !is_child(CHILD_TS) {
        return;
    }
    write_chat("big.json", &chat_json("big", "from the far future", 1e300, vec![user("x")]));
    write_chat("small.json", &chat_json("small", "from before time", -1e300, vec![user("x")]));
    write_chat("ok.json", &chat_json("ok", "normal", store::now(), vec![user("x")]));
    let mut k = kit();
    let mut c = Chat::new(&k.config);
    assert_eq!(c.chats.len(), 3);
    for off in [3600, -25_200, 0] {
        c.offset = off;
        k.render_side(&mut c, 34, 20);
    }
}

#[test]
#[ignore = "fails: one chat file with updated = 1e300 or -1e300 crashes the chat sidebar on every start (overflow in store::bucket, debug builds)"]
fn qa_chat_sidebar_huge_timestamps() {
    child_ok(CHILD_TS, "chat-ts");
}

// ------------------------------------------------------------------ odd transcripts

/// Tool bodies are "kind, line number, tab, text" (agent::line). A hand-edited or damaged file can hold anything.
fn odd_bodies() -> Vec<String> {
    vec![
        "+1\tfine".into(),
        "é1\tmultibyte kind".into(),
        "🎉\temoji kind".into(),
        "+abc\tnot a number".into(),
        format!("+{}\thuge number", "9".repeat(5000)),
        "+\t".into(),
        "\t".into(),
        "+1".into(),
        "-".into(),
        "@".into(),
        " 1\t\t\ttabs".into(),
        "-2\tmoved 🎉 here".into(),
        "+2\tmoved 🎉 there".into(),
        "-3\tموعد".into(),
        "+3\tמועד".into(),
        format!("+4\t{}", "x".repeat(20_000)),
        "!\terror \u{1b}[31mred".into(),
        ">\toutput\u{7}\r".into(),
    ]
}

const CHILD_BODIES: &str = "panes::chat::qa_data::child_chat_odd_tool_bodies";
#[test]
fn child_chat_odd_tool_bodies() {
    if !is_child(CHILD_BODIES) {
        return;
    }
    let parts = vec![
        tool("t1", "Edit", "Update", "src/a.rs", "done", odd_bodies()),
        tool("t2", "Bash", "Bash", "cargo test", "error", odd_bodies()),
        tool("t3", "Write", "Write", "new.rs", "done", odd_bodies()),
        tool("t4", "Read", "Read", "a.rs", "weird-status", odd_bodies()),
    ];
    write_chat("odd.json", &chat_json("odd", "odd bodies", store::now(), vec![user("go"), reply("done", parts)]));
    let mut k = kit();
    let mut c = Chat::new(&k.config);
    assert_eq!(c.chats.len(), 1, "the chat loads");
    c.open_chat(0);
    let s = render_all(&mut k, &mut c);
    let bad = controls(&s);
    assert!(bad.is_empty(), "control characters from a saved tool body reached the screen: {bad:?}");
}

#[test]
fn qa_chat_odd_tool_bodies() {
    child_ok(CHILD_BODIES, "chat-bodies");
}

const CHILD_EMPTY_BODY: &str = "panes::chat::qa_data::child_chat_empty_tool_body_line";
#[test]
fn child_chat_empty_tool_body_line() {
    if !is_child(CHILD_EMPTY_BODY) {
        return;
    }
    let parts = vec![tool("t1", "Edit", "Update", "src/a.rs", "done", vec!["-1\told".into(), "".into(), "+1\tnew".into()])];
    write_chat("empty-line.json", &chat_json("emptyline", "an empty body line", store::now(), vec![user("go"), reply("done", parts)]));
    let mut k = kit();
    let mut c = Chat::new(&k.config);
    c.open_chat(0);
    render_all(&mut k, &mut c);
}

#[test]
#[ignore = "fails: a saved tool call whose body has an empty line panics in agent::split_line (byte index 1 out of range) as soon as the chat is opened, release builds too"]
fn qa_chat_empty_tool_body_line() {
    child_ok(CHILD_EMPTY_BODY, "chat-empty-body");
}

const CHILD_NASTY: &str = "panes::chat::qa_data::child_chat_nasty_text";
#[test]
fn child_chat_nasty_text() {
    if !is_child(CHILD_NASTY) {
        return;
    }
    let mut strings = nasty();
    strings.extend(["tab\there".to_string(), "line\nbreak".into(), "cr\rhere".into(), "esc \u{1b}[31mred\u{1b}[0m".into(), "bell\u{7}".into(), "nul\u{0}".into(), "osc \u{1b}]0;pwned\u{7}".into()]);
    let now = store::now();
    for (i, s) in strings.iter().enumerate() {
        // nest a subagent's calls 30 deep
        let mut deep = tool(&format!("leaf{i}"), "Read", "Read", s, "done", vec![format!(">\t{s}")]);
        for d in 0..30 {
            let mut t = tool(&format!("n{i}-{d}"), "Agent", "Agent", s, "running", vec![]);
            t["children"] = json!([deep]);
            deep = t;
        }
        let todos: Vec<Value> = (0..300).map(|n| { let st = ["pending", "in_progress", "completed", "mystery", ""][n % 5]; json!({"text": format!("{s} {n}"), "status": st, "active": s}) }).collect();
        let parts = vec![
            json!({"kind": "text", "text": s}),
            json!({"kind": "thinking", "text": s, "tokens": u64::MAX}),
            {
                let mut t = tool(&format!("t{i}"), s, s, s, s, vec![format!("+1\t{s}"), format!("-1\t{s}")]);
                t["ms"] = json!(u64::MAX);
                t["summary"] = json!(s);
                t
            },
            deep,
            json!({"kind": "todos", "items": todos}),
            json!({"kind": "user", "text": s}),
            json!({"kind": "mark", "text": s}),
        ];
        let role = ["user", "assistant", "system", "", "tool", "🎉"][i % 6];
        let msgs = vec![
            user(s),
            json!({"role": "assistant", "content": s, "model": s, "note": s, "steps": [[s, "done", s], [s, "mystery", null]], "parts": parts}),
            json!({"role": role, "content": s}),
        ];
        let mut cj = chat_json(&format!("n{i}"), s, now - i as f64 * 40_000.0, msgs);
        cj["model"] = json!(s);
        cj["cwd"] = json!(s);
        let prov = ["claude", "codex", "someai", "", "🎉"][i % 5];
        cj["provider"] = json!(prov);
        write_chat(&format!("n{i}.json"), &cj);
    }
    let mut k = kit();
    let mut c = Chat::new(&k.config);
    assert_eq!(c.chats.len(), strings.len(), "every chat loads");
    let side = k.render_side(&mut c, 34, 40);
    let bad = controls(&side);
    assert!(bad.is_empty(), "control characters from chat titles reached the sidebar: {bad:?}\n{side}");
    for i in 0..c.chats.len() {
        c.open_chat(i);
        let s = render_all(&mut k, &mut c);
        let bad = controls(&s);
        assert!(bad.is_empty(), "control characters from a saved chat reached the screen (chat {i}): {bad:?}");
        k.key_mod(&mut c, KeyCode::PageUp, KeyModifiers::NONE);
        k.key_mod(&mut c, KeyCode::Up, KeyModifiers::CONTROL);
        k.render(&mut c, 120, 30);
    }
}

#[test]
fn qa_chat_nasty_text() {
    child_ok(CHILD_NASTY, "chat-nasty");
}

const CHILD_UNKNOWN_PART: &str = "panes::chat::qa_data::child_chat_unknown_part_kind";
#[test]
fn child_chat_unknown_part_kind() {
    if !is_child(CHILD_UNKNOWN_PART) {
        return;
    }
    // a chat written by a newer oriel (then `oriel rollback`): one part of a kind this version doesn't know
    let parts = vec![json!({"kind": "text", "text": "the answer"}), json!({"kind": "image", "path": "shot.png"})];
    write_chat("newer.json", &chat_json("newer", "a long conversation", store::now(), vec![user("q"), reply("the answer", parts)]));
    let loaded = store::load_all();
    assert_eq!(loaded.len(), 1, "a chat with one unknown part still shows up (loaded {} chats)", loaded.len());
}

#[test]
#[ignore = "fails: one transcript part of an unknown kind (a newer oriel's, after rollback) hides the whole chat from the list"]
fn qa_chat_unknown_part_kind() {
    child_ok(CHILD_UNKNOWN_PART, "chat-part");
}

// ------------------------------------------------------------------ delete and save go by id

fn open_by_id(c: &mut Chat, id: &str) {
    let i = c.chats.iter().position(|x| x.id == id).unwrap();
    c.open_chat(i);
}

fn delete_open_chat(k: &mut Kit, c: &mut Chat) {
    k.key_mod(c, KeyCode::Char('d'), KeyModifiers::CONTROL);
    k.key(c, KeyCode::Char('y'));
}

const CHILD_DEL_NAME: &str = "panes::chat::qa_data::child_chat_delete_renamed_file";
#[test]
fn child_chat_delete_renamed_file() {
    if !is_child(CHILD_DEL_NAME) {
        return;
    }
    // a chat file whose name isn't its id (copied, renamed or synced as "abc (1).json")
    let p = write_chat("abc123 (1).json", &chat_json("abc123", "renamed file", store::now(), vec![user("hi")]));
    let mut k = kit();
    let mut c = Chat::new(&k.config);
    open_by_id(&mut c, "abc123");
    delete_open_chat(&mut k, &mut c);
    assert!(k.notices().iter().any(|n| n.contains("deleted")), "{:?}", k.notices());
    let back = store::load_all();
    assert!(!p.exists() && back.is_empty(), "'chat deleted', but the file is still there ({}) and comes back on restart ({} chats)", p.exists(), back.len());
}

#[test]
#[ignore = "fails: deleting a chat removes <id>.json, so a chat file named anything else says 'chat deleted' and comes back on restart"]
fn qa_chat_delete_renamed_file() {
    child_ok(CHILD_DEL_NAME, "chat-del-name");
}

const CHILD_DEL_TRAVERSAL: &str = "panes::chat::qa_data::child_chat_delete_id_outside_folder";
#[test]
fn child_chat_delete_id_outside_folder() {
    if !is_child(CHILD_DEL_TRAVERSAL) {
        return;
    }
    // a damaged or planted chat file whose id climbs out of the chats folder
    let canary = crate::config::data_dir().join("victim.json");
    std::fs::write(&canary, "{\"important\":true}").unwrap();
    write_chat("evil.json", &chat_json("../victim", "looks harmless", store::now(), vec![user("hi")]));
    let mut k = kit();
    let mut c = Chat::new(&k.config);
    open_by_id(&mut c, "../victim");
    delete_open_chat(&mut k, &mut c);
    assert!(canary.exists(), "deleting a chat whose id is '../victim' deleted {} (outside the chats folder)", canary.display());
}

#[test]
#[ignore = "fails: a chat file's id is used as a path, so deleting a chat with id '../victim' deletes <data dir>/victim.json"]
fn qa_chat_delete_id_outside_folder() {
    child_ok(CHILD_DEL_TRAVERSAL, "chat-del-traversal");
}

const CHILD_SAVE_RO: &str = "panes::chat::qa_data::child_chat_save_readonly_file";
#[test]
fn child_chat_save_readonly_file() {
    if !is_child(CHILD_SAVE_RO) {
        return;
    }
    let p = write_chat("locked.json", &chat_json("locked", "read-only chat", 1_700_000_000.0, vec![user("first")]));
    let _ro = ReadOnly::set(&p);
    let mut k = kit();
    let mut c = Chat::new(&k.config);
    open_by_id(&mut c, "locked");
    // what a finished reply does: add the messages and save
    c.chat.messages.push(store::Msg { role: "user".into(), content: "second question".into(), ..Default::default() });
    c.chat.messages.push(store::Msg { role: "assistant".into(), content: "an answer worth keeping".into(), ..Default::default() });
    c.persist();
    let s = k.render(&mut c, 120, 30);
    let on_disk = std::fs::read_to_string(&p).unwrap().contains("an answer worth keeping");
    let told = !k.notices().is_empty() || c.info.iter().any(|l| l.contains("save")) || s.contains("couldn't save");
    let tmp_left = store::dir().join("locked.json.tmp").exists();
    assert!(on_disk || told, "the reply isn't on disk ({on_disk}) and nothing said so ({told}); leftover temp file: {tmp_left}");
}

#[test]
#[ignore = "fails: when a chat file can't be replaced (read-only), store::save drops every new message silently and leaves <id>.json.tmp behind"]
fn qa_chat_save_readonly_file() {
    child_ok(CHILD_SAVE_RO, "chat-save-ro");
}

// ------------------------------------------------------------------ lots of history

const CHILD_MANY: &str = "panes::chat::qa_data::child_chat_many_big_chats";
#[test]
fn child_chat_many_big_chats() {
    if !is_child(CHILD_MANY) {
        return;
    }
    // 200 chats with ~60 KB transcripts each: loaded on the UI thread when the chat app opens
    let body: Vec<String> = (0..400).map(|i| format!("+{i}\tlet value_{i} = compute(\"🎉 {i}\");")).collect();
    let now = store::now();
    for n in 0..200 {
        let parts = vec![json!({"kind": "text", "text": "done"}), tool(&format!("t{n}"), "Edit", "Update", "src/lib.rs", "done", body.clone())];
        write_chat(&format!("big{n}.json"), &chat_json(&format!("big{n}"), &format!("chat {n}"), now - n as f64 * 3600.0, vec![user("go"), reply("done", parts)]));
    }
    let t0 = std::time::Instant::now();
    let mut k = kit();
    let mut c = Chat::new(&k.config);
    let open_ms = t0.elapsed().as_millis();
    assert_eq!(c.chats.len(), 200);
    let t1 = std::time::Instant::now();
    k.render_side(&mut c, 34, 40);
    c.open_chat(0);
    c.expanded = true;
    k.render(&mut c, 150, 44);
    let draw_ms = t1.elapsed().as_millis();
    println!("qa_chat_many: 200 chats / {} MB: Chat::new {open_ms} ms, sidebar + open + expanded frame {draw_ms} ms (debug build)", std::fs::read_dir(store::dir()).unwrap().flatten().map(|e| e.metadata().map(|m| m.len()).unwrap_or(0)).sum::<u64>() / 1_000_000);
    assert!(open_ms < 20_000, "opening the chat app with 200 saved chats took {open_ms} ms");
}

#[test]
fn qa_chat_many_big_chats() {
    let out = crate::qa_data::run_child(CHILD_MANY, &crate::qa_data::scratch("chat-many")).unwrap_or_else(|e| panic!("{e}"));
    if let Some(l) = out.lines().find(|l| l.contains("qa_chat_many:")) {
        println!("{l}");
    }
}