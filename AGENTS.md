# Working agreement for agents on this repo

Read this before running anything. It exists because the two most expensive
mistakes here are both silent: a wrong toolchain, and a truncated error.

## Build: GNU only, and never trust the default toolchain

This project targets `x86_64-pc-windows-gnu` exclusively. An MSVC clippy run
will *succeed* while checking the wrong target, so "it compiled" is not evidence
on its own.

**Do not hand-assemble PATH.** Use `tools/gate.ps1`, which sets the toolchain and
asserts it exists. If you must call cargo directly:

```powershell
$env:PATH = "C:\Users\chris\.rustup\toolchains\stable-x86_64-pc-windows-gnu\bin;C:\Users\chris\.cargo\bin;" + $env:PATH
```

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
