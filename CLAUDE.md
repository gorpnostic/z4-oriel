# oriel — project rules

- **Every update = fresh README screenshots.** Whenever a change is shipped (released or pushed), run
  `pwsh tools\screenshots.ps1`, which regenerates all of `docs\screenshot-*.png` from the current code, and commit
  them with the change. `tools\release.ps1` already runs it before tagging. Screenshots use demo data only: no
  personal chats, files or usage.
- The repo is public (github.com/gorpnostic/z4-oriel). Nothing personal goes in: no Wren, no nest, no private
  server names, and no paths from Leif's machine in docs or fixtures.
- Test headlessly (`src/testkit.rs`, `tools/snap.ps1`). Never launch windows or send keystrokes to test. The
  user's desktop is in use.
- Release: `pwsh tools\release.ps1` (patch) / `-Minor`. GitHub Actions builds Linux + Windows; `oriel update`
  fetches it.
