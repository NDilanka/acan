# Shared helpers for the demo scripts. Dot-sourced by run.ps1, watch.ps1 and stop.ps1.
# Nothing here prints a token or a provider key.

$ErrorActionPreference = 'Stop'

$script:WorktreeRoot = Split-Path -Parent $PSScriptRoot
# Every take gets its own folder under this root, so a stale process can only write into its own take.
# The default is under target\, which git ignores. ACAN_RUN_ROOT moves it.
$script:RunRoot = if ($env:ACAN_RUN_ROOT) { $env:ACAN_RUN_ROOT } else { Join-Path $script:WorktreeRoot 'target\acan-run' }

# The folder of the latest take, or $null before the first take.
function Get-CurrentRunDir {
    $pointer = Join-Path $script:RunRoot 'current.txt'
    if (-not (Test-Path -LiteralPath $pointer)) { return $null }
    $path = (Get-Content -LiteralPath $pointer -Raw).Trim()
    if ($path) { $path } else { $null }
}

function Set-CurrentRunDir {
    param([string]$RunDir)
    New-Item -ItemType Directory -Force -Path $script:RunRoot | Out-Null
    Set-Content -LiteralPath (Join-Path $script:RunRoot 'current.txt') -Value $RunDir -Encoding utf8
}

function Get-RunState {
    param([string]$RunDir)
    $path = Join-Path $RunDir 'logs\run.json'
    if (-not (Test-Path -LiteralPath $path)) { return $null }
    Get-Content -LiteralPath $path -Raw | ConvertFrom-Json
}

function Save-RunState {
    param([hashtable]$State, [string]$RunDir)
    $dir = Join-Path $RunDir 'logs'
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $State | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $dir 'run.json') -Encoding utf8
}

function Get-OwnerToken {
    param([string]$RunDir)
    $entries = Get-Content -LiteralPath (Join-Path $RunDir '.acan\tokens.json') -Raw | ConvertFrom-Json
    ($entries | Where-Object { $_.principal.kind -eq 'member' } | Select-Object -First 1).token
}

# True when the recorded process is still the one that was started: the pid is live and its start time matches.
function Test-RecordedProcess {
    param([int]$ProcessId, [long]$StartTicks)
    $proc = Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
    $null -ne $proc -and $proc.StartTime.Ticks -eq $StartTicks
}

# Makes this process's stdio handles non-inheritable. A process started later without redirection
# would inherit the caller's output pipe, and the caller would wait for the stack to exit even after
# run.ps1 has returned. The stack's own output goes to log files, so nothing needs these handles.
function Clear-InheritedStdio {
    if (-not ('Acan.Demo.StdioHandles' -as [type])) {
        Add-Type -Namespace Acan.Demo -Name StdioHandles -MemberDefinition @'
[DllImport("kernel32.dll", SetLastError=true)] public static extern bool SetHandleInformation(System.IntPtr h, uint mask, uint flags);
[DllImport("kernel32.dll")] public static extern System.IntPtr GetStdHandle(int which);
'@
    }
    foreach ($which in @(-10, -11, -12)) {
        [void][Acan.Demo.StdioHandles]::SetHandleInformation([Acan.Demo.StdioHandles]::GetStdHandle($which), 1, 0)
    }
}

# Starts a process with stdout and stderr in the run's logs folder and records it in the run state.
function Start-Logged {
    param(
        [hashtable]$State,
        [string]$RunDir,
        [string]$Name,
        [string]$File,
        [string[]]$Arguments,
        [string]$WorkingDirectory
    )
    $logs = Join-Path $RunDir 'logs'
    New-Item -ItemType Directory -Force -Path $logs | Out-Null
    # Start-Process joins the argument array without quotes, so a value with spaces is quoted here.
    $quoted = $Arguments | ForEach-Object { if ($_ -match '\s') { '"' + $_ + '"' } else { $_ } }
    $proc = Start-Process -FilePath $File -ArgumentList $quoted -WorkingDirectory $WorkingDirectory `
        -RedirectStandardOutput (Join-Path $logs "$Name.out.log") `
        -RedirectStandardError (Join-Path $logs "$Name.err.log") `
        -PassThru -WindowStyle Hidden
    $State.processes.Add([ordered]@{ name = $Name; pid = $proc.Id; startTicks = $proc.StartTime.Ticks }) | Out-Null
    Save-RunState -State $State -RunDir $RunDir
    $proc
}

function Invoke-Api {
    param([int]$Port, [string]$Token, [string]$Method, [string]$Path, $Body = $null)
    $params = @{
        Method     = $Method
        Uri        = "http://127.0.0.1:$Port$Path"
        Headers    = @{ Authorization = "Bearer $Token" }
        TimeoutSec = 60
    }
    if ($null -ne $Body) {
        $params.Body = $Body | ConvertTo-Json -Depth 12 -Compress
        $params.ContentType = 'application/json'
    }
    Invoke-RestMethod @params
}

# The HTTP status of a failed web request, or $null when no response came back.
function Get-HttpStatus($ErrorRecord) {
    $response = $ErrorRecord.Exception.Response
    if ($null -eq $response) { return $null }
    [int]$response.StatusCode
}

# True when something already listens on the port. A start on a busy port would reach the wrong server.
function Test-PortBusy([int]$Port) {
    $null -ne (Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue)
}

# Stops a take before anything starts when a port is taken, and names the process and the parameter to change.
function Assert-PortFree {
    param([string]$Name, [int]$Port)
    if (-not (Test-PortBusy -Port $Port)) { return }
    $conn = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue | Select-Object -First 1
    $proc = if ($conn) { Get-Process -Id $conn.OwningProcess -ErrorAction SilentlyContinue }
    $holder = if ($proc) { "$($proc.ProcessName) (pid $($proc.Id))" } else { 'another process' }
    throw "port $Port is already in use by $holder. Stop it, or start the take on a free port with -$Name <port>, for example -$Name $($Port + 101)."
}

function Wait-Http200 {
    param([string]$Url, [int]$Seconds)
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        try {
            $response = Invoke-WebRequest -Uri $Url -TimeoutSec 3
            if ($response.StatusCode -eq 200) { return $true }
        } catch {
            # Not up yet; retry until the deadline.
        }
        Start-Sleep -Milliseconds 300
    }
    $false
}

# The environment variable of each provider the routing file uses, in file order. A provider that no
# routing row names is not needed, so its key is not asked for. A routing file with no rows names every
# provider. Reads the file only; no key is read or printed.
function Get-RoutedKeyNames {
    param([string]$LlmFile)
    $keyOf = @{}
    $order = New-Object System.Collections.Generic.List[string]
    $routed = New-Object System.Collections.Generic.List[string]
    $name = $null
    $section = ''
    foreach ($raw in (Get-Content -LiteralPath $LlmFile)) {
        $line = $raw.Trim()
        if ($line -match '^\[\[providers\]\]') { $name = $null; $section = 'providers'; continue }
        if ($line -match '^\[routing\.') { $section = 'routing'; continue }
        if ($line -match '^\[') { $section = ''; continue }
        if ($section -eq 'providers') {
            if ($line -match '^name\s*=\s*"([^"]+)"') { $name = $Matches[1]; $order.Add($name) | Out-Null }
            elseif ($line -match '^api_key_env\s*=\s*"([^"]+)"' -and $name) { $keyOf[$name] = $Matches[1] }
        } elseif ($section -eq 'routing' -and $line -match '^provider\s*=\s*"([^"]+)"') {
            $routed.Add($Matches[1]) | Out-Null
        }
    }
    $providers = if ($routed.Count -gt 0) { @($routed | Select-Object -Unique) } else { @($order) }
    foreach ($provider in $providers) {
        if (-not $keyOf.ContainsKey($provider)) {
            throw "the routing file names provider '$provider', which has no api_key_env in $LlmFile"
        }
    }
    @($providers | ForEach-Object { $keyOf[$_] } | Select-Object -Unique)
}

# The KEY=value lines of a provider key file, as a hashtable. Values stay in memory only.
function Get-EnvFileValues {
    param([string]$EnvFile)
    $values = @{}
    if (-not (Test-Path -LiteralPath $EnvFile)) { return $values }
    foreach ($raw in (Get-Content -LiteralPath $EnvFile)) {
        $line = $raw.Trim()
        if ($line -eq '' -or $line.StartsWith('#') -or -not $line.Contains('=')) { continue }
        $key, $value = $line -split '=', 2
        $values[$key.Trim()] = $value.Trim().Trim('"', "'")
    }
    $values
}

# Stops the take before anything costs money when a routed provider has no key, in the key file or the
# environment. Names the variables that are missing, never their values.
function Assert-KeysReady {
    param([string[]]$RoutedKeys, [string]$EnvFile)
    $fromFile = Get-EnvFileValues -EnvFile $EnvFile
    $missing = @($RoutedKeys | Where-Object {
        -not ($fromFile[$_] -or [Environment]::GetEnvironmentVariable($_))
    })
    if ($missing.Count -gt 0) {
        throw "no key for $($missing -join ', ') in $EnvFile or the environment. Nothing was started and no model was called."
    }
}

# Waits until the server's codebase has mapped, so the project takes writes. Returns the plant. Fails
# when the scan fails or does not finish in time. A mapping project answers reads, and writes get 503.
function Wait-Mapped {
    param([int]$Port, [string]$Token, [int]$Seconds)
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        $plant = $null
        try {
            $plant = Invoke-Api -Port $Port -Token $Token -Method GET -Path '/api/plant'
        } catch {
            if ((Get-HttpStatus $_) -eq 401) { throw 'the owner token was refused by acan serve' }
            # Not answering yet; retry until the deadline.
        }
        if ($plant -and $plant.scan.stage -eq 'done') { return $plant }
        if ($plant -and $plant.scan.stage -eq 'failed') { throw "the codebase did not map: $($plant.scan.error)" }
        Start-Sleep -Milliseconds 300
    }
    throw "the codebase did not finish mapping within $Seconds s; see the acan-serve log"
}
