<#
.SYNOPSIS
Records a human ruling on each out-of-scope episode of one receipt, through the member HTTP API.

.DESCRIPTION
The human's rules are in a rules file (demo/intent/rulings/<order>-rulings.json). For each
out-of-scope item of the receipt, the first rule whose field pattern and candidate state match
gives the predicate the ruling states. The predicate is built from the candidate run only. For each
episode the script reads the ruling draft (GET .../ruling-draft), takes the ledger head from it,
and posts one decision_confirmed event through POST /api/commands with cmd record_ruling. The
origin checks the predicate on every retained example and refuses a ruling that does not hold on
them; a refusal stops the script and is written to the output.

The script never decides a verdict itself: it turns the rules into predicates and sends them. The
gate evaluates the predicates. The owner token is read from the argument and never printed.
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

# Group the out-of-scope items by episode, in the order the receipt lists them.
$episodes = [ordered]@{}
foreach ($item in @($receipt.items)) {
    if ([string]$item.class -ne 'out_of_scope') { continue }
    $episode = [string]$item.episode_id
    if (-not $episode) { throw "an out-of-scope item has no episode: $($item.item_id)" }
    if (-not $episodes.Contains($episode)) { $episodes[$episode] = [System.Collections.Generic.List[object]]::new() }
    $episodes[$episode].Add($item)
}
Write-Output "out-of-scope episodes to rule: $($episodes.Count)"

foreach ($episode in $episodes.Keys) {
    $items = $episodes[$episode]
    $predicates = [System.Collections.Generic.List[object]]::new()
    $seen = @{}
    $label = $null
    $machine = $null
    $candidateRun = $null
    $fields = @()
    foreach ($item in $items) {
        $ev = @($item.evidence | Where-Object { $_.slot -eq 'candidate' })
        if ($ev.Count -ne 1) { throw "episode ${episode}: an item has $($ev.Count) candidate evidence rows, expected one" }
        $ev = $ev[0]
        $state = [string]$ev.value.state
        $rule = Select-Rule -Field ([string]$ev.field) -State $state
        if ($null -eq $rule) {
            throw "episode ${episode}: no rule in $RulesFile covers field $($ev.field) with candidate state $state; the item stays open"
        }
        $predicate = New-Predicate -Rule $rule -Step ([string]$ev.step) -Field ([string]$ev.field)
        $key = ($predicate | ConvertTo-Json -Depth 20 -Compress)
        if (-not $seen.ContainsKey($key)) { $seen[$key] = $true; $predicates.Add($predicate) }
        if ($null -eq $label) { $label = [string]$rule.label; $machine = [string]$item.machine_id; $candidateRun = [string]$ev.run_id }
        if ([string]$ev.run_id -ne $candidateRun) { throw "episode ${episode}: items name more than one candidate run" }
        $fields += "$($ev.step) $($ev.field) ($($rule.id))"
    }
    $predicate = if ($predicates.Count -eq 1) { $predicates[0] } else { [ordered]@{ predicate = 'all'; of = @($predicates) } }

    # The draft is read again for every episode: each ruling moves the ledger head.
    $draft = Invoke-RestMethod -Uri "$base/api/orders/$OrderId/episodes/$([uri]::EscapeDataString($episode))/ruling-draft" -Headers $headers -TimeoutSec 60
    $accepts = @()
    $rejects = @()
    foreach ($example in @($draft.examples)) {
        if ([string]$example.run -eq $candidateRun -and [string]$example.probe_hash -eq [string]$draft.probe_hash) {
            $accepts += [string]$example.hash
        } else {
            $rejects += [string]$example.hash
        }
    }
    if ($accepts.Count -eq 0) { throw "episode ${episode}: the draft keeps no example of candidate run $candidateRun for its probe" }
    $head = $draft.ledger_head
    $ruling = "r-$OrderId-" + (($episode -replace '^sha256:', '').Substring(0, 12))
    $decision = [ordered]@{
        event                 = 'decision_confirmed'
        schema_version        = 'acan-ledger/1'
        event_seq             = [uint64]$head.event_seq + 1
    }
    if ($null -ne $head.hash) { $decision['prev_hash'] = [string]$head.hash }
    $decision['actor'] = $Actor
    $decision['at'] = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    $decision['ruling_id'] = $ruling
    $decision['order'] = $OrderId
    $decision['scope_hash'] = [string]$draft.scope_hash
    $decision['episode_id'] = $episode
    $decision['evidence_hashes'] = @()
    $decision['selector'] = $draft.selector
    $decision['predicate_ast'] = $predicate
    $decision['accepts'] = @($accepts)
    $decision['rejects'] = @($rejects)
    $decision['ruled_on'] = [string]$draft.ruled_on
    $decision['normalization_version'] = [string]$draft.normalization_version
    $decision['normalization_hash'] = [string]$draft.normalization_hash
    $decision['applicability'] = 'standing_predicate_approval'
    $decision['label'] = $label
    $decision['machine_ids'] = @($machine)

    $body = ConvertTo-Json -Depth 60 -Compress @{ cmd = 'record_ruling'; event = $decision }
    $entry = [ordered]@{
        ruling_id  = $ruling
        episode_id = $episode
        fields     = $fields
        label      = $label
        predicate  = $predicate
        accepts    = @($accepts)
        rejects    = @($rejects)
        ledger_seq = $decision['event_seq']
    }
    try {
        $resp = Invoke-RestMethod -Method Post -Uri "$base/api/commands" -Headers $headers -ContentType 'application/json' -Body $body -TimeoutSec 120
        $entry['result'] = 'recorded'
        $entry['events'] = @($resp | ForEach-Object { [string]$_.event.type })
        Write-Output "[recorded] $ruling $($fields -join '; ')"
    } catch {
        $detail = if ($_.ErrorDetails) { $_.ErrorDetails.Message } else { $_.Exception.Message }
        $entry['result'] = 'refused'
        $entry['refusal'] = $detail
        $log.Add($entry)
        $entry | ConvertTo-Json -Depth 40 | Set-Content -LiteralPath $Out -Encoding utf8
        throw "the origin refused ruling $ruling for $episode`: $detail"
    }
    $log.Add($entry)
}

$result = [ordered]@{
    schema     = 'acan-ruling-run/1'
    order      = $Order
    order_id   = $OrderId
    receipt    = $ReceiptFile
    rules      = $RulesFile
    rulings    = @($log)
    finished   = (Get-Date).ToString('o')
}
$result | ConvertTo-Json -Depth 40 | Set-Content -LiteralPath $Out -Encoding utf8
Write-Output "rulings recorded: $($log.Count); log: $Out"
