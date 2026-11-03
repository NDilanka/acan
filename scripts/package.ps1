<#
.SYNOPSIS
    Stages one release file of the Acan app or the Acan Demo, and packs the Linux ones.

.DESCRIPTION
    Four files come from here: the app and the demo, each for Windows and Linux. A stage is a
    folder named for its file, such as acan-0.1.0-pre.1-linux-x64. A Linux stage is also packed
    into a tar.gz beside it. A Windows stage is the folder that the Inno Setup scripts package.

    Before anything is packed, the stage must hold every member, no file may carry a name that must
    not ship, and no text may match a pattern for a private path or a key. A failed check stops the
    build. The demo bundle is made from the demo repository at a pinned commit, and its tree and
    history get the same text check. Run with -SelfTest to check these rules on fake files.

.EXAMPLE
    ./scripts/package.ps1 -Kind app -Os linux -Version 0.1.0-pre.1 -BinDir target/release -WebDir dist/web -OutDir dist

.EXAMPLE
    ./scripts/package.ps1 -Kind demo -Os windows -Version 0.1.0-pre.1 -BinDir target/release -WebDir dist/web -ReplayDir replay -DemoRepo demo-src -DemoSha <full commit sha> -OutDir dist

.EXAMPLE
    ./scripts/package.ps1 -SelfTest
#>
[CmdletBinding()]
param(
    [ValidateSet('app', 'demo')]
    [string]$Kind,
    [ValidateSet('windows', 'linux')]
    [string]$Os,
    [string]$Version,
    [string]$BinDir,
    [string]$WebDir,
    [string]$ReplayDir,
    [string]$DemoRepo,
    [string]$DemoSha,
    [string]$DemoBundle,
    [string]$OutDir,
    [string]$RepoRoot,
    [switch]$SelfTest
)

$ErrorActionPreference = 'Stop'
# Windows PowerShell 5.1 leaves $PSScriptRoot empty in a parameter default, so the default is set here.
if (-not $RepoRoot) { $RepoRoot = Split-Path -Parent $PSScriptRoot }

# Patterns that must not ship in any file of a stage, or in the demo's tree and history. They are
# generic on purpose. The owner's private names are checked on the owner's machine before a release
# is published, so they are not written into this public file.
$PrivatePatterns = @(
    'D:\\dev',
    'C:\\Users\\(?!runneradmin\\)',
    '(META|BASETEN|FIREWORKS)_API_KEY\s*=\s*\S{8,}',
    '\bsk-[A-Za-z0-9]{32,}'
)
# A home folder other than the CI runner's. Checked in text files only, since a binary can hold
# such a string from a dependency.
$HomePattern = '/home/(?!runner/)'
$TextExtensions = @('.md', '.txt', '.sh', '.ps1', '.cmd', '.json', '.js', '.html', '.css', '.svg', '.toml', '.yml')
# File names that must not ship at all.
$ForbiddenNames = @('AGENTS.md', '.env', 'llm.toml', 'tokens.json', 'keys.toml')

function Get-StageName([string]$Kind, [string]$Version, [string]$Os) {
    $product = if ($Kind -eq 'demo') { 'acan-demo' } else { 'acan' }
    "$product-$Version-$Os-x64"
}

function Get-Exe([string]$Os) {
    if ($Os -eq 'windows') { '.exe' } else { '' }
}

# The launcher each file ships, relative to the repository root.
function Get-LauncherSource([string]$Kind, [string]$Os) {
    switch ("$Kind-$Os") {
        'app-windows' { 'installer/windows/acan-start.cmd' }
        'app-linux' { 'install.sh' }
        'demo-windows' { 'installer/windows/start-acan-demo.ps1' }
        'demo-linux' { 'installer/linux/start-acan-demo.sh' }
    }
}

# What a stage must hold, relative to its folder, with forward slashes.
function Get-Members([string]$Kind, [string]$Os) {
    $exe = Get-Exe $Os
    $launcher = Split-Path -Leaf (Get-LauncherSource $Kind $Os)
    $members = @("acan$exe", "acan-worker$exe", 'web/index.html', 'LICENSE', $launcher, 'FLAVOR')
    if ($Kind -eq 'demo') {
        $members += 'web/replay/manifest.json', 'web/replay/events.json', 'web/replay/plant.json', 'acan-demo.bundle', 'README.txt'
    }
    else {
        $members += 'README.md'
    }
    $members
}

function Assert-Members([string]$Stage, [string]$Kind, [string]$Os) {
    $missing = @(Get-Members $Kind $Os | Where-Object { -not (Test-Path -LiteralPath (Join-Path $Stage $_)) })
    if ($missing.Count -gt 0) {
        throw "$(Split-Path -Leaf $Stage) is missing: $($missing -join ', ')"
    }
}

# Throws when a file must not ship or matches a private pattern. Git metadata is skipped.
function Assert-Clean([string]$Root) {
    $files = @(Get-ChildItem -LiteralPath $Root -Recurse -File -Force |
        Where-Object { $_.FullName -notmatch '[\\/]\.git[\\/]' -and $_.Name -ne 'acan-demo.bundle' })
    $forbidden = @($files | Where-Object { $ForbiddenNames -contains $_.Name -or $_.Name -like 'screenshot*' })
    if ($forbidden.Count -gt 0) {
        throw "files that must not ship: $((@($forbidden | ForEach-Object { $_.Name })) -join ', ')"
    }
    foreach ($file in $files) {
        $text = [Text.Encoding]::UTF8.GetString([IO.File]::ReadAllBytes($file.FullName))
        $patterns = $PrivatePatterns
        if ($TextExtensions -contains $file.Extension) { $patterns = $PrivatePatterns + $HomePattern }
        foreach ($pattern in $patterns) {
            if ([regex]::IsMatch($text, $pattern)) {
                throw "$($file.Name) matches a private pattern: $pattern"
            }
        }
    }
}

# Makes the bundle of the demo repository at a pinned commit. The bundle's only branch is main.
function New-DemoBundle([string]$Repo, [string]$Sha, [string]$Out) {
    $work = Join-Path ([IO.Path]::GetTempPath()) ('acan-demo-' + [guid]::NewGuid())
    try {
        git clone --quiet --no-checkout -- $Repo $work
        if ($LASTEXITCODE -ne 0) { throw "could not clone the demo repository at $Repo" }
        git -C $work checkout --quiet -B main $Sha
        if ($LASTEXITCODE -ne 0) { throw "the demo repository has no commit $Sha" }
        $history = (git -C $work log --all --format='%an %ae%n%B') | Out-String
        foreach ($pattern in $PrivatePatterns + $HomePattern) {
            if ([regex]::IsMatch($history, $pattern)) {
                throw "the demo history matches a private pattern: $pattern"
            }
        }
        Assert-Clean $work
        git -C $work bundle create $Out main
        if ($LASTEXITCODE -ne 0) { throw 'git bundle create failed' }
    }
    finally {
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function New-Stage {
    param(
        [string]$Kind,
        [string]$Os,
        [string]$Version,
        [string]$BinDir,
        [string]$WebDir,
        [string]$ReplayDir,
        [string]$Bundle,
        [string]$OutDir,
        [string]$RepoRoot
    )
    $stage = Join-Path $OutDir (Get-StageName $Kind $Version $Os)
    if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $stage | Out-Null

    $exe = Get-Exe $Os
    foreach ($bin in 'acan', 'acan-worker') {
        $source = Join-Path $BinDir "$bin$exe"
        if (-not (Test-Path -LiteralPath $source)) { throw "missing binary: $source" }
        Copy-Item -LiteralPath $source -Destination (Join-Path $stage "$bin$exe")
    }
    Copy-Item -LiteralPath $WebDir -Destination (Join-Path $stage 'web') -Recurse
    # The in-app updater reads this to choose which setup to download: app or demo.
    Set-Content -LiteralPath (Join-Path $stage 'FLAVOR') -Value $Kind -Encoding ascii
    Copy-Item -LiteralPath (Join-Path $RepoRoot 'LICENSE') -Destination $stage
    Copy-Item -LiteralPath (Join-Path $RepoRoot (Get-LauncherSource $Kind $Os)) -Destination $stage
    if ($Kind -eq 'demo') {
        Copy-Item -LiteralPath $ReplayDir -Destination (Join-Path $stage 'web/replay') -Recurse
        Copy-Item -LiteralPath $Bundle -Destination (Join-Path $stage 'acan-demo.bundle')
        Copy-Item -LiteralPath (Join-Path $RepoRoot 'installer/demo/README.txt') -Destination (Join-Path $stage 'README.txt')
    }
    else {
        Copy-Item -LiteralPath (Join-Path $RepoRoot 'README.md') -Destination $stage
    }
    Assert-Members $stage $Kind $Os
    Assert-Clean $stage
    $stage
}

function New-LinuxArchive([string]$Stage, [string]$OutDir) {
    $name = Split-Path -Leaf $Stage
    if (-not $IsWindows) {
        foreach ($file in 'acan', 'acan-worker', 'install.sh', 'start-acan-demo.sh') {
            $path = Join-Path $Stage $file
            if (Test-Path -LiteralPath $path) { chmod 755 $path }
        }
    }
    $archive = Join-Path $OutDir "$name.tar.gz"
    if (Test-Path -LiteralPath $archive) { Remove-Item -LiteralPath $archive }
    tar -C $OutDir -czf $archive $name
    if ($LASTEXITCODE -ne 0) { throw "tar failed for $archive" }
    $archive
}

function Assert-Refused([scriptblock]$Attempt, [string]$Expected, [string]$Case) {
    $refused = $false
    try {
        & $Attempt | Out-Null
    }
    catch {
        $refused = $_.Exception.Message -like "*$Expected*"
    }
    if (-not $refused) { throw "self-test: $Case was not refused" }
}

function Invoke-SelfTest {
    $work = Join-Path ([IO.Path]::GetTempPath()) ('acan-package-test-' + [guid]::NewGuid())
    try {
        $bin = Join-Path $work 'bin'
        $empty = Join-Path $work 'bin-empty'
        $web = Join-Path $work 'web'
        $replay = Join-Path $work 'replay'
        $out = Join-Path $work 'out'
        $bundle = Join-Path $work 'acan-demo.bundle'
        New-Item -ItemType Directory -Force -Path $bin, $empty, (Join-Path $web 'pkg'), $replay, $out | Out-Null
        foreach ($name in 'acan', 'acan-worker', 'acan.exe', 'acan-worker.exe') {
            Set-Content -LiteralPath (Join-Path $bin $name) -Value $name
        }
        Set-Content -LiteralPath (Join-Path $web 'index.html') -Value '<!doctype html>'
        Set-Content -LiteralPath (Join-Path $web 'pkg/app.js') -Value 'export {};'
        foreach ($name in 'manifest.json', 'events.json', 'plant.json') {
            Set-Content -LiteralPath (Join-Path $replay $name) -Value '{}'
        }
        Set-Content -LiteralPath $bundle -Value 'a fake bundle for the self-test'
        $version = '0.0.0-selftest'

        foreach ($kind in 'app', 'demo') {
            foreach ($os in 'linux', 'windows') {
                $stage = New-Stage -Kind $kind -Os $os -Version $version -BinDir $bin -WebDir $web -ReplayDir $replay -Bundle $bundle -OutDir $out -RepoRoot $RepoRoot
                if ($os -eq 'linux') {
                    $archive = New-LinuxArchive -Stage $stage -OutDir $out
                    $listing = @(tar -tzf $archive)
                    $members = @(Get-Members $kind $os)
                    foreach ($member in $members) {
                        if ($listing -notcontains "$(Split-Path -Leaf $stage)/$member") {
                            throw "self-test: $(Split-Path -Leaf $archive) lacks $member"
                        }
                    }
                    Write-Output "ok: $(Split-Path -Leaf $archive) holds $($members.Count) members"
                }
                else {
                    Write-Output "ok: $(Split-Path -Leaf $stage) is staged for Inno Setup"
                }
            }
        }

        Assert-Refused { New-Stage -Kind app -Os linux -Version $version -BinDir $empty -WebDir $web -ReplayDir $replay -Bundle $bundle -OutDir $out -RepoRoot $RepoRoot } 'missing binary' 'a stage with no binaries'
        # The private path is joined from two parts, so this file holds no literal private path.
        Set-Content -LiteralPath (Join-Path $web 'notes.html') -Value ('see D:' + '\dev' + '\example')
        Assert-Refused { New-Stage -Kind app -Os linux -Version $version -BinDir $bin -WebDir $web -ReplayDir $replay -Bundle $bundle -OutDir $out -RepoRoot $RepoRoot } 'private pattern' 'a web file with a private path'
        Remove-Item -LiteralPath (Join-Path $web 'notes.html')
        Set-Content -LiteralPath (Join-Path $web 'tokens.json') -Value '[]'
        Assert-Refused { New-Stage -Kind app -Os linux -Version $version -BinDir $bin -WebDir $web -ReplayDir $replay -Bundle $bundle -OutDir $out -RepoRoot $RepoRoot } 'must not ship' 'a tokens file'
        Remove-Item -LiteralPath (Join-Path $web 'tokens.json')
        Write-Output 'ok: refusals hold (no binaries, a private path, a tokens file)'
    }
    finally {
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    }
}

if ($SelfTest) {
    Invoke-SelfTest
    return
}
foreach ($required in 'Kind', 'Os', 'Version', 'BinDir', 'WebDir', 'OutDir') {
    if (-not $PSBoundParameters.ContainsKey($required)) { throw "-$required is required (or use -SelfTest)" }
}
$bundle = $DemoBundle
if ($Kind -eq 'demo') {
    if (-not $ReplayDir) { throw '-ReplayDir is required for the demo' }
    if (-not $bundle) {
        if (-not ($DemoRepo -and $DemoSha)) { throw 'the demo needs -DemoBundle, or -DemoRepo with -DemoSha' }
        $bundle = Join-Path ([IO.Path]::GetTempPath()) "acan-demo-$Version.bundle"
        New-DemoBundle -Repo $DemoRepo -Sha $DemoSha -Out $bundle
    }
}
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$stage = New-Stage -Kind $Kind -Os $Os -Version $Version -BinDir $BinDir -WebDir $WebDir -ReplayDir $ReplayDir -Bundle $bundle -OutDir $OutDir -RepoRoot $RepoRoot
if ($Os -eq 'linux') { New-LinuxArchive -Stage $stage -OutDir $OutDir } else { $stage }
