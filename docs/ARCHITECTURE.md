# Architecture

## The three-crate split

```
        ┌──────────────────────────┐
        │          orca            │  binary, composition root
        │   (owns the GPUI app)    │
        └────┬────────────────┬────┘
             │                │
             ▼                ▼
    ┌─────────────────┐   ┌──────────────────┐
    │   orca-win      │   │    orca-core     │
    │  Win32 / shell  │   │  pure logic      │
    │  hotkey, tray,  │   │  types, ranking, │
    │  single inst.   │   │  config, index,  │
    │                 │   │  providers       │
    └─────────────────┘   └──────────────────┘
             \                /
              \              /
             dependencies point
             one way only, and
             never back toward orca
```

`orca` depends on both. `orca-win` does not depend on `orca-core` today; if it
ever needs a core type, that is the signal something has been misplaced.

## The no-GPUI-in-core rule

**`orca-core` must not depend on `gpui`, `gpui_platform`, `windows`,
`windows-sys`, or `winapi`.** This is the load-bearing constraint of the whole
layout, not a style preference.

The reason is testability and the cost of an upstream break:

- Ranking is the part of a launcher that is actually hard. It needs thousands
  of assertions run in milliseconds. If a `ResultItem` or a score is entangled
  with a GPUI `Window` or an `Entity<T>`, every one of those assertions has to
  stand up a GPU surface and a message loop. A ranking function you can only
  test by running a window is a ranking function you will not test.
- GPUI is a git pin into Zed, which renames and reshapes its types constantly
  (see the risk note below). If `orca-core` transitively depended on it, a
  routine `cargo update` could break the domain logic — the half of the app
  that has nothing to do with Zed. Keeping the dependency out of `orca-core`
  bounds the blast radius to the UI layer.
- Core logic that compiles on a non-Windows host can be checked by anyone with
  a Mac or a Linux box. That option disappears the moment a `windows` crate
  enters the graph.

`orca-win` is the only crate allowed to bind Win32, and it is the only crate
allowed to know that Windows exists. It is not allowed to know about GPUI.

### How the rule is enforced

Enforcement is mechanical, not cultural. The check is:

```powershell
cargo tree -p orca-core --target x86_64-pc-windows-gnu
```

The output must contain exactly one line: `orca-core`. If it grows a `gpui`, a
`windows-sys`, or anything else, the rule has been broken. This is the check to
put in CI when CI exists — it is four lines of `cargo metadata` piping, and it is
the reason the dependency lists in `crates/orca-core/Cargo.toml` and
`crates/orca-win/Cargo.toml` carry a comment saying not to edit them.

## Layering rules

1. **Dependencies point one way.** `orca` → {`orca-core`, `orca-win`}. Never
   from a library crate back to the binary.
2. **`orca-core` is pure.** Data plus total functions over that data. No clock,
   no filesystem, no environment, no threads, no global state. Anything that
   needs I/O is a trait in `orca-core` and an implementation in `orca-win` or in
   the binary.
3. **`orca-win` is the only place Win32 is called.** It exposes traits
   (`GlobalHotkey`, `SingleInstance`) so the rest of the app is written against
   a seam and can be driven by a fake in tests.
4. **Stubs fail, they do not pretend.** Every unimplemented platform operation
   returns `NotImplemented`. A caller that ignores the error still cannot
   conclude that the operation worked — `is_registered()` stays `false`. This
   is why `Win32GlobalHotkey::register` does not set its `registered` field
   before returning its error.
5. **`orca` is a composition root, not a library.** Logic that is not about
   wiring belongs in one of the other two crates.

## Adding GPUI later

`orca` does **not** depend on `gpui` yet, on purpose. Without it the workspace
builds and tests in seconds. With it, a cold resolve takes roughly 7 minutes and
a cold build roughly 10. Keeping it out of the initial commit means the
architecture can be reviewed, merged, and verified before anyone waits on
Zed's dependency graph.

The pin, when it lands, is:

```toml
[workspace.dependencies]
gpui          = { git = "https://github.com/zed-industries/zed", rev = "bcf6582" }
gpui_platform = { git = "https://github.com/zed-industries/zed", rev = "bcf6582" }
```

Full commit: `bcf6582ce3500df93a8a39366640173e6786cea6`

Verified against the GitHub API on 2026-09-28: the abbreviated `rev = "bcf6582"`
resolves to that commit, and it does touch `crates/gpui_util/`, consistent
with a gpui pin.

The short `rev = "bcf6582"` is what goes in the manifest, so the abbreviated
form in the snippet above is the operative value. Note that a *different*
40-character SHA was originally recorded here and it does **not** exist
upstream (GitHub returns 422 "no commit found" for it); the digits at
positions 24–31 are `66640173`, not `66666673`. Do not trust any
hand-transcribed full SHA for this pin — resolve `bcf6582` and read the full
hash back, or just use the abbreviated `rev`, which git and cargo both accept.

To add it:

1. Uncomment the `[workspace.dependencies]` block in the root `Cargo.toml`.
2. Add to `crates/orca/Cargo.toml`:
   ```toml
   gpui = { workspace = true }
   gpui_platform = { workspace = true }
   ```
3. Build with the GNU PATH fix in place and
   `--target x86_64-pc-windows-gnu`. GPUI needs a GNU-compatible linker; an
   MSVC cargo on `PATH` will fail here in a way that looks like a missing
   dependency.
4. Re-run `cargo tree -p orca-core --target x86_64-pc-windows-gnu`. GPUI must
   **not** appear. It will be in the tree, just not in that subtree.
5. Commit the resulting `Cargo.lock` update. `Cargo.lock` is committed on
   purpose — see the note in `.gitignore`.

Only then write the window.

### Risk: Zed breaks GPUI constantly

Zed does not publish GPUI as a stable crate. The `rev` pin is a commit hash into
a repository that is under active development, and upstream commits routinely
rename types, change trait signatures, and move modules. A `cargo update`, a
Zed-side force-push, or a transitive dependency bump can break the build with
errors that say nothing about orca.

Mitigations, in order of value:

- **Commit `Cargo.lock`.** With the lockfile committed, a `cargo build` uses the
  exact resolved graph that was tested. A break is then a deliberate, visible
  event rather than something that arrives unasked.
- **Pin by `rev`, never by `tag` or branch.** A branch pin means a different
  dependency set on every build. Already done.
- **Keep GPUI out of `orca-core` and `orca-win`.** This is the mitigation that
  actually pays off: when the pin breaks, the domain logic and its tests still
  compile and still pass, and the fix is confined to the UI crate. Rules 1 and
  2 in the layering section exist for this reason.
- **Expect to bump the pin, and budget for it.** Treat "update the gpui rev" as
  routine maintenance, not an emergency. A bump is a single-line change in the
  root `Cargo.toml` plus `cargo update -p gpui`, followed by fixing whatever
  upstream renamed. Budget an hour, not ten minutes.
- **Do not add a `[patch]` or fork workaround until the pin has actually
  broken.** Reaching for a fork on the first failure trades a five-minute
  version bump for permanent maintenance of a vendored copy of Zed.

## The popup: what the probe established

`crates/orca-probe` exists because these answers could not be reasoned out from
the API. Each was established by running the probe; treat them as evidence, not
as opinions, and do not re-derive them.

Findings, with how each one was actually learned — the negative results cost the
most time and are the ones worth keeping:

| Finding | How it was found | Consequence |
|---|---|---|
| `cx.activate(true)` is a **no-op on Windows** | Popup never took focus; read `gpui_windows/src/platform.rs`, where the `activate` body is a no-op comment | Use `Window::activate_window()`. Do not reach for `cx.activate`. |
| A dispatch node that is not focus-tracked silently drops typing | Text was not accepted; the missing `.track_focus(&focus)` on the query-bar dispatch node was the entire bug | Every input element needs `.track_focus(&focus)`. This is not optional. |
| `handle_input` is called from `paint`, not `prepaint` | Verified in `gpui/src/window.rs` | Input handling must be driven from paint. |
| `QuitMode::Default` **kills the process when the last window closes** | Esc hid the window and the process exited; traced to `remove_window` → `LastWindowClosed` → quit | `QuitMode::Explicit` is mandatory. This failed twice before it was fixed. |
| Foreground activation works | Probe's window class is `Zed::Window` and it took real foreground | The Win32 activation path is sound. |
| Hotkey must live on a dedicated thread | `RegisterHotKey` needs a live message loop | Keep the hotkey thread resident; do not fold it into the UI thread. |
| `Window::hide` / `Window::show` **do not exist** in this GPUI rev | Read the public API surface | The persistent-window design is blocked on a mechanism that does not exist yet. |

### Open performance problem

Create-and-destroy per toggle measures 260–820 ms hotkey-to-first-paint, most
cycles near 700 ms. A launcher that takes most of a second to appear is not
usable as a muscle-memory tool, so this is the highest-priority open issue.

The cause is almost certainly that each toggle builds a whole `Window` — the
expensive part is window creation, not rendering. The obvious fix is a persistent
window that is hidden and shown, but as above, GPUI exposes no way to do that at
this pin. Options, in order of preference:

1. Find or add a hide/show path (check for an unused `AppWindowHandle` that
   yields the raw `HWND`, and call `ShowWindow` directly).
2. Reuse the same `Window` and only vary visibility at the platform layer.
3. Accept the cost and optimise window creation itself.

Memory across ten create/destroy cycles ran 73 → 95 MB and then settled around
88 MB, which reads as bounded caching rather than a leak. Not run long enough to
be certain; `tools/soak.ps1 -Cycles 200` would settle it.

### Text encoding: a known, unfixed hazard

`Window::handle_input` reports selection and caret positions in **UTF-16** code
units, while the model stores the query as a Rust `String`, whose offsets are
**UTF-8 bytes**. The probe's own `caret` field is a `usize` byte offset.

For ASCII the two coincide and everything works. For anything else they diverge,
and a caret index computed in UTF-16 units will land in the wrong place in a
UTF-8 string — producing a visibly broken cursor, wrong selections, and
backspace that deletes the wrong number of bytes.

Nothing has fixed this yet. IME, emoji, and accented characters are all untested.
The fix has to decide which space the caret lives in; doing the conversion at the
`handle_input` boundary is the least invasive option.

## Decisions

**Workspace `resolver = "2"`, edition 2021.** Resolver 2 is required to be
explicit in a virtual workspace manifest; edition 2021 is the conservative
choice and is not load-bearing either way.

**`Cargo.lock` is committed.** orca is an application, not a published library,
and the guidance for applications is to commit it. The GPUI git pin makes an
unpinned resolve a real reproducibility problem.

**`rust-toolchain.toml` pins the GNU host triple.** The build machines have no
MSVC toolchain. Pinning the triple in the toolchain file means a bare `cargo
build` picks the right host even before the `--target` flag is added.

**`source` weights live in `orca-core`, not in the UI.** A weighting policy is
domain logic: it is testable, it changes without a UI rebuild, and it is the
thing that gets tuned. The numbers currently in `Source::weight` are
placeholders.
