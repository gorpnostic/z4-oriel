# oriel

A fast terminal workspace that looks like nest, rebuilt in Rust. A sidebar holds your apps: ai chat, music,
system, files, notes, storage and terminal. You can also split any tab into tmux-style panes. It's one binary
that runs on Windows and Linux, and themes itself live from Omarchy.

It replaces `nest` (`C:\Code\ai\wren`), the Python/Textual version, which was slow to open and to switch tabs.

## Install

Linux (Omarchy/Arch, Ubuntu, …):
```bash
curl -fsSL https://raw.githubusercontent.com/gorpnostic/z4-oriel/master/install.sh | sh
```
Windows (PowerShell):
```powershell
irm https://raw.githubusercontent.com/gorpnostic/z4-oriel/master/install.ps1 | iex
```
**Update later:** `oriel update`, which re-runs the installer for the latest release.

Icons need a Nerd Font in the terminal. Omarchy ships one. On Windows, use "Cascadia Mono NF".

## Run from source

```powershell
cargo run --release            # or: oriel <app>, e.g. oriel music
```

## Using it

The sidebar works like nest's. Click an app or press its F-key. Everything is clickable.

| Key | Does |
|---|---|
| `F1`–`F7` | ai · music · system · files · notes · storage · terminal |
| `F8` | play / pause music from anywhere |
| `alt t` / click **new tab** | a new tab of your own (it starts on a launcher) |
| `alt 1-9` | switch between your own tabs |
| `alt n` | new terminal split beside the current pane (`alt enter` also works outside Windows Terminal, which takes it for fullscreen) |
| `alt ←↑↓→` / `alt shift ←↑↓→` | move between panes / resize |
| `alt z` · `alt w` · `alt s` | zoom pane · close pane · hide the sidebar |
| `alt p` | palette: every app, split, theme, command |
| `ctrl+space` then `\|` `-` `hjkl` `x` `z` `c` `t` `?` | tmux-style prefix keys (`?` lists them) |

Each app shows its own keys in the hint line at the bottom, like nest did.

**ai:** chat with Wren (through its web server: research, memory, fast/balanced/smart), Claude Code, Codex,
Ollama, any OpenAI-compatible server, or the Anthropic API. `/model` switches between them. Your nest chats are
copied in on first run.

## Themes

`alt p` then type "theme". The palettes come from nest. `terminal` follows the terminal's own colours.
`omarchy` reads `~/.config/omarchy/current/theme` and recolours live when you switch Omarchy themes. It's the
default on Omarchy.

## Config

`oriel --config` prints the path: `%APPDATA%\oriel\config.toml` on Windows, `~/.config/oriel/config.toml` on Linux.
You can set the theme, shell, prefix key, startup app, AI providers and keys, Wren server URLs, and music folders.

## Releasing (for Leif)

`pwsh tools\release.ps1` bumps the version, tags and pushes. GitHub Actions then builds the Linux and Windows
binaries and publishes the release that the installers download (`.github/workflows/release.yml`).

## Testing

`cargo test` renders the apps off-screen (`src/testkit.rs`). `pwsh tools\snap.ps1 target\snap\<name>.html` turns
a snapshot into a PNG. Nothing opens on screen.

## Status (2026-09-24)

- Done: the nest-style shell, splits, terminal panes, the palette, themes with live Omarchy reload, ai chat with
  six providers, installers, `oriel update`, and the release workflow.
- Being built now: music, system, files, notes, storage.
- Not yet tested: on real Linux/Omarchy (it can't be tested here: WSL can't run on this PC), and the first
  GitHub release.
