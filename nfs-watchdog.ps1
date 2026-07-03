# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

# Newfoundsync watchdog — keeps the GUI server alive across the intermittent AMD-driver GUI hang.
#
#   Run:   powershell -ExecutionPolicy Bypass -File .\nfs-watchdog.ps1
#   Stop:  Ctrl+C in this window.
#
# Every 5s it:
#   * relaunches the GUI if the process is gone, AND
#   * kills + relaunches it if the window has been hung ("Not Responding") for ~15s.
# The windowed GUI intermittently hangs on AMD Radeon (AppHangB1) and either sits "Not Responding"
# or gets killed — both take every client offline. This keeps the GUI you want while self-healing.
# Before each relaunch it preserves the previous run's stderr (full backtrace via RUST_BACKTRACE)
# to nfs-crash-<timestamp>.log so a death can be diagnosed after the fact.

$exe = Join-Path $PSScriptRoot 'target\release\newfoundsync.exe'
$log = Join-Path $PSScriptRoot 'nfs-gui.log'
$out = Join-Path $PSScriptRoot 'nfs-gui.out'
$env:RUST_LOG = 'info'
$env:RUST_BACKTRACE = 'full'
$hangTicks = 0   # consecutive 5s checks the GUI has been Not Responding
$hangLimit = 3   # 3 * 5s = ~15s of "Not Responding" => treat as hung, kill + relaunch

if (-not (Test-Path $exe)) { Write-Host "Build first: cargo build --release -p newfoundsync"; exit 1 }

function Relaunch([string]$why) {
    $stamp = Get-Date -Format 'yyyy-MM-dd_HH-mm-ss'
    # Preserve the previous run's stderr (panic/backtrace) before Start-Process overwrites it.
    if ((Test-Path $log) -and (Get-Item $log).Length -gt 0) {
        Copy-Item $log (Join-Path $PSScriptRoot "nfs-crash-$stamp.log") -ErrorAction SilentlyContinue
        Write-Host "[$stamp] saved previous stderr -> nfs-crash-$stamp.log"
    }
    Write-Host "[$stamp] $why -> relaunching GUI..."
    try {
        Start-Process -FilePath $exe -RedirectStandardError $log -RedirectStandardOutput $out -ErrorAction Stop
    } catch {
        # A transient file lock on the log/out must not kill the watchdog loop — retry next tick.
        Write-Host "[$stamp] relaunch failed: $($_.Exception.Message) -- retrying in 5s"
    }
}

Write-Host "Newfoundsync GUI watchdog started (5s checks; ~15s hang => kill+relaunch). Ctrl+C to stop."
while ($true) {
    $procs = @(Get-Process newfoundsync -ErrorAction SilentlyContinue)
    if ($procs.Count -eq 0) {
        $hangTicks = 0
        Relaunch "server down"
    } else {
        $proc = $procs[0]
        if (-not $proc.Responding) {
            $hangTicks++
            if ($hangTicks -ge $hangLimit) {
                $now = Get-Date -Format 'HH:mm:ss'
                Write-Host ("[{0}] GUI hung (Not Responding ~{1}s) -> killing + relaunching" -f $now, ($hangTicks * 5))
                try { Stop-Process -Id $proc.Id -Force -ErrorAction Stop } catch {}
                Start-Sleep -Milliseconds 800
                $hangTicks = 0
                Relaunch "GUI was hung"
            } else {
                Write-Host ("[{0}] GUI Not Responding ({1}/{2})..." -f (Get-Date -Format 'HH:mm:ss'), $hangTicks, $hangLimit)
            }
        } else {
            $hangTicks = 0
        }
    }
    Start-Sleep -Seconds 5
}
