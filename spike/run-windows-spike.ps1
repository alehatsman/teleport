# teleport M1 spike -- Windows verification
#
# Run this from a REAL interactive Windows Terminal / PowerShell window.
# Do NOT run it through WSL interop (wsl.exe, or from inside the Linux shell) --
# that's exactly the bridge that hung every non-externally-killed test when this
# was tried from there. This script must be launched by double-clicking, or from
# a PowerShell/cmd window you opened normally on Windows.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File .\run-windows-spike.ps1
#
# Writes a full transcript to teleport-m1-spike-windows-results.txt next to this
# script, and prints it to the console as it runs.

$ErrorActionPreference = "Continue"
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$exeDir    = Join-Path $scriptDir "target\x86_64-pc-windows-gnu\debug"
$outFile   = Join-Path $scriptDir "teleport-m1-spike-windows-results.txt"

if (-not (Test-Path (Join-Path $exeDir "s1_reaper.exe"))) {
    Write-Error "Can't find s1_reaper.exe under $exeDir -- run this script from the spike/ dir it was checked out in, or fix `$exeDir at the top of this file."
    exit 1
}

"teleport M1 spike -- Windows results" | Out-File $outFile
"Host: $(hostname)   OS build: $([System.Environment]::OSVersion.Version)" | Out-File $outFile -Append
"Run at: $(Get-Date -Format o)" | Out-File $outFile -Append
"" | Out-File $outFile -Append

function Run-Spike {
    param(
        [string]$Label,
        [string]$Exe,
        [string[]]$ExeArgs,
        [int]$TimeoutSec
    )

    "=== $Label ===" | Tee-Object -FilePath $outFile -Append

    $stderrFile = Join-Path $env:TEMP "spike_stderr_$PID.txt"
    $stdoutFile = Join-Path $env:TEMP "spike_stdout_$PID.txt"
    Remove-Item $stderrFile, $stdoutFile -ErrorAction SilentlyContinue

    # Start-Process's -ArgumentList rejects an empty array (null/empty validation),
    # so only pass it when there's actually something to pass.
    $startArgs = @{
        FilePath              = Join-Path $exeDir $Exe
        NoNewWindow            = $true
        PassThru               = $true
        RedirectStandardError  = $stderrFile
        RedirectStandardOutput = $stdoutFile
    }
    if ($ExeArgs.Count -gt 0) {
        $startArgs.ArgumentList = $ExeArgs
    }
    $p = Start-Process @startArgs

    $finished = $p.WaitForExit($TimeoutSec * 1000)
    if (-not $finished) {
        "TIMEOUT after ${TimeoutSec}s -- killing PID $($p.Id)" | Tee-Object -FilePath $outFile -Append
        Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
    }

    Get-Content $stderrFile -ErrorAction SilentlyContinue | Tee-Object -FilePath $outFile -Append
    Get-Content $stdoutFile -ErrorAction SilentlyContinue | Tee-Object -FilePath $outFile -Append
    "" | Tee-Object -FilePath $outFile -Append

    Remove-Item $stderrFile, $stdoutFile -ErrorAction SilentlyContinue
}

# S0 -- control: no ConPTY at all, does `cmd /c "exit 0"` reap normally?
# Added after the first real run showed every graceful-exit ConPTY child hang;
# this isolates whether ConPTY is the variable.
Run-Spike "S0 control (no pty)" "s0_control.exe" @() 15

# S1 -- who reaps the child (see docs/15-open-questions.md#s1)
Run-Spike "S1 exit0 / poll"          "s1_reaper.exe" @("exit0","poll")             15
Run-Spike "S1 exit0 / blocking"      "s1_reaper.exe" @("exit0","blocking")         15
Run-Spike "S1 exit7 / poll"          "s1_reaper.exe" @("exit7","poll")             15
Run-Spike "S1 exit7 / blocking"      "s1_reaper.exe" @("exit7","blocking")         15
Run-Spike "S1 sigkill / poll"        "s1_reaper.exe" @("sigkill","poll")           15
Run-Spike "S1 sigkill / blocking"    "s1_reaper.exe" @("sigkill","blocking")       15
Run-Spike "S1 grandchild / blocking" "s1_reaper.exe" @("grandchild","blocking")    15

# S2 -- EOF is not exit (see docs/15-open-questions.md#s2)
Run-Spike "S2 basic"      "s2_eof.exe" @("basic")      20
Run-Spike "S2 grandchild" "s2_eof.exe" @("grandchild") 20
Run-Spike "S2 midburst"   "s2_eof.exe" @("midburst")   20

# S3 -- a blocking write wedges terminate (see docs/15-open-questions.md#s3)
Run-Spike "S3 separate" "s3_blocking_write.exe" @("separate") 15
Run-Spike "S3 shared"   "s3_blocking_write.exe" @("shared")   15

# S4 -- does dropping the master close the pseudoconsole (see docs/15-open-questions.md#s4)
Run-Spike "S4 plain"      "s4_drop_master.exe" @("plain")      15
Run-Spike "S4 grandchild" "s4_drop_master.exe" @("grandchild") 15

# S5 -- W1 follow-up: is the graceful-exit hang cmd.exe-specific, or does it happen
# for ANY process attached to a ConPTY? Same exit0/exit7/sigkill shape as S1, but
# spawns mini_exit.exe (a trivial Rust binary, no shell, no console API calls beyond
# std's implicit runtime init) instead of cmd.exe. See docs/15-open-questions.md#w1
Run-Spike "S5 minimal exit0"   "s5_minimal.exe" @("exit0")   15
Run-Spike "S5 minimal exit7"   "s5_minimal.exe" @("exit7")   15
Run-Spike "S5 minimal sigkill" "s5_minimal.exe" @("sigkill") 15

Write-Host ""
Write-Host "Done. Full transcript: $outFile"
Write-Host "Send that file back (or paste its contents) to fold the Windows results into the docs."
