# Developing oriel

## Layout

| Path | What |
|---|---|
| `src/app.rs` | the workspace: sidebar, tabs, split tree, focus, keys, palette, mouse, themes |
| `src/pane.rs` | the `Pane` trait every app implements |
| `src/panes/` | the apps: `chat` (+ `providers`, `store`, `md`), `agents` (board + lead mode), `ais` (your AIs), `music`, `system`, `files`, `notes`, `storage`, `term` (the terminal), `home`, `help` (F10: the in-app guide; update it when keys or commands change) |
| `src/onboard.rs` | first-run setup pages and the interactive tour (`STEPS`) |
| `src/clip.rs` | copy to the clipboard, and the alt+v image grab |
| `src/theme.rs` | palettes, the ANSI `terminal` theme, the live Omarchy reader |
| `src/ui.rs`, `src/font.rs` | shared drawing helpers, icons, the block-letter logo font |
| `src/testkit.rs` | headless rendering for tests |

## Testing

```bash
cargo test
```

Tests render panes off-screen through ratatui's `TestBackend`, so nothing opens on screen.
`testkit::save_html` writes a coloured snapshot, and `pwsh tools/snap.ps1 target/snap/<name>.html` turns it into a
PNG with headless Chrome. Tests that need a network or a running server are `#[ignore]`d. Run them with
`cargo test <name> -- --ignored`.

`ORIEL_DATA_DIR=<folder>` runs oriel with a separate profile (chats, notes, memory), which is handy for demos.
`ORIEL_LOG=<file>` logs every input event.

README screenshots: `pwsh tools/screenshots.ps1` regenerates all of `docs/screenshot-*.png` (demo data only). `tools/release.ps1` runs it on every release, so the GitHub page is always current.

## Releasing

```powershell
pwsh tools/release.ps1            # 0.1.2 -> 0.1.3   (-Minor / -Major / -Version x.y.z)
```

The script bumps the version in `Cargo.toml`, commits, tags and pushes. `.github/workflows/release.yml` then builds
Linux (on Ubuntu 22.04, for an older glibc) and Windows, and publishes the release with `install.sh` / `install.ps1`
attached. `oriel update` and the install one-liners always fetch the latest release.
