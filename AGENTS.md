# Working agreement for agents on this repo

Read this before running anything. It exists because the two most expensive
mistakes here are both silent: a wrong toolchain, and a truncated error.

## Build: GNU only, and never trust the default toolchain

The production app targets `x86_64-pc-windows-gnu`. An MSVC clippy run will
*succeed* while checking the wrong target, so "it compiled" is not evidence on
its own. `rust-toolchain.toml` pins the toolchain and `.cargo/config.toml` pins
the target, so a bare `cargo build` is already correct for the Windows app.

## Developing on Windows and Linux

- **Windows is the runtime source of truth.** Run `./tools/gate.ps1` to verify
  code changes; runtime checks use `./tools/run.ps1` and belong to the human.
- **Linux is for UI iteration only.** Use the Linux host toolchain and explicit
  target for static checks:
  ```bash
  RUSTUP_TOOLCHAIN=stable-x86_64-unknown-linux-gnu cargo build -p orca-preview --target x86_64-unknown-linux-gnu
  RUSTUP_TOOLCHAIN=stable-x86_64-unknown-linux-gnu cargo clippy -p orca-preview --target x86_64-unknown-linux-gnu -- -D warnings
  ```
  For a human visual check, run `./tools/preview.sh` or Setup's command named
  `UI preview (Linux)` in a graphical session. It reuses `ui.rs` with sample
  results and fake platform hooks, so it cannot verify Windows focus, tray,
  transparency, real app launching, or other Windows runtime behavior. Agents
  must not launch it or capture screenshots.

## Run the app: `./tools/run.ps1`

One command. It locates the toolchain, stops a copy of orca that would hold the
executable open, builds, launches, and writes `run.log`.

You do not need the gate to run the app. `tools/gate.ps1` (build + test + fmt +
clippy) is for checking a *change*, and running it is the agent's job, not the
user's. `./tools/run.ps1 -Gate` runs both if you want them together.

**Do not hand-assemble PATH, and never hardcode a toolchain path.** Use
`tools/gate.ps1`, which locates the toolchain via `rustup which rustc` and
asserts it exists. If you must call cargo directly:

```powershell
$env:PATH = (Split-Path (rustup which rustc)) + ";$env:PATH"
```

A hardcoded `C:\Users\<you>\.rustup\toolchains\...` is not just fragile, it
already caused a bug here: a `Test-Path` against a not-yet-resolved working
directory reported a correctly installed toolchain as missing, and the tooling
told the user to install a toolchain they already had. `tools/toolchain.ps1` is
the single source of truth for this and is what the scripts use.

Dot-sourcing `crates/orca-probe/probe-harness.ps1` also sets it automatically.

## Runtime verification: the human owns it, you do not

**Do not launch the app, drive it with synthetic input, or capture screenshots.**
Static verification is your job; runtime and visual verification is the user's.

Concretely, do not: run `tools/soak.ps1`, call the `probe-harness.ps1` send
functions, screenshot windows, or assert anything about frame timing, latency,
memory, or focus. Those runs are slow, environment-sensitive, and only the user
can judge the result.

This rule exists because it was learned the expensive way: automated keyboard
injection into a popup is a poor substitute for a person looking at the screen,
and it burns minutes per iteration while proving little.

When your change has a visible or interactive consequence, do this instead:

1. Make it compile clean under `tools/gate.ps1`.
2. State plainly, in your final message, what a human will need to check.
3. Add it to `docs/MANUAL-CHECKS.md` if it is not already listed.

Runtime findings that are *already* established stay valid as recorded evidence
— see the tables below and `docs/ARCHITECTURE.md`. Do not re-derive them, and do
not present new static reasoning as if it were observed at runtime.

| Question | Answer |
|---|---|
| Can the popup be toggled repeatedly? | **Yes** — 10 hide/show cycles confirmed by a human, `visible=true foreground=true` on every show |
| Was the "cannot reopen after Esc" bug ever about the hotkey? | No — it was the command pump (see `ARCHITECTURE.md`) |
| How fast is the popup? | 1st open ~1 s, every later open <30 ms. Cold start is the only slow part. |
| Is the tray icon working? | **Unknown — needs a human.** Two bugs, both real, both now fixed: `LoadImageW (IDI_APPLICATION)` failed with error 1813 because it is the wrong call for a *system* icon (`LoadIconW` is), and returning `None` afterwards sent `NIM_ADD` with a null `hIcon`, giving a blank slot. The stock icon is now loaded with `LoadIconW` and is never allowed to fail the install. See `ARCHITECTURE.md`. |
| How was it diagnosed? | By making each Win32 call name itself in the error. Do not re-add a shared error variant across unrelated calls. |
| How do you exit the app? | <kbd>Ctrl</kbd>+<kbd>Esc</kbd>. Do not add a second path; the tray entry is the intended one. |

## Verify with the gate, not with ad-hoc commands

```powershell
./tools/gate.ps1              # build + test + fmt + clippy, one summary line
./tools/gate.ps1 -Quick       # skip build while iterating
./tools/gate.ps1 -Step clippy # re-run one step
```

* Exit 0 = all steps passed. Exit 1 = a step failed, with the real diagnostic
  printed and the full log in `target/gate-logs/`.
* Always read `target/gate-logs/<step>.log` when a step fails. The console
  excerpt is only the first few lines; the log has everything.
* `clippy -- -D warnings` is load-bearing. Do not "fix" a warning by adding
  `#[allow]` to make the gate green — that is a failing, not a fix.

## Runtime verification (available, but not yours to run)

`tools/soak.ps1` toggles the probe and asserts: process survives, popup takes
foreground, every cycle paints, memory stays bounded. It exists for the *user*
to run on demand. You may read it and cite prior results, but do not invoke it.

```powershell
./tools/soak.ps1 -Cycles 10   # exit code is the verdict, for the human
```

Findings already proven at runtime — cite these, do not re-derive them:

| Question | Answer |
|---|---|
| Does a global hotkey work? | Yes, `RegisterHotKey`, must stay alive on a dedicated thread |
| Does the popup take foreground? | Yes, via `Window::activate_window`, class `Zed::Window` |
| Does `cx.activate(true)` work? | **No** — no-op on Windows. Use `win.activate_window()` |
| Does Esc kill the process? | Only under `QuitMode::Default`; fixed with `QuitMode::Explicit` |
| Is text input focused? | Only with `.track_focus(&focus)` on the dispatch node |
| Caret units? | `handle_input` speaks UTF-16, the model speaks UTF-8 bytes |

## Build and run through one path, or you will test old code

`.cargo/config.toml` sets `build.target = "x86_64-pc-windows-gnu"`, so `cargo run`
and `tools/gate.ps1` share one target directory.

**Do not add `--target` to one and not the other, and do not set
`CARGO_TARGET_DIR` for a single command.** Two different caches is how the
following happened: `cargo run --bin orca` built into `target/debug`, the gate
built into `target/x86_64-pc-windows-gnu/debug`, and a binary from the stale
`target/debug` cache ran for hours while three separate fixes appeared to have no
effect. The code under test was two days old and nobody could tell, because
`Finished` in 0.77s looks exactly like a fast successful build.

The check: **`Finished` with no `Compiling` lines means cargo did not rebuild.**
That is only ever correct immediately after a build. Verify with the file
timestamp before believing a fix is being tested:

```powershell
Get-Item target\x86_64-pc-windows-gnu\debug\orca.exe | Select-Object LastWriteTime
```

## Multi-agent rules

* **One primary writer per crate.** If two agents edit one crate, the merge is
  your problem, not theirs.
* **Root `Cargo.toml` and `Cargo.lock` have a single owner** (the integrator).
  Other agents report intended manifest changes as a diff in their final message.
* **Share one warm target dir** rather than creating per-agent ones. The GPUI
  graph is ~600 crates and 5.7 GB; a cold per-agent build costs ~11 minutes, and
  separate dirs multiply that. `target/` is already warm — reuse it.
  If you genuinely need isolation, use `tools/gate.ps1 -TargetDir <path>`.
* Keep `target/` out of git. Run artifacts (`run*.log`, `run*.png`) are ignored.

## Conventions

* Direct, concrete prose. No filler, no restating the obvious.
* Comments explain *why*, especially where the answer is non-obvious or was
  expensive to learn. Never narrate what the next line obviously does.
* Crate boundaries are strict: `orca-core` is pure (no GPUI, no Win32),
  `orca-win` owns all platform calls, `orca` owns the UI.
* Heavy work (search, indexing, ranking, I/O) runs on `BackgroundExecutor` via
  `cx.spawn`. Never block `render`.
* `docs/ARCHITECTURE.md` is the record of *why*. Update it when you learn
  something that would cost the next person the same experiment.
