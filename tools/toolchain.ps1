# Locate the pinned toolchain and put it first on PATH.
#
# Do NOT hardcode a path like C:\Users\<you>\.rustup\toolchains\... in any script.
# That is how the tooling here broke on a machine where the path was correct:
# a `Test-Path` relative to an unresolved directory reported the toolchain as
# missing and told the user to install a toolchain they already had.
#
# `rustup which` asks rustup where the toolchain actually is. It cannot be wrong
# about a path, and it follows rust-toolchain.toml if the pin ever changes.
#
# Dot-source this from any script that needs to run cargo:
#   . (Join-Path $PSScriptRoot 'toolchain.ps1')

function Initialize-OrcaToolchain {
  <#
    Puts the pinned GNU toolchain ahead of everything on PATH.

    Returns the bin directory. Throws if the toolchain is absent, because a
    silently wrong toolchain is worse than a loud failure: MSVC clippy "passes"
    without ever checking the GNU target this project builds for.
  #>
  $cargo = Get-Command cargo -ErrorAction SilentlyContinue
  if (-not $cargo) {
    throw "cargo is not on PATH. Install Rust from https://rustup.rs"
  }

  $rustc = & rustup which --toolchain stable-x86_64-pc-windows-gnu rustc 2>$null
  if (-not $rustc) {
    throw ("The pinned toolchain is not installed. Run: " +
           "rustup toolchain install stable-x86_64-pc-windows-gnu")
  }

  $bin = Split-Path -Parent $rustc
  if ($env:PATH -notlike "*$bin*") {
    $env:PATH = "$bin;$env:PATH"
  }
  $bin
}
