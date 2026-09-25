//! oriel — a fast terminal workspace: tabs, tmux-style split panes, and built-in apps.

mod app;
mod config;
mod font;
mod layout;
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
                "oriel {} — a terminal workspace\n\n  oriel              open (home screen)\n  oriel <app>        open straight into an app: terminal ai claude codex music system files notes storage\n  oriel --config     print the config file path\n  oriel --version\n\nInside: alt p = palette, alt enter = new terminal, {} then ? = all keys.",
                env!("CARGO_PKG_VERSION"),
                cfg.prefix
            );
            return Ok(());
        }
        Some("-V" | "--version") => {
            println!("oriel {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("--config") => {
            println!("{}", config::path().display());
            return Ok(());
        }
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
    let mut app = app::App::new(cfg, tx);
    let res = app.run(&mut terminal, rx);
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    res
}
