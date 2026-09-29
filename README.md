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
the build fails with confusing linker errors. Prepend the GNU toolchain to
`PATH` in every shell you build from:

```powershell
$env:PATH = "C:\Users\chris\.rustup\toolchains\stable-x86_64-pc-windows-gnu\bin;C:\Users\chris\.cargo\bin;" + $env:PATH
```

Confirm the GNU toolchain actually won:

```powershell
rustc -vV   # host: x86_64-pc-windows-gnu
```

## Build and verify

```powershell
$env:PATH = "C:\Users\chris\.rustup\toolchains\stable-x86_64-pc-windows-gnu\bin;C:\Users\chris\.cargo\bin;" + $env:PATH

cargo build --target x86_64-pc-windows-gnu
cargo test  --target x86_64-pc-windows-gnu
cargo fmt --check
cargo clippy --target x86_64-pc-windows-gnu -- -D warnings
cargo run   --target x86_64-pc-windows-gnu -p orca
```

Running `cargo run -p orca` prints a startup summary and exercises both library
crates. It does not open a window.

The `--target` flag is explicit on every command even though the toolchain is
already GNU-host. It keeps the target unambiguous, and it is what the pinned
`gpui` dependency will need.

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
