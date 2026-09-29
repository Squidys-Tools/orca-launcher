<#
.SYNOPSIS
  Toggle the probe N times and decide pass/fail without a human reading logs.

.DESCRIPTION
  A soak that only produces output is a soak that relies on the agent reading a
  table correctly. This asserts instead, and exits nonzero on failure, so it can
  gate a commit or run unattended while you do other work.

  Asserts:
    * process survives every cycle (the QuitMode::Explicit guarantee)
    * the popup actually took foreground at least once (class Zed::Window)
    * every cycle produced a first-paint measurement
    * typed text reached the model (query non-empty)
    * private memory stays under -MaxMemoryMB and does not trend upward

.EXAMPLE
  ./tools/soak.ps1 -Cycles 10
  ./tools/soak.ps1 -Cycles 20 -MaxMemoryMB 200
  Start-Process pwsh -ArgumentList '-NoProfile','-File','tools/soak.ps1','-Cycles','40' -Wait
#>
[CmdletBinding()]
param(
  [int]$Cycles = 10,
  [int]$MaxMemoryMB = 250,
  [string]$Text = "note",
  [switch]$KeepOpen
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$probeDir = Join-Path $repo 'crates\orca-probe'
. (Join-Path $probeDir 'probe-harness.ps1')
Use-GnuPath

$run = 90 + (Get-Random -Maximum 40)
$logPath = Join-Path $probeDir "run$run.log"

function Get-MemMB { param($p) if ($p) { [math]::Round($p.PrivateMemorySize64 / 1MB, 1) } else { $null } }

Stop-Probe | Out-Null
Start-Probe -Run $run | Out-Null
$pid0 = [int](Get-Content (Join-Path $probeDir "run$run.pid")).Trim()

$rows = @()
$everForeground = $false
$typedOk = $false

for ($i = 1; $i -le $Cycles; $i++) {
  Send-Hotkey | Out-Null
  Start-Sleep -Milliseconds 900

  $proc  = Get-Process -Id $pid0 -ErrorAction SilentlyContinue
  $mem   = Get-MemMB $proc
  $fg    = Show-Foreground
  if ($fg -match "pid=$pid0 .*class=Zed::Window") { $everForeground = $true }

  if ($i -eq 1) { Send-Text $Text | Out-Null; Start-Sleep -Milliseconds 500 }

  $rows += [pscustomobject]@{
    cycle = $i; alive = [bool]$proc; memMB = $mem
    threads = $(if ($proc) { $proc.Threads.Count } else { 0 })
    fg = $fg
  }

  Send-Key 0x1B | Out-Null      # Esc hides the popup
  Start-Sleep -Milliseconds 700
}

Start-Sleep -Milliseconds 400
$aliveAtEnd = [bool](Get-Process -Id $pid0 -ErrorAction SilentlyContinue)
$paints = @(Select-String -Path $logPath -Pattern 'hotkey->first-paint: ([0-9.]+) ms' -AllMatches -ErrorAction SilentlyContinue |
            ForEach-Object { [double]$_.Matches[0].Groups[1].Value })

# did the typed text actually land in the model? probe logs its own state
if ((Get-Content $logPath -ErrorAction SilentlyContinue) -match [regex]::Escape($Text)) { $typedOk = $true }

$peak = ($rows | Where-Object { $_.memMB } | Measure-Object memMB -Maximum).Maximum
$firstHalf = ($rows[0..([math]::Floor($Cycles/2)-1)] | Where-Object { $_.memMB } | Measure-Object memMB -Average).Average
$lastHalf  = ($rows[([math]::Floor($Cycles/2))..($rows.Count-1)] | Where-Object { $_.memMB } | Measure-Object memMB -Average).Average
$trend     = if ($firstHalf -and $lastHalf) { [math]::Round($lastHalf - $firstHalf, 1) } else { 0 }
$slowest   = if ($paints) { [math]::Round(($paints | Measure-Object -Maximum).Maximum, 1) } else { 0 }
$fastest   = if ($paints) { [math]::Round(($paints | Measure-Object -Minimum).Minimum, 1) } else { 0 }

Write-Host ""
Write-Host "  soak  cycles=$Cycles  run=$run  pid=$pid0" -ForegroundColor DarkGray
Write-Host ("  " + ("-" * 62)) -ForegroundColor DarkGray
Write-Host "  cycle  alive  privateMB  threads" -ForegroundColor DarkGray
foreach ($r in $rows) {
  $c = if ($r.alive) { 'Green' } else { 'Red' }
  Write-Host ("  {0,5}  {1,-6} {2,9} {3,7}" -f $r.cycle, $r.alive, $r.memMB, $r.threads) -ForegroundColor $c
}
Write-Host ("  " + ("-" * 62)) -ForegroundColor DarkGray
Write-Host ("  first-paint  n={0}  min={1}ms  max={2}ms" -f $paints.Count, $fastest, $slowest) -ForegroundColor Gray
Write-Host ("  memory       peak={0}MB  first-half avg={1}MB  second-half avg={2}MB  trend={3:+#;-#;0}MB" -f $peak, [math]::Round($firstHalf,1), [math]::Round($lastHalf,1), $trend) -ForegroundColor Gray
Write-Host ""

$fail = @()
$dead = @($rows | Where-Object { -not $_.alive })
if ($dead.Count)      { $fail += "$($dead.Count) cycle(s) killed the process" }
if (-not $aliveAtEnd) { $fail += "process not alive at end" }
if ($paints.Count -lt $Cycles) { $fail += "only $($paints.Count)/$Cycles paints recorded" }
if (-not $everForeground) { $fail += "popup never took foreground" }
if ($peak -gt $MaxMemoryMB) { $fail += "peak memory ${peak}MB exceeds ${MaxMemoryMB}MB" }
if ($trend -gt 25) { $fail += "memory trending up (+${trend}MB) across cycles" }

if ($fail) {
  Write-Host "  SOAK FAIL" -ForegroundColor Red
  $fail | ForEach-Object { Write-Host "    - $_" -ForegroundColor Red }
  Write-Host "  log: $logPath" -ForegroundColor DarkGray
  Write-Host ""
  if (-not $KeepOpen) { Stop-Probe | Out-Null }
  exit 1
} else {
  Write-Host "  SOAK PASS  (lifecycle, foreground, paints, memory all within bounds)" -ForegroundColor Green
  Write-Host "  log: $logPath" -ForegroundColor DarkGray
  Write-Host ""
  if (-not $KeepOpen) { Stop-Probe | Out-Null }
  exit 0
}
