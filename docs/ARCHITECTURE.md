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
| `DwmSetWindowAttribute` | suppressing the Win11 default corner rounding, which would clip the panel's drop shadow |

Six functions, in one file, the only `unsafe` in the crate, each with the gap
named above it. The right home for all six is `orca-win`, which was already
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

## GPUI's foreground executor stops when no window is visible

This is the single most expensive thing learned in this project, and the only
one that produced a bug with **no error message at all**.

`Context::spawn` runs its future on the *foreground* executor. On Windows that
executor is driven by window activity, and when no window is visible it stops
scheduling. A future parked on it is therefore not merely slow to wake — it is
not woken.

A launcher is hidden nearly all the time, so "a task that only runs while the
popup is open" is a contradiction. Orca had its command pump there: hotkey
presses were funnelled into an `async_channel` and drained by a `cx.spawn` loop.
The loop waited on `recv()`.

The reported symptom was that the launcher could not be reopened after pressing
Esc. The log ended at `popup: hidden (window retained)` and nothing more was
ever printed. That silence is the diagnostic signature:

* The hotkey was still registered and still firing.
* The command was still being queued, correctly.
* Nothing was draining it, because the executor holding the drain loop was
  asleep and the popup was the thing that would have woken it.

Two toggles appeared to work, which is what made it confusing: the first show
put a visible window up, which kept the executor awake long enough to handle the
next press. The failure only appeared once a hide was followed by a real pause.

The fix is a two-task shape. A dedicated thread parks on `recv_blocking` and
forwards to the foreground task over a second channel. The receive loop is
therefore never subject to window visibility, and each command still reaches the
app on the correct thread.

Note this could not be solved with `background_executor().spawn`: that requires
a `Send` future, and any future holding a GPUI type is `!Send`, because
`AsyncApp` is. A thread has no such bound, which is why the pump is a thread.

**The general rule:** anything that must run while the app is *idle* — tray
polling, single-instance listeners, hotkey handling, telemetry flushes — cannot
live on the foreground executor. It is not a slow path, it is a dead one.

## The window is transparent, and larger than the panel

The popup is a rounded panel floating over the desktop rather than a window with
a title bar, and three separate decisions have to hold for that to come out
right. All three are in `crates/orca/src/ui.rs` and `main.rs`; recording them
because each one is a plausible-looking thing to "simplify" back.

**The window is bigger than the panel, on all four sides, by
`ui::FRAME_MARGIN`.** Not decoration. The widest shadow in
`Theme::panel_shadows` is a 56px blur offset 24px down, and a window sized to
the panel clips its own shadow into a hard straight-edged rectangle. The
consequence is that every placement calculation — `Bounds::centered`,
`place_on_cursor`, the `rect=` in the show log — is in *window* coordinates, not
panel coordinates, and the panel is centred inside it.

**`WindowOptions::window_background` is `Transparent`, and the root view is not
painted.** `gpui_windows` clears the render target to `[0, 0, 0, 0]` for every
non-`Opaque` appearance and presents through a `DXGI_ALPHA_MODE_PREMULTIPLIED`
Direct Composition swap chain, so scene pixels that were not painted are
genuinely transparent. One side effect worth knowing: `Window::should_use_subpixel_rendering`
returns `false` for any non-opaque appearance, so the query text is grayscale-antialiased
inside the panel and on an opaque background.

### Rounded corners: painted, not delegated to DWM

`DWMWA_WINDOW_CORNER_PREFERENCE` looks like the answer and is not:

* it is Windows 11 build 22000 and later, with a small set of radii (8px, 4px,
  none) and no 16–20px option, which is what this design wants;
* it rounds the **window**, and the window is deliberately larger than the panel,
  so DWM would round the outside edge of the drop shadow — the one edge that has
  to stay square;
* it only reaches windows DWM redirects, and `gpui_windows` sets
  `WS_EX_NOREDIRECTIONBITMAP` whenever Direct Composition is enabled (the
  default; it is off only when `DISABLE_DIRECT_COMPOSITION` is set), in which
  case DComp composites the window and DWM leaves it alone.

Painting the radius has none of those constraints and is antialiased. The
remaining Win32 call, `win::round_corners`, therefore *suppresses* the DWM
preference (`DWMWCP_DONOTROUND`) rather than requesting one, so a Windows 11
build cannot quietly reintroduce the clipped-shadow failure. It logs whether
Windows accepted it, because that number is the only thing in the log that says
which path the corners are taking.

### The frosted backdrop is gone for now, and why

**Removed 2026-10-03.** The panel was translucent and painted a blurred copy of
the desktop behind itself. It is now an opaque card. The feature was cut while
the launcher still has almost no features in it, on the grounds that the capture
was the single most expensive thing on the keystroke path and the cheapest thing
to get wrong. It is deferred, not abandoned: everything below is the reason it
was not free, and it is the list to read before putting it back.

The three modules are deleted — `orca_win::backdrop`, `orca_core::backdrop`,
`orca::backdrop` — along with `image` as a direct dependency and the
`Win32_Graphics_Gdi` feature on `orca-win`. `win::cursor_physical_and_scale`
went with them; it existed only so the capture and the panel placement could not
disagree about the cursor's monitor.

**Why we could not use a system backdrop material.** `gpui` exposes
`WindowBackgroundAppearance::{Transparent, Blurred, MicaBackdrop,
MicaAltBackdrop}` and `gpui_windows` implements all four through DWM. None of
them fits, for a specific reason rather than a cautious one: they apply their
material to the **whole window rectangle**. The window is larger than the panel,
so a backdrop material would fill the shadow margin too and turn the rounded
panel into a rounded *square* — worse than no material at all.

**And the second obstacle, found by reading the backend rather than the docs.**
`MicaBackdrop` goes through `DwmSetWindowAttribute`, and `gpui_windows` sets
`WS_EX_NOREDIRECTIONBITMAP` whenever Direct Composition is enabled — the
default, off only when `DISABLE_DIRECT_COMPOSITION` is set. DWM backdrops are
applied to windows DWM redirects, and a DirectComposition window is not
redirected, so the attribute may do nothing visible at all. The documentation's
"not always supported" is doing a lot of quiet work there.

**What the capture cost.** GDI `BitBlt` off the screen DC, not
`Windows.Graphics.Capture`: the modern API shows a consent dialog and returns a
D3D frame pool to copy out of, which for blurring a few hundred thousand pixels
per hotkey press is the wrong trade. Even so it measured 20–50ms, and it had to
run while the window was hidden — `BitBlt` photographs the screen, so capturing
with the popup up captures the popup. That precondition is what made the design
awkward rather than cheap: the work had to be moved to the hide path and the
startup path so the show path could stay instant, which in turn made the blur
*stale* — showing what was behind the cursor at close, not at open. A launcher
that opens 40ms late is worse than one whose frosted panel is a frame old.

**The blur was a downsample, not a Gaussian.** `soften` box-averaged by 4,
blurred the small buffer, and let the renderer's bilinear filter stretch it back.
The bilinear upscale is just the last box in a separable chain, so it reads like
a wide Gaussian and costs about 16× less, because the expensive step runs on
1/16th of the pixels. `DOWNSAMPLE = 4` was a balance with both failure
directions visible: too large and the upscale shows as faint diagonal banding,
too small and it stops being cheap and reappears as keystroke latency.

**Two bugs in that code, named because they are the ones that would come back
with it:**

* **Alpha was normalised by the window width instead of the number of samples
  actually taken.** At an edge the window runs off the image, so every border
  lost opacity and the loss compounded through each pass. The visible symptom is
  a flat image coming out with translucent edges — a dark band down the right and
  bottom of the panel, which reads as "the shadow is wrong".
* **A GDI handle leak waiting to happen.** A memory DC with a bitmap still
  selected into it cannot be deleted without leaking the bitmap, and a screen DC
  must be released with `ReleaseDC` while a memory DC is destroyed with
  `DeleteDC`. There is no reliable query for which kind you have, so the guard
  recorded it at creation. Getting this backwards leaks a handle on every hotkey
  press.

**What to do when it comes back.** Keep the capture off the show path — the
blur is the cheap part of this feature and the latency is not. Do not
reintroduce `Win32_Graphics_Gdi` on `orca-win` for it: `orca` calls
`MonitorFromPoint` and now declares that feature itself, because a feature
another crate in the graph happens to enable is not a dependency.

### The window frame: `Transparent` asks Windows for a border

`WindowBackgroundAppearance::Transparent` — the value this whole design rests on
— puts a **one-pixel border around the entire window**, and the popup looked
like a dialog with a rounded panel inside it until this was found.

It is not a GPUI rendering decision, and it is not a consequence of the swap
chain being transparent. `gpui_windows` maps each appearance onto
`SetWindowCompositionAttribute` with a `WCA_ACCENT_POLICY`, and for
`Transparent` it sends:

| field | value | meaning |
|---|---|---|
| `accent_state` | 2 | `ACCENT_ENABLE_TRANSPARENTGRADIENT` |
| `accent_flags` | 2 | **`WCA_ACCENT_FLAG_DRAW_ALLBORDERS`** |

`Opaque` sends `accent_state = 0` instead, which is why the border is *new*
rather than pre-existing — choosing transparency is precisely what asks Windows
for it. Read it in `gpui_windows/src/window.rs`, `set_background_appearance`
and `set_window_composition_attribute`.

The two mechanisms are independent, which is what makes it fixable in six lines:
the per-pixel alpha comes from the Direct Composition swap chain being cleared
to `[0, 0, 0, 0]`, **not** from the accent policy. So `win::clear_window_frame`
sends `accent_state = 0` (`WCA_ACCENT_ENABLE_NONE`) after GPUI has set the
appearance. The frame goes, the shadow margin stays transparent.

`SetWindowCompositionAttribute` is resolved from `user32.dll` at runtime and
cached, because it is not exported by name in any import library. The
`AccentPolicy` and `WINDOWCOMPOSITIONATTRIBDATA` structs are declared locally —
the `windows` crate at 0.62 does not export them, and `gpui_windows` declares
the identical two. `#[repr(C)]` and the field order are load-bearing, because
both cross into user32 by pointer.

**The general lesson:** "transparent window" is two separate Windows features
that happen to be requested together, and turning on the wrong one is invisible
until a human looks at a screenshot. Anything that reads as a window
*decoration* — border, shadow, corner — is DWM's business, not the renderer's,
and has to be checked separately from anything that reads as pixel content.

### The panel is opaque, and the fills on top of it are not

The panel fill itself is opaque — it is a card sitting over the desktop, and
anything with an alpha there would put the desktop's own text behind the
launcher's text. So a contrast check against the panel's RGB needs no help.

Everything painted *inside* the panel is a different matter. The selection fill,
the footer pills and the border are alpha-blended over the panel rather than
replacing it: a selection is a lightening of the card it sits on, not a colour of
its own. So `theme.rs`'s tests still carry their own source-over, and assert every
pairing against the fill composited onto the panel.

The subtlety that cost a test, and that is worth keeping for when the frosted
backdrop comes back: a selection highlight does not sit on the desktop, it sits on
the panel. Compositing a 10%-white highlight directly against a white wallpaper
scores it at 1.00:1 and produces a failing test that describes nothing the user
would ever see. It is the strongest argument in this file for testing against the
real compositing chain rather than against the constants that feed it.

While the panel *was* translucent, that same test also bounded its alpha from both
sides: too opaque and the translucency was gone, too transparent and `dim` stopped
clearing 4.5:1 over a light wallpaper. With the backdrop removed, the panel's
alpha is simply 1.0 and `the_panel_is_opaque` asserts it — which is not redundant,
because an alpha of 0.9 passes every other test in the file.

## `Window::activate_window` is asynchronous, and that matters

`Window::activate_window()` looks like the obvious way to bring the popup to the
front. It is the wrong tool, and using it produced a bug that survived one round
of confident misdiagnosis.

Internally, `gpui_windows`'s `activate()` **spawns** its work onto the window's
executor and returns immediately. So:

* Anything done after `activate_window()` is racing a task that has not started.
* The spawned task calls `SetActiveWindow` and `SetFocus`, but **never**
  `SetForegroundWindow`. Under the foreground lock — which applies because this
  process is not at UIAccess integrity — the window can end up visible and
  behind whatever is in front.
* It also calls `set_window_placement()` first, which is a no-op after the first
  call (`initial_placement.take()`), so that part is harmless.

Combined with `SW_SHOWNOACTIVATE` — chosen deliberately, so that `ShowWindow`
would not race the `SetFocus` — the popup was shown without being raised and
without a real foreground call. Symptom: the launcher was opening *behind*
whichever window you were working in, which from the outside is identical to it
not opening at all.

The fix is to stop relying on GPUI for this and do it synchronously on the
calling thread, in a fixed order:

1. `place_on_cursor` — reposition for a possibly-changed monitor layout.
2. `refresh()` — request a frame, or the re-shown window shows its last one.
3. `focus` — GPUI-level focus id only.
4. `ShowWindow(SW_SHOWNOACTIVATE)` — make it visible without activating.
5. `SetWindowPos(HWND_TOPMOST, … | SWP_NOACTIVATE)` — lift the z-order, leave
   the foreground window alone.
6. `orca_win::activate` — the real foreground activation, which already carries
   the `AttachThreadInput` workaround the foreground lock requires.

## One error message must name one failure

The tray icon took four attempts and two wrong fixes, and the reason is a single
design mistake: five different Win32 calls all reported
`TrayError::WindowCreationFailed { code }`, which rendered as
`"tray message window could not be created"`.

Three of the five never had anything to do with creating a window. The real
failure was `LoadImageW` on the stock app icon, reported as a window-creation
error. The message was believed, so two rounds of debugging chased the window
class and the `NIF_ICON` flag. Both were changed. Neither was the cause. The
`WindowCreationFailed` variant is now `SetupFailed { at, code }`, where `at` is
the call that failed (`"GetModuleHandleW"`, `"RegisterClassW"`, `"CreateWindowExW"`,
`"LoadImageW (IDI_APPLICATION)"`, `"LoadImageW (file)"`). The same log line then
identified the real cause on the next run.

**A wrong error message is worse than no error message**, because it is believed
and it sends the search somewhere specific. If one variant covers several
failures, it is not one variant; it is a missing distinction. This is the same
mistake as the popup logging "shown" after a successful `Entity::update`, in a
different place.

## The tray icon itself is still broken, for a third, separate reason

Confirmed by log: `tray LoadImageW (IDI_APPLICATION) failed (Win32 error 1813)`.
`ERROR_RESOURCE_TYPE_NOT_FOUND` from `LoadImageW` with a null module means the
stock icon could not be resolved in this process at all. Not fixed.

The obvious next step is to stop asking for a system icon and ship a real `.ico`
in the repo, loaded via `LR_LOADFROMFILE`, which is a path this code already
supports through `TrayIconSource::File`. That is untested at runtime, so it is
recorded as the next thing to try rather than claimed as a fix.

## Every way out needs more than one path

Quit was reachable from exactly one place: the tray icon's *Quit* menu entry. The
tray icon does not work, so the launcher had **no way to exit at all** other than
killing it in Task Manager. A resident background process that cannot be stopped
from its own UI is a bug in its own right, independent of the tray being broken.

<kbd>Ctrl</kbd>+<kbd>Esc</kbd> now quits, as a `ui::Quit` action registered on
the popup's dispatch chain alongside the other actions. Two details worth keeping:

* It is bound to Ctrl+Esc rather than a bare Escape, because Escape is already
  "hide" and a bare key that quits would collide with text input.
* The handler calls the same `dismiss` path as hide before quitting. Under
  `QuitMode::Explicit` the process does not exit when the last window closes, so
  quitting without dismissing would leave the popup on screen.

Wiring it needed three things that are easy to miss: declaring the action in
`actions!`, adding an `on_quit` handler, **and** registering it with
`.on_action(cx.listener(Self::on_quit))`. The macro only defines the struct — a
handler with no listener is dead code, and clippy correctly refuses to let that
ship.

## A null `hIcon` with `NIF_ICON` set is a blank icon, not an error

The tray icon never appeared, with no error reported. `Shell_NotifyIcon` was
called with `NIF_ICON` in the flags and `hIcon` null, and Windows accepts that:
it adds a notification-area entry it cannot draw. The visible result is an empty
slot in the tray, which reads as "broken" rather than "missing".

The cause was an over-cautious earlier decision. `load_icon` loaded the system
icon *purely to check it could be loaded*, then returned `None` and dropped the
handle, on the reasoning that the stock icon is a shared system resource that
must never be destroyed. That reasoning was right about `DestroyIcon` and wrong
about the rest: a shared handle can be used indefinitely, and the safe thing was
to use it and simply never free it.

Two rules now hold, and both are tested:

* `NIF_ICON` is set only when there is a real handle. An entry the shell cannot
  draw is worse than no entry, because the user sees something.
* Icon ownership travels with the handle in `OwnedIcon`, which carries a
  `destroyable` flag. `LoadImageW` on a `.ico` file is owned and released;
  `LoadImageW` on `IDI_APPLICATION` is shared and never released. Three separate
  `DestroyIcon` call sites previously each had to remember a rule the type did
  not carry, which is how the shared handle was about to be freed.

## Log what you measured, not what you intended

The reason this bug took two attempts is worth recording, because it is a
general trap rather than a detail of this codebase.

`popup: shown (retained window)` was printed unconditionally after a successful
`Entity::update`. That call only proves the entity accepted an update. It says
nothing whatsoever about whether a window reached the screen. So the log
affirmatively reported success on every press while the launcher was opening
behind another window, and "the log says it worked" was actively misleading.

Four states are indistinguishable from outside the app: hidden, shown-but-
behind, shown-off-screen, and destroyed. Each has been mistaken for another here.

The show path now prints the measured state — `IsWindowVisible`, whether the
window is the foreground window, and its actual `GetWindowRect` — on every show.
A failure now names itself:

```
popup: show -> visible=true foreground=false rect=(478,232 640x400)  [NOT in foreground: it may be behind another window]
```

If a report says "it will not open" and the log says `visible=true
foreground=false`, the problem is z-order or activation, not lifecycle. If it
says `visible=false`, the window was never shown. If the rect is empty or
degenerate, it was moved somewhere it cannot be seen.

## A Win32 error code names the *category*, not the call site

`ERROR_RESOURCE_TYPE_NOT_FOUND` (1813) from the tray icon sent two rounds of
investigation to `CreateWindowExW`, and the window had been working the whole
time.

The chain was three mistakes in one function, not one bug in one call:

1. `load_icon` verified the stock icon with
   `LoadImageW(None, IDI_APPLICATION, IMAGE_ICON, 0, 0, LR_DEFAULTSIZE)`. A size
   of 0 plus `LR_DEFAULTSIZE` is not a reliable way to fetch a *system* icon;
   `LoadIconW` is. The call fails, and it fails for environmental reasons — not
   because anything is missing.
2. That verification was **fatal**. But `hIcon: NULL` in `NOTIFYICONDATAW` makes
   the shell draw its own default, which is exactly what
   `TrayIconSource::Application` is asking for. A cosmetic difference in the icon
   was taking down the entire tray.
3. The error it returned was `WindowCreationFailed`. 1813 reads as "class not
   found", so the message pointed at class registration and window creation, and
   nobody read the function that actually failed.

### Both fixes at once, which is why neither error message was right

A second line of work concluded from the other end: `load_icon` used to load the
icon *purely as a check* and then return `None`, throwing the handle away, so
`NIM_ADD` went out with `NIF_ICON` claimed and a null `hIcon`. The notification
area got an entry the shell could not draw — a blank slot, with no error anywhere.

Both diagnoses were right about their own half and neither was right overall:

| | believed | actually |
|---|---|---|
| one line of work | the tray was fine, the *probe* was fatal | the tray was absent, and the probe was why |
| the other | the icon had to be loaded for real | loading it must never be allowed to fail the install |

So the resolved `load_icon` uses the call that works (`LoadIconW`) **and** refuses
to be fatal: a failure returns `Ok(None)`, the caller declines `NIF_ICON`, and the
shell draws its default. The accepted cost is that this one failure is now
silent, which is the right way round — the alternative is a resident launcher
whose only exit is a tray menu that did not appear.

Two rules came out of it:

- **A diagnostic that names the wrong thing is worse than none.** One enum
  variant now covers exactly one failure: `SetupFailed { at, code }` names the
  call, and `IconLoadFailed { path, code }` is separate because a path is the one
  thing the person reading the log can act on. A name has to be falsifiable by
  reading the failing function. `SetLastError(0)` runs before `RegisterClassW`,
  because naming the failing call is only worth something if the code belongs to
  it.
- **An error code is a category, not a location.** 1813 is
  "resource type not found", which is *consistent* with a missing class, a
  missing icon resource, or a missing icon file. Reading it as proof of which
  call failed is what wasted the time.

The thing that actually found it was instrumenting the real function and
printing the class name it was about to register. A replica test built outside
the function could only have tested a guess: every variable — the class name
format, `WS_POPUP` on and off, `HWND_MESSAGE` on and off, non-null `lpParam`, and
running on a spawned thread — passed in isolation. Replicas prove a hypothesis
about the variables you already suspect. When a bug survives that, suspect the
function you have not read.

Related: [`orca-win/src/tray.rs`](crates/orca-win/src/tray.rs) has
`a_real_tray_icon_installs_and_uninstalls`, which exercises class, window, icon,
and notify against the live shell. A mocked `Shell_NotifyIconW` cannot catch any
of this, because the mock only ever runs the step *after* the one that broke.

## Window class names may not contain `(` or `)`

`CreateWindowExW` failed with **ERROR_RESOURCE_TYPE_NOT_FOUND (1813)** — "the
window class does not exist" — for a class that `RegisterClassW` had accepted
microseconds earlier.

The cause was the class name. It was built with `{:?}` on a `ThreadId`, which
formats as `ThreadId(3)`. MSDN restricts window class names to characters that
are valid in a file name, and parentheses are not among them. The failure is
deeply misleading because it is not reported where it happens: `RegisterClassW`
*succeeds*, and only `CreateWindowExW` fails, so the error names a class that
provably exists.

The thread id is now taken from `GetCurrentThreadId` rather than from
`std::thread::current().id()`, whose only *stable* formatting route is `Debug`
(`as_u64` is still unstable). A test asserts the generated name contains no
characters from the illegal set, so this cannot regress quietly.

**The general lesson:** when Win32 says a resource is missing, suspect the name
you built rather than the lifetime you gave it. Error 1813 in particular says
"not found", which invites a hunt through handle-lifetime bugs; here the handle
was fine and the *string* was malformed.

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

