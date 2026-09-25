# oriel installer for Windows.
#   irm https://raw.githubusercontent.com/gorpnostic/z4-oriel/master/install.ps1 | iex
# Re-running it (or `oriel update`) updates to the latest release.
$ErrorActionPreference = 'Stop'
$repo = 'gorpnostic/z4-oriel'
$dir = Join-Path $env:LOCALAPPDATA 'oriel\bin'
$url = "https://github.com/$repo/releases/latest/download/oriel-windows-x86_64.zip"

function Say($m) { Write-Host ':: ' -ForegroundColor Yellow -NoNewline; Write-Host $m }

if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64') { throw "no prebuilt oriel for $env:PROCESSOR_ARCHITECTURE yet; build from source: cargo install --git https://github.com/$repo" }

New-Item -ItemType Directory -Force $dir | Out-Null
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("oriel-" + [guid]::NewGuid())
New-Item -ItemType Directory $tmp | Out-Null
try {
    Say 'downloading oriel'
    $ProgressPreference = 'SilentlyContinue'
    Invoke-WebRequest $url -OutFile "$tmp\oriel.zip" -UseBasicParsing
    Expand-Archive "$tmp\oriel.zip" -DestinationPath $tmp -Force
    $exe = Join-Path $dir 'oriel.exe'
    # a running oriel.exe can't be overwritten, but it can be renamed out of the way. Each old copy gets its own
    # name, because an older one may still be running (and locked) too.
    Get-ChildItem $dir -Filter 'oriel.exe.old*' -ErrorAction SilentlyContinue | ForEach-Object { Remove-Item $_.FullName -Force -ErrorAction SilentlyContinue }
    if (Test-Path $exe) {
        Rename-Item $exe ("oriel.exe.old-" + (Get-Date -Format 'yyyyMMddHHmmss')) -Force
    }
    Copy-Item "$tmp\oriel.exe" $exe -Force
    Unblock-File $exe
} finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $dir) {
    [Environment]::SetEnvironmentVariable('Path', "$userPath;$dir".TrimStart(';'), 'User')
    $env:Path = "$env:Path;$dir"
    Say "added $dir to your PATH (new terminals pick it up)"
}
$v = & (Join-Path $dir 'oriel.exe') --version
Say "installed $v — run it with: oriel"
Say 'icons need a Nerd Font in your terminal, e.g. "Cascadia Mono NF" (Windows Terminal: Settings > Profiles > Appearance)'
