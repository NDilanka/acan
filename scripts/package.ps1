<#
.SYNOPSIS
    Packages one platform's acan archive and checks what it holds.

.DESCRIPTION
    The archive is a folder named acan-<target> holding acan (acan.exe on Windows), acan-worker
    (acan-worker.exe on Windows), web/, README.md and LICENSE. A Windows target gets a zip. Any
    other target gets a tar.gz. The layout is checked before the archive is written, so a missing
    file fails the build. Run with -SelfTest to package fake files and check the archives written.

.EXAMPLE
    ./scripts/package.ps1 -Target x86_64-unknown-linux-gnu -BinDir target/x86_64-unknown-linux-gnu/release -WebDir dist/web -OutDir dist

.EXAMPLE
    ./scripts/package.ps1 -SelfTest
#>
[CmdletBinding()]
param(
    [string]$Target,
    [string]$BinDir,
    [string]$WebDir,
    [string]$OutDir,
    [string]$RepoRoot,
    [switch]$SelfTest
)

$ErrorActionPreference = 'Stop'
# Windows PowerShell 5.1 leaves $PSScriptRoot empty in a parameter default, so the default is set here.
if (-not $RepoRoot) { $RepoRoot = Split-Path -Parent $PSScriptRoot }

function Test-WindowsTarget([string]$Target) {
    $Target -like '*windows*'
}

# The files an archive must hold, relative to its folder. The binaries carry .exe on Windows.
function Get-Members([string]$Target) {
    $exe = if (Test-WindowsTarget $Target) { '.exe' } else { '' }
    @("acan$exe", "acan-worker$exe", 'web', 'README.md', 'LICENSE')
}

function Assert-Layout([string]$Stage, [string]$Target) {
    $missing = @(Get-Members $Target | Where-Object { -not (Test-Path -LiteralPath (Join-Path $Stage $_)) })
    if ($missing.Count -gt 0) {
        throw "the archive for $Target is missing: $($missing -join ', ')"
    }
}

# Stages the members in OutDir/acan-<target> and writes the archive beside it. Returns the archive's path.
function New-Archive {
    param(
        [string]$Target,
        [string]$BinDir,
        [string]$WebDir,
        [string]$OutDir,
        [string]$RepoRoot
    )
    $name = "acan-$Target"
    $stage = Join-Path $OutDir $name
    if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $stage | Out-Null

    $exe = if (Test-WindowsTarget $Target) { '.exe' } else { '' }
    foreach ($bin in 'acan', 'acan-worker') {
        $source = Join-Path $BinDir "$bin$exe"
        if (-not (Test-Path -LiteralPath $source)) { throw "missing binary: $source" }
        Copy-Item -LiteralPath $source -Destination (Join-Path $stage "$bin$exe")
    }
    Copy-Item -LiteralPath $WebDir -Destination (Join-Path $stage 'web') -Recurse
    Copy-Item -LiteralPath (Join-Path $RepoRoot 'README.md'), (Join-Path $RepoRoot 'LICENSE') -Destination $stage
    Assert-Layout -Stage $stage -Target $Target

    if (Test-WindowsTarget $Target) {
        $archive = Join-Path $OutDir "$name.zip"
        if (Test-Path -LiteralPath $archive) { Remove-Item -LiteralPath $archive }
        Compress-Archive -Path $stage -DestinationPath $archive
    }
    else {
        $archive = Join-Path $OutDir "$name.tar.gz"
        tar -C $OutDir -czf $archive $name
        if ($LASTEXITCODE -ne 0) { throw "tar failed for $archive" }
    }
    $archive
}

function Get-ArchiveNames([string]$Archive, [string]$Target) {
    if (Test-WindowsTarget $Target) {
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $zip = [IO.Compression.ZipFile]::OpenRead($Archive)
        try { @($zip.Entries.FullName -replace '\\', '/') } finally { $zip.Dispose() }
    }
    else {
        @(tar -tzf $Archive)
    }
}

function Invoke-SelfTest {
    $work = Join-Path ([IO.Path]::GetTempPath()) ('acan-package-test-' + [guid]::NewGuid())
    try {
        foreach ($target in 'x86_64-unknown-linux-gnu', 'x86_64-pc-windows-msvc') {
            $exe = if (Test-WindowsTarget $target) { '.exe' } else { '' }
            $bin = Join-Path $work "bin-$target"
            $web = Join-Path $work "web-$target"
            $out = Join-Path $work "out-$target"
            New-Item -ItemType Directory -Force -Path $bin, (Join-Path $web 'pkg'), $out | Out-Null
            'acan' | Set-Content -LiteralPath (Join-Path $bin "acan$exe")
            'acan-worker' | Set-Content -LiteralPath (Join-Path $bin "acan-worker$exe")
            '<!doctype html>' | Set-Content -LiteralPath (Join-Path $web 'index.html')

            $archive = New-Archive -Target $target -BinDir $bin -WebDir $web -OutDir $out -RepoRoot $RepoRoot
            $names = Get-ArchiveNames -Archive $archive -Target $target
            $prefix = "acan-$target/"
            foreach ($member in @("acan$exe", "acan-worker$exe", 'web/index.html', 'README.md', 'LICENSE')) {
                if ($names -notcontains "$prefix$member") {
                    throw "$(Split-Path -Leaf $archive) does not hold $prefix$member"
                }
            }

            # An archive without the worker is refused, not made.
            Remove-Item -LiteralPath (Join-Path $bin "acan-worker$exe")
            $refused = $false
            try {
                New-Archive -Target $target -BinDir $bin -WebDir $web -OutDir $out -RepoRoot $RepoRoot | Out-Null
            }
            catch {
                $refused = $_.Exception.Message -like '*acan-worker*'
            }
            if (-not $refused) { throw "an archive for $target was made without acan-worker" }
            Write-Output "ok: $(Split-Path -Leaf $archive) holds acan, acan-worker, web/ and the notices"
        }
    }
    finally {
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    }
}

if ($SelfTest) {
    Invoke-SelfTest
    return
}
foreach ($required in 'Target', 'BinDir', 'WebDir', 'OutDir') {
    if (-not (Get-Variable -Name $required -ValueOnly)) { throw "-$required is required (or use -SelfTest)" }
}
New-Archive -Target $Target -BinDir $BinDir -WebDir $WebDir -OutDir $OutDir -RepoRoot $RepoRoot
