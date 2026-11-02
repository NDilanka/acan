<#
.SYNOPSIS
Records a human ruling on each out-of-scope step of one receipt, through the member HTTP API.

.DESCRIPTION
The human's rules are in a rules file (demo/intent/rulings/<order>-rulings.json). For each
out-of-scope item of the receipt, the first rule whose field pattern and candidate state match
gives the predicate the ruling states. The predicate reads one step of the candidate run, so a
ruling names one step: its selector is the episode's selector with step_context set to that step.
A ruling covers only the reached probes whose normalization plan it names (acan-intent receipt.rs),
so the script makes one ruling per (episode, step, plan). A probe's plan is the normalization_hash
of the examples the ruling draft (GET .../ruling-draft) keeps for it. The script reads the drafts
at planning and reads each one again before it sends. Each ruling's accepted examples are the draft
examples of its plan's probes, and the ruling is confirmed under that plan. It posts one
decision_confirmed event per ruling through POST /api/commands with cmd record_ruling.

Every item is matched to a rule before the first ruling is sent, so a receipt with an item no rule
covers records nothing. The log is written even when a ruling is refused.

The origin checks the decision's chain and actor, and records it. It does not run the predicate on
the examples at record time (acan-core ruling.rs decide_record; acan-intent predicate.rs check_examples
is not on that path). The gate evaluates each ruling on every reached probe on its plan: a failing
probe becomes violates_ruling, and a probe the predicate cannot judge leaves the ruling unheld. A
reached probe on another plan is outside the ruling and needs its own ruling.

One ruling per (episode, step, plan): the origin takes the probes from the selector, so the probes a
ruling reaches are fixed by its step, and only those on its plan are covered. A ruling already in the
ledger with the same content is skipped, so the script can run again after a resume; a ruling in the
ledger with other content stops the script.

The script never decides a verdict itself: it turns the rules into predicates and sends them. The
owner token is read from the argument and never printed. With -PlanOnly the script only reads the
receipt and the rules and prints the rulings it would send; it sends nothing.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Server,
    [Parameter(Mandatory)][string]$OwnerToken,
    [Parameter(Mandatory)][string]$Order,
    # The order's id in the server's ledger (o-0 for the first order of a clone). The rules file names
    # the order by its label in orders.json; the URLs and the ruling use the server's id.
    [string]$OrderId = '',
    [Parameter(Mandatory)][string]$ReceiptFile,
    [Parameter(Mandatory)][string]$RulesFile,
    [Parameter(Mandatory)][string]$Out,
    [string]$Actor = 'owner',
    [switch]$PlanOnly
)

$ErrorActionPreference = 'Stop'
$base = $Server.TrimEnd('/')
$headers = @{ Authorization = "Bearer $OwnerToken" }
$rules = Get-Content -LiteralPath $RulesFile -Raw | ConvertFrom-Json
if ($rules.schema -ne 'acan-demo-intent-rulings/1') { throw "the rules file must have schema acan-demo-intent-rulings/1, found '$($rules.schema)'" }
if ($rules.order -ne $Order) { throw "the rules file is for $($rules.order), not $Order" }
if (-not $OrderId) { $OrderId = $Order }
$receipt = (Get-Content -LiteralPath $ReceiptFile -Raw | ConvertFrom-Json).receipt
$log = [System.Collections.Generic.List[object]]::new()
$sha = [System.Security.Cryptography.SHA256]::Create()

function Select-Rule {
    param([string]$Field, [string]$State)
    foreach ($rule in @($rules.rules)) {
        if ($Field -notmatch $rule.field) { continue }
        if ($rule.candidate -ne 'any' -and $rule.candidate -ne $State) { continue }
        return $rule
    }
    return $null
}

function New-Predicate {
    param($Rule, [string]$Step, [string]$Field)
    $at = [ordered]@{ ref = 'field'; run = 'candidate'; step_id = $Step; path = $Field }
    switch ($Rule.rule) {
        'uuid'       { return [ordered]@{ predicate = 'matches_schema'; at = $at; schema = [ordered]@{ type = 'string'; format = 'uuid' } } }
        'status_429' { return [ordered]@{ predicate = 'status_in'; step_id = $Step; statuses = @(429) } }
        'present'    { return [ordered]@{ predicate = 'present'; at = $at } }
        'absent'     { return [ordered]@{ predicate = 'absent'; at = $at } }
        'equals'     {
            if ($null -eq $Rule.PSObject.Properties['value']) { throw "rule $($Rule.id) is 'equals' but names no value" }
            return [ordered]@{ predicate = 'equals'; at = $at; value = $Rule.value }
        }
        default      { throw "rule $($Rule.id) names an unknown kind '$($Rule.rule)'" }
    }
}

# The ruling draft of each episode, read once for planning (GET only). Its examples name each probe's
# normalization plan, the plan the probe was compared under.
$drafts = @{}
function Get-Draft {
    param([string]$Episode)
    if (-not $drafts.ContainsKey($Episode)) {
        $drafts[$Episode] = Invoke-RestMethod -Uri "$base/api/orders/$OrderId/episodes/$([uri]::EscapeDataString($Episode))/ruling-draft" -Headers $headers -TimeoutSec 60
    }
    return $drafts[$Episode]
}

# The normalization plan of one probe of an episode: the one plan its kept examples name.
function Get-ProbePlan {
    param([string]$Episode, [string]$Probe)
    $draft = Get-Draft -Episode $Episode
    $plans = @($draft.examples | Where-Object { [string]$_.probe_hash -eq $Probe } | ForEach-Object { [string]$_.normalization_hash } | Sort-Object -Unique)
    if ($plans.Count -eq 0) { throw "episode ${Episode}: the draft keeps no example for probe $Probe, so its normalization plan is unknown" }
    if ($plans.Count -gt 1) { throw "episode ${Episode}: probe $Probe has examples under $($plans.Count) normalization plans" }
    return $plans[0]
}

# Group the out-of-scope items by episode, step and plan, in the order the receipt lists them. A
# ruling names one step, because a predicate reads a field of one step, and one plan, because it
# covers only the probes on that plan.
$groups = [ordered]@{}
foreach ($item in @($receipt.items)) {
    if ([string]$item.class -ne 'out_of_scope') { continue }
    $episode = [string]$item.episode_id
    if (-not $episode) { throw "an out-of-scope item has no episode: $($item.item_id)" }
    $ev = @($item.evidence | Where-Object { $_.slot -eq 'candidate' })
    if ($ev.Count -ne 1) { throw "episode ${episode}: an item has $($ev.Count) candidate evidence rows, expected one" }
    $ev = $ev[0]
    $step = [string]$ev.step
    $probe = [string]$item.probe_hash
    $plan = Get-ProbePlan -Episode $episode -Probe $probe
    $key = "$episode|$step|$plan"
    if (-not $groups.Contains($key)) {
        $groups[$key] = [pscustomobject]@{ episode = $episode; step = $step; plan = $plan; probes = [System.Collections.Generic.List[string]]::new(); items = [System.Collections.Generic.List[object]]::new() }
    }
    $groups[$key].items.Add($item)
    if (-not $groups[$key].probes.Contains($probe)) { $groups[$key].probes.Add($probe) }
}
Write-Output "out-of-scope (episode, step, plan) groups to rule: $($groups.Count)"

# Plan every ruling before any is sent: a missing rule stops the script with nothing recorded.
$plans = [System.Collections.Generic.List[object]]::new()
foreach ($key in $groups.Keys) {
    $group = $groups[$key]
    $predicates = [System.Collections.Generic.List[object]]::new()
    $seen = @{}
    $label = $null
    $machine = $null
    $fields = @()
    foreach ($item in $group.items) {
        $ev = @($item.evidence | Where-Object { $_.slot -eq 'candidate' })[0]
        $state = [string]$ev.value.state
        $rule = Select-Rule -Field ([string]$ev.field) -State $state
        if ($null -eq $rule) {
            throw "episode $($group.episode) step $($group.step): no rule in $RulesFile covers field $($ev.field) with candidate state $state; the item stays open"
        }
        $predicate = New-Predicate -Rule $rule -Step $group.step -Field ([string]$ev.field)
        $pkey = ($predicate | ConvertTo-Json -Depth 20 -Compress)
        if (-not $seen.ContainsKey($pkey)) { $seen[$pkey] = $true; $predicates.Add($predicate) }
        if ($null -eq $label) { $label = [string]$rule.label; $machine = [string]$item.machine_id }
        $fields += "$($ev.field) ($($rule.id))"
    }
    $predicate = if ($predicates.Count -eq 1) { $predicates[0] } else { [ordered]@{ predicate = 'all'; of = @($predicates) } }
    $hex = -join ($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($key)) | ForEach-Object { $_.ToString('x2') })
    $plans.Add([pscustomobject]@{
        key       = $key
        episode   = $group.episode
        step      = $group.step
        plan      = $group.plan
        probes    = @($group.probes)
        fields    = $fields
        label     = $label
        machine   = $machine
        predicate = $predicate
        ruling    = "r-$OrderId-" + $hex.Substring(0, 12)
    })
}
Write-Output "rulings planned: $($plans.Count); every out-of-scope item has a rule"
if ($PlanOnly) {
    foreach ($plan in $plans) { Write-Output ('{0} {1} step={2} plan={3} probes={4} fields={5}' -f $plan.ruling, $plan.episode, $plan.step, $plan.plan, $plan.probes.Count, ($plan.fields -join '; ')) }
    return
}

# The ledger's rulings, read once. A ruling id is derived from its episode and step, so a resumed
# run plans the same id again. A ruling already in the ledger with the same content is skipped and
# logged as already_recorded; the same id with other content stops the script, because a changed
# ruling must supersede its head and this script does not do that.
$ledgerRulings = @{}
foreach ($row in @(Invoke-RestMethod -Uri "$base/api/events" -Headers $headers -TimeoutSec 60)) {
    $ev = $row.event
    if ($null -ne $ev -and [string]$ev.type -eq 'ruling_recorded') {
        $ledgerRulings[[string]$ev.event.ruling_id] = [pscustomobject]@{ seq = [uint64]$row.seq; decision = $ev.event }
    }
}

$failure = $null
try {
    foreach ($plan in $plans) {
        $episode = $plan.episode
        $step = $plan.step
        if ($ledgerRulings.ContainsKey($plan.ruling)) {
            $old = $ledgerRulings[$plan.ruling]
            $oldDecision = $old.decision
            $sameContent = ([string]$oldDecision.episode_id -eq $episode) `
                -and ([string]$oldDecision.selector.step_context.step_id -eq $step) `
                -and ([string]$oldDecision.normalization_hash -eq [string]$plan.plan) `
                -and ((ConvertTo-Json -Depth 60 -Compress $oldDecision.predicate_ast) -eq (ConvertTo-Json -Depth 60 -Compress $plan.predicate)) `
                -and ([string]$oldDecision.label -eq [string]$plan.label)
            if (-not $sameContent) {
                throw "ruling $($plan.ruling) for $episode step $step is in the ledger (seq $($old.seq)) with other content; a changed ruling must supersede its head, which this script does not do"
            }
            $log.Add([ordered]@{ ruling_id = $plan.ruling; episode_id = $episode; step_id = $step; normalization_hash = $plan.plan; fields = $plan.fields; label = $plan.label; result = 'already_recorded'; ledger_seq = $old.seq })
            Write-Output "[already recorded] $($plan.ruling) $episode $step (ledger seq $($old.seq))"
            continue
        }
        # The draft is read again for every ruling: each ruling moves the ledger head. The accepted
        # examples are the draft's examples of this plan's probes. An example of one of them under
        # another plan stops the script, because this ruling would not cover it.
        $draft = Invoke-RestMethod -Uri "$base/api/orders/$OrderId/episodes/$([uri]::EscapeDataString($episode))/ruling-draft" -Headers $headers -TimeoutSec 60
        $accepts = [System.Collections.Generic.List[string]]::new()
        $versions = [System.Collections.Generic.List[string]]::new()
        $probesSeen = @{}
        foreach ($example in @($draft.examples)) {
            $probe = [string]$example.probe_hash
            if ($plan.probes -notcontains $probe) { continue }
            if ([string]$example.normalization_hash -ne [string]$plan.plan) {
                throw "episode $episode step ${step}: probe $probe has an example under plan $($example.normalization_hash), not the planned $($plan.plan)"
            }
            $probesSeen[$probe] = $true
            $version = [string]$example.normalization_version
            if (-not $versions.Contains($version)) { $versions.Add($version) }
            if (-not $accepts.Contains([string]$example.hash)) { $accepts.Add([string]$example.hash) }
        }
        foreach ($probe in $plan.probes) {
            if (-not $probesSeen.ContainsKey($probe)) { throw "episode $episode step ${step}: the draft keeps no example for probe $probe" }
        }
        if ($versions.Count -ne 1) { throw "episode $episode step ${step}: the planned examples name $($versions.Count) normalization versions" }
        if ($accepts.Count -eq 0) { throw "episode $episode step ${step}: the draft keeps no example for the step's probes" }
        $ruledOn = if ($accepts -contains [string]$draft.ruled_on) { [string]$draft.ruled_on } else { $accepts[0] }
        $selector = [ordered]@{}
        foreach ($prop in $draft.selector.PSObject.Properties) { $selector[$prop.Name] = $prop.Value }
        $selector['step_context'] = [ordered]@{ kind = 'step'; step_id = $step }

        $head = $draft.ledger_head
        $decision = [ordered]@{
            event          = 'decision_confirmed'
            schema_version = 'acan-ledger/1'
            event_seq      = [uint64]$head.event_seq + 1
        }
        if ($null -ne $head.hash) { $decision['prev_hash'] = [string]$head.hash }
        $decision['actor'] = $Actor
        $decision['at'] = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
        $decision['ruling_id'] = $plan.ruling
        $decision['order'] = $OrderId
        $decision['scope_hash'] = [string]$draft.scope_hash
        $decision['episode_id'] = $episode
        $decision['evidence_hashes'] = @()
        $decision['selector'] = $selector
        $decision['predicate_ast'] = $plan.predicate
        $decision['accepts'] = @($accepts)
        $decision['rejects'] = @()
        $decision['ruled_on'] = $ruledOn
        $decision['normalization_version'] = [string]$versions[0]
        $decision['normalization_hash'] = [string]$plan.plan
        $decision['applicability'] = 'standing_predicate_approval'
        $decision['label'] = $plan.label
        $decision['machine_ids'] = @($plan.machine)

        $body = ConvertTo-Json -Depth 60 -Compress @{ cmd = 'record_ruling'; event = $decision }
        $entry = [ordered]@{
            ruling_id  = $plan.ruling
            episode_id = $episode
            step_id    = $step
            normalization_hash = $plan.plan
            fields     = $plan.fields
            label      = $plan.label
            predicate  = $plan.predicate
            probes     = $plan.probes
            accepts    = @($accepts)
            ruled_on   = $ruledOn
            ledger_seq = $decision['event_seq']
        }
        try {
            $resp = Invoke-RestMethod -Method Post -Uri "$base/api/commands" -Headers $headers -ContentType 'application/json' -Body $body -TimeoutSec 120
            $entry['result'] = 'recorded'
            $entry['events'] = @($resp | ForEach-Object { [string]$_.event.type })
            Write-Output "[recorded] $($plan.ruling) $episode $step ($($plan.fields -join '; '))"
        } catch {
            $detail = if ($_.ErrorDetails) { $_.ErrorDetails.Message } else { $_.Exception.Message }
            $entry['result'] = 'refused'
            $entry['refusal'] = $detail
            $log.Add($entry)
            throw "the origin refused ruling $($plan.ruling) for $episode step ${step}: $detail"
        }
        $log.Add($entry)
    }
} catch {
    $failure = $_.Exception.Message
}

$result = [ordered]@{
    schema     = 'acan-ruling-run/2'
    order      = $Order
    order_id   = $OrderId
    receipt    = $ReceiptFile
    rules      = $RulesFile
    planned    = $plans.Count
    rulings    = @($log)
    failure    = $failure
    finished   = (Get-Date).ToString('o')
}
$result | ConvertTo-Json -Depth 40 | Set-Content -LiteralPath $Out -Encoding utf8
if ($failure) { throw $failure }
Write-Output "rulings recorded: $($log.Count); log: $Out"
