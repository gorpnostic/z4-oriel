use super::*;
use crate::testkit::Kit;

/// A search app over the scratch machine from recall's tests, with its first index pass done.
fn app(k: &mut Kit, name: &str) -> (Search, Env) {
    let (env, ..) = crate::recall::tests::fixtures(name);
    let mut s = Search::with_env(env.clone());
    k.render(&mut s, 120, 30); // starts the worker: an index pass, then the (empty) query
    wait(k, &mut s, |s| s.summary.is_some() && s.shown == s.seq);
    (s, env)
}

/// Poll until `done` (the worker answers on its own thread).
fn wait(k: &mut Kit, s: &mut Search, done: impl Fn(&Search) -> bool) {
    let t0 = Instant::now();
    while !done(s) && t0.elapsed() < Duration::from_secs(10) {
        k.wait_wake(s, 50);
    }
    assert!(done(s), "the search worker never answered");
}

fn typ(k: &mut Kit, s: &mut Search, text: &str) {
    k.typ(s, text);
    wait(k, s, |s| s.shown == s.seq);
}

#[test]
fn search_app_finds_reads_and_filters() {
    let mut k = Kit::new();
    let (mut s, _env) = app(&mut k, "pane");
    let screen = k.render_html(&mut s, 120, 30, "target/snap/search-all.html");
    assert!(screen.contains("4 · newest first") && screen.contains("4 sessions"), "{screen}");
    assert!(screen.contains("claude code · demo · Flaky login test fix") && screen.contains("agent · api · fix upload limits"), "{screen}");
    let side = k.render_side(&mut s, 30, 20);
    assert!(side.contains("claude code") && side.contains("agent runs") && side.contains("projects") && side.contains("demo"), "{side}");

    typ(&mut k, &mut s, "fake clock");
    let screen = k.render_html(&mut s, 120, 30, "target/snap/search-hit.html");
    assert!(screen.contains("1 found"), "{screen}");
    assert!(screen.contains("ai   We decided to replace sleep with a fake clock."), "{screen}");
    // enter reads it here, in the chat's renderer, opened at the reply that matched
    k.key(&mut s, KeyCode::Enter);
    assert!(s.viewer.is_some());
    let screen = k.render_html(&mut s, 120, 30, "target/snap/search-read.html");
    assert!(screen.contains("‹ esc") && screen.contains("Flaky login test fix"), "{screen}");
    assert!(screen.contains("We decided to replace sleep with a fake clock.") && screen.contains("cargo test login"), "{screen}");
    assert!(screen.contains("resume in chat") && !screen.contains("message claude"), "read-only: no composer\n{screen}");
    assert!(!k.key(&mut s, KeyCode::Char('x')), "typing does nothing while reading");
    // a refresh that changes the results under you doesn't change what you're reading (or what a attaches)
    s.hits.clear();
    k.actions.clear();
    k.key(&mut s, KeyCode::Char('a'));
    assert!(k.actions.iter().any(|a| matches!(a, Action::AppPaste("ai", t) if t.contains("fake clock"))), "{:?}", k.actions.len());
    assert!(k.render(&mut s, 120, 30).contains("Flaky login test fix"));
    k.key(&mut s, KeyCode::Esc);
    assert!(s.viewer.is_none());

    // filters from the sidebar, and typed
    s.query.clear();
    s.toggle("ai:", "agent");
    wait(&mut k, &mut s, |s| s.shown == s.seq);
    assert_eq!(s.query, "ai:agent");
    assert_eq!(s.hits.len(), 1);
    assert!(k.render(&mut s, 120, 30).contains("only agent runs"));
    s.toggle("ai:", "agent");
    assert_eq!(s.query, "", "a second click takes it off");
    typ(&mut k, &mut s, "ai:nope");
    assert!(k.render(&mut s, 120, 30).contains("didn't understand ai:nope"));
    k.key_mod(&mut s, KeyCode::Char('u'), KeyModifiers::CONTROL);
    typ(&mut k, &mut s, "zebra");
    assert!(k.render(&mut s, 120, 30).contains("nothing matches \"zebra\""));
}

#[test]
fn search_app_resumes_and_attaches() {
    let mut k = Kit::new();
    let (mut s, _env) = app(&mut k, "resume");
    typ(&mut k, &mut s, "retry loop");
    // tab: the results take the keys, so r and a aren't typed
    k.key(&mut s, KeyCode::Tab);
    assert!(s.in_list);
    k.key(&mut s, KeyCode::Char('a'));
    let pasted = k.actions.iter().find_map(|a| match a {
        Action::AppPaste("ai", text) => Some(text.clone()),
        _ => None,
    });
    let pasted = pasted.expect("a pastes into the chat app");
    assert!(pasted.starts_with("(from a claude code session in demo") && pasted.contains("The clock mocking races with the retry loop."), "{pasted}");
    assert_eq!(s.query, "retry loop", "a wasn't typed into the box");

    // r: carried on in the chat app, resuming Claude's own session in its folder
    k.actions.clear();
    k.key(&mut s, KeyCode::Char('r'));
    assert!(k.actions.iter().any(|a| matches!(a, Action::GotoApp("ai"))));
    let mut c = Chat::new(&k.config);
    let _ = k.render(&mut c, 120, 30); // the chat picks it up when it draws
    let screen = k.render_html(&mut c, 120, 30, "target/snap/search-resumed.html");
    assert!(screen.contains("carrying on a claude code session") && screen.contains("We decided to replace sleep with a fake clock."), "{screen}");
    let (state, info, draft) = c.test_view();
    assert!(state.contains("\"session\":\"sess-1\""), "{state}");
    assert!(info[0].contains("your next message continues it"), "{info:?}");
    assert!(draft, "only saved as a chat of yours once you send something");

    // typing goes back to the box
    k.key(&mut s, KeyCode::Char('x'));
    assert!(!s.in_list && s.query == "retry loopx");
}

#[test]
fn search_resume_falls_back_to_context() {
    let (env, ..) = crate::recall::tests::fixtures("fallback");
    let (m, _) = recall::update(&env, 2, &|_, _| {});
    let e = m.sessions.iter().find(|e| e.src == Src::Claude).unwrap().clone();
    let turns = recall::load(&env, &e);
    let (c, note) = resumed(&e, &turns, true);
    assert_eq!(c.state["claude"]["session"], "sess-1");
    assert!(note.contains("continues it"), "{note}");
    // the CLI deleted it: a fresh session with the conversation as context
    let gone = Entry { gone: true, ..e.clone() };
    let (c, note) = resumed(&gone, &turns, true);
    assert!(c.state.is_empty() && note.contains("deleted the original"), "{note}");
    let (c, note) = resumed(&e, &turns, false);
    assert!(c.state.is_empty() && c.cwd.is_none() && note.contains("folder isn't there"), "{note}");
    // an AI oriel can't resume goes to your default one
    let kimi = m.sessions.iter().find(|e| e.src == Src::Kimi).unwrap();
    let (c, note) = resumed(kimi, &recall::load(&env, kimi), true);
    assert!(c.provider.is_none() && note.contains("can't resume"), "{note}");
    // the transcript comes across whole, tool calls included
    let (c, _) = resumed(&e, &turns, true);
    assert_eq!(c.messages.len(), 3);
    let tools: Vec<String> = c.messages[1].parts.iter().filter_map(|p| if let store::Part::Tool(t) = p { Some(format!("{} {} {} · {}", t.name, t.target, t.status, t.summary)) } else { None }).collect();
    assert_eq!(tools, vec!["Bash cargo test login error · test login_retries failed: timeout"], "the chat adds its own \"Error:\"");
}

#[test]
fn search_recall_command_and_highlight() {
    let mut k = Kit::new();
    let mut c = Chat::new(&k.config);
    k.typ(&mut c, "/recall retry loop");
    k.key(&mut c, KeyCode::Enter);
    assert!(k.actions.iter().any(|a| matches!(a, Action::GotoApp("search"))), "/recall opens the search app");
    let (env, ..) = crate::recall::tests::fixtures("recall-cmd");
    let mut s = Search::with_env(env);
    k.render(&mut s, 120, 30);
    assert_eq!(s.query, "retry loop", "…on the words you gave it");
    wait(&mut k, &mut s, |s| s.shown == s.seq && s.summary.is_some() && !s.hits.is_empty());
    assert_eq!(s.hits[0].entry.title, "Flaky login test fix");

    let t = crate::theme::get("oriel");
    let spans = highlight("Retry the RETRY loop", &["retry".into()], ui::muted(&t), ui::accent(&t));
    let marked: Vec<&str> = spans.iter().filter(|s| s.style == ui::accent(&t)).map(|s| s.content.as_ref()).collect();
    assert_eq!(marked, vec!["Retry", "RETRY"]);
    assert_eq!(span(30 * 86400), "30 days");
}

/// A saved chat opens at the message the matching line is in, even when the extract's own count is off (it drops
/// empty messages and splits off ones you queued mid-reply).
#[test]
fn search_opens_saved_chat_at_the_match() {
    let mut c = store::Chat::new("claude");
    let msg = |role: &str, content: &str| store::Msg { role: role.into(), content: content.into(), ..Default::default() };
    c.messages = vec![msg("user", "first question"), msg("assistant", ""), msg("user", "second question"), msg("assistant", "the answer")];
    c.messages[3].parts = vec![
        store::Part::Text { text: "the answer".into() },
        store::Part::User { text: "and one more thing".into() },
        store::Part::Tool(store::Tool { label: "Bash".into(), target: "cargo  test".into(), ..Default::default() }),
    ];
    assert_eq!(find_msg(&c, &["second question".into()]), Some(2));
    assert_eq!(find_msg(&c, &["…one more…".into()]), Some(3), "a cut line still matches");
    assert_eq!(find_msg(&c, &["Bash cargo test".into()]), Some(3), "a tool line is its label and target");
    assert_eq!(find_msg(&c, &["not in it".into()]), None);
    assert_eq!(find_msg(&c, &[]), None);
}

/// Opening the app answers from what's already indexed at once (counts and results), then again once the pass has
/// looked for news.
#[test]
fn search_answers_before_the_index_pass() {
    let (env, ..) = crate::recall::tests::fixtures("instant");
    recall::update(&env, 2, &|_, _| {});
    let (req_tx, req_rx) = channel();
    let (tx, rx) = channel();
    let (wake_tx, _wake_rx) = channel();
    req_tx.send(Req::Update).unwrap();
    req_tx.send(Req::Query(1, "fake clock".into())).unwrap();
    let env = Arc::new(env);
    std::thread::spawn(move || worker(env, req_rx, tx, Waker { id: 1, tx: wake_tx }));
    let mut order = vec![];
    while order.len() < 4 {
        match rx.recv_timeout(Duration::from_secs(10)).expect("the worker answers") {
            Msg::Progress(..) => {}
            Msg::Known(s) => order.push(format!("known {}", s.total)),
            Msg::Indexed(s, _) => order.push(format!("indexed {}", s.total)),
            Msg::Results { seq, hits, .. } => order.push(format!("results {seq}: {}", hits.len())),
        }
    }
    drop(req_tx);
    assert_eq!(order, vec!["known 4", "results 1: 1", "indexed 4", "results 1: 1"], "the sidebar and the answer first");
}
