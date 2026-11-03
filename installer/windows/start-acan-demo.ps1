<#
.SYNOPSIS
    Starts the Acan Demo on this PC.

.DESCRIPTION
    Each start clones the bundled demo repository into a new folder under
    %LOCALAPPDATA%\Acan Demo\runs, so the bundled copy is never changed. The server runs in its own
    console window, and the browser opens on it. Close that window to stop the server.

    -Mode replay opens the recorded run, which needs no keys. -Mode live opens the live page with the
    owner's link. The default is replay until the offline scope draft ships, then live.
#>
[CmdletBinding()]
param(
    [ValidateSet('default', 'live', 'replay')]
    [string]$Mode = 'default'
)

$ErrorActionPreference = 'Stop'
# Change to 'live' once the offline scope draft is in the build.
$DefaultMode = 'replay'
if ($Mode -eq 'default') { $Mode = $DefaultMode }
$port = if ($env:ACAN_DEMO_PORT) { [int]$env:ACAN_DEMO_PORT } else { 7311 }

try {
    $app = $PSScriptRoot
    $exe = Join-Path $app 'acan.exe'
    $bundle = Join-Path $app 'acan-demo.bundle'
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
        throw 'Acan needs Git. Install it from https://git-scm.com/download/win, then start the demo again.'
    }
    if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
        Write-Host 'Note: Docker Desktop is not on PATH. The replay still works; live work needs Docker.'
    }

    $runs = Join-Path $env:LOCALAPPDATA 'Acan Demo\runs'
    New-Item -ItemType Directory -Force -Path $runs | Out-Null
    $repo = Join-Path $runs (Get-Date -Format 'yyyyMMdd-HHmmss')
    # The bundle carries main only, so the clone names it. Without --branch the clone's HEAD is
    # master, which the bundle lacks, and the copy comes out with no files.
    git clone --quiet --branch main -- $bundle $repo
    if ($LASTEXITCODE -ne 0) { throw 'Could not copy the demo repository.' }

    $base = "http://127.0.0.1:$port"
    # Start-Process joins an array without quotes, so the repo folder is quoted here. Windows user
    # folders can hold spaces.
    $server = Start-Process -FilePath $exe -ArgumentList ('serve --repo "{0}" --port {1}' -f $repo, $port) -WorkingDirectory $app -PassThru

    $ready = $false
    for ($i = 0; $i -lt 120 -and -not $ready -and -not $server.HasExited; $i++) {
        Start-Sleep -Milliseconds 500
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$base/" -TimeoutSec 2 | Out-Null
            $ready = $true
        }
        catch { }
    }
    if (-not $ready) { throw "The server did not answer on $base. Its window shows why." }

    $url = "$base/?replay=1"
    if ($Mode -eq 'live') {
        $tokens = Join-Path $repo '.acan\tokens.json'
        for ($i = 0; $i -lt 20 -and -not (Test-Path -LiteralPath $tokens); $i++) { Start-Sleep -Milliseconds 250 }
        $owner = Get-Content -Raw -LiteralPath $tokens | ConvertFrom-Json |
            Where-Object { $_.principal.kind -eq 'member' -and $_.principal.id -eq 'owner' } |
            Select-Object -First 1
        if (-not $owner) { throw 'The owner token is not in the demo copy.' }
        $url = "$base/#token=$($owner.token)"
    }
    Start-Process $url

    Write-Host "Acan Demo is running in $Mode mode at $base."
    Write-Host "Demo copy: $repo"
    Write-Host 'Close the Acan server window to stop it.'
}
catch {
    Write-Host $_.Exception.Message -ForegroundColor Red
    Read-Host 'Press Enter to close'
    exit 1
}
