# orca

A Windows launcher. Windows-only, built with the GNU Rust toolchain.

## Status

**This is an empty scaffold. There is no launcher yet.** No window is drawn, no
hotkey is bound, no index is built. What exists is three crates with their
boundaries declared, a domain model, a ranking function, and a set of platform
seams that honestly report that they are not implemented.

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for why the workspace is split
the way it is.

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

Confirm the GNU toolchain actually won:

```powershell
rustc -vV   # host: x86_64-pc-windows-gnu
```

## Build and run

```powershell
./tools/run.ps1
```

That is the command you need. It locates the pinned toolchain, stops any copy of
orca already running, builds, launches, and saves the output to `run.log`. The
launcher opens no window at startup — it waits for `Ctrl+Shift+Space`, then shows
a popup. Press that again to hide it.

A bare `cargo run --bin orca` is also correct and does the same thing; the script
adds the log file and the "stop the old process" step. Other entry points:

```powershell
./tools/run.ps1 -BuildOnly   # compile without launching
./tools/run.ps1 -Gate        # run the full checks first, then launch
./tools/gate.ps1             # build, test, formatting, lints. Exit 0 = all passed
```

`tools/gate.ps1` is for checking a change, not for running the app. It asserts the
GNU toolchain before doing anything, because a wrong toolchain can "pass" while
checking the wrong target.

`.cargo/config.toml` pins `build.target`, so a bare `cargo run --bin orca` lands
in the same target directory as the gate. That is deliberate: when the two used
different directories, a stale binary was run for hours while fixes appeared to do
nothing.

## Workspace layout

| Crate            | Responsibility                                                   |
|------------------|------------------------------------------------------------------|
| `orca-core`      | Pure logic: types, ranking, config, SQLite, providers. No GPUI, no Win32. |
| `orca-win`       | Windows integration: global hotkey, single instance, autostart, tray, window enumeration. |
| `orca`           | The binary. Composition root; owns the GPUI app when it lands.      |

## Roadmap

Roughly in order, none of it started:

1. `orca-win`: real `RegisterHotKey`, single-instance mutex.
2. `orca-core`: SQLite schema and a filesystem provider.
3. `orca`: add the pinned `gpui` dependency and open a window.
4. Ranking against real data, then tune.
