# Regenerate every README screenshot in docs\ from the current code. Run on every release (tools\release.ps1 does).
# Nothing appears on screen: the app renders off-screen and headless Chrome turns it into PNGs.
#   pwsh tools\screenshots.ps1
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path $PSScriptRoot -Parent)

# demo data only: chats come from a throwaway profile, the agents board and "your AIs" from test fixtures
cargo test docs_ -- --ignored --test-threads=1          # hero, agent transcript, system, app catalog
if ($LASTEXITCODE) { throw "screenshot tests failed" }
cargo test agents_snapshots_board_form_diff
if ($LASTEXITCODE) { throw "agents snapshot test failed" }
cargo test ais_snapshots
if ($LASTEXITCODE) { throw "ais snapshot test failed" }
Copy-Item target\snap\agents-board.html docs\screenshot-agents.html -Force
Copy-Item target\snap\ais-overview.html docs\screenshot-ais.html -Force

foreach ($n in 'hero', 'agent', 'agents', 'ais', 'system', 'apps') {
    pwsh -NoProfile -File tools\snap.ps1 "docs\screenshot-$n.html" -Width 1380 -Height 860 | Out-Null
    if (-not (Test-Path "docs\screenshot-$n.png")) { throw "screenshot-$n.png wasn't made" }
}
Remove-Item docs\*.html -ErrorAction SilentlyContinue
Get-ChildItem docs\*.png | ForEach-Object { "{0,-24} {1,8:N0} bytes  {2}" -f $_.Name, $_.Length, $_.LastWriteTime }
