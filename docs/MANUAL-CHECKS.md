# Manual checks

Everything on this page is something the gate **cannot** tell you. `tools/gate.ps1`
proves the tree builds, the tests pass, the formatting is clean, and clippy is
silent under the GNU toolchain — that is all it proves. It launches no window,
presses no key, and takes no screenshot, on purpose: keystroke injection into a
popup is a poor substitute for a person looking at the screen, and it burns
minutes per iteration while proving little.

So this file is the entire hand-off surface. Assume you are skeptical. Every
check below says what to do, what *good* looks like, and what the code already
guarantees, so you can spend your attention on the parts that are not yet known.

If something here fails, that is the finding. Do not work around it in the app;
the log lines quoted below exist precisely so a failure has an unambiguous cause.

## How to run

```powershell
./tools/run.ps1
```

That is the whole thing. It sets the GNU toolchain, stops a launcher left running
from last time, builds only if a source file is newer than the binary, runs it,
and writes `run.log`.

Two shorter paths, for when you already know what you need:

```powershell
# already built — just run it, no cargo involved
.\target\x86_64-pc-windows-gnu\debug\orca.exe
```

```powershell
# if you must do it by hand, this is the one that works
$env:PATH = "C:\Users\chris\.rustup\toolchains\stable-x86_64-pc-windows-gnu\bin;C:\Users\chris\.cargo\bin;" + $env:PATH
cargo run -p orca --target x86_64-pc-windows-gnu 2>&1 | Tee-Object run.log
```

A bare `cargo run -p orca` is the trap: it picks MSVC off `PATH`, which builds
the wrong target.

**Nothing appears on screen, and that is correct.** The launcher is resident with
no window until summoned — press <kbd>Ctrl+Shift+Space</kbd>.

**Trust the hotkey in the log, not this page.** The default is
<kbd>Ctrl+Shift+Space</kbd>, but `config.toml` can change it and the `config:`
line is the one true on your machine. These instructions were wrong for a while —
they named <kbd>Ctrl+Alt+Space</kbd>, which the code has never shipped — and a
person following them would have concluded the launcher was broken.

**If the window does not open and you just changed something, check for a
resident launcher.** It is a single-instance app: a second copy tells the first
to show and exits with code 2, so the binary you just built never starts and the
window belongs to the *old* one. `run.ps1` stops it for you; that is the most
confusing way this app can appear not to have changed.

`run.log` matters. Most of the checks below are answered by reading it rather
than by looking at the screen.

Startup must log, in roughly this order:

```
[orca] config: hotkey Ctrl+Shift+Space, theme …, 50 result rows
[orca] providers: installed-apps, commands, env
[orca] hotkey: Ctrl+Shift+Space registered
[orca] ready: resident, waiting for the hotkey
```

Any line ending in `unavailable:` is a degraded capability, not a failure. The
app is meant to start and stay usable with the hotkey, the tray, the catalogue,
or the history database all missing. If one of those is the *only* way to reach
the popup, that is a bug.

## 1. The retained window — the foundation the rest of this sits on

### 1a. Reopening after Esc — please read the log, not just the screen

**This has now failed twice, in two different ways, and both were invisible from
the screen alone. The log line is the real diagnostic.**

Run it, then trigger the launcher 4–5 times with <kbd>Esc</kbd> between presses,
pausing a few seconds each time. Then look at the `popup: show ->` lines.

Every show now prints the window's measured state:

```
popup: show -> visible=true foreground=true rect=(478,232 640x400)
```

Read it like this:

| What the log says | What it means |
|---|---|
| `visible=true foreground=true` | Correct. The window is on screen and in front. |
| `visible=true foreground=false` | **This was bug #2.** The window is on screen but *behind* another window. Press <kbd>Alt+Tab</kbd> or check the taskbar — it is probably sitting there. |
| `visible=false` | The window was never shown. This is a lifecycle problem, not a z-order one. |
| `rect=(0,0 0x0)` or wildly off-screen | It was moved somewhere it cannot be seen. Note the coordinates. |
| No `popup: show ->` line at all | The command was not drained — the earlier pump bug, now fixed. |

The two bugs already fixed, for context:

1. The command pump was parked on GPUI's *foreground* executor, which Windows
   stops scheduling when no window is visible. The hotkey fired, the command
   queued, nothing read it. Now on a dedicated thread.
2. `Window::activate_window()` spawns its work and returns immediately, so the
   activation raced everything after it, and it never calls
   `SetForegroundWindow`. The popup opened *behind* the foreground window. Now
   activation is synchronous and explicit, in a fixed order.

**What to report back:** paste the `popup: show ->` lines. They are designed to
make a third round of guessing unnecessary.

### 1b. Esc hides without killing the launcher

There is a second, older bug in the same area, fixed earlier and worth keeping an
eye on. <kbd>Esc</kbd> and <kbd>Enter</kbd> used to call
`Window::remove_window()`, which destroys the window. The process survived (that
is `QuitMode::Explicit`) but `main` still held a `WindowHandle` and an `HWND` for
a window that no longer existed, so every later toggle was a silent no-op and the
launcher was dead until you restarted it. Both paths now go through one
`Launcher::hide`, which returns a decision instead of destroying anything.

1. Start the launcher. Wait for `ready: resident…`. No window is on screen; this
   is correct.
2. Press <kbd>Ctrl+Shift+Space</kbd>. The popup appears and takes focus.
3. Press <kbd>Esc</kbd>.
4. **Expected:** the popup disappears, the process is still running, and the log
   gained exactly one line: `popup: hidden (window retained)`.

   If instead you see `popup: hidden (window destroyed; no HWND was available)`,
   the retained path is not in use and the latency win is gone. That is a
   fallback, not a crash, but it is not what this design claims.

   A third possibility is `popup: the retained window was gone; a new one is
   built on the next toggle`. That is a real bug, not a benign message: it means
   something destroyed the window behind the launcher's back, and the next
   toggle has to rebuild it. Note which keypress produced it.

5. Press <kbd>Ctrl+Shift+Space</kbd> again, three more times, alternating with
   <kbd>Esc</kbd>.
   **Expected:** every cycle logs `popup: shown (retained window)`. The word
   `created` must appear **exactly once** in the whole log, on the first toggle.
   If `created` appears twice, the window is being recreated and the design has
   silently reverted.

6. **The stale-frame check.** `WM_SHOWWINDOW` reaches GPUI's
   `handle_window_visibility_changed`, which updates the visibility flag and
   requests *no frame*. Without an explicit `Window::refresh()` a re-shown popup
   sits on screen showing its last frame. So: type `note`, let the list fill,
   press <kbd>Esc</kbd>, press <kbd>Ctrl+Shift+Space</kbd>.
   **Expected:** the query bar is empty and the list is back to the empty-query
   state, painted correctly. If you see the *previous* query and stale results,
   `refresh()` has stopped being called and that is the bug.

### 1b. Enter launches and then still leaves a usable popup

This is the same defect on the more likely path, and it is the one to check
first.

1. Open the popup, type a query until a result you recognise is highlighted.
2. Press <kbd>Enter</kbd>.
   **Expected:** the program opens, the popup disappears, and the log says
   `popup: hidden (window retained)`.
3. Press <kbd>Ctrl+Shift+Space</kbd> again.
   **Expected:** the popup reappears, empty and focused. If it does not, and
   nothing at all happens, you have the pre-fix bug: activation destroyed the
   retained window.

### 1c. The window is genuinely not being recreated

The log check in 1a is the cheap version. The direct version: while the popup
has been toggled 20 times, count the process's windows.

```powershell
Get-Process orca | Select-Object Id, MainWindowHandle, MainWindowTitle
```

Better, if you have it: Sysinternals' Process Explorer, `orca` →
*Window* submenu, and watch the entry count. **Expected:** exactly one popup
window for the whole session, created once. A rising count means each toggle is
leaking a window, and the count is the only thing that will show it before the
process runs out of handles.

### 1d. Does hiding give focus back to the previous window?

`SW_HIDE` destroys activation. Windows usually hands foreground to whatever was
next, but "usually" is doing real work in that sentence.

1. Click into Notepad and type something so you can see where focus is.
2. <kbd>Ctrl+Shift+Space</kbd> to open the popup, then <kbd>Esc</kbd>.
   **Expected:** focus returns to Notepad with the caret where you left it, and
   your next keystroke lands there. If focus lands somewhere arbitrary or on the
   desktop, that is a real defect in the hide path, not a nitpick — it is the
   difference between a launcher you can leave open and one you cannot.

### 1e. The first keystroke after activation

`cx.activate(true)` is a literal no-op on Windows (in this GPUI rev the
`activate` body is a comment). `Window::activate_window()` is used instead: it
does `SetActiveWindow`/`SetFocus` plus a `SendInput` Alt tap to defeat the
foreground lock. That Alt tap is a known cost and a known source of weirdness.

1. Open the popup and type immediately, without clicking it.
   **Expected:** the first character appears.
2. Repeat five times in a row, quickly.
   **Expected:** no dropped first character, ever. The historical failure mode
   was typed characters vanishing with no error anywhere. If you see one
   character lost, the activation order in `main::show` is wrong.

## 2. The frame: blur, rounded corners, and a floating panel

**This is the highest-risk change in the project right now, because the gate
cannot see any of it.** Everything below is one boolean, set once, at window
creation, and it decides whether the launcher looks like the reference or looks
like a black rectangle. Nothing in the test suite can observe it.

The mechanism, in one sentence: the window is **transparent** and **larger than
the panel**, and the panel is a rounded rect painted inside it. If the
transparency does not composite, the margin around the panel is black and the
launcher is worse-looking than before this change.

### 2a. Is the margin around the panel actually see-through?

1. Start the launcher, press <kbd>Ctrl+Shift+Space</kbd>.
2. **Expected:** a rounded dark panel with a soft shadow under it, floating over
   whatever is behind it. Around the panel — roughly 32px on every side — you
   see the **desktop**, not black.

The failure to look for: a **black square** the size of the whole window, with
the panel drawn inside it. That means the swap chain's alpha is being ignored
and the clear colour reached the screen. Say so in the report; the fix is one
enum value in `main.rs` (`WindowOptions::window_background`) and it is a design
decision, not a bug hunt.

To get a proper read, put something with hard edges and high contrast behind the
launcher first — a browser window with white and black areas, or the desktop
wallpaper. A flat-coloured background hides a compositing failure completely.

### 2b. Rounded corners

- [ ] The panel's four corners are rounded, with a radius of roughly 16px. The
      radius must be **visibly larger** than the radius on the selected row
      inside it; if they look the same, the panel is not rounding.
- [ ] **There must be no frame around the panel.** Not a border, not an outline,
      not a rectangle of any kind outside the panel's own drop shadow. What you
      should see is the panel, its shadow, and then straight through to the
      desktop. Read the log:

      ```
      popup: panel rounds itself; DWM corner rounding suppressed=true, window frame cleared=true
      ```

      | What the log says | What it means |
      |---|---|
      | `window frame cleared=true` | The frame removal was accepted by Windows. |
      | `window frame cleared=false` | `SetWindowCompositionAttribute` was not found or refused. The frame will still be there. |
      | `DWM corner rounding suppressed=false` | The attribute was rejected — expected below Windows 11, a defect on it. |

      This is the highest-value line in the log for judging whether the popup
      looks like a launcher or like a dialog, and it is a `false` on the first
      line to look at when a 1px rectangle is visible around the panel.
- [ ] Repeat at 125% and 150% scaling. A radius is in logical pixels, so it
      should look the same physical size; if it looks chunky at 150%, the radius
      is being applied in physical pixels somewhere.

### 2c. The shadow

- [ ] The panel casts a soft shadow, and the shadow is **not cut off** at the
      window edge. This is what `ui::FRAME_MARGIN` is for; a hard straight edge
      in the shadow means the margin is too small for the blur.
- [ ] The shadow looks like two layers — a tight contact shadow and a wide soft
      one — rather than one uniform grey halo. If it is a single flat ring, only
      one of `Theme::panel_shadows`' two entries is being applied.

### 2d. The frosted backdrop

The panel is translucent *and* blurred: on every show the launcher captures the
screen region behind the panel, blurs it, and paints it inside the panel under
the translucent fill. So what shows through should be the desktop,
recognisably shaped, with no readable text.

- [ ] Open the launcher over a window with a lot of text. **Expected:** you can
      tell roughly *what* is behind it — light and dark areas, an image — but
      not one readable word. Legible text behind the panel means the blur is not
      running.
- [ ] Every show prints exactly one line about it:
      ```
      backdrop: captured and blurred in 3.2 ms at 1x scaling
      ```

      | What the log says | What it means |
      |---|---|
      | `captured and blurred in N ms` | Working. `N` is the real cost on the keystroke path. |
      | `unavailable, falling back to a flat fill` | The capture or blur was refused. The launcher still opens — deliberately not fatal — but the panel is a flat tint. The Win32 error is swallowed, so just report the line. |
      | No `backdrop:` line at all | The show path is not calling it. |
      | `at 2x scaling`, and the blur is offset toward the panel's top-left | The physical/logical conversion scaled the size but not the position. |

- [ ] **Check the cost.** The whole reason this blurs a downsampled copy is that
      it is cheap. If `N` is much above 20 ms it is competing with the reason
      the launcher exists, and the fix is to lower
      `orca_core::backdrop::DOWNSAMPLE`, not to accept it.
- [ ] The blur must **follow the panel**. Move the mouse to another monitor and
      press the hotkey: the backdrop must be sampled from *that* monitor. Show
      the previous monitor's wallpaper and the placement and the capture are
      disagreeing about where the cursor is.
- [ ] At 125% and 150% scaling the blur must still line up with the panel.
- [ ] The panel's corners must still be **round**. The backdrop is clipped to
      the panel; four hard square corners of desktop inside a rounded panel
      means `overflow_hidden` was removed from the panel.

### 2e. It really does capture the screen — know that

The backdrop comes from GDI `BitBlt` off the screen DC, so it photographs
whatever is in that rectangle at that moment. There is no consent dialog and
nothing is stored: the buffer is blurred, painted once, and replaced on the
next show. But it does mean the launcher can photograph part of the screen — a
password manager, a video call — every time the hotkey is pressed. If that is
unacceptable on a particular machine, the capture is one function
(`orca_win::capture_screen_region`) and removing it returns the launcher to a
flat translucent panel.

### 2f. The rest of the frame

- [ ] The search field has a magnifier at the left and the placeholder reads
      *Search for apps and commands…*. The magnifier is an inline SVG that
      `gpui` rasterises into an **alpha mask** and tints with `Theme::dim`, so it
      should be a flat dim grey. If it is a black blob or a missing-glyph box,
      the SVG failed to parse — check the string constant `ui::MAGNIFIER`.
- [ ] **Typing still works.** The `.track_focus(&focus)` call moved with the
      restyle onto the search field's row, which is the nearest ancestor of the
      input element. If it is ever left off that row, characters are dropped
      with no error anywhere. This regression is silent, so type after every
      layout change to that row.
- [ ] Each row shows the source name on the **right** (`Application`, `File`,
      …) and the title on the left. The old left-hand gutter tag (`app`, `file`)
      is gone; that space went to the title.
- [ ] The footer has two pills on the right, *Open · Enter* and *Hide · Esc*.
      Both name bindings that exist. When nothing is selected the *Open* pill is
      dimmed to 45% but does **not** change width — a reflowing footer twitches
      on every arrow key.
- [ ] The status line and the `paint N ms` readout are still present on the
      **left** of the footer. Section 4 below depends on them, so if you removed
      them, put them back.

### 2g. The frame on a small display

The window is 724×494 logical pixels, chosen so it fits a 1366×768 display at
150% scaling (910×512 logical) — the tightest case still in use. A test asserts
this, but the test cannot see what Windows does when a window is too big: it is
**clamped to the work area**, which crops the panel rather than failing.

- [ ] At 150% scaling on a 768-tall display: the whole panel is visible,
      including the footer. If the footer is cut off, the height budget in
      `ui::POPUP_HEIGHT` is over.

## 4. Latency, now that the window is retained

The old create-and-destroy design measured 260–820 ms hotkey-to-first-paint,
most cycles near 700 ms. That number is the baseline this change is supposed to
beat, and it has not been re-measured on a real machine — the gate cannot do it
and neither can I.

The popup shows its own hotkey-to-first-paint time in the status line, as
`paint N ms`, taken from a `Telemetry` armed on hotkey and read inside `paint`
(so it measures pixels, not window objects).

1. Start the launcher, toggle it open ten times, and read the `paint N ms`
   figures off the status line each time.
   **Expected:** every cycle well under 100 ms, and — the point of the retained
   window — the *first* toggle should be in the same range as the second through
   tenth. If the first is dramatically slower, the surface, font atlas, and
   entity tree really are being reused and the win is real. If the tenth is no
   faster than the first, the window is still being recreated; re-check 1a.
2. Compare against the old baseline above and say so in the change notes.
3. `paint --` means no measurement was taken, i.e. no `paint` ran after the
   hotkey armed the telemetry. That is a bug in itself.

## 5. Memory

Previously observed 73 → 95 MB across ten create/destroy cycles, settling near
88 MB. That was the *destroying* design, so it says almost nothing about the
retained one.

1. Leave the launcher resident and toggle it a few hundred times over 15+ minutes.
2. Watch private bytes in Task Manager.
   **Expected:** a plateau, not a climb. A retained window holds a swap chain, a
   font atlas, and the whole entity tree for the life of the process, so the
   steady state is higher than the old 88 MB; what matters is that it stops
   moving. Take three readings a minute apart in the last five minutes — if they
   are still trending up, that is a leak and it is worth finding.

## 6. Text input: the UTF-8 / UTF-16 caret

`Window::handle_input` and every `EntityInputHandler` method speak **UTF-16 code
units**. The model — `query` and `caret` — is a Rust `String` and a `usize`, so
they are **UTF-8 byte offsets**. For ASCII the two coincide, which is why the bug
was cheap to ship. Carrying a UTF-16 index into `String::replace_range` without
converting is not a rendering glitch: it **panics** on a non-boundary index.

The conversions now live in `crates/orca/src/text.rs` and the transitions built
on them (`apply`, `apply_and_mark`, `backspace`, `delete_forward`) are pure
functions, unit-tested over `""`, `"note"`, `"café"`, `"日本語"`, `"a😀b"`, and
`"é😀x 日本"` at **every** UTF-16 index for every sample. That covers the
arithmetic completely.

What the gate cannot cover is whether Win32 and the IME actually send the indices
those functions expect. So:

- [ ] Type `é`, `ü`, `ñ` by keyboard. Caret must track the typed character; no
      jump to the end, no jump to the start.
- [ ] Type those characters, then press <kbd>Left</kbd>/<kbd>Right</kbd> across
      them. The caret must move **one character** at a time, not one byte and
      not one screen pixel.
- [ ] <kbd>Home</kbd> and <kbd>End</kbd> with the caret after non-ASCII text.
- [ ] Type `abc😀xyz`, put the caret at the very end, press <kbd>Backspace</kbd>
      once.
      **Expected:** `abc😀xy`. One press removes the whole emoji. A press that
      leaves `abc�xyz` — a replacement character, or a blank gap — is a
      mid-character index and is the original bug.
- [ ] Same again with <kbd>Delete</kbd> from the start.
- [ ] Type a multi-byte character, then <kbd>Ctrl+A</kbd> and retype. (There is
      no select-all binding wired; use the mouse, or <kbd>Home</kbd> then
      <kbd>Shift+End</kbd> if you add one. If neither exists, note it as missing
      rather than assuming it works.)
- [ ] **IME, with a real Japanese or Chinese IME installed and active.** Type
      `にほん`, commit with <kbd>Space</kbd> or <kbd>Enter</kbd>.
      **Expected:** the composed text lands in the query bar, the list filters on
      commit but *not* on the intermediate pre-edit string, and the candidate
      window appears next to the caret. Then backspace over the committed CJK
      text: it must go one character at a time.
      Also check: while composing, does <kbd>Backspace</kbd> edit the composition
      or the query? The code deliberately leaves composition text to the IME
      (`text::backspace` returns early on a non-empty marked range) so a single
      keypress does not remove two characters. If you see two characters vanish,
      that guard is not working on the real IME path.
- [ ] If you use an emoji picker or Win+Emoji: commit one, then arrow past it.

Any caret in the wrong place after non-ASCII input is the UTF-8/UTF-16 mismatch,
not a rendering bug, and it is worth a bug report with the exact character
sequence that triggered it.

## 7. The popup paints before results arrive

Collection enumerates the Start Menu, opens a COM apartment, reads registry keys,
walks directory trees, and reads SQLite. That runs on a `BackgroundExecutor` via
`cx.spawn`; `render` never ranks, collects, or touches a file. This is a
structural property (there is no code path from `render` to the filesystem) but
"the popup appears promptly" is a timing observation.

- [ ] From a cold start, hotkey immediately. **Expected:** the popup paints and
      takes focus in well under a second, with the status line reading
      `searching...`, and rows appear afterwards. If the popup is *blank* or
      unresponsive until results land, something is on the UI thread that should
      not be — check `request_search` in `ui.rs`.
- [ ] On a machine with a large `C:\Users`, time hotkey → first painted pixel.
      A large file tree should delay the *rows*, not the *window*.
- [ ] Type fast, character by character, into a slow query.
      **Expected:** the list settles on results for what you finally typed and
      never flickers back through results for a prefix. A generation number is
      bumped per keystroke and stale results are dropped; if you see the flicker,
      that check is not working.
- [ ] Esc, then reopen, and re-type the same query. **Expected:** identical row
      order (determinism). A reshuffle under the cursor is a bug.

## 8. Ranking feel

Unit-tested and green, but every weight is a product judgement that has never
been judged by a person looking at a list.

- [ ] Type the first two letters of an app's name. Does the app come first, above
      files and folders that also match?
- [ ] Type a word that appears in a *folder* name (`downloads`). Do folders come
      above files whose names merely contain the letters?
- [ ] Launch something three times, then reopen with an empty query. Is it in the
      recents, near the top?
- [ ] Launch something once, wait a week, type its name. Does a *more* used but
      staler item still outrank the one you just wanted? That trade is
      deliberate; confirm it feels deliberate rather than wrong.
- [ ] Type a deliberate typo. Does a sensible result appear at the *bottom*?
- [ ] Type a query matching nothing. Is the list empty, rather than everything
      shown faintly?
- [ ] Check the status line wording: `3 matches` vs `50 of 400 matches` should
      read differently, and `searching...` should not stick once results land.

## 9. Display, DPI, and multiple monitors

The cursor-monitor lookup converts Win32 **physical** pixels to GPUI **logical**
pixels using that monitor's effective DPI, because `PlatformDisplay::bounds()` is
logical and `MonitorFromPoint` is physical. Getting that wrong puts the popup on
the wrong monitor on any display that is not at 100%, and no unit test can catch
it because the conversion depends on the real display configuration.

- [ ] Cursor on a secondary monitor, hotkey. **Expected:** popup centred on
      *that* monitor, not the primary.
- [ ] Repeat at 125% and 150% display scaling, on both monitors if they differ.
      **Expected:** the same physical size and position regardless of scale.
- [ ] A monitor arranged **above and left** of the primary (negative
      coordinates). The pure selection function is tested for this layout, but
      only the DPI conversion is untested.
- [ ] Unplug a monitor while the popup is hidden, then hotkey.
      **Expected:** the popup lands on a monitor that exists. It is repositioned
      on every show for exactly this reason; if it appears offscreen, that
      reposition is not running.
- [ ] Check the popup is exactly 640 logical px wide.

## 10. `orca-win` platform calls

Implemented and unit-tested, but the tests stop at the seam. A fake cannot prove
Windows does the thing.

- [ ] Bind a hotkey (`Ctrl+Shift+Space`): press it from another app, confirm the
  handler runs, then unbind and confirm the combination is free again.
- [ ] **The tray icon.** It has been broken and is now fixed but unverified. Look
  for an orca icon in the notification area (may need the `^` overflow chevron).
  - The line `tray icon unavailable: tray message window could not be created
    (Win32 error 1813)` should be **gone** from the log. If it is still there, the
    class-name fix did not work — please report it with the full log.
  - Left-click should show the popup; right-click should open a menu with
    *Show orca* and *Quit*; *Quit* must exit the process.
  - The icon must survive 20 popup toggles. It used to be impossible to test this
    at all, because the icon never appeared.
- [ ] Bind a hotkey another app already owns: the failure must be **reported**
      (`…unavailable: …`) and the launcher must carry on.
- [ ] Start the launcher twice: the second must raise the first and exit with
      code 2, leaving no second tray icon.
- [ ] Start the launcher, kill it, start it again: must succeed (the named mutex
      is released when the last handle closes).
- [ ] Enable autostart, then check what Explorer will read:
      `Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'`
      should show a quoted path.
- [ ] Log out and back in: the launcher must come up.
- [ ] Disable autostart twice: the second call must succeed.
- [ ] Tray icon: left click shows the launcher; right click opens the menu; a menu
      entry fires its command; quitting leaves no ghost in the notification area.
- [ ] Tray icon while a second instance is running: must not be duplicated.
- [ ] Launch from a shortcut in a folder with spaces, to confirm the Run-value
      quoting.
- [ ] `installed_apps()`: the list must include something from the Start Menu
      **and** something from App Paths, and must not stall the popup (it runs on
      the background executor).

## 11. What the gate covers, and what it cannot

So you do not have to re-derive this:

| Covered by the gate (422 tests, clippy `-D warnings`) | Not covered |
|---|---|
| Every UTF-16 ⇄ UTF-8 conversion, over six sample strings at every index | Whether Win32 and the IME send those indices |
| The edit transitions, at every index pair × 5 replacement strings | Whether the popup paints before results land |
| Backspace/delete at every caret position of every sample | Whether the window is really retained, or merely logged as such |
| The hide decision table (`hide_plan`) | Whether `ShowWindow(SW_HIDE)` hands focus back |
| **Panel contrast, composited over both a white and a black backdrop** | **Whether the window composites alpha at all — section 2a** |
| **Blur maths: edge opacity, spread, and the 16× cost reduction** | **Whether the capture lines up with the panel at your DPI — section 2d** |
| `InstalledApp` → `RawResult` field-for-field | Whether `installed_apps()` returns a good list on *your* machine |
| The `DirectoryLister` against the real filesystem | Whether the popup is on the right monitor at your DPI |
| Argument quoting, `PATH` resolution, hotkey spec parsing | Anything about latency, memory, or feel |
| Hotkey parse/format rules, frecency, ranking, config | — |

One gap is worth stating plainly rather than hiding: the popup's own
`EntityInputHandler` methods and the `Launcher` state machine **cannot be unit
tested at this GPUI pin.** `gpui::TestAppContext` is behind the `test-support`
feature, which enables the Wayland and X11 backends. That is why the risky logic
was moved out of the trait methods into the pure functions in
`crates/orca/src/text.rs` and the pure `hide_plan` — so it could be tested at
all. The thin GPUI adapters left behind are therefore unverified by
construction, and section 1 and section 6 above are where you check them.

## Not built yet

So nobody assumes otherwise:

- **Multi-monitor and DPI are untested at runtime.** The selection logic is
  unit-tested; the DPI conversion is not tested anywhere.
- **A real tray icon.** `TrayIconSource::Application` uses the stock system
  icon, which is deliberately never destroyed. Shipping a `.ico` via
  `TrayIconSource::File` is untested.
- **No result icons, no fuzzy-match tuning UI, no config file of your own.**
  `[files]` and `[commands]` come from `config.toml`; the launcher falls back to
  defaults with a `config.toml … unavailable` line if it cannot read one.
- **A real `Ctrl+A`.** See section 6.
- **No section headers and no favourites.** The reference groups rows under
  "Favorites" / "Applications". Grouping by `Source` was left out on purpose:
  the ranked list interleaves sources, so contiguous-run grouping would produce
  a dozen one-row headers, and grouping *all* rows by source would silently
  re-order the list against the frecency ranking the policy is built on. The
  right-hand per-row source label carries the same information without
  reordering anything.
- **No app icons, and therefore no shortcut chips.** `crates/orca-win/src/apps.rs`
  is explicit that it is not an icon loader. The chips in the reference are
  trivial to draw and useless without a shortcut registry, and no binding
  exists for them yet, so neither is drawn — a chip advertising a shortcut
  nothing handles is worse than no chip.
- **No actions menu.** `Ctrl+K` is not bound, and gpui at this pin has no
  `popover` element (`anchored()` only) with `ContextMenu` living in Zed's `ui`
  crate, which is not in the graph. The footer advertises *Open* and *Hide*
  only, because those bindings exist.
- **The result rows are still noisy.** Every application row shows its full
  executable path as a subtitle, and at this density the list is much busier
  than the reference. The subtitle is not wrong — it is what tells you *which*
  of four similarly-named things you are about to launch — but it wants to be
  optional, or shown only for the selected row.
- **The five Win32 calls in `src/win.rs` belong in `orca-win`.** They are an
  itemised, documented exception to the layering rule, quarantined in one file
  with the only `unsafe` in the crate, pending that crate being reopened.
