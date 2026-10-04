<#
.SYNOPSIS
  Build and run orca.

.DESCRIPTION
  Honestly, this script does very little, and that is deliberate. Two config
  files already do the real work:

    rust-toolchain.toml  picks stable-x86_64-pc-windows-gnu
    .cargo/config.toml   pins the target directory

  So a bare `cargo run --bin orca` is already correct, and you do not need this
  to launch the app. What it adds is the log:

      ./tools/run.ps1                # build, launch, write run.log
      cargo run --bin orca           # identical, but output only in the console

  The log is worth having. The launcher reports its own lifecycle decisions, and
  most questions about it ("did it open?", "is it behind another window?",
  "is the tray icon there?") are answered by reading a few lines rather than by
  watching a window. A bug this morning was invisible on screen and obvious in
  the log.

.EXAMPLE
  ./tools/run.ps1
  ./tools/run.ps1 -BuildOnly
  ./tools/run.ps1 -Gate
#>
[CmdletBinding()]
param(
  [switch]$BuildOnly,
  [switch]$Gate,
  [string]$LogFile = ""
)

$ErrorActionPreference = 'Continue'
$repo = Split-Path -Parent $PSScriptRoot

# Cargo needs to run in the workspace root, and this script can be invoked from
# anywhere (`.\tools\run.ps1`, or by full path from another directory). Without
# this, cargo reports "could not find Cargo.toml" for a reason that has nothing to
# do with the user's actual mistake.
Push-Location $repo
try {
  . (Join-Path $PSScriptRoot 'toolchain.ps1')
  $null = Initialize-OrcaToolchain

  if (-not $LogFile) { $LogFile = Join-Path $repo 'run.log' }

  # A running orca holds the exe open, and cargo then fails to replace it with
  # "Access is denied" — which reads as a build failure but is not one. Stopping
  # the old process is the whole reason this script is more than `cargo run`.
  $stray = @(Get-Process -Name 'orca' -ErrorAction SilentlyContinue)
  if ($stray.Count) {
    $stray | Stop-Process -Force
    Write-Host "stopped a running orca so the rebuild can replace it" -ForegroundColor DarkGray
  }

  if ($Gate) {
    & pwsh -NoProfile -File (Join-Path $PSScriptRoot 'gate.ps1')
    if ($LASTEXITCODE -ne 0) { Write-Host "gate failed - not launching" -ForegroundColor Red; exit 1 }
  }

  # cargo writes progress to stderr, so `2>&1` produces ErrorRecord objects rather
  # than strings. Under a strict ErrorActionPreference the first "Compiling" line
  # would throw and look like a build failure. Read $LASTEXITCODE instead.
  Write-Host "building..." -ForegroundColor DarkGray
  $buildOut = & cargo build --bin orca 2>&1
  if ($LASTEXITCODE -ne 0) {
    $buildOut | ForEach-Object { "$_" } | Select-String -Pattern '^error' -Context 0,3 |
      Select-Object -First 5 | ForEach-Object { Write-Host "  $_" -ForegroundColor Red }
    Write-Host "build failed" -ForegroundColor Red
    exit 1
  }
  Write-Host "built ok" -ForegroundColor Green
  if ($BuildOnly) { exit 0 }

  Write-Host "log -> $LogFile" -ForegroundColor DarkGray
  Write-Host ""

  # `Tee-Object` rather than `>`: a plain redirect hides all output until the
  # process ends, which is useless for a launcher that is meant to be watched.
  & cargo run --bin orca 2>&1 | Tee-Object -FilePath $LogFile
  exit $LASTEXITCODE
}
finally {
  Pop-Location
}
