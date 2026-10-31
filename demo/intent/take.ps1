<#
.SYNOPSIS
One intent take: the real Acan server on a clean, disposable clone of the acan-demo repository,
driven only through the member HTTP API, with live Meta models.

.DESCRIPTION
Every run gets its own folder under -RunsRoot. The clone is the only git repository the script
runs git in. Its top level is checked after the clone and before every git call, so a clone
that failed can never send git up to a parent repository.

Keys are copied from crates/acan-llm/.env into the run folder and handed to the server through
its environment only. They are never printed, never written to summary.json, and never put in
the clone. The owner and agent tokens live in the clone's ignored .acan/tokens.json.

There is no offline mode. Each model call is a live call through the provider the routing file
names, and summary.json records that provider for every step.

Steps that are not built yet are written as not_run with the reason. They are never reported as
passed. summary.json is written even when the take fails.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$Label,
    [int]$Port = 7411,
    [string]$RunsRoot = 'D:\dev\acan-takes',
    [string]$DemoRepo = 'D:\dev\acan-demo',
    [string]$Pin = 'f91962156841332d5753d869554839f7c4cdea79',
    [string]$AcanRepo = 'D:\dev\acan-wt\d02',
    [string]$AcanExe = 'C:\cargo-targets\acan-d02\debug\acan.exe',
    [string]$LlmSource = 'D:\dev\acan\crates\acan-llm\llm.toml',
    [string]$EnvSource = 'D:\dev\acan\crates\acan-llm\.env',
    [string]$EditsFile = '',
    [int]$HealthWaitSec = 90,
    [int]$StepWaitSec = 900,
    # Phase one: stop right after the foreman's draft, so a person can read it and write the edits
    # for that draft. The draft is kept in the run folder and nothing is applied.
    [switch]$DraftOnly,
    # Phase two: resume a run folder from phase one. Its clone, database and draft are reused; the
    # draft is read back from the server, not drafted again.
    [string]$ResumeRun = ''
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
if (-not $EditsFile) { $EditsFile = Join-Path $AcanRepo 'demo\intent\o-1-edits.json' }
if ($DraftOnly -and $ResumeRun) { throw 'use either -DraftOnly or -ResumeRun, not both' }

$script:Steps = [System.Collections.Generic.List[object]]::new()
$script:Secrets = [System.Collections.Generic.List[string]]::new()
$script:Proc = $null
$script:DraftReady = $false
$script:Base = ''
$script:Repo = ''
$script:Summary = $null
$demoBefore = $null
$started = (Get-Date).ToString('o')
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$run = Join-Path $RunsRoot "$Label-$stamp"
$outcome = 'incomplete'
$failure = $null

function Add-Step {
    param([string]$Name, [string]$Actor, [string]$Provider, [string]$Result, [hashtable]$Evidence = @{})
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

try {
    # ---- preflight -----------------------------------------------------------------------
    $freeGb = [math]::Round((Get-PSDrive C).Free / 1GB, 1)
    if ($freeGb -lt 5) { throw "C: has $freeGb GB free; the take stops below 5 GB" }
    docker info *> $null
    if ($LASTEXITCODE -ne 0) { throw 'Docker is not running; start Docker Desktop before the take' }
    foreach ($path in @($AcanExe, $LlmSource, $EnvSource, $EditsFile, (Join-Path $AcanRepo 'demo\intent\orders.json'))) {
        if (-not (Test-Path -LiteralPath $path)) { throw "missing input: $path" }
    }
    Assert-OutsideGit -Path $RunsRoot
    $demoBefore = Get-DemoRefs

    # ---- run folder and clone -----------------------------------------------------------
    # Nothing is created inside the clone before git clone succeeds. The clone is the only repo.
    if ($ResumeRun) {
        $run = (Resolve-Path -LiteralPath $ResumeRun).Path
        $script:Repo = Join-Path $run 'repo'
        if (-not (Test-Path -LiteralPath (Join-Path $script:Repo '.git'))) { throw "resume: no clone in $run" }
        Assert-OwnRepo
        $head = (Invoke-Git rev-parse HEAD | Out-String).Trim()
        if ($head -ne $Pin) { throw "the resumed clone's HEAD is $head, not the pin $Pin" }
        Add-Step -Name 'resume the run on its clone at the pin' -Actor 'script' -Provider 'none' -Result 'pass' -Evidence @{ repo = $script:Repo; head = $head; resumed_from = $run }
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

    # ---- keys, routing and tokens: outside the repository except the ignored tokens file ----
    $keysDir = Join-Path $run 'keys'
    $configDir = Join-Path $run 'config'
    New-Item -ItemType Directory -Path $keysDir, $configDir, (Join-Path $run 'logs') | Out-Null
    Copy-Item -LiteralPath $EnvSource -Destination (Join-Path $keysDir '.env')
    Copy-Item -LiteralPath $LlmSource -Destination (Join-Path $configDir 'llm.toml')
    $keys = Read-KeyFile -Path (Join-Path $keysDir '.env')
    foreach ($value in $keys.Values) {
        if ($value.Length -ge 8) { $script:Secrets.Add($value) }
    }
    $routes = Read-Routing -Text (Get-Content -LiteralPath (Join-Path $configDir 'llm.toml') -Raw)
    $script:OwnerToken = 'acan_owner_' + [guid]::NewGuid().ToString('N')
    $agentA = 'acan_agent_a_' + [guid]::NewGuid().ToString('N')
    $control = 'acan_agent_c_' + [guid]::NewGuid().ToString('N')
    $agentB = 'acan_agent_b_' + [guid]::NewGuid().ToString('N')
    $script:Secrets.Add($script:OwnerToken)
    $script:Secrets.Add($agentA)
    $script:Secrets.Add($control)
    $script:Secrets.Add($agentB)
    $tokens = @(
        @{ token = $script:OwnerToken; principal = @{ kind = 'member'; id = 'owner' } },
        @{ token = $agentA; principal = @{ kind = 'agent'; id = 'worker-a' } },
        @{ token = $control; principal = @{ kind = 'agent'; id = 'control-seeded' } },
        @{ token = $agentB; principal = @{ kind = 'agent'; id = 'worker-b' } }
    )
    Set-Content -LiteralPath (Join-Path $script:Repo '.acan\tokens.json') -Value (ConvertTo-Json -Depth 5 $tokens) -Encoding utf8
    Add-Step -Name 'write the run configuration' -Actor 'script' -Provider 'none' -Result 'pass' -Evidence @{
        keys_loaded = @($keys.Keys); routing = $routes; tokens = 'written to .acan/tokens.json (ignored), not printed'
    }
    }
    if ($ResumeRun) {
        # The resumed run keeps its own keys, routing and tokens; nothing is copied or rewritten.
        $keysDir = Join-Path $run 'keys'
        $configDir = Join-Path $run 'config'
        $keys = Read-KeyFile -Path (Join-Path $keysDir '.env')
        foreach ($value in $keys.Values) {
            if ($value.Length -ge 8) { $script:Secrets.Add($value) }
        }
        $routes = Read-Routing -Text (Get-Content -LiteralPath (Join-Path $configDir 'llm.toml') -Raw)
        $existingTokens = Get-Content -LiteralPath (Join-Path $script:Repo '.acan\tokens.json') -Raw | ConvertFrom-Json
        $script:OwnerToken = [string](@($existingTokens | Where-Object { $_.principal.kind -eq 'member' })[0].token)
        $agentA = [string](@($existingTokens | Where-Object { $_.principal.id -eq 'worker-a' })[0].token)
        $control = [string](@($existingTokens | Where-Object { $_.principal.id -eq 'control-seeded' })[0].token)
        $agentB = [string](@($existingTokens | Where-Object { $_.principal.id -eq 'worker-b' })[0].token)
        foreach ($tok in @($script:OwnerToken, $agentA, $control, $agentB)) { $script:Secrets.Add($tok) }
        Add-Step -Name 'reuse the run configuration (resume)' -Actor 'script' -Provider 'none' -Result 'pass' -Evidence @{
            keys_loaded = @($keys.Keys); routing = $routes; tokens = 'read from the clone, not printed'
        }
    }

    # ---- server ----------------------------------------------------------------------------
    $script:Base = "http://127.0.0.1:$Port"
    $serverArgs = @('serve', '--repo', $script:Repo, '--port', "$Port", '--bind', '127.0.0.1',
        '--protected-branch', 'main', '--llm', (Join-Path $configDir 'llm.toml'))
    foreach ($name in $keys.Keys) { Set-Item -Path "Env:$name" -Value $keys[$name] }
    $logTag = if ($ResumeRun) { 'resume' } else { 'first' }
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
    Add-Step -Name 'start the server on the clone' -Actor 'script' -Provider 'none' -Result 'pass' -Evidence @{ port = $Port; pid = $script:Proc.Id }

    # ---- order o-1: post, split, draft, adopt, answer, confirm -------------------------------
    $orders = Get-Content -LiteralPath (Join-Path $AcanRepo 'demo\intent\orders.json') -Raw | ConvertFrom-Json
    $o1 = $orders.orders | Where-Object { $_.id -eq 'o-1' }
    if ($ResumeRun) {
        $state = Get-Content -LiteralPath (Join-Path $run 'run-state.json') -Raw | ConvertFrom-Json
        $raw = [string]$state.raw
        $oid = [string]$state.oid
        Add-Step -Name 'resume: the order posted in phase one is read back' -Actor 'script' -Provider 'none' -Result 'pass' -Evidence @{ order = $oid; machine = $state.machine }
    } else {
        $posted = Send-Command @{ cmd = 'post_order'; machine = $o1.machine; sentence = $o1.sentence; witnesses = @() }
        $postedEvent = $posted | Where-Object { $_.event.type -eq 'order_posted' } | Select-Object -First 1
        if (-not $postedEvent) { throw 'post_order returned no order_posted event' }
        $raw = "$($postedEvent.event.order)"
        $oid = if ($raw -like 'o-*') { $raw } else { "o-$raw" }
        Set-Content -LiteralPath (Join-Path $run 'run-state.json') -Value (ConvertTo-Json @{ raw = $raw; oid = $oid; machine = $o1.machine }) -Encoding utf8
        Add-Step -Name 'post order o-1' -Actor 'owner (member HTTP)' -Provider 'none' -Result 'pass' -Evidence @{ order = $oid; machine = $o1.machine }

        $null = Wait-Until -What 'the order split' -Probe {
            $events = Invoke-Api -Path '/api/events'
            $events | Where-Object { $_.event.type -eq 'order_split' -and "$($_.event.order)" -eq $raw } | Select-Object -First 1
        } -Terminal { $null }
        Add-Step -Name 'split the order into tasks' -Actor 'foreman' -Provider "$($routes.foreman.provider) / $($routes.foreman.model)" -Result 'pass'
    }

    $view = Wait-Until -What 'the foreman draft' -Probe { Invoke-Api404Tolerant -Path "/api/orders/$oid/scope-draft" } -Terminal { $null }
    if ($view.state -ne 'drafted') { throw "the foreman draft did not check: $($view.reason | ConvertTo-Json -Compress)" }
    $d = $view.draft
    $proposal = $d.draft.proposal
    # The model's draft as the foreman returned it, kept in the run folder for review. It holds no key.
    Set-Content -LiteralPath (Join-Path $run 'scope-draft.json') -Value ($d | ConvertTo-Json -Depth 60) -Encoding utf8
    Add-Step -Name 'foreman drafts the scope' -Actor 'foreman' -Provider "$($routes.foreman.provider) / $($d.draft.model)" -Result 'pass' -Evidence @{
        provenance = $d.draft.provenance
        rev = $d.rev
        may = @($proposal.may).Count
        must_keep = @($proposal.must_keep).Count
        open = $proposal.open
    }
    if ($DraftOnly) {
        $script:DraftReady = $true
        throw "draft recorded for human review; write the edits for this draft in $EditsFile, then resume with -ResumeRun $run"
    }

    # Human edits apply to the draft before the one revise_scope, so the scope the owner confirms is
    # the draft plus the logged edits. Each edit is logged as a human step; an unknown op stops the take.
    $edits = Get-Content -LiteralPath $EditsFile -Raw | ConvertFrom-Json
    if ($edits.schema -ne 'acan-demo-intent-edits/2') {
        throw "the edits file must have schema acan-demo-intent-edits/2, found '$($edits.schema)'"
    }
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

    # Choice rules match an open choice by its question text, because the foreman's question ids and
    # wording change between drafts. A choice matched by no rule is left for an explicit answer below.
    foreach ($choice in @($proposal.open)) {
        $matched = @($edits.choice_rules | Where-Object { $choice.question -match $_.question_match })
        if ($matched.Count -eq 0) { continue }
        if ($matched.Count -gt 1) { throw "more than one choice rule matches $($choice.id): $((@($matched) | ForEach-Object { $_.id }) -join ', ')" }
        $rule = $matched[0]
        $keep = @($choice.options | Where-Object { $_.label -match $rule.option_label_match })
        if ($keep.Count -gt 1) { throw "choice rule $($rule.id) matches more than one option of $($choice.id)" }
        if ($keep.Count -eq 1) {
            $humanAnswers.Add(@{ choice_id = $choice.id; option = $keep[0].key })
            $humanEdits.Add(@{ op = 'answer_choice'; id = $choice.id; rule = $rule.id; option_label = $keep[0].label; result = 'answered; choice stays open until the answer is confirmed'; reason = $rule.reason })
        } elseif ($rule.when_no_option_matches -eq 'remove') {
            $proposal.open = @($proposal.open | Where-Object { $_.id -ne $choice.id })
            $humanEdits.Add(@{ op = 'remove_open'; id = $choice.id; rule = $rule.id; question = $choice.question; result = 'removed'; reason = $rule.reason })
        } else {
            throw "choice $($choice.id) matches rule $($rule.id) but no option matches '$($rule.option_label_match)': $((@($choice.options) | ForEach-Object { $_.label }) -join ' | ')"
        }
    }

    $scope = @{ version = 'acan-scope/1'; order = $d.order; rev = $d.rev; base_commit = $d.base_commit; body = $proposal }
    $null = Send-Command @{ cmd = 'revise_scope'; order = [int]$raw; scope = $scope }
    Add-Step -Name 'owner revises the draft with the logged human edits (revise_scope)' -Actor 'owner (member HTTP)' -Provider 'none' -Result 'pass' -Evidence @{ rev = $d.rev; human_edits = $humanEdits.Count; open_after = @($proposal.open).Count; may_after = @($proposal.may).Count; must_keep_after = @($proposal.must_keep).Count }
    foreach ($entry in $humanEdits) {
        Add-Step -Name "human scope edit: $($entry.op) $($entry.id)" -Actor 'human (o-1-edits.json, applied by the owner through revise_scope)' -Provider 'none' -Result 'pass' -Evidence $entry
    }

    $body = Invoke-Api -Path "/api/orders/$oid/scope/confirmation"
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
            throw "answer '$($given[0].option_label)' for $($choice.id) matches no option label; options: $((@($choice.options) | ForEach-Object { $_.label }) -join ' | ')"
        }
        $answers += @{ choice_id = $choice.id; option = $option[0].key }
    }
    $body.answers = $answers
    $confirmed = Send-Command @{ cmd = 'confirm_scope'; order = [int]$raw; confirmation = $body }
    $scopeConfirmed = @($confirmed | Where-Object { $_.event.type -eq 'scope_confirmed' }).Count
    if ($scopeConfirmed -lt 1) { throw 'confirm_scope returned no scope_confirmed event' }
    Add-Step -Name 'owner confirms the scope (member HTTP)' -Actor 'owner (member HTTP)' -Provider 'none' -Result 'pass' -Evidence @{
        scope_confirmed = $scopeConfirmed
        answers = $answers
        scope_hash = "$($body.scope_hash)"
    }

    # Refusals: an agent token must not reach the member routes.
    $agentHeaders = @{ Authorization = "Bearer $agentA" }
    $confirmStatus = Test-Forbidden -Call { Invoke-RestMethod -Uri "$script:Base/api/orders/$oid/scope/confirmation" -Headers $agentHeaders }
    # A well-formed command, so the request reaches the member check and not the body parser.
    $commandStatus = Test-Forbidden -Call { Invoke-RestMethod -Method Post -Uri "$script:Base/api/commands" -Headers $agentHeaders -ContentType 'application/json' -Body '{"cmd":"cancel_order","order":1}' }
    Add-Step -Name 'agent token refused on member routes' -Actor 'worker-a token' -Provider 'none' -Result $(if ($confirmStatus -eq 403 -and $commandStatus -eq 403) { 'pass' } else { 'fail' }) -Evidence @{
        confirmation_status = $confirmStatus
        command_status = $commandStatus
    }

    $outcome = 'stopped_before_workers'
    $failure = 'the workers, the gate, the land and the ruling are not part of this take yet; see checks'
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
    $demoAfter = $null
    try { $demoAfter = Get-DemoRefs } catch { $demoAfter = "unreadable: $($_.Exception.Message)" }
    $demoUnchanged = if ($null -eq $demoBefore) { 'not_read' } else { ($demoBefore -eq $demoAfter) }
    if (-not (Test-Path -LiteralPath $run)) { New-Item -ItemType Directory -Path $run | Out-Null }

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

    $checks = [ordered]@{
        '1_two_worker_identities'           = @{ status = 'not_run'; reason = 'worker phase not built in this take' }
        '2_seeded_regression_bounced'       = @{ status = 'not_run'; reason = 'worker and control phase not built in this take' }
        '3_real_ruling_through_member_http' = @{ status = 'not_run'; reason = 'ruling-draft route is not built: contrast examples are not retained (see report)' }
        '4_second_order_blocked_by_ruling'  = @{ status = 'not_run'; reason = 'depends on check 3' }
        '5_protected_head_equals_receipt'   = @{ status = 'not_run'; reason = 'no land in this take' }
        '6_clean_rerun_same_classes'        = @{ status = 'not_run'; reason = 'one take has run; no rerun yet' }
        '7_human_steps_via_member_http'     = @{ status = $(if ($outcome -eq 'failed') { 'fail' } else { 'partial' }); reason = 'o-1 steps so far: post, adopt, confirm, refusals' }
    }
    $script:Summary = [ordered]@{
        schema          = 'acan-take/1'
        label           = $Label
        run             = $run
        started         = $started
        finished        = (Get-Date).ToString('o')
        outcome         = $outcome
        failure         = $failure
        complete        = $false
        repo            = $script:Repo
        pin             = $Pin
        demo            = [ordered]@{ path = $DemoRepo; refs_before = $demoBefore; refs_after = $demoAfter; unchanged = $demoUnchanged }
        providers       = $routes
        server          = [ordered]@{ binary = $AcanExe; port = $Port; protected_branch = 'main'; logs = (Join-Path $run 'logs') }
        inputs          = [ordered]@{ edits_file = $EditsFile; edits_sha256 = (Get-FileHash -LiteralPath $EditsFile -Algorithm SHA256).Hash }
        steps           = $script:Steps
        checks          = $checks
        secret_scan     = $(if ($scanHits.Count -eq 0) { 'clean' } else { "hits: $($scanHits -join ', ')" })
    }
    $summaryPath = Join-Path $run 'summary.json'
    # A resumed run keeps the summary of the run it resumes, so no earlier step is lost.
    if (Test-Path -LiteralPath $summaryPath) {
        Move-Item -LiteralPath $summaryPath -Destination (Join-Path $run ("summary.prev-" + (Get-Date -Format 'yyyyMMdd-HHmmss') + '.json'))
    }
    $summaryText = $script:Summary | ConvertTo-Json -Depth 30
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
