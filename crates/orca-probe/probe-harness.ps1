# Reusable test harness for the GPUI launcher probe.
#
#   . .\probe-harness.ps1
#   Start-Probe -Run 1
#   Send-Hotkey; Show-Foreground
#   Send-Text "note"
#   Dump-Log -Run 1
#   Stop-Probe
#
# Assumes the workspace is built with the GNU toolchain (see rust-toolchain.toml).
# The probe's window class is Zed::Window; use it to confirm the popup really
# holds foreground. See docs/ARCHITECTURE.md for the findings this proves.

if (-not ("ProbeNative" -as [type])) {
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public class ProbeNative {
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern int GetWindowThreadProcessId(IntPtr h, out int pid);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);

  public static void Combo(byte ctrl, byte alt, byte key) {
    if (ctrl != 0) keybd_event(ctrl, 0, 0, UIntPtr.Zero);
    if (alt  != 0) keybd_event(alt,  0, 0, UIntPtr.Zero);
    keybd_event(key, 0, 0, UIntPtr.Zero);
    keybd_event(key, 0, 2, UIntPtr.Zero);
    if (alt  != 0) keybd_event(alt,  0, 2, UIntPtr.Zero);
    if (ctrl != 0) keybd_event(ctrl, 0, 2, UIntPtr.Zero);
  }

  // letters and spaces only; enough to prove WM_CHAR arrives
  public static void Type(string t) {
    foreach (char c in t) {
      byte vk;
      if (c >= 'a' && c <= 'z')      vk = (byte)('A' + (c - 'a'));
      else if (c >= 'A' && c <= 'Z') vk = (byte)c;
      else if (c == ' ')             vk = 0x20;
      else continue;
      if (c >= 'A' && c <= 'Z') keybd_event(0x10, 0, 0, UIntPtr.Zero);
      keybd_event(vk, 0, 0, UIntPtr.Zero);
      keybd_event(vk, 0, 2, UIntPtr.Zero);
      if (c >= 'A' && c <= 'Z') keybd_event(0x10, 0, 2, UIntPtr.Zero);
    }
  }

  public static string Who() {
    IntPtr h = GetForegroundWindow(); int pid;
    GetWindowThreadProcessId(h, out pid);
    var t = new StringBuilder(256); GetWindowTextW(h, t, 256);
    var c = new StringBuilder(256); GetClassNameW(h, c, 256);
    return "hwnd=" + h + " pid=" + pid + " class=" + c + " title='" + t + "'";
  }
}
'@
}

$script:GnuBin   = "C:\Users\chris\.rustup\toolchains\stable-x86_64-pc-windows-gnu\bin"
$script:ProbeDir = $PSScriptRoot
$script:ProbeExe = Join-Path $PSScriptRoot "..\..\target\x86_64-pc-windows-gnu\debug\orca-probe.exe"
if (-not (Test-Path $script:ProbeExe)) {
  $script:ProbeExe = Join-Path $PSScriptRoot "target\x86_64-pc-windows-gnu\debug\orca-probe.exe"
}

# Apply the toolchain on dot-source, not only when someone remembers to call
# Use-GnuPath. A silently-wrong toolchain is the failure mode worth designing
# away: MSVC clippy "passes" without ever checking our GNU target.
if ($env:PATH -notlike "*$($script:GnuBin)*") {
  $env:PATH = "$script:GnuBin;C:\Users\chris\.cargo\bin;" + $env:PATH
}

function Use-GnuPath {
  $env:PATH = "$script:GnuBin;C:\Users\chris\.cargo\bin;" + $env:PATH
}

function Stop-Probe {
  $killed = @()
  Get-Process -Name "orca-probe" -ErrorAction SilentlyContinue | ForEach-Object {
    $killed += $_.Id; Stop-Process -Id $_.Id -Force
  }
  if ($killed) { "stopped: $($killed -join ',')" } else { "stopped: none" }
  Start-Sleep -Seconds 2
}

function Build-Probe {
  Use-GnuPath
  Push-Location $script:ProbeDir
  cargo build --target x86_64-pc-windows-gnu --message-format short 2>&1 |
    Select-String -Pattern "^src.*error|^error|Finished" | Select-Object -First 12
  $code = $LASTEXITCODE
  Pop-Location
  "BUILD_EXIT=$code"
}

function Start-Probe {
  param([int]$Run)
  $script:Run = $Run
  $out = "$ProbeDir\run$Run.log"; $err = "$ProbeDir\run$Run.err"
  Remove-Item $out, $err -ErrorAction SilentlyContinue
  $p = Start-Process -FilePath $script:ProbeExe -PassThru `
         -RedirectStandardOutput $out -RedirectStandardError $err
  $p.Id | Set-Content "$ProbeDir\run$Run.pid"
  Start-Sleep -Seconds 5
  "pid=$($p.Id) alive=$([bool](Get-Process -Id $p.Id -ErrorAction SilentlyContinue))"
}

function Show-Foreground { [ProbeNative]::Who() }

function Send-Hotkey { [ProbeNative]::Combo(0x11, 0x12, 0x20); "hotkey sent" }
function Send-Text   { param([string]$T) [ProbeNative]::Type($T); "typed '$T'" }
function Send-Key    { param([byte]$K) [ProbeNative]::Combo(0,0,$K); "key $K sent" }

function Test-Alive {
  param([int]$Run)
  $pid0 = Get-Content "$ProbeDir\run$Run.pid"
  $p = Get-Process -Id $pid0 -ErrorAction SilentlyContinue
  if ($p) { "alive  private=$([math]::Round($p.PrivateMemorySize64/1MB,1))MB  threads=$($p.Threads.Count)" }
  else { "EXITED" }
}

function Dump-Log {
  param([int]$Run, [int]$ErrLines = 8)
  "--- stdout ---"; Get-Content "$ProbeDir\run$Run.log" -ErrorAction SilentlyContinue
  "--- stderr ---"; Get-Content "$ProbeDir\run$Run.err" -ErrorAction SilentlyContinue | Select-Object -First $ErrLines
}
