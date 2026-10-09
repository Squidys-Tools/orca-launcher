#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != Linux ]]; then
  printf '%s\n' 'The GPUI UI preview requires Linux.' >&2
  exit 2
fi

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"
channel="$(awk -F '\"' '/^[[:space:]]*channel[[:space:]]*=/ { print $2; exit }' rust-toolchain.toml)"
case "$channel" in
  *-x86_64-pc-windows-gnu) toolchain="${channel%-x86_64-pc-windows-gnu}-x86_64-unknown-linux-gnu" ;;
  *) printf 'Unsupported Rust toolchain pin: %s\n' "$channel" >&2; exit 1 ;;
esac
RUSTUP_TOOLCHAIN="$toolchain" cargo run -p orca-preview --target x86_64-unknown-linux-gnu
