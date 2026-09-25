# oriel

A fast terminal workspace. Tabs, tmux-style split panes, and built-in apps (terminal, AI chat, Claude Code, Codex,
music, system, files, notes, storage) in one Rust binary. It runs on Windows and Linux, and themes itself live from
Omarchy.

The Rust rewrite of `nest` (`C:\Code\ai\wren`), which was Python/Textual and too slow.

## Run it

```powershell
cargo run --release            # or: cargo build --release, then target\release\oriel.exe
oriel terminal                 # open straight into an app
oriel --help
```

## Keys

| Key | Does |
|---|---|
| `alt p` | palette: open any app, split, switch theme, everything |
| `alt n` (or `alt enter`) | new terminal, split from the current pane (Windows Terminal keeps alt+enter for fullscreen) |
| `alt ←↑↓→` / `alt shift ←↑↓→` | move between panes / resize |
| `alt 1-9`, `alt t` | switch tab, new tab |
| `alt z`, `alt w` | zoom pane, close pane |
| `ctrl+space` then `\|` `-` `hjkl` `x` `z` `c` `n/p` `t` `?` | tmux-style: split right/down, move, close, zoom, tab, next/prev, themes, help |
| mouse | click to focus, drag a divider to resize, wheel scrolls |

## Themes

`alt p` then type "theme". The built-in palettes come from nest. `terminal` uses only ANSI colours, so it follows
the terminal's own theme. `omarchy` reads `~/.config/omarchy/current/theme` and live-reloads when you switch
Omarchy themes. It's the default on Omarchy machines.

## Config

`oriel --config` prints the path: `%APPDATA%\oriel\config.toml` on Windows, `~/.config/oriel/config.toml` on Linux.
You can set the theme, shell, prefix key, startup app, AI providers and music folders there.

## Status (2026-09-24)

- Done: the core (tabs, splits, terminal panes on ConPTY/pty, palette, prefix keys, mouse, themes with live Omarchy
  reload), the home screen, and Claude Code / Codex as terminal panes.
- Placeholders for now: ai chat, music, system, files, notes, storage.
- Not done yet: Linux/Omarchy testing, install scripts, release binaries.
