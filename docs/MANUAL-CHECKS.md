# Manual checks

Things a human must look at. Each entry says what to do, what "good" looks like,
and what the code already guarantees, so the check is narrow.

Nothing here is automated on purpose. Keystroke injection into a popup is a poor
substitute for a person looking at the screen, so `tools/soak.ps1` exists for
optional stress runs but the judgement calls below stay human.

## How to run

```powershell
./tools/gate.ps1              # must be green before any of this
cargo run -p orca --target x86_64-pc-windows-gnu
```

## First run of the probe

| # | Check | How | Expected |
|---|-------|-----|----------|
| 1 | Process stays resident | Launch, wait, do nothing | No window appears, process still alive |
| 2 | Hotkey opens the popup | `Ctrl+Alt+Space` | Popup appears and takes focus within ~1s |
| 3 | Popup has focus | Type immediately | Text appears with a blinking caret |
| 4 | Text filters | Type `note` | List narrows; `Notepad` visible |
| 5 | Esc hides, does not kill | `Esc`, then check the process | Popup gone, **process still running** |
| 6 | Reopen works | `Ctrl+Alt+Space` again | Popup reappears focused, not blank |
| 7 | Repeated toggling | `Esc` / `Ctrl+Alt+Space` ×10 | No flicker, no duplicate windows, no hang |

Check 5 is the one that regressed most: it failed twice during development, and
`QuitMode::Explicit` is the only reason it passes now.

## Latency

Measured during development: 260–820 ms from hotkey to first paint, with most
cycles around 700 ms. That is slow for a launcher and is the main open
performance issue.

- [ ] Time `Ctrl+Alt+Space` → first painted pixel with a stopwatch or screen recorder
- [ ] Note whether the delay is before or after the window appears
- [ ] Compare cold (first launch after boot) against warm

## Memory

Observed 73 → 95 MB across ten create/destroy cycles, settling near 88 MB. Looks
like bounded caching rather than a leak, but it has not been run long enough to
be sure.

- [ ] Leave the popup toggling for 10+ minutes
- [ ] Watch private bytes in Task Manager
- [ ] Confirm it plateaus rather than climbing without bound

## Text input

The caret is a UTF-16 index while the model stores UTF-8 byte offsets. ASCII is
therefore proven and everything else is not.

- [ ] Type accented characters (`é`, `ü`)
- [ ] Type CJK via IME
- [ ] Type emoji
- [ ] Backspace over a multi-byte character
- [ ] Home/End, and arrow keys, with the caret after non-ASCII text
- [ ] Select-all then retype

Any caret that lands in the wrong place after non-ASCII input is the UTF-8/UTF-16
mismatch, not a rendering bug.

## Display

- [ ] Popup appears on the correct monitor when the cursor is on a secondary one
- [ ] Correct size and position at 125% and 150% display scaling
- [ ] Does not appear offscreen after a monitor is unplugged while it is open

## Not yet built

These do not exist yet; listed so nobody assumes they are done:

- Persistent-window hide/show. Pinned GPUI exposes no public `Window::hide`, so
  the current design destroys and recreates the window per toggle.
- Multiple monitors and DPI are untested.
- `cx.activate(true)` is a no-op on Windows; `Window::activate_window()` is used
  instead. The first keystroke after activation is worth watching for.
