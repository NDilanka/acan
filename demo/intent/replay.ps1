<#
.SYNOPSIS
X01 replay: the 12 pre-registered orders in demo/intent/orders.json and the four controls in
demo/intent/mutants.json, run once each through the real Acan server with live Meta models.

.DESCRIPTION
-DryRun reads orders.json and mutants.json only, checks their shape, and prints the plan. It starts
nothing, clones nothing, calls no model, and draws nothing. The seed is generated at the start of a
live run, and a dry run never shows which orders it draws.

A live run refuses to start offline, writes the draw (seed and drawn orders) and hashes it before the
first candidate exists, and writes one results.jsonl line per order and per control into a new run
folder under -RunsRoot. The README section "Replay (X01)" describes the inputs and the counts.

Scope and ruling decisions for every order are delegated by the owner to the coordinator (an AI). The
replay does not decide them and does not write them. When an order needs a scope decision or a ruling
and its file is not there yet, the run pauses: it writes the draft (or the ruling draft) into the run
folder, prints needs_scope_decision or needs_ruling with the path, stops the server, and exits with
code 3. The coordinator writes the file under demo/intent and resumes with -ResumeRun <run folder>.
Every decision file names who decided it in decided_by.

Exit codes: 0 the run finished (summary.json says complete or incomplete), 1 the run stopped on a
failure, 2 a secret was found in a log or result, 3 the run paused for a decision.
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
    # The most model calls the worker's edit loop may make per build. take.ps1's value (200 since D02M).
    [int]$WorkerMaxSteps = 200,
    # How long a control waits for its task to be open for its claim. take.ps1's value.
    [int]$ClaimWaitSec = 600,
    # A rehearsal runs only these orders. The draw still covers all 12, and the run is reported partial.
    [string[]]$Only = @(),
    # The folder of a run that paused for a decision. Its summary.json, draw.json and repo\ are reused.
    [string]$ResumeRun = '',
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
$ResultsSchema = 'acan-replay-result/1'
$SummarySchema = 'acan-replay-summary/1'
$DrawSchema = 'acan-replay-draw/1'
$DrawCount = 4
$intentDir = Join-Path $AcanRepo 'demo\intent'
# Who decided a scope or ruling. A file's decided_by is one of the two file values; the record label is
# the matching label. A coordinator decision is never recorded as the human's.
$DelegatedDecider = 'coordinator, delegated by the owner'
$DelegatedLabel = 'coordinator (delegated by the owner)'
$HumanLabel = 'human (owner)'
# No decision file may omit decided_by, for any order. The o-1 files name the coordinator too (D02), so
# no order is human-decided by default.
$HumanDecidedOrders = @()

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
    Write-Output '  1. Decisions: demo/intent/<id>-edits.json (acan-demo-intent-edits/2) and'
    Write-Output '     demo/intent/rulings/<id>-rulings.json (acan-demo-intent-rulings/1) are read where an order needs them.'
    Write-Output '     A missing file pauses the run (exit 3): needs_scope_decision or needs_ruling is printed with the'
    Write-Output '     draft to read. Each file names its decider in decided_by (coordinator, delegated by the owner).'
    Write-Output '  2. Draw: a seed from the OS random source, then 4 of 12 by SHA-256 of "seed|id". The seed, the drawn'
    Write-Output '     list and one generated injected bundle per drawn order (a header on GET /health) go to draw.json,'
    Write-Output '     whose SHA-256 goes into summary.json, all before any clone.'
    Write-Output '  3. Clone the demo at the pin, write tokens (owner, worker-r, control-seeded), start acan serve'
    Write-Output "     on port $Port, register worker-r and control-seeded."
    Write-Output '  4. Each order once: post, split, draft, scope edits and confirm, worker-r (model), receipts,'
    Write-Output '     rulings, and the injected candidate when the draw picked the order. Paused orders resume with'
    Write-Output '     -ResumeRun <run folder>; an order that finished in an earlier invocation is not run again.'
    Write-Output '     The injected candidate of a drawn order runs after that order, on its control task (a split task the worker did not build; the task worktree HEAD).'
    Write-Output '     After the order and its controls, its tasks that did not land end through cancel_order (member route), so no later worker takes them.'
    Write-Output '  5. Controls after o-1: m-overfit and m-harmless-difference. m-broad-scope: a separate order with the'
    Write-Output '     o-1 sentence, revised with broad items (method ANY on each link route the draft names), confirmation tried with no broad'
    Write-Output '     acknowledgement. m-missing-fixture: the f-accounts copy without f5 and its static prediction;'
    Write-Output '     the gate is not run on the copy, so that control stays not_run.'
    Write-Output '  6. summary.json (complete, incomplete with reasons, or paused), results.jsonl, secret scan.'
    Write-Output ''
    Write-Output 'Not shown: which orders the seeded draw picks. It is decided at live start.'
}

# echoed into the console log of every invocation, so the run record shows how the replay was called
Write-Output ("arguments: label={0} only=[{1}] resume=[{2}] dry={3} port={4}" -f $Label, ($Only -join ','), $ResumeRun, [bool]$DryRun, $Port)

if ($DryRun) {
    Show-Plan
    exit 0
}

# ================================================================================================
# Live path. The HTTP, wait, worker, receipt, scope, ruling and git helpers come from take.ps1 through
# its -LibraryOnly guard. The take's own main body calls the same functions, so there is one copy of each.
# ================================================================================================

$script:KnownErrorClasses = @('setup_failed', 'draft_not_ready', 'draft_changed', 'needs_human_scope', 'ruling_refused', 'worker_timeout', 'worker_no_build')
$script:PauseClasses = @('needs_scope_decision', 'needs_ruling')
$script:IncompleteClasses = @('incomplete') + $script:KnownErrorClasses
$script:Outcomes = @{}
$script:RulingOwners = @{}
$script:OidByOrder = @{}
$script:TasksByOrder = @{}
$script:InjectedHashes = @{}
$script:ResultRecords = [System.Collections.Generic.List[object]]::new()
$script:Tokens = @{}
$script:Routes = $null
$script:Proc = $null
$script:Base = ''
$script:Repo = ''
$script:LlmPath = ''
$script:EnvPath = ''
$script:OwnerToken = ''
$script:Pause = $null
$script:ExitCode = 0

function Write-JsonFile {
    param([string]$Path, $Object)
    ConvertTo-Json -Depth 40 $Object | Set-Content -LiteralPath $Path -Encoding utf8
}

function Get-FileSha256 {
    param([string]$Path)
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Get-TextSha256 {
    param([string]$Text)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        ([BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($Text))) -replace '-', '').ToLowerInvariant()
    } finally {
        $sha.Dispose()
    }
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

function Get-DecisionLabel {
    # Who decided a scope edits file or a rulings file. A coordinator decision must say so in
    # decided_by; a file with no decided_by is accepted only for an order the human decided.
    param($Doc, [string]$Order)
    $decided = [string]$Doc.decided_by
    if ($decided -eq '') {
        if ($HumanDecidedOrders -contains $Order) { return $HumanLabel }
        throw "$Order's decision file names no decided_by; a coordinator decision must say '$DelegatedDecider'"
    }
    if ($decided -eq $DelegatedDecider) { return $DelegatedLabel }
    if ($decided -eq $HumanLabel) { return $HumanLabel }
    throw "$Order's decision file names decided_by '$decided'; only '$DelegatedDecider' or '$HumanLabel' is accepted"
}

function Read-OrderState {
    # The order's own checkpoint: the ids, the draft hash, the scope decision, and the rulings done.
    # Empty when the order has not started.
    param([string]$Dir)
    $path = Join-Path $Dir 'state.json'
    if (-not (Test-Path -LiteralPath $path)) { return @{} }
    Get-Content -LiteralPath $path -Raw | ConvertFrom-Json -AsHashtable
}

function Save-OrderState {
    param([string]$Dir, $State)
    Write-JsonFile -Path (Join-Path $Dir 'state.json') -Object $State
}

function Find-UnruledItem {
    # The first out-of-scope item of a receipt that no rule in the order's rules file covers, or null.
    # The match is the same as ruling.ps1: the field pattern, and the candidate state when it is not any.
    param($Body, [string]$RulesFile)
    $rules = @()
    if (Test-Path -LiteralPath $RulesFile) { $rules = @((Read-JsonFile -Path $RulesFile).rules) }
    foreach ($item in @(Get-ReceiptItems -Body $Body | Where-Object { $_.class -eq 'out_of_scope' })) {
        $ev = @($item.evidence | Where-Object { $_.slot -eq 'candidate' })
        if ($ev.Count -ne 1) { throw "setup_failed: out-of-scope item $($item.item_id) has $($ev.Count) candidate evidence rows" }
        $episode = [string]$item.episode_id
        if (-not $episode) { throw "setup_failed: out-of-scope item $($item.item_id) has no episode" }
        $field = [string]$ev[0].field
        $candState = [string]$ev[0].value.state
        $covered = $false
        foreach ($rule in $rules) {
            if ($field -notmatch $rule.field) { continue }
            if ($rule.candidate -ne 'any' -and $rule.candidate -ne $candState) { continue }
            $covered = $true
            break
        }
        if (-not $covered) {
            return [pscustomobject]@{ episode = $episode; field = $field; state = $candState; item = [string]$item.item_id }
        }
    }
    $null
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
    # The helpers this file calls. take.ps1 defines each of them when it is dot-sourced with -LibraryOnly.
    $needed = @(
        'Send-Command', 'Invoke-Api', 'Invoke-Api404Tolerant', 'Wait-Until', 'Get-OrderTasks', 'Start-Worker',
        'Get-AgentEvents', 'Read-ControlResult', 'Read-KeyFile', 'Read-Routing', 'Assert-OutsideGit', 'Get-DemoRefs',
        'New-TakeClone', 'Start-TakeServer', 'Invoke-ScopeEdits', 'Invoke-ScopeConfirm', 'Invoke-ControlWithRuling',
        'Wait-TaskClaimable', 'Get-TaskView', 'Get-WorkerTask', 'Close-OrderLeftovers'
    )
    $missing = @($needed | Where-Object { -not (Get-Command -Name $_ -CommandType Function -ErrorAction SilentlyContinue) })
    if ($missing.Count -gt 0) {
        throw "take.ps1 does not define: $($missing -join ', '); the library functions are missing (see take.ps1 -LibraryOnly)"
    }
}

function Add-ResultLine {
    param($Record)
    $script:ResultRecords.Add($Record)
    Add-Content -LiteralPath $script:ResultsPath -Value (ConvertTo-Json -Depth 30 -Compress $Record) -Encoding utf8
}

function Test-ControlLine {
    # True when a control's result line is already in this run (a resumed run does not run it twice).
    param([string]$Id)
    @($script:ResultRecords | Where-Object { $_.kind -eq 'control' -and [string]$_.id -eq $Id }).Count -gt 0
}

function Get-ReceiptItems {
    param($Body)
    @($Body.receipt.items | Where-Object { $null -ne $_ })
}

# ---- the injector: one out-of-scope change for each drawn order ------------------------------------
# The change is one response header on GET /health, a route that no order's scope names. It is generated
# from the pinned src/server.ts, so no file is kept per order. The anchor is the whole health handler; it
# must occur exactly once in the pinned file, and the preflight checks that before any write.
$script:InjectorId = 'header-on-health/1'
$script:InjectorHeader = 'x-replay-injected'
$script:InjectorRoute = 'GET /health'
$script:InjectorFile = 'src/server.ts'
$script:AnchorSha = $null

function Get-PinnedText {
    param([string]$Path)
    $lines = & git -C $DemoRepo show "${Pin}:$Path"
    if ($LASTEXITCODE -ne 0) { throw "git show ${Pin}:$Path failed" }
    [string]::Join("`n", $lines) + "`n"
}

function Get-InjectorEdit {
    # The one edit for an order: the health handler, with the header added to its response.
    param([string]$OrderId)
    [ordered]@{
        old = "function health(ctx: Context): void {`n  sendJson(ctx.res, 200, { ok: true });`n}"
        new = "function health(ctx: Context): void {`n  sendJson(ctx.res, 200, { ok: true }, { '$script:InjectorHeader': '$OrderId' });`n}"
    }
}

function Get-InjectorAnchorSha {
    # The anchor must occur exactly once in the pinned file, or the bundle would edit nothing or the wrong place.
    $text = Get-PinnedText -Path $script:InjectorFile
    $anchor = [string](Get-InjectorEdit -OrderId 'x').old
    $at = $text.IndexOf($anchor, [StringComparison]::Ordinal)
    if ($at -lt 0) { throw "the injector's anchor is not in ${Pin}:$($script:InjectorFile); the pin changed" }
    if ($text.IndexOf($anchor, $at + 1, [StringComparison]::Ordinal) -ge 0) { throw "the injector's anchor occurs twice in ${Pin}:$($script:InjectorFile)" }
    Get-TextSha256 -Text $anchor
}

function New-InjectedChangeRecord {
    param([string]$OrderId)
    [ordered]@{
        injector = $script:InjectorId; route = $script:InjectorRoute; header = $script:InjectorHeader; value = $OrderId
        placement = 'the task worktree HEAD of the order''s control task (a split task worker-r did not build; the order''s base there), through control.ps1 -OnWorkerCandidate'
    }
}

function New-InjectedBundle {
    # The acan-demo-control/1 bundle for one drawn order. control.ps1 -OnWorkerCandidate applies it at the HEAD of the order's control task.
    param([string]$OrderId)
    $edit = Get-InjectorEdit -OrderId $OrderId
    [ordered]@{
        schema         = 'acan-demo-control/1'
        id             = "inject-$OrderId"
        kind           = 'injected_out_of_scope'
        applies_to     = $OrderId
        base           = $Pin
        control_of     = "mutants.json seeded_mutants: one out-of-scope change for each drawn order (injector $($script:InjectorId))"
        description    = "One response header on $($script:InjectorRoute), an unrelated route: $($script:InjectorHeader): $OrderId. No status or body byte changes."
        injector       = [ordered]@{ id = $script:InjectorId; route = $script:InjectorRoute; header = $script:InjectorHeader; value = $OrderId; anchor_sha256 = $script:AnchorSha }
        declared_files = @($script:InjectorFile)
        files          = @([ordered]@{ path = $script:InjectorFile; edits = @($edit) })
    }
}

function Test-SeededLine {
    param([string]$Id)
    @($script:ResultRecords | Where-Object { $_.kind -eq 'seeded' -and [string]$_.order -eq $Id }).Count -gt 0
}

function New-SeededNotRun {
    param([string]$OrderId, [string]$RunId, [string]$Reason)
    [ordered]@{
        schema = $ResultsSchema; run_id = $RunId; kind = 'seeded'; id = "seeded-$OrderId"; order = $OrderId
        injected_change = (New-InjectedChangeRecord -OrderId $OrderId)
        bundle_sha256 = $script:InjectedHashes[$OrderId]; actor = 'control-seeded (control identity, not a model)'
        provider = 'none'; scope = 'not_applicable'; attempt = 1; outcome = 'not_run'; reason = $Reason
        receipt_hash = $null; merged_sha = $null; gate_state = $null; model_calls = 0; wall_seconds = $null
        catch_natural_ruling = $false; catch_seeded = $false; catch_base_only = $false; errors = @()
        finished = (Get-Date).ToString('o')
    }
}

function Invoke-SeededCandidate {
    # A drawn order gets one injected out-of-scope change, run as a control on the order's control task: a split
    # task that worker-r did not build (Get-ControlTask). control.ps1 -OnWorkerCandidate reads that task's worktree
    # at its HEAD (the order's base there, since no worker built it) and commits the injected header on top of it.
    # The gate sees no ruling from the replay, so an injected item that stays unruled is judged as blocked. A claim
    # refused because the task already landed or was closed is an error line, and it is not a catch.
    param([string]$OrderId, [string]$Task, [string]$RunId)
    $bundle = Join-Path $script:CtrlDir "injected-$OrderId.json"
    $out = Join-Path $script:CtrlDir "seeded-$OrderId.json"
    $line = [ordered]@{
        schema = $ResultsSchema; run_id = $RunId; kind = 'seeded'; id = "seeded-$OrderId"; order = $OrderId; task = $(if ($Task) { $Task } else { $null })
        injected_change = (New-InjectedChangeRecord -OrderId $OrderId)
        bundle = "controls\injected-$OrderId.json"; bundle_sha256 = $script:InjectedHashes[$OrderId]
        actor = 'control-seeded (control identity, not a model)'
        provider = 'none'; scope = 'not_applicable'; attempt = 1; outcome = 'not_run'
        receipt_hash = $null; merged_sha = $null; gate_state = $null; model_calls = 0; wall_seconds = $null
        catch_natural_ruling = $false; catch_seeded = $false; catch_base_only = $false; errors = @()
        started = (Get-Date).ToString('o'); finished = $null
    }
    $watch = [Diagnostics.Stopwatch]::StartNew()
    try {
        if (-not $Task) { throw 'not_run: the order has no task other than the worker''s, so there is no task to inject into' }
        Wait-TaskClaimable -Task $Task -Token $script:Tokens['control-seeded'] -Seconds $ClaimWaitSec | Out-Null
        & (Join-Path $intentDir 'control.ps1') -Server $script:Base -Token $script:Tokens['control-seeded'] -Repo $script:Repo -Task $Task -Bundle $bundle -Out $out -StageWaitSec 300 -ReceiptWaitSec $ControlWaitSec -OnWorkerCandidate -RedeclareOnce
        $r = Read-ControlResult -Path $out
        if ($null -eq $r) { throw 'error: the seeded control wrote no result file' }
        if (-not $r.receipt_hash) { throw "error: the seeded control got no receipt; its error: $([string]$r.error)" }
        $gate = [string]$r.gate_state
        $blocking = @($r.items | Where-Object { $_.class -in @('out_of_scope', 'violates_ruling', 'obligation_failed') })
        $line['receipt_hash'] = [string]$r.receipt_hash
        $line['base_source'] = [string]$r.base_source
        $line['base_head'] = [string]$r.base_head
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

function Get-ControlTask {
    # The task an order's control or seeded change uses: the first split task that the worker did not build, so
    # no control claims the worker's task. Null when the order split into one task only.
    param([string[]]$Tasks, [string]$WorkerTask)
    $other = @($Tasks | Where-Object { $_ -and $_ -ne $WorkerTask })
    if ($other.Count -gt 0) { return [string]$other[0] }
    return $null
}

function Close-ReplayOrder {
    # Ends the order's tasks that did not land, after the order and its controls. Until then they stay open
    # for the controls; after it they would be claimable by a later order's worker, because list_tasks has no
    # order filter. The result goes into the order's state.json; a finished order is closed once.
    param($Ord)
    $dir = Join-Path $run ([string]$Ord.id)
    $state = Read-OrderState -Dir $dir
    if (-not $state.raw -or $state.leftovers_closed) { return }
    $res = Close-OrderLeftovers -Raw ([string]$state.raw) -Tasks @($state.tasks)
    $state.leftovers = [ordered]@{ dropped = @($res.dropped); reply = @($res.reply) }
    $state.leftovers_closed = $true
    Save-OrderState -Dir $dir -State $state
}

function Invoke-ReplayOrder {
    # One order, from its post to its last ruling. A decision the replay does not have yet stops the
    # order at that point: it sets $script:Pause and returns, and nothing about the order is recorded
    # as a result. Every phase checkpoints into the order's state.json, so a resumed run goes on from
    # the phase that paused, and never posts, splits, drafts or runs a worker twice.
    param($Ord, [bool]$Drawn)
    $id = [string]$Ord.id
    $dir = Join-Path $run $id
    New-Item -ItemType Directory -Path $dir -Force | Out-Null
    $state = Read-OrderState -Dir $dir
    $resumed = $state.Count -gt 0
    if (-not $resumed) {
        $state = @{ seq_start = (Get-EventSeqMax); ruled_tasks = @(); rulings = 0 }
        Save-OrderState -Dir $dir -State $state
    }
    $seqStart = [int64]$state.seq_start
    $line = [ordered]@{
        schema = $ResultsSchema; run_id = $script:RunId; kind = 'order'; id = $id; machine = [string]$Ord.machine
        supersedes = $Ord.supersedes; drawn_seeded = $Drawn; attempt = 1; resumed_after_pause = $resumed
        actor = 'owner (member HTTP); foreman (model); worker-r (model)'
        provider = "foreman $($script:Routes.foreman.provider) / $($script:Routes.foreman.model); worker $($script:Routes.worker.provider) / $($script:Routes.worker.model)"
        scope = 'not_reached'; scope_decided_by = $null; scope_hash = $null; outcome = 'incomplete'; receipt_hash = $null; merged_sha = $null
        gate_state = $null; rulings = 0; rulings_decided_by = $null; split_tasks = @(); tasks = @(); model_calls = $null; wall_seconds = $null; errors = @()
        catch_natural_ruling = $false; catch_seeded = $false; catch_base_only = $false
        started = (Get-Date).ToString('o'); finished = $null
    }
    $watch = [Diagnostics.Stopwatch]::StartNew()
    $tasks = @()
    $receipts = @()
    $errorClass = $null
    $pauseInfo = $null
    try {
        # post the order, once
        if (-not $state.raw) {
            $posted = Send-Command @{ cmd = 'post_order'; machine = $Ord.machine; sentence = $Ord.sentence; witnesses = @() }
            $postedEvent = $posted | Where-Object { $_.event.type -eq 'order_posted' } | Select-Object -First 1
            if (-not $postedEvent) { throw 'setup_failed: post_order returned no order_posted event' }
            $raw0 = "$($postedEvent.event.order)"
            $state.raw = $raw0
            $state.oid = if ($raw0 -like 'o-*') { $raw0 } else { "o-$raw0" }
            Save-OrderState -Dir $dir -State $state
        }
        $raw = [string]$state.raw
        $oid = [string]$state.oid
        $script:OidByOrder[$id] = $oid

        # split, then the foreman's draft
        $null = Wait-Until -What "the split of $id" -Probe {
            Invoke-Api -Path '/api/events' | Where-Object { $_.event.type -eq 'order_split' -and "$($_.event.order)" -eq $raw } | Select-Object -First 1
        } -Terminal { $null }
        $tasks = @(Get-OrderTasks -Events (Invoke-Api -Path '/api/events') -Raw $raw)
        if ($tasks.Count -eq 0) { throw 'setup_failed: the split of the order made no task' }
        $state.tasks = $tasks
        Save-OrderState -Dir $dir -State $state
        $script:TasksByOrder[$id] = $tasks
        $line['split_tasks'] = $tasks
        $view = Wait-Until -What "the foreman draft for $id" -Probe { Invoke-Api404Tolerant -Path "/api/orders/$oid/scope-draft" } -Terminal { $null }
        if ($view.state -ne 'drafted') { throw "draft_not_ready: the draft state is $($view.state)" }
        $draftPath = Join-Path $dir 'scope-draft.json'
        $draftKey = ConvertTo-Json -Depth 60 -Compress @{ order = $view.draft.order; rev = $view.draft.rev; base_commit = $view.draft.base_commit; proposal = $view.draft.draft.proposal }
        $draftSha = Get-TextSha256 -Text $draftKey
        if (-not $state.draft_sha256) {
            Write-JsonFile -Path $draftPath -Object $view.draft
            $state.draft_sha256 = $draftSha
            Save-OrderState -Dir $dir -State $state
        } elseif ($state.draft_sha256 -ne $draftSha) {
            throw "draft_changed: the foreman's draft for $id differs from the one recorded before the pause; any decision file was written for the recorded draft"
        }

        # the scope decision, once per order: a missing file pauses here, before any model worker runs
        if (-not $state.scope) {
            $editsPath = Join-Path $intentDir "$id-edits.json"
            if (-not (Test-Path -LiteralPath $editsPath)) {
                $pauseInfo = [ordered]@{ kind = 'needs_scope_decision'; order = $id; draft = $draftPath }
                throw "needs_scope_decision: $id has no edits file yet; the draft to read is $draftPath"
            }
            $edits = Read-JsonFile -Path $editsPath
            if ($edits.schema -ne 'acan-demo-intent-edits/2') { throw "setup_failed: $editsPath must have schema acan-demo-intent-edits/2" }
            $label = Get-DecisionLabel -Doc $edits -Order $id
            # An edits file written for another draft does not match this run's draft: the decision is for the
            # coordinator to write again, so the run pauses here (resumable: the draft is pinned in state.json).
            try {
                $scope = Invoke-ScopeEdits -Order $id -Raw $raw -Oid $oid -View $view -EditsFile $editsPath -OrderDir $dir -DecidedBy $label
            } catch {
                $mismatch = [string]$_.Exception.Message
                if ($mismatch -match 'is not in the draft|is already in the draft|must match the draft') {
                    $pauseInfo = [ordered]@{ kind = 'needs_scope_decision'; order = $id; draft = $draftPath; edits_file = $editsPath; mismatch = $mismatch }
                    throw "needs_scope_decision: $id's edits file $editsPath does not match this run's draft ($mismatch); the draft to read is $draftPath"
                }
                throw
            }
            # logged edits: any scope edit, any choice answered or removed by a rule, or any explicit answer in the edits file
            $humanCount = @($scope.logged_edits | Where-Object { $null -ne $_ }).Count + @($scope.answers | Where-Object { $null -ne $_ }).Count + @($scope.edits.answers | Where-Object { $null -ne $_ }).Count
            $confirmed = Invoke-ScopeConfirm -Order $id -Raw $raw -Oid $oid -Scope $scope -MaxEpisodes $MaxEpisodes
            $scopeLabel = if ($humanCount -eq 0) { 'drafted' } elseif ($label -eq $HumanLabel) { 'human_edits' } else { 'delegated_edits' }
            $state.scope = [ordered]@{ decided_by = $label; edits_file = $editsPath; edits_sha256 = (Get-FileSha256 -Path $editsPath); edit_count = $humanCount; label = $scopeLabel; scope_hash = [string]$confirmed.scope_hash; max_episodes = [int]$confirmed.budget.max_episodes }
            Save-OrderState -Dir $dir -State $state
        }
        $line['scope'] = [string]$state.scope.label
        $line['scope_decided_by'] = [string]$state.scope.decided_by
        $line['scope_hash'] = [string]$state.scope.scope_hash

        # worker-r, a model worker, once
        if (-not $state.worker_done) {
            $wr = Start-Worker -Name 'worker-r' -Token $script:Tokens['worker-r'] -Tag ($id -replace '-', '')
            if (-not $wr.exited) { throw "worker_timeout: worker-r did not exit within $WorkerWaitSec s" }
            if ($wr.exit_code -ne 0) { $line['errors'] = @($line['errors']) + "worker-r exited with code $($wr.exit_code)" }
            $wrEvents = @(Get-AgentEvents -Events (Invoke-Api -Path '/api/events') -Agent 'worker-r' | Where-Object { $_.seq -gt $seqStart })
            $wrTypes = @($wrEvents | ForEach-Object { $_.type } | Select-Object -Unique)
            if ($wrTypes -notcontains 'build_reported' -and $wrTypes -notcontains 'landed') {
                throw "worker_no_build: worker-r reported no build (events: $($wrTypes -join ', '))"
            }
            # list_tasks has no order filter, so worker-r takes the first open task on the board. The order's task is
            # the one its own events name, not tasks[0]; the control task is another split task of the order.
            $wrTask = Get-WorkerTask -WorkerEvents $wrEvents
            if (-not $wrTask) { throw 'worker_no_build: worker-r''s events name no task' }
            $state.worker_task = $wrTask
            $state.control_task = Get-ControlTask -Tasks $tasks -WorkerTask $wrTask
            $state.worker_done = $true
            Save-OrderState -Dir $dir -State $state
        }
        $line['worker_task'] = $(if ($state.worker_task) { [string]$state.worker_task } else { $null })
        $line['control_task'] = $(if ($state.control_task) { [string]$state.control_task } else { $null })

        # the gate's receipt for the task the worker built. The order's other tasks have no build in this run;
        # they stay open for the order's controls until Close-ReplayOrder ends them.
        $receiptTasks = @($state.worker_task | Where-Object { $_ })
        for ($i = 0; $i -lt $receiptTasks.Count; $i++) {
            $t = [string]$receiptTasks[$i]
            $body = Wait-Until -What "the receipt of $t" -Probe { Invoke-Api404Tolerant -Path "/api/tasks/$t/receipt" } -Terminal { $null }
            $receipts += [pscustomobject]@{ task = $t; body = $body }
        }

        # out-of-scope differences: the order's rulings file, one ruling per receipt, then the gate again.
        # A missing rule pauses the run with the ruling draft for that episode.
        $rulesPath = Join-Path $intentDir "rulings\$id-rulings.json"
        for ($i = 0; $i -lt $receipts.Count; $i++) {
            $r = $receipts[$i]
            if (@(Get-ReceiptItems -Body $r.body | Where-Object { $_.class -eq 'out_of_scope' }).Count -eq 0) { continue }
            if (@($state.ruled_tasks) -contains $r.task) { continue }
            $rFile = Join-Path $dir "receipt-$($r.task).json"
            Write-JsonFile -Path $rFile -Object ([ordered]@{ receipt = $r.body.receipt })
            $uncovered = Find-UnruledItem -Body $r.body -RulesFile $rulesPath
            if ($null -ne $uncovered) {
                $draftFile = Join-Path $dir ("ruling-draft-" + ($uncovered.episode -replace '[^A-Za-z0-9._-]', '_') + '.json')
                $ruleDraft = Invoke-Api -Path "/api/orders/$oid/episodes/$([uri]::EscapeDataString($uncovered.episode))/ruling-draft"
                Write-JsonFile -Path $draftFile -Object $ruleDraft
                $pauseInfo = [ordered]@{ kind = 'needs_ruling'; order = $id; episode = $uncovered.episode; field = $uncovered.field; candidate_state = $uncovered.state; draft = $draftFile; receipt = $rFile }
                throw "needs_ruling: $id episode $($uncovered.episode) has no rule for $($uncovered.field) with candidate $($uncovered.state); the ruling draft to read is $draftFile"
            }
            $rulesLabel = Get-DecisionLabel -Doc (Read-JsonFile -Path $rulesPath) -Order $id
            $outRl = Join-Path $dir "rulings-$($r.task).json"
            try {
                & (Join-Path $intentDir 'ruling.ps1') -Server $script:Base -OwnerToken $script:OwnerToken -Order $id -OrderId $oid -ReceiptFile $rFile -RulesFile $rulesPath -Out $outRl
            } catch {
                throw "ruling_refused: $($_.Exception.Message)"
            }
            $rl = Get-Content -LiteralPath $outRl -Raw | ConvertFrom-Json
            $rl | Add-Member -NotePropertyName decided_by -NotePropertyValue $rulesLabel -Force
            Write-JsonFile -Path $outRl -Object $rl
            foreach ($x in @($rl.rulings)) { $script:RulingOwners["$($x.ruling_id)"] = $id }
            $state.rulings = [int]$state.rulings + @($rl.rulings).Count
            $state.ruled_tasks = @($state.ruled_tasks | Where-Object { $_ }) + $r.task
            $state.rulings_decided_by = $rulesLabel
            Save-OrderState -Dir $dir -State $state
            $after = Wait-Until -What "the gate on $($r.task) after the rulings" -Probe {
                $b = Invoke-Api404Tolerant -Path "/api/tasks/$($r.task)/receipt"
                if ($b -and @('landed', 'blocked') -contains [string]$b.receipt.gate_state) { $b } else { $null }
            } -Terminal { $null }
            $receipts[$i] = [pscustomobject]@{ task = $r.task; body = $after }
        }
    } catch {
        $msg = $_.Exception.Message
        if ($null -ne $pauseInfo -and $msg -match '^(needs_scope_decision|needs_ruling):') {
            # The order is not finished: no result line, no seeded control. The state file holds the progress.
            $script:Pause = $pauseInfo
            return
        }
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
    $line['rulings'] = [int]$state.rulings
    $line['rulings_decided_by'] = if ([int]$state.rulings -gt 0) { [string]$state.rulings_decided_by } else { $null }
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
        # the order's control task (not the worker's); a one-task order has none, and its seeded line is not_run
        $null = Invoke-SeededCandidate -OrderId $id -Task ([string]$state.control_task) -RunId $script:RunId
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
    # The two controls that apply to o-1, run on o-1's control task: a split task that worker-r did not build, so
    # the controls never claim the task the worker built (take.ps1 runs them on its first task). Once per run.
    if (Test-ControlLine -Id 'm-overfit') { return }
    $o1 = Read-OrderState -Dir (Join-Path $run 'o-1')
    $ctl = if ($o1.control_task) { [string]$o1.control_task } else { $null }
    if (-not $ctl) {
        foreach ($c in @('m-overfit', 'm-harmless-difference')) {
            Add-ResultLine -Record (New-ControlLine -Id $c -AppliesTo 'o-1' -Expected ([string](@($controlList | Where-Object { $_.id -eq $c })[0].expected_outcome)) -Outcome 'not_run' -Result $null -Reason 'o-1 has no task other than the worker''s task in this run')
        }
        return
    }
    $expectedOf = @{}
    foreach ($c in $controlList) { $expectedOf[[string]$c.id] = [string]$c.expected_outcome }

    # m-overfit: the control must fail s12 (bob keeps his own quota) while the gate is blocked
    $mo = Join-Path $script:CtrlDir 'm-overfit.json'
    # take.ps1 stops o-1 unless the gate blocked m-overfit with an obligation_failed item (take.ps1:850).
    $moBlocked = $false; $moGate = ''; $moObl = 0; $moErr = ''
    $watch = [Diagnostics.Stopwatch]::StartNew()
    try {
        # As take.ps1 (its step 1): the task must be open for the claim, and the first control on the order
        # claims and declares again once if its claim ends before the build is cleared (-RedeclareOnce).
        $null = Wait-TaskClaimable -Task $ctl -Token $script:Tokens['control-seeded'] -Seconds $ClaimWaitSec
        & (Join-Path $intentDir 'control.ps1') -Server $script:Base -Token $script:Tokens['control-seeded'] -Repo $script:Repo -Task $ctl -Bundle (Join-Path $intentDir 'controls\m-overfit.json') -Out $mo -StageWaitSec 300 -ReceiptWaitSec $ControlWaitSec -RedeclareOnce
        $r = Read-ControlResult -Path $mo
        $moGate = [string]$r.gate_state
        $moObl = @($r.items | Where-Object { $_.class -eq 'obligation_failed' }).Count
        $moBlocked = ($moGate -eq 'blocked' -and $moObl -ge 1)
        $s12 = @($r.items | Where-Object { $_.class -eq 'obligation_failed' -and ((ConvertTo-Json -Depth 20 -Compress $_) -match '"s12"') })
        $observed = if ([string]$r.gate_state -eq 'blocked' -and $s12.Count -ge 1) { 'as_expected' } else { 'not_as_expected' }
        $line = New-ControlLine -Id 'm-overfit' -AppliesTo 'o-1' -Expected $expectedOf['m-overfit'] -Outcome $observed -Result $r -Reason ''
        $line['s12_failed'] = $s12.Count
    } catch {
        $moErr = [string]$_.Exception.Message
        $line = New-ControlLine -Id 'm-overfit' -AppliesTo 'o-1' -Expected $expectedOf['m-overfit'] -Outcome 'error' -Result $null -Reason ''
        $line['errors'] = @($_.Exception.Message)
    }
    $watch.Stop()
    $line['wall_seconds'] = [math]::Round($watch.Elapsed.TotalSeconds, 1)
    Add-ResultLine -Record $line

    # As take.ps1 (take.ps1:850), o-1 stops when m-overfit was not blocked with an obligation_failed item, so
    # m-harmless-difference is not run. Its line is written as not_run with the reason, and the run goes on to
    # the next order.
    if (-not $moBlocked) {
        $why = if ($moErr) { "m-overfit ended in an error ($moErr)" } else { "m-overfit was not blocked with an obligation_failed item (gate_state '$moGate', obligation_failed $moObl)" }
        $skip = New-ControlLine -Id 'm-harmless-difference' -AppliesTo 'o-1' -Expected $expectedOf['m-harmless-difference'] -Outcome 'not_run' -Result $null -Reason "o-1 stopped before m-harmless-difference: $why. take.ps1 stops o-1 here too (take.ps1:850)."
        Add-ResultLine -Record $skip
        return
    }

    # m-harmless-difference: out of scope at first, not a regression once the owner's rules accept it
    $mh = Join-Path $script:CtrlDir 'm-harmless-difference.json'
    $watch = [Diagnostics.Stopwatch]::StartNew()
    try {
        # The task must be open for the m-harmless claim (take.ps1's step 2). The rulings on m-overfit's receipt
        # come first, as take.ps1 does (D02Q); then the re-gate and the land.
        $null = Wait-TaskClaimable -Task $ctl -Token $script:Tokens['control-seeded'] -Seconds $ClaimWaitSec
        $ruled = Invoke-ControlWithRuling -Task $ctl -Bundle (Join-Path $intentDir 'controls\m-harmless-difference.json') -Out $mh -Order 'o-1' -OrderId $script:OidByOrder['o-1'] -RulesFile (Join-Path $intentDir 'rulings\o-1-rulings.json') -PriorReceipt "$mo.receipt.json" -PriorRulingsOut (Join-Path $script:CtrlDir 'rulings-o-1-moverfit.json') -FinalReceipt (Join-Path $script:CtrlDir 'm-harmless-final-receipt.json') -ControlWaitSec $ControlWaitSec -RulingWaitSec $RulingWaitSec
        if ($ruled.no_receipt) { throw 'error: the m-harmless control produced no receipt to rule on' }
        if ($ruled.ruling_error) { throw "ruling_refused: $($ruled.ruling_error)" }
        foreach ($rid in @($ruled.ruling_ids)) { $script:RulingOwners["$rid"] = 'control:m-harmless-difference' }
        $r = Read-ControlResult -Path $mh
        $xr = @($r.items | Where-Object { $_.class -eq 'out_of_scope' -and ((ConvertTo-Json -Depth 20 -Compress $_) -match 'x-request-id') })
        $observed = if ([string]$r.gate_state -eq 'landed' -and $xr.Count -ge 1) { 'as_expected' } else { 'not_as_expected' }
        $line = New-ControlLine -Id 'm-harmless-difference' -AppliesTo 'o-1' -Expected $expectedOf['m-harmless-difference'] -Outcome $observed -Result $r -Reason ''
        $line['x_request_id_out_of_scope'] = $xr.Count
        $line['ruling_ids'] = @($ruled.ruling_ids)
        $line['ruling_decided_by'] = Get-DecisionLabel -Doc ([pscustomobject]@{ decided_by = [string]$ruled.rulings_decided_by }) -Order 'o-1'
        if ([string]$r.gate_state -eq 'landed') { $line['merged_sha'] = Get-LandedCommit -Task $ctl }
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

function Invoke-BroadScopeControl {
    # m-broad-scope: a scope draft whose may-change items are method ANY on each link route it names (/api/links,
    # /api/links/:code; mutants.json says /api/*, which the route grammar refuses). The control is
    # posted as its own order with o-1's sentence, so o-1's run is not touched. Its draft is revised with
    # the broad item, then confirmation is tried with no broad acknowledgement. The expected outcome is a
    # refusal that names the item, no scope_confirmed event, and no claim or build on the control's tasks.
    # The control uses no agent token and starts no worker. Its revision and confirm use the owner's
    # member token, as the take's own scope steps do.
    $dir = Join-Path $script:CtrlDir 'm-broad-scope'
    New-Item -ItemType Directory -Path $dir -Force | Out-Null
    $line = New-ControlLine -Id 'm-broad-scope' -AppliesTo 'o-1' -Expected (Get-ControlExpectation -Id 'm-broad-scope') -Outcome 'error' -Result $null -Reason 'a separate order with the o-1 sentence; o-1 itself is not changed'
    $line['actor'] = 'control identity (m-broad-scope), sent with the owner member token; not a model and not a human decision'
    $line['worker_started'] = $false
    $controlOrder = 'control-m-broad-scope'
    $watch = [Diagnostics.Stopwatch]::StartNew()
    try {
        $o1 = @($orderList | Where-Object { [string]$_.id -eq 'o-1' })[0]
        $posted = Send-Command @{ cmd = 'post_order'; machine = $o1.machine; sentence = $o1.sentence; witnesses = @() }
        $postedEvent = $posted | Where-Object { $_.event.type -eq 'order_posted' } | Select-Object -First 1
        if (-not $postedEvent) { throw 'setup_failed: post_order returned no order_posted event' }
        $raw = "$($postedEvent.event.order)"
        $oid = if ($raw -like 'o-*') { $raw } else { "o-$raw" }
        $line['control_order'] = $raw
        $line['control_oid'] = $oid
        $null = Wait-Until -What 'the split of the control order' -Probe {
            Invoke-Api -Path '/api/events' | Where-Object { $_.event.type -eq 'order_split' -and "$($_.event.order)" -eq $raw } | Select-Object -First 1
        } -Terminal { $null }
        $tasks = @(Get-OrderTasks -Events (Invoke-Api -Path '/api/events') -Raw $raw)
        if ($tasks.Count -eq 0) { throw 'setup_failed: the split of the control order made no task' }
        $line['tasks'] = $tasks
        $view = Wait-Until -What 'the foreman draft of the control order' -Probe { Invoke-Api404Tolerant -Path "/api/orders/$oid/scope-draft" } -Terminal { $null }
        if ($view.state -ne 'drafted') { throw "draft_not_ready: the draft state is $($view.state)" }

        # The o-1 choice rules answer the same open choices. The broad items are copies of the draft's own link
        # items with method ANY, one per route template under the link API that the draft names. A route template
        # has no wildcard, so the mutant's /api/* is not a valid template (crates/acan-intent/src/types.rs:332).
        # Each copy keeps the draft item's selector, acceptance and example, with new ids, so it is a complete item.
        $o1Edits = Read-JsonFile -Path (Join-Path $intentDir 'o-1-edits.json')
        $editsFile = Join-Path $dir 'edits.json'
        $linkItems = @($view.draft.draft.proposal.may | Where-Object { [string]$_.selector.route_template -like '/api/links*' })
        if ($linkItems.Count -eq 0) { throw 'setup_failed: the control draft has no may item on the link API (/api/links*)' }
        $seenRoutes = @{}
        $broadEdits = [System.Collections.Generic.List[object]]::new()
        foreach ($item in $linkItems) {
            $route = [string]$item.selector.route_template
            if ($seenRoutes.ContainsKey($route)) { continue }
            $seenRoutes[$route] = $true
            $n = $seenRoutes.Count
            $copy = ConvertFrom-Json (ConvertTo-Json -Depth 60 -Compress $item)
            $copy.id = "i-broad-control-$n"
            $copy.selector.method = 'ANY'
            $k = 0
            foreach ($a in @($copy.acceptance)) { $k++; $a.obligation_id = "obl-broad-control-$n-$k" }
            $broadEdits.Add([ordered]@{
                    op     = 'add_may'
                    reason = "The broad selector of m-broad-scope: method ANY on $route, a copy of the draft's item $($item.id)."
                    item   = $copy
                })
        }
        $controlEdits = [ordered]@{
            schema       = 'acan-demo-intent-edits/2'
            order        = $controlOrder
            decided_by   = 'control (m-broad-scope), not a decision'
            note         = 'Control edits for m-broad-scope: the o-1 choice rules and broad may-change items (method ANY on each link route the draft names). Nothing here is a human decision.'
            choice_rules = @($o1Edits.choice_rules)
            scope_edits  = @($broadEdits)
            answers      = @()
        }
        Write-JsonFile -Path $editsFile -Object $controlEdits
        $line['broad_items'] = @($broadEdits | ForEach-Object { [string]$_.item.id })
        $line['broad_routes'] = @($seenRoutes.Keys | Sort-Object)

        $scope = $null
        try {
            $scope = Invoke-ScopeEdits -Order $controlOrder -Raw $raw -Oid $oid -View $view -EditsFile $editsFile -OrderDir $dir -DecidedBy 'control (m-broad-scope), not a decision'
        } catch {
            $line['errors'] = @($line['errors']) + "the revision with the broad item was refused: $($_.Exception.Message)"
            $line['outcome'] = 'not_as_expected'
            $line['reason'] = 'the origin refused the revision, so the confirmation step was not reached'
        }
        if ($null -ne $scope) {
            $refusal = $null
            try {
                $null = Invoke-ScopeConfirm -Order $controlOrder -Raw $raw -Oid $oid -Scope $scope -MaxEpisodes $MaxEpisodes
            } catch {
                $refusal = $_.Exception.Message
            }
            $events = @(Invoke-Api -Path '/api/events')
            $confirmedEvents = @($events | Where-Object { $_.event.type -eq 'scope_confirmed' -and "$($_.event.order)" -eq $raw }).Count
            $buildEvents = @($events | Where-Object { $tasks -contains "$($_.event.task)" -and "$($_.event.type)" -match 'claim|build|land' }).Count
            $confirmation = Invoke-Api404Tolerant -Path "/api/orders/$oid/scope/confirmation"
            $line['confirmation_names_broad_item'] = ((ConvertTo-Json -Depth 30 -Compress $confirmation) -match 'i-broad-control')
            $line['refusal'] = $refusal
            $line['scope_confirmed_events'] = $confirmedEvents
            $line['build_events'] = $buildEvents
            $named = [bool]$refusal -and ($refusal -match 'needs its own acknowledgement')
            if ($named -and $confirmedEvents -eq 0 -and $buildEvents -eq 0) {
                $line['outcome'] = 'as_expected'
                $line['reason'] = 'confirmation refused: the broad item needs its own acknowledgement; no scope_confirmed and no claim or build'
            } else {
                $line['outcome'] = 'not_as_expected'
                $line['reason'] = 'the refusal, the scope events or the build events differ from the expected outcome; see refusal, scope_confirmed_events and build_events'
            }
        }
    } catch {
        $line['errors'] = @($line['errors']) + $_.Exception.Message
        $line['outcome'] = 'error'
        $line['reason'] = 'the control stopped before the confirmation step'
    }
    $watch.Stop()
    $line['wall_seconds'] = [math]::Round($watch.Elapsed.TotalSeconds, 1)
    $line['finished'] = (Get-Date).ToString('o')
    Add-ResultLine -Record $line
}

function Get-MissingFixturePrediction {
    # What the gate should find on the copied recipe, read from the recipe alone. A login as bob has no
    # account when no step signs bob up, and a capture whose source step is absent or failed has no value.
    param($Recipe)
    $steps = @($Recipe)
    $ids = @($steps | ForEach-Object { [string]$_.id })
    $bobSignups = @($steps | Where-Object { $_.step -eq 'http' -and "$($_.request.path)" -eq '/api/signup' -and "$($_.request.body.username)" -eq 'bob' })
    $failed = [System.Collections.Generic.HashSet[string]]::new()
    if ($bobSignups.Count -eq 0) {
        foreach ($s in $steps) {
            if ($s.step -eq 'http' -and "$($s.request.path)" -eq '/api/login' -and "$($s.request.body.username)" -eq 'bob') { $null = $failed.Add([string]$s.id) }
        }
    }
    $changed = $true
    while ($changed) {
        $changed = $false
        foreach ($s in $steps) {
            if ($s.step -ne 'capture' -or $failed.Contains([string]$s.id)) { continue }
            $source = [string]$s.from
            if (($ids -notcontains $source) -or $failed.Contains($source)) {
                $null = $failed.Add([string]$s.id)
                $changed = $true
            }
        }
    }
    [ordered]@{
        bob_signups_left            = $bobSignups.Count
        steps_left                  = $ids.Count
        predicted_failed_or_missing = @($failed | Sort-Object)
        basis                       = 'the recipe alone: a login as bob has no account when no step signs bob up; a capture whose source step is absent or failed has no value'
    }
}

function Invoke-MissingFixtureControl {
    # m-missing-fixture: the f-accounts recipe at the pin with step f5 (bob's signup) removed, written as a
    # copy into the run with its static prediction. The gate is not run on the copy: the probe corpus is
    # read from the protected snapshot of a run repository, and take.ps1 starts no run whose protected
    # head carries the copy. The control is reported not_run with that reason. It is never a pass.
    # The product gap: the gate reads the corpus only from the protected-head snapshot (observe.rs:54 names
    # the file, observe.rs:284-296 writes the snapshot from the protected head, observe.rs:305-308 reads it),
    # so a copy in a run repository cannot be gated without a product change.
    # The reason every line of this control states, with the product gap at file:line.
    $gapReason = 'not_run: the gate reads the probe corpus only from the protected-head snapshot, so a copy in a run repository cannot be gated without a product change (crates/acan-server/src/observe.rs:54 names probes/core.json; observe.rs:284-296 writes the snapshot from the protected head; observe.rs:305-308 reads it).'
    $dir = Join-Path $script:CtrlDir 'm-missing-fixture'
    New-Item -ItemType Directory -Path $dir -Force | Out-Null
    $line = New-ControlLine -Id 'm-missing-fixture' -AppliesTo 'f-accounts' -Expected (Get-ControlExpectation -Id 'm-missing-fixture') -Outcome 'not_run' -Result $null -Reason $gapReason
    try {
        $pinnedLines = & git -C $DemoRepo show "${Pin}:.acan/probes/core.json"
        if ($LASTEXITCODE -ne 0) { throw "git show ${Pin}:.acan/probes/core.json failed" }
        $corpus = ($pinnedLines -join "`n") | ConvertFrom-Json -Depth 60
        $fixtures = @($corpus.fixtures | Where-Object { $_.id -eq 'f-accounts' })
        if ($fixtures.Count -ne 1) { throw 'the pinned probe corpus has no single f-accounts fixture' }
        $recipe = @($fixtures[0].recipe)
        if (@($recipe | Where-Object { $_.id -eq 'f5' }).Count -ne 1) { throw 'the f-accounts recipe has no step f5 to omit' }
        $fixtures[0].recipe = @($recipe | Where-Object { $_.id -ne 'f5' })
        $copy = Join-Path $dir 'core.json'
        Set-Content -LiteralPath $copy -Value (ConvertTo-Json -Depth 60 $corpus) -Encoding utf8
        $line['copy'] = $copy
        $line['copy_sha256'] = Get-FileSha256 -Path $copy
        $line['omitted_steps'] = @('f5')
        $line['static_prediction'] = Get-MissingFixturePrediction -Recipe $fixtures[0].recipe
        $line['reason'] = "$gapReason The copy and its static prediction are written; nothing here is a pass."
    } catch {
        $line['errors'] = @($_.Exception.Message)
        $line['reason'] = "$gapReason The copy could not be written, so there is no static prediction either."
    }
    Add-ResultLine -Record $line
}

if (-not $DryRun) {
    # ---- take.ps1, as a library ---------------------------------------------------------------
    if (-not (Test-Path -LiteralPath $TakeScript)) { throw "missing input: $TakeScript" }
    if (-not (Select-String -LiteralPath $TakeScript -Pattern '\[switch\]\$LibraryOnly' -Quiet)) {
        throw 'take.ps1 has no -LibraryOnly guard; a live replay refuses to dot-source the take as a script'
    }
    # Dot-sourcing runs take.ps1's param block in this scope, so every take parameter that is not passed takes
    # its default here. -ResumeRun was not passed, so the take's '' replaced this script's value: every
    # -ResumeRun invocation then started a new run. It is passed now, and the value is checked after the load.
    $replayResumeRun = $ResumeRun
    . $TakeScript -LibraryOnly -Label $Label -Port $Port -RunsRoot $RunsRoot -DemoRepo $DemoRepo -Pin $Pin -AcanRepo $AcanRepo -AcanExe $AcanExe -WorkerExe $WorkerExe -LlmSource $LlmSource -EnvSource $EnvSource -MaxEpisodes $MaxEpisodes -HealthWaitSec $HealthWaitSec -StepWaitSec $StepWaitSec -WorkerWaitSec $WorkerWaitSec -ControlWaitSec $ControlWaitSec -RulingWaitSec $RulingWaitSec -WorkerMaxSteps $WorkerMaxSteps -ClaimWaitSec $ClaimWaitSec -ResumeRun $ResumeRun
    if ($ResumeRun -ne $replayResumeRun) { throw 'dot-sourcing take.ps1 changed -ResumeRun; the replay would not resume its run' }
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
    # The injector needs no file per order. Its anchor must sit once in the pinned src/server.ts; the bundles
    # of the drawn orders are generated after the draw and before any clone or candidate.
    $script:AnchorSha = Get-InjectorAnchorSha
    # A decision file is optional until the run needs it. A file that is there must be well formed.
    foreach ($o in $selected) {
        $editsPath = Join-Path $intentDir "$($o.id)-edits.json"
        if (Test-Path -LiteralPath $editsPath) {
            $ed = Read-JsonFile -Path $editsPath
            if ($ed.schema -ne 'acan-demo-intent-edits/2') { throw "$editsPath must be acan-demo-intent-edits/2" }
            if ($ed.order -and [string]$ed.order -ne [string]$o.id) { throw "$editsPath is for $($ed.order), not $($o.id)" }
            $null = Get-DecisionLabel -Doc $ed -Order ([string]$o.id)
        }
        $rulesPath = Join-Path $intentDir "rulings\$($o.id)-rulings.json"
        if (Test-Path -LiteralPath $rulesPath) {
            $rl = Read-JsonFile -Path $rulesPath
            if ($rl.schema -ne 'acan-demo-intent-rulings/1') { throw "$rulesPath must be acan-demo-intent-rulings/1" }
            if ([string]$rl.order -ne [string]$o.id) { throw "$rulesPath is for $($rl.order), not $($o.id)" }
            $null = Get-DecisionLabel -Doc $rl -Order ([string]$o.id)
        }
    }
    $script:Routes = Read-Routing -Text (Get-Content -LiteralPath $LlmSource -Raw)
    $preflight = Test-LiveRoutes

    if ($ResumeRun) {
        # ---- a paused run: its folder, its draw, its clone and tokens, its results so far -----
        $run = (Resolve-Path -LiteralPath $ResumeRun).Path
        $script:Summary = Get-Content -LiteralPath (Join-Path $run 'summary.json') -Raw | ConvertFrom-Json -AsHashtable
        if ([string]$script:Summary['status'] -ne 'paused') { throw "resume needs a paused run; $run is '$($script:Summary['status'])'" }
        if ($PSBoundParameters.ContainsKey('Port') -and $Port -ne [int]$script:Summary['port']) {
            throw "the run uses port $($script:Summary['port']), not $Port"
        }
        $Port = [int]$script:Summary['port']
        $script:RunId = [string]$script:Summary['run_id']
        $script:CtrlDir = Join-Path $run 'controls'
        $script:ResultsPath = Join-Path $run 'results.jsonl'
        $script:Repo = Join-Path $run 'repo'
        $script:LlmPath = Join-Path $run 'config\llm.toml'
        $script:EnvPath = Join-Path $run 'keys\.env'
        $drawPath = Join-Path $run 'draw.json'
        if ((Get-FileSha256 -Path $drawPath) -ne [string]$script:Summary['draw_sha256']) {
            throw 'draw.json no longer matches the hash recorded before the first candidate; the run cannot resume'
        }
        $drawRec = Read-JsonFile -Path $drawPath
        $drawn = @($drawRec.drawn)
        foreach ($id in $drawn) {
            $bundlePath = Join-Path $script:CtrlDir "injected-$id.json"
            $sha = Get-FileSha256 -Path $bundlePath
            if ($sha -ne [string]$drawRec.injected_bundles.$id) { throw "the injected bundle for $id no longer matches draw.json; the run cannot resume" }
            $script:InjectedHashes[$id] = $sha
        }
        Assert-OwnRepo
        $head = (Invoke-Git rev-parse HEAD | Out-String).Trim()
        if ($head -ne $Pin) { throw "the resumed clone's HEAD is $head, not the pin $Pin" }
        $keys = Read-KeyFile -Path $script:EnvPath
        foreach ($value in $keys.Values) { if ($value.Length -ge 8) { $script:Secrets.Add($value) } }
        $existingTokens = Get-Content -LiteralPath (Join-Path $script:Repo '.acan\tokens.json') -Raw | ConvertFrom-Json
        $script:Tokens = @{
            owner            = [string](@($existingTokens | Where-Object { $_.principal.kind -eq 'member' })[0].token)
            'worker-r'       = [string](@($existingTokens | Where-Object { $_.principal.id -eq 'worker-r' })[0].token)
            'control-seeded' = [string](@($existingTokens | Where-Object { $_.principal.id -eq 'control-seeded' })[0].token)
        }
        foreach ($tok in $script:Tokens.Values) { $script:Secrets.Add([string]$tok) }
        $script:OwnerToken = $script:Tokens.owner
        $script:Base = "http://127.0.0.1:$Port"
        foreach ($l in @(Get-Content -LiteralPath $script:ResultsPath -ErrorAction SilentlyContinue)) {
            if ($l.Trim()) { $script:ResultRecords.Add(($l | ConvertFrom-Json -AsHashtable)) }
        }
        foreach ($rec in @($script:ResultRecords | Where-Object { $_.kind -eq 'order' })) { $script:Outcomes[[string]$rec.id] = [string]$rec.outcome }
        foreach ($ord in $selected) {
            $od = Join-Path $run ([string]$ord.id)
            $st = Read-OrderState -Dir $od
            if ($st.oid) { $script:OidByOrder[[string]$ord.id] = [string]$st.oid }
            if ($st.tasks) { $script:TasksByOrder[[string]$ord.id] = @($st.tasks) }
            foreach ($t in @($st.ruled_tasks | Where-Object { $_ })) {
                $rulingFile = Join-Path $od "rulings-$t.json"
                if (Test-Path -LiteralPath $rulingFile) {
                    foreach ($x in @((Read-JsonFile -Path $rulingFile).rulings)) { $script:RulingOwners["$($x.ruling_id)"] = [string]$ord.id }
                }
            }
        }
        $script:Summary['resumes'] = @($script:Summary['resumes']) + (Get-Date).ToString('o')
        $script:Summary['pause'] = $null
    } else {
        # ---- run folder; the draw and the seeded bundles are written before any clone or candidate
        $script:RunId = "$Label-$(Get-Date -Format 'yyyyMMdd-HHmmss')"
        $run = Join-Path $RunsRoot $script:RunId
        $script:CtrlDir = Join-Path $run 'controls'
        $script:ResultsPath = Join-Path $run 'results.jsonl'
        $script:Summary = [ordered]@{
            schema = $SummarySchema; run_id = $script:RunId; label = $Label; status = 'starting'
            partial = ($Only.Count -gt 0); only = @($Only); orders_planned = @($selected | ForEach-Object { [string]$_.id })
            pin = $Pin; port = $Port; started = (Get-Date).ToString('o'); preflight = $preflight
            draw_sha256 = $null; demo_refs_before = $null; demo_refs_after = $null; failure = $null; pause = $null
            incomplete_reasons = @(); secret_scan = 'not_run'
        }
        $null = New-Item -ItemType Directory -Path $run, $script:CtrlDir, (Join-Path $run 'logs'), (Join-Path $run 'keys'), (Join-Path $run 'config') -Force
    }

    try {
        if (-not $ResumeRun) {
            $seed = [string][System.Security.Cryptography.RandomNumberGenerator]::GetInt32(1, [int]::MaxValue)
            $drawn = @(Get-DrawnOrders -Seed $seed -Ids $orderIds -Count $DrawCount)
            # one generated bundle per drawn order, written into the run folder before the draw record names it
            $injectedRecord = [ordered]@{}
            foreach ($id in $drawn) {
                $bundlePath = Join-Path $script:CtrlDir "injected-$id.json"
                Write-JsonFile -Path $bundlePath -Object (New-InjectedBundle -OrderId $id)
                $script:InjectedHashes[$id] = Get-FileSha256 -Path $bundlePath
                $injectedRecord[$id] = $script:InjectedHashes[$id]
            }
            $drawRecord = [ordered]@{
                schema = $DrawSchema; run_id = $script:RunId; drawn_at = (Get-Date).ToString('o')
                algorithm = 'the orders with the smallest SHA-256 of "<seed>|<order id>", first 4, in orders.json order'
                seed = $seed; pool = $orderIds; drawn = $drawn
                injector = [ordered]@{ id = $script:InjectorId; route = $script:InjectorRoute; header = $script:InjectorHeader; file = $script:InjectorFile; anchor_sha256 = $script:AnchorSha; placement = 'the task worktree HEAD of the order''s control task (not the worker''s task), through control.ps1 -OnWorkerCandidate' }
                injected_bundles = $injectedRecord
                orders_json_sha256 = Get-FileSha256 -Path (Join-Path $intentDir 'orders.json')
                mutants_json_sha256 = Get-FileSha256 -Path (Join-Path $intentDir 'mutants.json')
            }
            $drawPath = Join-Path $run 'draw.json'
            Write-JsonFile -Path $drawPath -Object $drawRecord
            $script:Summary['draw_sha256'] = Get-FileSha256 -Path $drawPath
            $script:Summary['status'] = 'drawn'
            Write-JsonFile -Path (Join-Path $run 'summary.json') -Object $script:Summary

            # ---- keys, routing, tokens; the clone -------------------------------------------------
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
        }

        # ---- the server and the agents ----------------------------------------------------------
        $logTag = if ($ResumeRun) { 'resume-' + (Get-Date -Format 'yyyyMMdd-HHmmss') } else { 'replay' }
        Start-TakeServer -Run $run -Repo $script:Repo -Port $Port -AcanExe $AcanExe -LlmPath $script:LlmPath -EnvPath $script:EnvPath -LogTag $logTag -HealthWaitSec $HealthWaitSec | Out-Null
        $existing = @(Invoke-Api -Path '/api/events')
        foreach ($name in @('worker-r', 'control-seeded')) {
            $already = @($existing | Where-Object { $_.event.type -eq 'agent_registered' -and (ConvertTo-Json -Depth 8 -Compress $_) -like "*`"$name`"*" }).Count
            if ($already -eq 0) { $null = Send-Command @{ cmd = 'register_agent'; agent = $name } }
        }

        # ---- the orders, one attempt each, in orders.json order; a pause ends the invocation ----
        $script:Summary['status'] = 'running'
        Write-JsonFile -Path (Join-Path $run 'summary.json') -Object $script:Summary
        foreach ($ord in $selected) {
            $ordId = [string]$ord.id
            $finished = @($script:ResultRecords | Where-Object { $_.kind -eq 'order' -and [string]$_.id -eq $ordId }).Count -gt 0
            if (-not $finished) {
                Invoke-ReplayOrder -Ord $ord -Drawn ($drawn -contains $ordId)
                if ($script:Pause) { break }
            }
            if ($ordId -eq 'o-1') { Invoke-O1Controls }
            # after the order and its controls: its tasks that did not land end here, so no later worker takes them
            Close-ReplayOrder -Ord $ord
        }
        if (-not $script:Pause) {
            if (-not ($selected | Where-Object { [string]$_.id -eq 'o-1' }) -and -not (Test-ControlLine -Id 'm-overfit')) {
                foreach ($c in @('m-overfit', 'm-harmless-difference')) {
                    Add-ResultLine -Record (New-ControlLine -Id $c -AppliesTo 'o-1' -Expected (Get-ControlExpectation -Id $c) -Outcome 'not_run' -Result $null -Reason 'o-1 is not in this run')
                }
            }
            foreach ($id in $drawn) {
                if (-not ($selected | Where-Object { [string]$_.id -eq $id }) -and -not (Test-SeededLine -Id $id)) {
                    Add-ResultLine -Record (New-SeededNotRun -OrderId $id -RunId $script:RunId -Reason 'the drawn order is not in this rehearsal (-Only); its injected change was not run')
                }
            }
            if (-not (Test-ControlLine -Id 'm-broad-scope')) { Invoke-BroadScopeControl }
            if (-not (Test-ControlLine -Id 'm-missing-fixture')) { Invoke-MissingFixtureControl }
        }
    } catch {
        $script:Summary['failure'] = $_.Exception.Message
        # where it stopped: the line and the call stack, so a harness failure can be found without a rerun
        $script:Summary['failure_at'] = [string]$_.InvocationInfo.ScriptLineNumber + ': ' + ([string]$_.InvocationInfo.Line).Trim()
        $script:Summary['failure_stack'] = [string]$_.ScriptStackTrace
        $script:ExitCode = 1
        Write-Output "FAILED: $($_.Exception.Message)"
        Write-Output "failed at line $($script:Summary['failure_at'])"
        Write-Output $_.ScriptStackTrace
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
        $script:Summary['decisions'] = [ordered]@{
            scope_human = @($orderLines | Where-Object { $_.scope_decided_by -eq $HumanLabel }).Count
            scope_delegated = @($orderLines | Where-Object { $_.scope_decided_by -eq $DelegatedLabel }).Count
            rulings_human = @($orderLines | Where-Object { $_.rulings_decided_by -eq $HumanLabel }).Count
            rulings_delegated = @($orderLines | Where-Object { $_.rulings_decided_by -eq $DelegatedLabel }).Count
            note = 'one count per order that made the decision. The controls use the rulings file of o-1, and each line names that file''s decided_by.'
        }
        if ($script:Pause) {
            $script:Summary['status'] = 'paused'
            $script:Summary['pause'] = $script:Pause
            $script:Summary['incomplete_reasons'] = @("paused: $($script:Pause.kind) for $($script:Pause.order); resume with -ResumeRun after the decision file is written")
        } else {
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
        }
        $script:Summary['finished'] = (Get-Date).ToString('o')
        $script:Summary['orders'] = @($orderLines | ForEach-Object { [ordered]@{ id = $_.id; outcome = $_.outcome; scope = $_.scope; scope_decided_by = $_.scope_decided_by; rulings_decided_by = $_.rulings_decided_by; drawn_seeded = $_.drawn_seeded; gate_state = $_.gate_state; wall_seconds = $_.wall_seconds; model_calls = $_.model_calls } })
        $script:Summary['controls'] = @($controlLines + $seededLines | ForEach-Object { [ordered]@{ id = $_.id; outcome = $_.outcome } })
        # each order's leftover tasks: the ones that did not land and were ended by cancel_order after the order
        if ($run) {
            $script:Summary['leftovers'] = @($selected | ForEach-Object {
                    $st = Read-OrderState -Dir (Join-Path $run ([string]$_.id))
                    if ($st.leftovers_closed) { [ordered]@{ order = [string]$_.id; dropped = @($st.leftovers.dropped); reply = @($st.leftovers.reply) } }
                } | Where-Object { $_ })
        }
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
        if ($script:Pause) {
            if ($script:Pause.kind -eq 'needs_scope_decision') {
                Write-Output "needs_scope_decision $($script:Pause.order) $($script:Pause.draft)"
            } else {
                Write-Output "needs_ruling $($script:Pause.order) $($script:Pause.episode) $($script:Pause.draft)"
            }
        }
        if ($scanHits.Count -gt 0) {
            Write-Output "secret scan found a token or key in: $($scanHits -join ', ')"
            $script:ExitCode = 2
        } elseif ($script:Pause) {
            $script:ExitCode = 3
        }
    }
    if ($script:ExitCode -ne 0) { exit $script:ExitCode }
}
