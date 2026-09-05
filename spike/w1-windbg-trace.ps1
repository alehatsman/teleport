# Run this in an ADMINISTRATOR PowerShell window.
# Reproduces the W1 hang (a ConPTY child that exits gracefully is never reaped)
# and attaches cdb.exe non-invasively to dump the stuck thread's stack, plus the
# stacks of any conhost.exe/OpenConsole.exe process that started at the same
# moment (the ConPTY host side of the handshake docs/15-open-questions.md#w1
# suspects is where this is actually stuck).

$ErrorActionPreference = "Continue"
$spikeDir = "C:\Users\aleh\projects\teleport\spike"
$cdb = "C:\Program Files (x86)\Windows Kits\10\Debuggers\x64\cdb.exe"
$outFile = "C:\Users\aleh\AppData\Local\Temp\claude\C--Users-aleh-projects\2f375835-0dda-470e-a65b-131a54b7bed9\scratchpad\w1-windbg-trace-results.txt"

if (-not (Test-Path $cdb)) {
    Write-Error "cdb.exe not found at $cdb"
    exit 1
}

"W1 WinDbg trace -- $(Get-Date -Format o)" | Out-File $outFile
"Host: $(hostname)   OS build: $([System.Environment]::OSVersion.Version)" | Out-File $outFile -Append
"" | Out-File $outFile -Append

# Snapshot conhost/OpenConsole PIDs BEFORE launch, so we can tell which one is new.
$before = @{}
Get-Process conhost, OpenConsole -ErrorAction SilentlyContinue | ForEach-Object { $before[$_.Id] = $true }

Push-Location $spikeDir
$p = Start-Process -FilePath ".\target\debug\s5_minimal.exe" -ArgumentList "exit0" -NoNewWindow -PassThru -RedirectStandardError "$env:TEMP\w1_trace_stderr.txt"
"s5_minimal launched, pid=$($p.Id)" | Tee-Object -FilePath $outFile -Append
Pop-Location

Start-Sleep -Seconds 2

$mini = Get-Process mini_exit -ErrorAction SilentlyContinue | Sort-Object StartTime -Descending | Select-Object -First 1
if (-not $mini) {
    "mini_exit.exe not found after 2s -- either it exited already (interesting!) or spawn failed. Check $env:TEMP\w1_trace_stderr.txt" | Tee-Object -FilePath $outFile -Append
    Get-Content "$env:TEMP\w1_trace_stderr.txt" -ErrorAction SilentlyContinue | Tee-Object -FilePath $outFile -Append
    exit 1
}
"mini_exit.exe pid=$($mini.Id) StartTime=$($mini.StartTime)" | Tee-Object -FilePath $outFile -Append

# Find any conhost/OpenConsole that appeared since the snapshot -- that's ConPTY's
# hidden host process for this session.
$newHosts = Get-Process conhost, OpenConsole -ErrorAction SilentlyContinue | Where-Object { -not $before.ContainsKey($_.Id) }
"New conhost/OpenConsole processes since launch:" | Tee-Object -FilePath $outFile -Append
$newHosts | ForEach-Object { "  pid=$($_.Id) name=$($_.ProcessName) start=$($_.StartTime)" | Tee-Object -FilePath $outFile -Append }
"" | Tee-Object -FilePath $outFile -Append

function Dump-Process {
    param([int]$TargetPid, [string]$Label)
    "=== cdb dump: $Label (pid=$TargetPid) ===" | Tee-Object -FilePath $outFile -Append
    $dumpOut = & $cdb -pv -p $TargetPid -lines -c "~*kP 100; !locks; lm; q" 2>&1
    $dumpOut | Tee-Object -FilePath $outFile -Append
    "" | Tee-Object -FilePath $outFile -Append
}

Dump-Process -TargetPid $mini.Id -Label "mini_exit.exe (the stuck graceful-exit child)"
foreach ($h in $newHosts) {
    Dump-Process -TargetPid $h.Id -Label "$($h.ProcessName).exe (ConPTY host)"
}

# Confirm it's still alive/unaffected after the non-invasive dumps (proves cdb -pv
# didn't disturb it -- if it HAD died from our poking, that's a different, also
# interesting result, worth knowing either way).
Start-Sleep -Seconds 1
$stillAlive = Get-Process -Id $mini.Id -ErrorAction SilentlyContinue
"mini_exit.exe still alive after dump: $([bool]$stillAlive)" | Tee-Object -FilePath $outFile -Append

Write-Host ""
Write-Host "Done. Full transcript: $outFile"
