# Run this in an ADMINISTRATOR PowerShell window, on a freshly rebooted machine
# (no leftover conhost/mini_exit/OpenConsole zombies from prior spike runs -- see
# docs/15-open-questions.md#w1 for why that matters).
#
# Three runs, answering the open questions the single-snapshot trace left:
#
#   A. Undisturbed control -- no debugger at all. How long does the hang actually
#      last when nothing pokes at it? (The single-snapshot trace's target exited
#      ~1s after a non-invasive cdb attach; unclear if that's typical or the attach
#      nudging something loose.)
#   B. Multi-snapshot -- attach+dump conhost's ConsoleIoThread every ~1.5s across
#      the hang. Is it parked at the same VtIo::StartIfNeeded/ReadFile call chain
#      for the whole window, or does it move?
#   C. Early single snapshot, like the original trace, for a same-machine
#      same-conditions comparison point.

$ErrorActionPreference = "Continue"
$spikeDir = "C:\Users\aleh\projects\teleport\spike"
$cdb = "C:\Program Files (x86)\Windows Kits\10\Debuggers\x64\cdb.exe"
$outFile = "C:\Users\aleh\AppData\Local\Temp\claude\C--Users-aleh-projects\2f375835-0dda-470e-a65b-131a54b7bed9\scratchpad\w1-windbg-trace-multi-results.txt"

if (-not (Test-Path $cdb)) { Write-Error "cdb.exe not found at $cdb"; exit 1 }

"W1 multi-snapshot trace -- $(Get-Date -Format o)" | Out-File $outFile
"Host: $(hostname)   OS build: $([System.Environment]::OSVersion.Version)" | Out-File $outFile -Append
$preExisting = Get-Process conhost, OpenConsole, mini_exit -ErrorAction SilentlyContinue
"Pre-existing conhost/OpenConsole/mini_exit processes (should be ~0 if freshly rebooted): $($preExisting.Count)" | Tee-Object -FilePath $outFile -Append
"" | Out-File $outFile -Append

function Get-NewestMiniExit {
    Get-Process mini_exit -ErrorAction SilentlyContinue | Sort-Object StartTime -Descending | Select-Object -First 1
}
function Get-NewestConhost($before) {
    Get-Process conhost -ErrorAction SilentlyContinue | Where-Object { -not $before.ContainsKey($_.Id) } | Select-Object -First 1
}

# ---------- Run A: undisturbed control ----------
"=== RUN A: undisturbed control (no debugger) ===" | Tee-Object -FilePath $outFile -Append
Push-Location $spikeDir
$before = @{}; Get-Process conhost -ErrorAction SilentlyContinue | ForEach-Object { $before[$_.Id] = $true }
$pA = Start-Process -FilePath ".\target\debug\s5_minimal.exe" -ArgumentList "exit0" -NoNewWindow -PassThru -RedirectStandardError "$env:TEMP\w1_runA_stderr.txt"
Pop-Location
"s5_minimal launched, pid=$($pA.Id)" | Tee-Object -FilePath $outFile -Append
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$resolved = $false
while ($sw.ElapsedMilliseconds -lt 15000) {
    Start-Sleep -Milliseconds 250
    $mini = Get-NewestMiniExit
    if (-not $mini) {
        "  RESOLVED at $($sw.ElapsedMilliseconds)ms (mini_exit no longer present, undisturbed)" | Tee-Object -FilePath $outFile -Append
        $resolved = $true
        break
    }
}
if (-not $resolved) { "  STILL ALIVE at 15000ms, undisturbed" | Tee-Object -FilePath $outFile -Append }
"" | Tee-Object -FilePath $outFile -Append
Start-Sleep -Seconds 1

# ---------- Run B: multi-snapshot across the hang ----------
"=== RUN B: multi-snapshot (attach+dump conhost every ~1.5s) ===" | Tee-Object -FilePath $outFile -Append
Push-Location $spikeDir
$before = @{}; Get-Process conhost -ErrorAction SilentlyContinue | ForEach-Object { $before[$_.Id] = $true }
$pB = Start-Process -FilePath ".\target\debug\s5_minimal.exe" -ArgumentList "exit0" -NoNewWindow -PassThru -RedirectStandardError "$env:TEMP\w1_runB_stderr.txt"
Pop-Location
"s5_minimal launched, pid=$($pB.Id)" | Tee-Object -FilePath $outFile -Append
Start-Sleep -Milliseconds 800
$conhostB = Get-NewestConhost $before
if ($conhostB) { "conhost.exe pid=$($conhostB.Id)" | Tee-Object -FilePath $outFile -Append }

$sw = [System.Diagnostics.Stopwatch]::StartNew()
for ($i = 0; $i -lt 7; $i++) {
    $mini = Get-NewestMiniExit
    if (-not $mini) {
        "  [{0,6}ms] mini_exit already gone -- stopping snapshots" -f $sw.ElapsedMilliseconds | Tee-Object -FilePath $outFile -Append
        break
    }
    if ($conhostB -and (Get-Process -Id $conhostB.Id -ErrorAction SilentlyContinue)) {
        "--- snapshot $i at $($sw.ElapsedMilliseconds)ms (conhost ConsoleIoThread stack, ~1 100) ---" | Tee-Object -FilePath $outFile -Append
        $dump = & $cdb -pv -p $conhostB.Id -lines -c "~0kP 30; q" 2>&1
        # Trim the boilerplate, keep the stack + a marker of which module/offset it's at
        $dump | Where-Object { $_ -match "conhost!|KERNELBASE!|ntdll!|WARNING|not attached" } | Tee-Object -FilePath $outFile -Append
    } else {
        "  [{0,6}ms] conhost no longer present" -f $sw.ElapsedMilliseconds | Tee-Object -FilePath $outFile -Append
    }
    Start-Sleep -Milliseconds 1500
}
$stillAlive = Get-NewestMiniExit
"mini_exit still alive after snapshot loop: $([bool]$stillAlive)" | Tee-Object -FilePath $outFile -Append
"" | Tee-Object -FilePath $outFile -Append

Write-Host ""
Write-Host "Done. Full transcript: $outFile"
