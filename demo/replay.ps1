# Replays the recorded provider receipts in receipts/ without any API key.
#
# It checks each receipt against receipts/SHA256SUMS, then prints the receipts. It reads no keys and
# makes no network calls, so a judge can run it on a machine with no provider accounts.
#
# Runs on PowerShell 7 on Windows and Linux (macOS not tested): pwsh -File demo/replay.ps1
# Exit code 0 when every receipt matches its checksum, 1 otherwise.

[CmdletBinding()]
param(
    # The folder with the receipts and SHA256SUMS. Defaults to receipts/ at the repository root.
    [string]$ReceiptsDir = (Join-Path (Split-Path -Parent $PSScriptRoot) 'receipts'),
    # Check the checksums only. Without this switch the receipts are printed after the check.
    [switch]$VerifyOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ($PSVersionTable.PSVersion.Major -lt 7) {
    Write-Output 'replay.ps1 needs PowerShell 7 (pwsh).'
    exit 1
}

$sums = Join-Path $ReceiptsDir 'SHA256SUMS'
if (-not (Test-Path -LiteralPath $sums)) {
    Write-Output "missing checksum file: $sums"
    exit 1
}

$failures = 0
$names = @()
foreach ($line in Get-Content -LiteralPath $sums) {
    if (-not $line.Trim()) { continue }
    $parts = $line.Trim() -split '\s+', 2
    $expected = $parts[0].ToLowerInvariant()
    $name = $parts[1]
    $names += $name
    $path = Join-Path $ReceiptsDir $name
    if (-not (Test-Path -LiteralPath $path)) {
        Write-Output "MISSING   $name"
        $failures++
        continue
    }
    $actual = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -eq $expected) {
        Write-Output "OK        $name"
    } else {
        Write-Output "MISMATCH  $name"
        $failures++
    }
}

if ($failures -gt 0) {
    Write-Output "$failures receipt(s) failed the check. Do not use them."
    exit 1
}
Write-Output "all $($names.Count) receipts match SHA256SUMS"

if (-not $VerifyOnly) {
    foreach ($name in $names) {
        Write-Output ''
        Write-Output ('=' * 78)
        Write-Output "receipt: $name"
        Write-Output ('=' * 78)
        Get-Content -LiteralPath (Join-Path $ReceiptsDir $name)
    }
}

exit 0
