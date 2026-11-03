#requires -Version 7
# Checks the StoppedLane and LaneBudget models under their configs and prints a table of cfg, rule and result.
# 1. The statechart skill runs on each cfg and writes tla/out/<cfg>/ (chart, report, TLC output).
#    It exits 1 when a rule breaks, which is expected, so its exit code is not read.
# 2. Plain TLC (tla/.jar/tla2tools.jar) checks each rule alone on each cfg, to confirm the verdicts.
# Exit 0 only when every expectation below holds and the two checkers agree.
$ErrorActionPreference = 'Stop'

$tla = $PSScriptRoot
$out = Join-Path $tla 'out'
$jarDir = Join-Path $tla '.jar'
$localBin = Join-Path $HOME '.local/bin'
$statechart = if ($env:STATECHART) { $env:STATECHART } else { Join-Path $HOME 'work/tools/pstack/skills/statechart/scripts/statechart' }
$mise = if ($env:MISE) { $env:MISE } else { Join-Path $localBin 'mise' }
$env:PATH = $localBin + [IO.Path]::PathSeparator + $env:PATH

# TLC and Java come from mise, which the statechart command installs on its first run.
$tlcHome = (& $mise where 'http:tla2tools@1.7.4').Trim()
$javaHome = (& $mise where 'java@temurin-25').Trim()
if (-not $tlcHome -or -not $javaHome) {
    throw 'mise cannot find TLC or Java. Run the statechart command once so that mise installs them.'
}
New-Item -ItemType Directory -Force -Path $jarDir | Out-Null
Copy-Item -Force (Join-Path $tlcHome 'tla2tools.jar') (Join-Path $jarDir 'tla2tools.jar')
$jar = Join-Path $jarDir 'tla2tools.jar'
$java = Join-Path $javaHome 'bin/java'

# Each model names its module, its view, its rules, and the rules that are liveness properties.
# A cfg lists the rules that must break (Breaks). Every other rule must hold, unless Record is set:
# then the other rules are recorded only, and the two checkers must still agree on each of them.
# Steps gives the length that a break's shortest path must have.
$models = @(
    @{
        Name = 'StoppedLane'
        Spec = Join-Path $tla 'StoppedLane.tla'
        View = Join-Path $tla 'views/stopped_lane.view.ts'
        Rules = @('TypeOK', 'NoBuildWhenStopped', 'LeaseMeansClaimed', 'LeasesEnd', 'StopIsFinal')
        Liveness = @('LeasesEnd', 'StopIsFinal')
        Expectations = @(
            @{ Cfg = 'StoppedLane_today'; Breaks = @('NoBuildWhenStopped', 'LeasesEnd'); Steps = @{ NoBuildWhenStopped = 5 }; Mark = 'bugPath' },
            @{ Cfg = 'StoppedLane_fixed'; Breaks = @(); Steps = @{} },
            @{ Cfg = 'StoppedLane_keep_lease'; Breaks = @('LeaseMeansClaimed', 'LeasesEnd'); Steps = @{} },
            @{ Cfg = 'StoppedLane_unstop'; Breaks = @('StopIsFinal'); Steps = @{} }
        )
    },
    @{
        Name = 'LaneBudget'
        Spec = Join-Path $tla 'LaneBudget.tla'
        View = Join-Path $tla 'views/lane_budget.view.ts'
        Rules = @('RoundsBounded', 'StopAfterRound', 'ResidueClears', 'StopIsFinal')
        Liveness = @('ResidueClears', 'StopIsFinal')
        Expectations = @(
            @{ Cfg = 'LaneBudget_today_cap1'; Breaks = @('ResidueClears'); Steps = @{}; Mark = 'bugPath' },
            @{ Cfg = 'LaneBudget_today_cap3'; Breaks = @('ResidueClears'); Steps = @{} },
            @{ Cfg = 'LaneBudget_fixed_cap3'; Breaks = @(); Steps = @{} },
            @{ Cfg = 'LaneBudget_off_by_one'; Breaks = @('RoundsBounded'); Steps = @{} },
            @{ Cfg = 'LaneBudget_unstop'; Breaks = @('StopIsFinal'); Steps = @{} },
            @{ Cfg = 'LaneBudget_cap0'; Breaks = @('StopAfterRound'); Steps = @{ StopAfterRound = 1 }; Record = $true }
        )
    }
)

function Get-CfgCore([string]$path) {
    # Keep the lines that set the spec and constants. The rule lines are added per check.
    Get-Content $path | Where-Object { $_ -match '^(SPECIFICATION|CONSTANT|CHECK_DEADLOCK)\b' }
}

function Invoke-Chart($model, [string]$cfgName) {
    $dir = Join-Path $out $cfgName
    if (Test-Path $dir) { Remove-Item -Recurse -Force $dir }
    & $statechart $model.Spec --config (Join-Path $tla "$cfgName.cfg") --view $model.View --out $dir *> (Join-Path $out "$cfgName.statechart.log")
    if (-not (Test-Path (Join-Path $dir 'report.md'))) {
        throw "statechart wrote no report for $cfgName. See $(Join-Path $out "$cfgName.statechart.log")"
    }
    return $dir
}

function Read-Report([string]$dir) {
    # Returns verdicts from report.md: breaks (rule -> step count) and holds (list).
    $breaks = @{}
    $holds = @()
    $section = $null
    $current = $null
    foreach ($line in Get-Content (Join-Path $dir 'report.md')) {
        if ($line -match '^## (\w+) breaks$') { $section = 'breaks'; $current = $Matches[1]; $breaks[$current] = $null; continue }
        if ($line -match '^## Rules that hold') { $section = 'holds'; continue }
        if ($line -match '^## ') { $section = 'other'; $current = $null; continue }
        if ($section -eq 'breaks' -and $null -eq $breaks[$current] -and $line -match 'has (\d+) steps?') {
            $breaks[$current] = [int]$Matches[1]
        }
        if ($section -eq 'holds' -and $line.Trim()) {
            $holds += ($line.Trim() -split ',\s*')
        }
    }
    return @{ Breaks = $breaks; Holds = $holds }
}

function Invoke-PlainRule($model, [string]$cfgName, [string]$rule) {
    # Plain TLC on one rule at a time. An invariant or a property is checked as listed in its cfg.
    $dir = Join-Path $out "$cfgName/plain"
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $kind = if ($rule -in $model.Liveness) { 'PROPERTY' } else { 'INVARIANT' }
    $cfgFile = Join-Path $dir "$rule.cfg"
    $lines = @(Get-CfgCore (Join-Path $tla "$cfgName.cfg")) + "$kind $rule"
    Set-Content -Path $cfgFile -Value $lines
    $log = Join-Path $dir "$rule.out"
    $meta = Join-Path $dir "states-$rule"
    & $java -XX:+UseParallelGC -cp $jar tlc2.TLC -workers 1 -metadir $meta -config $cfgFile $model.Spec *> $log
    $text = Get-Content $log -Raw
    if ($text -match 'No error has been found') { return 'holds' }
    if ($text -match 'Error:') { return 'breaks' }
    return 'unknown'
}

function Get-FullStates($model, [string]$cfgName) {
    # Plain TLC with no rules explores the whole reachable graph, so the count is complete.
    $dir = Join-Path $out "$cfgName/plain"
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $cfgFile = Join-Path $dir 'no-rules.cfg'
    Set-Content -Path $cfgFile -Value @(Get-CfgCore (Join-Path $tla "$cfgName.cfg"))
    $log = Join-Path $dir 'no-rules.out'
    & $java -XX:+UseParallelGC -cp $jar tlc2.TLC -workers 1 -metadir (Join-Path $dir 'states-none') -config $cfgFile $model.Spec *> $log
    $m = [regex]::Matches((Get-Content $log -Raw), '([0-9]+) distinct states found')
    if ($m.Count -eq 0) { return '' }
    return [int]$m[$m.Count - 1].Groups[1].Value
}

$allOk = $true

foreach ($model in $models) {
    $rows = @()
    $stateRows = @()

    foreach ($exp in $model.Expectations) {
        $cfgName = $exp.Cfg
        $dir = Invoke-Chart $model $cfgName
        $report = Read-Report $dir

        $states = Get-FullStates $model $cfgName
        $under500 = ($states -is [int]) -and ($states -lt 500)
        $stateRows += [pscustomobject]@{ Cfg = $cfgName; States = $states; Under500 = $under500 }
        if (-not $under500) { $allOk = $false }

        foreach ($rule in $model.Rules) {
            if ($report.Breaks.ContainsKey($rule)) { $skill = 'breaks' }
            elseif ($report.Holds -contains $rule) { $skill = 'holds' }
            else { $skill = 'not checked' }

            $plain = Invoke-PlainRule $model $cfgName $rule

            $expected = if ($rule -in $exp.Breaks) { 'breaks' } elseif ($exp.Record) { 'recorded' } else { 'holds' }

            $path = ''
            if ($report.Breaks.ContainsKey($rule) -and $null -ne $report.Breaks[$rule]) { $path = [int]$report.Breaks[$rule] }
            $pathOk = $true
            if ($exp.Steps.ContainsKey($rule)) {
                $pathOk = ($path -eq $exp.Steps[$rule])
            }

            if ($expected -eq 'recorded') {
                $ok = ($skill -eq $plain) -and ($skill -in @('breaks', 'holds')) -and $pathOk
            } else {
                $ok = ($skill -eq $expected) -and ($plain -eq $expected) -and $pathOk
            }
            if (-not $ok) { $allOk = $false }
            $rows += [pscustomobject]@{
                Cfg = $cfgName
                Rule = $rule
                Skill = $skill
                Plain = $plain
                Expected = $expected
                Steps = $path
                Result = $(if ($ok) { 'ok' } else { 'MISMATCH' })
            }
        }

        if ($exp.Mark) {
            $chart = Join-Path $dir 'chart.mmd'
            $chartOk = (Test-Path $chart) -and ((Get-Content $chart -Raw) -match $exp.Mark)
            if (-not $chartOk) { $allOk = $false; Write-Host "MISMATCH: $cfgName chart.mmd is missing or marks no $($exp.Mark)" }
        }
    }

    Write-Host ''
    Write-Host "Rule results, $($model.Name) (skill = statechart command, plain = plain TLC on the rule alone)"
    $rows | Format-Table -AutoSize | Out-String -Width 200 | Write-Host
    Write-Host "State counts, $($model.Name) (all reachable states, plain TLC with no rules, limit 500)"
    $stateRows | Format-Table -AutoSize | Out-String | Write-Host
}

Write-Host ("Result: " + $(if ($allOk) { 'all expectations hold' } else { 'at least one expectation differs' }))
if ($allOk) { exit 0 } else { exit 1 }
