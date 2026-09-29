<#
.SYNOPSIS
  One command that decides whether the tree is shippable.

.DESCRIPTION
  Exists because these facts are easy to get wrong and expensive to notice:
    - this project only builds with the GNU toolchain, and a wrong toolchain
      can *silently* pass (MSVC clippy "succeeds" without checking our target)
    - `-D warnings` on clippy is load-bearing, not optional
    - omitting fmt/clippy is invisible unless something tracks that

  So: every step always runs, full output is always kept, and the summary is
  the only thing you need to read. Logs land in target/gate-logs/.

.EXAMPLE
  ./tools/gate.ps1                 # full gate
  ./tools/gate.ps1 -Quick          # skip the workspace build (fmt/clippy/test)
  ./tools/gate.ps1 -Step clippy    # re-run one step while iterating
  ./tools/gate.ps1 -TargetDir ..\..\shared-target   # share a warm target dir
#>
[CmdletBinding()]
param(
  [int]$Jobs = 0,
  [switch]$Quick,
  [string[]]$Step = @(),
  [string]$TargetDir = ""
)

$ErrorActionPreference = 'Continue'
$repo  = Split-Path -Parent $PSScriptRoot
$logDir = Join-Path $repo 'target\gate-logs'
New-Item -ItemType Directory -Force -Path $logDir | Out-Null

# --- toolchain: the single most important thing in this file -----------------
# Put the GNU bin dir ahead of everything, and *assert* it, because a silently
# wrong toolchain is worse than a failing one.
$gnu = "C:\Users\chris\.rustup\toolchains\stable-x86_64-pc-windows-gnu\bin"
if (-not (Test-Path (Join-Path $gnu 'rustc.exe'))) {
  Write-Host "FATAL: GNU rustc not found at $gnu" -ForegroundColor Red
  Write-Host "Install it with: rustup toolchain install stable-x86_64-pc-windows-gnu" -ForegroundColor Yellow
  exit 2
}
$env:PATH = "$gnu;C:\Users\chris\.cargo\bin;" + $env:PATH
$env:CARGO_TERM_COLOR = 'never'
$env:CARGO_TERM_PROGRESS_WHEN = 'never'
$env:RUST_BACKTRACE = '1'

if ($TargetDir) {
  $env:CARGO_TARGET_DIR = (Resolve-Path (Join-Path $repo $TargetDir) -ErrorAction SilentlyContinue)?.Path ??
                          (Join-Path $repo $TargetDir)
}

# A running probe holds orca-probe.exe open, and cargo then fails to replace it
# with "Access is denied" - a confusing failure that has nothing to do with the
# code. Clear it up front and say so.
$stray = @(Get-Process -Name 'orca-probe' -ErrorAction SilentlyContinue)
if ($stray.Count) {
  $stray | Stop-Process -Force
  Write-Host "  (stopped $($stray.Count) running orca-probe process(es) holding the exe)"
}

$target = 'x86_64-pc-windows-gnu'
$j = if ($Jobs -gt 0) { $Jobs } else { [Environment]::ProcessorCount }
$failed = 0

function Get-ConfiguredSteps {
  $all = @(
    @{ name = 'build';  desc = 'workspace build (all targets)'; jobs = $true;
       cmd  = 'cargo build --workspace --all-targets --target {T} -j {J}' },
    @{ name = 'test';   desc = 'unit tests';                  jobs = $true;
       cmd  = 'cargo test --workspace --target {T} -j {J}' },
    # `cargo fmt` takes no -j, and clippy needs -j *before* the `--` separator
    # or it forwards the flag to clippy-driver as an unknown option.
    @{ name = 'fmt';    desc = 'formatting (no drift allowed)'; jobs = $false;
       cmd  = 'cargo fmt --all --check' },
    @{ name = 'clippy'; desc = 'lints, warnings are errors';     jobs = $true;
       cmd  = 'cargo clippy --workspace --all-targets --target {T} -j {J} -- -D warnings' }
  )
  if ($Step.Count) { $all = $all | Where-Object { $Step -contains $_.name } }
  elseif ($Quick)  { $all = $all | Where-Object { $_.name -ne 'build' } }
  return $all
}

$steps = Get-ConfiguredSteps
if (-not $steps) { Write-Host "No steps matched -Step $Step" -ForegroundColor Red; exit 2 }

Write-Host ""
Write-Host "  orca gate  |  $($steps.Count) step(s)  |  -j$j  |  target: $target" -ForegroundColor DarkGray
Write-Host ("  " + ("-" * 74)) -ForegroundColor DarkGray

$sw = [Diagnostics.Stopwatch]::StartNew()
$results = @()

foreach ($s in $steps) {
  $cmdline = ($s.cmd -replace '\{T\}', $target) -replace '\{J\}', $j
  if (-not $s.jobs) { $cmdline = ($s.cmd -replace '\{T\}', $target) }
  $label   = "  $($s.name.PadRight(7)) $($s.desc)"
  Write-Host $label -NoNewline -ForegroundColor Gray

  $out = Join-Path $logDir "$($s.name).log"
  $stepSw = [Diagnostics.Stopwatch]::StartNew()
  # Full output to a file. Never truncate: a truncated error is how you end up
  # debugging a warning that was never the actual failure.
  & ([scriptblock]::Create($cmdline)) *> $out
  $code = $LASTEXITCODE
  $stepSw.Stop()

  $secs = [math]::Round($stepSw.Elapsed.TotalSeconds, 1)
  if ($code -eq 0) {
    $tests = ''
    if ($s.name -eq 'test') {
      $m = Select-String -Path $out -Pattern 'test result: ok\. (\d+) passed' -AllMatches |
           ForEach-Object { $_.Matches } | ForEach-Object { [int]$_.Groups[1].Value }
      if ($m) { $tests = "  ($((($m | Measure-Object -Sum).Sum)) tests)" }
    }
    Write-Host "  PASS  ${secs}s$tests" -ForegroundColor Green
    $results += [pscustomobject]@{ step = $s.name; ok = $true; secs = $secs; note = '' }
  } else {
    $failed++
    Write-Host "  FAIL  ${secs}s  (exit $code)" -ForegroundColor Red
    # Show the actual diagnostic, with a little context, straight from the log.
    $bad = Select-String -Path $out -Pattern '^(error|warning)' -Context 0,4 |
           Select-Object -First 4
    foreach ($b in $bad) {
      foreach ($l in @($b.Line) + $b.Context.PostContext) {
        if ($l.Trim()) { Write-Host "        $l" -ForegroundColor Red }
      }
      Write-Host "        ---" -ForegroundColor DarkRed
    }
    $results += [pscustomobject]@{ step = $s.name; ok = $false; secs = $secs; note = "exit $code" }
  }
}
$sw.Stop()

Write-Host ("  " + ("-" * 74)) -ForegroundColor DarkGray
$summary = ($results | ForEach-Object {
  "{0}={1}" -f $_.step, $(if ($_.ok) { 'pass' } else { 'FAIL' })
}) -join '  '
Write-Host "  $summary   [$([math]::Round($sw.Elapsed.TotalSeconds,1))s total]" -ForegroundColor $(
  if ($failed) { 'Red' } else { 'Green' })
Write-Host "  logs: target\gate-logs\" -ForegroundColor DarkGray
Write-Host ""

if ($failed) { exit 1 } else { exit 0 }
