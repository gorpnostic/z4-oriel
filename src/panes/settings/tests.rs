//! The settings app, headless: every change goes through the app (Kit applies it to its own config, never to
//! disk), links ask the app to go somewhere, and nothing looks at the network or your files.

use super::*;
use crate::testkit::Kit;

/// A Kit on the real defaults (the theme filled in, as the app always has it).
fn kit() -> Kit {
    let mut k = Kit::new();
    k.config = crate::config::defaults();
    k
}

/// Wide enough that the panel's notes don't wrap.
fn show(k: &mut Kit, p: &mut Settings) -> String {
    k.render(p, 190, 44)
}

fn at(k: &mut Kit, p: &mut Settings, id: &str) -> String {
    jump(id);
    show(k, p)
}

fn key(k: &mut Kit, p: &mut Settings, c: KeyCode) {
    k.key(p, c);
}

fn scratch(name: &str) -> std::path::PathBuf {
    let d = std::path::absolute(format!("target/test-scratch/config/settings-{name}")).unwrap();
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Every section draws its rows with their values and the panel beside them.
#[test]
fn settings_every_section_renders() {
    let mut k = kit();
    k.theme = crate::theme::get("ultra");
    let mut p = Settings::new(&k.config);
    let want: &[(&str, &[&str])] = &[
        ("general", &["theme", "plain icons", "sidebar at start", "open on", "prefix key", "ctrl+space", "shell", "default ("]),
        ("AI chat", &["default AI", "permissions", "edits", "effort", "Claude Code model", "Ollama model"]),
        ("providers & keys", &["Ollama address", "http://127.0.0.1:11434", "OpenAI-compatible URL", "OpenAI key", "t tests it"]),
        ("lead mode", &["lead AI", "workers at once", "run budget", "$8.00", "merge gate", "detect", "gate timeout", "15 min", "hung after", "5 min"]),
        ("roster", &["edit the roster", "defaults from what's installed", "reset to the defaults"]),
        ("alerts", &["desktop notifications", "notify: agent needs you", "memory warning", "92%", "calendar reminder", "10 min", "send a test notification"]),
        ("folders", &["notes folder", "music folders", "none: the OS music folder", "add a folder", "music source"]),
        ("tools", &["token saver", "AI tools", "updates", "check for updates", "replay the tour", "config file"]),
    ];
    for (i, (cat, rows)) in want.iter().enumerate() {
        p.set_cat(i, &mut Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: &mut vec![], focused: true, time: 1.0 });
        let s = k.render_html(&mut p, 160, 44, &format!("target/snap/settings-{i}.html"));
        assert!(p.title().contains(cat), "{}", p.title());
        for r in *rows {
            assert!(s.contains(r), "{cat}: {r:?} missing\n{s}");
        }
        assert!(s.contains("in file") && s.contains("applies") || *cat == "roster" || *cat == "tools", "{cat}: the panel\n{s}");
    }
    // a narrow pane: the panel goes under the list
    p.set_cat(1, &mut Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: &mut vec![], focused: true, time: 1.0 });
    let s = k.render_html(&mut p, 96, 40, "target/snap/settings-narrow.html");
    assert!(s.contains("default AI") && s.contains("in file"), "{s}");
    let side = k.render_side(&mut p, 30, 12);
    assert!(side.contains("general") && side.contains("providers & keys") && side.contains("tools"), "{side}");
}

/// Switches, choices and numbers save through the app; ● marks a changed row and r puts it back.
#[test]
fn settings_change_and_reset() {
    let mut k = kit();
    let mut p = Settings::new(&k.config);
    at(&mut k, &mut p, "update_check");
    key(&mut k, &mut p, KeyCode::Char(' '));
    assert_eq!(k.apply_config(&mut p), 1, "saved through the app");
    assert!(!k.config.update_check);
    let s = show(&mut k, &mut p);
    assert!(s.contains("● check for updates") && s.contains("● 1 changed"), "{s}");
    key(&mut k, &mut p, KeyCode::Char('r'));
    k.apply_config(&mut p);
    assert!(k.config.update_check, "back to the default");
    // a choice: ← → walk it
    at(&mut k, &mut p, "ai.perms");
    key(&mut k, &mut p, KeyCode::Right);
    k.apply_config(&mut p);
    assert_eq!(k.config.ai.perms, "auto");
    for _ in 0..3 {
        key(&mut k, &mut p, KeyCode::Left);
        k.apply_config(&mut p); // the app applies each change before the next key
    }
    assert_eq!(k.config.ai.perms, "bypass", "wraps around");
    let s = show(&mut k, &mut p);
    assert!(s.contains("⚠ bypass: agents never ask"), "bypass is called out\n{s}");
    // numbers: steps, typed values are clamped, words refused
    at(&mut k, &mut p, "lead.max_parallel");
    key(&mut k, &mut p, KeyCode::Right);
    k.apply_config(&mut p);
    assert_eq!(k.config.lead.max_parallel, 4);
    key(&mut k, &mut p, KeyCode::Enter);
    key(&mut k, &mut p, KeyCode::Backspace);
    k.typ(&mut p, "9");
    key(&mut k, &mut p, KeyCode::Enter);
    k.apply_config(&mut p);
    assert_eq!(k.config.lead.max_parallel, 5, "clamped to 1-5");
    at(&mut k, &mut p, "lead.run_budget_usd");
    key(&mut k, &mut p, KeyCode::Left);
    k.apply_config(&mut p);
    assert_eq!(k.config.lead.run_budget_usd, 7.5, "$0.50 steps");
    key(&mut k, &mut p, KeyCode::Enter);
    k.typ(&mut p, "x");
    key(&mut k, &mut p, KeyCode::Enter);
    assert!(show(&mut k, &mut p).contains("isn't a number"));
    assert_eq!(k.apply_config(&mut p), 0);
    key(&mut k, &mut p, KeyCode::Esc);
    // an alert kind on and off: [alerts] desktop keeps the others
    at(&mut k, &mut p, "alerts.desktop.agent_done");
    key(&mut k, &mut p, KeyCode::Enter);
    k.apply_config(&mut p);
    assert!(k.config.alerts.desktop.contains(&"agent_done".to_string()) && k.config.alerts.desktop.contains(&"needs_you".to_string()));
    // the per-AI model: a suggestion, or your own, and "default" removes it
    at(&mut k, &mut p, "ai.models.claude");
    key(&mut k, &mut p, KeyCode::Right);
    k.apply_config(&mut p);
    assert_eq!(k.config.ai.models.get("claude").map(String::as_str), Some("opus"));
    key(&mut k, &mut p, KeyCode::Enter);
    for _ in 0..8 {
        key(&mut k, &mut p, KeyCode::Backspace);
    }
    k.typ(&mut p, "claude-something-new");
    key(&mut k, &mut p, KeyCode::Enter);
    k.apply_config(&mut p);
    assert_eq!(k.config.ai.models.get("claude").map(String::as_str), Some("claude-something-new"));
    key(&mut k, &mut p, KeyCode::Char('r'));
    k.apply_config(&mut p);
    assert_eq!(k.config.ai.models.get("claude"), None);
}

/// API keys are typed out of sight and shown as sk-…a1b2; x clears one; an address must look like one.
#[test]
fn settings_keys_are_masked_and_addresses_checked() {
    let mut k = kit();
    let mut p = Settings::new(&k.config);
    at(&mut k, &mut p, "ai.anthropic_key");
    key(&mut k, &mut p, KeyCode::Enter);
    k.typ(&mut p, "sk-ant-secret-a1b2");
    k.render_html(&mut p, 160, 36, "target/snap/settings-typing.html");
    let s = show(&mut k, &mut p);
    assert!(!s.contains("secret") && s.contains("a1b2"), "masked while typing\n{s}");
    key(&mut k, &mut p, KeyCode::Enter);
    k.apply_config(&mut p);
    assert_eq!(k.config.ai.anthropic_key, "sk-ant-secret-a1b2");
    let s = show(&mut k, &mut p);
    assert!(s.contains("sk-…a1b2") && !s.contains("secret") && s.contains("plain text"), "{s}");
    key(&mut k, &mut p, KeyCode::Char('x'));
    k.apply_config(&mut p);
    assert!(k.config.ai.anthropic_key.is_empty());
    // an address
    at(&mut k, &mut p, "ai.ollama_url");
    key(&mut k, &mut p, KeyCode::Enter);
    for _ in 0..40 {
        key(&mut k, &mut p, KeyCode::Backspace);
    }
    k.typ(&mut p, "localhost:11434");
    key(&mut k, &mut p, KeyCode::Enter);
    assert!(show(&mut k, &mut p).contains("starts with http"));
    for _ in 0..40 {
        key(&mut k, &mut p, KeyCode::Backspace);
    }
    k.typ(&mut p, "http://10.0.0.5:11434/");
    key(&mut k, &mut p, KeyCode::Enter);
    k.apply_config(&mut p);
    assert_eq!(k.config.ai.ollama_url, "http://10.0.0.5:11434");
    // t would test it (offline here: it says so rather than trying)
    key(&mut k, &mut p, KeyCode::Char('t'));
    assert!(show(&mut k, &mut p).contains("not tested here"));
    // the OpenAI-compatible URL has presets
    at(&mut k, &mut p, "ai.openai_url");
    key(&mut k, &mut p, KeyCode::Right);
    k.apply_config(&mut p);
    assert_eq!(k.config.ai.openai_url, "https://openrouter.ai/api/v1");
}

/// The prefix is captured by pressing it: only ctrl+<letter> or ctrl+space, and not a key an app needs.
#[test]
fn settings_prefix_capture_refuses_clashes() {
    let mut k = kit();
    let mut p = Settings::new(&k.config);
    at(&mut k, &mut p, "prefix");
    key(&mut k, &mut p, KeyCode::Enter);
    assert!(show(&mut k, &mut p).contains("press a key"));
    k.render_html(&mut p, 160, 36, "target/snap/settings-capture.html");
    k.key_mod(&mut p, KeyCode::Char('s'), KeyModifiers::CONTROL);
    let s = show(&mut k, &mut p);
    assert!(s.contains("ctrl+s is save in the agents forms"), "names the clash\n{s}");
    k.key(&mut p, KeyCode::Char('b'));
    assert!(show(&mut k, &mut p).contains("isn't ctrl+<letter>"), "a bare key would eat every b");
    assert_eq!(k.apply_config(&mut p), 0);
    k.key_mod(&mut p, KeyCode::Char('b'), KeyModifiers::CONTROL);
    assert_eq!(k.apply_config(&mut p), 1);
    assert_eq!(k.config.prefix, "ctrl+b");
    // esc leaves it alone
    key(&mut k, &mut p, KeyCode::Enter);
    key(&mut k, &mut p, KeyCode::Esc);
    assert_eq!(k.apply_config(&mut p), 0);
    // a hand-edited bare key is flagged
    k.config.prefix = "b".into();
    let s = at(&mut k, &mut p, "prefix");
    assert!(s.contains("(unusable)") && s.contains("⚠ 'b' isn't ctrl+<letter>"), "{s}");
}

/// Music folders: a adds (it has to exist), each shows its song count, J/K reorder, x removes.
#[test]
fn settings_music_folders_list() {
    let (a, b) = (scratch("music-a"), scratch("music-b"));
    std::fs::write(a.join("one.mp3"), "").unwrap();
    std::fs::write(b.join("two.flac"), "").unwrap();
    std::fs::write(b.join("three.ogg"), "").unwrap();
    let mut k = kit();
    let mut p = Settings::new(&k.config);
    at(&mut k, &mut p, "music.folders");
    for d in [&a, &b] {
        key(&mut k, &mut p, KeyCode::Char('a'));
        k.typ(&mut p, &d.to_string_lossy());
        key(&mut k, &mut p, KeyCode::Enter);
        k.apply_config(&mut p);
    }
    assert_eq!(k.config.music.folders, vec![a.to_string_lossy().to_string(), b.to_string_lossy().to_string()]);
    let s = k.render_html(&mut p, 160, 44, "target/snap/settings-music.html");
    assert!(s.contains("1 song") && !s.contains("1 songs") && s.contains("2 songs") && s.contains("2 folders"), "{s}");
    // a folder that isn't there is refused
    key(&mut k, &mut p, KeyCode::Enter);
    k.typ(&mut p, "/no/such/folder");
    key(&mut k, &mut p, KeyCode::Enter);
    assert!(show(&mut k, &mut p).contains("doesn't exist"));
    assert_eq!(k.apply_config(&mut p), 0);
    key(&mut k, &mut p, KeyCode::Esc);
    // up to the second folder, move it first, then remove the other
    key(&mut k, &mut p, KeyCode::Up);
    assert_eq!(p.sel, Sel::Item("music.folders".into(), 1));
    key(&mut k, &mut p, KeyCode::Char('K'));
    k.apply_config(&mut p);
    assert_eq!(k.config.music.folders[0], b.to_string_lossy());
    key(&mut k, &mut p, KeyCode::Down);
    key(&mut k, &mut p, KeyCode::Char('x'));
    k.apply_config(&mut p);
    assert_eq!(k.config.music.folders, vec![b.to_string_lossy().to_string()]);
    // the notes folder says whether it's there
    at(&mut k, &mut p, "notes_folder");
    key(&mut k, &mut p, KeyCode::Enter);
    k.typ(&mut p, &a.to_string_lossy());
    key(&mut k, &mut p, KeyCode::Enter);
    k.apply_config(&mut p);
    assert_eq!(k.config.notes_folder, a.to_string_lossy());
    assert!(show(&mut k, &mut p).contains("exists ✓"));
}

/// Links ask the app to go somewhere; resetting the roster takes a second enter and really writes roster = [].
#[test]
fn settings_links_and_actions() {
    let mut k = kit();
    let mut p = Settings::new(&k.config);
    let jumps = |k: &Kit| k.actions.iter().filter_map(|a| match a {
        Action::GotoApp(x) => Some(format!("go {x}")),
        Action::AppKey(x, c) => Some(format!("key {x} {c}")),
        Action::Tour => Some("tour".into()),
        _ => None,
    }).collect::<Vec<_>>();
    at(&mut k, &mut p, "roster.edit");
    key(&mut k, &mut p, KeyCode::Enter);
    assert_eq!(jumps(&k), vec!["go agents", "key agents R"], "the roster editor, behind R in agents");
    k.actions.clear();
    at(&mut k, &mut p, "tools.saver");
    key(&mut k, &mut p, KeyCode::Enter);
    assert_eq!(jumps(&k), vec!["go ais", "key ais 4"]);
    k.actions.clear();
    at(&mut k, &mut p, "tools.tour");
    key(&mut k, &mut p, KeyCode::Enter);
    assert_eq!(jumps(&k), vec!["tour"]);
    // the roster
    k.config.roster = vec![crate::config::RosterEntry { name: "kimi".into(), agent: "kimi".into(), ..Default::default() }];
    let s = at(&mut k, &mut p, "roster.reset");
    assert!(s.contains("1 worker: kimi"), "{s}");
    key(&mut k, &mut p, KeyCode::Enter);
    assert_eq!(k.apply_config(&mut p), 0, "asks first");
    assert!(show(&mut k, &mut p).contains("enter again"));
    key(&mut k, &mut p, KeyCode::Enter);
    assert_eq!(k.apply_config(&mut p), 1);
    assert!(k.config.roster.is_empty());
    assert!(show(&mut k, &mut p).contains("defaults from what's installed"));
    // a test notification (no toast in tests)
    at(&mut k, &mut p, "alerts.test");
    key(&mut k, &mut p, KeyCode::Enter);
    assert!(show(&mut k, &mut p).contains("sent"));
}

/// /settings perms, the palette's rows and / all find a setting in any section.
#[test]
fn settings_find_and_jump() {
    let mut k = kit();
    let mut p = Settings::new(&k.config);
    at(&mut k, &mut p, "perms");
    assert_eq!((p.cat, p.sel.clone()), (1, Sel::Row("ai.perms".into())), "found by its key");
    at(&mut k, &mut p, "alerts");
    assert_eq!(p.cat, 5, "a section");
    at(&mut k, &mut p, "keys");
    assert_eq!(p.cat, 2, "a word of a section's name");
    let s = at(&mut k, &mut p, "model");
    k.render_html(&mut p, 160, 36, "target/snap/settings-find.html");
    assert!(p.title().contains("find") && s.contains("── AI chat") && s.contains("── lead mode") && s.contains("lead model"), "several: a search\n{s}");
    key(&mut k, &mut p, KeyCode::Esc);
    assert!(p.filter.is_empty());
    // typing a search
    key(&mut k, &mut p, KeyCode::Char('/'));
    k.typ(&mut p, "timeout");
    key(&mut k, &mut p, KeyCode::Enter);
    let s = show(&mut k, &mut p);
    assert!(s.contains("gate timeout") && !s.contains("workers at once"), "{s}");
    assert_eq!(p.sel, Sel::Row("lead.gate_timeout_s".into()));
    // the palette lists every setting with its value
    let items = palette_items(&k.config);
    assert!(items.iter().any(|(l, id)| l == "permissions · edits" && id == "ai.perms"), "{items:?}");
    assert!(topics().iter().any(|(w, _)| w == "providers & keys"));
}

/// ← → on the theme shows each one without saving; esc goes back, enter keeps it.
#[test]
fn settings_theme_previews_live() {
    let mut k = kit();
    k.config.theme = "oriel".into();
    let mut p = Settings::new(&k.config);
    let previews = |k: &mut Kit| {
        let v: Vec<String> = k.actions.iter().filter_map(|a| if let Action::PreviewTheme(t) = a { Some(t.clone()) } else { None }).collect();
        k.actions.retain(|a| !matches!(a, Action::PreviewTheme(_)));
        v
    };
    at(&mut k, &mut p, "theme");
    key(&mut k, &mut p, KeyCode::Right);
    let shown = previews(&mut k);
    assert_eq!(shown.len(), 1);
    assert_ne!(shown[0], "oriel");
    assert_eq!(k.apply_config(&mut p), 0, "not saved yet");
    k.theme = crate::theme::get(&shown[0]); // what the app does with it
    assert!(show(&mut k, &mut p).contains("(preview)"));
    key(&mut k, &mut p, KeyCode::Esc);
    assert_eq!(previews(&mut k), vec!["oriel".to_string()], "esc goes back");
    k.theme = crate::theme::get("oriel");
    key(&mut k, &mut p, KeyCode::Right);
    let next = previews(&mut k)[0].clone();
    key(&mut k, &mut p, KeyCode::Enter);
    assert_eq!(k.apply_config(&mut p), 1);
    assert_eq!(k.config.theme, next, "enter keeps it");
    // moving off the row mid-preview goes back too
    key(&mut k, &mut p, KeyCode::Right);
    previews(&mut k);
    key(&mut k, &mut p, KeyCode::Down);
    assert_eq!(previews(&mut k), vec![next]);
    // enter without a preview: the palette's theme picker
    key(&mut k, &mut p, KeyCode::Up);
    key(&mut k, &mut p, KeyCode::Enter);
    assert!(k.actions.iter().any(|a| matches!(a, Action::Palette(q) if q == "theme ")));
}

#[test]
fn settings_parses_what_servers_answer() {
    assert_eq!(parse_tags(r#"{"models":[{"name":"llama3.2:latest"},{"name":"qwen3"}]}"#), Some(vec!["llama3.2:latest".to_string(), "qwen3".into()]));
    assert_eq!(parse_tags("<html>"), None);
    assert_eq!(parse_models(r#"{"object":"list","data":[{"id":"a"},{"id":"b"},{"id":"c"}]}"#), Some(3));
    assert_eq!(mask("sk-proj-0123456789a1b2"), "sk-…a1b2");
    assert_eq!(mask("short"), "•••••");
    assert_eq!(with_unit(900.0, Unit::Secs), "15 min");
    assert_eq!(with_unit(45.0, Unit::Secs), "45 s");
    assert_eq!(with_unit(2.5, Unit::Usd), "$2.50");
}
