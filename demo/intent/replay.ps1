<#
.SYNOPSIS
X01 replay: the 12 pre-registered orders in demo/intent/orders.json and the four controls in
demo/intent/mutants.json, run once each through the real Acan server with live Meta models.

.DESCRIPTION
-DryRun reads orders.json and mutants.json only, checks their shape, and prints the plan. It starts
nothing, clones nothing, calls no model, and draws no seeded orders. The seed is generated at the
start of a live run, and a dry run never shows which orders it draws.

A live run refuses to start offline, writes the draw (seed and drawn orders) and hashes it before the
first candidate exists, and writes one results.jsonl line per order and per control into a new run
folder under -RunsRoot. The README section "Replay (X01)" describes the inputs and the counts.
#>
[CmdletBinding()]
param(
    [string]$Label = 'x01',
    # 7411 and 7412 belong to other builders and to the UI chat.
    [ValidateScript({
            if ($_ -in 7411, 7412) { throw "port $_ belongs to another builder or the UI chat" }
            if ($_ -lt 1024 -or $_ -gt 65535) { throw "port $_ is outside 1024-65535" }
            $true
        })]
    [int]$Port = 7413,
    [string]$RunsRoot = 'D:\dev\acan-demo-runs\intent',
    [string]$DemoRepo = 'D:\dev\acan-demo',
    [string]$Pin = 'f91962156841332d5753d869554839f7c4cdea79',
    [string]$AcanRepo = (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)),
    [string]$AcanExe = 'C:\cargo-targets\acan-x01\debug\acan.exe',
    [string]$WorkerExe = 'C:\cargo-targets\acan-x01\debug\acan-worker.exe',
    [string]$LlmSource = 'D:\dev\acan\crates\acan-llm\llm.toml',
    [string]$EnvSource = 'D:\dev\acan\crates\acan-llm\.env',
    [string]$TakeScript = (Join-Path $PSScriptRoot 'take.ps1'),
    [int]$MaxEpisodes = 40,
    [int]$HealthWaitSec = 90,
    [int]$StepWaitSec = 900,
    [int]$WorkerWaitSec = 2700,
    [int]$ControlWaitSec = 1500,
    [int]$RulingWaitSec = 1200,
    # A rehearsal runs only these orders. The draw still covers all 12, and the run is reported partial.
    [string[]]$Only = @(),
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
$ResultsSchema = 'acan-replay-result/1'
$SummarySchema = 'acan-replay-summary/1'
$DrawSchema = 'acan-replay-draw/1'
$DrawCount = 4
$intentDir = Join-Path $AcanRepo 'demo\intent'

function Read-JsonFile {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) { throw "missing input: $Path" }
    Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
}

# ---- inputs: the two pre-registered files, read and checked, never written ----------------------
$orderSet = Read-JsonFile -Path (Join-Path $intentDir 'orders.json')
$mutantSet = Read-JsonFile -Path (Join-Path $intentDir 'mutants.json')
if ($orderSet.schema -ne 'acan-demo-orders/1') { throw "orders.json must have schema acan-demo-orders/1, found '$($orderSet.schema)'" }
if ($mutantSet.schema -ne 'acan-demo-mutants/1') { throw "mutants.json must have schema acan-demo-mutants/1, found '$($mutantSet.schema)'" }
$orderList = @($orderSet.orders)
$orderIds = @($orderList | ForEach-Object { [string]$_.id })
if ($orderList.Count -ne 12) { throw "orders.json holds $($orderList.Count) orders; the replay is written for 12" }
if (@($orderIds | Select-Object -Unique).Count -ne 12) { throw 'orders.json repeats an order id' }
foreach ($o in $orderList) {
    if ($o.supersedes -and $orderIds -notcontains [string]$o.supersedes) {
        throw "$($o.id) supersedes $($o.supersedes), which is not in orders.json"
    }
}
$controlList = @($mutantSet.controls)
if ($controlList.Count -ne 4) { throw "mutants.json holds $($controlList.Count) controls; the replay runs 4" }
if ([int]$mutantSet.seeded_mutants.drawn -ne $DrawCount -or [int]$mutantSet.seeded_mutants.orders -ne 12) {
    throw "mutants.json draws $($mutantSet.seeded_mutants.drawn) of $($mutantSet.seeded_mutants.orders); the replay is written for $DrawCount of 12"
}
foreach ($id in $Only) {
    if ($orderIds -notcontains $id) { throw "-Only names $id, which is not in orders.json" }
}
$selected = @($orderList | Where-Object { $Only.Count -eq 0 -or $Only -contains [string]$_.id })

function Show-Plan {
    Write-Output 'DRY RUN. Reads orders.json and mutants.json only. Starts nothing, calls no model, draws nothing.'
    Write-Output "port $Port (7411 and 7412 refused); runs root $RunsRoot; pin $Pin"
    if ($Only.Count -gt 0) { Write-Output "rehearsal: only $($Only -join ', ') run; the run is reported partial" }
    Write-Output ''
    Write-Output "Orders ($($selected.Count) of 12, orders.json order):"
    foreach ($o in $selected) {
        $sup = if ($o.supersedes) { "supersedes $($o.supersedes)" } else { '-' }
        Write-Output ("  {0,-5} {1,-10} {2,-16} {3}" -f $o.id, $o.machine, $sup, $o.sentence)
    }
    Write-Output ''
    Write-Output "Controls (mutants.json, outcomes set $($mutantSet.outcomes_set)):"
    foreach ($c in $controlList) {
        Write-Output "  $($c.id) [$($c.kind)] applies_to $($c.applies_to)"
        Write-Output "      expected: $($c.expected_outcome)"
    }
    Write-Output ''
    Write-Output 'Live plan, in order:'
    Write-Output '  0. Refuse offline: foreman must be meta / muse-spark*, worker baseten / muse-spark-open;'
    Write-Output '     both hosts must answer on TCP 443; each key variable must be set in .env (names only).'
    Write-Output '  1. Inputs: demo/intent/<id>-edits.json (acan-demo-intent-edits/2) for each order run;'
    Write-Output '     demo/intent/rulings/<id>-rulings.json (acan-demo-intent-rulings/1) for each order run;'
    Write-Output '     demo/intent/seeded/<id>.json (acan-demo-control/1) for all 12, hashed before the draw.'
    Write-Output '  2. Draw: a seed from the OS random source, then 4 of 12 by SHA-256 of "seed|id". The seed and'
    Write-Output '     the drawn list go to draw.json, its SHA-256 goes into summary.json, all before any clone.'
    Write-Output '  3. Clone the demo at the pin, write tokens (owner, worker-r, control-seeded), start acan serve'
    Write-Output "     on port $Port, register worker-r and control-seeded."
    Write-Output '  4. Each order once: post, split, draft, human edits and confirm, worker-r (model), receipts,'
    Write-Output '     rulings, and the seeded control when the draw picked the order.'
    Write-Output '  5. Controls after o-1: m-overfit and m-harmless-difference. m-broad-scope and m-missing-fixture'
    Write-Output '     are not_run until a driver and bundle exist for them.'
    Write-Output '  6. summary.json (complete, or incomplete with reasons), results.jsonl, secret scan.'
    Write-Output ''
    Write-Output 'Not shown: which orders the seeded draw picks. It is decided at live start.'
}

if ($DryRun) {
    Show-Plan
    exit 0
}

throw 'the live path is not written yet; run with -DryRun'
