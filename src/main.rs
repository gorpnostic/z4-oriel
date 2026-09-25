//! oriel — a fast terminal workspace: tabs, tmux-style split panes, and built-in apps.

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
                "oriel {} — a terminal workspace\n\n  oriel              open (starts in the ai app)\n  oriel <app>        open straight into an app: ai agents ais terminal claude codex music system files notes storage\n  oriel --config     print the config file path\n  oriel update       update to the latest release\n  oriel --version\n\nInside: F1-F9 apps, alt p palette, alt n terminal split, {} then ? = all keys.",
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
            // re-run the installer: it fetches the latest release and swaps the binary (even this running one)
            const RAW: &str = "https://raw.githubusercontent.com/gorpnostic/z4-oriel/master";
            println!("oriel {} — updating to the latest release…", env!("CARGO_PKG_VERSION"));
            let status = if cfg!(windows) {
                std::process::Command::new("powershell.exe")
                    .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &format!("irm {RAW}/install.ps1 | iex")])
                    .status()
            } else {
                std::process::Command::new("sh").args(["-c", &format!("curl -fsSL {RAW}/install.sh | sh")]).status()
            };
            std::process::exit(status.map(|s| s.code().unwrap_or(1)).unwrap_or(1));
        }
        Some("--mcp-approve") => {
            // the approval bridge Claude Code starts for /perms ask (see panes/chat/approve.rs)
            panes::chat::approve::serve_stdio(args.get(1).map(String::as_str).unwrap_or(""), args.get(2).map(String::as_str).unwrap_or(""));
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
    execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;
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
