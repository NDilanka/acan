<#
.SYNOPSIS
One control run in a take: the control identity claims one task over MCP, writes a seeded bundle
into the task's worktree, reports the build, and records the gate's receipt.

.DESCRIPTION
A control is not a model. Its code is the bundle in demo/intent/controls/<id>.json: exact edits
applied to the pinned version of each file, so the candidate does not depend on what has already
landed. The control drives the same MCP tools a worker uses (get_scope, post_scoped_readback,
claim_task, declare_changes, get_task, get_worktree, report_built, get_receipt, release_task), with
its own agent token. Every call is recorded in the result file, and the token is never printed.

Waits are bounded: each wait ends on a terminal state or on its timeout, whichever comes first.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Server,
    [Parameter(Mandatory)][string]$Token,
    [Parameter(Mandatory)][string]$Repo,
    [Parameter(Mandatory)][string]$Task,
    [Parameter(Mandatory)][string]$Bundle,
    [Parameter(Mandatory)][string]$Out,
    [string]$Summary = '',
    [int]$StageWaitSec = 300,
    [int]$ReceiptWaitSec = 900,
    # Hold the claim after a receipt that a human ruling can change, until the gate lands it.
    [switch]$AwaitRuling,
    [int]$RulingWaitSec = 1200,
    # A file the take writes once the rulings are recorded; the control then asks for a fresh receipt.
    [string]$RulingsDone = '',
    [string]$Pin = 'f91962156841332d5753d869554839f7c4cdea79'
)

$ErrorActionPreference = 'Stop'
$mcpUrl = "$($Server.TrimEnd('/'))/mcp"
$authHeaders = @{ Authorization = "Bearer $Token" }
$script:RpcId = 0
$steps = [System.Collections.Generic.List[object]]::new()
$result = [ordered]@{
    schema     = 'acan-control-run/1'
    actor      = 'control identity (control-seeded agent token), not a model'
    task       = $Task
    bundle     = $Bundle
    started    = (Get-Date).ToString('o')
}

function Add-Step {
    param([string]$Name, [bool]$Ok, $Evidence = $null)
    $steps.Add([ordered]@{
            at       = (Get-Date).ToString('o')
            step     = $Name
            result   = $(if ($Ok) { 'pass' } else { 'fail' })
            evidence = $Evidence
        })
    Write-Output "[$(if ($Ok) { 'pass' } else { 'fail' })] $Name"
}

function Post-Mcp {
    param([hashtable]$Body)
    $json = ConvertTo-Json -Depth 60 -Compress $Body
    Invoke-RestMethod -Method Post -Uri $mcpUrl -Headers $authHeaders -ContentType 'application/json' -Body $json -TimeoutSec 120
}

function Call-Tool {
    param([string]$Name, [hashtable]$Arguments)
    $script:RpcId++
    try {
        $resp = Post-Mcp @{ jsonrpc = '2.0'; id = $script:RpcId; method = 'tools/call'; params = @{ name = $Name; arguments = $Arguments } }
    } catch {
        return [pscustomobject]@{ ok = $false; text = "transport: $($_.Exception.Message)"; value = $null }
    }
    if ($resp.error) {
        return [pscustomobject]@{ ok = $false; text = [string]$resp.error.message; value = $null }
    }
    $text = [string]$resp.result.content[0].text
    if ($resp.result.isError) {
        return [pscustomobject]@{ ok = $false; text = $text; value = $null }
    }
    $value = $null
    try { $value = $text | ConvertFrom-Json -Depth 60 } catch { $value = $text }
    [pscustomobject]@{ ok = $true; text = $text; value = $value }
}

function Read-PinnedFile {
    param([string]$Path)
    $lines = & git -C $Repo show "${Pin}:$Path"
    if ($LASTEXITCODE -ne 0) { throw "git show ${Pin}:$Path failed" }
    [string]::Join("`n", $lines) + "`n"
}

function Apply-Edits {
    param([string]$Text, $Edits, [string]$Path)
    foreach ($edit in @($Edits)) {
        $at = $Text.IndexOf([string]$edit.old, [StringComparison]::Ordinal)
        if ($at -lt 0) { throw "edit did not match in ${Path}: $(([string]$edit.old).Substring(0, [Math]::Min(60, ([string]$edit.old).Length)))" }
        if ($Text.IndexOf([string]$edit.old, $at + 1, [StringComparison]::Ordinal) -ge 0) { throw "edit matches more than once in $Path" }
        $Text = $Text.Substring(0, $at) + [string]$edit.new + $Text.Substring($at + ([string]$edit.old).Length)
    }
    $Text
}

function Write-Result {
    $result['finished'] = (Get-Date).ToString('o')
    $result['steps'] = $steps
    $result | ConvertTo-Json -Depth 60 | Set-Content -LiteralPath $Out -Encoding utf8
}

$bundleObj = Get-Content -LiteralPath $Bundle -Raw | ConvertFrom-Json
$result['control'] = [string]$bundleObj.id
$result['applies_to'] = [string]$bundleObj.applies_to
$result['base'] = [string]$bundleObj.base
$claimed = $false
$generation = $null
try {
    # ---- session -----------------------------------------------------------------------------
    $init = Post-Mcp @{ jsonrpc = '2.0'; id = 0; method = 'initialize'; params = @{ protocolVersion = '2025-06-18'; capabilities = @{}; clientInfo = @{ name = 'acan-control'; version = '1' } } }
    Add-Step -Name 'control connects over MCP' -Ok ([bool]$init.result.protocolVersion) -Evidence @{ protocol = [string]$init.result.protocolVersion }
    $null = Post-Mcp @{ jsonrpc = '2.0'; method = 'notifications/initialized' }

    # ---- readback: the control's own predictions, from the confirmed scope ---------------
    $sc = Call-Tool -Name 'get_scope' -Arguments @{ version = 'acan-mcp/1'; task = $Task }
    if (-not $sc.ok) { Add-Step -Name 'get the confirmed scope' -Ok $false -Evidence @{ refused = $sc.text }; throw 'scope refused' }
    $body = $sc.value.scope.body
    $preds = @()
    foreach ($item in @($body.may)) { $preds += @{ selector = $item.selector; predicted = @{ kind = 'change'; change = $item.change } } }
    foreach ($rule in @($body.must_keep)) { $preds += @{ selector = $rule.selector; predicted = @{ kind = 'unchanged' } } }
    Add-Step -Name 'get the confirmed scope' -Ok $true -Evidence @{ scope_rev = $sc.value.scope.rev; scope_hash = [string]$sc.value.scope_hash; predictions = $preds.Count }
    $readSummary = if ($Summary) { $Summary } else { "control $($bundleObj.id): seeded bundle, not a model plan" }
    $rb = Call-Tool -Name 'post_scoped_readback' -Arguments @{
        version    = 'acan-mcp/1'
        task       = $Task
        scope_rev  = $sc.value.scope.rev
        scope_hash = $sc.value.scope_hash
        summary    = $readSummary
        predictions = @($preds)
    }
    Add-Step -Name 'control reads back the task at the confirmed scope' -Ok $rb.ok -Evidence @{ refused = $(if ($rb.ok) { $null } else { $rb.text }) }
    if (-not $rb.ok) { throw 'readback refused' }

    # ---- claim and declare ----------------------------------------------------------------------
    $cl = Call-Tool -Name 'claim_task' -Arguments @{ task = $Task }
    Add-Step -Name 'control claims the task' -Ok $cl.ok -Evidence @{ refused = $(if ($cl.ok) { $null } else { $cl.text }); generation = $cl.value.generation }
    if (-not $cl.ok) { throw 'claim refused' }
    $claimed = $true
    $generation = [uint64]$cl.value.generation
    $result['generation'] = $generation
    $dc = Call-Tool -Name 'declare_changes' -Arguments @{ task = $Task; generation = $generation; files = @($bundleObj.declared_files) }
    Add-Step -Name 'control declares the files of its bundle' -Ok $dc.ok -Evidence @{ files = @($bundleObj.declared_files); refused = $(if ($dc.ok) { $null } else { $dc.text }) }
    if (-not $dc.ok) { throw 'declare refused' }

    # ---- wait for build clearance ----------------------------------------------------------------
    $cleared = $false
    $stageEnd = (Get-Date).AddSeconds($StageWaitSec)
    $lastStage = ''
    while ($true) {
        $tk = Call-Tool -Name 'get_task' -Arguments @{ task = $Task }
        if (-not $tk.ok) { throw "get_task refused: $($tk.text)" }
        $stage = [string]$tk.value.stage
        $state = [string]$tk.value.state
        $holds = ($tk.value.you_hold -eq $true) -and ([uint64]$tk.value.generation -eq $generation)
        if ($stage -ne $lastStage) { $lastStage = $stage; Write-Output "task stage: $stage" }
        if ($state -ne 'claimed' -or -not $holds) { throw "task is $state at generation $($tk.value.generation); the control holds $generation" }
        if ($stage -eq 'building') { $cleared = $true; break }
        if ((Get-Date) -gt $stageEnd) { break }
        Start-Sleep -Seconds 2
    }
    Add-Step -Name 'origin clears the task to build' -Ok $cleared -Evidence @{ stage = $lastStage }
    if (-not $cleared) { throw "build clearance did not arrive within $StageWaitSec s" }

    # ---- write the bundle into the worktree and commit ---------------------------------------
    $wt = Call-Tool -Name 'get_worktree' -Arguments @{ task = $Task }
    if (-not $wt.ok) { throw "get_worktree refused: $($wt.text)" }
    $wtPath = [string]$wt.value.path
    $written = @()
    foreach ($file in @($bundleObj.files)) {
        $text = Read-PinnedFile -Path ([string]$file.path)
        $text = Apply-Edits -Text $text -Edits $file.edits -Path ([string]$file.path)
        $target = Join-Path $wtPath ([string]$file.path)
        [IO.File]::WriteAllText($target, $text, (New-Object System.Text.UTF8Encoding($false)))
        $written += [string]$file.path
    }
    Add-Step -Name 'control writes its bundle into the task worktree' -Ok $true -Evidence @{ worktree = $wtPath; files = $written }
    & git -C $wtPath add -A
    & git -C $wtPath commit -q -m "control $($bundleObj.id) on $Task ($($bundleObj.applies_to))"
    if ($LASTEXITCODE -ne 0) { throw 'git commit failed in the worktree (nothing to commit, or identity missing)' }
    $head = (& git -C $wtPath rev-parse HEAD).Trim()
    Add-Step -Name 'control commits the candidate' -Ok ([bool]$head) -Evidence @{ head = $head; base = [string]$bundleObj.base }
    $result['head'] = $head

    # ---- report and wait for the receipt -----------------------------------------------------
    # Without -AwaitRuling the control stops at its first new receipt. With it, the control keeps
    # its claim while a human rules: each new receipt is saved, and the control waits until the gate
    # lands the candidate, the claim ends, or -RulingWaitSec passes with no new receipt.
    $before = Call-Tool -Name 'get_receipt' -Arguments @{ version = 'acan-mcp/1'; task = $Task }
    $lastHash = if ($before.ok) { [string]$before.value.receipt_hash } else { '' }
    $rep = Call-Tool -Name 'report_built' -Arguments @{ task = $Task; generation = $generation; head = $head }
    Add-Step -Name 'control reports the build' -Ok $rep.ok -Evidence @{ refused = $(if ($rep.ok) { $null } else { $rep.text }) }
    if (-not $rep.ok) { throw 'report refused' }
    $deadline = (Get-Date).AddSeconds($ReceiptWaitSec)
    $latest = $null
    $receiptsSaved = 0
    $reReported = $false
    while ($true) {
        $gr = Call-Tool -Name 'get_receipt' -Arguments @{ version = 'acan-mcp/1'; task = $Task }
        if ($gr.ok -and [string]$gr.value.receipt_hash -ne $lastHash) {
            $lastHash = [string]$gr.value.receipt_hash
            $latest = $gr
            $receiptsSaved++
            $gr.value | ConvertTo-Json -Depth 80 | Set-Content -LiteralPath "$Out.receipt.json" -Encoding utf8
            $gate = [string]$gr.value.receipt.gate_state
            Write-Output "receipt $lastHash gate_state $gate"
            if ($gate -eq 'landed' -or -not $AwaitRuling) { break }
            $deadline = (Get-Date).AddSeconds($RulingWaitSec)
        }
        if ($null -ne $latest -and $AwaitRuling) {
            $tk = Call-Tool -Name 'get_task' -Arguments @{ task = $Task }
            if (-not $tk.ok -or $tk.value.you_hold -ne $true -or [string]$tk.value.state -ne 'claimed') { break }
            # The rulings file appears once the human has recorded the rulings. The control then
            # reports the same build once more, so the gate runs again on the new ledger head.
            if (-not $reReported -and $RulingsDone -and (Test-Path -LiteralPath $RulingsDone)) {
                $reReported = $true
                $again = Call-Tool -Name 'report_built' -Arguments @{ task = $Task; generation = $generation; head = $head }
                Add-Step -Name 'control reports the same build again after the rulings' -Ok $again.ok -Evidence @{ refused = $(if ($again.ok) { $null } else { $again.text }) }
                $deadline = (Get-Date).AddSeconds($ReceiptWaitSec)
            }
        }
        if ((Get-Date) -gt $deadline) { break }
        Start-Sleep -Seconds 3
    }
    if ($null -eq $latest) {
        Add-Step -Name 'the gate answers the build with a receipt' -Ok $false -Evidence @{ waited_sec = $ReceiptWaitSec }
    } else {
        $rec = $latest.value.receipt
        $items = @()
        foreach ($it in @($rec.items)) {
            $items += [ordered]@{ class = [string]$it.class; state = [string]$it.state; ruling_id = [string]$it.ruling_id; episode_id = [string]$it.episode_id; probe_hash = [string]$it.probe_hash }
        }
        $result['receipt_hash'] = [string]$latest.value.receipt_hash
        $result['gate_state'] = $rec.gate_state
        $result['counts'] = $rec.counts
        $result['items'] = $items
        $result['rulings'] = $rec.rulings
        $result['receipt_file'] = "$Out.receipt.json"
        $result['receipts_saved'] = $receiptsSaved
        Add-Step -Name 'the gate answers the build with a receipt' -Ok $true -Evidence @{ receipt_hash = $result['receipt_hash']; gate_state = $rec.gate_state; items = $items.Count; receipts_saved = $receiptsSaved }
    }
} catch {
    $result['error'] = $_.Exception.Message
    Write-Output "CONTROL FAILED: $($_.Exception.Message)"
} finally {
    if ($claimed) {
        $rel = Call-Tool -Name 'release_task' -Arguments @{ task = $Task; generation = $generation }
        # A blocked receipt makes the server release the claim itself. A later release of the same
        # generation is then refused as stale with no current holder: that exact refusal means released.
        $alreadyReleased = (-not $rel.ok) -and ($rel.text -eq 'stale generation; current is None')
        if ($alreadyReleased) {
            $result['release'] = 'already released by the server (stale generation; current is None)'
            Add-Step -Name 'control releases the claim after its receipt' -Ok $true -Evidence @{ released = 'already released by the server'; refused = $rel.text }
        } else {
            Add-Step -Name 'control releases the claim after its receipt' -Ok $rel.ok -Evidence @{ refused = $(if ($rel.ok) { $null } else { $rel.text }) }
        }
    }
    Write-Result
    Write-Output "result: $Out"
}
