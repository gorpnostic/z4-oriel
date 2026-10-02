//! oriel — a fast terminal workspace: tabs, tmux-style split panes, and built-in apps.

mod alerts;
mod app;
mod clip;
mod config;
mod editor;
mod font;
mod layout;
mod onboard;
mod pane;
mod panes;
mod session;
mod testkit;
mod theme;
mod ui;
mod update;

use crossterm::{
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste, EnableFocusChange, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
};
use std::sync::mpsc;

fn help(prefix: &str) -> String {
    format!(
        "oriel {} — a terminal workspace\n\n  oriel              open where you left off (your tabs, splits and chat)\n  oriel <app>        open straight into an app, this time only: ai agents ais terminal claude codex music system files notes calendar storage settings themes help\n  oriel --config     print the config file path (the settings app, alt , inside, changes all of it)\n  oriel update       update to the latest release (keeps this one for rollback)\n  oriel rollback     go back to the version before the last update\n  oriel changelog    what's new in recent releases\n  oriel --tour       replay the tour\n  oriel --version\n\nInside: F1-F9 apps, F10 help (every key and how-to), alt p palette, alt n terminal split, {prefix} = tmux-style prefix.",
        env!("CARGO_PKG_VERSION")
    )
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // a config.toml that doesn't parse: run on the defaults, say so, and never write over it
    let (cfg, broken) = match config::load_checked() {
        Ok(c) => (c, None),
        Err(e) => (config::load(), Some(e)),
    };
    // `oriel <app>` opens that app this once; it's never saved as the startup app
    let mut start = None;
    match args.first().map(String::as_str) {
        Some("-h" | "--help") => {
            println!("{}", help(&config::prefix(&cfg)));
            return Ok(());
        }
        Some("-V" | "--version") => {
            println!("oriel {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("update" | "--update") => {
            // oriel updates itself (keeping this version for `oriel rollback`); the install script is the fallback,
            // only for failures it can get past (not offline, a bad download or a failed backup), into the same folder
            let code = update::cli_update();
            if code == update::TRY_SCRIPT && args.get(1).map(String::as_str) != Some("--no-fallback") {
                std::process::exit(update::cli_script_fallback());
            }
            std::process::exit(code);
        }
        Some("rollback" | "--rollback") => std::process::exit(update::cli_rollback()),
        Some("changelog" | "--changelog" | "whats-new") => {
            match update::check(true) {
                Ok(rs) => {
                    for r in rs.iter().take(5) {
                        println!("## {}\n\n{}\n", r.version, r.notes.lines().filter(|l| !l.contains("**Full Changelog**")).collect::<Vec<_>>().join("\n").trim());
                    }
                }
                Err(e) => eprintln!("{e}"),
            }
            return Ok(());
        }
        Some("--mcp-approve") => {
            // the approval bridge Claude Code starts for /perms ask (see panes/chat/approve.rs)
            panes::chat::approve::serve_stdio(args.get(1).map(String::as_str).unwrap_or(""), args.get(2).map(String::as_str).unwrap_or(""));
            return Ok(());
        }
        Some("mcp-lead") => {
            // the MCP server a lead agent's CLI starts in lead mode (see panes/agents/mcp.rs)
            panes::agents::mcp_lead(args.get(1).map(String::as_str).unwrap_or(""), args.get(2).map(String::as_str).unwrap_or(""));
            return Ok(());
        }
        // helpers other programs call (hooks, status lines) — they print and exit, no UI
        Some("report") => std::process::exit(panes::agents::cli(&args[1..])),
        Some("usage-sink") => std::process::exit(panes::ais::cli(&args[1..])),
        Some("--config") => {
            println!("{}", config::path().display());
            return Ok(());
        }
        Some("--tour") => {}
        Some(app) if panes::known(app) => start = Some(app.to_string()),
        Some(other) => {
            let what = if other.starts_with('-') { format!("unknown option {other}") } else { format!("no app called '{other}'") };
            eprintln!("oriel: {what}\n\n{}", help(&config::prefix(&cfg)));
            std::process::exit(2);
        }
        None => {}
    }

    let (tx, rx) = mpsc::channel();
    // drop keystrokes typed before oriel started (e.g. the Enter that launched it)
    while crossterm::event::poll(std::time::Duration::ZERO).unwrap_or(false) {
        let _ = crossterm::event::read();
    }
    let mut terminal = ratatui::init();
    execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste, EnableFocusChange)?;
    // keep the terminal's own title to put back at exit (terminals with a title stack; others ignore it)
    let _ = execute!(std::io::stdout(), crossterm::style::Print("\x1b[22;0t"));
    // terminals with the kitty keyboard protocol can tell shift+enter from enter (a new line in the chat box).
    // Asked before the input thread starts reading, which would swallow the answer.
    let enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false)
        && execute!(std::io::stdout(), PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)).is_ok();
    // ratatui's panic hook only undoes raw mode and the alternate screen: turn our modes off too, or a crash leaves
    // the shell printing mouse and focus escape codes
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if enhanced {
            let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
        }
        let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste, DisableFocusChange, crossterm::terminal::SetTitle(""), crossterm::style::Print("\x1b[23;0t"));
        prev(info);
    }));
    let input_tx = tx.clone();
    std::thread::spawn(move || {
        while let Ok(ev) = crossterm::event::read() {
            if input_tx.send(pane::Event::Input(ev)).is_err() {
                break;
            }
        }
    });
    let tour = args.first().map(String::as_str) == Some("--tour");
    let mut app = app::App::with_start(cfg, tx, start);
    if let Some(e) = broken {
        app.config_error(e);
    }
    if tour {
        app.start_tour();
    }
    let res = app.run(&mut terminal, rx);
    if enhanced {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    // the title oriel set ("oriel · 1 needs you") goes: empty resets it to the terminal's default, and the pop
    // brings back the one from before where the terminal keeps a stack
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste, DisableFocusChange, crossterm::terminal::SetTitle(""), crossterm::style::Print("\x1b[23;0t"));
    ratatui::restore();
    res
}
