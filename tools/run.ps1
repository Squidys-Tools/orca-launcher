<#
.SYNOPSIS
  Build and run the launcher, with the right toolchain, in one command.

.DESCRIPTION
  Exists because the obvious command is wrong in two separate ways, and both
  failures are quiet:

    - `cargo run -p orca` on its own picks whichever rustc is first on PATH,
      and on this machine that is MSVC, which builds the wrong target. The
      result is a binary that runs and behaves subtly differently, or a link
      error about an MSVC-only import library. Setting PATH by hand works but
      is the thing people forget, and forgetting is silent.

    - the launcher is a *resident* single-instance app. A second copy does not
      open a second window: it finds the running one, tells it to show, and
      exits with code 2. So `cargo run` after a previous run appears to do
      nothing at all, which reads as "my change isn't in the binary". This
      script stops the old one first and says that it did.

  Anything ending in `unavailable:` in run.log is a degraded capability, not a
  failure — see docs/MANUAL-CHECKS.md.

.EXAMPLE
  ./tools/run.ps1                  # build (if needed) and run, logging to run.log
  ./tools/run.ps1 -Release         # the optimised build, much slower to compile
  ./tools/run.ps1 -NoLog           # do not write run.log
  ./tools/run.ps1 -KeepOld         # do not stop an already-running launcher
#>
[CmdletBinding()]
param(
  [switch]$Release,
  [switch]$NoLog,
  # Exists for exactly one case: debugging a second copy against a resident
  # first one. The single-instance handshake makes that a special request, not
  # the default.
  [switch]$KeepOld
)

$ErrorActionPreference = 'Continue'
$repo = Split-Path -Parent $PSScriptRoot

# --- toolchain: assert it, never hardcode it --------------------------------
# An earlier version of this file hardcoded
# `C:\Users\chris\.rustup\toolchains\stable-x86_64-pc-windows-gnu\bin`, which
# fails on any machine whose profile lives somewhere else — and there is more
# than one profile in play, so that was not hypothetical. The toolchain is
# already named in rust-toolchain.toml, so cargo picks it up by itself and all
# that is needed is to *check* it.
#
# `rustc -vV`, NOT `rustc --version`. The short form prints only
# `rustc 1.98.1 (48a229cea 2026-09-01)` — it has never contained the host
# triple, so grepping it for one fails on every machine including a correctly
# configured one. The verbose form has a `host:` line, which is the only place
# the triple appears.
$requiredHost = 'x86_64-pc-windows-gnu'
$rustcVerbose = (& rustc -vV) 2>&1 | Out-String
$hostTriple = ([regex]::Match($rustcVerbose, '(?m)^host:\s*(\S+)')).Groups[1].Value
if ($hostTriple -ne $requiredHost) {
  Write-Host "FATAL: the active rustc is not the $requiredHost toolchain." -ForegroundColor Red
  Write-Host "  rustc says its host is: ${hostTriple:-<nothing - rustc -vV failed>}" -ForegroundColor Red
  Write-Host "  rust-toolchain.toml pins the channel; something is overriding it." -ForegroundColor Yellow
  Write-Host "  Check with:  rustup show   (look for an override)" -ForegroundColor Yellow
  Write-Host "  Remove with: rustup override unset --path <this directory>" -ForegroundColor Yellow
  exit 2
}
$env:CARGO_TERM_COLOR = 'never'
$env:CARGO_TERM_PROGRESS_WHEN = 'never'
$env:RUST_BACKTRACE = '1'

$target = 'x86_64-pc-windows-gnu'

# --- a resident launcher would swallow this run ------------------------------
# Not cosmetic. The second instance hands its `Show` to the first and exits 2,
# so the process you just built never starts and the window you get belongs to
# the *old* binary. That is the single most confusing way this app can fail to
# look like it changed.
if (-not $KeepOld) {
  $old = @(Get-Process -Name 'orca' -ErrorAction SilentlyContinue)
  if ($old.Count) {
    $old | Stop-Process -Force
    Write-Host "  (stopped $($old.Count) running orca process(es) — otherwise the" -ForegroundColor DarkGray
    Write-Host "   new build would hand its window to the old one and exit)" -ForegroundColor DarkGray
  }
}

$profileArgs = if ($Release) { @('--release') } else { @() }
$exe = Join-Path $repo "target\$target\$(if ($Release) { 'release' } else { 'debug' })\orca.exe"

# --- build only if the binary is missing or stale ---------------------------
# `cargo run` would rebuild anyway, but asking cargo to build a 250 MB binary
# it has already built costs a second and re-prints a wall of `Fresh` lines.
$needsBuild = $true
if (Test-Path $exe) {
  $exeTime = (Get-Item $exe).LastWriteTimeUtc
  $newest = Get-ChildItem -Recurse -File -Path (Join-Path $repo 'crates') -Include *.rs,*.toml |
            Sort-Object LastWriteTimeUtc -Descending | Select-Object -First 1
  if ($newest -and $newest.LastWriteTimeUtc -le $exeTime) { $needsBuild = $false }
}

if ($needsBuild) {
  $sw = [Diagnostics.Stopwatch]::StartNew()
  $what = if ($Release) { 'building (release)' } else { 'building' }
  Write-Host "  orca run  |  $what ... " -NoNewline -ForegroundColor Gray
  & cargo build -p orca @profileArgs --target $target
  if ($LASTEXITCODE -ne 0) {
    Write-Host "`n  build FAILED (exit $LASTEXITCODE) - not running" -ForegroundColor Red
    exit $LASTEXITCODE
  }
  Write-Host "  ok in $([math]::Round($sw.Elapsed.TotalSeconds,1))s" -ForegroundColor Green
} else {
  Write-Host "  orca run  |  up to date" -ForegroundColor DarkGray
}

if (-not (Test-Path $exe)) {
  Write-Host "  FATAL: expected the binary at $exe and it is not there" -ForegroundColor Red
  exit 2
}

# --- run ---------------------------------------------------------------------
# Logged by default because most of the manual checks are answered by reading
# run.log rather than by looking at the screen, and a redirected pipeline is
# block-buffered - the lines only reach the file when the process exits.
$logPath = Join-Path $repo 'run.log'
Write-Host "  starting; the window is hidden until you press the hotkey." -ForegroundColor DarkGray
Write-Host "  log: $logPath" -ForegroundColor DarkGray
Write-Host "" -ForegroundColor DarkGray

if ($NoLog) {
  & $exe
} else {
  & $exe 2>&1 | Tee-Object -FilePath $logPath
}
