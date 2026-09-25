# Turn an HTML snapshot written by the tests (testkit::save_html) into a PNG with headless Chrome.
# Nothing appears on screen.   pwsh tools\snap.ps1 target\snap\app.html   ->  target\snap\app.png
param([Parameter(Mandatory)][string]$Html, [int]$Width = 1500, [int]$Height = 1000)
$chrome = @("$env:ProgramFiles\Google\Chrome\Application\chrome.exe", "${env:ProgramFiles(x86)}\Google\Chrome\Application\chrome.exe",
            "${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $chrome) { "no Chrome/Edge found"; exit 1 }
$full = (Resolve-Path $Html).Path
$png = [IO.Path]::ChangeExtension($full, ".png")
$prof = Join-Path ([IO.Path]::GetTempPath()) ("oriel-snap-" + [guid]::NewGuid())
$p = Start-Process -FilePath $chrome -PassThru -WindowStyle Hidden -ArgumentList "--headless=new", "--disable-gpu",
    "--user-data-dir=`"$prof`"", "--disk-cache-size=1", "--hide-scrollbars", "--window-size=$Width,$Height",
    "--screenshot=`"$png`"", ("file:///" + ($full -replace '\\', '/'))
$p.WaitForExit(30000) | Out-Null
Remove-Item $prof -Recurse -Force -ErrorAction SilentlyContinue
$png
