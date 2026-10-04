# orca

A Windows launcher. Windows-only, built with the GNU Rust toolchain.

## Status

**The launcher runs.** <kbd>Ctrl+Shift+Space</kbd> opens a rounded panel over
the desktop, typing ranks the results, <kbd>Enter</kbd> opens the selected one,
<kbd>Esc</kbd> hides the panel without exiting. It lives in the tray between
uses and can be started at login.

Working today:

- The popup and its frame — a rounded panel in a transparent window larger than
  itself, so the drop shadow is not clipped.
- Global hotkey, tray icon and menu, single-instance guard, autostart.
- Installed applications, commands and aliases from `config.toml`, environment
  variables, and file and folder search when `[files]` is enabled.
- Ranking by match tier — prefix, word boundary, substring, fuzzy — weighted by
  launch frequency and recency, with a SQLite usage store behind it.

Not built: clipboard, calculator, web search, result icons, a settings window, an
installer. The frosted-glass backdrop was deliberately
[removed](docs/ARCHITECTURE.md) while the feature list was this short; it is
deferred, not abandoned.

Two documents matter more than this one:

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — why the workspace is split the
  way it is, and every GPUI and Win32 fact that cost a debugging cycle.
- [docs/MANUAL-CHECKS.md](docs/MANUAL-CHECKS.md) — what a human has to look at,
  because the gate cannot see a window.

## Requirements

- Rust, GNU toolchain (`stable-x86_64-pc-windows-gnu`). Pinned in
  [`rust-toolchain.toml`](rust-toolchain.toml), so `rustup` picks the right
  host triple automatically.
- **There is no MSVC / Visual Studio requirement**, and no MSVC linker is
  installed. Every build must target the GNU triple.

### The PATH caveat — read this before your first build

If PowerShell resolves a standalone MSVC `cargo.exe` before `rustup`'s shim,
the build fails with confusing linker errors. Do not hand-assemble `PATH` for
this: `tools/gate.ps1` sets the toolchain and asserts it exists.

```powershell
./tools/gate.ps1              # build + test + fmt + clippy, one summary line
./tools/gate.ps1 -Quick       # skip the workspace build while iterating
./tools/gate.ps1 -Step clippy # re-run one step
```

Exit 0 means every step passed. On failure the real diagnostic is on the console
and the full log is in `target/gate-logs/`.

To run the app itself:

```powershell
./tools/run.ps1
```

It stops any launcher already running — otherwise the new build hands its window
to the old one and exits — then starts it and writes `run.log`. Most of the
manual checks are answered by reading that log rather than by looking at the
screen.

## Build and verify

`tools/gate.ps1` is the supported path and the only one worth trusting. The
underlying commands are below for when you need to run one of them directly —
every one of them needs `--target x86_64-pc-windows-gnu` on the front, even
though the pinned toolchain is already GNU-host, because an MSVC invocation
*silently* checks the wrong target and still exits 0.

```powershell
$env:PATH = "C:\Users\chris\.rustup\toolchains\stable-x86_64-pc-windows-gnu\bin;C:\Users\chris\.cargo\bin;" + $env:PATH

cargo build --target x86_64-pc-windows-gnu
cargo test  --target x86_64-pc-windows-gnu
cargo fmt --check
cargo clippy --target x86_64-pc-windows-gnu -- -D warnings
cargo run   --target x86_64-pc-windows-gnu -p orca
```

`cargo run -p orca` starts the real launcher, not a summary: it registers the
hotkey, installs the tray icon, and waits. Nothing appears on screen until you
press <kbd>Ctrl+Shift+Space</kbd>. The process stays alive with no window open.

## Workspace layout

| Crate            | Responsibility                                                   |
|------------------|------------------------------------------------------------------|
| `orca-core`      | Pure logic: types, matching, ranking, frecency, config, SQLite, provider traits. No GPUI, no Win32. |
| `orca-win`       | Windows integration: global hotkey, single instance, autostart, tray, foreground activation, installed-app enumeration. |
| `orca`           | The binary. Composition root and the only crate that touches GPUI. |
| `orca-probe`     | A throwaway harness that proved the popup could be opened at all, before there was an app. |

The split is the load-bearing decision, not an organisational one: it is what
lets the ranking be tested thousands of times without a desktop, and the
platform work be tested without a window. `AGENTS.md` has the rules for changing
it.

## Roadmap

Tracked in Linear as [orca](https://linear.app/squidys-tools/project/orca-819e663d5a1b),
one phase per milestone with a checkable exit condition:

0. **The shell** — mostly built; the ten-cycle toggle check and the tray icon are
   still unverified at runtime.
1. **Sources worth having** — clipboard, calculator, web search, aliases.
2. **Looking like the reference** — result icons, row polish, the frosted
   backdrop.
3. **Configuration and control** — a settings window, so the ranking weights can
   move without a rebuild.
4. **Shipping** — per-user installer, self-update, and the DPI and multi-monitor
   verification that is still outstanding.
