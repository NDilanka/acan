# Stages one take of the hackathon demo on a fresh copy of the demo repository.
#
# Each take gets its own run folder, <run root>\take-<stamp>, and current.txt names it. The run root is
# target\acan-run in this checkout, or ACAN_RUN_ROOT when that is set. A take never reuses a folder, so an
# earlier take's logs are never overwritten, and a stale process from an earlier take writes only into
# that take's folder.
#
# It clones the demo repo into the run folder, copies the live routing and provider key file in, starts
# `acan serve` with the UI and the demo app, mints tokens for the owner and one agent per worker,
# registers the agents, posts the order with its witnesses, and starts one worker slot per agent
# (worker-slot.ps1). It returns once everything is running. Run watch.ps1 next to follow the take.
#
# The owner token is never printed. It is in the run folder, in .acan\tokens.json and in
# logs\ui-url.txt, which holds the UI URL with the token in its fragment.

[CmdletBinding()]
param(
    # Empty means a new take folder under the run root (target\acan-run, or ACAN_RUN_ROOT).
    [string]$RunDir = '',
    # The demo repo to clone. Empty means ACAN_DEMO_REPO, then the public repo. A local path works too.
    [string]$DemoRepo = $env:ACAN_DEMO_REPO,
    # The port of acan serve (the UI and API).
    [int]$Port = 7311,
    # The port of the demo app. 3000 is taken on many machines, so the default is 7312. Pass another free
    # port here if the take stops with "already in use".
    [int]$DemoPort = 7312,
    # The scanner gives each file of the demo's src folder its own machine, named by file stem, so the
    # machine of src/middleware.ts is "middleware". run.ps1 stops if the plant does not list it.
    [string]$Machine = 'middleware',
    # A task nobody has claimed yet holds the order's declare barrier, so every task of the split needs
    # its own worker at once. A split of three needs three slots.
    [int]$Workers = 3,
    [string]$Sentence = 'Add per-user rate limiting and an audit log of every API request, and keep the login flow untouched.',
    [int]$WaitSecs = 900,
    # Follow the take in this process until it ends, so one background job holds the whole take.
    [switch]$Watch,
    [ValidateRange(0, 3)][int]$Settle = 3
)

. (Join-Path $PSScriptRoot 'lib.ps1')

$repo = $script:WorktreeRoot
$acan = Join-Path $repo 'target\debug\acan.exe'
$worker = Join-Path $repo 'target\debug\acan-worker.exe'
$www = Join-Path $repo 'crates\acan-render-web\www'
$llmSource = Join-Path $repo 'crates\acan-llm\llm.toml'
$envSource = Join-Path $repo 'crates\acan-llm\.env'

# The previous take's processes stop. Its folder stays as it was.
$previous = Get-CurrentRunDir
if ($null -ne $previous) { & (Join-Path $PSScriptRoot 'stop.ps1') -RunDir $previous }

if (-not $RunDir) { $RunDir = Join-Path $script:RunRoot ('take-' + (Get-Date -Format 'yyyyMMdd-HHmmss')) }
if (Test-Path -LiteralPath $RunDir) { throw "$RunDir already exists; a take never reuses a folder" }
# Both ports are checked before the clone and the build, so a busy port costs nothing.
Assert-PortFree -Name 'Port' -Port $Port
Assert-PortFree -Name 'DemoPort' -Port $DemoPort
if (-not $DemoRepo) { $DemoRepo = 'https://github.com/NDilanka/acan-demo.git' }

New-Item -ItemType Directory -Force -Path (Split-Path -Parent $RunDir) | Out-Null
git clone --quiet $DemoRepo $RunDir
if ($LASTEXITCODE -ne 0) { throw "git clone of $DemoRepo failed with exit code $LASTEXITCODE" }
Set-CurrentRunDir -RunDir $RunDir

$logs = Join-Path $RunDir 'logs'
New-Item -ItemType Directory -Force -Path $logs | Out-Null
$state = @{
    runDir     = $RunDir
    startedUtc = (Get-Date).ToUniversalTime().ToString('o')
    port       = $Port
    demoPort   = $DemoPort
    machine    = $Machine
    workers    = $Workers
    sentence   = $Sentence
    processes  = New-Object System.Collections.ArrayList
}
Save-RunState -State $state -RunDir $RunDir

# The copied key file must not show up as untracked in the run repo, since that blocks a land.
Add-Content -LiteralPath (Join-Path $RunDir '.git\info\exclude') -Value '/.env'

$acanDir = Join-Path $RunDir '.acan'
New-Item -ItemType Directory -Force -Path $acanDir | Out-Null
Copy-Item -LiteralPath $llmSource -Destination (Join-Path $acanDir 'llm.toml')
Copy-Item -LiteralPath $envSource -Destination (Join-Path $RunDir '.env')

# Tokens go in before the first start, so the server loads them and mints no owner token of its own.
$owner = 'acan_' + [guid]::NewGuid().ToString('N')
$tokens = @(@{ token = $owner; principal = @{ kind = 'member'; id = 'owner' } })
$agents = @(1..$Workers | ForEach-Object { "agent-$_" })
foreach ($name in $agents) {
    $tokens += @{ token = 'acan_' + [guid]::NewGuid().ToString('N'); principal = @{ kind = 'agent'; id = $name } }
}
$tokens | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $acanDir 'tokens.json') -Encoding utf8

Push-Location $repo
try {
    cargo build -p acan-server -p acan-agent --bins *> (Join-Path $logs 'build.log')
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed; see $logs\build.log" }
} finally {
    Pop-Location
}

# The stack outlives this script, so its processes must not inherit this process's output pipe.
# Everything below starts with its output redirected to log files.
Clear-InheritedStdio

$serve = Start-Logged -State $state -RunDir $RunDir -Name 'acan-serve' -File $acan -WorkingDirectory $RunDir -Arguments @(
    'serve',
    '--repo', $RunDir,
    '--port', "$Port",
    '--web', $www,
    '--llm', (Join-Path $acanDir 'llm.toml'),
    '--protected-branch', 'main',
    '--witness-http-base-url', "http://127.0.0.1:$DemoPort",
    '--witness-concurrency', '2'
)
if (-not (Wait-Http200 -Url "http://127.0.0.1:$Port/health" -Seconds 60)) {
    throw "acan serve did not answer on port $Port; see $logs\acan-serve.err.log"
}

$env:PORT = "$DemoPort"
$bun = (Get-Command bun -CommandType Application).Source
$app = Start-Logged -State $state -RunDir $RunDir -Name 'demo-app' -File $bun -WorkingDirectory $RunDir -Arguments @('src/server.ts')
Remove-Item Env:PORT
if (-not (Wait-Http200 -Url "http://127.0.0.1:$DemoPort/health" -Seconds 60)) {
    throw "the demo app did not answer on port $DemoPort; see $logs\demo-app.err.log"
}

$plant = Invoke-Api -Port $Port -Token $owner -Method GET -Path '/api/plant'
$machines = @($plant.machines | ForEach-Object { $_.name })
Write-Output "plant machines: $($machines -join ', ')"
if ($machines -notcontains $Machine) { throw "machine '$Machine' is not on the plant; pick one of the names above and pass -Machine" }

foreach ($name in $agents) {
    Invoke-Api -Port $Port -Token $owner -Method POST -Path '/api/commands' -Body @{ cmd = 'register_agent'; agent = $name } | Out-Null
}

# The witnesses are the bar the build must clear: the login flow, the links flow, and the health route.
$witnesses = @(
    @{ check = @{ kind = 'test'; command = 'bun test test/login.test.ts' }; reason = 'the login flow stays untouched' },
    @{ check = @{ kind = 'test'; command = 'bun test test/links.test.ts' }; reason = 'links keep working' },
    @{ check = @{ kind = 'http'; method = 'GET'; path = '/health'; expect_status = 200; expect_body = 'ok' }; reason = 'the app still answers its health check' }
)
$posted = Invoke-Api -Port $Port -Token $owner -Method POST -Path '/api/commands' -Body @{
    cmd       = 'post_order'
    machine   = $Machine
    sentence  = $Sentence
    witnesses = $witnesses
}
$state.orderPostedUtc = (Get-Date).ToUniversalTime().ToString('o')
Save-RunState -State $state -RunDir $RunDir
Write-Output "order posted: $($posted.Count) event(s) returned"

# Each slot runs acan-worker again after every task, so the slots can build a split of any size.
$slot = Join-Path $PSScriptRoot 'worker-slot.ps1'
$pwsh = [Environment]::ProcessPath
for ($i = 1; $i -le $Workers; $i++) {
    Start-Logged -State $state -RunDir $RunDir -Name "worker-$i" -File $pwsh -WorkingDirectory $RunDir -Arguments @(
        '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $slot,
        '-Name', "worker-$i", '-Agent', "agent-$i", '-RunDir', $RunDir,
        '-Worker', $worker, '-Port', "$Port", '-WaitSecs', "$WaitSecs"
    ) | Out-Null
}

# The URL with the owner token in its fragment. The token never goes to the console.
$uiUrl = "http://127.0.0.1:$Port/#token=$owner"
Set-Content -LiteralPath (Join-Path $logs 'ui-url.txt') -Value $uiUrl -Encoding utf8
Write-Output "run folder: $RunDir"
Write-Output "workers: $Workers slot(s), agents $($agents -join ', ')"
Write-Output "UI: http://127.0.0.1:$Port/  (open $logs\ui-url.txt for the URL with the owner token)"
Write-Output "watch: pwsh $PSScriptRoot\watch.ps1 -RunDir `"$RunDir`" -Settle 3"
Write-Output "stop: pwsh $PSScriptRoot\stop.ps1 -RunDir `"$RunDir`""

if ($Watch) {
    & (Join-Path $PSScriptRoot 'watch.ps1') -RunDir $RunDir -Port $Port -Settle $Settle
    # The watch has ended, so this process ends with it. The stack keeps running until stop.ps1 runs.
    # Every process this script starts writes to a log file, not to this process's output.
    exit 0
}
