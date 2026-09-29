//! Global hotkey: claim a system-wide key combination and deliver each press.
//!
//! # Why a dedicated thread
//!
//! `RegisterHotKey` delivers `WM_HOTKEY` to the *message queue of the thread
//! that called it*. A thread with no running message loop receives nothing. So
//! the call, the loop, and the teardown all have to live on the same thread,
//! and that thread cannot be the UI thread: the UI thread is busy running a
//! frame. This is not a stylistic preference — it is the whole reason the
//! probe had to spawn a thread to get a working hotkey.
//!
//! # Why a backend trait
//!
//! Claiming a real hotkey is exclusive, global, and impossible to undo from a
//! test that also runs in parallel with other tests. So the Win32 call sits
//! behind [`HotkeyBackend`], and the registrar's state machine —
//! register/unregister/is_registered, and the honest reporting of failure — is
//! exercised against a recording fake. What that fake cannot cover is whether
//! Windows actually delivers the key, which is the one thing no fake can prove
//! and the thing the probe already proved.

use std::fmt;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use windows::Win32::Foundation::ERROR_HOTKEY_ALREADY_REGISTERED;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PostThreadMessageW, TranslateMessage, MSG, WM_HOTKEY, WM_QUIT,
};

use crate::{Hotkey, Key, Modifiers};

// ---------------------------------------------------------------------------
// Key -> virtual-key translation
// ---------------------------------------------------------------------------

/// The Win32 virtual-key code for a non-modifier [`Key`].
///
/// Returned as a `u32` because `RegisterHotKey` takes the code as a `u32` and
/// the `VIRTUAL_KEY` newtype is `u16`-backed; the conversion is explicit here
/// rather than smeared across call sites.
pub fn virtual_key(key: Key) -> Option<u32> {
    Some(match key {
        // Letters and digits are their own VK codes. A shift-modified digit
        // (Ctrl+Shift+1 => VK_1) is the same physical key, and the modifier
        // bits already say Shift is down, so uppercasing letters and leaving
        // digits alone is the whole mapping.
        Key::Char(c) => {
            let upper = c.to_ascii_uppercase();
            if !upper.is_ascii_alphanumeric() {
                return None;
            }
            upper as u32
        }
        Key::Function(n) => {
            if !(1..=24).contains(&n) {
                return None;
            }
            0x70 + (n as u32 - 1)
        }
        Key::Space => 0x20,
        Key::Enter => 0x0D,
        Key::Tab => 0x09,
        Key::Escape => 0x1B,
        Key::Backspace => 0x08,
        Key::Delete => 0x2E,
    })
}

/// The `MOD_*` flag bits for a [`Modifiers`] set.
pub fn modifier_flags(modifiers: Modifiers) -> u32 {
    let mut flags = 0;
    if modifiers.contains(Modifiers::ALT) {
        flags |= MOD_ALT.0;
    }
    if modifiers.contains(Modifiers::CTRL) {
        flags |= MOD_CONTROL.0;
    }
    if modifiers.contains(Modifiers::SHIFT) {
        flags |= MOD_SHIFT.0;
    }
    if modifiers.contains(Modifiers::WIN) {
        flags |= MOD_WIN.0;
    }
    flags
}

/// `(MOD_* flags, virtual key)` for a complete hotkey, or `None` if the key
/// has no virtual-key code.
pub fn hotkey_code(hotkey: &Hotkey) -> Option<(u32, u32)> {
    Some((modifier_flags(hotkey.modifiers), virtual_key(hotkey.key)?))
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Something went wrong binding or unbinding a system-wide hotkey.
///
/// A hotkey another application already owns is a *normal* condition, not a
/// programming error and not a reason to panic: the user has it bound to
/// something else, and the right response is to report it and carry on. That is
/// why [`HotkeyError::AlreadyTaken`] is a distinct, expected variant rather
/// than a generic failure code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyError {
    /// Windows refused the hotkey; another application already owns it.
    AlreadyTaken,
    /// The combination has no virtual-key code, so it cannot be bound.
    ///
    /// Reachable by constructing a [`Hotkey`] directly, which bypasses
    /// [`Hotkey::parse`] and its validation.
    UnsupportedKey {
        /// The key that has no code.
        key: Key,
    },
    /// [`GlobalHotkey::unregister`] was called with nothing registered.
    NotRegistered,
    /// A hotkey was already registered on this registrar.
    AlreadyRegistered,
    /// The hotkey thread could not be started, or exited unexpectedly.
    ThreadFailure {
        /// What went wrong, for the log line.
        reason: String,
    },
    /// Windows reported a failure for a reason we do not model.
    SystemFailure {
        /// The raw Win32 error code, as `GetLastError()` reported it.
        code: u32,
    },
}

impl fmt::Display for HotkeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HotkeyError::AlreadyTaken => {
                write!(f, "hotkey is already owned by another application")
            }
            HotkeyError::UnsupportedKey { key } => {
                write!(f, "{key:?} has no Windows virtual-key code")
            }
            HotkeyError::NotRegistered => write!(f, "no hotkey is registered"),
            HotkeyError::AlreadyRegistered => write!(f, "a hotkey is already registered"),
            HotkeyError::ThreadFailure { reason } => {
                write!(f, "hotkey thread failed: {reason}")
            }
            HotkeyError::SystemFailure { code } => {
                write!(f, "hotkey call failed (Win32 error {code})")
            }
        }
    }
}

impl std::error::Error for HotkeyError {}

/// Pulls a Win32 error code out of a `windows::core::Error`.
///
/// `RegisterHotKey` reports failure as an `HRESULT` produced by
/// `HRESULT_FROM_WIN32`, which is `0x8007_0000 | code` for codes that fit in
/// 16 bits. Codes that do not fit come from `HRESULT_FROM_NTSTATUS` instead and
/// are returned unchanged, so a caller can still see the real value.
fn win32_code(error: &windows::core::Error) -> u32 {
    const HRESULT_FROM_WIN32: u32 = 0x8007_0000;
    const FACILITY_WIN32: u32 = 0x0007_0000;

    let hresult = error.code().0 as u32;
    if hresult & 0xFFFF_0000 == HRESULT_FROM_WIN32 && hresult & 0x0000_FFFF != 0 {
        hresult & 0x0000_FFFF
    } else {
        // Not a packed Win32 code. `FACILITY_WIN32` is read to keep the
        // classification explicit; a COM-style failure has no Win32 code to
        // report and is surfaced as the HRESULT itself.
        let _ = FACILITY_WIN32;
        hresult
    }
}

// ---------------------------------------------------------------------------
// The seam
// ---------------------------------------------------------------------------

/// Claims and releases one system-wide hotkey, and delivers presses.
///
/// `Send` because the real implementation owns a thread that outlives the call
/// to `register`.
pub trait HotkeyBackend: Send {
    /// Binds `hotkey` system-wide and calls `on_press` on every press.
    ///
    /// The handler is invoked on the backend's own thread, not the caller's. A
    /// caller that needs to touch UI state must forward the press over its own
    /// channel.
    fn register(
        &mut self,
        hotkey: &Hotkey,
        on_press: Box<dyn Fn() + Send + 'static>,
    ) -> Result<(), HotkeyError>;

    /// Releases the binding. Reports [`HotkeyError::NotRegistered`] if there is
    /// nothing to release.
    fn unregister(&mut self) -> Result<(), HotkeyError>;
}

/// A system-wide hotkey: pressed while *any* application has focus.
pub trait GlobalHotkey: Send {
    /// Binds `hotkey` system-wide. On success, the handler is invoked on every
    /// press until [`GlobalHotkey::unregister`].
    fn register(&mut self, hotkey: &Hotkey) -> Result<(), HotkeyError>;

    /// Releases the hotkey, if one is bound. Idempotent in spirit: calling it
    /// without a registration is reported as [`HotkeyError::NotRegistered`]
    /// rather than treated as a no-op success.
    fn unregister(&mut self) -> Result<(), HotkeyError>;

    /// Whether a hotkey is currently bound.
    fn is_registered(&self) -> bool;

    /// Sets the closure invoked on every press. Replaces any previous handler.
    ///
    /// Setting a handler while a hotkey is bound takes effect on the next press;
    /// it does not rebind.
    fn set_press_handler(&mut self, handler: Box<dyn Fn() + Send + 'static>);
}

// ---------------------------------------------------------------------------
// The real backend
// ---------------------------------------------------------------------------

/// The Win32 [`HotkeyBackend`]: `RegisterHotKey` on a resident thread.
///
/// # Thread lifetime
///
/// The thread is created lazily by the first `register` and torn down by
/// `unregister` or by `Drop`. Between those it is *resident*: it blocks in
/// `GetMessageW` forever. That blocking is the feature. A hotkey thread that
/// returns stops receiving presses, and the hotkey silently dies while
/// `is_registered()` still says otherwise.
pub struct Win32HotkeyBackend {
    thread: Option<HotkeyThread>,
    bound: Option<Hotkey>,
}

/// The resident thread plus the handle needed to stop it.
struct HotkeyThread {
    /// Windows thread id, used by `PostThreadMessageW`.
    thread_id: u32,
    /// Set when the thread should wind down. Checked by the message loop only
    /// via the `WM_QUIT` post, so this exists to let `Drop` be idempotent
    /// rather than to poll.
    join: Option<JoinHandle<()>>,
}

/// Hotkey ids must be unique per thread, not per process. `0` is legal, but
/// any non-zero value is easier to spot in a debugger.
const HOTKEY_ID: i32 = 0x00A1;

/// Signals from the hotkey thread back to whoever called `register`.
enum Startup {
    /// The hotkey is bound and the thread is pumping. Carries the thread id so
    /// `register` can post to it.
    Ready { thread_id: u32 },
    /// Binding failed; the thread is exiting. Carries the reason.
    Failed(HotkeyError),
}

impl Default for Win32HotkeyBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Win32HotkeyBackend {
    /// Creates a backend with nothing registered.
    pub fn new() -> Win32HotkeyBackend {
        Win32HotkeyBackend {
            thread: None,
            bound: None,
        }
    }
}

impl HotkeyBackend for Win32HotkeyBackend {
    fn register(
        &mut self,
        hotkey: &Hotkey,
        on_press: Box<dyn Fn() + Send + 'static>,
    ) -> Result<(), HotkeyError> {
        if self.bound.is_some() {
            return Err(HotkeyError::AlreadyRegistered);
        }

        // Validate before spending a thread on it. `Hotkey::parse` already
        // rejects most of these, but a `Hotkey` built by hand reaches here.
        let Some((flags, vk)) = hotkey_code(hotkey) else {
            return Err(HotkeyError::UnsupportedKey { key: hotkey.key });
        };

        let modifiers = HOT_KEY_MODIFIERS(flags);
        let key = vk;

        let (startup_tx, startup_rx) = mpsc::channel::<Startup>();

        let join = std::thread::Builder::new()
            .name("orca-hotkey".into())
            .spawn(move || hotkey_thread_body(modifiers, key, on_press, startup_tx))
            .map_err(|e| HotkeyError::ThreadFailure {
                reason: e.to_string(),
            })?;

        // The thread reports whether the bind succeeded before it starts
        // pumping, so a caller that gets `Err` knows nothing was claimed and
        // `is_registered()` is right to say so.
        match startup_rx.recv() {
            Ok(Startup::Ready { thread_id }) => {
                self.thread = Some(HotkeyThread {
                    thread_id,
                    join: Some(join),
                });
                self.bound = Some(*hotkey);
                Ok(())
            }
            Ok(Startup::Failed(error)) => {
                // The thread has already returned or is about to; join it so a
                // failed register does not leak a thread.
                let _ = join.join();
                Err(error)
            }
            Err(_) => {
                // The sender is gone, which means the thread panicked or
                // exited without reporting. Treat it as a thread failure
                // rather than a success: nothing may be assumed to be bound.
                let _ = join.join();
                Err(HotkeyError::ThreadFailure {
                    reason: "hotkey thread exited before reporting readiness".into(),
                })
            }
        }
    }

    fn unregister(&mut self) -> Result<(), HotkeyError> {
        if self.bound.is_none() {
            return Err(HotkeyError::NotRegistered);
        }

        // Clear state *first*. If the post or the join fails, the binding is
        // still gone as far as any later caller is concerned, and reporting
        // `is_registered() == true` after a failed unregister would be a lie.
        self.bound = None;

        let Some(mut thread) = self.thread.take() else {
            return Err(HotkeyError::NotRegistered);
        };

        // SAFETY: the id came from `GetCurrentThreadId` on the hotkey thread
        // itself, and that thread has been running `GetMessageW`, so a queue
        // exists. A post that fails means the thread already exited, in which
        // case the join below returns immediately.
        let post = unsafe { PostThreadMessageW(thread.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };

        // Always join, even if the post failed. The thread may already be gone
        // (the post fails with an invalid-id error), and dropping a detached
        // thread handle is exactly the leak this is avoiding.
        let joined = thread.join.take().map(|handle| handle.join().is_ok());

        match (post, joined) {
            (Ok(()), Some(true)) => Ok(()),
            (Ok(()), _) => Err(HotkeyError::ThreadFailure {
                reason: "hotkey thread panicked during shutdown".into(),
            }),
            (Err(e), _) => Err(HotkeyError::SystemFailure {
                code: win32_code(&e),
            }),
        }
    }
}

impl Drop for Win32HotkeyBackend {
    fn drop(&mut self) {
        // Best effort. `Drop` cannot report, and a hotkey that outlives the
        // process would be held by a thread that no longer exists to release
        // it, so the attempt is worth making even though the result is lost.
        let _ = self.unregister();
    }
}

/// The resident hotkey thread: bind, then pump until told to quit.
///
/// Runs on its own thread because `RegisterHotKey` targets the calling thread's
/// queue and `GetMessageW` must be drained on that same thread.
fn hotkey_thread_body(
    modifiers: HOT_KEY_MODIFIERS,
    key: u32,
    on_press: Box<dyn Fn() + Send + 'static>,
    startup: mpsc::Sender<Startup>,
) {
    // A NULL hwnd associates the hotkey with this thread and delivers
    // WM_HOTKEY to this thread's queue.
    match unsafe { RegisterHotKey(None, HOTKEY_ID, modifiers, key) } {
        Ok(()) => {}
        Err(e) => {
            let code = win32_code(&e);
            let _ = startup.send(Startup::Failed(
                if code == ERROR_HOTKEY_ALREADY_REGISTERED.0 {
                    // The expected case: someone else got there first. This is
                    // reported, not panicked on, and leaves nothing bound.
                    HotkeyError::AlreadyTaken
                } else {
                    HotkeyError::SystemFailure { code }
                },
            ));
            return;
        }
    }

    let thread_id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    if startup.send(Startup::Ready { thread_id }).is_err() {
        // Nobody is waiting to hear we are ready, so nobody will ever post
        // WM_QUIT. Release the hotkey and exit rather than becoming an
        // unstoppable thread holding a global resource.
        // SAFETY: the hotkey was registered on this thread a moment ago with
        // the same id, so the unregister matches a live registration.
        unsafe {
            let _ = UnregisterHotKey(None, HOTKEY_ID);
        }
        return;
    }

    let mut msg = MSG::default();
    loop {
        // SAFETY: `msg` is a correctly sized, owned MSG that outlives the call;
        // GetMessageW writes into it and the thread keeps running afterwards.
        // The returned BOOL is tri-state: >0 a message, 0 WM_QUIT, -1 error.
        let got = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        // Tri-state: > 0 a message, 0 for WM_QUIT, -1 for a real error. Neither
        // the quit nor the error leaves anything useful to do, and staying in
        // the loop would spin.
        if got.0 <= 0 {
            break;
        }

        if msg.message == WM_HOTKEY && msg.wParam.0 == HOTKEY_ID as usize {
            on_press();
            continue;
        }

        // SAFETY: `msg` was filled by GetMessageW above and is still valid and
        // owned here. TranslateMessage/DispatchMessageW only read it and act on
        // its hwnd, which for a hotkey message is null and is dispatched to
        // DefWindowProc internally.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // The thread is going away, so the hotkey must go with it. Without this,
    // the combination stays claimed by a dead thread's id until the process
    // exits.
    //
    // SAFETY: the hotkey was registered on this thread at the top of this
    // function with the same id, so this releases a live registration and the
    // null hwnd matches the one it was claimed with.
    unsafe {
        let _ = UnregisterHotKey(None, HOTKEY_ID);
    }
}

// ---------------------------------------------------------------------------
// The registrar
// ---------------------------------------------------------------------------

/// Binds a hotkey through a [`HotkeyBackend`] and tracks whether it is bound.
///
/// The recorded state is deliberately private to this type: `is_registered`
/// must be answerable without consulting the backend, because the whole point
/// of the rule below is that a caller who discards errors still cannot
/// conclude the hotkey is live when it is not.
pub struct Win32GlobalHotkey {
    backend: Box<dyn HotkeyBackend>,
    /// Shared with the backend's thread, which calls through it on every press.
    /// A cell rather than a plain field so `set_press_handler` works after
    /// `register`, which is the order the app actually uses: bind, then decide
    /// what a press means.
    handler: Arc<Mutex<Box<dyn FnMut() + Send + 'static>>>,
    /// Set only after `backend.register` returns `Ok`. Never set on the way to
    /// an error.
    registered: Option<Hotkey>,
}

impl Default for Win32GlobalHotkey {
    fn default() -> Self {
        Self::new()
    }
}

impl Win32GlobalHotkey {
    /// Creates a registrar backed by the real Win32 `RegisterHotKey`.
    ///
    /// Constructing it claims nothing and spawns nothing; the hotkey thread is
    /// created by the first [`GlobalHotkey::register`].
    pub fn new() -> Win32GlobalHotkey {
        Self::with_backend(Box::new(Win32HotkeyBackend::new()))
    }

    /// Creates a registrar over an arbitrary backend.
    ///
    /// This is the seam that makes the registrar testable: pass a recording
    /// fake and the whole state machine runs without claiming a global hotkey.
    pub fn with_backend(backend: Box<dyn HotkeyBackend>) -> Win32GlobalHotkey {
        Win32GlobalHotkey {
            backend,
            handler: Arc::new(Mutex::new(Box::new(|| {}))),
            registered: None,
        }
    }

    /// The hotkey currently bound, if any.
    pub fn current(&self) -> Option<Hotkey> {
        self.registered
    }
}

impl GlobalHotkey for Win32GlobalHotkey {
    fn register(&mut self, hotkey: &Hotkey) -> Result<(), HotkeyError> {
        let cell = Arc::clone(&self.handler);
        let result = self
            .backend
            .register(hotkey, Box::new(move || lock(&cell)()));

        match result {
            Ok(()) => {
                self.registered = Some(*hotkey);
                Ok(())
            }
            Err(e) => {
                // Do not record a registration we do not have. This is the
                // invariant that makes discarding the error safe.
                self.registered = None;
                Err(e)
            }
        }
    }

    fn unregister(&mut self) -> Result<(), HotkeyError> {
        match self.backend.unregister() {
            Ok(()) => {
                self.registered = None;
                Ok(())
            }
            Err(HotkeyError::NotRegistered) => Err(HotkeyError::NotRegistered),
            Err(e) => {
                // Any other failure means the binding state is unknown, and
                // claiming it is still bound would be the more dangerous lie:
                // the caller would believe a global resource is held.
                self.registered = None;
                Err(e)
            }
        }
    }

    fn is_registered(&self) -> bool {
        self.registered.is_some()
    }

    fn set_press_handler(&mut self, handler: Box<dyn Fn() + Send + 'static>) {
        *lock(&self.handler) = Box::new(handler);
    }
}

/// Locks `mutex`, treating a poisoned lock as empty rather than panicking.
///
/// A panic while a press handler holds this lock would otherwise turn a bug in
/// a UI callback into a hotkey thread that dies silently, and the next press
/// would then do nothing at all — the exact "is_registered says true but the
/// hotkey is dead" state this module exists to prevent.
fn lock<'a, T>(mutex: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// What the fake backend saw, and how the test drives a press.
    #[derive(Default)]
    struct Recorded {
        register_calls: Vec<Hotkey>,
        unregister_calls: usize,
        register_result: Option<HotkeyError>,
        unregister_result: Option<HotkeyError>,
        /// The handler the registrar handed over, so a test can fire a press
        /// without a real key and without a message loop.
        handler: Option<Box<dyn Fn() + Send + 'static>>,
    }

    /// A backend that records what it was asked to do and never touches the
    /// global hotkey table. Shared with the test so both sides can see it.
    #[derive(Clone, Default)]
    struct FakeBackend(Arc<Mutex<Recorded>>);

    impl FakeBackend {
        fn failing_register(error: HotkeyError) -> FakeBackend {
            let backend = FakeBackend::default();
            backend.0.lock().unwrap().register_result = Some(error);
            backend
        }

        fn recorded(&self) -> Vec<Hotkey> {
            self.0.lock().unwrap().register_calls.clone()
        }

        /// Simulates a press, by invoking the handler the registrar installed.
        fn press(&self) {
            // Borrowed, not taken: a real backend delivers many presses through
            // one handler, and a fake that consumed it would test nothing after
            // the first.
            let state = self.0.lock().unwrap();
            if let Some(handler) = state.handler.as_ref() {
                handler();
            }
        }
    }

    impl HotkeyBackend for FakeBackend {
        fn register(
            &mut self,
            hotkey: &Hotkey,
            on_press: Box<dyn Fn() + Send + 'static>,
        ) -> Result<(), HotkeyError> {
            let mut state = self.0.lock().unwrap();
            state.register_calls.push(*hotkey);
            if let Some(failure) = state.register_result.take() {
                return Err(failure);
            }
            // Only keep the handler when the bind succeeded, matching the real
            // backend: a refused hotkey delivers nothing.
            state.handler = Some(on_press);
            Ok(())
        }

        fn unregister(&mut self) -> Result<(), HotkeyError> {
            let mut state = self.0.lock().unwrap();
            state.unregister_calls += 1;
            state.handler = None;
            match state.unregister_result.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }

    fn spec(text: &str) -> Hotkey {
        Hotkey::parse(text).expect("spec should parse")
    }

    #[test]
    fn maps_the_common_keys_to_virtual_key_codes() {
        assert_eq!(virtual_key(Key::Space), Some(0x20));
        assert_eq!(virtual_key(Key::Enter), Some(0x0D));
        assert_eq!(virtual_key(Key::Tab), Some(0x09));
        assert_eq!(virtual_key(Key::Escape), Some(0x1B));
        assert_eq!(virtual_key(Key::Backspace), Some(0x08));
        assert_eq!(virtual_key(Key::Delete), Some(0x2E));
    }

    #[test]
    fn maps_function_keys_to_their_contiguous_vk_range() {
        // F1 is 0x70 and they run consecutively to F24 at 0x87.
        assert_eq!(virtual_key(Key::Function(1)), Some(0x70));
        assert_eq!(virtual_key(Key::Function(5)), Some(0x74));
        assert_eq!(virtual_key(Key::Function(24)), Some(0x87));
        assert_eq!(virtual_key(Key::Function(0)), None);
        assert_eq!(virtual_key(Key::Function(25)), None);
    }

    #[test]
    fn maps_letters_case_insensitively_and_digits_directly() {
        assert_eq!(virtual_key(Key::Char('k')), virtual_key(Key::Char('K')));
        assert_eq!(virtual_key(Key::Char('k')), Some(u32::from(b'K')));
        assert_eq!(virtual_key(Key::Char('7')), Some(u32::from(b'7')));
    }

    #[test]
    fn rejects_characters_with_no_virtual_key_code() {
        // Punctuation and non-ASCII have no fixed VK: their meaning depends on
        // the active keyboard layout, so a global binding for them is not
        // expressible as a VK pair.
        assert_eq!(virtual_key(Key::Char('/')), None);
        assert_eq!(virtual_key(Key::Char('\u{00e9}')), None);
    }

    #[test]
    fn modifiers_become_the_matching_mod_flag_bits() {
        assert_eq!(modifier_flags(Modifiers::NONE), 0);
        assert_eq!(modifier_flags(Modifiers::CTRL), MOD_CONTROL.0);
        assert_eq!(
            modifier_flags(Modifiers::CTRL.union(Modifiers::SHIFT)),
            MOD_CONTROL.0 | MOD_SHIFT.0
        );
        assert_eq!(
            modifier_flags(
                Modifiers::CTRL
                    .union(Modifiers::SHIFT)
                    .union(Modifiers::ALT)
                    .union(Modifiers::WIN)
            ),
            MOD_CONTROL.0 | MOD_SHIFT.0 | MOD_ALT.0 | MOD_WIN.0
        );
    }

    #[test]
    fn hotkey_code_pairs_the_flags_with_the_key() {
        let (flags, key) = hotkey_code(&spec("Ctrl+Shift+Space")).expect("should map");
        assert_eq!(flags, MOD_CONTROL.0 | MOD_SHIFT.0);
        assert_eq!(key, 0x20);
    }

    #[test]
    fn win32_code_unpacks_a_packed_win32_error() {
        // HRESULT_FROM_WIN32(1409), what RegisterHotKey returns for "taken".
        let packed = windows::core::HRESULT::from_win32(ERROR_HOTKEY_ALREADY_REGISTERED.0);
        let error = windows::core::Error::from_hresult(packed);
        assert_eq!(win32_code(&error), ERROR_HOTKEY_ALREADY_REGISTERED.0);
    }

    #[test]
    fn win32_code_preserves_a_success_code_as_zero() {
        let error = windows::core::Error::from_hresult(windows::core::HRESULT(0));
        assert_eq!(win32_code(&error), 0);
    }

    #[test]
    fn register_marks_the_registrar_bound_and_passes_the_hotkey_through() {
        let backend = FakeBackend::default();
        let mut registrar = Win32GlobalHotkey::with_backend(Box::new(backend.clone()));
        let hotkey = spec("Ctrl+Shift+K");

        assert!(!registrar.is_registered());
        registrar
            .register(&hotkey)
            .expect("fake backend accepts anything");
        assert!(registrar.is_registered());
        assert_eq!(registrar.current(), Some(hotkey));
        assert_eq!(backend.recorded(), vec![hotkey]);
    }

    #[test]
    fn a_refused_hotkey_leaves_the_registrar_unbound() {
        // The load-bearing test: a hotkey owned by another app is a normal
        // outcome, and a caller that ignores the error must not conclude the
        // hotkey is live.
        let backend = FakeBackend::failing_register(HotkeyError::AlreadyTaken);
        let mut registrar = Win32GlobalHotkey::with_backend(Box::new(backend));

        let error = registrar
            .register(&spec("Ctrl+Shift+Space"))
            .expect_err("should fail");

        assert_eq!(error, HotkeyError::AlreadyTaken);
        assert!(
            !registrar.is_registered(),
            "a failed register must not record a binding"
        );
        assert_eq!(registrar.current(), None);
    }

    #[test]
    fn register_reports_a_backend_that_already_holds_a_hotkey() {
        let backend = FakeBackend::failing_register(HotkeyError::AlreadyRegistered);
        let mut registrar = Win32GlobalHotkey::with_backend(Box::new(backend));

        assert_eq!(
            registrar.register(&spec("Ctrl+Shift+K")),
            Err(HotkeyError::AlreadyRegistered)
        );
        assert!(!registrar.is_registered());
    }

    #[test]
    fn a_press_reaches_the_installed_handler() {
        let backend = FakeBackend::default();
        let mut registrar = Win32GlobalHotkey::with_backend(Box::new(backend.clone()));
        registrar.register(&spec("Ctrl+Shift+K")).expect("register");

        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        registrar.set_press_handler(Box::new(move || sink.lock().unwrap().push(1u8)));

        backend.press();
        backend.press();

        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_handler_set_after_registration_still_receives_presses() {
        // The order the app actually uses: bind, then decide what a press means.
        let backend = FakeBackend::default();
        let mut registrar = Win32GlobalHotkey::with_backend(Box::new(backend.clone()));
        registrar.register(&spec("Ctrl+Shift+K")).expect("register");

        let seen = Arc::new(Mutex::new(0u32));
        let sink = Arc::clone(&seen);
        registrar.set_press_handler(Box::new(move || *sink.lock().unwrap() += 1));

        backend.press();
        assert_eq!(*seen.lock().unwrap(), 1);
    }

    #[test]
    fn unregister_releases_and_clears_the_recorded_binding() {
        let backend = FakeBackend::default();
        let mut registrar = Win32GlobalHotkey::with_backend(Box::new(backend.clone()));
        registrar.register(&spec("Ctrl+Shift+K")).expect("register");

        registrar.unregister().expect("unregister");

        assert!(!registrar.is_registered());
        assert_eq!(backend.0.lock().unwrap().unregister_calls, 1);
    }

    #[test]
    fn unregister_without_a_registration_is_reported_not_silently_ignored() {
        let backend = FakeBackend::default();
        backend.0.lock().unwrap().unregister_result = Some(HotkeyError::NotRegistered);
        let mut registrar = Win32GlobalHotkey::with_backend(Box::new(backend));

        assert_eq!(registrar.unregister(), Err(HotkeyError::NotRegistered));
        assert!(!registrar.is_registered());
    }

    #[test]
    fn a_key_with_no_virtual_key_is_refused_before_any_thread_is_spent() {
        // Built by hand to bypass `Hotkey::parse`, which would have rejected it.
        let bogus = Hotkey {
            modifiers: Modifiers::CTRL,
            key: Key::Char('/'),
        };
        let error = Win32HotkeyBackend::new()
            .register(&bogus, Box::new(|| {}))
            .expect_err("punctuation has no VK");

        assert_eq!(
            error,
            HotkeyError::UnsupportedKey {
                key: Key::Char('/')
            }
        );
    }

    #[test]
    fn hotkey_errors_explain_themselves() {
        assert!(HotkeyError::AlreadyTaken
            .to_string()
            .contains("already owned"));
        assert!(HotkeyError::NotRegistered.to_string().contains("no hotkey"));
        assert!(HotkeyError::SystemFailure { code: 5 }
            .to_string()
            .contains('5'));
    }
}
