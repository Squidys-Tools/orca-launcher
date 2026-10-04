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
the build fails with confusing linker errors. The scripts in `tools/` handle this
by asking `rustup which rustc` where the pinned toolchain actually is and putting
it first on `PATH`, so you do not need to do anything.

If you want to run cargo by hand, do the same thing:

```powershell
$env:PATH = (Split-Path (rustup which rustc)) + ";$env:PATH"
```

Never hardcode a path like `C:\Users\<you>\.rustup\toolchains\...`. Earlier
versions of these scripts did, and it made the tooling report a correctly
installed toolchain as missing and tell the user to install a toolchain they
already had.

## Build and run

```powershell
./tools/run.ps1              # build, launch, and write run.log
./tools/run.ps1 -BuildOnly   # compile without launching
./tools/run.ps1 -Gate        # run the full checks first, then launch
./tools/gate.ps1             # build, test, formatting, lints. Exit 0 = all passed
```

`run.ps1` stops any copy of orca already running before it launches. That step is
not cosmetic: the launcher is a resident single-instance app, so a second copy
does not open a second window — it hands its `Show` to the first and exits. Skip
it and the process you just built never starts, while the window you are looking
at belongs to the old binary. It is the single most confusing way this app can
fail to look like it changed.

The launcher opens no window at startup — it waits for <kbd>Ctrl+Shift+Space</kbd>,
then shows a popup. Press that again to hide it. Most of the manual checks are
answered by reading `run.log` rather than by looking at the screen.

A bare `cargo run --bin orca` is also correct and does the same thing; the script
adds the log file and the "stop the old process" step.

`tools/gate.ps1` is for checking a change, not for running the app. It asserts the
GNU toolchain before doing anything, because a wrong toolchain can "pass" while
checking the wrong target.

`.cargo/config.toml` pins `build.target`, so a bare `cargo run --bin orca` lands
in the same target directory as the gate. That is deliberate: when the two used
different directories, a stale binary was run for hours while fixes appeared to do
nothing. The corollary is that `Finished` with no `Compiling` line means nothing
rebuilt, so check the binary's timestamp before believing a fix is under test.

Every direct `cargo` invocation also needs `--target x86_64-pc-windows-gnu` on the
front, even though the pinned toolchain is already GNU-host, because an MSVC
invocation *silently* checks the wrong target and still exits 0.

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
