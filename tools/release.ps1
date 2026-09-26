# Publish a new oriel version: bumps the version, commits, tags and pushes. GitHub then builds the Linux and
# Windows binaries and creates the release (.github/workflows/release.yml); `oriel update` picks it up.
#   pwsh tools\release.ps1            # 0.1.0 -> 0.1.1
#   pwsh tools\release.ps1 -Minor     # 0.1.0 -> 0.2.0
#   pwsh tools\release.ps1 -Version 1.0.0
param([string]$Version = "", [switch]$Minor, [switch]$Major)
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path $PSScriptRoot -Parent)
if (git status --porcelain) { throw "commit or stash your changes first" }
# no release with failing tests
cargo test
if ($LASTEXITCODE) { throw "tests failed - not releasing" }
# every release ships fresh README screenshots
pwsh -NoProfile -File tools\screenshots.ps1
if ($LASTEXITCODE) { throw "screenshots failed" }
$toml = Get-Content Cargo.toml -Raw
$cur = [regex]::Match($toml, '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
if (-not $Version) {
    $p = $cur.Split('.') | ForEach-Object { [int]$_ }
    if ($Major) { $p = @(($p[0] + 1), 0, 0) } elseif ($Minor) { $p = @($p[0], ($p[1] + 1), 0) } else { $p = @($p[0], $p[1], ($p[2] + 1)) }  # parens: "a, b + 1" would append 1 as a 4th part
    $Version = $p -join '.'
}
$toml = [regex]::Replace($toml, '(?m)^version\s*=\s*"[^"]+"', "version = `"$Version`"", 1)
[IO.File]::WriteAllText((Resolve-Path Cargo.toml), $toml)
cargo update --workspace --offline   # just refreshes Cargo.lock with the new version (the workflow builds with --locked)
git add Cargo.toml Cargo.lock docs
git commit -m "release v$Version"
# release notes: what changed since the last version, from the commit subjects (the workflow publishes them,
# and oriel shows them under "what's new")
$prev = git describe --tags --abbrev=0 "HEAD^" 2>$null
$range = if ($prev) { "$prev..HEAD" } else { "HEAD" }
$lines = git log $range --no-merges --pretty=format:%s | Where-Object { $_ -and $_ -notmatch '^release v' -and $_ -notmatch '^Screenshots' }
$notes = if ($lines) { ($lines | ForEach-Object { "- $_" }) -join "`n" } else { "- small fixes" }
$notesFile = Join-Path ([IO.Path]::GetTempPath()) "oriel-notes-$Version.md"
[IO.File]::WriteAllText($notesFile, $notes)
git tag -a "v$Version" -F $notesFile
Remove-Item $notesFile
git push
git push origin "v$Version"
"v$Version pushed. GitHub is building it: https://github.com/gorpnostic/z4-oriel/actions"
