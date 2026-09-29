<#
.SYNOPSIS
  Build and run orca, saving the log to run.log.

.DESCRIPTION
  The one command you need. It exists because the two things that used to go
  wrong were both easy to miss:

  * Building into the wrong directory. `cargo run` and the gate used to write to
    two different target folders, so you could run a binary built from code that
    was days old while every fix appeared to do nothing. `.cargo/config.toml` now
    pins one target for everything, and this script asserts the toolchain.

  * Losing the log. The launcher reports its own lifecycle decisions, and most
    questions about it are answered by reading them rather than by watching the
    window. The log is tee'd to run.log, and the path is printed at the end.

  Use `-BuildOnly` to compile without launching, and `-Gate` to run the full
  check (tests, formatting, lints) first. You do not normally need `-Gate`; it
  exists so a change can be verified before it is trusted.

.EXAMPLE
  ./run.ps1                 # build, launch, log to run.log
  ./run.ps1 -BuildOnly      # just check it compiles
  ./run.ps1 -Gate           # full checks, then launch
#>
[CmdletBinding()]
param(
  [switch]$BuildOnly,
  [switch]$Gate,
  [string]$LogFile = ""
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot

# Same toolchain assertion as the gate. A silently wrong toolchain is worse than
# a failing one: MSVC clippy "passes" without ever checking the GNU target.
$gnu = "C:\Users\chris\.rustup\toolchains\stable-x86_64-pc-windows-gnu\bin"
if (-not (Test-Path (Join-Path $gnu 'rustc.exe'))) {
  Write-Host "FATAL: GNU rustc not found at $gnu" -ForegroundColor Red
  Write-Host "rustup toolchain install stable-x86_64-pc-windows-gnu" -ForegroundColor Yellow
  exit 2
}
$env:PATH = "$gnu;C:\Users\chris\.cargo\bin;" + $env:PATH
$env:CARGO_TERM_COLOR = 'never'

if (-not $LogFile) { $LogFile = Join-Path $repo 'run.log' }

# A running orca holds the executable open and cargo then cannot replace it, with
# an "Access is denied" that looks like a build failure. Clear it first.
$stray = @(Get-Process -Name 'orca' -ErrorAction SilentlyContinue)
if ($stray.Count) {
  $stray | Stop-Process -Force
  Write-Host "stopped a running orca (pid $($stray.Id -join ', '))"
}

if ($Gate) {
  Write-Host "running full checks first..." -ForegroundColor DarkGray
  & pwsh -NoProfile -File (Join-Path $PSScriptRoot 'gate.ps1')
  if ($LASTEXITCODE -ne 0) {
    Write-Host "gate failed - not launching" -ForegroundColor Red
    exit 1
  }
}

Write-Host "building..." -ForegroundColor DarkGray

# cargo writes its progress to stderr, so `2>&1` produces ErrorRecord objects,
# not strings. Under `$ErrorActionPreference = 'Stop'` that would throw on the
# first "Compiling" line and look like a build failure. Temporarily relaxing it
# and reading $LASTEXITCODE is the reliable way to ask cargo how it went.
$previous = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
$buildOut = & cargo build --bin orca 2>&1
$code = $LASTEXITCODE
$ErrorActionPreference = $previous

if ($code -ne 0) {
  # Show the errors, not the whole log.
  $buildOut | ForEach-Object { "$_" } |
    Select-String -Pattern '^error' -Context 0,3 | Select-Object -First 6 |
    ForEach-Object { Write-Host "  $_" -ForegroundColor Red }
  Write-Host "build failed (exit $code)" -ForegroundColor Red
  exit 1
}

Write-Host "built ok" -ForegroundColor Green

if ($BuildOnly) { exit 0 }

Write-Host ""
Write-Host "  launching orca - press Ctrl+C here to stop" -ForegroundColor Cyan
Write-Host "  log: $LogFile" -ForegroundColor DarkGray
Write-Host ""

# Tee so the console still shows everything, and the file keeps it for reading
# afterwards. Encoding is UTF-8 so the arrow and box characters survive.
& cargo run --bin orca 2>&1 | Tee-Object -FilePath $LogFile
exit $LASTEXITCODE
