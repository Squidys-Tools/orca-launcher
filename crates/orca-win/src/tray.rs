//! Tray icon: a notification-area icon with a context menu, driven by its own
//! message loop.
//!
//! # Why a hand-rolled pump
//!
//! `Shell_NotifyIconW` does not take a callback. It sends a message to a window
//! you name, and that window must have a message loop draining its queue or the
//! clicks go nowhere. There is no "run this closure" option, so the choice is
//! between a window class with a real `WNDPROC` and a shell extension. The
//! former is about eighty lines and has no dependency; the latter is a COM
//! server. This is the former.
//!
//! # Resource lifetime
//!
//! Two things must outlive the icon, and both are easy to get wrong:
//!
//! * the `NOTIFYICONDATA` buffer. It is passed by pointer and the shell copies
//!   out of it during the call, so the buffer itself is only needed across the
//!   call — but it is built in one place and used in three (`NIM_ADD`,
//!   `NIM_MODIFY`, `NIM_DELETE`), and the handle and id inside it must be
//!   byte-identical each time or the shell is talking about a different icon.
//!   [`NotifyIconData`] exists to make that impossible to get wrong by hand.
//! * the `HICON`. `NIM_DELETE` must be issued *before* the icon is destroyed,
//!   or the shell is left holding a handle to freed memory. The drop order in
//!   `TrayIcon` is what enforces that, and it is load-bearing.
//!
//! # Why not `NIM_SETVERSION`
//!
//! Version 4 packs the icon id and the click coordinates into `lParam` in a way
//! that is easy to get subtly wrong, and its benefit for a launcher is nil.
//! The default version 1 delivers the plain mouse message in `lParam` and the
//! id in `wParam`, which is what this module decodes.

use std::fmt;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
    NOTIFY_ICON_MESSAGE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, GetWindowLongPtrW, LoadImageW,
    PostQuitMessage, PostThreadMessageW, RegisterClassW, SetForegroundWindow, SetWindowLongPtrW,
    TrackPopupMenuEx, TranslateMessage, UnregisterClassW, CREATESTRUCTW, GWLP_USERDATA, HICON,
    HMENU, IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTSIZE, LR_LOADFROMFILE, MF_SEPARATOR, MF_STRING,
    MSG, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CONTEXTMENU,
    WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NCCREATE, WM_NCDESTROY, WM_QUIT, WM_RBUTTONUP,
    WNDCLASSW,
};

use crate::wide::{copy_wide_fixed, wide_nul};

/// Something went wrong driving the tray icon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayError {
    /// The hidden message window or its class could not be created.
    WindowCreationFailed {
        /// The raw Win32 error code, as `GetLastError()` reported it.
        code: u32,
    },
    /// `Shell_NotifyIconW` refused the operation.
    ///
    /// Almost always means Explorer is not running yet, or the icon was already
    /// deleted. Reported rather than ignored: a tray icon that silently failed
    /// to appear is indistinguishable from one that was never asked for.
    NotifyFailed {
        /// The `NIM_*` operation that was refused.
        operation: &'static str,
        /// The raw Win32 error code, as `GetLastError()` reported it.
        code: u32,
    },
    /// The message pump thread could not be started.
    PumpThreadFailed {
        /// What went wrong, for the log line.
        reason: String,
    },
}

impl fmt::Display for TrayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrayError::WindowCreationFailed { code } => {
                write!(
                    f,
                    "tray message window could not be created (Win32 error {code})"
                )
            }
            TrayError::NotifyFailed { operation, code } => {
                write!(
                    f,
                    "Shell_NotifyIcon {operation} failed (Win32 error {code})"
                )
            }
            TrayError::PumpThreadFailed { reason } => {
                write!(f, "tray pump thread failed: {reason}")
            }
        }
    }
}

impl std::error::Error for TrayError {}

// ---------------------------------------------------------------------------
// Public model
// ---------------------------------------------------------------------------

/// One entry in the tray context menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayMenuItem {
    /// What the user sees.
    pub label: String,
    /// The command reported back when this entry is chosen.
    pub command: String,
    /// A separator has no label or command and is not selectable.
    pub separator: bool,
}

impl TrayMenuItem {
    /// A selectable entry.
    pub fn new(label: &str, command: &str) -> TrayMenuItem {
        TrayMenuItem {
            label: label.to_owned(),
            command: command.to_owned(),
            separator: false,
        }
    }

    /// A divider.
    pub fn separator() -> TrayMenuItem {
        TrayMenuItem {
            label: String::new(),
            command: String::new(),
            separator: true,
        }
    }
}

/// Where the icon image comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayIconSource {
    /// The stock application icon. Nothing to load and nothing to destroy.
    Application,
    /// A `.ico` file. Loaded, and destroyed after `NIM_DELETE`.
    File(std::path::PathBuf),
}

/// What the tray icon is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraySpec {
    /// Hover text. Truncated to the 128 code units `NOTIFYICONDATA` allows.
    pub tooltip: String,
    /// The image.
    pub icon: TrayIconSource,
    /// The context menu, in order.
    pub menu: Vec<TrayMenuItem>,
}

impl TraySpec {
    /// A spec with the stock icon, the given tooltip, and the given menu.
    pub fn new(tooltip: &str, menu: Vec<TrayMenuItem>) -> TraySpec {
        TraySpec {
            tooltip: tooltip.to_owned(),
            icon: TrayIconSource::Application,
            menu,
        }
    }
}

/// Something the user did to the tray icon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayEvent {
    /// Left click: the launcher should show itself.
    LeftClick,
    /// Double left click: same intent, and the gesture Windows users reach for.
    DoubleClick,
    /// Right click, opening the context menu. The menu choice, if any, follows
    /// as a separate [`TrayEvent::MenuCommand`].
    ContextMenuOpened,
    /// A menu entry was chosen. Carries the command string from the spec.
    MenuCommand(String),
}

// ---------------------------------------------------------------------------
// The NOTIFYICONDATA builder
// ---------------------------------------------------------------------------

/// The id the shell knows this icon by. Arbitrary but fixed: it must be
/// identical across `NIM_ADD`, `NIM_MODIFY`, and `NIM_DELETE` or the shell is
/// addressing a different icon.
const TRAY_ICON_ID: u32 = 1;

/// The private message the shell posts to our window for icon callbacks.
const TRAY_CALLBACK_MESSAGE: u32 = WM_APP + 1;

/// Builds a `NOTIFYICONDATAW` for one operation.
///
/// Exists as a function because the three calls must agree: the handle and the
/// id are how the shell identifies the icon, and hand-building the struct three
/// times is how they drift apart. The `NIM_DELETE` case sets neither icon nor
/// tip, because the shell ignores everything but the handle and id there.
pub(crate) fn notify_icon_data(
    operation: NOTIFY_ICON_MESSAGE,
    hwnd: HWND,
    icon: HICON,
    tooltip: &str,
) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        ..Default::default()
    };

    if operation != NIM_DELETE {
        // NIF_MESSAGE is what routes the shell's clicks back to `hwnd` at all;
        // without it the icon is inert and nothing is ever delivered.
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = TRAY_CALLBACK_MESSAGE;
        data.hIcon = icon;
        copy_wide_fixed(tooltip, &mut data.szTip);
    }

    data
}

/// Decodes the packed callback message into a [`TrayEvent`].
///
/// Under the default (pre-v4) notification version the shell puts the mouse
/// message in `lParam` and the icon id in `wParam`. `WM_CONTEXTMENU` is treated
/// as equivalent to `WM_RBUTTONUP` because Windows sends the former on some
/// configurations and only the latter on others — checking one and showing a
/// menu the user can reach is not worth the version-4 packing.
pub(crate) fn decode_tray_message(message: u32) -> Option<TrayEvent> {
    match message {
        WM_LBUTTONUP => Some(TrayEvent::LeftClick),
        WM_LBUTTONDBLCLK => Some(TrayEvent::DoubleClick),
        WM_RBUTTONUP | WM_CONTEXTMENU => Some(TrayEvent::ContextMenuOpened),
        _ => None,
    }
}

/// Command id that `AppendMenuW` is given for menu entry `index`.
///
/// Menu ids start at 1 because `TrackPopupMenuEx` with `TPM_RETURNCMD` returns 0
/// when the menu is dismissed, and 0 must therefore mean "nothing chosen".
pub(crate) fn menu_command_id(index: usize) -> usize {
    index + 1
}

/// Inverse of [`menu_command_id`]. `None` for the dismissal value.
pub(crate) fn menu_index_from_command(command: usize) -> Option<usize> {
    command.checked_sub(1)
}

// ---------------------------------------------------------------------------
// The pump thread
// ---------------------------------------------------------------------------

/// The state the window procedure reaches through `GWLP_USERDATA`.
struct PumpState {
    /// Delivers decoded events to whoever owns the `TrayIcon`.
    events: Sender<TrayEvent>,
    /// The menu to show on a right click.
    menu: Vec<TrayMenuItem>,
}

/// Runs the message loop for the tray window.
///
/// The window class name for this process's tray pump.
///
/// Unique per process *and* per thread, so a test's tray and the real one cannot
/// collide inside one process.
///
/// Only characters valid in a file name are allowed. Windows rejects anything
/// else for a class name, and does so confusingly: `RegisterClassW` succeeds and
/// the failure surfaces at `CreateWindowExW` as ERROR_RESOURCE_TYPE_NOT_FOUND
/// (1813) — "class not found" for a class that was just registered. The
/// `ThreadId(3)` that `{:?}` produces contains parentheses, which is exactly such
/// a character, so the thread id is taken from Win32 rather than from
/// `std::thread::current().id()` — whose only stable formatting route is
/// `Debug`, and whose numeric accessor is still unstable.
fn tray_class_name() -> String {
    // SAFETY: `GetCurrentThreadId` takes no arguments, has no preconditions and
    // cannot fail; it is a pure query about the calling thread.
    let thread = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    format!("OrcaTrayWindow-{}-{}", std::process::id(), thread)
}

/// Lives on its own thread because it must: the loop blocks, and the caller's
/// thread is the one that has to keep answering questions.
fn pump_body(spec: TraySpec, events: Sender<TrayEvent>, ready: Sender<Result<u32, TrayError>>) {
    // SAFETY: a null name asks for the address of the main module, which is
    // what a window class needs for its `hInstance`.
    let module = match unsafe { GetModuleHandleW(None) } {
        Ok(module) => HINSTANCE(module.0),
        Err(e) => {
            let _ = ready.send(Err(TrayError::WindowCreationFailed {
                code: crate::single_instance::last_error_code(&e),
            }));
            return;
        }
    };

    // The class name includes this thread's id so two tray icons in one process
    // — a test's and the real one, say — cannot collide. `RegisterClassW` fails
    // for a name that is already registered, which would otherwise make the
    // second one fail for no visible reason.
    //
    // The id is formatted with `as_u64`, NOT with `{:?}`. `Debug` for
    // `ThreadId` prints `ThreadId(3)`, and the parentheses are not legal in a
    // window class name: MSDN restricts class names to characters that are valid
    // in a file name, and `(` and `)` are excluded. The failure mode is nasty
    // rather than obvious — `RegisterClassW` *succeeds*, and then
    // `CreateWindowExW` fails with ERROR_RESOURCE_TYPE_NOT_FOUND (1813),
    // "the class does not exist", for a class that was registered a moment
    // earlier. That cost a full debugging session: the tray icon simply never
    // appeared, with an error that pointed nowhere near the real cause.
    let class_name = tray_class_name();
    let class_wide = wide_nul(&class_name);

    let window_class = WNDCLASSW {
        lpfnWndProc: Some(tray_wndproc),
        hInstance: module,
        lpszClassName: PCWSTR(class_wide.as_ptr()),
        ..Default::default()
    };

    // SAFETY: `window_class` is a live, fully initialised WNDCLASSW whose
    // strings point at NUL-terminated buffers that outlive the call. The wndproc
    // is a plain `extern "system"` function, so the vtable entry is valid for
    // the process lifetime.
    if unsafe { RegisterClassW(&window_class) } == 0 {
        let _ = ready.send(Err(TrayError::WindowCreationFailed {
            code: unsafe { windows::Win32::Foundation::GetLastError() }.0,
        }));
        return;
    }

    // The `Box` is leaked into the window as `GWLP_USERDATA` and reclaimed in
    // `WM_NCDESTROY`. That pairing is what makes the wndproc's access sound, and
    // it is the only raw pointer crossing the FFI boundary in this module.
    let state: *mut PumpState = Box::into_raw(Box::new(PumpState {
        events,
        menu: spec.menu,
    }));

    // SAFETY: `class_wide` is a live NUL-terminated buffer. HWND_MESSAGE makes
    // this a message-only window: it is never drawn, never enumerated by the
    // user, and cannot be a target for `SetForegroundWindow`, all of which is
    // exactly right for an invisible pump host. The class was just registered.
    let hwnd = match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(class_wide.as_ptr()),
            PCWSTR::null(),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            Some(windows::Win32::UI::WindowsAndMessaging::HWND_MESSAGE),
            None,
            Some(module),
            Some(state.cast()),
        )
    } {
        Ok(hwnd) => hwnd,
        Err(e) => {
            // The window was never created, so nothing stored `state` and the
            // leak would be permanent. Reclaim it here.
            reclaim_state(state);
            unregister_class(module, &class_wide);
            let _ = ready.send(Err(TrayError::WindowCreationFailed {
                code: crate::single_instance::last_error_code(&e),
            }));
            return;
        }
    };

    let icon = match load_icon(&spec.icon) {
        Ok(icon) => icon,
        Err(e) => {
            // Destroy the window so `WM_NCDESTROY` runs and reclaims `state`.
            // SAFETY: `hwnd` is live and owned here.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            unregister_class(module, &class_wide);
            let _ = ready.send(Err(e));
            return;
        }
    };

    // SAFETY: `hwnd` is live, `icon` is either absent (the shell draws its own
    // default) or a handle this thread owns, and `notify_icon_data` builds a
    // fully initialised buffer whose address is passed. The shell copies what it
    // needs before returning, so the buffer's stack lifetime is sufficient.
    let added = unsafe {
        let data = notify_icon_data(NIM_ADD, hwnd, icon.unwrap_or_default(), &spec.tooltip);
        Shell_NotifyIconW(NIM_ADD, &data)
    };
    if !added.as_bool() {
        // The icon is not registered, so the `icon` is ours to destroy, and the
        // window must go so `state` is reclaimed.
        let code = unsafe { windows::Win32::Foundation::GetLastError() }.0;
        // SAFETY: `icon` was loaded here and is not referenced by any shell
        // entry, because the add failed.
        unsafe {
            if let Some(icon) = icon {
                let _ = DestroyIcon(icon);
            }
            let _ = DestroyWindow(hwnd);
        }
        unregister_class(module, &class_wide);
        let _ = ready.send(Err(TrayError::NotifyFailed {
            operation: "NIM_ADD",
            code,
        }));
        return;
    }

    // Reported only once the icon exists and the loop is about to start, so a
    // `WM_QUIT` posted after this can never be missed.
    // SAFETY: takes no arguments and cannot fail.
    let thread_id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    if ready.send(Ok(thread_id)).is_err() {
        // The owner gave up. Tear the icon down rather than leaving a live one
        // nobody can remove.
        remove_icon(hwnd);
        // SAFETY: `hwnd` is live and owned here.
        unsafe {
            PostQuitMessage(0);
            let _ = DestroyWindow(hwnd);
        }
        if let Some(icon) = icon {
            // SAFETY: the shell entry that referenced it is gone.
            unsafe {
                let _ = DestroyIcon(icon);
            }
        }
        unregister_class(module, &class_wide);
        return;
    }

    // `icon` is deliberately held across the loop: the shell keeps a reference
    // to the image for as long as the entry exists, and releasing it earlier
    // would leave the notification area drawing freed memory. It is destroyed
    // below, after `NIM_DELETE`.
    let mut msg = MSG::default();
    loop {
        // SAFETY: `msg` is a live, correctly sized MSG that outlives the call.
        let got = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if !got.as_bool() {
            // 0 is WM_QUIT, -1 is a real error. Either way the loop is over.
            break;
        }
        // SAFETY: `msg` was filled by GetMessageW and is still valid and owned.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // The loop ended: the owner dropped the `TrayIcon`, which posts WM_QUIT.
    // Remove the shell entry before anything else, so no icon is left pointing
    // at a window that is about to stop existing.
    remove_icon(hwnd);
    // SAFETY: `hwnd` is live; this releases the window and runs WM_NCDESTROY,
    // which reclaims the `PumpState`.
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    if let Some(icon) = icon {
        // SAFETY: the shell entry that referenced the image has been deleted,
        // so nothing outside this process still holds it.
        unsafe {
            let _ = DestroyIcon(icon);
        }
    }
    unregister_class(module, &class_wide);
}

/// Releases a window class this thread registered.
///
/// A class registration is process-wide and outlives its window, so a tray that
/// is installed and dropped repeatedly would accumulate one per cycle. The class
/// name embeds the thread id, which is unique for the life of the process, so
/// no two registrations can collide and this is safe to call unconditionally
/// after the last window of ours is gone.
fn unregister_class(module: HINSTANCE, class_name: &[u16]) {
    // SAFETY: `class_name` is the NUL-terminated buffer the class was registered
    // with, `module` is the same instance, and the window is already destroyed,
    // so no window still depends on the registration.
    unsafe {
        let _ = UnregisterClassW(PCWSTR(class_name.as_ptr()), Some(module));
    }
}

/// Issues `NIM_DELETE` for this icon.
///
/// The buffer's handle and id must match the ones used for `NIM_ADD`, which is
/// why it goes through the same builder.
fn remove_icon(hwnd: HWND) {
    // SAFETY: the buffer is a fully initialised, live local whose address is
    // passed; the shell copies from it during the call. `NIM_DELETE` reads only
    // the handle and the id, both of which match the add.
    unsafe {
        let data = notify_icon_data(NIM_DELETE, hwnd, HICON::default(), "");
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
    }
}

/// Reclaims a `PumpState` that was never handed to a window.
fn reclaim_state(state: *mut PumpState) {
    // SAFETY: `state` came from `Box::into_raw` in `pump_body` and no window
    // ever received it, so it has exactly one owner and this is its only free.
    unsafe {
        drop(Box::from_raw(state));
    }
}

/// The window procedure. Runs on the pump thread, inside `DispatchMessageW`.
///
/// # Safety contract
///
/// `hwnd` and the message parameters come from the window manager and are only
/// valid for the duration of this call. `GWLP_USERDATA` holds a `*mut
/// PumpState` that was leaked in `pump_body`; it is read back as a shared
/// reference, which is sound because nothing else mutates the box while the
/// window exists, and it is cleared and freed in `WM_NCDESTROY` before the
/// window handle becomes invalid.
unsafe extern "system" fn tray_wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        // Take the pointer the window was created with, before anything can
        // dispatch to us and expect it to be there.
        // SAFETY: `lparam` is the CREATESTRUCTW the window manager passed for
        // WM_NCCREATE; its `lpCreateParams` is the `lpParam` given to
        // CreateWindowExW, which is our `*mut PumpState`.
        let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        // SAFETY: `hwnd` is live and the index is the documented slot for
        // user data.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        // SAFETY: as above.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }

    if message == WM_NCDESTROY {
        // Clear first, then free. If anything below re-enters, it finds a null
        // rather than a freed box.
        // SAFETY: `hwnd` is live and still has our pointer at this point.
        let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut PumpState;
        // SAFETY: as above.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        }
        if !state.is_null() {
            // SAFETY: the box was leaked in `pump_body` and this is the
            // documented single owner releasing it.
            unsafe {
                drop(Box::from_raw(state));
            }
        }
        // SAFETY: the window is being destroyed; forwarding is still required.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }

    if message == TRAY_CALLBACK_MESSAGE {
        // SAFETY: the pointer was stored in WM_NCCREATE from our own
        // `Box::into_raw` and is only freed in WM_NCDESTROY, which cannot have
        // run while a message is being dispatched to this window.
        let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut PumpState;
        if !state.is_null() {
            // SAFETY: as above, so the reference is valid for this call.
            let state = unsafe { &*state };
            if let Some(event) = decode_tray_message(lparam.0 as u32) {
                if event == TrayEvent::ContextMenuOpened {
                    // Required before tracking a menu: without the window being
                    // foreground, Windows dismisses the menu the moment it
                    // loses focus, which is immediately.
                    // SAFETY: `hwnd` is live.
                    unsafe {
                        let _ = SetForegroundWindow(hwnd);
                    }
                    // SAFETY: `hwnd` is the live window whose callback this is,
                    // and `state.menu` is owned by the `PumpState` this dispatch
                    // is borrowing, so both outlive the call.
                    let chosen = unsafe { show_menu(hwnd, &state.menu) };
                    if let Some(command) = chosen {
                        let _ = state.events.send(TrayEvent::MenuCommand(command));
                    }
                } else {
                    let _ = state.events.send(event);
                }
            }
        }
        return LRESULT(0);
    }

    if message == WM_DESTROY {
        // Nothing in this crate destroys the window from outside the loop, but
        // if anything ever does, the loop has to end or the thread outlives its
        // own window and the icon keeps pointing at it.
        //
        // SAFETY: takes no arguments; it posts WM_QUIT to this thread's queue,
        // which is the thread running the loop.
        unsafe {
            PostQuitMessage(0);
        }
        // SAFETY: the window is being destroyed, so forwarding is still
        // required; `PostQuitMessage` alone would skip default handling.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }

    // SAFETY: forwarding an unhandled message is exactly what the default
    // procedure is for.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// Builds and tracks the context menu, returning the chosen command.
///
/// # Safety
/// `hwnd` must be a live window on the calling thread, and `menu` must outlive
/// the call. Called only from `tray_wndproc` on the pump thread.
unsafe fn show_menu(hwnd: HWND, menu: &[TrayMenuItem]) -> Option<String> {
    // SAFETY: CreatePopupMenu takes no arguments and returns an owned handle.
    let handle: HMENU = match unsafe { CreatePopupMenu() } {
        Ok(handle) => handle,
        Err(_) => return None,
    };

    for (index, item) in menu.iter().enumerate() {
        let flags = if item.separator {
            // SAFETY: `handle` is live.
            unsafe { AppendMenuW(handle, MF_SEPARATOR, 0, PCWSTR::null()) }
        } else {
            let label = wide_nul(&item.label);
            // SAFETY: `handle` is live, `label` is a NUL-terminated buffer that
            // outlives the call, and MF_STRING is what says the pointer is a
            // string rather than a resource id.
            unsafe {
                AppendMenuW(
                    handle,
                    MF_STRING,
                    menu_command_id(index),
                    PCWSTR(label.as_ptr()),
                )
            }
        };
        if flags.is_err() {
            // SAFETY: `handle` is live and this is its only close.
            unsafe {
                let _ = DestroyMenu(handle);
            }
            return None;
        }
    }

    // SAFETY: `GetCursorPos` takes no arguments and cannot fail.
    let mut point = windows::Win32::Foundation::POINT::default();
    // SAFETY: `point` is a live, correctly sized out-parameter.
    unsafe { GetCursorPos(&mut point) }.ok();

    // TPM_RETURNCMD makes the call return the chosen id directly instead of
    // posting WM_COMMAND, which halves the moving parts.
    // SAFETY: `handle` is live, `hwnd` is the live owner, and `point` is a live
    // value read a moment ago.
    let chosen = unsafe {
        TrackPopupMenuEx(
            handle,
            TPM_RIGHTBUTTON.0 | TPM_RETURNCMD.0,
            point.x,
            point.y,
            hwnd,
            None,
        )
    };

    // SAFETY: `handle` is live and this is its only close.
    unsafe {
        let _ = DestroyMenu(handle);
    }

    if chosen.0 == 0 {
        // Dismissed. Reporting this as a command would fire a menu action the
        // user explicitly cancelled.
        return None;
    }
    let index = menu_index_from_command(chosen.0 as usize)?;
    menu.get(index).map(|item| item.command.clone())
}

/// Loads the icon image, or `None` to let the shell draw its own default.
///
/// The distinction matters: the stock application icon is a *shared* system
/// resource, and `DestroyIcon` on it would damage every other process using
/// it. Only a file-backed icon is owned by this thread.
fn load_icon(source: &TrayIconSource) -> Result<Option<HICON>, TrayError> {
    match source {
        TrayIconSource::Application => {
            // Verified on purpose, not used: if the shared icon cannot be
            // loaded the shell still draws a default, so there is no reason to
            // own a handle we must not destroy. Checking it here turns "the icon
            // is silently missing" into a reported error.
            // SAFETY: a null module asks for the system icon; the
            // MAKEINTRESOURCE constant is the documented way to name one.
            let probe =
                unsafe { LoadImageW(None, IDI_APPLICATION, IMAGE_ICON, 0, 0, LR_DEFAULTSIZE) };
            if probe.is_err() {
                return Err(TrayError::WindowCreationFailed {
                    code: crate::single_instance::last_error_code(
                        &probe.expect_err("checked above"),
                    ),
                });
            }
            Ok(None)
        }
        TrayIconSource::File(path) => {
            let wide = crate::wide::wide_path_nul(path);
            // SAFETY: `wide` is a NUL-terminated buffer that outlives the call.
            // LR_LOADFROMFILE is what makes `wide` a path rather than a
            // resource id, and the null module is correct for a file.
            match unsafe {
                LoadImageW(
                    None,
                    PCWSTR(wide.as_ptr()),
                    IMAGE_ICON,
                    0,
                    0,
                    LR_LOADFROMFILE | LR_DEFAULTSIZE,
                )
            } {
                // SAFETY: LoadImageW with LR_LOADFROMFILE returns a handle the
                // caller owns and must release with DestroyIcon.
                Ok(handle) => Ok(Some(HICON(handle.0))),
                Err(e) => Err(TrayError::WindowCreationFailed {
                    code: crate::single_instance::last_error_code(&e),
                }),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The public handle
// ---------------------------------------------------------------------------

/// A live tray icon.
///
/// `Drop` shuts the pump down by posting `WM_QUIT` to its thread and joining it.
/// The pump thread then issues `NIM_DELETE` before destroying the window and the
/// image, in that order — reversing any two of those leaves the shell pointing
/// at freed memory.
pub struct TrayIcon {
    /// Events from the icon. Disconnected when the icon goes away, which is how
    /// a reader knows to stop.
    events: Receiver<TrayEvent>,
    pump: Option<PumpThread>,
}

/// The pump thread and the handle needed to stop it.
struct PumpThread {
    /// The pump's Windows thread id, used by `PostThreadMessageW`.
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl TrayIcon {
    /// Creates the icon and starts its pump.
    ///
    /// Blocks only long enough to know the icon was added; the pump itself runs
    /// on its own thread. A failure here has already cleaned up whatever it
    /// managed to create, so there is nothing to unwind by hand.
    pub fn install(spec: TraySpec) -> Result<TrayIcon, TrayError> {
        let (event_tx, events) = mpsc::channel::<TrayEvent>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<u32, TrayError>>();

        let join = std::thread::Builder::new()
            .name("orca-tray".into())
            .spawn(move || pump_body(spec, event_tx, ready_tx))
            .map_err(|e| TrayError::PumpThreadFailed {
                reason: e.to_string(),
            })?;

        match ready_rx.recv() {
            // The id arrives only once the loop is about to start, so a `WM_QUIT`
            // posted after this can never be lost.
            Ok(Ok(thread_id)) => Ok(TrayIcon {
                events,
                pump: Some(PumpThread {
                    thread_id,
                    join: Some(join),
                }),
            }),
            Ok(Err(e)) => {
                // The thread returned on its own; joining reclaims the icon
                // handle it is holding.
                let _ = join.join();
                Err(e)
            }
            Err(_) => {
                let _ = join.join();
                Err(TrayError::PumpThreadFailed {
                    reason: "tray pump thread exited before reporting readiness".into(),
                })
            }
        }
    }

    /// Takes the next event without blocking.
    ///
    /// `None` means no event is waiting *or* the icon is gone; the two are
    /// distinguished by [`TrayIcon::is_live`]. Polling is the caller's choice:
    /// a blocking `recv` on the UI thread would freeze the frame.
    pub fn try_next_event(&self) -> Option<TrayEvent> {
        self.events.try_recv().ok()
    }

    /// Whether the pump thread is still running.
    pub fn is_live(&self) -> bool {
        self.pump.is_some()
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        // Nothing else can end the loop: `GetMessageW` blocks, and the window
        // that would post WM_QUIT is destroyed by the loop itself. So the quit
        // is posted from out here, by thread id.
        if let Some(mut pump) = self.pump.take() {
            if let Some(join) = pump.join.take() {
                // SAFETY: `thread_id` came from `GetCurrentThreadId` on the pump
                // thread itself, and a message queue exists because that thread
                // has been running `GetMessageW`. A post that fails means the
                // thread is already gone, in which case the join returns at once.
                unsafe {
                    let _ = PostThreadMessageW(pump.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
                }
                let _ = join.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tray_class_name_contains_no_illegal_characters() {
        // Regression test. The name used to be built with `{:?}` on a
        // `ThreadId`, which formats as `ThreadId(3)`. The parentheses are not
        // legal in a window class name, so `RegisterClassW` succeeded and then
        // `CreateWindowExW` failed with ERROR_RESOURCE_TYPE_NOT_FOUND (1813) —
        // "class not found" for a class that had been registered moments
        // earlier. The tray icon simply never appeared, and the error pointed
        // nowhere near the cause.
        let name = tray_class_name();
        for (index, character) in name.chars().enumerate() {
            assert!(
                !matches!(
                    character,
                    '(' | ')' | '[' | ']' | '{' | '}' | ';' | ',' | '=' | '+' | '`' | '\'' | '"'
                ),
                "illegal character {character:?} at position {index} in class name {name:?}"
            );
        }
        assert!(name.starts_with("OrcaTrayWindow-"), "{name}");
    }

    #[test]
    fn the_tray_class_name_is_unique_per_process_and_thread() {
        // Two trays in one process must not share a class, or `RegisterClassW`
        // fails for the second one for no visible reason.
        assert_eq!(tray_class_name(), tray_class_name());
        assert_ne!(tray_class_name(), "OrcaTrayWindow");
    }

    #[test]
    fn decodes_the_left_click_shell_sends() {
        assert_eq!(
            decode_tray_message(WM_LBUTTONUP),
            Some(TrayEvent::LeftClick)
        );
    }

    #[test]
    fn decodes_the_double_click_shell_sends() {
        assert_eq!(
            decode_tray_message(WM_LBUTTONDBLCLK),
            Some(TrayEvent::DoubleClick)
        );
    }

    #[test]
    fn treats_context_menu_as_a_right_click() {
        // Windows sends WM_CONTEXTMENU on some configurations and only
        // WM_RBUTTONUP on others; checking one and not the other means a menu
        // the user cannot reach.
        assert_eq!(
            decode_tray_message(WM_RBUTTONUP),
            Some(TrayEvent::ContextMenuOpened)
        );
        assert_eq!(
            decode_tray_message(WM_CONTEXTMENU),
            Some(TrayEvent::ContextMenuOpened)
        );
    }

    #[test]
    fn ignores_messages_that_are_not_ours() {
        assert_eq!(decode_tray_message(WM_QUIT), None);
        assert_eq!(decode_tray_message(WM_DESTROY), None);
        assert_eq!(decode_tray_message(0x1234), None);
    }

    #[test]
    fn the_callback_message_is_above_the_system_range() {
        // WM_APP is the first message an application may use, and the shell
        // sends this one, so it has to be private to us. Checked as values
        // rather than as a const assertion so the test still means something if
        // the constant is ever moved.
        let message = TRAY_CALLBACK_MESSAGE;
        assert!(
            message >= WM_APP,
            "message {message} must be at or above WM_APP"
        );
        assert!(
            message < WM_APP + 0x8000,
            "message {message} must stay in the private range"
        );
    }

    #[test]
    fn the_menu_id_mapping_never_collides_with_the_dismiss_value() {
        // TrackPopupMenuEx with TPM_RETURNCMD returns 0 on dismissal, so 0 has
        // to mean "nothing chosen" and every real id has to be non-zero.
        assert_eq!(menu_command_id(0), 1);
        assert_eq!(menu_index_from_command(0), None);
        assert_eq!(menu_index_from_command(menu_command_id(3)), Some(3));
    }

    #[test]
    fn the_add_buffer_carries_the_fields_the_shell_needs() {
        let data = notify_icon_data(NIM_ADD, HWND::default(), HICON::default(), "orca");

        assert_eq!(data.cbSize, std::mem::size_of::<NOTIFYICONDATAW>() as u32);
        assert!(data.uFlags.contains(NIF_MESSAGE));
        assert!(data.uFlags.contains(NIF_ICON));
        assert!(data.uFlags.contains(NIF_TIP));
        assert_eq!(data.uCallbackMessage, TRAY_CALLBACK_MESSAGE);
        assert_eq!(data.uID, TRAY_ICON_ID);
    }

    #[test]
    fn the_delete_buffer_keeps_the_identity_and_drops_the_rest() {
        // The shell addresses the icon by handle and id, so those must survive;
        // the tip and icon are meaningless for a delete and sending them risks
        // the shell reading a stale handle.
        let hwnd = HWND(0x1234 as *mut std::ffi::c_void);
        let icon = HICON(0x5678 as *mut std::ffi::c_void);
        let added = notify_icon_data(NIM_ADD, hwnd, icon, "orca");
        let deleted = notify_icon_data(NIM_DELETE, hwnd, icon, "orca");

        assert_eq!(deleted.hWnd, added.hWnd);
        assert_eq!(deleted.uID, added.uID);
        assert!(!deleted.uFlags.contains(NIF_ICON));
        assert!(!deleted.uFlags.contains(NIF_TIP));
    }

    /// Decodes the NUL-terminated tip field back to a `String`, the way the
    /// shell would read it.
    fn tip_of(data: &NOTIFYICONDATAW) -> String {
        let units = unsafe { std::slice::from_raw_parts(data.szTip.as_ptr(), data.szTip.len()) };
        let end = units
            .iter()
            .position(|&u| u == 0)
            .expect("tip is terminated");
        String::from_utf16(&units[..end]).expect("tip is valid UTF-16")
    }

    #[test]
    fn a_long_tooltip_is_truncated_not_overflowed() {
        let long = "x".repeat(400);
        let data = notify_icon_data(NIM_ADD, HWND::default(), HICON::default(), &long);
        // 128 code units, the last of which is the NUL.
        assert_eq!(tip_of(&data).chars().count(), 127);
    }

    #[test]
    fn a_tooltip_with_astral_characters_never_splits_a_pair() {
        // Each is two UTF-16 units; an odd truncation would leave a lone
        // surrogate in the buffer the shell reads.
        let long = "\u{1F600}".repeat(200);
        let data = notify_icon_data(NIM_ADD, HWND::default(), HICON::default(), &long);
        let tip = tip_of(&data);
        assert_eq!(tip.chars().count(), 63, "126 units plus the NUL");
    }

    #[test]
    fn menu_items_describe_selectable_entries_and_separators() {
        let item = TrayMenuItem::new("Show orca", "show");
        assert!(!item.separator);
        assert_eq!(item.command, "show");

        let rule = TrayMenuItem::separator();
        assert!(rule.separator);
        assert!(rule.label.is_empty());
    }

    #[test]
    fn a_spec_defaults_to_the_stock_icon() {
        let spec = TraySpec::new("orca", vec![TrayMenuItem::new("Quit", "quit")]);
        assert_eq!(spec.icon, TrayIconSource::Application);
        assert_eq!(spec.tooltip, "orca");
        assert_eq!(spec.menu.len(), 1);
    }

    #[test]
    fn errors_explain_themselves() {
        let error = TrayError::NotifyFailed {
            operation: "NIM_ADD",
            code: 5,
        };
        let message = error.to_string();
        assert!(message.contains("NIM_ADD"), "{message}");
        assert!(message.contains('5'), "{message}");
    }
}
