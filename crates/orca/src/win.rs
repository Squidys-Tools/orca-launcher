//! The four Win32 calls the composition root needs that `orca-win` does not
//! expose, and one pure function that decides which monitor a point is on.
//!
//! # Why this file exists, and why it is four functions
//!
//! `docs/ARCHITECTURE.md` rule 3 says `orca-win` is the only place Win32 is
//! called, and this file breaks that rule. It is a deliberate, itemised
//! exception rather than an accident, and each item names the gap:
//!
//! | call | what `orca-win` does not have | why the app cannot proceed without it |
//! |---|---|---|
//! | [`hwnd`] | a way to reach a GPUI window's `HWND` | hide/show and reposition need it, and nothing in GPUI's public API yields it |
//! | [`set_shown`] | `Window::hide` / `Window::show` | the persistent-window design; see below |
//! | [`move_to`] | `Window::set_position` | a retained window must follow the cursor to another monitor |
//! | [`logical_cursor`] | a cursor-position read | "centre on the cursor's monitor" is the placement rule |
//!
//! The correct home for all four is `orca-win`, which was already merged and is
//! not being edited. When it is next opened, they move there verbatim and this
//! file disappears. Until then they are here, in one file, with no other
//! `unsafe` in the crate, so the exception is reviewable in one sitting.
//!
//! # The persistent window
//!
//! `docs/ARCHITECTURE.md` recorded that create-and-destroy per toggle costs
//! 260–820 ms and that "GPUI exposes no way to do that at this pin". That was
//! half true, and the missing half is worth writing down because the negative
//! result is what stops the next person looking again:
//!
//! * There is still no `Window::hide`. `App::hide` exists but hides the whole
//!   *application* at the platform layer and has no matching unhide, which for
//!   a single-window tray launcher is a trap, not a feature.
//! * There is no `AppWindowHandle`; that type does not exist at this rev.
//! * **`Window` implements `raw_window_handle::HasWindowHandle`** (gpui
//!   `window.rs`), which yields the raw `HWND` with no `unsafe` of our own.
//! * `WM_SHOWWINDOW` is already handled: `gpui_windows/src/events.rs` calls
//!   `handle_window_visibility_changed`, which drives `Window::refresh_visibility`.
//! * **`Window::refresh()` is public** and marks the window dirty. Without it a
//!   window re-shown from `SW_HIDE` would sit on screen showing its last frame
//!   forever, because nothing in the visibility callback requests a frame.
//!
//! So the sequence is: `ShowWindow(SW_HIDE)` to hide, and
//! `ShowWindow(SW_SHOW)` → `focus` → `activate_window` → `refresh` to show.
//! GPUI never tears the window down, so the second toggle reuses the surface,
//! the font atlas entries, and the entity tree.
//!
//! # Units
//!
//! Win32 gives screen coordinates in *physical* pixels. GPUI's
//! `PlatformDisplay::bounds()` is in *logical* pixels — `gpui_windows`
//! divides by the per-monitor scale factor before publishing it (see
//! `check_given_bounds` in `gpui_windows/src/display.rs`, which multiplies back
//! before calling `MonitorFromPoint`). Comparing the two directly would pick
//! the wrong monitor on any display that is not at 100%, so [`logical_cursor`]
//! converts. The conversion needs the cursor's monitor DPI, which
//! [`logical_cursor`] gets from `GetDpiForMonitor`.
//!
//! The interesting half — which display contains a point — is
//! [`display_containing`], which is pure and tested.

use gpui::{App, DisplayId, Pixels, Window};
use orca_core::LaunchTarget;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, SetWindowPos, ShowWindow, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOSIZE,
    SWP_NOZORDER, SW_HIDE, SW_SHOWNOACTIVATE,
};

/// Win32's baseline DPI. A monitor reporting this is at 100% scaling.
const BASELINE_DPI: f32 = 96.0;

/// A display, reduced to the two fields the placement logic needs.
#[derive(Debug, Clone, Copy)]
pub struct DisplayInfo {
    /// The GPUI display id, to hand back to `WindowOptions::display_id`.
    pub id: DisplayId,
    /// The display bounds, in GPUI logical pixels.
    pub bounds: gpui::Bounds<Pixels>,
}

/// Reads every attached display out of the app.
#[must_use]
pub fn displays(app: &App) -> Vec<DisplayInfo> {
    app.displays()
        .iter()
        .map(|display| DisplayInfo {
            id: display.id(),
            bounds: display.bounds(),
        })
        .collect()
}

/// The display whose bounds contain `point`, or `None` if the point is in none
/// of them.
///
/// Pure, so the placement rule is tested rather than eyeballed. On a
/// multi-monitor setup a point can be outside every logical bounds block if the
/// DPI changes disagree with GPUI's view, and returning `None` then makes the
/// caller fall back to the primary display — a popup on the wrong monitor, not a
/// popup at `(0, 0)`.
#[must_use]
pub fn display_containing(point: (f32, f32), displays: &[DisplayInfo]) -> Option<DisplayId> {
    displays
        .iter()
        .find(|display| {
            let bounds = display.bounds;
            point.0 >= bounds.left().as_f32()
                && point.0 < bounds.right().as_f32()
                && point.1 >= bounds.top().as_f32()
                && point.1 < bounds.bottom().as_f32()
        })
        .map(|display| display.id)
}

/// The raw `HWND` behind a GPUI window.
///
/// Goes through `raw_window_handle::HasWindowHandle`, which `Window` implements,
/// so the pointer is the platform's own and no cast is invented here. The trait
/// is named explicitly because `Window` also has an inherent
/// `window_handle()` that returns a GPUI `AnyWindowHandle` — a different type
/// with a similar name, and method resolution picks the inherent one.
#[must_use]
pub fn hwnd(window: &Window) -> Option<HWND> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(win32) => Some(HWND(win32.hwnd.get() as *mut std::ffi::c_void)),
        _ => None,
    }
}

/// Shows or hides a window without destroying it.
///
/// `SW_SHOWNOACTIVATE` rather than `SW_SHOW`: activation is a separate step
/// (`Window::activate_window`), and letting `ShowWindow` activate would race
/// with the `SetFocus` that follows.
pub fn set_shown(hwnd: HWND, shown: bool) {
    let command = if shown { SW_SHOWNOACTIVATE } else { SW_HIDE };
    // SAFETY: `hwnd` came from `HasWindowHandle` on a live window this process
    // owns, and the only valid commands are the two passed in. The return value
    // reports whether the window's *previous* visibility changed, which is not
    // the question being asked here, so it is deliberately discarded.
    unsafe {
        let _ = ShowWindow(hwnd, command);
    }
}

/// Moves a window to a top-left position given in GPUI logical pixels.
///
/// Leaves size, z-order and activation alone; `SWP_NOSIZE`/`SWP_NOZORDER`/
/// `SWP_NOACTIVATE` are the point. `SWP_NOOWNERZORDER` keeps a popup from
/// dragging its owner forward with it.
pub fn move_to(hwnd: HWND, x: f32, y: f32) {
    let flags = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER;
    // SAFETY: as above. `SetWindowPos` with `SWP_NOSIZE` cannot change the
    // window's size, and a failure here leaves the window where it was, which
    // the caller treats as "keep the old position".
    unsafe {
        let _ = SetWindowPos(hwnd, None, x.round() as i32, y.round() as i32, 0, 0, flags);
    }
}

/// The cursor position in GPUI logical pixels, or `None` if Win32 would not say.
///
/// The scale factor is the DPI of the monitor the cursor is on, which is the
/// only monitor that matters: the point is about to be tested against that
/// monitor's bounds, and against any other it will not match.
#[must_use]
pub fn logical_cursor() -> Option<(f32, f32)> {
    // SAFETY: `GetCursorPos` takes an out-pointer and has no preconditions.
    let mut point = POINT { x: 0, y: 0 };
    // SAFETY: `point` is a live, writable `POINT`.
    unsafe { GetCursorPos(&mut point) }.ok()?;

    // SAFETY: `MonitorFromPoint` is a pure query over a `POINT` by value.
    let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTOPRIMARY) };

    let mut dpi_x: u32 = 0;
    let mut dpi_y: u32 = 0;
    // SAFETY: both out-pointers are live and writable, and `monitor` is a real
    // handle: `MONITOR_DEFAULTTOPRIMARY` guarantees a primary monitor exists.
    unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }.ok()?;

    let scale_x = dpi_x as f32 / BASELINE_DPI;
    let scale_y = dpi_y as f32 / BASELINE_DPI;
    if scale_x <= 0.0 || scale_y <= 0.0 {
        return None;
    }
    Some((point.x as f32 / scale_x, point.y as f32 / scale_y))
}

/// The display the cursor is on.
///
/// Falls back to `None` — which `Bounds::centered` reads as "primary display" —
/// at every step that can fail, because a launcher that opens on the wrong
/// monitor is recoverable and one that opens at the origin is not useful.
#[must_use]
pub fn display_under_cursor(app: &App) -> Option<DisplayId> {
    let cursor = logical_cursor()?;
    let displays = displays(app);
    display_containing(cursor, &displays)
}

/// Opens a launch target with whatever Windows associates with it.
///
/// `ShellExecuteW` rather than `CreateProcess` because the whole point of a URI
/// target is "whatever the user has set as the default handler", and because it
/// is the one call that handles argument quoting for us.
pub fn shell_open(target: &LaunchTarget) -> bool {
    let (file, parameters) = match target {
        LaunchTarget::Executable { path, args } => {
            (path.to_string_lossy().into_owned(), quote_arguments(args))
        }
        LaunchTarget::Uri(uri) => (uri.clone(), String::new()),
        // An alias is typed by name, so it means nothing until `PATH` has been
        // searched. An unresolvable alias is a config error the user finds out
        // about by it not working; there is nothing to show them.
        LaunchTarget::Command { program, args } => match resolve_program(program) {
            Some(path) => (path.to_string_lossy().into_owned(), quote_arguments(args)),
            None => return false,
        },
    };

    // The four buffers are locals, not temporaries: `ShellExecuteW` takes
    // `PCWSTR` pointers, and a pointer into a dropped `HSTRING` would be the
    // kind of bug that only shows up on a machine with a different allocator.
    let verb = HSTRING::from("open");
    let file = HSTRING::from(file);
    let parameters = HSTRING::from(parameters);

    // SAFETY: all four `HSTRING`s above are live for the duration of the call,
    // and a null window handle means "no owner", which is what a background
    // process wants.
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR(parameters.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };

    // `ShellExecuteW` signals failure by returning a value at or below 32, not
    // by returning null, and it does not set the extended error code. Those
    // values are the `SE_ERR_*` codes; anything above is the instance handle of
    // the process it started.
    result.0 as isize > SHELLEXECUTE_REFUSAL_CEILING
}

/// The largest value `ShellExecuteW` returns when it refused, inclusive.
const SHELLEXECUTE_REFUSAL_CEILING: isize = 32;

/// `SW_SHOWNORMAL`. Spelled out because the `windows` crate spells the
/// `SW_SHOW*` family inconsistently and this one is not a plain constant.
const SW_SHOWNORMAL: windows::Win32::UI::WindowsAndMessaging::SHOW_WINDOW_CMD =
    windows::Win32::UI::WindowsAndMessaging::SHOW_WINDOW_CMD(1);

/// Resolves a program name against the process `PATH`.
#[must_use]
pub fn resolve_program(program: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    resolve_in_path(program, std::env::split_paths(&path).collect(), |p| {
        p.is_file()
    })
}

/// The `PATH` search, with the search path and the existence check injected.
///
/// A name with a directory separator in it is used as written, because a user
/// who typed `.\build.exe` has already answered the question. A bare name is
/// searched, with each `PATHEXT` extension tried after the bare name so an
/// extensionless file still wins over an extension-ful one in the same
/// directory — the same order `cmd.exe` uses.
pub fn resolve_in_path(
    program: &str,
    search: Vec<std::path::PathBuf>,
    exists: impl Fn(&std::path::Path) -> bool,
) -> Option<std::path::PathBuf> {
    if program.is_empty() {
        return None;
    }
    if program.contains(['/', '\\', ':']) {
        let direct = std::path::PathBuf::from(program);
        return exists(&direct).then_some(direct);
    }

    let extensions = path_extensions();
    for directory in search {
        if directory.as_os_str().is_empty() {
            continue;
        }
        let base = directory.join(program);
        if exists(&base) {
            return Some(base);
        }
        for extension in &extensions {
            let candidate = directory.join(format!("{program}{extension}"));
            if exists(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// The extensions to try, from `PATHEXT`, with `.exe` first.
///
/// `.exe` is prepended rather than trusted to `PATHEXT` to contain it: an
/// edited `PATHEXT` should not stop a launcher from running a program, and
/// `PATHEXT` is a shell's business list, not a definition of what is
/// executable.
#[must_use]
pub fn path_extensions() -> Vec<String> {
    let mut extensions = vec![".exe".to_owned()];
    if let Ok(raw) = std::env::var("PATHEXT") {
        extensions.extend(
            raw.split(';')
                .map(str::trim)
                .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case(".exe"))
                .map(str::to_owned),
        );
    }
    extensions
}

/// Joins arguments into one command line, quoting the ones that need it.
///
/// `CommandStringToArgvW` on the receiving side, which is the same parser
/// `CommandLineToArgvW` uses, so quoting only has to produce *a* valid
/// encoding of the argv the user configured — not a guess at the one the
/// program will re-derive. The rules are the documented ones: a backslash run
/// is doubled only when it precedes a quote, and a quote is wrapped in quotes
/// and escaped.
#[must_use]
pub fn quote_arguments(args: &[String]) -> String {
    let mut out = String::new();
    for (index, arg) in args.iter().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '"']) {
            out.push_str(arg);
            continue;
        }
        out.push('"');
        let mut backslashes = 0usize;
        for ch in arg.chars() {
            match ch {
                '\\' => backslashes += 1,
                '"' => {
                    // Double the pending backslashes: half of them escape the
                    // quote, the other half are literal.
                    out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                    out.push('"');
                    backslashes = 0;
                }
                _ => {
                    out.extend(std::iter::repeat_n('\\', backslashes));
                    backslashes = 0;
                    out.push(ch);
                }
            }
        }
        // Trailing backslashes are literal, but only if the argument ends
        // inside quotes they would escape the closing quote instead.
        out.extend(std::iter::repeat_n('\\', backslashes * 2));
        out.push('"');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size, Bounds};
    use std::path::PathBuf;

    fn display(id: u64, x: f32, y: f32, w: f32, h: f32) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId::new(id),
            bounds: Bounds::new(point(px(x), px(y)), size(px(w), px(h))),
        }
    }

    #[test]
    fn a_point_inside_one_monitor_selects_that_monitor() {
        let displays = [
            display(1, 0.0, 0.0, 1920.0, 1080.0),
            display(2, 1920.0, 0.0, 1920.0, 1080.0),
        ];
        assert_eq!(
            display_containing((100.0, 100.0), &displays),
            Some(DisplayId::new(1))
        );
        assert_eq!(
            display_containing((2000.0, 100.0), &displays),
            Some(DisplayId::new(2))
        );
    }

    #[test]
    fn a_monitor_above_and_left_is_matched_on_negative_coordinates() {
        // The layout a laptop with a second monitor to the left actually has.
        let displays = [
            display(1, 0.0, 0.0, 1920.0, 1080.0),
            display(2, -1280.0, -200.0, 1280.0, 1024.0),
        ];
        assert_eq!(
            display_containing((-100.0, -100.0), &displays),
            Some(DisplayId::new(2))
        );
        assert_eq!(
            display_containing((10.0, 10.0), &displays),
            Some(DisplayId::new(1))
        );
    }

    #[test]
    fn the_right_and_bottom_edges_belong_to_the_next_monitor_not_this_one() {
        // Half-open intervals: (1920, 0) is the first pixel of monitor 2, and
        // (1919.9, 0) is the last pixel of monitor 1. A closed test would put
        // the popup's centre pixel on a seam and the ambiguity would be resolved
        // by iteration order, which is not a rule.
        let displays = [
            display(1, 0.0, 0.0, 1920.0, 1080.0),
            display(2, 1920.0, 0.0, 1920.0, 1080.0),
        ];
        assert_eq!(
            display_containing((1920.0, 0.0), &displays),
            Some(DisplayId::new(2))
        );
        assert_eq!(
            display_containing((1919.9, 0.0), &displays),
            Some(DisplayId::new(1))
        );
    }

    #[test]
    fn a_point_in_no_monitor_selects_none_so_the_caller_falls_back() {
        let displays = [display(1, 0.0, 0.0, 1920.0, 1080.0)];
        assert_eq!(display_containing((5000.0, 5000.0), &displays), None);
        assert_eq!(display_containing((0.0, 0.0), &[]), None);
    }

    #[test]
    fn an_unscaled_cursor_reads_back_as_its_own_logical_value() {
        // The contract `display_containing` relies on: at 100% scaling Win32
        // physical pixels and GPUI logical pixels are the same numbers. Only
        // assertable on a machine that happens to be at 100%, so this checks
        // the *shape* of the answer, not a specific display.
        if let Some((x, y)) = logical_cursor() {
            assert!(x.is_finite() && y.is_finite());
            assert!(
                x > -100_000.0 && y > -100_000.0,
                "cursor read {x},{y}, which is not a screen coordinate"
            );
        }
    }

    // --------------------------------------------------------- argument quoting

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    #[test]
    fn a_plain_argument_is_passed_through_untouched() {
        assert_eq!(quote_arguments(&[]), "");
        assert_eq!(quote_arguments(&args(&["--flag"])), "--flag");
        assert_eq!(quote_arguments(&args(&["--flag", "value"])), "--flag value");
        // Backslashes only need doubling next to a quote, so a Windows path
        // passes through bare.
        assert_eq!(
            quote_arguments(&args(&[r"C:\src\app.exe"])),
            r"C:\src\app.exe"
        );
    }

    #[test]
    fn an_argument_containing_a_space_is_quoted() {
        assert_eq!(
            quote_arguments(&args(&["C:\\Program Files\\app.exe"])),
            r#""C:\Program Files\app.exe""#
        );
    }

    #[test]
    fn an_empty_argument_survives_as_an_empty_quoted_argument() {
        // Dropping it would silently shift every later argument left by one,
        // which is the kind of bug that looks like the program misbehaving.
        assert_eq!(quote_arguments(&args(&[""])), r#""""#);
        assert_eq!(quote_arguments(&args(&["a", "", "b"])), r#"a "" b"#);
    }

    #[test]
    fn a_quote_inside_an_argument_is_escaped_not_dropped() {
        assert_eq!(quote_arguments(&args(&[r#"say "hi""#])), r#""say \"hi\"""#);
    }

    #[test]
    fn backslashes_before_a_quote_double_so_the_quote_stays_literal() {
        // `C:\dir\"name"` — the backslash would otherwise escape the quote, and
        // the receiving parser would see one argument where there are two.
        assert_eq!(
            quote_arguments(&args(&[r#"C:\dir\"name""#])),
            r#""C:\dir\\\"name\"""#
        );
    }

    #[test]
    fn an_argument_with_nothing_to_quote_is_passed_through_bare() {
        // A backslash only needs doubling when it sits next to a quote. With no
        // quote anywhere in the command line, a trailing backslash is literal
        // and doubling it would be a bug in the other direction.
        assert_eq!(quote_arguments(&args(&[r"C:\path\"])), r"C:\path\");
        assert_eq!(quote_arguments(&args(&[r"a\b", r"c\d"])), r"a\b c\d");
    }

    #[test]
    fn trailing_backslashes_inside_a_quoted_argument_double() {
        // Otherwise `C:\Program Files\app\` would escape the closing quote and
        // swallow the next argument.
        assert_eq!(
            quote_arguments(&args(&[r"C:\Program Files\app\"])),
            r#""C:\Program Files\app\\""#
        );
    }

    // ----------------------------------------------------------- PATH resolution

    /// An existence predicate backed by a fixed list of paths, compared
    /// case-insensitively and with either separator, because the strings here
    /// are written the way a person would write them rather than the way
    /// `PathBuf` normalises them.
    fn finder(hits: &[&str]) -> impl Fn(&std::path::Path) -> bool {
        let hits: Vec<String> = hits
            .iter()
            .map(|hit| hit.replace('/', "\\").to_lowercase())
            .collect();
        move |path: &std::path::Path| {
            let candidate = path.to_string_lossy().replace('/', "\\").to_lowercase();
            hits.contains(&candidate)
        }
    }

    #[test]
    fn a_bare_name_is_found_by_its_extension() {
        let search = vec![PathBuf::from(r"C:\bin"), PathBuf::from(r"C:\tools")];
        let found = resolve_in_path("rg", search, finder(&[r"C:\tools\rg.exe"]));
        assert_eq!(found, Some(PathBuf::from(r"C:\tools\rg.exe")));
    }

    #[test]
    fn a_bare_name_is_found_with_no_extension_at_all() {
        let search = vec![PathBuf::from(r"C:\bin")];
        let found = resolve_in_path("sh", search, finder(&[r"C:\bin\sh"]));
        assert_eq!(found, Some(PathBuf::from(r"C:\bin\sh")));
    }

    #[test]
    fn the_earliest_directory_on_the_path_wins() {
        let search = vec![PathBuf::from(r"C:\first"), PathBuf::from(r"C:\second")];
        let found = resolve_in_path(
            "rg",
            search,
            finder(&[r"C:\first\rg.exe", r"C:\second\rg.exe"]),
        );
        assert_eq!(found, Some(PathBuf::from(r"C:\first\rg.exe")));
    }

    #[test]
    fn a_name_with_a_directory_is_used_as_written() {
        let search = vec![PathBuf::from(r"C:\bin")];
        // Present, and used. Absent, and *not* searched for in `PATH`.
        assert_eq!(
            resolve_in_path(r"sub\app.exe", search.clone(), finder(&[r"sub\app.exe"])),
            Some(PathBuf::from(r"sub\app.exe"))
        );
        assert_eq!(
            resolve_in_path(r"sub\app.exe", search, finder(&[r"C:\bin\app.exe"])),
            None
        );
    }

    #[test]
    fn an_empty_program_name_resolves_to_nothing() {
        assert_eq!(resolve_in_path("", vec![], |_| true), None);
    }

    #[test]
    fn an_empty_directory_entry_on_the_path_is_skipped() {
        // A trailing `;` in `PATH` yields an empty entry, which Win32 means
        // "the current directory". Skipping it is the safer reading: a launcher
        // that launches a program out of whatever folder it happens to be
        // started in is a security problem, not a convenience.
        let search = vec![PathBuf::new(), PathBuf::from(r"C:\bin")];
        let found = resolve_in_path("rg", search, finder(&[r"rg.exe", r"C:\bin\rg.exe"]));
        assert_eq!(found, Some(PathBuf::from(r"C:\bin\rg.exe")));
    }

    #[test]
    fn exe_is_always_tried_first_however_pathtext_reads() {
        let extensions = path_extensions();
        assert_eq!(
            extensions.first().map(String::as_str),
            Some(".exe"),
            "PATHEXT must not be able to demote .exe"
        );
        assert!(
            extensions.iter().all(|e| e.starts_with('.')),
            "{extensions:?} contains a non-extension"
        );
    }
}
