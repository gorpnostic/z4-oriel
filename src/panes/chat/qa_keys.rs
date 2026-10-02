//! QA key-mash for the chat (see src/qa_keys.rs). It runs in a child copy of the test binary with its own
//! ORIEL_DATA_DIR, because the chat saves and deletes chat files there (ctrl+d y deletes the open chat). AIs never
//! run in tests (providers::LIVE); replies are faked instead, by feeding the chat the events a provider would send
//! (text, thinking, tool calls with diffs, todos, approvals, questions, errors) while the keys keep coming.
//! Held back: enter when it would run /save, which writes into the real Documents folder.

use super::*;
use crate::qa_keys::{Ev as QEv, Opts, Rng, in_child, mash_pane, run_child, scratch};

#[test]
fn qa_keys_chat() {
    run_child("panes::chat::qa_keys::qa_keys_chat_child", "chat", &[], Duration::from_secs(400));
}

/// Saved chats to open from the sidebar: long ones, unicode, markdown, tool calls, an old one, a from-the-future one.
fn demo_chats() {
    let t = store::now();
    let md = "# heading\n\nsome **bold** and `code` and a [link](http://x)\n\n| a | b |\n|---|---|\n| 1 | 中文 |\n\n```rust\nfn main() {\n\tprintln!(\"🙂\");\n}\n```\n\n- one\n  - nested\n1. first\n> quote\n\n---\n";
    for (i, (title, age)) in [("short", 60.0), ("中文 chat 🙂", 5000.0), ("", 90_000.0), ("a very long title that will never fit in the sidebar at all no way", 400_000.0), ("future", -86_400.0), ("old", 90_000_000.0)].iter().enumerate() {
        let mut c = store::Chat::new(["claude", "codex", "ollama", "openai", "nope"][i % 5]);
        c.title = title.to_string();
        c.created = t - age;
        for j in 0..(i * 7 + 1) {
            c.messages.push(store::Msg { role: "user".into(), content: format!("question {j} {}", "word ".repeat(j * 11)), ..Default::default() });
            let mut m = store::Msg { role: "assistant".into(), model: Some("claude".into()), content: if j % 2 == 0 { md.repeat(1 + j % 3) } else { String::new() }, ..Default::default() };
            if j % 3 == 1 {
                m.parts.push(store::Part::Tool(tool(j, j % 2 == 0)));
            }
            c.messages.push(m);
        }
        store::save(&mut c).expect("seeding a chat");
        // save stamps `updated` with now: put the age back
        if let Ok(s) = std::fs::read_to_string(store::dir().join(format!("{}.json", c.id))) {
            let s = s.replacen(&format!("\"updated\": {}", c.updated), &format!("\"updated\": {}", t - age), 1);
            let _ = std::fs::write(store::dir().join(format!("{}.json", c.id)), s);
        }
    }
}

fn tool(i: usize, running: bool) -> store::Tool {
    let (name, label) = [("Edit", "Update"), ("Bash", "Bash"), ("Read", "Read"), ("Agent", "Agent"), ("Grep", "Grep")][i % 5];
    let mut body: Vec<String> = (0..(i * 5 % 60)).map(|n| agent::line(['+', '-', ' ', '@', '>', '!', '#', '…'][n % 8], if n % 4 == 0 { None } else { Some(n as u64 * 7) }, &format!("line {n} {}\tend 中文 🙂", "x".repeat(n * 13 % 200)))).collect();
    if i % 7 == 0 {
        body.clear();
    }
    store::Tool {
        id: format!("t{i}"),
        name: name.into(),
        label: label.into(),
        target: if i % 3 == 0 { String::new() } else { format!("src/{}.rs 中文 {}", i, "deep/".repeat(i % 9)) },
        status: if running { "running" } else { ["done", "error", "stopped"][i % 3] }.into(),
        summary: format!("+{i} -{}", i / 2),
        body,
        children: if name == "Agent" { vec![tool(i + 1, false), tool(i + 2, running)] } else { vec![] },
        ..Default::default()
    }
}

/// Start a fake reply the way `send` does, without a provider behind it.
fn start_reply(c: &mut Chat, provider: &str) {
    c.chat.messages.push(store::Msg { role: "user".into(), content: "qa".into(), ..Default::default() });
    c.chat.messages.push(store::Msg { role: "assistant".into(), model: Some(provider.into()), ..Default::default() });
    let steer = providers::steerable(provider).then(|| std::sync::mpsc::channel().0);
    c.stream = Some(Stream { stop: Arc::default(), inbox: Arc::default(), status: String::new(), started: Instant::now(), tokens: 0, steer, pid: Arc::default(), perms: String::new() });
}

/// A burst of what a provider sends mid-reply.
fn events(r: &mut Rng, n: usize) -> Vec<Ev> {
    let mut v = vec![];
    for k in 0..n {
        v.push(match r.below(14) {
            0..=3 => Ev::Token(crate::qa_keys::paste_text(r, true)),
            4 => Ev::Thinking { text: if r.pct(50) { String::new() } else { "hmm 中文".into() }, tokens: r.below(5000) as u64 },
            5 | 6 => Ev::Tool(tool(r.below(40), r.pct(50))),
            7 => Ev::Todos((0..r.below(12)).map(|i| store::Todo { text: format!("todo {i} 🙂"), active: if i % 2 == 0 { format!("doing {i}") } else { String::new() }, status: ["pending", "in_progress", "completed", "weird"][i % 4].into(), id: String::new() }).collect()),
            8 => Ev::Usage(r.below(100_000) as u64),
            9 => Ev::Status(["thinking", "", "running tests 中文"][k % 3].into()),
            10 => Ev::Ask(approve::Ask { rule: ["Bash(rm:*)", "Edit", "Write"][k % 3].into(), label: "Bash".into(), target: "rm -rf nothing".into(), body: vec![agent::line('+', Some(1), "added"), agent::line('-', Some(2), "removed")], reply: std::sync::mpsc::channel().0 }),
            11 => Ev::Question(approve::Question {
                qs: (0..1 + r.below(3))
                    .map(|i| approve::Q { question: format!("which one {i}? 中文"), header: format!("h{i}"), multi: r.pct(40), options: (0..r.below(6)).map(|o| (format!("option {o}"), "desc 🙂".repeat(o))).collect() })
                    .collect(),
                reply: std::sync::mpsc::channel().0,
            }),
            12 => Ev::Steered("steered message".into()),
            _ => Ev::Mark(["conversation compacted", "usage limit reached"][k % 2].into()),
        });
    }
    v
}

/// What enter would run right now (the menu's pick, else the box), if anything.
fn enter_runs(c: &Chat) -> Option<String> {
    let items = c.menu();
    let text = match items.get(c.menu_sel.min(items.len().saturating_sub(1))) {
        Some(it) if !it.run => return None, // it only completes the command
        Some(it) => it.fill.clone(),
        None => c.input.trim().to_string(),
    };
    text.starts_with('/').then_some(text)
}

#[test]
#[ignore = "child process: run by qa_keys_chat"]
fn qa_keys_chat_child() {
    if !in_child() {
        return;
    }
    demo_chats();
    let themes = scratch("chat-themes");
    let mut r = Rng::new(crate::qa_keys::seed_for("chat-replies"));
    let mut n = 0usize;
    mash_pane(
        "chat",
        Opts { budget: Duration::from_secs(360), ..Default::default() },
        move |k| {
            crate::theme::TEST_DIR.with(|d| *d.borrow_mut() = Some(themes)); // /theme new writes here
            Chat::new(&k.config)
        },
        move |c, ev| {
            n += 1;
            // now and then a reply starts, and events trickle in while the keys keep coming
            if n % 45 == 0 {
                if c.stream.is_none() {
                    start_reply(c, ["claude", "codex", "ollama"][n / 45 % 3]);
                }
                if let Some(s) = &c.stream {
                    let count = 1 + r.below(12);
                    let mut evs = events(&mut r, count);
                    if r.pct(25) {
                        evs.push(if r.pct(70) { Ev::Done { note: Some("12s · 3k tokens".into()) } } else { Ev::Error("qa: provider fell over 中文".into()) });
                    }
                    s.inbox.lock().unwrap().extend(evs);
                }
            }
            if let QEv::Key(k) = ev {
                let enter = k.code == KeyCode::Enter && !k.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT);
                let busy = c.confirm_delete || !c.questions.is_empty(); // (a pending approval or ctrl+x still lets enter through)
                if enter && !busy && enter_runs(c).is_some_and(|t| t.split(' ').next() == Some("/save")) {
                    return false; // /save writes into the real Documents folder
                }
            }
            true
        },
    );
}
