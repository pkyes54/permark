# THROWAWAY SPIKE (feature/tabs-spike). Launches the spike build once per config, samples RAM of the process tree
# (PerMark + msedgewebview2 children) at MARK READY + MARK CYCLED, waits for the scripted run to exit by itself
# (kills the tree on timeout), writes one log per run + summary.csv. No mouse | keyboard input.
# Windows PowerShell 5.1:
#   powershell -ExecutionPolicy Bypass -File measure.ps1 -Exe <permark.exe> [-OutDir <dir>] [-Paths "a;b"] [-Repeat 1]
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [string]$OutDir = (Join-Path $env:TEMP "permark-tabs-spike-results"),
    [string]$Paths = "",
    [int]$Repeat = 1,
    [int]$TimeoutSec = 120,
    # CI only: sends real key presses (SendKeys) to the spike window. NEVER on a person's desktop.
    [switch]$Keys
)
$ErrorActionPreference = "Stop"
New-Item -ItemType Directory -Force $OutDir | Out-Null

# name, tabs, lazy
$configs = @(
    @("baseline-main", 0, 0),
    @("tabs1", 1, 0),
    @("tabs2", 2, 0),
    @("tabs4", 4, 0),
    @("tabs4-lazy", 4, 1)
)

function Get-Tree([int]$RootId) {
    $all = Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId, Name, WorkingSetSize, PrivatePageCount
    $ids = New-Object System.Collections.Generic.HashSet[int]
    [void]$ids.Add($RootId)
    $grew = $true
    while ($grew) {
        $grew = $false
        foreach ($p in $all) {
            if ($ids.Contains([int]$p.ParentProcessId) -and -not $ids.Contains([int]$p.ProcessId)) {
                [void]$ids.Add([int]$p.ProcessId); $grew = $true
            }
        }
    }
    $all | Where-Object { $ids.Contains([int]$_.ProcessId) }
}

function Measure-Tree([int]$RootId, [string]$Phase) {
    $tree = @(Get-Tree $RootId)
    $ws = ($tree | Measure-Object WorkingSetSize -Sum).Sum
    # PrivatePageCount = private bytes
    $priv = ($tree | Measure-Object PrivatePageCount -Sum).Sum
    $web = @($tree | Where-Object { $_.Name -like "msedgewebview2*" }).Count
    [pscustomobject]@{
        phase = $Phase; procs = $tree.Count; webview2_procs = $web
        working_set_mb = [math]::Round($ws / 1MB, 1); private_mb = [math]::Round($priv / 1MB, 1)
    }
}

function Wait-Mark([string]$Log, [string]$Mark, [System.Diagnostics.Process]$Proc, [int]$Sec) {
    $deadline = (Get-Date).AddSeconds($Sec)
    while ((Get-Date) -lt $deadline) {
        if ((Test-Path $Log) -and (Select-String -Path $Log -SimpleMatch $Mark -Quiet)) { return $true }
        if ($Proc.HasExited) { return $false }
        Start-Sleep -Milliseconds 200
    }
    return $false
}

function First-Ms([string]$Log, [string]$Pattern) {
    $line = Select-String -Path $Log -Pattern $Pattern | Select-Object -First 1
    if ($line) { return [int](($line.Line -split " ")[0]) } else { return $null }
}

if ($Keys) { $configs += , @("keys", 3, 0) }

function Send-SpikeKeys([System.Diagnostics.Process]$Proc) {
    Add-Type -AssemblyName System.Windows.Forms
    $wsh = New-Object -ComObject WScript.Shell
    Write-Host "   keys: activate=$($wsh.AppActivate($Proc.Id))"
    Start-Sleep -Milliseconds 700
    foreach ($k in @("^{TAB}", "^{TAB}", "^+{TAB}", "^3", "^1", "abc")) {
        [System.Windows.Forms.SendKeys]::SendWait($k); Start-Sleep -Milliseconds 1000
    }
}

$rows = @()
for ($r = 1; $r -le $Repeat; $r++) {
    foreach ($c in $configs) {
        $name = $c[0]; $tabs = $c[1]; $lazy = $c[2]
        $log = Join-Path $OutDir "$name-r$r.log"
        Remove-Item $log -ErrorAction SilentlyContinue
        $env:PERMARK_TABS_SPIKE = "$tabs"
        $env:PERMARK_TABS_SPIKE_LAZY = "$lazy"
        $env:PERMARK_TABS_SPIKE_PLAN = "1"
        $env:PERMARK_TABS_SPIKE_LOG = $log
        $env:PERMARK_TABS_SPIKE_PATHS = $Paths
        $env:PERMARK_TABS_SPIKE_HOLD_MS = $(if ($name -eq "keys") { "12000" } else { "6000" })
        Write-Host "== $name (run $r)"
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        $proc = Start-Process -FilePath $Exe -PassThru
        $samples = @()
        if (Wait-Mark $log "MARK READY" $proc $TimeoutSec) {
            Start-Sleep -Milliseconds 1500
            $samples += Measure-Tree $proc.Id "ready"
            if ($name -eq "keys") { Send-SpikeKeys $proc }
            if ($tabs -ge 2 -and (Wait-Mark $log "MARK CYCLED" $proc $TimeoutSec)) {
                Start-Sleep -Milliseconds 1500
                $samples += Measure-Tree $proc.Id "cycled"
            }
        } else {
            Write-Host "   no READY mark (see $log)"
        }
        if (-not $proc.WaitForExit($TimeoutSec * 1000)) {
            Write-Host "   timeout: killing tree"
            & taskkill /PID $proc.Id /T /F | Out-Null
        }
        $wall = $sw.ElapsedMilliseconds
        $firstLoad = if (Test-Path $log) { First-Ms $log "page-load finished" } else { $null }
        foreach ($s in $samples) {
            $rows += [pscustomobject]@{
                config = $name; run = $r; phase = $s.phase; procs = $s.procs; webview2_procs = $s.webview2_procs
                working_set_mb = $s.working_set_mb; private_mb = $s.private_mb
                first_page_load_ms = $firstLoad; exit_code = $proc.ExitCode; wall_ms = $wall
            }
        }
        if ($samples.Count -eq 0) {
            $rows += [pscustomobject]@{ config = $name; run = $r; phase = "failed"; first_page_load_ms = $firstLoad; exit_code = $proc.ExitCode; wall_ms = $wall }
        }
        Start-Sleep -Seconds 2
    }
}
"PERMARK_TABS_SPIKE", "PERMARK_TABS_SPIKE_LAZY", "PERMARK_TABS_SPIKE_PLAN", "PERMARK_TABS_SPIKE_LOG", "PERMARK_TABS_SPIKE_PATHS", "PERMARK_TABS_SPIKE_HOLD_MS" |
    ForEach-Object { Remove-Item "Env:$_" -ErrorAction SilentlyContinue }
$csv = Join-Path $OutDir "summary.csv"
$rows | Export-Csv -NoTypeInformation -Path $csv
$rows | Format-Table -AutoSize | Out-String -Width 200 | Write-Host
Write-Host "Logs + summary: $OutDir"
Write-Host "Check per log: 'activate ... focus=Ok(())', 'probe(after-activate) hasFocus=true' on the active tab only, 'WARN' lines, 'bounds' after resize."
