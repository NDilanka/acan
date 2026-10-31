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

# ================================================================================================
# Live path. The HTTP, wait, worker, receipt and git helpers come from take.ps1 through its
# -LibraryOnly guard. Four take steps are called by name (see Test-TakeFunctions): they are the
# take's inline blocks lifted into functions, and the replay uses them unchanged in behaviour.
# ================================================================================================

$script:KnownErrorClasses = @('setup_failed', 'draft_not_ready', 'needs_human_scope', 'needs_human_ruling', 'worker_timeout', 'worker_no_build')
$script:IncompleteClasses = @('incomplete', 'setup_failed', 'draft_not_ready', 'needs_human_scope', 'needs_human_ruling', 'worker_timeout', 'worker_no_build')
$script:Outcomes = @{}
$script:RulingOwners = @{}
$script:OidByOrder = @{}
$script:TasksByOrder = @{}
$script:SeedBundleHashes = @{}
$script:ResultRecords = [System.Collections.Generic.List[object]]::new()
$script:Tokens = @{}
$script:Routes = $null
$script:Proc = $null
$script:Base = ''
$script:Repo = ''
$script:LlmPath = ''
$script:EnvPath = ''
$script:OwnerToken = ''

function Write-JsonFile {
    param([string]$Path, $Object)
    ConvertTo-Json -Depth 40 $Object | Set-Content -LiteralPath $Path -Encoding utf8
}

function Get-FileSha256 {
    param([string]$Path)
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Get-DrawKey {
    param([string]$Seed, [string]$Id)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $hash = $sha.ComputeHash([Text.Encoding]::UTF8.GetBytes("$Seed|$Id"))
        ([BitConverter]::ToString($hash) -replace '-', '').ToLowerInvariant()
    } finally {
        $sha.Dispose()
    }
}

function Get-DrawnOrders {
    # The Count ids with the smallest SHA-256 of "<seed>|<id>", returned in orders.json order.
    # Anyone with the seed and orders.json can recompute the draw.
    param([string]$Seed, [string[]]$Ids, [int]$Count)
    $keyed = @($Ids | ForEach-Object { [pscustomobject]@{ id = $_; key = (Get-DrawKey -Seed $Seed -Id $_) } })
    $first = @($keyed | Sort-Object -Property key | Select-Object -First $Count | ForEach-Object { $_.id })
    @($Ids | Where-Object { $first -contains $_ })
}

function Get-EventSeqMax {
    $seqs = @(Invoke-Api -Path '/api/events' | ForEach-Object { [int64]$_.seq })
    if ($seqs.Count -eq 0) { return [int64]0 }
    [int64](($seqs | Measure-Object -Maximum).Maximum)
}

function Get-ModelCallCount {
    # model_used events between two sequence numbers. The replay runs one order at a time, so the
    # window holds exactly the model calls of that order.
    param([int64]$AfterSeq, [int64]$UpToSeq)
    $events = @(Invoke-Api -Path '/api/events')
    @($events | Where-Object { [int64]$_.seq -gt $AfterSeq -and [int64]$_.seq -le $UpToSeq -and "$($_.event.type)" -eq 'model_used' }).Count
}

function Get-SupersedeEvents {
    $events = @(Invoke-Api -Path '/api/events')
    @($events | Where-Object { "$($_.event.type)" -match 'supersed' } | ForEach-Object { [ordered]@{ seq = $_.seq; type = [string]$_.event.type } })
}

function Get-LandedCommit {
    param([string]$Task)
    $events = @(Invoke-Api -Path '/api/events')
    $landed = @($events | Where-Object { $_.event.type -eq 'landed' -and "$($_.event.task)" -eq $Task })
    if ($landed.Count -eq 0) { return $null }
    [string]$landed[-1].event.commit
}

function Read-ProviderEndpoint {
    param([string]$Text, [string]$Name)
    foreach ($block in ($Text -split '\[\[providers\]\]')) {
        if ($block -notmatch ('name\s*=\s*"' + [regex]::Escape($Name) + '"')) { continue }
        $base = $null
        $envName = $null
        if ($block -match 'base_url\s*=\s*"([^"]+)"') { $base = $Matches[1] }
        if ($block -match 'api_key_env\s*=\s*"([^"]+)"') { $envName = $Matches[1] }
        return [ordered]@{ base_url = $base; api_key_env = $envName }
    }
    $null
}

function Test-LiveRoutes {
    # Refuses to run offline, and refuses any model other than the two the brief names. It checks
    # TCP reachability and that each key variable is set in .env, by name only. No model is called.
    $text = Get-Content -LiteralPath $LlmSource -Raw
    $routes = Read-Routing -Text $text
    $f = $routes['foreman']
    $w = $routes['worker']
    if ($null -eq $f -or $f.provider -ne 'meta' -or "$($f.model)" -notlike 'muse-spark*') {
        throw "the foreman must be Muse Spark on meta; llm.toml says $($f.provider) / $($f.model)"
    }
    if ($null -eq $w -or $w.provider -ne 'baseten' -or "$($w.model)" -notlike '*muse-spark-open*') {
        throw "the worker must be Muse Spark Open on baseten; llm.toml says $($w.provider) / $($w.model)"
    }
    $keys = Read-KeyFile -Path $EnvSource
    $report = [ordered]@{}
    foreach ($pair in @(@('foreman', $f), @('worker', $w))) {
        $role = [string]$pair[0]
        $route = $pair[1]
        $endpoint = Read-ProviderEndpoint -Text $text -Name ([string]$route.provider)
        if ($null -eq $endpoint -or -not $endpoint.base_url -or -not $endpoint.api_key_env) {
            throw "llm.toml gives no base_url and api_key_env for provider $($route.provider)"
        }
        $hostName = ([uri][string]$endpoint.base_url).Host
        $reachable = $false
        $tcp = [System.Net.Sockets.TcpClient]::new()
        try {
            $reachable = [bool]$tcp.ConnectAsync($hostName, 443).Wait(5000)
        } catch {
            $reachable = $false
        } finally {
            $tcp.Dispose()
        }
        if (-not $reachable) {
            throw "offline: $hostName does not answer on TCP 443 for the $role; the replay refuses to run offline"
        }
        $envName = [string]$endpoint.api_key_env
        $keySet = $keys.Contains($envName) -and "$($keys[$envName])".Length -ge 8
        if (-not $keySet) { throw "$envName is not set in the .env file; the $role cannot call its model" }
        $report[$role] = [ordered]@{ provider = [string]$route.provider; model = [string]$route.model; host = $hostName; tcp_443 = 'reachable'; key_variable = $envName; key_present = $true }
    }
    $report
}

function Test-TakeFunctions {
    # The helpers this file calls. Each one is either a take.ps1 helper that exists today, or one of
    # the four take steps that must be lifted into a function first (see the report of X01).
    $needed = @(
        'Send-Command', 'Invoke-Api', 'Invoke-Api404Tolerant', 'Wait-Until', 'Get-OrderTasks', 'Start-Worker',
        'Get-AgentEvents', 'Read-ControlResult', 'Read-KeyFile', 'Read-Routing', 'Assert-OutsideGit', 'Get-DemoRefs',
        'New-TakeClone', 'Start-TakeServer', 'Invoke-ScopeEdits', 'Invoke-ScopeConfirm', 'Invoke-ControlWithRuling'
    )
    $missing = @($needed | Where-Object { -not (Get-Command -Name $_ -CommandType Function -ErrorAction SilentlyContinue) })
    if ($missing.Count -gt 0) {
        throw "take.ps1 does not define: $($missing -join ', '); the take steps are not lifted into functions yet"
    }
}

function Add-ResultLine {
    param($Record)
    $script:ResultRecords.Add($Record)
    Add-Content -LiteralPath $script:ResultsPath -Value (ConvertTo-Json -Depth 30 -Compress $Record) -Encoding utf8
}

function Get-ReceiptItems {
    param($Body)
    @($Body.receipt.items | Where-Object { $null -ne $_ })
}

function Invoke-SeededCandidate {
    # A drawn order gets one seeded out-of-scope bundle, run as a control on the order's first task.
    # The control applies the bundle to the pinned base, so the candidate is the bundle alone. The
    # gate sees no ruling from the replay: a seeded item that stays unruled is judged as blocked.
    param([string]$OrderId, [string]$Task, [string]$RunId)
    $bundle = Join-Path $intentDir "seeded\$OrderId.json"
    $out = Join-Path $script:CtrlDir "seeded-$OrderId.json"
    $line = [ordered]@{
        schema = $ResultsSchema; run_id = $RunId; kind = 'seeded'; id = "seeded-$OrderId"; order = $OrderId
        bundle_sha256 = $script:SeedBundleHashes[$OrderId]; actor = 'control-seeded (control identity, not a model)'
        provider = 'none'; scope = 'not_applicable'; attempt = 1; outcome = 'not_run'
        receipt_hash = $null; merged_sha = $null; gate_state = $null; model_calls = 0; wall_seconds = $null
        catch_natural_ruling = $false; catch_seeded = $false; catch_base_only = $false; errors = @()
        started = (Get-Date).ToString('o'); finished = $null
    }
    $watch = [Diagnostics.Stopwatch]::StartNew()
    try {
        if (-not $Task) { throw 'not_run: the order split into no task, so there is no task to inject into' }
        & (Join-Path $intentDir 'control.ps1') -Server $script:Base -Token $script:Tokens['control-seeded'] -Repo $script:Repo -Task $Task -Bundle $bundle -Out $out -StageWaitSec 300 -ReceiptWaitSec $ControlWaitSec
        $r = Read-ControlResult -Path $out
        if ($null -eq $r) { throw 'error: the seeded control wrote no result file' }
        $gate = [string]$r.gate_state
        $blocking = @($r.items | Where-Object { $_.class -in @('out_of_scope', 'violates_ruling', 'obligation_failed') })
        $line['receipt_hash'] = [string]$r.receipt_hash
        $line['gate_state'] = $gate
        $line['catch_seeded'] = ($gate -ne 'landed' -and $blocking.Count -ge 1)
        $line['catch_base_only'] = ($gate -ne 'landed' -and @($r.items | Where-Object { $_.class -eq 'obligation_failed' }).Count -ge 1 -and @($r.items | Where-Object { $_.class -eq 'violates_ruling' }).Count -eq 0)
        $line['outcome'] = if ($line['catch_seeded']) { 'caught' } elseif ($gate -eq 'landed') { 'landed_not_caught' } else { 'not_caught' }
    } catch {
        $msg = $_.Exception.Message
        $line['errors'] = @($line['errors']) + $msg
        $line['outcome'] = if ($msg -like 'not_run:*') { 'not_run' } else { 'error' }
    } finally {
        $watch.Stop()
        $line['wall_seconds'] = [math]::Round($watch.Elapsed.TotalSeconds, 1)
        $line['finished'] = (Get-Date).ToString('o')
        Add-ResultLine -Record $line
    }
    $line
}

function Invoke-ReplayOrder {
    param($Ord, [bool]$Drawn)
    $id = [string]$Ord.id
    $dir = Join-Path $run $id
    New-Item -ItemType Directory -Path $dir -Force | Out-Null
    $line = [ordered]@{
        schema = $ResultsSchema; run_id = $script:RunId; kind = 'order'; id = $id; machine = [string]$Ord.machine
        supersedes = $Ord.supersedes; drawn_seeded = $Drawn; attempt = 1
        actor = 'owner (member HTTP); foreman (model); worker-r (model)'
        provider = "foreman $($script:Routes.foreman.provider) / $($script:Routes.foreman.model); worker $($script:Routes.worker.provider) / $($script:Routes.worker.model)"
        scope = 'not_reached'; scope_hash = $null; outcome = 'incomplete'; receipt_hash = $null; merged_sha = $null
        gate_state = $null; rulings = 0; tasks = @(); model_calls = $null; wall_seconds = $null; errors = @()
        catch_natural_ruling = $false; catch_seeded = $false; catch_base_only = $false
        started = (Get-Date).ToString('o'); finished = $null
    }
    $watch = [Diagnostics.Stopwatch]::StartNew()
    $seqStart = Get-EventSeqMax
    $oid = ''
    $tasks = @()
    $receipts = @()
    $errorClass = $null
    try {
        # post the order, once
        $posted = Send-Command @{ cmd = 'post_order'; machine = $Ord.machine; sentence = $Ord.sentence; witnesses = @() }
        $postedEvent = $posted | Where-Object { $_.event.type -eq 'order_posted' } | Select-Object -First 1
        if (-not $postedEvent) { throw 'setup_failed: post_order returned no order_posted event' }
        $raw = "$($postedEvent.event.order)"
        $oid = if ($raw -like 'o-*') { $raw } else { "o-$raw" }
        $script:OidByOrder[$id] = $oid

        # split, then the foreman's draft
        $null = Wait-Until -What "the split of $id" -Probe {
            Invoke-Api -Path '/api/events' | Where-Object { $_.event.type -eq 'order_split' -and "$($_.event.order)" -eq $raw } | Select-Object -First 1
        } -Terminal { $null }
        $tasks = @(Get-OrderTasks -Events (Invoke-Api -Path '/api/events') -Raw $raw)
        if ($tasks.Count -eq 0) { throw 'setup_failed: the split of the order made no task' }
        $script:TasksByOrder[$id] = $tasks
        $view = Wait-Until -What "the foreman draft for $id" -Probe { Invoke-Api404Tolerant -Path "/api/orders/$oid/scope-draft" } -Terminal { $null }
        if ($view.state -ne 'drafted') { throw "draft_not_ready: the draft state is $($view.state)" }
        Write-JsonFile -Path (Join-Path $dir 'scope-draft.json') -Object $view.draft

        # the human scope: this order's edits file, one revise_scope, then confirm (take steps c and d)
        $editsPath = Join-Path $intentDir "$id-edits.json"
        $scope = Invoke-ScopeEdits -Order $id -Raw $raw -Oid $oid -View $view -EditsFile $editsPath -OrderDir $dir
        $confirmed = Invoke-ScopeConfirm -Order $id -Raw $raw -Oid $oid -Scope $scope -MaxEpisodes $MaxEpisodes -OrderDir $dir
        # human_edits: any logged edit, any choice answered by a rule, or any explicit answer in the edits file
        $humanCount = @($scope.human_edits | Where-Object { $null -ne $_ }).Count + @($scope.answers | Where-Object { $null -ne $_ }).Count + @($scope.edits.answers | Where-Object { $null -ne $_ }).Count
        $line['scope'] = if ($humanCount -gt 0) { 'human_edits' } else { 'drafted' }
        $line['scope_hash'] = [string]$confirmed.scope_hash

        # worker-r, a model worker, once
        $wr = Start-Worker -Name 'worker-r' -Token $script:Tokens['worker-r'] -Tag ($id -replace '-', '')
        if (-not $wr.exited) { throw "worker_timeout: worker-r did not exit within $WorkerWaitSec s" }
        if ($wr.exit_code -ne 0) { $line['errors'] = @($line['errors']) + "worker-r exited with code $($wr.exit_code)" }
        $wrEvents = @(Get-AgentEvents -Events (Invoke-Api -Path '/api/events') -Agent 'worker-r' | Where-Object { $_.seq -gt $seqStart })
        $wrTypes = @($wrEvents | ForEach-Object { $_.type } | Select-Object -Unique)
        if ($wrTypes -notcontains 'build_reported' -and $wrTypes -notcontains 'landed') {
            throw "worker_no_build: worker-r reported no build (events: $($wrTypes -join ', '))"
        }

        # the gate's receipt for each task
        for ($i = 0; $i -lt $tasks.Count; $i++) {
            $t = $tasks[$i]
            $body = Wait-Until -What "the receipt of $t" -Probe { Invoke-Api404Tolerant -Path "/api/tasks/$t/receipt" } -Terminal { $null }
            $receipts += [pscustomobject]@{ task = $t; body = $body }
        }

        # out-of-scope differences: the order's own rules file, one ruling per item, then the gate again
        for ($i = 0; $i -lt $receipts.Count; $i++) {
            $r = $receipts[$i]
            if (@(Get-ReceiptItems -Body $r.body | Where-Object { $_.class -eq 'out_of_scope' }).Count -eq 0) { continue }
            $rFile = Join-Path $dir "receipt-$($r.task).json"
            Write-JsonFile -Path $rFile -Object ([ordered]@{ receipt = $r.body.receipt })
            $outRl = Join-Path $dir "rulings-$($r.task).json"
            try {
                & (Join-Path $intentDir 'ruling.ps1') -Server $script:Base -OwnerToken $script:OwnerToken -Order $id -OrderId $oid -ReceiptFile $rFile -RulesFile (Join-Path $intentDir "rulings\$id-rulings.json") -Out $outRl
            } catch {
                throw "needs_human_ruling: $($_.Exception.Message)"
            }
            $rl = Get-Content -LiteralPath $outRl -Raw | ConvertFrom-Json
            foreach ($x in @($rl.rulings)) { $script:RulingOwners["$($x.ruling_id)"] = $id }
            $line['rulings'] = [int]$line['rulings'] + @($rl.rulings).Count
            $after = Wait-Until -What "the gate on $($r.task) after the rulings" -Probe {
                $b = Invoke-Api404Tolerant -Path "/api/tasks/$($r.task)/receipt"
                if ($b -and @('landed', 'blocked') -contains [string]$b.receipt.gate_state) { $b } else { $null }
            } -Terminal { $null }
            $receipts[$i] = [pscustomobject]@{ task = $r.task; body = $after }
        }
    } catch {
        $msg = $_.Exception.Message
        $line['errors'] = @($line['errors']) + $msg
        $errorClass = if ($msg -match '^([a-z_]+):' -and $script:KnownErrorClasses -contains $Matches[1]) { $Matches[1] } else { 'incomplete' }
    }

    # classify from what the gate said, in this order: the replay's own stop, landed, the block classes
    $got = @($receipts | Where-Object { $null -ne $_.body })
    $items = @($got | ForEach-Object { Get-ReceiptItems -Body $_.body })
    $states = @($got | ForEach-Object { [string]$_.body.receipt.gate_state })
    $line['tasks'] = @($got | ForEach-Object {
            [ordered]@{ task = $_.task; receipt_hash = [string]$_.body.receipt_hash; gate_state = [string]$_.body.receipt.gate_state; items = @(Get-ReceiptItems -Body $_.body).Count }
        })
    if ($got.Count -gt 0) {
        $line['receipt_hash'] = [string]$got[-1].body.receipt_hash
        $line['gate_state'] = ($states -join ',')
    }
    if ($errorClass) {
        $line['outcome'] = $errorClass
    } elseif ($got.Count -eq 0) {
        $line['outcome'] = 'incomplete'
        $line['errors'] = @($line['errors']) + 'no receipt was read for any task'
    } elseif (@($states | Where-Object { $_ -ne 'landed' }).Count -eq 0) {
        $line['outcome'] = 'landed'
        $line['merged_sha'] = (@($got | ForEach-Object { Get-LandedCommit -Task $_.task }) | Where-Object { $_ } | Select-Object -Last 1)
    } elseif (@($items | Where-Object { $_.class -eq 'violates_ruling' }).Count -gt 0) {
        $line['outcome'] = 'blocked_violates_ruling'
    } elseif (@($items | Where-Object { $_.class -eq 'obligation_failed' }).Count -gt 0) {
        $line['outcome'] = 'blocked_obligation_failed'
    } elseif (@($items | Where-Object { $_.class -eq 'out_of_scope' }).Count -gt 0) {
        $line['outcome'] = 'blocked_out_of_scope'
    } else {
        $line['outcome'] = 'blocked_other'
    }
    $script:Outcomes[$id] = [string]$line['outcome']

    # the three catch flags, each set on its own
    $natural = @($items | Where-Object { $_.class -eq 'violates_ruling' -and $script:RulingOwners.ContainsKey("$($_.ruling_id)") -and $script:RulingOwners["$($_.ruling_id)"] -ne $id })
    $line['catch_natural_ruling'] = ($natural.Count -gt 0)
    $line['catch_base_only'] = ($line['outcome'] -ne 'landed' -and @($items | Where-Object { $_.class -eq 'obligation_failed' }).Count -gt 0 -and @($items | Where-Object { $_.class -eq 'violates_ruling' }).Count -eq 0)

    if ($Ord.supersedes) {
        $target = [string]$Ord.supersedes
        $line['supersedes_handling'] = [ordered]@{
            target = $target
            declared_in = 'orders.json'
            product_field = 'none: HumanCmd::PostOrder has no supersedes field (crates/acan-core/src/event.rs:75-82); the order is posted as a new, independent order'
            target_outcome_in_run = $script:Outcomes[$target]
            supersession_events = @(Get-SupersedeEvents)
        }
    }

    $watch.Stop()
    $line['wall_seconds'] = [math]::Round($watch.Elapsed.TotalSeconds, 1)
    try {
        $line['model_calls'] = Get-ModelCallCount -AfterSeq $seqStart -UpToSeq (Get-EventSeqMax)
    } catch {
        $line['errors'] = @($line['errors']) + "model call count failed: $($_.Exception.Message)"
    }
    $line['finished'] = (Get-Date).ToString('o')
    Add-ResultLine -Record $line

    if ($Drawn) {
        $firstTask = if ($tasks.Count -gt 0) { [string]$tasks[0] } else { $null }
        $null = Invoke-SeededCandidate -OrderId $id -Task $firstTask -RunId $script:RunId
    }
}

function New-ControlLine {
    param([string]$Id, [string]$AppliesTo, [string]$Expected, [string]$Outcome, $Result, [string]$Reason)
    [ordered]@{
        schema = $ResultsSchema; run_id = $script:RunId; kind = 'control'; id = $Id; applies_to = $AppliesTo
        actor = 'control identity (control-seeded), not a model'; provider = 'none'; attempt = 1
        expected_outcome = $Expected; outcome = $Outcome; reason = $Reason
        receipt_hash = $(if ($Result) { [string]$Result.receipt_hash } else { $null })
        gate_state = $(if ($Result) { [string]$Result.gate_state } else { $null })
        merged_sha = $null; model_calls = 0; wall_seconds = $null; errors = @()
        catch_natural_ruling = $false; catch_seeded = $false; catch_base_only = $false
        finished = (Get-Date).ToString('o')
    }
}

function Invoke-O1Controls {
    # The two controls that apply to o-1, run on o-1's first task as take.ps1 runs them.
    $t0 = if ($script:TasksByOrder.ContainsKey('o-1')) { [string]$script:TasksByOrder['o-1'][0] } else { $null }
    if (-not $t0) {
        foreach ($c in @('m-overfit', 'm-harmless-difference')) {
            Add-ResultLine -Record (New-ControlLine -Id $c -AppliesTo 'o-1' -Expected ([string](@($controlList | Where-Object { $_.id -eq $c })[0].expected_outcome)) -Outcome 'not_run' -Result $null -Reason 'o-1 has no task in this run')
        }
        return
    }
    $expectedOf = @{}
    foreach ($c in $controlList) { $expectedOf[[string]$c.id] = [string]$c.expected_outcome }

    # m-overfit: the control must fail s12 (bob keeps his own quota) while the gate is blocked
    $mo = Join-Path $script:CtrlDir 'm-overfit.json'
    $watch = [Diagnostics.Stopwatch]::StartNew()
    try {
        & (Join-Path $intentDir 'control.ps1') -Server $script:Base -Token $script:Tokens['control-seeded'] -Repo $script:Repo -Task $t0 -Bundle (Join-Path $intentDir 'controls\m-overfit.json') -Out $mo -StageWaitSec 300 -ReceiptWaitSec $ControlWaitSec
        $r = Read-ControlResult -Path $mo
        $s12 = @($r.items | Where-Object { $_.class -eq 'obligation_failed' -and ((ConvertTo-Json -Depth 20 -Compress $_) -match '"s12"') })
        $observed = if ([string]$r.gate_state -eq 'blocked' -and $s12.Count -ge 1) { 'as_expected' } else { 'not_as_expected' }
        $line = New-ControlLine -Id 'm-overfit' -AppliesTo 'o-1' -Expected $expectedOf['m-overfit'] -Outcome $observed -Result $r -Reason ''
        $line['s12_failed'] = $s12.Count
    } catch {
        $line = New-ControlLine -Id 'm-overfit' -AppliesTo 'o-1' -Expected $expectedOf['m-overfit'] -Outcome 'error' -Result $null -Reason ''
        $line['errors'] = @($_.Exception.Message)
    }
    $watch.Stop()
    $line['wall_seconds'] = [math]::Round($watch.Elapsed.TotalSeconds, 1)
    Add-ResultLine -Record $line

    # m-harmless-difference: out of scope at first, not a regression once the human rules it acceptable
    $mh = Join-Path $script:CtrlDir 'm-harmless-difference.json'
    $watch = [Diagnostics.Stopwatch]::StartNew()
    try {
        $ruled = Invoke-ControlWithRuling -Task $t0 -Bundle (Join-Path $intentDir 'controls\m-harmless-difference.json') -Out $mh -Order 'o-1' -OrderId $script:OidByOrder['o-1'] -RulesFile (Join-Path $intentDir 'rulings\o-1-rulings.json') -Run $run
        foreach ($rid in @($ruled.ruling_ids)) { $script:RulingOwners["$rid"] = 'control:m-harmless-difference' }
        $r = Read-ControlResult -Path $mh
        $xr = @($r.items | Where-Object { $_.class -eq 'out_of_scope' -and ((ConvertTo-Json -Depth 20 -Compress $_) -match 'x-request-id') })
        $observed = if ([string]$r.gate_state -eq 'landed' -and $xr.Count -ge 1) { 'as_expected' } else { 'not_as_expected' }
        $line = New-ControlLine -Id 'm-harmless-difference' -AppliesTo 'o-1' -Expected $expectedOf['m-harmless-difference'] -Outcome $observed -Result $r -Reason ''
        $line['x_request_id_out_of_scope'] = $xr.Count
        $line['ruling_ids'] = @($ruled.ruling_ids)
        if ([string]$r.gate_state -eq 'landed') { $line['merged_sha'] = Get-LandedCommit -Task $t0 }
    } catch {
        $line = New-ControlLine -Id 'm-harmless-difference' -AppliesTo 'o-1' -Expected $expectedOf['m-harmless-difference'] -Outcome 'error' -Result $null -Reason ''
        $line['errors'] = @($_.Exception.Message)
    }
    $watch.Stop()
    $line['wall_seconds'] = [math]::Round($watch.Elapsed.TotalSeconds, 1)
    Add-ResultLine -Record $line
}

function Get-ControlExpectation {
    param([string]$Id)
    [string](@($controlList | Where-Object { $_.id -eq $Id })[0].expected_outcome)
}

if (-not $DryRun) {
    # ---- take.ps1, as a library ---------------------------------------------------------------
    if (-not (Test-Path -LiteralPath $TakeScript)) { throw "missing input: $TakeScript" }
    if (-not (Select-String -LiteralPath $TakeScript -Pattern '\[switch\]\$LibraryOnly' -Quiet)) {
        throw 'take.ps1 has no -LibraryOnly guard yet; a live replay refuses to dot-source the take as a script'
    }
    . $TakeScript -LibraryOnly -Label $Label -Port $Port -RunsRoot $RunsRoot -DemoRepo $DemoRepo -Pin $Pin -AcanRepo $AcanRepo -AcanExe $AcanExe -WorkerExe $WorkerExe -LlmSource $LlmSource -EnvSource $EnvSource -MaxEpisodes $MaxEpisodes -HealthWaitSec $HealthWaitSec -StepWaitSec $StepWaitSec -WorkerWaitSec $WorkerWaitSec -ControlWaitSec $ControlWaitSec -RulingWaitSec $RulingWaitSec
    Test-TakeFunctions

    # ---- preflight: no writes until every check passes ---------------------------------------
    Assert-OutsideGit -Path $RunsRoot
    $freeGb = [math]::Round((Get-PSDrive C).Free / 1GB, 1)
    if ($freeGb -lt 5) { throw "C: has $freeGb GB free; the replay stops below 5 GB" }
    docker info *> $null
    if ($LASTEXITCODE -ne 0) { throw 'Docker is not running; start Docker Desktop before the replay' }
    foreach ($path in @($AcanExe, $WorkerExe, $LlmSource, $EnvSource, (Join-Path $intentDir 'orders.json'), (Join-Path $intentDir 'mutants.json'))) {
        if (-not (Test-Path -LiteralPath $path)) { throw "missing input: $path" }
    }
    $missingInputs = [System.Collections.Generic.List[string]]::new()
    foreach ($o in $orderList) {
        $bundlePath = Join-Path $intentDir "seeded\$($o.id).json"
        if (-not (Test-Path -LiteralPath $bundlePath)) { $missingInputs.Add($bundlePath); continue }
        $bundle = Read-JsonFile -Path $bundlePath
        if ($bundle.schema -ne 'acan-demo-control/1' -or [string]$bundle.applies_to -ne [string]$o.id -or [string]$bundle.base -ne $Pin) {
            throw "$bundlePath must be acan-demo-control/1 for $($o.id) at the pin"
        }
        $script:SeedBundleHashes[[string]$o.id] = Get-FileSha256 -Path $bundlePath
    }
    foreach ($o in $selected) {
        $editsPath = Join-Path $intentDir "$($o.id)-edits.json"
        if (-not (Test-Path -LiteralPath $editsPath)) { $missingInputs.Add($editsPath) }
        elseif ((Read-JsonFile -Path $editsPath).schema -ne 'acan-demo-intent-edits/2') { throw "$editsPath must be acan-demo-intent-edits/2" }
        $rulesPath = Join-Path $intentDir "rulings\$($o.id)-rulings.json"
        if (-not (Test-Path -LiteralPath $rulesPath)) { $missingInputs.Add($rulesPath) }
        elseif ((Read-JsonFile -Path $rulesPath).schema -ne 'acan-demo-intent-rulings/1') { throw "$rulesPath must be acan-demo-intent-rulings/1" }
    }
    if ($missingInputs.Count -gt 0) {
        throw "missing inputs, nothing was drawn or started: $($missingInputs -join '; ')"
    }
    $script:Routes = Read-Routing -Text (Get-Content -LiteralPath $LlmSource -Raw)
    $preflight = Test-LiveRoutes

    # ---- run folder and the draw, before any clone or candidate ------------------------------
    $script:RunId = "$Label-$(Get-Date -Format 'yyyyMMdd-HHmmss')"
    $run = Join-Path $RunsRoot $script:RunId
    $script:CtrlDir = Join-Path $run 'controls'
    $script:ResultsPath = Join-Path $run 'results.jsonl'
    $orderIds = @($orderList | ForEach-Object { [string]$_.id })
    $script:Summary = [ordered]@{
        schema = $SummarySchema; run_id = $script:RunId; label = $Label; status = 'starting'
        partial = ($Only.Count -gt 0); only = @($Only); orders_planned = @($selected | ForEach-Object { [string]$_.id })
        pin = $Pin; port = $Port; started = (Get-Date).ToString('o'); preflight = $preflight
        draw_sha256 = $null; demo_refs_before = $null; demo_refs_after = $null; failure = $null
        incomplete_reasons = @(); secret_scan = 'not_run'
    }
    $null = New-Item -ItemType Directory -Path $run, $script:CtrlDir, (Join-Path $run 'logs'), (Join-Path $run 'keys'), (Join-Path $run 'config') -Force
    try {
        $seed = [string][System.Security.Cryptography.RandomNumberGenerator]::GetInt32(1, [int]::MaxValue)
        $drawn = @(Get-DrawnOrders -Seed $seed -Ids $orderIds -Count $DrawCount)
        $drawRecord = [ordered]@{
            schema = $DrawSchema; run_id = $script:RunId; drawn_at = (Get-Date).ToString('o')
            algorithm = 'the orders with the smallest SHA-256 of "<seed>|<order id>", first 4, in orders.json order'
            seed = $seed; pool = $orderIds; drawn = $drawn
            orders_json_sha256 = Get-FileSha256 -Path (Join-Path $intentDir 'orders.json')
            mutants_json_sha256 = Get-FileSha256 -Path (Join-Path $intentDir 'mutants.json')
            seeded_bundle_sha256 = $script:SeedBundleHashes
        }
        $drawPath = Join-Path $run 'draw.json'
        Write-JsonFile -Path $drawPath -Object $drawRecord
        $script:Summary['draw_sha256'] = Get-FileSha256 -Path $drawPath
        $script:Summary['status'] = 'drawn'
        Write-JsonFile -Path (Join-Path $run 'summary.json') -Object $script:Summary

        # ---- keys, routing, tokens; the clone; the server; the agents ------------------------
        Copy-Item -LiteralPath $EnvSource -Destination (Join-Path $run 'keys\.env')
        Copy-Item -LiteralPath $LlmSource -Destination (Join-Path $run 'config\llm.toml')
        $script:LlmPath = Join-Path $run 'config\llm.toml'
        $script:EnvPath = Join-Path $run 'keys\.env'
        $keys = Read-KeyFile -Path $script:EnvPath
        foreach ($value in $keys.Values) { if ($value.Length -ge 8) { $script:Secrets.Add($value) } }
        $script:Summary['demo_refs_before'] = Get-DemoRefs
        New-TakeClone -Run $run -DemoRepo $DemoRepo -Pin $Pin | Out-Null
        $script:Tokens = @{
            owner            = 'acan_owner_' + [guid]::NewGuid().ToString('N')
            'worker-r'       = 'acan_agent_r_' + [guid]::NewGuid().ToString('N')
            'control-seeded' = 'acan_agent_c_' + [guid]::NewGuid().ToString('N')
        }
        $tokenEntries = @(
            @{ token = $script:Tokens.owner; principal = @{ kind = 'member'; id = 'owner' } },
            @{ token = $script:Tokens['worker-r']; principal = @{ kind = 'agent'; id = 'worker-r' } },
            @{ token = $script:Tokens['control-seeded']; principal = @{ kind = 'agent'; id = 'control-seeded' } }
        )
        New-Item -ItemType Directory -Path (Join-Path $script:Repo '.acan') -Force | Out-Null
        Set-Content -LiteralPath (Join-Path $script:Repo '.acan\tokens.json') -Value (ConvertTo-Json -Depth 5 $tokenEntries) -Encoding utf8
        $script:OwnerToken = $script:Tokens.owner
        foreach ($tok in $script:Tokens.Values) { $script:Secrets.Add([string]$tok) }
        $script:Base = "http://127.0.0.1:$Port"
        Start-TakeServer -Run $run -Repo $script:Repo -Port $Port -AcanExe $AcanExe -LlmPath $script:LlmPath -EnvPath $script:EnvPath -LogTag 'replay' -HealthWaitSec $HealthWaitSec | Out-Null
        $existing = @(Invoke-Api -Path '/api/events')
        foreach ($name in @('worker-r', 'control-seeded')) {
            $already = @($existing | Where-Object { $_.event.type -eq 'agent_registered' -and (ConvertTo-Json -Depth 8 -Compress $_) -like "*`"$name`"*" }).Count
            if ($already -eq 0) { $null = Send-Command @{ cmd = 'register_agent'; agent = $name } }
        }

        # ---- the orders, one attempt each, in orders.json order -----------------------------
        $script:Summary['status'] = 'running'
        Write-JsonFile -Path (Join-Path $run 'summary.json') -Object $script:Summary
        foreach ($ord in $selected) {
            $drawnHere = $drawn -contains [string]$ord.id
            Invoke-ReplayOrder -Ord $ord -Drawn $drawnHere
            if ([string]$ord.id -eq 'o-1') { Invoke-O1Controls }
        }
        if (-not ($selected | Where-Object { [string]$_.id -eq 'o-1' })) {
            foreach ($c in @('m-overfit', 'm-harmless-difference')) {
                Add-ResultLine -Record (New-ControlLine -Id $c -AppliesTo 'o-1' -Expected (Get-ControlExpectation -Id $c) -Outcome 'not_run' -Result $null -Reason 'o-1 is not in this run')
            }
        }
        # Two controls have no driver or bundle in demo/intent, so they are reported not_run, never passed.
        Add-ResultLine -Record (New-ControlLine -Id 'm-broad-scope' -AppliesTo 'o-1' -Expected (Get-ControlExpectation -Id 'm-broad-scope') -Outcome 'not_run' -Result $null -Reason 'no scope-draft driver or bundle for m-broad-scope exists in demo/intent')
        Add-ResultLine -Record (New-ControlLine -Id 'm-missing-fixture' -AppliesTo 'f-accounts' -Expected (Get-ControlExpectation -Id 'm-missing-fixture') -Outcome 'not_run' -Result $null -Reason 'no fixture-copy driver or bundle for m-missing-fixture exists in demo/intent')
    } catch {
        $script:Summary['failure'] = $_.Exception.Message
        Write-Output "FAILED: $($_.Exception.Message)"
    } finally {
        if ($script:Proc -and -not $script:Proc.HasExited) {
            Stop-Process -Id $script:Proc.Id -Force -ErrorAction SilentlyContinue
        }
        $demoAfter = $null
        try { $demoAfter = Get-DemoRefs } catch { $demoAfter = "unreadable: $($_.Exception.Message)" }
        $script:Summary['demo_refs_after'] = $demoAfter
        $script:Summary['demo_unchanged'] = if ($null -eq $script:Summary['demo_refs_before']) { 'not_read' } else { ($script:Summary['demo_refs_before'] -eq $demoAfter) }

        $lines = @($script:ResultRecords)
        $orderLines = @($lines | Where-Object { $_.kind -eq 'order' })
        $controlLines = @($lines | Where-Object { $_.kind -eq 'control' })
        $seededLines = @($lines | Where-Object { $_.kind -eq 'seeded' })
        $script:Summary['catches'] = [ordered]@{
            natural_ruling = @($lines | Where-Object { $_.catch_natural_ruling }).Count
            seeded_demonstrations = @($seededLines | Where-Object { $_.catch_seeded }).Count
            base_only = @($lines | Where-Object { $_.catch_base_only }).Count
            note = 'counted separately; one line can count in more than one of the three'
        }
        $script:Summary['totals'] = [ordered]@{
            wall_seconds = [math]::Round((@($lines | ForEach-Object { [double]$_.wall_seconds } | Measure-Object -Sum).Sum), 1)
            model_calls = (@($orderLines | ForEach-Object { [int]$_.model_calls } | Measure-Object -Sum).Sum)
        }
        $reasons = [System.Collections.Generic.List[string]]::new()
        if ($script:Summary['failure']) { $reasons.Add("run stopped: $($script:Summary['failure'])") }
        if ($Only.Count -gt 0) { $reasons.Add("rehearsal: only $($Only -join ', ') were run, not all 12") }
        if ($orderLines.Count -lt $selected.Count) { $reasons.Add("only $($orderLines.Count) of $($selected.Count) orders have a result line") }
        foreach ($l in $orderLines) {
            if ($script:IncompleteClasses -contains [string]$l.outcome) { $reasons.Add("$($l.id): $($l.outcome)") }
        }
        foreach ($l in $controlLines) {
            if ($l.outcome -in @('not_run', 'error')) { $reasons.Add("$($l.id): $($l.outcome) ($($l.reason))") }
        }
        $script:Summary['incomplete_reasons'] = @($reasons)
        $script:Summary['status'] = if ($reasons.Count -eq 0) { 'complete' } else { 'incomplete' }
        $script:Summary['finished'] = (Get-Date).ToString('o')
        $script:Summary['orders'] = @($orderLines | ForEach-Object { [ordered]@{ id = $_.id; outcome = $_.outcome; scope = $_.scope; drawn_seeded = $_.drawn_seeded; gate_state = $_.gate_state; wall_seconds = $_.wall_seconds; model_calls = $_.model_calls } })
        $script:Summary['controls'] = @($controlLines + $seededLines | ForEach-Object { [ordered]@{ id = $_.id; outcome = $_.outcome } })
        $drawPath = Join-Path $run 'draw.json'
        if (Test-Path -LiteralPath $drawPath) {
            $script:Summary['seed'] = (Read-JsonFile -Path $drawPath).seed
            $script:Summary['drawn'] = @((Read-JsonFile -Path $drawPath).drawn)
        }

        # secret scan over the logs, the results and the summary; values are never printed
        $scanHits = [System.Collections.Generic.List[string]]::new()
        $scanTargets = @(Get-ChildItem -LiteralPath (Join-Path $run 'logs') -File -ErrorAction SilentlyContinue) + @(Get-Item -LiteralPath $script:ResultsPath -ErrorAction SilentlyContinue)
        foreach ($file in $scanTargets) {
            $text = Get-Content -LiteralPath $file.FullName -Raw -ErrorAction SilentlyContinue
            foreach ($secret in $script:Secrets) { if ($text -and $text.Contains($secret)) { $scanHits.Add($file.Name) } }
        }
        $summaryText = ConvertTo-Json -Depth 40 $script:Summary
        foreach ($secret in $script:Secrets) { if ($summaryText.Contains($secret)) { $scanHits.Add('summary.json') } }
        $script:Summary['secret_scan'] = if ($scanHits.Count -eq 0) { 'clean' } else { "hits: $((@($scanHits) | Select-Object -Unique) -join ', ')" }
        $summaryText = ConvertTo-Json -Depth 40 $script:Summary
        Set-Content -LiteralPath (Join-Path $run 'summary.json') -Value $summaryText -Encoding utf8
        Write-Output "run: $run"
        Write-Output "status: $($script:Summary['status'])"
        if ($scanHits.Count -gt 0) {
            Write-Output "secret scan found a token or key in: $($scanHits -join ', ')"
            exit 2
        }
    }
}
