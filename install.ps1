# Installs the Acan app on Windows (x64) for the current user, with no admin rights.
#
# It downloads the newest release (pre-releases included), checks the setup's SHA256 against the
# release's SHA256SUMS, and runs the setup silently. The setup installs to
# %LOCALAPPDATA%\Programs\Acan and adds that folder to your user PATH. It does not start Acan.
#
#   irm https://raw.githubusercontent.com/NDilanka/acan/main/install.ps1 | iex
#
# Optional: set $env:ACAN_INSTALL_TAG = 'v0.1.0-pre.1' to install that tag instead of the newest.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repo = 'NDilanka/acan'

if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne 'X64') {
    throw 'There is no Windows build for this architecture yet. The pre-release ships x64 only.'
}

$tag = $env:ACAN_INSTALL_TAG
if (-not $tag) {
    $releases = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases?per_page=1" `
        -Headers @{ Accept = 'application/vnd.github+json' }
    $tag = $releases[0].tag_name
}
if (-not $tag) { throw "No release found for $repo. GitHub may be rate limiting; try again later." }
$version = $tag.TrimStart('v')

$base = "https://github.com/$repo/releases/download/$tag"
$setupName = "acan-setup-$version-windows-x64.exe"
$tmp = Join-Path ([IO.Path]::GetTempPath()) ('acan-install-' + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null

try {
    Write-Host "Downloading Acan $tag"
    $setup = Join-Path $tmp $setupName
    Invoke-WebRequest -Uri "$base/$setupName" -OutFile $setup
    $sums = Join-Path $tmp 'SHA256SUMS'
    Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile $sums

    $line = Get-Content $sums | Where-Object { $_ -match ('\s' + [regex]::Escape($setupName) + '$') } |
        Select-Object -First 1
    if (-not $line) { throw "$setupName is not listed in SHA256SUMS. Refusing to install." }
    $expected = ($line -split '\s+')[0].ToLowerInvariant()
    $actual = (Get-FileHash -Algorithm SHA256 -Path $setup).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        throw "SHA256 mismatch for $setupName (expected $expected, got $actual). Nothing was installed."
    }
    Write-Host "SHA256 ok: $setupName"

    $run = Start-Process -FilePath $setup -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/NOLAUNCH=1' -Wait -PassThru
    if ($run.ExitCode -ne 0) { throw "The setup exited with code $($run.ExitCode)." }
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

Write-Host ''
Write-Host "Installed Acan $tag to $(Join-Path $env:LOCALAPPDATA 'Programs\Acan')"
Write-Host 'Open Acan from the Start menu, or run acan in a new terminal.'
Write-Host 'Acan needs Git on your PATH. Keys are optional; the README explains them.'
