<#
.SYNOPSIS
One intent take: the real Acan server on a clean, disposable clone of the acan-demo repository,
driven only through the member HTTP API, with live Meta models for the foreman and the workers.

.DESCRIPTION
Every run gets its own folder under -RunsRoot. The clone is the only git repository the script
runs git in. Its top level is checked after the clone and before every git call, so a clone
that failed can never send git up to a parent repository.

Keys are copied from crates/acan-llm/.env into the run folder and handed to the server and the
workers through their environment or their --env file only. They are never printed, never written
to summary.json, and never put in the clone. The member and agent tokens live in the clone's ignored
.acan/tokens.json.

Phases, one order at a time:
  1. New run with -DraftOnly -Order o-1: clone, server, post the order, wait for the foreman's draft,
     stop. A person writes the edits for that draft into the order's edits file.
  2. -ResumeRun <run> -Order o-1 -EditsFile <edits>: apply the edits as one revise_scope, confirm
     the scope (the owner raises the repair budget's max_episodes, logged), register the agents,
     then: the seeded regression m-overfit (control, blocked), then the seeded harmless difference
     m-harmless-difference (control): its out-of-scope episodes are ruled through member HTTP, it
     re-gates, and the passing gate lands o-1 on its own. Then the land check. No model worker
     runs in this phase.
  3. -ResumeRun <run> -Order o-8 -DraftOnly, then the same with -EditsFile: worker B (model) builds
     and lands the order, then the seeded ruling violation m-ruling-violation (control).

Each human step is a member HTTP call and is logged in summary.json. Each control is a seeded
bundle run through the MCP tools with its own agent token, never a model. A model call that fails
stops the take and is logged.

Steps that are not run are written as not_run with the reason. They are never reported as passed.
summary.json is written even when the take fails.
#>
[CmdletBinding()]
param(
    [string]$Label = 'take',
    [ValidateSet('o-1', 'o-8')]
    [string]$Order = 'o-1',
    [int]$Port = 7411,
    [string]$RunsRoot = 'D:\dev\acan-takes',
    [string]$DemoRepo = 'D:\dev\acan-demo',
    [string]$Pin = 'f91962156841332d5753d869554839f7c4cdea79',
    # The Acan checkout the take runs from. Empty means the repository that holds this script.
    [string]$AcanRepo = '',
    # Required: the server and worker binaries built for this checkout, e.g. <CARGO_TARGET_DIR>\debug\acan.exe.
    [Parameter(Mandatory)][string]$AcanExe,
    [Parameter(Mandatory)][string]$WorkerExe,
    # Required: the local LLM config and its .env with the keys. Neither is in git, so neither has a default.
    [Parameter(Mandatory)][string]$LlmSource,
    [Parameter(Mandatory)][string]$EnvSource,
    [string]$EditsFile = '',
    [string]$RulesFile = '',
    # The repair budget's cap on new episodes, set at the order's first confirmation. The offered
    # cap is four, and every differing field of a route is one episode, so the seeded differences
    # need more room. The value is a human decision and is logged as one.
    [int]$MaxEpisodes = 40,
    # The repair budget's rounds for the order, set at its first confirmation: 0 to 8 (the type's bound).
    # 0 keeps the offered value. The value is a human decision and is logged as one.
    [ValidateRange(0, 8)]
    [int]$RepairRounds = 0,
    # The most model calls the worker's edit loop may make per build. Set by a human decision, logged in the summary.
    # 200 since D02M: the D02T run used all 120 on its fourth build without a commit.
    [int]$WorkerMaxSteps = 200,
    # A JSON file of the coordinator's decisions, written into summary.json as coordinator_decisions.
    [string]$DecisionsFile = '',
    [int]$HealthWaitSec = 90,
    [int]$StepWaitSec = 900,
    [int]$WorkerWaitSec = 2700,
    [int]$ControlWaitSec = 1500,
    [int]$RulingWaitSec = 1200,
    # How long the take waits for a passing gate's land to show in the latest receipt (the gate lands
    # a passing candidate on its own), for m-harmless-difference in o-1 and for worker B in o-8.
    [int]$LandWaitSec = 900,
    # How long a control waits for its task to be open (unclaimed, not gating) before it claims. A
    # gating run holds its task until it closes or is released, so the take fails after this wait
    # and names the state; it does not release another run's claim.
    [int]$ClaimWaitSec = 600,
    [switch]$DraftOnly,
    [string]$ResumeRun = '',
    [switch]$SkipWorkers,
    # Defines the helper and step functions and returns, starting nothing. demo/intent/replay.ps1 dot-sources
    # this file with it. The mandatory inputs must still be given.
    [switch]$LibraryOnly
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
if (-not $AcanRepo) { $AcanRepo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot) }
if (-not $EditsFile) { $EditsFile = Join-Path $AcanRepo "demo\intent\$Order-edits.json" }
if (-not $RulesFile) { $RulesFile = Join-Path $AcanRepo 'demo\intent\rulings\o-1-rulings.json' }
$intentDir = Join-Path $AcanRepo 'demo\intent'
$coordinatorDecisions = if ($DecisionsFile -and (Test-Path -LiteralPath $DecisionsFile)) { Get-Content -LiteralPath $DecisionsFile -Raw | ConvertFrom-Json } else { $null }

$script:Steps = [System.Collections.Generic.List[object]]::new()
$script:Secrets = [System.Collections.Generic.List[string]]::new()
$script:Proc = $null
$script:Base = ''
$script:Repo = ''
$script:OwnerToken = ''
$script:Tokens = @{}
$script:Evidence = @{ orders = @{}; workers = @{}; controls = @{}; rulings = @{}; lands = @{} }
$script:HumanSteps = 0
$script:MemberCalls = 0
$script:DraftReady = $false
$script:Routes = $null
$script:LlmPath = ''
$script:EnvPath = ''
$demoBefore = $null
$started = (Get-Date).ToString('o')
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$run = Join-Path $RunsRoot "$Label-$stamp"
$outcome = 'incomplete'
$failure = $null

function Add-Step {
    param([string]$Name, [string]$Actor, [string]$Provider, [string]$Result, [hashtable]$Evidence = @{}, [switch]$Human)
    if ($Human) { $script:HumanSteps++ }
    $script:Steps.Add([ordered]@{
            at       = (Get-Date).ToString('o')
            step     = $Name
            actor    = $Actor
            provider = $Provider
            result   = $Result
            evidence = $Evidence
        })
    Write-Host "[$Result] $Name"
}

function Save-State {
    $state = [ordered]@{ label = $Label; run = $run; evidence = $script:Evidence; updated = (Get-Date).ToString('o') }
    ConvertTo-Json -Depth 40 $state | Set-Content -LiteralPath (Join-Path $run 'run-state.json') -Encoding utf8
}

function Assert-OutsideGit {
    param([string]$Path)
    $probe = $Path
    while (-not (Test-Path -LiteralPath $probe)) { $probe = Split-Path -Parent $probe }
    & git -C $probe rev-parse --show-toplevel *> $null
    if ($LASTEXITCODE -eq 0) {
        throw "the run root $Path sits inside a git repository; choose another -RunsRoot"
    }
}

function Assert-OwnRepo {
    $top = & git -C $script:Repo rev-parse --show-toplevel 2>$null
    if ($LASTEXITCODE -ne 0) { throw "the clone is not a git repository: $script:Repo" }
    $top = ([string]($top | Select-Object -First 1)).Trim() -replace '/', '\'
    $want = [IO.Path]::GetFullPath($script:Repo).TrimEnd('\')
    if ([IO.Path]::GetFullPath($top).TrimEnd('\') -ne $want) {
        throw "git resolves the clone to $top, not $want; refusing to run git there"
    }
}

function Invoke-Git {
    Assert-OwnRepo
    $out = & git -C $script:Repo @args
    if ($LASTEXITCODE -ne 0) { throw "git $($args -join ' ') failed in the clone" }
    $out
}

function Get-DemoRefs {
    $refs = & git -C $DemoRepo show-ref --heads 2>$null
    if ($LASTEXITCODE -ne 0) { throw "the demo repository at $DemoRepo does not read" }
    ($refs -join "`n")
}

function Read-KeyFile {
    param([string]$Path)
    $pairs = [ordered]@{}
    foreach ($line in Get-Content -LiteralPath $Path) {
        if ($line -match '^\s*(#|$)') { continue }
        if ($line -match '^\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*?)\s*$') {
            $pairs[$Matches[1]] = $Matches[2].Trim('"', "'")
        }
    }
    $pairs
}

function Read-Routing {
    param([string]$Text)
    $providers = [ordered]@{}
    foreach ($block in ($Text -split '\[\[providers\]\]')) {
        if ($block -match 'name\s*=\s*"([^"]+)"') {
            # Keep the name before the next match overwrites $Matches.
            $name = $Matches[1]
            $model = ''
            if ($block -match 'default_model\s*=\s*"([^"]+)"') { $model = $Matches[1] }
            $providers[$name] = $model
        }
    }
    $roles = [ordered]@{}
    foreach ($m in [regex]::Matches($Text, '\[routing\.(\w+)\]\s*\r?\nprovider\s*=\s*"([^"]+)"')) {
        $name = $m.Groups[2].Value
        $roles[$m.Groups[1].Value] = [ordered]@{ provider = $name; model = $providers[$name] }
    }
    $roles
}

function Invoke-Api {
    param([string]$Path)
    # Invoke-RestMethod writes a JSON array to the pipeline as one Object[] item, so `Invoke-Api ... | ForEach-Object`
    # saw the whole list as one item. Each element is written on its own here; an empty answer writes nothing.
    $reply = Invoke-RestMethod -Uri "$script:Base$Path" -Headers @{ Authorization = "Bearer $script:OwnerToken" } -TimeoutSec 60
    if ($null -ne $reply) { foreach ($item in @($reply)) { $item } }
}

function Invoke-Api404Tolerant {
    param([string]$Path)
    try {
        Invoke-Api -Path $Path
    } catch {
        $status = $null
        if ($_.Exception.Response) { $status = [int]$_.Exception.Response.StatusCode }
        if ($status -eq 404) { return $null }
        throw
    }
}

function Send-Command {
    param($Cmd)
    $script:MemberCalls++
    $body = ConvertTo-Json -Depth 60 -Compress $Cmd
    try {
        Invoke-RestMethod -Method Post -Uri "$script:Base/api/commands" -Headers @{ Authorization = "Bearer $script:OwnerToken" } -ContentType 'application/json' -Body $body -TimeoutSec 120
    } catch {
        $detail = if ($_.ErrorDetails) { $_.ErrorDetails.Message } else { $_.Exception.Message }
        throw "the origin refused $($Cmd.cmd): $detail"
    }
}

function Wait-Until {
    param([string]$What, [scriptblock]$Probe, [scriptblock]$Terminal)
    $end = (Get-Date).AddSeconds($StepWaitSec)
    while ($true) {
        if ($script:Proc.HasExited) {
            throw "the server exited with code $($script:Proc.ExitCode) while waiting for $What"
        }
        $found = & $Probe
        if ($found) { return $found }
        $stop = & $Terminal
        if ($stop) { throw "${What}: $stop" }
        if ((Get-Date) -gt $end) { throw "timed out after $StepWaitSec s waiting for $What" }
        Start-Sleep -Seconds 2
    }
}

function Test-Forbidden {
    param([scriptblock]$Call)
    try {
        & $Call | Out-Null
        return $null
    } catch {
        if ($_.Exception.Response) { return [int]$_.Exception.Response.StatusCode }
        throw
    }
}

function Find-Value {
    # The first property with this name anywhere in a parsed JSON tree.
    param($Node, [string]$Name)
    if ($null -eq $Node) { return $null }
    if ($Node -is [System.Collections.IDictionary]) {
        foreach ($key in $Node.Keys) {
            if ($key -eq $Name) { return $Node[$key] }
            $found = Find-Value -Node $Node[$key] -Name $Name
            if ($null -ne $found) { return $found }
        }
        return $null
    }
    if ($Node -is [System.Management.Automation.PSCustomObject]) {
        foreach ($prop in $Node.PSObject.Properties) {
            if ($prop.Name -eq $Name) { return $prop.Value }
            $found = Find-Value -Node $prop.Value -Name $Name
            if ($null -ne $found) { return $found }
        }
        return $null
    }
    if ($Node -is [System.Collections.IEnumerable] -and $Node -isnot [string]) {
        foreach ($item in $Node) {
            $found = Find-Value -Node $item -Name $Name
            if ($null -ne $found) { return $found }
        }
    }
    return $null
}

function Get-OrderTasks {
    # The task ids the foreman's split made for one order, as the MCP tools name them (t-N).
    param($Events, [string]$Raw)
    $split = @($Events | Where-Object { $_.event.type -eq 'order_split' -and "$($_.event.order)" -eq $Raw })
    if ($split.Count -eq 0) { return @() }
    @($split[0].event.tasks | ForEach-Object { "t-$($_[0])" })
}

function Start-Worker {
    param([string]$Name, [string]$Token, [string]$Tag)
    $argLine = "--server $script:Base --token $Token --repo `"$script:Repo`" --llm `"$script:LlmPath`" --env `"$script:EnvPath`" --test-command `"bun test`" --wait-secs 600 --max-steps $WorkerMaxSteps --heartbeat-secs 20"
    $p = Start-Process -FilePath $WorkerExe -ArgumentList $argLine `
        -RedirectStandardOutput (Join-Path $run "logs\$Name.$Tag.out.log") `
        -RedirectStandardError (Join-Path $run "logs\$Name.$Tag.err.log") -PassThru -WindowStyle Hidden
    if (-not $p.WaitForExit($WorkerWaitSec * 1000)) {
        Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
        return [ordered]@{ exited = $false; exit_code = $null }
    }
    [ordered]@{ exited = $true; exit_code = $p.ExitCode }
}

function Get-WorkerFinish {
    # The worker's own last line about how it ended, from its error log, without ANSI colour codes.
    param([string]$Name, [string]$Tag)
    $log = Join-Path $run "logs\$Name.$Tag.err.log"
    if (-not (Test-Path -LiteralPath $log)) { return $null }
    $line = Get-Content -LiteralPath $log | Where-Object { $_ -match 'the worker finished' } | Select-Object -Last 1
    if ($null -eq $line) { return $null }
    ($line -replace "`e\[[0-9;]*m", '')
}

function Save-TaskReceipt {
    # The task's latest receipt, as the origin answers get_receipt over MCP, saved to Path. Returns
    # its gate_state. The call uses the control identity's token, which may read receipts.
    param([string]$Task, [string]$Token, [string]$Path)
    $body = ConvertTo-Json -Depth 20 -Compress @{ jsonrpc = '2.0'; id = 1; method = 'tools/call'; params = @{ name = 'get_receipt'; arguments = @{ version = 'acan-mcp/1'; task = $Task } } }
    $resp = Invoke-RestMethod -Method Post -Uri "$($script:Base.TrimEnd('/'))/mcp" -Headers @{ Authorization = "Bearer $Token" } -ContentType 'application/json' -Body $body -TimeoutSec 120
    if ($resp.error) { throw "get_receipt refused: $($resp.error.message)" }
    $text = [string]$resp.result.content[0].text
    if ($resp.result.isError) { throw "get_receipt refused: $text" }
    Set-Content -LiteralPath $Path -Value $text -Encoding utf8
    [string](($text | ConvertFrom-Json).receipt.gate_state)
}

function Wait-TaskLanded {
    # Polls the task's latest receipt, saving each read to Path, until it is landed, blocked or
    # stopped, or Seconds pass. A passing gate lands its candidate on its own, a moment after the
    # receipt. The gate closes with the receipt's record, so a receipt that still reads prepared
    # while the task is neither gating nor queued means the gate failed: the wait stops there
    # instead of polling a prepared receipt until Seconds pass. Returns the last gate_state read.
    # Errors from the reads are left to the caller.
    param([string]$Task, [string]$Token, [string]$Path, [int]$Seconds)
    $state = ''
    $end = (Get-Date).AddSeconds($Seconds)
    while ($true) {
        $state = Save-TaskReceipt -Task $Task -Token $Token -Path $Path
        if (@('landed', 'blocked', 'stopped') -contains $state) { break }
        $view = Get-TaskView -Task $Task -Token $Token
        if ([string]$view.state -eq 'landed') { $state = 'landed'; break }
        $gateOpen = ([string]$view.state -eq 'queued') -or ([string]$view.stage -eq 'gating')
        if (-not $gateOpen) { break }
        if ((Get-Date) -gt $end) { break }
        Start-Sleep -Seconds 5
    }
    $state
}

function Get-TaskView {
    # The task's view as the origin answers get_task over MCP, for the control identity.
    param([string]$Task, [string]$Token)
    $body = ConvertTo-Json -Depth 20 -Compress @{ jsonrpc = '2.0'; id = 1; method = 'tools/call'; params = @{ name = 'get_task'; arguments = @{ task = $Task } } }
    $resp = Invoke-RestMethod -Method Post -Uri "$($script:Base.TrimEnd('/'))/mcp" -Headers @{ Authorization = "Bearer $Token" } -ContentType 'application/json' -Body $body -TimeoutSec 120
    if ($resp.error) { throw "get_task refused: $($resp.error.message)" }
    $text = [string]$resp.result.content[0].text
    if ($resp.result.isError) { throw "get_task refused: $text" }
    $text | ConvertFrom-Json
}

function Wait-TaskClaimable {
    # Returns the task's view once its state is open, so the next control can claim it. Polls
    # until Seconds pass; then throws, naming the state the task is still in.
    param([string]$Task, [string]$Token, [int]$Seconds)
    $end = (Get-Date).AddSeconds($Seconds)
    while ($true) {
        $view = Get-TaskView -Task $Task -Token $Token
        if ([string]$view.state -eq 'open') { return $view }
        if ((Get-Date) -gt $end) {
            throw "task $Task is $($view.state) (stage $($view.stage), generation $($view.generation)) after $Seconds s; a control cannot claim it. The take does not release another claim"
        }
        Start-Sleep -Seconds 5
    }
}

function Get-AgentEvents {
    param($Events, [string]$Agent)
    @($Events | Where-Object { (ConvertTo-Json -Depth 8 -Compress $_) -like "*`"$Agent`"*" } | ForEach-Object {
            [ordered]@{ seq = $_.seq; type = [string]$_.event.type; task = "$($_.event.task)" }
        })
}

function Get-WorkerTask {
    # The task a worker reports, from its own events: the task of its last build_reported event, or
    # else of its last lease_granted. The origin's list_tasks has no order filter, so the worker takes
    # whichever open task comes first; the take follows the task it reports, not the one it planned.
    param($WorkerEvents)
    $reported = @($WorkerEvents | Where-Object { $_.type -eq 'build_reported' -and $_.task -ne '' })
    $granted = @($WorkerEvents | Where-Object { $_.type -eq 'lease_granted' -and $_.task -ne '' })
    $pick = if ($reported.Count -gt 0) { $reported[-1] } elseif ($granted.Count -gt 0) { $granted[-1] } else { $null }
    if ($null -eq $pick) { return $null }
    "t-$($pick.task)"
}

function Close-OrderLeftovers {
    # Ends the order's tasks that have not landed, through the member route cancel_order (any phase but
    # landed; it drops the open and claimed tasks). Without it a later order's worker would claim them,
    # because list_tasks has no order filter. Sends nothing when every task of the order has landed.
    param([string]$Raw, [string[]]$Tasks)
    $events = @(Invoke-Api -Path '/api/events')
    # Once the order is cancelled, the call is done: a resumed replay does not send it again.
    if (@($events | Where-Object { $_.event.type -eq 'order_cancelled' -and "$($_.event.order)" -eq $Raw }).Count -gt 0) {
        return [ordered]@{ dropped = @(); reply = @('already cancelled') }
    }
    $landed = @($events | Where-Object { $_.event.type -eq 'landed' } | ForEach-Object { "t-$($_.event.task)" })
    $left = @($Tasks | Where-Object { $_ -and ($landed -notcontains $_) })
    if ($left.Count -eq 0) { return [ordered]@{ dropped = @(); reply = @() } }
    $reply = Send-Command @{ cmd = 'cancel_order'; order = [int]$Raw }
    [ordered]@{ dropped = $left; reply = @($reply | ForEach-Object { [string]$_.event.type }) }
}

function Get-LandCheck {
    param([string]$ReceiptFile, [string]$What)
    $rec = Get-Content -LiteralPath $ReceiptFile -Raw | ConvertFrom-Json
    $merged = [string](Find-Value -Node $rec -Name 'merged_commit')
    $head = [string]((Invoke-Git rev-parse main | Out-String).Trim())
    [ordered]@{ what = $What; receipt_merged_sha = $merged; protected_head = $head; equal = ($merged -ne '' -and $merged -eq $head) }
}

function Read-ControlResult {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) { return $null }
    Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
}

function New-TakeClone {
    # The clone of the demo repository at the pin, under the run folder. Sets $script:Repo, checks
    # out the pin on main and records the clone as a step. Throws when the clone is not right.
    param([string]$Run, [string]$DemoRepo, [string]$Pin)
    $script:Repo = Join-Path $Run 'repo'
    & git clone --no-hardlinks -q $DemoRepo $script:Repo
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath (Join-Path $script:Repo '.git'))) {
        throw 'the clone failed; the run folder has no repository and nothing else ran'
    }
    Assert-OwnRepo
    Invoke-Git checkout -q -B main $Pin | Out-Null
    $head = (Invoke-Git rev-parse HEAD | Out-String).Trim()
    if ($head -ne $Pin) { throw "the clone's HEAD is $head, not the pin $Pin" }
    Add-Step -Name 'clone the demo at the pin' -Actor 'script' -Provider 'none' -Result 'pass' -Evidence @{ repo = $script:Repo; head = $head }
}

function Start-TakeServer {
    # acan serve on the clone, with the keys in its environment only, then /health. Sets $script:Base
    # and $script:Proc, records the start as a step and returns the process. Throws when the server
    # exits or does not answer within HealthWaitSec.
    param([string]$Run, [string]$Repo, [int]$Port, [string]$AcanExe, [string]$LlmPath, [string]$EnvPath, [string]$LogTag, [int]$HealthWaitSec)
    $keys = Read-KeyFile -Path $EnvPath
    $script:Base = "http://127.0.0.1:$Port"
    $serverArgs = @('serve', '--repo', $Repo, '--port', "$Port", '--bind', '127.0.0.1',
        '--protected-branch', 'main', '--llm', $LlmPath)
    foreach ($name in $keys.Keys) { Set-Item -Path "Env:$name" -Value $keys[$name] }
    try {
        $script:Proc = Start-Process -FilePath $AcanExe -ArgumentList $serverArgs `
            -RedirectStandardOutput (Join-Path $Run "logs\server.$LogTag.out.log") `
            -RedirectStandardError (Join-Path $Run "logs\server.$LogTag.err.log") -PassThru -WindowStyle Hidden
    } finally {
        foreach ($name in $keys.Keys) { Remove-Item -Path "Env:$name" -ErrorAction SilentlyContinue }
    }
    $up = $false
    $healthEnd = (Get-Date).AddSeconds($HealthWaitSec)
    while (-not $up) {
        if ($script:Proc.HasExited) { throw "the server exited with code $($script:Proc.ExitCode); see the server logs in logs\" }
        if ((Get-Date) -gt $healthEnd) { throw 'the server did not answer /health in time' }
        try { $null = Invoke-RestMethod -Uri "$script:Base/health" -TimeoutSec 5; $up = $true } catch { Start-Sleep -Milliseconds 500 }
    }
    Add-Step -Name "start the server on the clone ($LogTag)" -Actor 'script' -Provider 'none' -Result 'pass' -Evidence @{ port = $Port; pid = $script:Proc.Id }
    $script:Proc
}

function Invoke-ScopeEdits {
    # One order's scope edits, applied to the foreman's draft as one revise_scope. The edits file is
    # read and checked here. Each scope edit, and each open choice that a choice rule answers or
    # removes, is logged as a step. The decider is $DecidedBy, or the file's decided_by when it is
    # empty. The file's explicit answers are left to Invoke-ScopeConfirm. Returns the revised scope,
    # the proposal, the logged edits and the answers.
    param([string]$Order, [string]$Raw, [string]$Oid, $View, [string]$EditsFile, [string]$OrderDir = '', [string]$DecidedBy = '')
    $edits = Get-Content -LiteralPath $EditsFile -Raw | ConvertFrom-Json
    if ($edits.schema -ne 'acan-demo-intent-edits/2') {
        throw "the edits file must have schema acan-demo-intent-edits/2, found '$($edits.schema)'"
    }
    if ($edits.order -and $edits.order -ne $Order) { throw "the edits file is for $($edits.order), not $Order" }
    $decider = if ($DecidedBy) { $DecidedBy } else { [string]$edits.decided_by }
    if (-not $decider) { throw "$EditsFile names no decided_by; a decision file must say who decided it" }
    $d = $View.draft
    $proposal = $d.draft.proposal
    $humanEdits = [System.Collections.Generic.List[object]]::new()
    $humanAnswers = [System.Collections.Generic.List[object]]::new()
    foreach ($edit in @($edits.scope_edits)) {
        if ($edit.op -eq 'remove_may') {
            if (@($proposal.may | Where-Object { $_.id -eq $edit.id }).Count -ne 1) {
                throw "remove_may: may item $($edit.id) is not in the draft; the edits file must match the draft"
            }
            $proposal.may = @($proposal.may | Where-Object { $_.id -ne $edit.id })
            $humanEdits.Add(@{ op = $edit.op; id = $edit.id; result = 'removed'; reason = $edit.reason })
        } elseif ($edit.op -eq 'replace_must_keep') {
            if (@($proposal.must_keep | Where-Object { $_.id -eq $edit.rule.id }).Count -ne 1) {
                throw "replace_must_keep: must-keep $($edit.rule.id) is not in the draft; the edits file must match the draft"
            }
            $proposal.must_keep = @($proposal.must_keep | ForEach-Object { if ($_.id -eq $edit.rule.id) { $edit.rule } else { $_ } })
            $humanEdits.Add(@{ op = $edit.op; id = $edit.rule.id; result = 'replaced'; reason = $edit.reason })
        } elseif ($edit.op -eq 'add_must_keep') {
            if (@($proposal.must_keep | Where-Object { $_.id -eq $edit.rule.id }).Count -gt 0) {
                throw "must-keep $($edit.rule.id) is already in the draft"
            }
            $proposal.must_keep = @($proposal.must_keep) + @($edit.rule)
            $humanEdits.Add(@{ op = $edit.op; id = $edit.rule.id; result = 'added'; reason = $edit.reason })
        } elseif ($edit.op -eq 'add_may') {
            if (@($proposal.may | Where-Object { $_.id -eq $edit.item.id }).Count -gt 0) {
                throw "may item $($edit.item.id) is already in the draft"
            }
            $proposal.may = @($proposal.may) + @($edit.item)
            $humanEdits.Add(@{ op = $edit.op; id = $edit.item.id; result = 'added'; reason = $edit.reason })
        } else {
            throw "unknown scope edit op '$($edit.op)' in $EditsFile"
        }
    }
    # Choice rules match an open choice by its question text, because the foreman's question ids
    # and wording change between drafts. A choice matched by no rule is left for an explicit answer.
    foreach ($choice in @($proposal.open)) {
        $matched = @($edits.choice_rules | Where-Object { $choice.question -match $_.question_match })
        if ($matched.Count -eq 0) { continue }
        if ($matched.Count -gt 1) { throw "more than one choice rule matches $($choice.id)" }
        $rule = $matched[0]
        $keep = @($choice.options | Where-Object { $_.label -match $rule.option_label_match })
        if ($keep.Count -gt 1) { throw "choice rule $($rule.id) matches more than one option of $($choice.id)" }
        if ($keep.Count -eq 1) {
            $humanAnswers.Add(@{ choice_id = $choice.id; option = $keep[0].key })
            $humanEdits.Add(@{ op = 'answer_choice'; id = $choice.id; rule = $rule.id; option_label = $keep[0].label; result = 'answered'; reason = $rule.reason })
        } elseif ($rule.when_no_option_matches -eq 'remove') {
            $proposal.open = @($proposal.open | Where-Object { $_.id -ne $choice.id })
            $humanEdits.Add(@{ op = 'remove_open'; id = $choice.id; rule = $rule.id; question = $choice.question; result = 'removed'; reason = $rule.reason })
        } else {
            throw "choice $($choice.id) matches rule $($rule.id) but no option matches '$($rule.option_label_match)'"
        }
    }
    $scope = @{ version = 'acan-scope/1'; order = $d.order; rev = $d.rev; base_commit = $d.base_commit; body = $proposal }
    $null = Send-Command @{ cmd = 'revise_scope'; order = [int]$Raw; scope = $scope }
    $leaf = Split-Path -Leaf $EditsFile
    Add-Step -Name "${Order}: the owner revises the draft with the logged scope edits (revise_scope)" -Actor 'owner (member HTTP)' -Provider 'none' -Result 'pass' -Evidence @{ rev = $d.rev; decided_by = $decider; logged_edits = $humanEdits.Count; open_after = @($proposal.open).Count; may_after = @($proposal.may).Count; must_keep_after = @($proposal.must_keep).Count } -Human
    foreach ($entry in $humanEdits) {
        Add-Step -Name "$Order scope edit: $($entry.op) $($entry.id)" -Actor "$decider (edits file $leaf, applied by the owner through revise_scope)" -Provider 'none' -Result 'pass' -Evidence $entry -Human
    }
    if ($OrderDir) {
        $record = [ordered]@{ order = $Order; decided_by = $decider; edits_file = $EditsFile; edits_sha256 = (Get-FileHash -LiteralPath $EditsFile -Algorithm SHA256).Hash; rev = $d.rev; logged_edits = @($humanEdits); answers = @($humanAnswers) }
        ConvertTo-Json -Depth 40 $record | Set-Content -LiteralPath (Join-Path $OrderDir 'scope-edits.json') -Encoding utf8
    }
    [ordered]@{ scope = $scope; proposal = $proposal; edits = $edits; edits_file = $EditsFile; decided_by = $decider; logged_edits = @($humanEdits); answers = @($humanAnswers); rev = $d.rev }
}

function Invoke-ScopeConfirm {
    # The owner's confirmation of a revised scope. The confirmation body is read from the origin, its
    # episode cap is raised to at least MaxEpisodes, and its repair rounds are set when RepairRounds is
    # above zero. Every open choice gets one answer: from a choice rule, or from the edits file. Sends
    # confirm_scope and returns the scope hash, the budget and the answers. Throws when the origin
    # refuses the confirmation or confirms no scope.
    param($Scope, [string]$Order, [string]$Raw, [string]$Oid, [int]$MaxEpisodes, [int]$RepairRounds = 0)
    $body = Invoke-Api -Path "/api/orders/$Oid/scope/confirmation"
    $offeredCap = [int]$body.budget.max_episodes
    $body.budget.max_episodes = [uint32]([math]::Max($MaxEpisodes, $offeredCap))
    $offeredRounds = [int]$body.budget.repair_rounds
    if ($RepairRounds -gt 0) { $body.budget.repair_rounds = [uint32]$RepairRounds }
    $answers = @()
    foreach ($choice in @($Scope.proposal.open)) {
        $fromRule = @($Scope.answers | Where-Object { $_.choice_id -eq $choice.id })
        if ($fromRule.Count -eq 1) {
            $answers += @{ choice_id = $choice.id; option = $fromRule[0].option }
            continue
        }
        $given = @($Scope.edits.answers | Where-Object { $_.choice_id -eq $choice.id })
        if ($given.Count -ne 1) {
            throw "open choice $($choice.id) needs exactly one answer in $($Scope.edits_file); options: $((@($choice.options) | ForEach-Object { $_.label }) -join ' | ')"
        }
        $option = @($choice.options | Where-Object { $_.label -eq $given[0].option_label })
        if ($option.Count -ne 1) {
            throw "answer '$($given[0].option_label)' for $($choice.id) matches no option label"
        }
        $answers += @{ choice_id = $choice.id; option = $option[0].key }
    }
    $body.answers = $answers
    $confirmed = Send-Command @{ cmd = 'confirm_scope'; order = [int]$Raw; confirmation = $body }
    $scopeConfirmed = @($confirmed | Where-Object { $_.event.type -eq 'scope_confirmed' }).Count
    if ($scopeConfirmed -lt 1) { throw 'confirm_scope returned no scope_confirmed event' }
    [ordered]@{
        scope_confirmed = $scopeConfirmed; scope_hash = "$($body.scope_hash)"; budget = $body.budget; answers = $answers
        offered_max_episodes = $offeredCap; offered_repair_rounds = $offeredRounds
    }
}

function Invoke-ControlWithRuling {
    # A seeded control whose receipt has out-of-scope differences that the owner's rules decide. The
    # control runs as its own process with -AwaitRuling. Each receipt it saves is ruled from RulesFile
    # through member HTTP (ruling.ps1) before the done flag lets it report again. An earlier control's
    # receipt on the same task (PriorReceipt) is ruled first in the same way, so the re-gate is not
    # blocked by that candidate's episodes (D02Q). Then the re-gate is waited for and, when it passes,
    # the land. Returns a summary. A failed ruling is returned in ruling_error and is not thrown; the
    # caller records it.
    param(
        [string]$Task, [string]$Bundle, [string]$Out, [string]$Order, [string]$OrderId, [string]$RulesFile,
        [string]$PriorReceipt = '', [string]$PriorRulingsOut = '', [string]$FinalReceipt = '',
        [int]$ControlWaitSec = 1500, [int]$RulingWaitSec = 1200, [int]$LandWaitSec = 900
    )
    $token = $script:Tokens['control-seeded']
    $ctrlDir = Split-Path -Parent $Out
    $done = Join-Path $ctrlDir 'rulings-done.flag'
    $rulingsOut = Join-Path $ctrlDir "rulings-$Order.json"
    if (-not $PriorRulingsOut) { $PriorRulingsOut = Join-Path $ctrlDir "rulings-$Order-prior.json" }
    if (-not $FinalReceipt) { $FinalReceipt = Join-Path $ctrlDir 'm-harmless-final-receipt.json' }
    if (Test-Path -LiteralPath $done) { Remove-Item -LiteralPath $done }
    # An earlier attempt's receipt, result or rulings are not this control's. They move aside, so the
    # ruling step waits for this control's own receipt; an old receipt would be ruled on as if new.
    $staleStamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    foreach ($stale in @($Out, "$Out.receipt.json", $FinalReceipt, $rulingsOut, $PriorRulingsOut)) {
        if (Test-Path -LiteralPath $stale) { Move-Item -LiteralPath $stale -Destination "$stale.prev-$staleStamp" }
    }
    $ctlArgs = "-NoProfile -File `"$(Join-Path $intentDir 'control.ps1')`" -Server $script:Base -Token $token -Repo `"$script:Repo`" -Task $Task -Bundle `"$Bundle`" -Out `"$Out`" -StageWaitSec 300 -ReceiptWaitSec $ControlWaitSec -AwaitRuling -RulingWaitSec $RulingWaitSec -RulingsDone `"$done`""
    $ctl = Start-Process -FilePath (Get-Command pwsh).Source -ArgumentList $ctlArgs -RedirectStandardOutput "$Out.log" -RedirectStandardError "$Out.err.log" -PassThru -WindowStyle Hidden
    $recEnd = (Get-Date).AddSeconds(300 + $ControlWaitSec)
    while (-not (Test-Path -LiteralPath "$Out.receipt.json")) {
        if ($ctl.HasExited) { break }
        if ((Get-Date) -gt $recEnd) { break }
        Start-Sleep -Seconds 3
    }
    $noReceipt = -not (Test-Path -LiteralPath "$Out.receipt.json")
    $controlExited = $ctl.HasExited
    $rulingErr = $null
    $rulings = @()
    $rulesDecidedBy = [string]((Get-Content -LiteralPath $RulesFile -Raw | ConvertFrom-Json).decided_by)
    if (-not $noReceipt) {
        # The control writes the receipt file in one call; give the write time to finish.
        Start-Sleep -Seconds 5
        try {
            & (Join-Path $intentDir 'ruling.ps1') -Server $script:Base -OwnerToken $script:OwnerToken -Order $Order -OrderId $OrderId -ReceiptFile "$Out.receipt.json" -RulesFile $RulesFile -Out $rulingsOut
            if ($PriorReceipt) {
                & (Join-Path $intentDir 'ruling.ps1') -Server $script:Base -OwnerToken $script:OwnerToken -Order $Order -OrderId $OrderId -ReceiptFile $PriorReceipt -RulesFile $RulesFile -Out $PriorRulingsOut
            }
            $rulings = @((Get-Content -LiteralPath $rulingsOut -Raw | ConvertFrom-Json).rulings)
            if ($PriorReceipt) {
                $rulings = $rulings + @((Get-Content -LiteralPath $PriorRulingsOut -Raw | ConvertFrom-Json).rulings)
            }
        } catch {
            $rulingErr = $_.Exception.Message
        }
    }
    # The flag is set whether or not the rulings went through, so the control is released to report again.
    Set-Content -LiteralPath $done -Value 'rulings finished' -Encoding utf8
    $ctlKilled = $false
    # After the flag the control waits for its re-gate's receipt for up to ControlWaitSec, so the wait
    # here is that long plus a margin before a kill.
    if (-not $ctl.WaitForExit(($ControlWaitSec + 300) * 1000)) {
        # The control saves its result after every step, so its evidence survives the kill. A killed
        # control runs no release, so its claim is not released by this run.
        $ctlKilled = $true
        Stop-Process -Id $ctl.Id -Force -ErrorAction SilentlyContinue
        Write-Host "control $Task was killed at its wait; its result so far is $Out"
    }
    $r3 = Read-ControlResult -Path $Out
    $rulingsOk = ($null -eq $rulingErr) -and (-not $noReceipt) -and (Test-Path -LiteralPath $rulingsOut) -and ((-not $PriorReceipt) -or (Test-Path -LiteralPath $PriorRulingsOut))
    # The re-gate passes when the receipt after the rulings is not blocked: prepared when every item
    # passes. A control that was killed at its wait does not pass.
    $regatePassed = ($null -ne $r3) -and (-not $ctlKilled) -and $rulingsOk -and (@('prepared', 'landed') -contains [string]$r3.gate_state)
    # A passing gate lands its candidate on its own, a moment after the receipt. The wait ends when the
    # latest receipt shows landed, blocked or stopped; the merged commit is then checked against the head.
    $landState = ''
    if (-not $ctlKilled) {
        # Saved whenever the control was not killed, so a blocked re-gate leaves its receipt too.
        try { $landState = Wait-TaskLanded -Task $Task -Token $token -Path $FinalReceipt -Seconds $LandWaitSec }
        catch { Write-Host "get_receipt: $($_.Exception.Message)" }
    }
    $landedNow = [bool]($regatePassed -and $landState -eq 'landed' -and (Test-Path -LiteralPath $FinalReceipt))
    $land = $null
    if ($landedNow) { $land = Get-LandCheck -ReceiptFile $FinalReceipt -What $Order }
    [ordered]@{
        ruling_error = $rulingErr; no_receipt = $noReceipt; control_exited = $controlExited
        rulings = @($rulings); ruling_ids = @($rulings | ForEach-Object { $_.ruling_id }); rulings_ok = $rulingsOk
        rulings_decided_by = $rulesDecidedBy; rulings_file = $rulingsOut
        prior_rulings_file = $(if ($PriorReceipt) { $PriorRulingsOut } else { $null })
        control_result = $r3; killed_at_wait = $ctlKilled; regate_passed = $regatePassed
        final_receipt = $FinalReceipt; final_gate_state = $landState; landed = $landedNow; land = $land
    }
}

# The helpers and step functions above are the take's library. With -LibraryOnly the script stops here:
# demo/intent/replay.ps1 dot-sources this file for the functions and starts nothing.
if ($LibraryOnly) { return }

try {
    # ---- preflight -----------------------------------------------------------------------
    $freeGb = [math]::Round((Get-PSDrive C).Free / 1GB, 1)
    if ($freeGb -lt 5) { throw "C: has $freeGb GB free; the take stops below 5 GB" }
    docker info *> $null
    if ($LASTEXITCODE -ne 0) { throw 'Docker is not running; start Docker Desktop before the take' }
    foreach ($path in @($AcanExe, $WorkerExe, $LlmSource, $EnvSource, (Join-Path $intentDir 'orders.json'), $RulesFile)) {
        if (-not (Test-Path -LiteralPath $path)) { throw "missing input: $path" }
    }
    Assert-OutsideGit -Path $RunsRoot
    $demoBefore = Get-DemoRefs
    $orders = Get-Content -LiteralPath (Join-Path $intentDir 'orders.json') -Raw | ConvertFrom-Json
    $ord = $orders.orders | Where-Object { $_.id -eq $Order }
    if (-not $ord) { throw "order $Order is not in orders.json" }

    # ---- run folder and clone, or the resumed run -----------------------------------------
    $priorSteps = @()
    if ($ResumeRun) {
        $run = (Resolve-Path -LiteralPath $ResumeRun).Path
        $script:Repo = Join-Path $run 'repo'
        if (-not (Test-Path -LiteralPath (Join-Path $script:Repo '.git'))) { throw "resume: no clone in $run" }
        Assert-OwnRepo
        $head = (Invoke-Git rev-parse HEAD | Out-String).Trim()
        if ($head -ne $Pin) { throw "the resumed clone's HEAD is $head, not the pin $Pin" }
        $priorSummary = Join-Path $run 'summary.json'
        if (Test-Path -LiteralPath $priorSummary) {
            $prev = Get-Content -LiteralPath $priorSummary -Raw | ConvertFrom-Json -AsHashtable
            $priorSteps = @($prev.steps)
            Move-Item -LiteralPath $priorSummary -Destination (Join-Path $run ("summary.prev-" + (Get-Date -Format 'yyyyMMdd-HHmmss') + '.json'))
        }
        $statePath = Join-Path $run 'run-state.json'
        if (Test-Path -LiteralPath $statePath) {
            $prevState = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json -AsHashtable
            $script:Evidence = $prevState.evidence
        }
        foreach ($k in @('orders', 'workers', 'controls', 'rulings', 'lands')) { if (-not $script:Evidence.ContainsKey($k)) { $script:Evidence[$k] = @{} } }
    } else {
        New-Item -ItemType Directory -Path $run | Out-Null
        New-TakeClone -Run $run -DemoRepo $DemoRepo -Pin $Pin
    }

    # ---- keys, routing and tokens: outside the repository except the ignored tokens file --
    $keysDir = Join-Path $run 'keys'
    $configDir = Join-Path $run 'config'
    if (-not $ResumeRun) {
        New-Item -ItemType Directory -Path $keysDir, $configDir, (Join-Path $run 'logs'), (Join-Path $run 'controls') | Out-Null
        Copy-Item -LiteralPath $EnvSource -Destination (Join-Path $keysDir '.env')
        Copy-Item -LiteralPath $LlmSource -Destination (Join-Path $configDir 'llm.toml')
        $script:Tokens = @{
            owner          = 'acan_owner_' + [guid]::NewGuid().ToString('N')
            'worker-a'     = 'acan_agent_a_' + [guid]::NewGuid().ToString('N')
            'control-seeded' = 'acan_agent_c_' + [guid]::NewGuid().ToString('N')
            'worker-b'     = 'acan_agent_b_' + [guid]::NewGuid().ToString('N')
        }
        $tokenEntries = @(
            @{ token = $script:Tokens.owner; principal = @{ kind = 'member'; id = 'owner' } },
            @{ token = $script:Tokens['worker-a']; principal = @{ kind = 'agent'; id = 'worker-a' } },
            @{ token = $script:Tokens['control-seeded']; principal = @{ kind = 'agent'; id = 'control-seeded' } },
            @{ token = $script:Tokens['worker-b']; principal = @{ kind = 'agent'; id = 'worker-b' } }
        )
        Set-Content -LiteralPath (Join-Path $script:Repo '.acan\tokens.json') -Value (ConvertTo-Json -Depth 5 $tokenEntries) -Encoding utf8
        Add-Step -Name 'write the run configuration' -Actor 'script' -Provider 'none' -Result 'pass' -Evidence @{ tokens = 'written to .acan/tokens.json (ignored), not printed' }
    } else {
        $existing = Get-Content -LiteralPath (Join-Path $script:Repo '.acan\tokens.json') -Raw | ConvertFrom-Json
        $script:Tokens = @{
            owner            = [string](@($existing | Where-Object { $_.principal.kind -eq 'member' })[0].token)
            'worker-a'       = [string](@($existing | Where-Object { $_.principal.id -eq 'worker-a' })[0].token)
            'control-seeded' = [string](@($existing | Where-Object { $_.principal.id -eq 'control-seeded' })[0].token)
            'worker-b'       = [string](@($existing | Where-Object { $_.principal.id -eq 'worker-b' })[0].token)
        }
    }
    $keys = Read-KeyFile -Path (Join-Path $keysDir '.env')
    foreach ($value in $keys.Values) { if ($value.Length -ge 8) { $script:Secrets.Add($value) } }
    foreach ($tok in $script:Tokens.Values) { $script:Secrets.Add([string]$tok) }
    $script:OwnerToken = $script:Tokens.owner
    $script:Routes = Read-Routing -Text (Get-Content -LiteralPath (Join-Path $configDir 'llm.toml') -Raw)
    $script:LlmPath = Join-Path $configDir 'llm.toml'
    $script:EnvPath = Join-Path $keysDir '.env'

    # ---- server ----------------------------------------------------------------------------
    $logTag = if ($ResumeRun) { "resume-$Order-$(Get-Date -Format 'HHmmss')" } else { 'first' }
    $null = Start-TakeServer -Run $run -Repo $script:Repo -Port $Port -AcanExe $AcanExe -LlmPath $script:LlmPath -EnvPath $script:EnvPath -LogTag $logTag -HealthWaitSec $HealthWaitSec
    for ($i = 0; $i -lt $priorSteps.Count; $i++) { $script:Steps.Insert($i, $priorSteps[$i]) }

    # ---- the order: post or read back, split, draft -----------------------------------------
    $stateFile = Join-Path $run "order-$Order.json"
    if (Test-Path -LiteralPath $stateFile) {
        $state = Get-Content -LiteralPath $stateFile -Raw | ConvertFrom-Json
        $raw = [string]$state.raw
        $oid = [string]$state.oid
        Add-Step -Name "${Order}: read back the order posted earlier" -Actor 'script' -Provider 'none' -Result 'pass' -Evidence @{ order = $oid }
    } else {
        $posted = Send-Command @{ cmd = 'post_order'; machine = $ord.machine; sentence = $ord.sentence; witnesses = @() }
        $postedEvent = $posted | Where-Object { $_.event.type -eq 'order_posted' } | Select-Object -First 1
        if (-not $postedEvent) { throw 'post_order returned no order_posted event' }
        $raw = "$($postedEvent.event.order)"
        $oid = if ($raw -like 'o-*') { $raw } else { "o-$raw" }
        ConvertTo-Json @{ raw = $raw; oid = $oid; machine = $ord.machine; sentence = $ord.sentence } | Set-Content -LiteralPath $stateFile -Encoding utf8
        Add-Step -Name "${Order}: post the order" -Actor 'owner (member HTTP)' -Provider 'none' -Result 'pass' -Evidence @{ order = $oid; machine = $ord.machine } -Human
    }
    $null = Wait-Until -What "the split of $Order" -Probe {
        $events = Invoke-Api -Path '/api/events'
        $events | Where-Object { $_.event.type -eq 'order_split' -and "$($_.event.order)" -eq $raw } | Select-Object -First 1
    } -Terminal { $null }
    $splitEvents = Invoke-Api -Path '/api/events'
    $tasks = @(Get-OrderTasks -Events $splitEvents -Raw $raw)
    if ($tasks.Count -eq 0) { throw "the split of $Order made no task" }
    Add-Step -Name "${Order}: the foreman splits the order into tasks" -Actor 'foreman' -Provider "$($script:Routes.foreman.provider) / $($script:Routes.foreman.model)" -Result 'pass' -Evidence @{ tasks = $tasks }

    $confirmed = $script:Evidence.orders.ContainsKey($Order) -and [bool]$script:Evidence.orders[$Order].confirmed
    if (-not $confirmed) {
        $view = Wait-Until -What "the foreman draft for $Order" -Probe { Invoke-Api404Tolerant -Path "/api/orders/$oid/scope-draft" } -Terminal { $null }
        if ($view.state -ne 'drafted') { throw "the foreman draft did not check: $($view.reason | ConvertTo-Json -Compress)" }
        $d = $view.draft
        $proposal = $d.draft.proposal
        Set-Content -LiteralPath (Join-Path $run "scope-draft-$Order.json") -Value ($d | ConvertTo-Json -Depth 60) -Encoding utf8
        Add-Step -Name "${Order}: the foreman drafts the scope" -Actor 'foreman' -Provider "$($script:Routes.foreman.provider) / $($d.draft.model)" -Result 'pass' -Evidence @{
            provenance = $d.draft.provenance; rev = $d.rev; may = @($proposal.may).Count; must_keep = @($proposal.must_keep).Count
            may_ids = @($proposal.may | ForEach-Object { $_.id }); must_keep_ids = @($proposal.must_keep | ForEach-Object { $_.id }); open_ids = @($proposal.open | ForEach-Object { $_.id })
        }
        if ($DraftOnly) {
            $script:DraftReady = $true
            throw "draft recorded for human review; write the edits for this draft in $EditsFile, then resume with -ResumeRun $run -Order $Order -EditsFile <that file>"
        }

        # ---- the human scope: edits, one revise_scope, then confirm ---------------------------
        # Invoke-ScopeEdits reads the edits file and sends revise_scope; the decider is the file's decided_by.
        $scope = Invoke-ScopeEdits -Order $Order -Raw $raw -Oid $oid -View $view -EditsFile $EditsFile
        $confirmed = Invoke-ScopeConfirm -Order $Order -Raw $raw -Oid $oid -Scope $scope -MaxEpisodes $MaxEpisodes -RepairRounds $RepairRounds
        Add-Step -Name "${Order}: the owner raises the repair budget's episode cap at confirmation" -Actor 'owner (member HTTP)' -Provider 'none' -Result 'pass' -Evidence @{
            offered_max_episodes = $confirmed.offered_max_episodes; set_max_episodes = [int]$confirmed.budget.max_episodes
            offered_repair_rounds = $confirmed.offered_repair_rounds; set_repair_rounds = [int]$confirmed.budget.repair_rounds
            reason = 'every differing field of a route is one discrepancy episode, and the offered cap of four stops the order at the first receipt with more; the owner sets the cap for this order'
        } -Human
        Add-Step -Name "${Order}: the owner confirms the scope (member HTTP)" -Actor 'owner (member HTTP)' -Provider 'none' -Result 'pass' -Evidence @{
            scope_confirmed = $confirmed.scope_confirmed; answers = $confirmed.answers; scope_hash = $confirmed.scope_hash; budget = $confirmed.budget
        } -Human
        $script:Evidence.orders[$Order] = @{ oid = $oid; raw = $raw; confirmed = $true; scope_hash = $confirmed.scope_hash; max_episodes = [int]$confirmed.budget.max_episodes }
        Save-State
    } else {
        Add-Step -Name "${Order}: the scope was confirmed in an earlier invocation" -Actor 'script' -Provider 'none' -Result 'pass' -Evidence @{ order = $oid }
    }

    if ($Order -eq 'o-1' -and -not $script:Evidence.orders.ContainsKey('refusals')) {
        # Refusals: an agent token must not reach the member routes.
        $agentHeaders = @{ Authorization = "Bearer $($script:Tokens['worker-a'])" }
        $confirmStatus = Test-Forbidden -Call { Invoke-RestMethod -Uri "$script:Base/api/orders/$oid/scope/confirmation" -Headers $agentHeaders }
        $commandStatus = Test-Forbidden -Call { Invoke-RestMethod -Method Post -Uri "$script:Base/api/commands" -Headers $agentHeaders -ContentType 'application/json' -Body "{`"cmd`":`"cancel_order`",`"order`":$raw}" }
        $refusalOk = ($confirmStatus -eq 403 -and $commandStatus -eq 403)
        Add-Step -Name 'agent token refused on member routes' -Actor 'worker-a token' -Provider 'none' -Result $(if ($refusalOk) { 'pass' } else { 'fail' }) -Evidence @{ confirmation_status = $confirmStatus; command_status = $commandStatus }
        $script:Evidence.orders['refusals'] = @{ confirmation_status = $confirmStatus; command_status = $commandStatus; ok = $refusalOk }
        Save-State
    }

    # ---- agents: register the three identities the take uses, once each ---------------------
    $events = Invoke-Api -Path '/api/events'
    foreach ($name in @('worker-a', 'control-seeded', 'worker-b')) {
        $already = @($events | Where-Object { $_.event.type -eq 'agent_registered' -and (ConvertTo-Json -Depth 8 -Compress $_) -like "*`"$name`"*" }).Count
        if ($already -eq 0) {
            $null = Send-Command @{ cmd = 'register_agent'; agent = $name }
            Add-Step -Name "register agent $name" -Actor 'owner (member HTTP)' -Provider 'none' -Result 'pass' -Evidence @{ agent = $name } -Human
        }
    }

    $ctrlRoot = Join-Path $run 'controls'
    New-Item -ItemType Directory -Path $ctrlRoot -Force | Out-Null
    $controlToken = $script:Tokens['control-seeded']

    if ($Order -eq 'o-1') {
        $t0 = $tasks[0]
        # ---- step 1: seeded regression m-overfit, control ----------------------------------------
        try {
            $view0 = Wait-TaskClaimable -Task $t0 -Token $controlToken -Seconds $ClaimWaitSec
            Add-Step -Name 'o-1: task t-0 is open for the m-overfit claim' -Actor 'script' -Provider 'none' -Result 'pass' -Evidence @{ task = $t0; state = [string]$view0.state; generation = $view0.generation }
        } catch {
            Add-Step -Name 'o-1: task t-0 is open for the m-overfit claim' -Actor 'script' -Provider 'none' -Result 'fail' -Evidence @{ task = $t0; error = $_.Exception.Message }
            throw
        }
        $mo = Join-Path $ctrlRoot 'm-overfit.json'
        # The first control on the order: if its claim ends before the build is cleared (its lease ran
        # out while the order's declare barrier held), it claims and declares again once.
        & (Join-Path $intentDir 'control.ps1') -Server $script:Base -Token $controlToken -Repo $script:Repo -Task $t0 `
            -Bundle (Join-Path $intentDir 'controls\m-overfit.json') -Out $mo -StageWaitSec 300 -ReceiptWaitSec $ControlWaitSec -RedeclareOnce
        $r2 = Read-ControlResult -Path $mo
        $r2Obl = @($r2.items | Where-Object { $_.class -eq 'obligation_failed' }).Count
        $r2Ok = ($r2.gate_state -eq 'blocked' -and $r2Obl -ge 1)
        $script:Evidence.controls['m-overfit'] = @{ task = $t0; result = $mo; receipt_hash = $r2.receipt_hash; gate_state = $r2.gate_state; counts = $r2.counts; head = $r2.head; ok = $r2Ok }
        Save-State
        Add-Step -Name 'o-1 step 1: seeded regression m-overfit (control, not a model)' -Actor 'control-seeded (control identity, not a model)' -Provider 'none' -Result $(if ($r2Ok) { 'pass' } else { 'fail' }) -Evidence @{ task = $t0; receipt_hash = $r2.receipt_hash; gate_state = $r2.gate_state; counts = $r2.counts; result_file = $mo }
        # The seeded regression must be blocked with an obligation_failed item. If it is not, the gate
        # has not caught the regression, so the take stops before the harmless difference and its land.
        if (-not $r2Ok) { throw "m-overfit was not blocked with an obligation_failed item (gate_state '$($r2.gate_state)', obligation_failed $r2Obl, control error '$($r2.error)'); o-1 stops before m-harmless-difference" }

        # ---- step 2: seeded harmless difference m-harmless-difference, rulings, re-gate ----------
        try {
            $view1 = Wait-TaskClaimable -Task $t0 -Token $controlToken -Seconds $ClaimWaitSec
            Add-Step -Name 'o-1: task t-0 is open for the m-harmless claim' -Actor 'script' -Provider 'none' -Result 'pass' -Evidence @{ task = $t0; state = [string]$view1.state; generation = $view1.generation }
        } catch {
            Add-Step -Name 'o-1: task t-0 is open for the m-harmless claim' -Actor 'script' -Provider 'none' -Result 'fail' -Evidence @{ task = $t0; error = $_.Exception.Message }
            throw
        }
        $mh = Join-Path $ctrlRoot 'm-harmless-difference.json'
        $mhFinalPath = Join-Path $ctrlRoot 'm-harmless-final-receipt.json'
        $rulingsOut = Join-Path $ctrlRoot 'rulings-o-1.json'
        $moRulingsOut = Join-Path $ctrlRoot 'rulings-o-1-moverfit.json'
        # The control, the rulings on both receipts, the re-gate and the land: Invoke-ControlWithRuling.
        $mhRun = Invoke-ControlWithRuling -Task $t0 -Bundle (Join-Path $intentDir 'controls\m-harmless-difference.json') -Out $mh -Order $Order -OrderId $oid -RulesFile $RulesFile -PriorReceipt "$mo.receipt.json" -PriorRulingsOut $moRulingsOut -FinalReceipt $mhFinalPath -ControlWaitSec $ControlWaitSec -RulingWaitSec $RulingWaitSec -LandWaitSec $LandWaitSec
        $rulingErr = $mhRun.ruling_error
        if ($mhRun.no_receipt) {
            Add-Step -Name 'o-1 rulings: the control produced no receipt to rule on' -Actor 'script' -Provider 'none' -Result 'fail' -Evidence @{ control_exited = $mhRun.control_exited }
        } elseif ($rulingErr) {
            Add-Step -Name 'o-1 rulings: the owner rules on the out-of-scope episodes (record_ruling, member HTTP)' -Actor 'owner (member HTTP)' -Provider 'none' -Result 'fail' -Evidence @{ error = $rulingErr }
        } else {
            $script:Evidence.rulings[$Order] = @{ log = $rulingsOut; log_moverfit = $moRulingsOut; count = @($mhRun.rulings).Count; ids = @($mhRun.ruling_ids) }
            Add-Step -Name 'o-1 rulings: the owner rules on the out-of-scope episodes of both control receipts (record_ruling, member HTTP)' -Actor "owner (member HTTP; rules file decided by $($mhRun.rulings_decided_by), applied by ruling.ps1)" -Provider 'none' -Result 'pass' -Evidence @{ rulings = @($mhRun.rulings | ForEach-Object { [ordered]@{ ruling_id = $_.ruling_id; episode_id = $_.episode_id; fields = $_.fields; result = $_.result; ledger_seq = $_.ledger_seq } }); log = $rulingsOut; log_moverfit = $moRulingsOut } -Human
            Save-State
        }
        $r3 = $mhRun.control_result
        $ctlKilled = $mhRun.killed_at_wait
        $rulingsOk = $mhRun.rulings_ok
        $regatePassed = $mhRun.regate_passed
        $landState = $mhRun.final_gate_state
        $landedNow = $mhRun.landed
        if ($landedNow) { $script:Evidence.lands['o-1'] = $mhRun.land }
        $script:Evidence.controls['m-harmless-difference'] = @{ task = $t0; result = $mh; receipt_hash = $r3.receipt_hash; gate_state = $r3.gate_state; counts = $r3.counts; head = $r3.head; ruling_error = $rulingErr; rulings_ok = $rulingsOk; receipts_saved = $r3.receipts_saved; killed_at_wait = $ctlKilled; regate_passed = $regatePassed; final_receipt = $mhFinalPath; final_gate_state = $landState; landed = $landedNow; last_reply = $r3.last_reply; control_error = $r3.error }
        Save-State
        Add-Step -Name 'o-1 step 2: seeded harmless difference m-harmless-difference (control): rulings recorded through member HTTP, the re-gate passes and the gate lands o-1' -Actor 'control-seeded (control identity, not a model)' -Provider 'none' -Result $(if ($landedNow) { 'pass' } else { 'fail' }) -Evidence @{ task = $t0; receipt_hash = $r3.receipt_hash; gate_state = $r3.gate_state; final_gate_state = $landState; counts = $r3.counts; rulings_ok = $rulingsOk; killed_at_wait = $ctlKilled; land = $script:Evidence.lands['o-1']; result_file = $mh; final_receipt = $mhFinalPath }

        # o-1's tasks other than t0 were never built here. They end through cancel_order, so no later
        # order's worker can claim them (list_tasks has no order filter).
        $o1Left = Close-OrderLeftovers -Raw $Raw -Tasks $tasks
        Add-Step -Name 'o-1: its leftover tasks end through cancel_order (member route)' -Actor 'owner (member HTTP)' -Provider 'none' -Result 'pass' -Evidence @{ dropped = $o1Left.dropped; reply_events = $o1Left.reply } -Human
        # Worker A is not in this take: the o-1 land is m-harmless-difference's, and worker B builds
        # and lands in o-8.
        $outcome = $(if ($landedNow) { 'o1_landed' } else { 'o1_not_landed' })
    }

    if ($Order -eq 'o-8') {
        # The control and the worker take different tasks of o-8. With one task the control runs first. With
        # more, the worker runs first and takes the task the origin gives it; the control takes another one.
        $lastTask = $tasks[-1]
        $controlFirst = ($tasks.Count -lt 2)
        $ctlTask = $lastTask
        $wbTask = $null
        $viol = Join-Path $ctrlRoot 'm-ruling-violation.json'
        if ($controlFirst) {
            # One task only: the seeded violation runs first; the model worker builds after it releases the task.
            & (Join-Path $intentDir 'control.ps1') -Server $script:Base -Token $controlToken -Repo $script:Repo -Task $ctlTask `
                -Bundle (Join-Path $intentDir 'controls\m-ruling-violation.json') -Out $viol -StageWaitSec 300 -ReceiptWaitSec $ControlWaitSec -RedeclareOnce
        }
        if (-not $SkipWorkers) {
            $wb = Start-Worker -Name 'worker-b' -Token $script:Tokens['worker-b'] -Tag 'o8'
            $events = Invoke-Api -Path '/api/events'
            $wbEvents = Get-AgentEvents -Events $events -Agent 'worker-b'
            $script:Evidence.workers['o-8-worker-b'] = @{ process = $wb; events = $wbEvents }
            Save-State
            $wbTypes = @($wbEvents | ForEach-Object { $_.type } | Select-Object -Unique)
            $wbBuilt = [bool]($wb.exited -and $wb.exit_code -eq 0 -and ($wbTypes -contains 'build_reported' -or $wbTypes -contains 'landed'))
            $wbTask = Get-WorkerTask -WorkerEvents $wbEvents
            if ($wbBuilt -and -not $wbTask) { $wbBuilt = $false }
            # A passing gate lands the candidate on its own; the take waits for the latest receipt.
            $wbFinal = Join-Path $ctrlRoot 'o-8-final-receipt.json'
            $wbState = ''
            if ($wbBuilt) {
                try { $wbState = Wait-TaskLanded -Task $wbTask -Token $controlToken -Path $wbFinal -Seconds $LandWaitSec }
                catch { Write-Output "get_receipt: $($_.Exception.Message)" }
            }
            $wbLanded = [bool]($wbBuilt -and $wbState -eq 'landed' -and (Test-Path -LiteralPath $wbFinal))
            if ($wbLanded) { $script:Evidence.lands['o-8'] = Get-LandCheck -ReceiptFile $wbFinal -What 'o-8' }
            Save-State
            Add-Step -Name 'o-8 step 4: worker B builds and lands Retry-After (model)' -Actor 'worker-b (model worker)' -Provider "$($script:Routes.worker.provider) / $($script:Routes.worker.model)" -Result $(if ($wbLanded) { 'pass' } else { 'fail' }) -Evidence @{ process = $wb; worker_task = $wbTask; worker_finished = (Get-WorkerFinish -Name 'worker-b' -Tag 'o8'); event_types = $wbTypes; events = $wbEvents.Count; final_gate_state = $wbState; final_receipt = $wbFinal; land = $script:Evidence.lands['o-8']; note = 'pass only when the worker exited with code 0 (its build was committed and reported) and the latest receipt is landed with its merged commit equal to the protected head; a worker still running at the wait, or one that ended without a commit, is recorded as fail' }
        } else {
            Add-Step -Name 'o-8 step 4: worker B (skipped by -SkipWorkers)' -Actor 'worker-b (model worker)' -Provider 'none' -Result 'not_run' -Evidence @{ reason = '-SkipWorkers' }
        }
        if (-not $controlFirst) {
            $ctlTask = @($tasks | Where-Object { $_ -ne $wbTask })[-1]
            if (-not $ctlTask) { throw 'the worker took every task of o-8; none is left for m-ruling-violation' }
        }
        if (-not $controlFirst) {
            & (Join-Path $intentDir 'control.ps1') -Server $script:Base -Token $controlToken -Repo $script:Repo -Task $ctlTask `
                -Bundle (Join-Path $intentDir 'controls\m-ruling-violation.json') -Out $viol -StageWaitSec 300 -ReceiptWaitSec $ControlWaitSec -RedeclareOnce
        }
        $r5 = Read-ControlResult -Path $viol
        $violated = @($r5.items | Where-Object { $_.class -eq 'violates_ruling' })
        $r5Ok = ($violated.Count -ge 1)
        $script:Evidence.controls['m-ruling-violation'] = @{ task = $ctlTask; result = $viol; receipt_hash = $r5.receipt_hash; gate_state = $r5.gate_state; counts = $r5.counts; violates_ruling = $violated.Count; ruling_ids = @($violated | ForEach-Object { $_.ruling_id } | Select-Object -Unique); ok = $r5Ok }
        Save-State
        Add-Step -Name 'o-8 step 5: seeded ruling violation m-ruling-violation (control) is blocked by the step 3 ruling' -Actor 'control-seeded (control identity, not a model)' -Provider 'none' -Result $(if ($r5Ok) { 'pass' } else { 'fail' }) -Evidence @{ task = $ctlTask; receipt_hash = $r5.receipt_hash; gate_state = $r5.gate_state; counts = $r5.counts; ruling_ids = $script:Evidence.controls['m-ruling-violation'].ruling_ids; control_first = $controlFirst; result_file = $viol }
        # o-8's leftover tasks end here, so they are not claimed by a later order's worker.
        $o8Left = Close-OrderLeftovers -Raw $Raw -Tasks $tasks
        Add-Step -Name 'o-8: its leftover tasks end through cancel_order (member route)' -Actor 'owner (member HTTP)' -Provider 'none' -Result 'pass' -Evidence @{ dropped = $o8Left.dropped; reply_events = $o8Left.reply } -Human
        $outcome = $(if ($r5Ok) { 'o8_violation_blocked' } else { 'o8_violation_not_blocked' })
    }
}
catch {
    $failure = $_.Exception.Message
    if ($script:DraftReady) {
        # Phase one ends here on purpose: the draft is recorded and nothing was applied.
        $outcome = 'draft_ready'
        Write-Output "DRAFT READY: $failure"
    } else {
        $outcome = 'failed'
        Write-Output "FAILED: $failure"
    }
}
finally {
    if ($script:Proc -and -not $script:Proc.HasExited) {
        Stop-Process -Id $script:Proc.Id -Force -ErrorAction SilentlyContinue
    }
    if (-not (Test-Path -LiteralPath $run)) { New-Item -ItemType Directory -Path $run | Out-Null }
    try { Save-State } catch { Write-Output "state not saved: $($_.Exception.Message)" }
    $demoAfter = $null
    try { $demoAfter = Get-DemoRefs } catch { $demoAfter = "unreadable: $($_.Exception.Message)" }
    $demoUnchanged = if ($null -eq $demoBefore) { 'not_read' } else { ($demoBefore -eq $demoAfter) }

    # Secret scan: the logs and the summary must not hold any token or key value.
    $scanHits = @()
    if (Test-Path -LiteralPath (Join-Path $run 'logs')) {
        foreach ($file in Get-ChildItem -LiteralPath (Join-Path $run 'logs') -File) {
            $text = Get-Content -LiteralPath $file.FullName -Raw -ErrorAction SilentlyContinue
            foreach ($secret in $script:Secrets) {
                if ($text -and $text.Contains($secret)) { $scanHits += $file.Name }
            }
        }
    }

    # Checks. Each one is computed from the evidence of this run, and is not_run until its evidence exists.
    $wB = $script:Evidence.workers['o-8-worker-b']
    $landO1 = if ($script:Evidence.lands.ContainsKey('o-1')) { [bool]$script:Evidence.lands['o-1'].equal } else { 'not_run' }
    $landO8 = if ($script:Evidence.lands.ContainsKey('o-8')) { [bool]$script:Evidence.lands['o-8'].equal } else { 'not_run' }
    # Check 1: a model worker builds and lands an order. Worker B on o-8 is that worker in this take.
    $c1 = if ($null -ne $wB) {
        [ordered]@{ status = $(if ($landO8 -eq $true) { 'pass' } else { 'fail' }); worker_b_lands_o8 = $landO8; worker_b_lease_granted = @($wB.events | Where-Object { $_.type -eq 'lease_granted' }).Count }
    } else { [ordered]@{ status = 'not_run'; worker_b_lands_o8 = $landO8; reason = 'worker B (o-8) has not run in this take; the o-8 draft-only stop does not run it' } }
    $m2 = $script:Evidence.controls['m-overfit']
    $c2 = if ($null -ne $m2) { [ordered]@{ status = $(if ($m2.ok) { 'pass' } else { 'fail' }); receipt_hash = $m2.receipt_hash; gate_state = $m2.gate_state; counts = $m2.counts; result_file = $m2.result } } else { [ordered]@{ status = 'not_run'; reason = 'm-overfit has not run' } }
    $rl = $script:Evidence.rulings['o-1']
    $mhEv = $script:Evidence.controls['m-harmless-difference']
    # Check 3: the rulings recorded through member HTTP, and m-harmless-difference lands o-1 after them.
    $c3 = if ($null -ne $rl -and $null -ne $mhEv) { [ordered]@{ status = $(if ($rl.count -ge 1 -and $mhEv.rulings_ok -and $mhEv.landed) { 'pass' } else { 'fail' }); rulings_recorded = $rl.count; rulings = $rl.ids; rulings_ok = $mhEv.rulings_ok; m_harmless_lands_o1 = $landO1; regate_gate_state = $mhEv.final_gate_state } } else { [ordered]@{ status = 'not_run'; reason = 'no rulings recorded, or m-harmless-difference has not run' } }
    $m8 = $script:Evidence.controls['m-ruling-violation']
    $c4 = if ($null -ne $m8) { [ordered]@{ status = $(if ($m8.ok) { 'pass' } else { 'fail' }); receipt_hash = $m8.receipt_hash; violates_ruling = $m8.violates_ruling; ruling_ids = $m8.ruling_ids } } else { [ordered]@{ status = 'not_run'; reason = 'm-ruling-violation has not run' } }
    $lands = @($script:Evidence.lands.Values)
    $c5 = if ($lands.Count -gt 0) { [ordered]@{ status = $(if (@($lands | Where-Object { -not $_.equal }).Count -eq 0) { 'pass' } else { 'fail' }); lands = $lands } } else { [ordered]@{ status = 'not_run'; reason = 'no land in this take yet' } }
    $c6 = [ordered]@{ status = 'not_run'; reason = 'a clean rerun is a second take folder; compare its summary classes with this one' }
    $c7 = [ordered]@{ status = $(if ($script:HumanSteps -gt 0 -and $script:MemberCalls -ge $script:HumanSteps) { 'partial' } else { 'not_run' }); human_steps = $script:HumanSteps; member_http_calls = $script:MemberCalls; note = 'every human step is a member HTTP call logged in steps; the take does not claim the rerun check' }
    $checks = [ordered]@{
        '1_model_worker_builds_and_lands'   = $c1
        '2_seeded_regression_bounced'       = $c2
        '3_rulings_then_harmless_lands_o1'  = $c3
        '4_second_order_blocked_by_ruling'  = $c4
        '5_protected_head_equals_receipt'   = $c5
        '6_clean_rerun_same_classes'        = $c6
        '7_human_steps_via_member_http'     = $c7
    }
    $script:Summary = [ordered]@{
        schema          = 'acan-take/2'
        label           = $Label
        run             = $run
        order           = $Order
        started         = $started
        finished        = (Get-Date).ToString('o')
        outcome         = $outcome
        failure         = $failure
        repo            = $script:Repo
        pin             = $Pin
        demo            = [ordered]@{ path = $DemoRepo; refs_before = $demoBefore; refs_after = $demoAfter; unchanged = $demoUnchanged }
        providers       = $script:Routes
        server          = [ordered]@{ binary = $AcanExe; worker_binary = $WorkerExe; port = $Port; protected_branch = 'main'; logs = (Join-Path $run 'logs') }
        inputs          = [ordered]@{ edits_file = $EditsFile; edits_sha256 = $(if (Test-Path -LiteralPath $EditsFile) { (Get-FileHash -LiteralPath $EditsFile -Algorithm SHA256).Hash } else { 'none' }); rules_file = $RulesFile; rules_sha256 = (Get-FileHash -LiteralPath $RulesFile -Algorithm SHA256).Hash; max_episodes = $MaxEpisodes }
        steps           = @($script:Steps)
        worker_max_steps = $WorkerMaxSteps
        coordinator_decisions = $coordinatorDecisions
        checks          = $checks
        secret_scan     = $(if ($scanHits.Count -eq 0) { 'clean' } else { "hits: $($scanHits -join ', ')" })
    }
    $summaryPath = Join-Path $run 'summary.json'
    $summaryText = $script:Summary | ConvertTo-Json -Depth 40
    foreach ($secret in $script:Secrets) {
        if ($summaryText.Contains($secret)) { $scanHits += 'summary.json' }
    }
    Set-Content -LiteralPath $summaryPath -Value $summaryText -Encoding utf8
    Write-Output "summary: $summaryPath"
    Write-Output "outcome: $outcome"
    if ($scanHits.Count -gt 0) {
        Write-Output "secret scan found a token or key in: $($scanHits -join ', ')"
        exit 2
    }
}
