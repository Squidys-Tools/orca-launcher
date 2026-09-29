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
4. Re-run `cargo tree -p orca-core --edges normal --target x86_64-pc-windows-gnu`.
   GPUI must **not** appear. It will be in the tree, just not in that subtree.
   The `--edges normal` matters — see "How the rule is enforced" above.
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

