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
     then: the seeded regression m-overfit (control), the seeded harmless difference
     m-harmless-difference (control) with its human rulings and re-gate, then worker A (model),
     which builds and lands the order once the control has released the task, and the land check.
  3. -ResumeRun <run> -Order o-8 -DraftOnly, then the same with -EditsFile: worker B (model), then
     the seeded ruling violation m-ruling-violation (control).

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
    [string]$AcanRepo = 'D:\dev\acan-wt\d02',
    [string]$AcanExe = 'C:\cargo-targets\acan-d02\debug\acan.exe',
    [string]$WorkerExe = 'C:\cargo-targets\acan-d02\debug\acan-worker.exe',
    [string]$LlmSource = 'D:\dev\acan\crates\acan-llm\llm.toml',
    [string]$EnvSource = 'D:\dev\acan\crates\acan-llm\.env',
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
    # How long the land check waits for worker A's final receipt to show landed.
    [int]$LandWaitSec = 900,
    [switch]$DraftOnly,
    [string]$ResumeRun = '',
    [switch]$SkipWorkers
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
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
    Write-Output "[$Result] $Name"
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
    Invoke-RestMethod -Uri "$script:Base$Path" -Headers @{ Authorization = "Bearer $script:OwnerToken" } -TimeoutSec 60
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

function Get-AgentEvents {
    param($Events, [string]$Agent)
    @($Events | Where-Object { (ConvertTo-Json -Depth 8 -Compress $_) -like "*`"$Agent`"*" } | ForEach-Object {
            [ordered]@{ seq = $_.seq; type = [string]$_.event.type; task = "$($_.event.task)" }
        })
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
        $script:Repo = Join-Path $run 'repo'
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
    $script:Base = "http://127.0.0.1:$Port"
    $serverArgs = @('serve', '--repo', $script:Repo, '--port', "$Port", '--bind', '127.0.0.1',
        '--protected-branch', 'main', '--llm', $script:LlmPath)
    foreach ($name in $keys.Keys) { Set-Item -Path "Env:$name" -Value $keys[$name] }
    $logTag = if ($ResumeRun) { "resume-$Order-$(Get-Date -Format 'HHmmss')" } else { 'first' }
    try {
        $script:Proc = Start-Process -FilePath $AcanExe -ArgumentList $serverArgs `
            -RedirectStandardOutput (Join-Path $run "logs\server.$logTag.out.log") `
            -RedirectStandardError (Join-Path $run "logs\server.$logTag.err.log") -PassThru -WindowStyle Hidden
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
    Add-Step -Name "start the server on the clone ($logTag)" -Actor 'script' -Provider 'none' -Result 'pass' -Evidence @{ port = $Port; pid = $script:Proc.Id }
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
        $edits = Get-Content -LiteralPath $EditsFile -Raw | ConvertFrom-Json
        if ($edits.schema -ne 'acan-demo-intent-edits/2') {
            throw "the edits file must have schema acan-demo-intent-edits/2, found '$($edits.schema)'"
        }
        if ($edits.order -and $edits.order -ne $Order) { throw "the edits file is for $($edits.order), not $Order" }
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
        $null = Send-Command @{ cmd = 'revise_scope'; order = [int]$raw; scope = $scope }
        Add-Step -Name "${Order}: the owner revises the draft with the logged human edits (revise_scope)" -Actor 'owner (member HTTP)' -Provider 'none' -Result 'pass' -Evidence @{ rev = $d.rev; human_edits = $humanEdits.Count; open_after = @($proposal.open).Count; may_after = @($proposal.may).Count; must_keep_after = @($proposal.must_keep).Count } -Human
        foreach ($entry in $humanEdits) {
            Add-Step -Name "$Order human scope edit: $($entry.op) $($entry.id)" -Actor "human ($(Split-Path -Leaf $EditsFile), applied by the owner through revise_scope)" -Provider 'none' -Result 'pass' -Evidence $entry -Human
        }

        $body = Invoke-Api -Path "/api/orders/$oid/scope/confirmation"
        $offeredCap = [int]$body.budget.max_episodes
        $body.budget.max_episodes = [uint32]([math]::Max($MaxEpisodes, $offeredCap))
        $offeredRounds = [int]$body.budget.repair_rounds
        if ($RepairRounds -gt 0) { $body.budget.repair_rounds = [uint32]$RepairRounds }
        $answers = @()
        foreach ($choice in @($proposal.open)) {
            $fromKeep = @($humanAnswers | Where-Object { $_.choice_id -eq $choice.id })
            if ($fromKeep.Count -eq 1) {
                $answers += @{ choice_id = $choice.id; option = $fromKeep[0].option }
                continue
            }
            $given = @($edits.answers | Where-Object { $_.choice_id -eq $choice.id })
            if ($given.Count -ne 1) {
                throw "open choice $($choice.id) needs exactly one human answer in $EditsFile; options: $((@($choice.options) | ForEach-Object { $_.label }) -join ' | ')"
            }
            $option = @($choice.options | Where-Object { $_.label -eq $given[0].option_label })
            if ($option.Count -ne 1) {
                throw "answer '$($given[0].option_label)' for $($choice.id) matches no option label"
            }
            $answers += @{ choice_id = $choice.id; option = $option[0].key }
        }
        $body.answers = $answers
        $confirmed = Send-Command @{ cmd = 'confirm_scope'; order = [int]$raw; confirmation = $body }
        $scopeConfirmed = @($confirmed | Where-Object { $_.event.type -eq 'scope_confirmed' }).Count
        if ($scopeConfirmed -lt 1) { throw 'confirm_scope returned no scope_confirmed event' }
        Add-Step -Name "${Order}: the owner raises the repair budget's episode cap at confirmation" -Actor 'owner (member HTTP)' -Provider 'none' -Result 'pass' -Evidence @{
            offered_max_episodes = $offeredCap; set_max_episodes = [int]$body.budget.max_episodes
            offered_repair_rounds = $offeredRounds; set_repair_rounds = [int]$body.budget.repair_rounds
            reason = 'every differing field of a route is one discrepancy episode, and the offered cap of four stops the order at the first receipt with more; the owner sets the cap for this order'
        } -Human
        Add-Step -Name "${Order}: the owner confirms the scope (member HTTP)" -Actor 'owner (member HTTP)' -Provider 'none' -Result 'pass' -Evidence @{
            scope_confirmed = $scopeConfirmed; answers = $answers; scope_hash = "$($body.scope_hash)"; budget = $body.budget
        } -Human
        $script:Evidence.orders[$Order] = @{ oid = $oid; raw = $raw; confirmed = $true; scope_hash = "$($body.scope_hash)"; max_episodes = [int]$body.budget.max_episodes }
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
        $mo = Join-Path $ctrlRoot 'm-overfit.json'
        & (Join-Path $intentDir 'control.ps1') -Server $script:Base -Token $controlToken -Repo $script:Repo -Task $t0 `
            -Bundle (Join-Path $intentDir 'controls\m-overfit.json') -Out $mo -StageWaitSec 300 -ReceiptWaitSec $ControlWaitSec
        $r2 = Read-ControlResult -Path $mo
        $r2Obl = @($r2.items | Where-Object { $_.class -eq 'obligation_failed' }).Count
        $r2Ok = ($r2.gate_state -eq 'blocked' -and $r2Obl -ge 1)
        $script:Evidence.controls['m-overfit'] = @{ task = $t0; result = $mo; receipt_hash = $r2.receipt_hash; gate_state = $r2.gate_state; counts = $r2.counts; head = $r2.head; ok = $r2Ok }
        Save-State
        Add-Step -Name 'o-1 step 1: seeded regression m-overfit (control, not a model)' -Actor 'control-seeded (control identity, not a model)' -Provider 'none' -Result $(if ($r2Ok) { 'pass' } else { 'fail' }) -Evidence @{ task = $t0; receipt_hash = $r2.receipt_hash; gate_state = $r2.gate_state; counts = $r2.counts; result_file = $mo }

        # ---- step 2: seeded harmless difference m-harmless-difference, rulings, re-gate ----------
        $mh = Join-Path $ctrlRoot 'm-harmless-difference.json'
        $done = Join-Path $ctrlRoot 'rulings-done.flag'
        $rulingsOut = Join-Path $ctrlRoot 'rulings-o-1.json'
        if (Test-Path -LiteralPath $done) { Remove-Item -LiteralPath $done }
        $ctlArgs = "-NoProfile -File `"$(Join-Path $intentDir 'control.ps1')`" -Server $script:Base -Token $controlToken -Repo `"$script:Repo`" -Task $t0 -Bundle `"$(Join-Path $intentDir 'controls\m-harmless-difference.json')`" -Out `"$mh`" -StageWaitSec 300 -ReceiptWaitSec $ControlWaitSec -AwaitRuling -RulingWaitSec $RulingWaitSec -RulingsDone `"$done`""
        $ctl = Start-Process -FilePath (Get-Command pwsh).Source -ArgumentList $ctlArgs -RedirectStandardOutput "$mh.log" -RedirectStandardError "$mh.err.log" -PassThru -WindowStyle Hidden
        $recEnd = (Get-Date).AddSeconds(300 + $ControlWaitSec)
        while (-not (Test-Path -LiteralPath "$mh.receipt.json")) {
            if ($ctl.HasExited) { break }
            if ((Get-Date) -gt $recEnd) { break }
            Start-Sleep -Seconds 3
        }
        $rulingErr = $null
        if (Test-Path -LiteralPath "$mh.receipt.json") {
            # The control writes the receipt file in one call; give the write time to finish.
            Start-Sleep -Seconds 5
            try {
                & (Join-Path $intentDir 'ruling.ps1') -Server $script:Base -OwnerToken $script:OwnerToken -Order $Order -OrderId $oid `
                    -ReceiptFile "$mh.receipt.json" -RulesFile $RulesFile -Out $rulingsOut
                $rl = Get-Content -LiteralPath $rulingsOut -Raw | ConvertFrom-Json
                $script:Evidence.rulings[$Order] = @{ log = $rulingsOut; count = @($rl.rulings).Count; ids = @($rl.rulings | ForEach-Object { $_.ruling_id }) }
                Add-Step -Name 'o-1 rulings: the owner rules on the out-of-scope episodes (record_ruling, member HTTP)' -Actor 'owner (member HTTP; rules file applied by ruling.ps1)' -Provider 'none' -Result 'pass' -Evidence @{ rulings = @($rl.rulings | ForEach-Object { [ordered]@{ ruling_id = $_.ruling_id; episode_id = $_.episode_id; fields = $_.fields; result = $_.result; ledger_seq = $_.ledger_seq } }); log = $rulingsOut } -Human
                Save-State
            } catch {
                $rulingErr = $_.Exception.Message
                Add-Step -Name 'o-1 rulings: the owner rules on the out-of-scope episodes (record_ruling, member HTTP)' -Actor 'owner (member HTTP)' -Provider 'none' -Result 'fail' -Evidence @{ error = $rulingErr }
            }
        } else {
            Add-Step -Name 'o-1 rulings: the control produced no receipt to rule on' -Actor 'script' -Provider 'none' -Result 'fail' -Evidence @{ control_exited = $ctl.HasExited }
        }
        Set-Content -LiteralPath $done -Value 'rulings finished' -Encoding utf8
        $ctlKilled = $false
        if (-not $ctl.WaitForExit(($RulingWaitSec + 300) * 1000)) {
            # The control saves its result after every step, so its evidence survives the kill. A
            # killed control runs no release, so its claim is not released by this run.
            $ctlKilled = $true
            Stop-Process -Id $ctl.Id -Force -ErrorAction SilentlyContinue
            Write-Output "control m-harmless-difference was killed at its wait; its result so far is $mh"
        }
        $r3 = Read-ControlResult -Path $mh
        $rulingsOk = ($null -eq $rulingErr) -and (Test-Path -LiteralPath $rulingsOut)
        # The re-gate passes when the receipt after the rulings is not blocked: prepared when every
        # item passes. A control that was killed at its wait does not pass.
        $regatePassed = ($null -ne $r3) -and (-not $ctlKilled) -and $rulingsOk -and (@('prepared', 'landed') -contains [string]$r3.gate_state)
        $script:Evidence.controls['m-harmless-difference'] = @{ task = $t0; result = $mh; receipt_hash = $r3.receipt_hash; gate_state = $r3.gate_state; counts = $r3.counts; head = $r3.head; ruling_error = $rulingErr; rulings_ok = $rulingsOk; receipts_saved = $r3.receipts_saved; killed_at_wait = $ctlKilled; regate_passed = $regatePassed; last_reply = $r3.last_reply; control_error = $r3.error }
        Save-State
        Add-Step -Name 'o-1 step 2: seeded harmless difference m-harmless-difference (control): rulings recorded and the re-gate passes (not a land)' -Actor 'control-seeded (control identity, not a model)' -Provider 'none' -Result $(if ($regatePassed) { 'pass' } else { 'fail' }) -Evidence @{ task = $t0; receipt_hash = $r3.receipt_hash; gate_state = $r3.gate_state; counts = $r3.counts; rulings_ok = $rulingsOk; killed_at_wait = $ctlKilled; result_file = $mh }

        # ---- step 3: worker A, model, builds and lands the order once the control released the task ----
        if (-not $SkipWorkers) {
            $wa = Start-Worker -Name 'worker-a' -Token $script:Tokens['worker-a'] -Tag 'o1'
            $events = Invoke-Api -Path '/api/events'
            $waEvents = Get-AgentEvents -Events $events -Agent 'worker-a'
            $script:Evidence.workers['o-1-worker-a'] = @{ process = $wa; events = $waEvents }
            Save-State
            $waTypes = @($waEvents | ForEach-Object { $_.type } | Select-Object -Unique)
            $waBuilt = [bool]($wa.exited -and $wa.exit_code -eq 0 -and ($waTypes -contains 'build_reported' -or $waTypes -contains 'landed'))
            # The gate lands a passing candidate on its own; the latest receipt says whether it did.
            $finalFile = Join-Path $ctrlRoot 'o-1-final-receipt.json'
            $finalState = ''
            $landEnd = (Get-Date).AddSeconds($LandWaitSec)
            while ($waBuilt) {
                try {
                    $finalState = Save-TaskReceipt -Task $t0 -Token $controlToken -Path $finalFile
                } catch {
                    Write-Output "get_receipt: $($_.Exception.Message)"
                    break
                }
                if (@('landed', 'blocked', 'stopped') -contains $finalState) { break }
                if ((Get-Date) -gt $landEnd) { break }
                Start-Sleep -Seconds 10
            }
            $landedNow = [bool]($waBuilt -and $finalState -eq 'landed' -and (Test-Path -LiteralPath $finalFile))
            if ($landedNow) {
                $script:Evidence.lands['o-1'] = Get-LandCheck -ReceiptFile $finalFile -What 'o-1'
            }
            Save-State
            Add-Step -Name 'o-1 step 3: worker A builds and lands the order (honest attempt, model)' -Actor 'worker-a (model worker)' -Provider "$($script:Routes.worker.provider) / $($script:Routes.worker.model)" -Result $(if ($landedNow) { 'pass' } else { 'fail' }) -Evidence @{ process = $wa; worker_finished = (Get-WorkerFinish -Name 'worker-a' -Tag 'o1'); event_types = $waTypes; events = $waEvents.Count; final_gate_state = $finalState; final_receipt = $finalFile; land = $script:Evidence.lands['o-1']; note = 'pass only when the worker exited with code 0 after a build was reported, and the latest receipt is landed with its merged commit equal to the protected head; a worker still running at the wait, or one that ended without a commit, is recorded as fail' }
        } else {
            Add-Step -Name 'o-1 step 3: worker A (skipped by -SkipWorkers)' -Actor 'worker-a (model worker)' -Provider 'none' -Result 'not_run' -Evidence @{ reason = '-SkipWorkers' }
            $landedNow = $false
        }
        $outcome = $(if ($landedNow) { 'o1_landed' } else { 'o1_not_landed' })
    }

    if ($Order -eq 'o-8') {
        $lastTask = $tasks[-1]
        $controlFirst = ($tasks.Count -lt 2)
        $viol = Join-Path $ctrlRoot 'm-ruling-violation.json'
        if ($controlFirst) {
            # One task only: the seeded violation runs first; the model worker builds after it releases the task.
            & (Join-Path $intentDir 'control.ps1') -Server $script:Base -Token $controlToken -Repo $script:Repo -Task $lastTask `
                -Bundle (Join-Path $intentDir 'controls\m-ruling-violation.json') -Out $viol -StageWaitSec 300 -ReceiptWaitSec $ControlWaitSec
        }
        if (-not $SkipWorkers) {
            $wb = Start-Worker -Name 'worker-b' -Token $script:Tokens['worker-b'] -Tag 'o8'
            $events = Invoke-Api -Path '/api/events'
            $wbEvents = Get-AgentEvents -Events $events -Agent 'worker-b'
            $script:Evidence.workers['o-8-worker-b'] = @{ process = $wb; events = $wbEvents }
            Save-State
            $wbTypes = @($wbEvents | ForEach-Object { $_.type } | Select-Object -Unique)
            Add-Step -Name 'o-8 step 4: worker B builds Retry-After (model)' -Actor 'worker-b (model worker)' -Provider "$($script:Routes.worker.provider) / $($script:Routes.worker.model)" -Result $(if ($wb.exited -and $wb.exit_code -eq 0 -and ($wbTypes -contains 'build_reported' -or $wbTypes -contains 'landed')) { 'pass' } else { 'fail' }) -Evidence @{ process = $wb; worker_finished = (Get-WorkerFinish -Name 'worker-b' -Tag 'o8'); event_types = $wbTypes; events = $wbEvents.Count; note = 'pass only when the worker exited with code 0 (its build was committed and reported) and a build was reported; a worker still running at the wait, or one that ended without a commit, is recorded as fail' }
        } else {
            Add-Step -Name 'o-8 step 4: worker B (skipped by -SkipWorkers)' -Actor 'worker-b (model worker)' -Provider 'none' -Result 'not_run' -Evidence @{ reason = '-SkipWorkers' }
        }
        if (-not $controlFirst) {
            & (Join-Path $intentDir 'control.ps1') -Server $script:Base -Token $controlToken -Repo $script:Repo -Task $lastTask `
                -Bundle (Join-Path $intentDir 'controls\m-ruling-violation.json') -Out $viol -StageWaitSec 300 -ReceiptWaitSec $ControlWaitSec
        }
        $r5 = Read-ControlResult -Path $viol
        $violated = @($r5.items | Where-Object { $_.class -eq 'violates_ruling' })
        $r5Ok = ($violated.Count -ge 1)
        $script:Evidence.controls['m-ruling-violation'] = @{ task = $lastTask; result = $viol; receipt_hash = $r5.receipt_hash; gate_state = $r5.gate_state; counts = $r5.counts; violates_ruling = $violated.Count; ruling_ids = @($violated | ForEach-Object { $_.ruling_id } | Select-Object -Unique); ok = $r5Ok }
        Save-State
        Add-Step -Name 'o-8 step 5: seeded ruling violation m-ruling-violation (control) is blocked by the step 3 ruling' -Actor 'control-seeded (control identity, not a model)' -Provider 'none' -Result $(if ($r5Ok) { 'pass' } else { 'fail' }) -Evidence @{ task = $lastTask; receipt_hash = $r5.receipt_hash; gate_state = $r5.gate_state; counts = $r5.counts; ruling_ids = $script:Evidence.controls['m-ruling-violation'].ruling_ids; control_first = $controlFirst; result_file = $viol }
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
    $wA = $script:Evidence.workers['o-1-worker-a']
    $wB = $script:Evidence.workers['o-8-worker-b']
    $agentsClaimed = @()
    foreach ($ev in @($wA, $wB)) {
        if ($null -eq $ev) { continue }
        foreach ($e in @($ev.events)) { if ($e.type -eq 'lease_granted') { $agentsClaimed += $e } }
    }
    $claimantIds = @($agentsClaimed | ForEach-Object { $_.task } | Select-Object -Unique)
    # Part A is worker A landing o-1; the status below is the lease check for workers A and B.
    $partA = if ($script:Evidence.lands.ContainsKey('o-1')) { [bool]$script:Evidence.lands['o-1'].equal } else { 'not_run' }
    $c1 = if ($null -ne $wA -and $null -ne $wB) {
        $aClaim = @($wA.events | Where-Object { $_.type -eq 'lease_granted' }).Count
        $bClaim = @($wB.events | Where-Object { $_.type -eq 'lease_granted' }).Count
        [ordered]@{ status = $(if ($aClaim -ge 1 -and $bClaim -ge 1) { 'pass' } else { 'fail' }); part_a_worker_a_lands_o1 = $partA; worker_a_lease_granted = $aClaim; worker_b_lease_granted = $bClaim; ids = @('worker-a', 'worker-b') }
    } else { [ordered]@{ status = 'not_run'; part_a_worker_a_lands_o1 = $partA; reason = 'worker A and worker B have not both run in this take' } }
    $m2 = $script:Evidence.controls['m-overfit']
    $c2 = if ($null -ne $m2) { [ordered]@{ status = $(if ($m2.ok) { 'pass' } else { 'fail' }); receipt_hash = $m2.receipt_hash; gate_state = $m2.gate_state; counts = $m2.counts; result_file = $m2.result } } else { [ordered]@{ status = 'not_run'; reason = 'm-overfit has not run' } }
    $rl = $script:Evidence.rulings['o-1']
    $mhEv = $script:Evidence.controls['m-harmless-difference']
    # Check 3 is the rulings and the re-gate, not a land: the land is worker A's, in check 1 part A.
    $c3 = if ($null -ne $rl -and $null -ne $mhEv) { [ordered]@{ status = $(if ($rl.count -ge 1 -and $mhEv.regate_passed) { 'pass' } else { 'fail' }); rulings = $rl.ids; regate_gate_state = $mhEv.gate_state; regate_passed = $mhEv.regate_passed } } else { [ordered]@{ status = 'not_run'; reason = 'no rulings recorded, or m-harmless-difference has not run' } }
    $m8 = $script:Evidence.controls['m-ruling-violation']
    $c4 = if ($null -ne $m8) { [ordered]@{ status = $(if ($m8.ok) { 'pass' } else { 'fail' }); receipt_hash = $m8.receipt_hash; violates_ruling = $m8.violates_ruling; ruling_ids = $m8.ruling_ids } } else { [ordered]@{ status = 'not_run'; reason = 'm-ruling-violation has not run' } }
    $lands = @($script:Evidence.lands.Values)
    $c5 = if ($lands.Count -gt 0) { [ordered]@{ status = $(if (@($lands | Where-Object { -not $_.equal }).Count -eq 0) { 'pass' } else { 'fail' }); lands = $lands } } else { [ordered]@{ status = 'not_run'; reason = 'no land in this take yet' } }
    $c6 = [ordered]@{ status = 'not_run'; reason = 'a clean rerun is a second take folder; compare its summary classes with this one' }
    $c7 = [ordered]@{ status = $(if ($script:HumanSteps -gt 0 -and $script:MemberCalls -ge $script:HumanSteps) { 'partial' } else { 'not_run' }); human_steps = $script:HumanSteps; member_http_calls = $script:MemberCalls; note = 'every human step is a member HTTP call logged in steps; the take does not claim the rerun check' }
    $checks = [ordered]@{
        '1_two_worker_identities'           = $c1
        '2_seeded_regression_bounced'       = $c2
        '3_real_ruling_through_member_http' = $c3
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
