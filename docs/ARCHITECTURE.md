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
cargo tree -p orca-core --edges normal --target x86_64-pc-windows-gnu
```

The output must contain exactly one root, `orca-core`, and nothing that can
reach a window or a Win32 call. This is the check to put in CI when CI exists.

**`--edges normal` is load-bearing, and getting it wrong is a false alarm.**
`cargo tree` includes dev-dependencies by default, and the benchmark dependency
(`criterion`) transitively pulls in `winapi` and `windows-sys`. That is harmless
— a benchmark binary is not shipped, and nothing in it is on the ranking path —
but a plain `cargo tree -p orca-core` will show them and look like a violation.
`--edges normal` is what actually links into the library.

Note also that build-dependencies (`cc`, `vcpkg`, `pkg-config`, reached through
`libsqlite3-sys`) appear in the normal-edge tree. They compile the bundled
SQLite amalgamation and are not linked into the binary, so they do not count
against the rule either.

The list in `crates/orca-core/Cargo.toml` currently holds `serde`, `toml`,
`rusqlite` (with `bundled`, so there is no system SQLite to install), and
`criterion` as a dev-dependency. None of them can reach a window or a Win32 call,
which is the property being protected. Do not add to it without reading this
file.

## Where the "pure" rule actually bites

Layering rule 2 says *no clock, no filesystem, no environment, no global state*.
That is stricter than "no gpui, no Win32", and it is the rule that costs
something. Each forbidden thing is replaced by a seam rather than by an
exception:

| Forbidden | Replaced by | Where the caller lives |
|---|---|---|
| clock | `frecency::Timestamp`, passed in | `orca` |
| filesystem | `config::ConfigPaths`, `providers::DirectoryLister` | `orca` |
| environment | an injected `(name, value)` list | `orca` |
| persistence | the `store::UsageStore` trait | `orca`, via `store::sqlite` |
| globals | nothing; every function is free | — |

The one place `orca-core` does read the environment is
`config::ConfigPaths::default()`, which resolves `%APPDATA%`. It is behind
`Default` rather than behind any function the tests call, so a test that uses
`ConfigPaths::at` or `ConfigPaths::under_app_data` never touches it.

**`std::fs`-backed file search lives outside this crate, deliberately.**
`providers::FileSearchProvider` is a bounded directory walk — depth limit, cycle
guard, result cap, id derivation, mtime-to-prior — over an injected
`DirectoryLister`. `providers::InMemoryDirectory` is a complete implementation,
so the traversal is tested against fixtures containing a junction loop, a
permission denial, and a 200-level tree, none of which a real filesystem makes
cheap to produce. The `std::fs` adapter is a dozen lines and belongs to `orca`,
which is the crate that owns the composition root. If you add it to `orca-core`,
the no-filesystem rule becomes unenforceable and the cycle-guard test starts
depending on the machine it runs on.


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
4. **No operation lies about having happened.** There are no `NotImplemented`
   variants left to hide behind — the platform layer is implemented. What
   replaces that rule is stricter in the way that matters: a failed operation
   never leaves the object in a state that reads as success.
   `Win32GlobalHotkey::register` does not set its `registered` field before
   returning its error, so a caller that ignores the error still cannot conclude
   the hotkey was claimed. Every stateful operation follows this shape.
5. **`orca` is a composition root, not a library.** Logic that is not about
   wiring belongs in one of the other two crates.

## Adding GPUI later

GPUI is now linked. `orca` depends on `gpui` and `gpui_platform` through the
workspace pin, and the popup is built. The pin, in one place in the root
`Cargo.toml`:

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

`cargo add -p orca gpui` picks the workspace pin up on its own and writes
`gpui.workspace = true`, so the rev still lives in exactly one place.

**The check that must keep passing, and did after GPUI landed:**

```powershell
cargo tree -p orca-core --edges normal --target x86_64-pc-windows-gnu
```

28 lines, one root, and no `gpui`, `windows`, `windows-sys` or `winapi` in the
subtree. `--edges normal` is load-bearing here for the reason given above. Run
it after any dependency change to `orca`; it is the mechanical form of the rule
this file spends most of its length defending.

### A trap worth naming: `use super::*` in a binary's test module

A crate root glob-imports both `gpui::*` and `gpui::prelude::*` — together
well over a thousand names. A `#[cfg(test)] mod tests` that then does
`use super::*` inherits both, and expanding the built-in `#[test]` attribute
under that many glob candidates exhausts the macro-expansion recursion limit.

The error is `recursion limit reached while expanding #[test]`, attributed to
the *first* `#[test]` in the module, so it points at the tests rather than at
the imports. `crates/orca/src/main.rs` names its test imports explicitly. This
did not affect the library modules, only the binary root — which is why it is
worth knowing rather than rediscovering.

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
| `Window::hide` / `Window::show` **do not exist** in this GPUI rev | Read the public API surface | True, and not the blocker it looked like: `Window` implements `raw_window_handle::HasWindowHandle`, which yields the `HWND`, and `Window::refresh()` is public. See below. |

### The persistent window: solved, and the negative results that hid it

Create-and-destroy per toggle measured 260–820 ms hotkey-to-first-paint, most
cycles near 700 ms. `Window::hide` and `Window::show` do not exist at this
GPUI pin, which is what made the problem look blocked.

It was not blocked. It was **one missing public function**, and the negative
results were what made it look that way. The full account is in
`crates/orca/src/win.rs`; the short version is the sequence and the four facts
that make it work:

```text
hide:  ShowWindow(hwnd, SW_HIDE)
show:  ShowWindow(hwnd, SW_SHOWNOACTIVATE)
       Window::focus(&handle, cx)
       Window::activate_window()
       Window::refresh()          <- without this, stale pixels forever
```

| question | answer at rev `bcf6582` |
|---|---|
| Is there an `AppWindowHandle`? | No. The type does not exist. |
| Does `App::hide` help? | It hides the *application*, with no unhide. A trap for a single-window tray launcher, not a feature. |
| How do we get the `HWND`? | `Window` implements `raw_window_handle::HasWindowHandle`. Call it as `HasWindowHandle::window_handle(window)` — the trait must be named, because `Window` also has an inherent `window_handle()` returning a GPUI `AnyWindowHandle`, and method resolution picks the inherent one. |
| Does the window survive `SW_HIDE`? | Yes. Only `remove_window()` destroys it. |
| Does showing it again repaint? | **Not by itself.** `WM_SHOWWINDOW` reaches `handle_window_visibility_changed` → `Window::refresh_visibility`, and that only updates the visibility flag; it requests no frame. `Window::refresh()` is public and is required. |

`SW_SHOWNOACTIVATE` rather than `SW_SHOW` on the way up, because activation is
the next line and letting `ShowWindow` activate would race with the `SetFocus`
that follows it.

The `HWND` is captured once, when the window is created. If it is ever `None`,
`hide` falls back to destroy-and-recreate and says so in the log — the design
degrades to the old one rather than to a window that cannot be hidden.

`SetWindowPos` is needed too, and there is no `Window::set_position`: a
retained window has to follow the cursor to another monitor. Both calls are
Win32, and they are the layering exception described next.

### The one deliberate layering exception

Rule 3 says `orca-win` is the only place Win32 is called, and
`crates/orca/src/win.rs` breaks it. It is a bounded, itemised exception, and
each item names the gap it fills:

| call | the gap |
|---|---|
| `HasWindowHandle` → `HWND` | no public way to reach a GPUI window's handle |
| `ShowWindow` | `Window::hide` / `Window::show` do not exist |
| `SetWindowPos` | `Window::set_position` does not exist |
| `GetCursorPos` + `MonitorFromPoint` + `GetDpiForMonitor` | "centre on the cursor's monitor" needs a cursor read |
| `ShellExecuteW` | neither crate opens a `LaunchTarget` |

Five functions, in one file, the only `unsafe` in the crate, each with the gap
named above it. The right home for all five is `orca-win`, which was already
merged and not being edited. **When `orca-win` is next opened they move there
verbatim and the file disappears.** The interesting half — which display
contains a point — is a pure function and is tested.

Note the DPI detail, because getting it wrong is invisible at 100% and wrong on
every other display: Win32 gives screen coordinates in *physical* pixels while
`PlatformDisplay::bounds()` is in *logical* ones. Comparing them directly picks
the wrong monitor on any scaled display, so the cursor is divided by the DPI of
the monitor it is on.

### Where the keystroke time actually goes

Measured with `cargo bench -p orca-core --target x86_64-pc-windows-gnu` on the
development machine, **debug profile, criterion 40 samples**. These are not
shipped-build numbers and are not a claim about the running app — they are a
lower bound, and the point is the *shape*:

| operation | 100 items | 1 000 | 5 000 |
|---|---|---|---|
| `rank` with an empty query | 140 µs | 2.25 ms | 11.4 ms |
| `rank` with a typed query | 123 µs | 1.65 ms | 7.8 ms |
| `rank` with a long query | 206 µs | 2.36 ms | 7.8 ms |
| one `match_score` | ~0.6 µs | — | — |
| `frecency` score, warm | 152 ns | — | — |

Three things fall out of that, and all three are design decisions rather than
accidents:

* **A typed query is *faster* than an empty one at 5 000 items** (7.8 ms vs
  11.4 ms). The empty query keeps every candidate and sorts all 5 000; a typed
  one drops the non-matches first. Ranking is not the bottleneck on the
  keystroke path — collection is.
* **The sort dominates, not the matching.** 5 000 × ~0.6 µs of `match_score` is
  ~3 ms; the whole typed rank is 7.8 ms. A `min_score` that drops weak results
  before the sort is the obvious lever if this ever needs to get cheaper.
* **The fuzzy tier is the expensive tier, and it is bounded.** The
  `min_fuzzy_length` default of 2 exists partly for this: one character is an
  in-order subsequence of almost every string, so without it the fuzzy scan runs
  over the whole catalogue for every keystroke and matches nearly everything.

If ranking ever shows up in a latency profile, the first thing to check is
whether the catalogue is actually a few hundred items, not a few thousand — and
whether `ProviderSet::collect_all` is being called per keystroke rather than
per toggle.

### Text encoding: fixed at the boundary

`Window::handle_input` reports selection and caret positions in **UTF-16 code
units**, while the model stores the query as a Rust `String`, whose offsets are
**UTF-8 bytes**. For ASCII the two coincide and everything works; for anything
else they diverge immediately, because `é` is 1 UTF-16 unit and 2 UTF-8 bytes and
`😀` is 2 and 4.

This is fixed. `crates/orca/src/text.rs` owns the conversion, and the rule is
that **every range crossing into `EntityInputHandler` is UTF-16 and every range
staying inside the model is UTF-8 bytes** — no flag, because a flag would
eventually be set wrong. Both directions snap to a character boundary, and a
UTF-16 index landing inside a surrogate pair snaps *back* to the start of the
character rather than past it, so the caret is never after a character the user
cannot see half of.

This is a correctness bug and not a polish item, and the reason is worth
recording: a caret index carried across the boundary without conversion lands
mid-character, and `str::replace_range` **panics** on a non-boundary index. A
backspace over a multi-byte character would crash the launcher.

The tests walk ASCII, accented, CJK, emoji, and mixed strings in both
directions, checking that the invariant holds after *every* caret step rather
than only at the ends.

**The probe's version of this is wrong in two places, and `orca` is not a copy
of it.** `selected_text_range` returned the raw byte offset instead of
converting to UTF-16, and `character_index_for_point` called the UTF-16→byte
helper with a value that was already a byte offset — the conversion ran the
wrong way. Both are only wrong for non-ASCII, which is exactly why they
survived. Anyone copying text handling out of `crates/orca-probe` should copy
`orca/src/text.rs` instead.

### Backspace is a key binding, not text input

Not an encoding issue, but it lives in the same code and is equally
counter-intuitive. `gpui_windows`' `parse_char_message` filters out every
control character before the input handler sees it
(`char::from_u32(code_point).filter(|c| !c.is_control())`), and `WM_CHAR` for
<kbd>Backspace</kbd> is `0x08`. So the text input handler **never** receives a
backspace, a delete, or an Enter. All three are bound as actions instead.

The upside is that there is no double-delete risk between the input handler and
a `backspace` binding, which a platform that did deliver the control character
would have. `Launcher::backspace` still refuses to act while the IME holds a
non-empty marked range, because the IME replaces its own marked text through
`replace_text_in_range` and deleting as well would remove two characters for
one keypress.

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

**That is now done, and the policy is three normalised signals.**
`policy::RankingPolicy` combines them in one place, and both weights are stored
as a single fraction with their complements derived, so the halves cannot fail
to sum to one:

```text
final = match_weight * match
      + (1 - match_weight) * source_weight * (provider_share * provider
                                           + (1 - provider_share) * frecency)
```

`match_weight` is 0.70 and `provider_share` is 0.40. Three consequences are
asserted in the tests rather than left as intentions, because each is a
deliberate trade someone will otherwise "fix":

* **A cold exact match always outranks a browse candidate.** Typing can never be
  a mistake.
* **A weak match with a strong history *can* outrank a cold exact match.** That
  is what frecency ranking means, and removing it would make the history
  pointless. `RankingPolicy::match_weight` is the knob.
* **Source weighting can never reorder two different match tiers** under the
  default table, because the whole weight band is worth
  `DEFAULT_MAX_SOURCE_SWING` and the narrowest tier gap is worth more. A
  hand-written `[sources]` value of 0.0 or 1.0 *can* — that is
  `MAX_ABSOLUTE_SOURCE_SWING`, exactly the prior budget, and a config author who
  writes that is making a statement the code should carry out.

**The fuzzy band is capped below the browse state on purpose.** `FUZZY_FLOOR +
FUZZY_CEILING == 0.48 < BROWSE_SCORE == 0.50`, and both halves are asserted in
a `const` block so a retune that breaks the invariant stops the crate compiling
rather than quietly promoting typo matches above the recents list.

**`SOURCE` weights are the one thing here that still needs real data.** The
ordering (Application > Command > Calculator > Folder > File ≈ WebSearch >
Clipboard > Unknown) is a product judgement, not a measurement. Tuning them
against actual ranking data is the open work.

## The dependencies orca-core now carries

`serde` + `toml` for the config file, `rusqlite` with `bundled` for the frecency
store, and `criterion` as a dev-dependency for the ranking benchmarks. The
`bundled` feature is a deliberate trade: the SQLite amalgamation compiles into
the binary, so a user's launcher works on a machine where nothing has been
installed, at the cost of a slower first build.

