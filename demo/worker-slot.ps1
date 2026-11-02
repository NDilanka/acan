# Runs one worker slot. An acan-worker process builds one task and exits, so the slot starts a new
# worker after each exit. A slot therefore builds every task its agent can claim, and a split into
# three tasks needs no third process. Each run writes its own log pair under the run's logs folder.

[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Name,
    [Parameter(Mandatory)][string]$Agent,
    [Parameter(Mandatory)][string]$RunDir,
    [Parameter(Mandatory)][string]$Worker,
    [Parameter(Mandatory)][int]$Port,
    [int]$MaxRuns = 8,
    [int]$WaitSecs = 900
)

$ErrorActionPreference = 'Stop'
$logs = Join-Path $RunDir 'logs'
$acanDir = Join-Path $RunDir '.acan'
$entry = Get-Content -LiteralPath (Join-Path $acanDir 'tokens.json') -Raw | ConvertFrom-Json |
    Where-Object { $_.principal.kind -eq 'agent' -and $_.principal.id -eq $Agent } | Select-Object -First 1
if ($null -eq $entry) { throw "no agent token for $Agent in $acanDir\tokens.json" }

# The token goes to acan-worker in ACAN_AGENT_TOKEN, not on its command line, so the process list does not show it.
# Start-Process gives each worker run this process's environment.
$env:ACAN_AGENT_TOKEN = $entry.token

$arguments = @(
    '--server', "http://127.0.0.1:$Port",
    '--repo', $RunDir,
    '--llm', (Join-Path $acanDir 'llm.toml'),
    '--env', (Join-Path $RunDir '.env'),
    '--test-command', 'bun run test',
    '--wait-secs', "$WaitSecs",
    '--heartbeat-secs', '20'
)
# Start-Process joins the argument array without quotes, so a value with spaces is quoted here.
$quoted = $arguments | ForEach-Object { if ($_ -match '\s') { '"' + $_ + '"' } else { $_ } }

for ($run = 1; $run -le $MaxRuns; $run++) {
    Start-Process -FilePath $Worker -ArgumentList $quoted -WorkingDirectory $RunDir -Wait -WindowStyle Hidden `
        -RedirectStandardOutput (Join-Path $logs "$Name.$run.out.log") `
        -RedirectStandardError (Join-Path $logs "$Name.$run.err.log")
    Start-Sleep -Seconds 3
}
