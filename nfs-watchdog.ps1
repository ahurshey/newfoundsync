# Newfoundsync watchdog — keeps the headless server alive across the intermittent silent death.
#
#   Run:   powershell -ExecutionPolicy Bypass -File .\nfs-watchdog.ps1
#   Stop:  Ctrl+C in this window.
#
# It checks every 5s; if newfoundsync isn't running it relaunches it headless. Before each
# relaunch it preserves the previous run's stderr (which now carries a full backtrace, via
# RUST_BACKTRACE) to nfs-crash-<timestamp>.log so a death can be diagnosed after the fact.

$exe = Join-Path $PSScriptRoot 'target\release\newfoundsync.exe'
$log = Join-Path $PSScriptRoot 'nfs-server.log'
$out = Join-Path $PSScriptRoot 'nfs-server.out'
$env:RUST_LOG = 'info'
$env:RUST_BACKTRACE = 'full'

if (-not (Test-Path $exe)) { Write-Host "Build first: cargo build --release -p newfoundsync"; exit 1 }

Write-Host "Newfoundsync watchdog started (checking every 5s). Ctrl+C to stop."
while ($true) {
    $p = Get-Process newfoundsync -ErrorAction SilentlyContinue
    if (-not $p) {
        $stamp = Get-Date -Format 'yyyy-MM-dd_HH-mm-ss'
        # Preserve the previous run's stderr (panic/backtrace) before Start-Process overwrites it.
        if ((Test-Path $log) -and (Get-Item $log).Length -gt 0) {
            Copy-Item $log (Join-Path $PSScriptRoot "nfs-crash-$stamp.log") -ErrorAction SilentlyContinue
            Write-Host "[$stamp] previous stderr was non-empty -> saved nfs-crash-$stamp.log (send it to debug the death)"
        }
        Write-Host "[$stamp] server down -> relaunching headless..."
        try {
            Start-Process -FilePath $exe -ArgumentList '--headless' -RedirectStandardError $log -RedirectStandardOutput $out -ErrorAction Stop
        } catch {
            # A transient file lock on the log/out must not kill the watchdog loop — retry next tick.
            Write-Host "[$stamp] relaunch failed: $($_.Exception.Message) -- retrying in 5s"
        }
    }
    Start-Sleep -Seconds 5
}
