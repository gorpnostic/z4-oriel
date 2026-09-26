//! oriel — a fast terminal workspace: tabs, tmux-style split panes, and built-in apps.

mod alerts;
mod app;
mod clip;
mod config;
mod font;
mod layout;
mod onboard;
mod pane;
mod panes;
mod testkit;
mod theme;
mod ui;
mod update;

use crossterm::{
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
};
use std::sync::mpsc;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut cfg = config::load();
    match args.first().map(String::as_str) {
        Some("-h" | "--help") => {
            println!(
                "oriel {} — a terminal workspace\n\n  oriel              open (starts in the ai app)\n  oriel <app>        open straight into an app: ai agents ais terminal claude codex music system files notes calendar storage themes help\n  oriel --config     print the config file path\n  oriel update       update to the latest release (keeps this one for rollback)\n  oriel rollback     go back to the version before the last update\n  oriel changelog    what's new in recent releases\n  oriel --tour       replay the first-run setup and tour\n  oriel --version\n\nInside: F1-F9 apps, F10 help (every key and how-to), alt p palette, alt n terminal split, {} = tmux-style prefix.",
                env!("CARGO_PKG_VERSION"),
                cfg.prefix
            );
            return Ok(());
        }
        Some("-V" | "--version") => {
            println!("oriel {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("update" | "--update") => {
            // oriel updates itself (keeping this version for `oriel rollback`); the install script is the fallback
            let code = update::cli_update();
            if code != 0 && args.get(1).map(String::as_str) != Some("--no-fallback") {
                const RAW: &str = "https://raw.githubusercontent.com/gorpnostic/z4-oriel/master";
                println!(":: trying the install script instead");
                let status = if cfg!(windows) {
                    std::process::Command::new("powershell.exe")
                        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &format!("irm {RAW}/install.ps1 | iex")])
                        .status()
                } else {
                    std::process::Command::new("sh").args(["-c", &format!("curl -fsSL {RAW}/install.sh | sh")]).status()
                };
                std::process::exit(status.map(|s| s.code().unwrap_or(1)).unwrap_or(1));
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
        Some(app) => cfg.startup = app.to_string(),
        None => {}
    }

    let (tx, rx) = mpsc::channel();
    // drop keystrokes typed before oriel started (e.g. the Enter that launched it)
    while crossterm::event::poll(std::time::Duration::ZERO).unwrap_or(false) {
        let _ = crossterm::event::read();
    }
    let input_tx = tx.clone();
    std::thread::spawn(move || {
        while let Ok(ev) = crossterm::event::read() {
            if input_tx.send(pane::Event::Input(ev)).is_err() {
                break;
            }
        }
    });

    let mut terminal = ratatui::init();
    execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste, crossterm::event::EnableFocusChange)?;
    let tour = args.first().map(String::as_str) == Some("--tour");
    let mut app = app::App::new(cfg, tx);
    if tour {
        app.start_tour();
    }
    let res = app.run(&mut terminal, rx);
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    res
}
