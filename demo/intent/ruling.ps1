<#
.SYNOPSIS
Records a human ruling on each out-of-scope step of one receipt, through the member HTTP API.

.DESCRIPTION
The human's rules are in a rules file (demo/intent/rulings/<order>-rulings.json). For each
out-of-scope item of the receipt, the first rule whose field pattern and candidate state match
gives the predicate the ruling states. The predicate reads one step of the candidate run, so the
ruling is per (episode, step): its selector is the episode's selector with step_context set to that
step, so the gate judges it only on the probes that have that step. For each (episode, step) the
script reads the ruling draft (GET .../ruling-draft) fresh, keeps the draft examples of the probes
where that step differs as the accepted examples, and posts one decision_confirmed event through
POST /api/commands with cmd record_ruling.

The origin checks the decision's chain and actor, and records it. It does not run the predicate on
the examples at record time (acan-core ruling.rs decide_record; acan-intent predicate.rs check_examples
is not on that path). The gate evaluates each ruling on every probe its selector reaches: a failing
probe becomes violates_ruling, and a probe the predicate cannot judge leaves the ruling unheld.

The script never decides a verdict itself: it turns the rules into predicates and sends them. The
owner token is read from the argument and never printed.
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
    [string]$Actor = 'owner'
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
        default      { throw "rule $($Rule.id) names an unknown kind '$($Rule.rule)'" }
    }
}

# Group the out-of-scope items by episode and step, in the order the receipt lists them. A ruling
# names one step, because a predicate reads a field of one step.
$groups = [ordered]@{}
foreach ($item in @($receipt.items)) {
    if ([string]$item.class -ne 'out_of_scope') { continue }
    $episode = [string]$item.episode_id
    if (-not $episode) { throw "an out-of-scope item has no episode: $($item.item_id)" }
    $ev = @($item.evidence | Where-Object { $_.slot -eq 'candidate' })
    if ($ev.Count -ne 1) { throw "episode ${episode}: an item has $($ev.Count) candidate evidence rows, expected one" }
    $ev = $ev[0]
    $step = [string]$ev.step
    $key = "$episode|$step"
    if (-not $groups.Contains($key)) {
        $groups[$key] = [pscustomobject]@{ episode = $episode; step = $step; probes = [System.Collections.Generic.List[string]]::new(); items = [System.Collections.Generic.List[object]]::new() }
    }
    $groups[$key].items.Add($item)
    $probe = [string]$item.probe_hash
    if (-not $groups[$key].probes.Contains($probe)) { $groups[$key].probes.Add($probe) }
}
Write-Output "out-of-scope episode steps to rule: $($groups.Count)"

foreach ($key in $groups.Keys) {
    $group = $groups[$key]
    $episode = $group.episode
    $step = $group.step
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
            throw "episode ${episode} step ${step}: no rule in $RulesFile covers field $($ev.field) with candidate state $state; the item stays open"
        }
        $predicate = New-Predicate -Rule $rule -Step $step -Field ([string]$ev.field)
        $pkey = ($predicate | ConvertTo-Json -Depth 20 -Compress)
        if (-not $seen.ContainsKey($pkey)) { $seen[$pkey] = $true; $predicates.Add($predicate) }
        if ($null -eq $label) { $label = [string]$rule.label; $machine = [string]$item.machine_id }
        $fields += "$($ev.field) ($($rule.id))"
    }
    $predicate = if ($predicates.Count -eq 1) { $predicates[0] } else { [ordered]@{ predicate = 'all'; of = @($predicates) } }
    $hex = -join ($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($key)) | ForEach-Object { $_.ToString('x2') })
    $ruling = "r-$OrderId-" + $hex.Substring(0, 12)

    # The draft is read again for every ruling: each ruling moves the ledger head.
    $draft = Invoke-RestMethod -Uri "$base/api/orders/$OrderId/episodes/$([uri]::EscapeDataString($episode))/ruling-draft" -Headers $headers -TimeoutSec 60
    $accepts = [System.Collections.Generic.List[string]]::new()
    $examplesByProbe = @{}
    foreach ($example in @($draft.examples)) {
        $probe = [string]$example.probe_hash
        $examplesByProbe[$probe] = $true
        if ($group.probes -contains $probe -and -not $accepts.Contains([string]$example.hash)) { $accepts.Add([string]$example.hash) }
    }
    foreach ($probe in $group.probes) {
        if (-not $examplesByProbe.ContainsKey($probe)) { throw "episode ${episode} step ${step}: the draft keeps no example for probe $probe" }
    }
    if ($accepts.Count -eq 0) { throw "episode ${episode} step ${step}: the draft keeps no example for the step's probes" }
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
    $decision['ruling_id'] = $ruling
    $decision['order'] = $OrderId
    $decision['scope_hash'] = [string]$draft.scope_hash
    $decision['episode_id'] = $episode
    $decision['evidence_hashes'] = @()
    $decision['selector'] = $selector
    $decision['predicate_ast'] = $predicate
    $decision['accepts'] = @($accepts)
    $decision['rejects'] = @()
    $decision['ruled_on'] = $ruledOn
    $decision['normalization_version'] = [string]$draft.normalization_version
    $decision['normalization_hash'] = [string]$draft.normalization_hash
    $decision['applicability'] = 'standing_predicate_approval'
    $decision['label'] = $label
    $decision['machine_ids'] = @($machine)

    $body = ConvertTo-Json -Depth 60 -Compress @{ cmd = 'record_ruling'; event = $decision }
    $entry = [ordered]@{
        ruling_id  = $ruling
        episode_id = $episode
        step_id    = $step
        fields     = $fields
        label      = $label
        predicate  = $predicate
        probes     = @($group.probes)
        accepts    = @($accepts)
        ruled_on   = $ruledOn
        ledger_seq = $decision['event_seq']
    }
    try {
        $resp = Invoke-RestMethod -Method Post -Uri "$base/api/commands" -Headers $headers -ContentType 'application/json' -Body $body -TimeoutSec 120
        $entry['result'] = 'recorded'
        $entry['events'] = @($resp | ForEach-Object { [string]$_.event.type })
        Write-Output "[recorded] $ruling $episode $step ($($fields -join '; '))"
    } catch {
        $detail = if ($_.ErrorDetails) { $_.ErrorDetails.Message } else { $_.Exception.Message }
        $entry['result'] = 'refused'
        $entry['refusal'] = $detail
        $log.Add($entry)
        $entry | ConvertTo-Json -Depth 40 | Set-Content -LiteralPath $Out -Encoding utf8
        throw "the origin refused ruling $ruling for $episode step ${step}: $detail"
    }
    $log.Add($entry)
}

$result = [ordered]@{
    schema     = 'acan-ruling-run/2'
    order      = $Order
    order_id   = $OrderId
    receipt    = $ReceiptFile
    rules      = $RulesFile
    rulings    = @($log)
    finished   = (Get-Date).ToString('o')
}
$result | ConvertTo-Json -Depth 40 | Set-Content -LiteralPath $Out -Encoding utf8
Write-Output "rulings recorded: $($log.Count); log: $Out"
