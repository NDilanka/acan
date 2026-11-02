# Installs the acan alpha on Windows (x64).
#
# It downloads the newest release (pre-releases included), checks the archive's SHA256 against
# the release's SHA256SUMS, and installs acan.exe and its web folder under
# %LOCALAPPDATA%\Programs\acan. It adds that folder to your user PATH. It prints what you need
# next. It changes nothing else: no config, no keys, no other environment variables.
#
#   irm https://raw.githubusercontent.com/NDilanka/acan/main/install.ps1 | iex
#
# Optional: set $env:ACAN_INSTALL_TAG = 'v0.1.0-alpha.1' to install that tag instead of the newest.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repo = 'NDilanka/acan'
$target = 'x86_64-pc-windows-msvc'

if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne 'X64') {
    throw 'There is no Windows build for this architecture yet. The alpha ships x64 only.'
}

$tag = $env:ACAN_INSTALL_TAG
if (-not $tag) {
    $releases = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases?per_page=1" `
        -Headers @{ Accept = 'application/vnd.github+json' }
    $tag = $releases[0].tag_name
}
if (-not $tag) { throw "No release found for $repo. GitHub may be rate limiting; try again later." }

$base = "https://github.com/$repo/releases/download/$tag"
$archive = "acan-$target.zip"
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("acan-install-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null

try {
    Write-Host "Downloading acan $tag for $target"
    $zip = Join-Path $tmp $archive
    Invoke-WebRequest -Uri "$base/$archive" -OutFile $zip
    $sums = Join-Path $tmp 'SHA256SUMS'
    Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile $sums

    $line = Get-Content $sums | Where-Object { $_ -match ('\s' + [regex]::Escape($archive) + '$') } |
        Select-Object -First 1
    if (-not $line) { throw "$archive is not listed in SHA256SUMS. Refusing to install." }
    $expected = ($line -split '\s+')[0].ToLowerInvariant()
    $actual = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        throw "SHA256 mismatch for $archive (expected $expected, got $actual). Nothing was installed."
    }
    Write-Host "SHA256 ok: $archive"

    Expand-Archive -Path $zip -DestinationPath $tmp
    $stage = Join-Path $tmp "acan-$target"
    if (-not (Test-Path (Join-Path $stage 'acan.exe'))) { throw 'The archive has no acan.exe.' }
    if (-not (Test-Path (Join-Path $stage 'web'))) { throw 'The archive has no web folder.' }

    $installDir = Join-Path $env:LOCALAPPDATA 'Programs\acan'
    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    Copy-Item -Path (Join-Path $stage 'acan.exe') -Destination (Join-Path $installDir 'acan.exe') -Force
    $installedWorker = Test-Path (Join-Path $stage 'acan-worker.exe')
    if ($installedWorker) {
        Copy-Item -Path (Join-Path $stage 'acan-worker.exe') -Destination (Join-Path $installDir 'acan-worker.exe') -Force
    }
    $web = Join-Path $installDir 'web'
    if (Test-Path $web) { Remove-Item -Recurse -Force $web }
    Copy-Item -Path (Join-Path $stage 'web') -Destination $web -Recurse
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$entries = @()
if ($userPath) { $entries = $userPath -split ';' | Where-Object { $_ } }
if ($entries -notcontains $installDir) {
    $newPath = (@($entries) + $installDir) -join ';'
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    Write-Host "Added $installDir to your user PATH. Open a new terminal to use it."
}

Write-Host ''
Write-Host "Installed acan ${tag}: $(Join-Path $installDir 'acan.exe') (web UI in $web)"
if ($installedWorker) {
    Write-Host "Installed $(Join-Path $installDir 'acan-worker.exe') (optional): the built-in worker; agents can also connect over MCP."
}
Write-Host ''
Write-Host 'Before you run it:'
Write-Host "  - Git: 'git --version' must work, and your project must be a git repository with a commit."
Write-Host "  - Docker Desktop: 'docker version' must work. Candidate code runs in containers."
Write-Host '  - A model key: bring your own key for the provider you use. Keys stay on your machine and'
Write-Host '    are never sent anywhere else by acan. Without a routing file, acan answers with labelled'
Write-Host '    offline replies. The README explains the key setup.'
Write-Host '  - The binary is not code-signed, so Windows may warn the first time you run it.'
Write-Host ''
Write-Host 'Start it on your project:'
Write-Host '  acan serve --repo C:\path\to\your\project'
Write-Host ''
Write-Host 'Then open the address it prints. Stop it with Ctrl+C.'
