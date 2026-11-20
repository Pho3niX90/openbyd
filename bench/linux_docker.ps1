<#
.SYNOPSIS
  Times openbyd against pyBYD on Linux (Debian, glibc) from a Windows machine with Docker Desktop.

.DESCRIPTION
  Runs bench/linux_docker.sh in a python:3.12-slim container. The checkout is mounted read-only;
  everything the run generates (Rust toolchain, cargo registry, target directory, virtualenv, pip cache,
  the build copy of the sources, wheels, result logs) lives under -ScratchDir, so nothing large
  lands on C:. Re-running reuses those caches.

  Phases (-Phase, default all): setup, build, test, measure, allocator, info.
  "all" runs setup, build, test, then -Rounds times (host load, measure), then allocator
  (the headline row with the mimalloc wheel and the glibc-malloc wheel alternating).

.PARAMETER Checkout     The openbyd checkout to measure (default: the one this script is in).
.PARAMETER ScratchDir   Where caches and results go, about 1.5 GB (default: $env:OPENBYD_BENCH_SCRATCH\linux,
                        or target\bench-scratch\linux in the checkout).
.PARAMETER CorpusRt     Optional VehicleRealtimeData.json of real replies, mounted read-only and timed by index.
.PARAMETER Rounds       How many times to run the final measurement (default 2).
.PARAMETER WaitQuiet    Wait until no process whose command line matches any of these words is running
                        (default none), then measure. Example: -WaitQuiet run_all.py,load.py,fake_server
.PARAMETER WaitUntil    Stop waiting for -WaitQuiet at this time and measure anyway (default: wait forever).
.PARAMETER RoundGapMinutes  Pause between measurement rounds, used only when -WaitUntil passed with the machine still busy (default 0).
.PARAMETER Image        Base image (default python:3.12-slim).
.PARAMETER RustcVersion Rust toolchain to install into the scratch dir (default 1.100.0).

.EXAMPLE
  pwsh bench/linux_docker.ps1 -CorpusRt E:\path\to\VehicleRealtimeData.json
#>
[CmdletBinding()]
param(
    [string]$Checkout = (Split-Path -Parent $PSScriptRoot),
    [string]$ScratchDir = '',
    [string]$CorpusRt = '',
    [ValidateSet('all', 'setup', 'build', 'test', 'measure', 'allocator', 'info')]
    [string]$Phase = 'all',
    [int]$Rounds = 2,
    [string[]]$WaitQuiet = @(),
    [datetime]$WaitUntil = [datetime]::MaxValue,
    [int]$RoundGapMinutes = 0,
    [string]$Image = 'python:3.12-slim',
    [string]$RustcVersion = '1.100.0',
    [int]$Runs = 7,
    [int]$Iterations = 20000
)

$ErrorActionPreference = 'Stop'
# The tables contain a micro sign; read the container's output as UTF-8.
[Console]::OutputEncoding = [Text.UTF8Encoding]::new()
$OutputEncoding = [Text.UTF8Encoding]::new()
# `pwsh -File ... -WaitQuiet a,b` delivers one string "a,b"; accept both forms.
$WaitQuiet = @($WaitQuiet | ForEach-Object { $_ -split ',' } | Where-Object { $_ })
$Checkout = (Resolve-Path $Checkout).Path
if (-not $ScratchDir) {
    $base = if ($env:OPENBYD_BENCH_SCRATCH) { $env:OPENBYD_BENCH_SCRATCH } else { Join-Path $Checkout 'target\bench-scratch' }
    $ScratchDir = Join-Path $base 'linux'
}
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$results = Join-Path $ScratchDir "results\$stamp"
New-Item -ItemType Directory -Force $ScratchDir, $results | Out-Null

function Get-HostLoad {
    $cpu = (Get-Counter '\Processor(_Total)\% Processor Time' -SampleInterval 1 -MaxSamples 5 |
        ForEach-Object { $_.CounterSamples.CookedValue } | Measure-Object -Average).Average
    $busy = Get-CimInstance Win32_Process | Where-Object { $_.Name -match '^(python|fake_server|cargo|rustc)' -and $_.CommandLine -match 'run_all\.py|load\.py|fake_server|cargo|rustc|maturin|pytest' } |
        ForEach-Object { $_.Name } | Sort-Object -Unique
    '{0:N1}% of all logical processors busy (5 s mean); related processes: {1}' -f $cpu, $(if ($busy) { $busy -join ', ' } else { 'none' })
}

function Invoke-InContainer([string]$Name) {
    $mounts = @('-v', "${Checkout}:/src:ro", '-v', "${ScratchDir}:/work")
    $envs = @('-e', "RUSTC_VERSION=$RustcVersion", '-e', "RUNS=$Runs", '-e', "ITERATIONS=$Iterations")
    if ($CorpusRt) {
        $mounts += @('-v', "$((Resolve-Path $CorpusRt).Path):/corpus-rt/VehicleRealtimeData.json:ro")
        $envs += @('-e', 'RT_CORPUS=/corpus-rt/VehicleRealtimeData.json')
    }
    $log = Join-Path $results "$Name.txt"
    Write-Host "== phase $Name (log: $log)"
    # The script is read from the read-only checkout; --rm removes the container afterwards.
    $script = $Name -replace '-\d+$', ''   # measure-1, measure-2 run the "measure" phase
    & docker run --rm --name "openbyd-linux-$Name" @mounts @envs $Image bash /src/bench/linux_docker.sh $script 2>&1 |
        Tee-Object -FilePath $log
    if ($LASTEXITCODE -ne 0) { throw "phase $Name failed with exit code $LASTEXITCODE" }
}

function Wait-Quiet {
    if (-not $WaitQuiet) { return }
    $pattern = ($WaitQuiet | ForEach-Object { [regex]::Escape($_) }) -join '|'
    while ($true) {
        # Only real benchmark processes: shells and this script mention the words in their own command lines.
        $running = @(Get-CimInstance Win32_Process | Where-Object {
                $_.ProcessId -ne $PID -and $_.Name -match '^(python|fake_server|cargo|rustc)' -and ($_.CommandLine -match $pattern -or $_.Name -match $pattern) })
        if (-not $running) { return }
        if ((Get-Date) -ge $WaitUntil) { Write-Warning "still busy at ${WaitUntil}: measuring anyway"; $script:fellBack = $true; return }
        Write-Host ("{0} waiting for {1} process(es): {2}" -f (Get-Date -Format 'HH:mm:ss'), $running.Count, (($running.Name | Sort-Object -Unique) -join ', '))
        Start-Sleep -Seconds 60
    }
}

$all = $Phase -eq 'all'
if ($all -or $Phase -eq 'setup') { Invoke-InContainer 'setup' }
if ($all -or $Phase -eq 'build') { Invoke-InContainer 'build' }
if ($all -or $Phase -eq 'info') { Invoke-InContainer 'info' }
if ($all -or $Phase -eq 'test') { Invoke-InContainer 'test' }
if ($all -or $Phase -eq 'measure') {
    Wait-Quiet
    1..$Rounds | ForEach-Object {
        if ($_ -gt 1 -and $script:fellBack -and $RoundGapMinutes -gt 0) { Start-Sleep -Seconds (60 * $RoundGapMinutes) }
        $load = Get-HostLoad
        Write-Host "host load before measurement round ${_}: $load"
        "round ${_} at $(Get-Date -Format s): $load" | Add-Content (Join-Path $results 'host-load.txt')
        Invoke-InContainer "measure-$_"
    }
}
if ($all -or $Phase -eq 'allocator') {
    $load = Get-HostLoad
    Write-Host "host load before the allocator comparison: $load"
    "allocator at $(Get-Date -Format s): $load" | Add-Content (Join-Path $results 'host-load.txt')
    Invoke-InContainer 'allocator'
}

Write-Host "Results: $results"
Write-Host "Scratch size ($ScratchDir):"
& docker run --rm -v "${ScratchDir}:/work:ro" $Image du -sh /work/rustup /work/cargo /work/target /work/venv /work/pip-cache /work/wheels /work/src
